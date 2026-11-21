use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VNode {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub modified_at: u64,
    pub created_at: u64,
    pub readonly: bool,
    /// Dotfiles everywhere, plus the Hidden attribute on Windows (which marks
    /// NTUSER.DAT and the legacy "My Documents"-style junctions).
    #[serde(default)]
    pub hidden: bool,
    pub mime_type: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// The seam for non-local backends (remote shares, archives).
///
/// `read_dir` is currently unused by the app: local listing goes through the
/// synchronous [`list_dir`], because the only caller is the UI thread, which
/// blocks on the result anyway. The method is kept so the trait still
/// describes a complete provider.
#[allow(dead_code)]
#[async_trait]
#[allow(clippy::double_must_use)] // emitted by the async_trait macro
pub trait FileProvider: Send + Sync {
    async fn read_dir(&self, path: &Path) -> Result<Vec<VNode>, VfsError>;
    async fn remove(&self, path: &Path, recursive: bool) -> Result<(), VfsError>;
}

/// Best-effort MIME type from the file extension.
pub(crate) fn mime_for(path: &Path, is_dir: bool) -> &'static str {
    if is_dir {
        return "inode/directory";
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("rs") => "text/rust",
        Some("toml") => "text/toml",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Seconds since the Unix epoch, or 0 when the platform cannot report the
/// timestamp (`created` is not available on every filesystem).
fn epoch_secs(time: std::io::Result<std::time::SystemTime>) -> u64 {
    time.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(windows)]
fn has_hidden_attribute(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn has_hidden_attribute(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Build a `VNode` from a path and its metadata.
///
/// `meta` is expected to be symlink-level metadata (as returned by
/// `symlink_metadata` or `DirEntry::metadata`), so that a symlink is described
/// as itself rather than as its target.
pub fn node_from_metadata(path: PathBuf, meta: &std::fs::Metadata) -> VNode {
    let is_dir = meta.is_dir();
    VNode {
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        size: meta.len(),
        is_dir,
        is_symlink: meta.file_type().is_symlink(),
        modified_at: epoch_secs(meta.modified()),
        created_at: epoch_secs(meta.created()),
        readonly: meta.permissions().readonly(),
        hidden: path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            || has_hidden_attribute(meta),
        mime_type: mime_for(&path, is_dir).to_string(),
        path,
    }
}

/// Whether activating this row should enter it as a folder.
///
/// Listing uses non-following metadata so a directory symlink is not walked
/// (that would loop). Navigation still follows once, so folder shortcuts can
/// be opened in-app.
#[allow(dead_code)] // covered by tests; the UI asks `api::is_navigable` by path
pub fn is_navigable_directory(node: &VNode) -> bool {
    if node.is_dir {
        return true;
    }
    if node.is_symlink {
        std::fs::metadata(&node.path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    } else {
        false
    }
}

/// List a directory in a single blocking pass.
///
/// Each entry is described using the metadata the directory entry already
/// carries. The previous implementation awaited a separate `symlink_metadata`
/// syscall per entry, which cost roughly 8x more: ~98ms versus ~13ms for a
/// 10,000-entry directory, all of it on the UI thread.
///
/// Entries whose metadata cannot be read are skipped rather than failing the
/// whole listing — an unreadable entry should not blank the window.
pub fn list_dir(path: &Path) -> Result<Vec<VNode>, VfsError> {
    let entries = std::fs::read_dir(path)?;
    let mut results = Vec::new();
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            results.push(node_from_metadata(entry.path(), &meta));
        }
    }
    Ok(results)
}

/// Entries in the first batch of a streamed listing. Small enough to read and
/// encode in a few milliseconds, so the first paint never waits for the whole
/// folder.
pub const FIRST_CHUNK: usize = 2_000;
/// Entries in each later batch of a streamed listing.
pub const CHUNK: usize = 5_000;

/// How a streamed listing ended.
#[derive(Debug, PartialEq, Eq)]
pub enum ListEnd {
    /// Every entry was read and handed to the callback.
    Complete,
    /// The cancel flag was set; the rest of the folder was not read.
    Cancelled,
}

/// Collects entries and releases them in batches: `first` entries in the first
/// batch, then `rest` at a time, then whatever is left over.
struct Batcher {
    first: usize,
    rest: usize,
    sent_any: bool,
    pending: Vec<VNode>,
}

impl Batcher {
    fn new(first: usize, rest: usize) -> Self {
        Self {
            first: first.max(1),
            rest: rest.max(1),
            sent_any: false,
            pending: Vec::new(),
        }
    }

    /// Adds an entry. Returns a full batch when one is ready.
    fn push(&mut self, node: VNode) -> Option<Vec<VNode>> {
        self.pending.push(node);
        let target = if self.sent_any { self.rest } else { self.first };
        if self.pending.len() >= target {
            self.sent_any = true;
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    /// The entries not yet released (possibly none).
    fn finish(self) -> Vec<VNode> {
        self.pending
    }
}

/// Lists a directory in batches, handing each to `on_chunk` as soon as it is
/// ready. Entries whose metadata cannot be read are skipped, as in
/// [`list_dir`]. A directory that cannot be opened is an error before any
/// batch is sent. The cancel flag is checked between entries, so a stale
/// request stops within a few entries of being cancelled.
pub fn list_dir_chunked(
    path: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    on_chunk: impl FnMut(Vec<VNode>),
) -> Result<ListEnd, VfsError> {
    list_dir_batched(path, cancel, FIRST_CHUNK, CHUNK, on_chunk)
}

/// [`list_dir_chunked`] with the batch sizes given explicitly, so tests can
/// use small folders.
pub fn list_dir_batched(
    path: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    first: usize,
    rest: usize,
    mut on_chunk: impl FnMut(Vec<VNode>),
) -> Result<ListEnd, VfsError> {
    use std::sync::atomic::Ordering;
    let entries = std::fs::read_dir(path)?;
    let mut batcher = Batcher::new(first, rest);
    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(ListEnd::Cancelled);
        }
        if let Ok(meta) = entry.metadata() {
            if let Some(batch) = batcher.push(node_from_metadata(entry.path(), &meta)) {
                on_chunk(batch);
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(ListEnd::Cancelled);
    }
    on_chunk(batcher.finish());
    Ok(ListEnd::Complete)
}

/// Why a folder could not be listed, in words the UI shows under
/// "Can't read this folder".
pub fn describe_list_error(err: &VfsError) -> String {
    match err {
        VfsError::Io(e) => match e.kind() {
            std::io::ErrorKind::NotFound => "the folder no longer exists".into(),
            std::io::ErrorKind::PermissionDenied => "permission denied".into(),
            _ => e.to_string(),
        },
    }
}

pub struct LocalFileProvider;

#[async_trait]
#[allow(clippy::double_must_use)] // emitted by the async_trait macro
impl FileProvider for LocalFileProvider {
    async fn read_dir(&self, path: &Path) -> Result<Vec<VNode>, VfsError> {
        list_dir(path)
    }

    async fn remove(&self, path: &Path, recursive: bool) -> Result<(), VfsError> {
        let meta = tokio::fs::symlink_metadata(path).await?;
        if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(path).await?;
            } else {
                tokio::fs::remove_dir(path).await?;
            }
        } else {
            tokio::fs::remove_file(path).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn names(nodes: &[VNode]) -> Vec<String> {
        let mut n: Vec<String> = nodes.iter().map(|x| x.name.clone()).collect();
        n.sort();
        n
    }

    #[test]
    fn list_dir_reports_every_entry() {
        let tmp = TempDir::new("list");
        tmp.file("a.txt", b"hello");
        tmp.file("b.rs", b"fn main() {}");
        tmp.dir("sub");

        let nodes = list_dir(tmp.path()).expect("list");
        assert_eq!(names(&nodes), vec!["a.txt", "b.rs", "sub"]);
    }

    #[test]
    fn list_dir_populates_size_kind_and_mime() {
        let tmp = TempDir::new("meta");
        tmp.file("a.txt", b"hello");
        tmp.dir("sub");

        let nodes = list_dir(tmp.path()).expect("list");
        let file = nodes.iter().find(|n| n.name == "a.txt").expect("file");
        let dir = nodes.iter().find(|n| n.name == "sub").expect("dir");

        assert_eq!(file.size, 5);
        assert!(!file.is_dir);
        assert_eq!(file.mime_type, "text/plain");
        assert!(file.modified_at > 0);
        assert_eq!(file.path, tmp.path().join("a.txt"));

        assert!(dir.is_dir);
        assert_eq!(dir.mime_type, "inode/directory");
    }

    #[test]
    fn list_dir_scales_to_a_large_directory() {
        // The listing must not be capped anywhere along the way.
        let tmp = TempDir::new("many");
        for i in 0..750 {
            tmp.file(&format!("f{i}.txt"), b"");
        }
        assert_eq!(list_dir(tmp.path()).expect("list").len(), 750);
    }

    #[test]
    fn list_dir_on_an_empty_directory_is_empty_not_an_error() {
        let tmp = TempDir::new("empty");
        assert!(list_dir(tmp.path()).expect("list").is_empty());
    }

    #[test]
    fn describe_list_error_says_why_a_folder_cannot_be_read() {
        let tmp = TempDir::new("list_describe");
        let missing = list_dir(&tmp.path().join("nope")).unwrap_err();
        assert_eq!(describe_list_error(&missing), "the folder no longer exists");
    }

    #[cfg(unix)]
    #[test]
    fn describe_list_error_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("list_denied");
        let locked = tmp.dir("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = list_dir(&locked).unwrap_err();
        assert_eq!(describe_list_error(&err), "permission denied");
    }

    #[test]
    fn list_dir_errors_on_a_missing_directory() {
        let tmp = TempDir::new("missing");
        assert!(list_dir(&tmp.path().join("nope")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn list_dir_describes_a_symlink_as_itself() {
        let tmp = TempDir::new("symlink");
        let target = tmp.dir("real_dir");
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).expect("symlink");

        let nodes = list_dir(tmp.path()).expect("list");
        let link = nodes.iter().find(|n| n.name == "link").expect("link");

        // Symlink-level metadata: the link is not reported as a directory,
        // which is what stops the search walker following it into a loop.
        assert!(link.is_symlink);
        assert!(!link.is_dir);
        assert!(
            is_navigable_directory(link),
            "activating a folder symlink must still enter it"
        );
    }

    /// Creates `n` empty files and returns the directory holding them.
    fn populated(tag: &str, n: usize) -> TempDir {
        let tmp = TempDir::new(tag);
        for i in 0..n {
            std::fs::File::create(tmp.path().join(format!("file_{i:06}.txt"))).expect("create");
        }
        tmp
    }

    /// Benchmark: what a folder open costs before any chunk can paint.
    /// Run with `cargo test --release -- --ignored perf_ --nocapture`.
    /// Times the full blocking listing plus the JSON encoding that Tauri
    /// sends over IPC, for 10k and 100k entries.
    #[test]
    #[ignore = "benchmark: run with --release -- --ignored perf_ --nocapture"]
    fn perf_list_dir_and_serialize() {
        use std::time::Instant;
        for n in [10_000usize, 100_000] {
            let tmp = populated(&format!("perf_list_{n}"), n);
            let _warm = list_dir(tmp.path()).expect("warm");

            let t = Instant::now();
            let nodes = list_dir(tmp.path()).expect("list");
            let list_ms = t.elapsed().as_secs_f64() * 1e3;

            let t = Instant::now();
            let json = serde_json::to_vec(&nodes).expect("serialize");
            let ser_ms = t.elapsed().as_secs_f64() * 1e3;

            eprintln!(
                "PERF full_listing n={n} list_dir={list_ms:.1}ms serialize={ser_ms:.1}ms total={:.1}ms ipc_bytes={}",
                list_ms + ser_ms,
                json.len()
            );
            assert_eq!(nodes.len(), n);
        }
    }

    #[test]
    fn chunked_listing_sends_a_small_first_batch_then_larger_ones() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks");
        for i in 0..25 {
            tmp.file(&format!("f{i:02}.txt"), b"");
        }
        let cancel = AtomicBool::new(false);
        let mut sizes = Vec::new();
        let mut seen = Vec::new();
        let end = list_dir_batched(tmp.path(), &cancel, 4, 10, |batch| {
            sizes.push(batch.len());
            seen.extend(batch.into_iter().map(|n| n.name));
        })
        .expect("list");

        assert_eq!(end, ListEnd::Complete);
        assert_eq!(sizes, vec![4, 10, 10, 1]);
        seen.sort();
        assert_eq!(seen, names(&list_dir(tmp.path()).expect("list")));
    }

    #[test]
    fn chunked_listing_of_a_small_folder_is_one_batch() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks_small");
        tmp.file("a.txt", b"");
        tmp.file("b.txt", b"");
        let cancel = AtomicBool::new(false);
        let mut batches = 0;
        list_dir_chunked(tmp.path(), &cancel, |batch| {
            batches += 1;
            assert_eq!(batch.len(), 2);
        })
        .expect("list");
        assert_eq!(batches, 1);
    }

    #[test]
    fn chunked_listing_of_an_empty_folder_still_reports_completion() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks_empty");
        let cancel = AtomicBool::new(false);
        let mut batches = Vec::new();
        let end = list_dir_chunked(tmp.path(), &cancel, |b| batches.push(b.len())).expect("list");
        assert_eq!(end, ListEnd::Complete);
        assert_eq!(batches, vec![0]);
    }

    #[test]
    fn chunked_listing_of_a_large_folder_keeps_every_entry() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks_big");
        for i in 0..5_500 {
            tmp.file(&format!("f{i}.txt"), b"");
        }
        let cancel = AtomicBool::new(false);
        let mut batches = Vec::new();
        list_dir_chunked(tmp.path(), &cancel, |b| batches.push(b.len())).expect("list");
        assert_eq!(batches.first(), Some(&FIRST_CHUNK));
        assert_eq!(batches.iter().sum::<usize>(), 5_500);
        assert!(batches.len() >= 2);
    }

    #[test]
    fn cancelling_a_listing_stops_it_and_sends_nothing_more() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = TempDir::new("chunks_cancel");
        for i in 0..40 {
            tmp.file(&format!("f{i:02}.txt"), b"");
        }
        // Stale request: the user navigated away after the first batch.
        let cancel = AtomicBool::new(false);
        let mut batches = 0;
        let end = list_dir_batched(tmp.path(), &cancel, 5, 5, |_| {
            batches += 1;
            cancel.store(true, Ordering::Relaxed);
        })
        .expect("list");
        assert_eq!(end, ListEnd::Cancelled);
        assert_eq!(batches, 1, "no batch is sent after cancellation");
    }

    #[test]
    fn a_listing_cancelled_before_it_starts_reads_nothing() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks_precancel");
        tmp.file("a.txt", b"");
        let cancel = AtomicBool::new(true);
        let mut batches = 0;
        let end = list_dir_chunked(tmp.path(), &cancel, |_| batches += 1).expect("list");
        assert_eq!(end, ListEnd::Cancelled);
        assert_eq!(batches, 0);
    }

    #[test]
    fn chunked_listing_reports_a_missing_folder_as_an_error() {
        use std::sync::atomic::AtomicBool;
        let tmp = TempDir::new("chunks_missing");
        let cancel = AtomicBool::new(false);
        let mut called = false;
        let err = list_dir_chunked(&tmp.path().join("nope"), &cancel, |_| called = true);
        assert!(err.is_err());
        assert!(
            !called,
            "no batch is sent for a folder that cannot be opened"
        );
    }

    #[test]
    #[ignore = "benchmark: run with --release -- --ignored perf_ --nocapture"]
    fn perf_first_chunk_latency() {
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;
        for n in [10_000usize, 100_000] {
            let tmp = populated(&format!("perf_chunk_{n}"), n);
            let _warm = list_dir(tmp.path()).expect("warm");
            let cancel = AtomicBool::new(false);

            let t = Instant::now();
            let mut first_ms = None;
            let mut first_bytes = 0;
            let mut total_ms = 0.0;
            let mut chunks = 0;
            list_dir_chunked(tmp.path(), &cancel, |batch| {
                // Encoding is part of what the IPC sends, so time it too.
                let json = serde_json::to_vec(&batch).expect("serialize");
                chunks += 1;
                if first_ms.is_none() {
                    first_ms = Some(t.elapsed().as_secs_f64() * 1e3);
                    first_bytes = json.len();
                }
            })
            .expect("list");
            total_ms += t.elapsed().as_secs_f64() * 1e3;
            eprintln!(
                "PERF first_chunk n={n} first_paint={:.1}ms first_chunk_bytes={first_bytes} chunks={chunks} stream_total={total_ms:.1}ms",
                first_ms.unwrap_or(0.0)
            );
        }
    }

    #[test]
    fn mime_for_is_case_insensitive_and_falls_back() {
        assert_eq!(mime_for(Path::new("a.PNG"), false), "image/png");
        assert_eq!(mime_for(Path::new("a.jpeg"), false), "image/jpeg");
        assert_eq!(
            mime_for(Path::new("a.unknown"), false),
            "application/octet-stream"
        );
        assert_eq!(
            mime_for(Path::new("noext"), false),
            "application/octet-stream"
        );
        assert_eq!(mime_for(Path::new("a.txt"), true), "inode/directory");
    }
}
