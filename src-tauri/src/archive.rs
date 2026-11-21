//! Archives opened as read-only folders.
//!
//! A location inside an archive is an ordinary path: the archive file's path
//! followed by the folders inside it, such as `/home/a.zip/inner/dir`. Every
//! other part of the app already passes paths around as strings, so history,
//! the location bar and "up" carry over unchanged. [`locate`] recovers the
//! split by looking for an archive-named ancestor that is a file.
//!
//! Browsing a zip reads only its central directory. A tar has no index, so
//! listing one decompresses the whole stream, up to the same byte cap that
//! extraction obeys, to reach every header. Entry data is decompressed only to
//! extract it or to preview one file, and never beyond the limits.
//!
//! Extraction plans every write before it starts, so a bad archive writes
//! nothing. It then fills a staging folder inside the destination and renames
//! the finished items into place under names that do not exist yet. Nothing
//! partial is left under a real name, and nothing is overwritten.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::{mime_for, VNode};
use crate::ops::{self, OpKind, OpProgress};
use crate::preview::{self, Preview};

/// Caps that keep a hostile archive from exhausting the disk or the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Entries an archive may hold before it is refused outright.
    pub max_entries: usize,
    /// Bytes extraction may write in total, across all entries.
    pub max_bytes: u64,
}

pub const LIMITS: Limits = Limits {
    max_entries: 100_000,
    max_bytes: 4 * 1024 * 1024 * 1024,
};

/// Dictionary memory an xz stream may ask for, in KiB.
const XZ_MEM_LIMIT_KB: u32 = 256 * 1024;
const COPY_BUF_BYTES: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
}

/// Longest suffixes first, so `.tar.gz` is not read as `.gz`.
const FORMATS: [(&str, Format); 6] = [
    (".tar.gz", Format::TarGz),
    (".tgz", Format::TarGz),
    (".tar.bz2", Format::TarBz2),
    (".tar.xz", Format::TarXz),
    (".tar", Format::Tar),
    (".zip", Format::Zip),
];

/// The name without its archive extension, and the archive's format.
pub fn split_archive_name(name: &str) -> Option<(&str, Format)> {
    let lower = name.to_ascii_lowercase();
    FORMATS
        .iter()
        .find(|(ext, _)| lower.ends_with(*ext))
        .map(|(ext, format)| (&name[..name.len() - ext.len()], *format))
}

fn format_of_path(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_str()?;
    split_archive_name(name).map(|(_, format)| format)
}

/// Where a path sits inside an archive. `inner` is `/`-separated and empty for
/// the archive's root.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Location {
    pub archive: PathBuf,
    pub inner: String,
}

/// The archive a path lies in or is, if any. The archive file itself gives an
/// empty `inner`, which is its root when browsed. A path with a `..` component
/// is not an archive location: `a.zip/..` would otherwise name the archive's
/// root, when it is really the folder holding the archive.
pub fn locate(path: &Path) -> Option<Location> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let mut inside: Vec<String> = Vec::new();
    for ancestor in path.ancestors() {
        let Some(name) = ancestor.file_name() else {
            continue;
        };
        if format_of_path(ancestor).is_some() && ancestor.is_file() {
            inside.reverse();
            return Some(Location {
                archive: ancestor.to_path_buf(),
                inner: inside.join("/"),
            });
        }
        inside.push(name.to_string_lossy().into_owned());
    }
    None
}

/// The path of an entry as the app shows it: the archive's path plus `inner`.
pub fn virtual_path(archive: &Path, inner: &str) -> PathBuf {
    inner
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(archive.to_path_buf(), |path, part| path.join(part))
}

/// Refuse a change to anything inside an archive. The archive file itself is
/// an ordinary file and is not refused.
pub fn ensure_writable<P: AsRef<Path>>(paths: &[P]) -> Result<(), String> {
    for path in paths {
        let path = path.as_ref();
        if locate(path).is_some_and(|loc| !loc.inner.is_empty()) {
            return Err(format!(
                "{} is inside an archive, and archives are read-only",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Whether any of `paths` is inside an archive (not the archive file itself).
pub fn any_inside(paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .any(|p| locate(p).is_some_and(|loc| !loc.inner.is_empty()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Symlink,
    /// Hard links and device nodes: listed nowhere and never recreated.
    Other,
}

/// One entry of an archive, with its name already checked.
#[derive(Clone, Debug)]
struct Meta {
    /// Position in the archive: the zip index, or the tar entry number.
    idx: usize,
    /// Relative, `/`-separated, with no `.` or `..` parts.
    path: String,
    kind: Kind,
    size: u64,
    modified_at: u64,
}

/// Turn an entry name into a relative path, or refuse it. An empty name or
/// `.` (the archive's root) gives `Ok(None)`. Absolute paths, `..` parts,
/// drive or stream syntax and backslashes are refused: the entry could point
/// outside the folder it is extracted into.
fn clean_name(raw: &str) -> Result<Option<String>, String> {
    if raw.starts_with('/') || raw.contains(['\\', ':', '\0']) {
        return Err(unsafe_name(raw));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_name(raw)),
            other => parts.push(other),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

fn unsafe_name(raw: &str) -> String {
    format!("the archive has an entry with an unsafe name ({raw}), so it can't be used")
}

fn not_archive(path: &Path) -> String {
    format!("{} is not a supported archive", path.display())
}

fn too_many_entries(limits: Limits) -> String {
    format!("the archive has more than {} entries", limits.max_entries)
}

fn read_failed(e: impl std::fmt::Display) -> String {
    format!("could not read the archive: {e}")
}

fn open_zip(archive: &Path) -> Result<zip::ZipArchive<BufReader<File>>, String> {
    let file = File::open(archive).map_err(read_failed)?;
    zip::ZipArchive::new(BufReader::new(file)).map_err(read_failed)
}

/// Passes a tar stream through, failing once more than `left` bytes have come
/// out of it. Listing a tar means reading all of it, so this stops a small,
/// highly compressible archive from costing unbounded time and memory.
struct CappedReader<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // One byte past the cap is asked for, so an exact fit and an overrun can be told apart.
        let want = buf
            .len()
            .min(usize::try_from(self.left.saturating_add(1)).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n as u64 > self.left {
            return Err(std::io::Error::other(TOO_LARGE_TO_BROWSE));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

const TOO_LARGE_TO_BROWSE: &str = "archive too large to browse";

/// A tar stream behind the decompressor its format needs, reading at most
/// `max_bytes` out of it.
fn open_tar(
    archive: &Path,
    format: Format,
    max_bytes: u64,
) -> Result<tar::Archive<Box<dyn Read>>, String> {
    let file = BufReader::new(File::open(archive).map_err(read_failed)?);
    let reader: Box<dyn Read> = match format {
        Format::Tar => Box::new(file),
        Format::TarGz => Box::new(flate2::read::GzDecoder::new(file)),
        Format::TarBz2 => Box::new(bzip2::read::BzDecoder::new(file)),
        Format::TarXz => Box::new(lzma_rust2::XzReader::new_mem_limit(
            file,
            true,
            XZ_MEM_LIMIT_KB,
        )),
        Format::Zip => return Err(not_archive(archive)),
    };
    let capped: Box<dyn Read> = Box::new(CappedReader {
        inner: reader,
        left: max_bytes,
    });
    Ok(tar::Archive::new(capped))
}

/// Seconds since the Unix epoch. Zip stores local time without a zone, which
/// is read here as UTC.
fn civil_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> u64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + h * 3_600 + mi * 60 + s).max(0) as u64
}

fn tar_meta<R: Read>(idx: usize, entry: &tar::Entry<'_, R>) -> Result<Option<Meta>, String> {
    let raw = entry.path().map_err(read_failed)?;
    let raw = raw
        .to_str()
        .ok_or_else(|| "the archive has an entry whose name is not UTF-8".to_string())?;
    let Some(path) = clean_name(raw)? else {
        return Ok(None);
    };
    let ty = entry.header().entry_type();
    let kind = if ty.is_dir() {
        Kind::Dir
    } else if ty.is_symlink() {
        Kind::Symlink
    } else if ty.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Ok(Some(Meta {
        idx,
        path,
        kind,
        size: entry.size(),
        modified_at: entry.header().mtime().unwrap_or(0),
    }))
}

/// Every entry of the archive, in archive order, with names checked. Fails on
/// an unsafe name or too many entries, before anything else is done with it.
fn index(archive: &Path, format: Format, limits: Limits) -> Result<Vec<Meta>, String> {
    let mut metas = Vec::new();
    match format {
        Format::Zip => {
            let zip = open_zip(archive)?;
            if zip.len() > limits.max_entries {
                return Err(too_many_entries(limits));
            }
            for idx in 0..zip.len() {
                // `by_index_data` reads the directory record only; nothing is decompressed.
                let entry = zip.by_index_data(idx).map_err(read_failed)?;
                let raw = entry.name().map_err(read_failed)?.into_owned();
                let Some(path) = clean_name(&raw)? else {
                    continue;
                };
                if entry.enclosed_name().is_none() {
                    return Err(unsafe_name(&raw));
                }
                let kind = if entry.is_dir() {
                    Kind::Dir
                } else if entry.is_symlink() {
                    Kind::Symlink
                } else {
                    Kind::File
                };
                let modified_at = entry
                    .last_modified()
                    .map(|d| {
                        civil_secs(
                            d.year().into(),
                            d.month().into(),
                            d.day().into(),
                            d.hour().into(),
                            d.minute().into(),
                            d.second().into(),
                        )
                    })
                    .unwrap_or(0);
                metas.push(Meta {
                    idx,
                    path,
                    kind,
                    size: entry.size(),
                    modified_at,
                });
            }
        }
        _ => {
            let mut tar = open_tar(archive, format, limits.max_bytes)?;
            for (idx, entry) in tar.entries().map_err(read_failed)?.enumerate() {
                if idx >= limits.max_entries {
                    return Err(too_many_entries(limits));
                }
                let entry = entry.map_err(read_failed)?;
                if let Some(meta) = tar_meta(idx, &entry)? {
                    metas.push(meta);
                }
            }
        }
    }
    Ok(metas)
}

fn node(
    archive: &Path,
    parent: &str,
    name: &str,
    kind: Kind,
    size: u64,
    modified_at: u64,
) -> VNode {
    let inner = if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    };
    let path = virtual_path(archive, &inner);
    VNode {
        name: name.to_string(),
        size,
        is_dir: kind == Kind::Dir,
        is_symlink: kind == Kind::Symlink,
        modified_at,
        created_at: 0,
        readonly: true,
        hidden: name.starts_with('.'),
        mime_type: mime_for(&path, kind == Kind::Dir).to_string(),
        path,
    }
}

/// The children of the folder at `inner`, shaped like a directory listing.
fn folder_nodes(metas: &[Meta], archive: &Path, inner: &str) -> Result<Vec<VNode>, String> {
    let prefix = if inner.is_empty() {
        String::new()
    } else {
        format!("{inner}/")
    };
    if !inner.is_empty() {
        match metas.iter().find(|m| m.path == inner) {
            Some(m) if m.kind == Kind::Dir => {}
            Some(_) => return Err(format!("{inner} is not a folder")),
            None if metas.iter().any(|m| m.path.starts_with(&prefix)) => {}
            None => return Err(format!("{inner} is not in the archive")),
        }
    }
    // Folders that have no entry of their own still exist: they are implied
    // by the paths of the entries inside them.
    let mut children: BTreeMap<String, VNode> = BTreeMap::new();
    for m in metas {
        let Some(rest) = m.path.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if rest.is_empty() || m.kind == Kind::Other {
            continue;
        }
        match rest.split_once('/') {
            Some((dir, _)) => {
                children
                    .entry(dir.to_string())
                    .or_insert_with(|| node(archive, inner, dir, Kind::Dir, 0, 0));
            }
            None => {
                children.insert(
                    rest.to_string(),
                    node(archive, inner, rest, m.kind, m.size, m.modified_at),
                );
            }
        }
    }
    Ok(children.into_values().collect())
}

/// The rows of the folder at `loc`, read-only and with the uncompressed size.
pub fn list(loc: &Location, limits: Limits) -> Result<Vec<VNode>, String> {
    let format = format_of_path(&loc.archive).ok_or_else(|| not_archive(&loc.archive))?;
    let metas = index(&loc.archive, format, limits)?;
    folder_nodes(&metas, &loc.archive, &loc.inner)
}

/// The first `limit` bytes of the file at `loc`, with its full size.
pub fn read_file(loc: &Location, limit: u64) -> Result<(u64, Vec<u8>), String> {
    let format = format_of_path(&loc.archive).ok_or_else(|| not_archive(&loc.archive))?;
    let mut buf = Vec::new();
    match format {
        Format::Zip => {
            let metas = index(&loc.archive, format, LIMITS)?;
            let meta = metas
                .iter()
                .find(|m| m.path == loc.inner && m.kind == Kind::File)
                .ok_or_else(|| format!("{} is not a file in the archive", loc.inner))?;
            let mut zip = open_zip(&loc.archive)?;
            let mut entry = zip.by_index(meta.idx).map_err(read_failed)?;
            (&mut entry)
                .take(limit)
                .read_to_end(&mut buf)
                .map_err(read_failed)?;
            Ok((meta.size, buf))
        }
        _ => {
            let mut tar = open_tar(&loc.archive, format, LIMITS.max_bytes)?;
            for (idx, entry) in tar.entries().map_err(read_failed)?.enumerate() {
                if idx >= LIMITS.max_entries {
                    return Err(too_many_entries(LIMITS));
                }
                let mut entry = entry.map_err(read_failed)?;
                let Some(meta) = tar_meta(idx, &entry)? else {
                    continue;
                };
                if meta.path == loc.inner && meta.kind == Kind::File {
                    (&mut entry)
                        .take(limit)
                        .read_to_end(&mut buf)
                        .map_err(read_failed)?;
                    return Ok((meta.size, buf));
                }
            }
            Err(format!("{} is not in the archive", loc.inner))
        }
    }
}

/// A preview of a file inside an archive, read as far as the preview needs.
pub fn preview(loc: &Location) -> Preview {
    let name = loc.inner.rsplit('/').next().unwrap_or_default();
    match read_file(loc, preview::MAX_PREVIEW_BYTES) {
        Ok((size, bytes)) => preview::decode_entry_preview(Path::new(name), size, &bytes),
        Err(_) => Preview::None,
    }
}

/// What to pull out of an archive: the entry or folder at `inner` (`""` for
/// the whole archive), landing in the destination folder as `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub inner: String,
    pub name: String,
}

/// Progress of an extraction.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
}

#[derive(Debug, Default)]
pub struct Extracted {
    /// `(source inside the archive, new path)` for each selection, for undo.
    pub outcomes: Vec<(PathBuf, PathBuf)>,
    /// Symlinks and special entries that were left out.
    pub skipped: u64,
}

/// A selection rejects names that are not one safe component, and selections
/// that overlap or share a name: each entry must be claimed by one selection.
fn validate_selections(selections: &[Selection]) -> Result<(), String> {
    let mut names = HashSet::new();
    for sel in selections {
        if sel.name.contains('/') || clean_name(&sel.name)?.as_deref() != Some(sel.name.as_str()) {
            return Err(format!("{:?} is not a usable name", sel.name));
        }
        if !names.insert(sel.name.as_str()) {
            return Err(format!("two selected items are both called {}", sel.name));
        }
        if !sel.inner.is_empty() && clean_name(&sel.inner)?.as_deref() != Some(sel.inner.as_str()) {
            return Err(format!("{:?} is not a usable path", sel.inner));
        }
    }
    for (i, a) in selections.iter().enumerate() {
        for b in &selections[i + 1..] {
            if is_within(&a.inner, &b.inner) || is_within(&b.inner, &a.inner) {
                return Err("the selected items overlap".into());
            }
        }
    }
    Ok(())
}

/// Whether `inner` is `outer` or lies under it. The archive root covers everything.
fn is_within(inner: &str, outer: &str) -> bool {
    outer.is_empty() || inner == outer || inner.starts_with(&format!("{outer}/"))
}

/// Where an entry lands under the staging folder, if the selection covers it.
fn route(path: &str, sel: &Selection) -> Option<String> {
    if sel.inner.is_empty() {
        return Some(format!("{}/{path}", sel.name));
    }
    if path == sel.inner {
        return Some(sel.name.clone());
    }
    path.strip_prefix(sel.inner.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| format!("{}/{rest}", sel.name))
}

/// One entry to write, with the path it lands at under the staging folder.
struct Wanted {
    idx: usize,
    kind: Kind,
    size: u64,
    rel: String,
}

struct Tally {
    budget: u64,
    bytes: u64,
    files: u64,
    skipped: u64,
    max_bytes: u64,
}

fn copy_capped(
    src: &mut dyn Read,
    dst: &mut File,
    t: &mut Tally,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut buf = vec![0u8; COPY_BUF_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = src.read(&mut buf).map_err(read_failed)?;
        if n == 0 {
            return Ok(());
        }
        let n = n as u64;
        // The declared sizes were checked up front; this counts what is really
        // produced, so an entry that expands past its header still stops here.
        if n > t.budget {
            return Err(format!(
                "the archive expands past the {} byte extraction limit",
                t.max_bytes
            ));
        }
        t.budget -= n;
        t.bytes += n;
        dst.write_all(&buf[..n as usize])
            .map_err(|e| format!("could not write: {e}"))?;
    }
}

/// Create one entry under the staging folder. Files are created with
/// `create_new`, so an existing path is never overwritten.
fn place(
    t: &mut Tally,
    kind: Kind,
    dest: &Path,
    src: Option<&mut dyn Read>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    match (kind, src) {
        (Kind::Dir, _) => {
            fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?
        }
        (Kind::File, Some(src)) => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                .map_err(|e| format!("{}: {e}", dest.display()))?;
            copy_capped(src, &mut out, t, cancel)?;
            t.files += 1;
        }
        // Symlinks and special entries are never recreated: a link could point
        // anywhere, and the app does not follow links out of an archive.
        _ => t.skipped += 1,
    }
    Ok(())
}

/// Remove a path the extraction created, whatever it is.
fn remove_created(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Plan the writes: which entries each selection covers, and whether they fit
/// the limits. Nothing is written here.
fn plan(metas: &[Meta], selections: &[Selection], limits: Limits) -> Result<Vec<Wanted>, String> {
    let mut wanted = Vec::new();
    for sel in selections {
        let before = wanted.len();
        for m in metas {
            if let Some(rel) = route(&m.path, sel) {
                wanted.push(Wanted {
                    idx: m.idx,
                    kind: m.kind,
                    size: m.size,
                    rel,
                });
            }
        }
        if !sel.inner.is_empty() && wanted.len() == before {
            return Err(format!("{} is not in the archive", sel.inner));
        }
    }
    let declared = wanted
        .iter()
        .filter(|w| w.kind == Kind::File)
        .fold(0u64, |a, w| a.saturating_add(w.size));
    if declared > limits.max_bytes {
        return Err(format!(
            "the archive is too large to extract ({declared} bytes; the limit is {} bytes)",
            limits.max_bytes
        ));
    }
    Ok(wanted)
}

/// Write every planned entry into `staging`.
fn write_staged(
    archive: &Path,
    format: Format,
    wanted: &[Wanted],
    staging: &Path,
    limits: Limits,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Tally, String> {
    let mut t = Tally {
        budget: limits.max_bytes,
        bytes: 0,
        files: 0,
        skipped: 0,
        max_bytes: limits.max_bytes,
    };
    let files_total = wanted.iter().filter(|w| w.kind == Kind::File).count() as u64;
    let bytes_total = wanted
        .iter()
        .filter(|w| w.kind == Kind::File)
        .fold(0u64, |a, w| a.saturating_add(w.size));
    let mut report = |t: &Tally| {
        progress(Progress {
            bytes_done: t.bytes,
            bytes_total,
            files_done: t.files,
            files_total,
        })
    };

    match format {
        Format::Zip => {
            let mut zip = open_zip(archive)?;
            for w in wanted {
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                let dest = staging.join(&w.rel);
                if w.kind == Kind::File {
                    let mut entry = zip.by_index(w.idx).map_err(read_failed)?;
                    place(&mut t, w.kind, &dest, Some(&mut entry), cancel)?;
                } else {
                    place(&mut t, w.kind, &dest, None, cancel)?;
                }
                report(&t);
            }
        }
        _ => {
            let by_idx: HashMap<usize, &Wanted> = wanted.iter().map(|w| (w.idx, w)).collect();
            let mut tar = open_tar(archive, format, limits.max_bytes)?;
            for (idx, entry) in tar.entries().map_err(read_failed)?.enumerate() {
                let Some(w) = by_idx.get(&idx) else {
                    continue;
                };
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                let mut entry = entry.map_err(read_failed)?;
                let dest = staging.join(&w.rel);
                if w.kind == Kind::File {
                    place(&mut t, w.kind, &dest, Some(&mut entry), cancel)?;
                } else {
                    place(&mut t, w.kind, &dest, None, cancel)?;
                }
                report(&t);
            }
        }
    }
    Ok(t)
}

/// The whole archive goes in a folder of its own, even when it turns out empty.
fn make_roots<'a>(
    staging: &Path,
    roots: impl Iterator<Item = &'a Selection>,
) -> Result<(), String> {
    for sel in roots {
        let folder = staging.join(&sel.name);
        fs::create_dir(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    Ok(())
}

/// Move the finished selections from staging to their final names.
fn place_all(
    archive: &Path,
    selections: &[Selection],
    staging: &Path,
    dest_dir: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut placed: Vec<PathBuf> = Vec::new();
    let mut outcomes = Vec::new();
    for sel in selections {
        let src = staging.join(&sel.name);
        // `unique_sibling` names a copy, so it is only used when the name is taken.
        // Both checks look at the entry itself, so a dangling symlink counts as taken.
        let wanted = dest_dir.join(&sel.name);
        let dest = if ops::path_occupied(&wanted) {
            ops::unique_sibling(&wanted)
        } else {
            wanted
        };
        // `rename_noreplace` refuses an existing `dest`, which also covers an
        // entry created after the check above.
        if let Err(e) = crate::desktop::rename_noreplace(&src, &dest) {
            for path in &placed {
                remove_created(path);
            }
            return Err(format!("could not move {} into place: {e}", sel.name));
        }
        let source = if sel.inner.is_empty() {
            archive.to_path_buf()
        } else {
            virtual_path(archive, &sel.inner)
        };
        placed.push(dest.clone());
        outcomes.push((source, dest));
    }
    Ok(outcomes)
}

/// Extract the selections into `dest_dir`. Each lands under a name that does
/// not exist yet, so nothing is overwritten. On failure or cancel, nothing is
/// left in `dest_dir`.
pub fn extract(
    archive: &Path,
    selections: &[Selection],
    dest_dir: &Path,
    limits: Limits,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Extracted, String> {
    let format = format_of_path(archive).ok_or_else(|| not_archive(archive))?;
    validate_selections(selections)?;
    let metas = index(archive, format, limits)?;
    let wanted = plan(&metas, selections, limits)?;

    let staging = dest_dir.join(format!(".ferris-extract-{}", ops::next_id()));
    fs::create_dir(&staging)
        .map_err(|e| format!("could not work in {}: {e}", dest_dir.display()))?;
    let roots = selections.iter().filter(|sel| sel.inner.is_empty());
    let result = make_roots(&staging, roots)
        .and_then(|()| write_staged(archive, format, &wanted, &staging, limits, cancel, progress))
        .and_then(|t| {
            let outcomes = place_all(archive, selections, &staging, dest_dir)?;
            Ok(Extracted {
                outcomes,
                skipped: t.skipped,
            })
        });
    let _ = fs::remove_dir_all(&staging);
    result
}

/// The selection for a whole archive: everything in a folder named after it.
pub fn whole_archive(archive: &Path) -> Result<Selection, String> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| not_archive(archive))?;
    let (stem, _) = split_archive_name(name).ok_or_else(|| not_archive(archive))?;
    if !archive.is_file() {
        return Err(format!("{} is not a file", archive.display()));
    }
    validate_selections(&[Selection {
        inner: String::new(),
        name: stem.to_string(),
    }])
    .map_err(|e| format!("can't extract {name}: {e}"))?;
    Ok(Selection {
        inner: String::new(),
        name: stem.to_string(),
    })
}

/// The archive and selections for copying items out of an archive. `None`
/// when none of `sources` is inside one.
pub fn copy_out_request(sources: &[PathBuf]) -> Result<Option<(PathBuf, Vec<Selection>)>, String> {
    let inside: Vec<Location> = sources
        .iter()
        .filter_map(|p| locate(p))
        .filter(|loc| !loc.inner.is_empty())
        .collect();
    if inside.is_empty() {
        return Ok(None);
    }
    if inside.len() != sources.len() {
        return Err("copy items from an archive separately from files outside it".into());
    }
    let archive = inside[0].archive.clone();
    if inside.iter().any(|loc| loc.archive != archive) {
        return Err("copy items from one archive at a time".into());
    }
    let selections = inside
        .into_iter()
        .map(|loc| {
            let name = loc.inner.rsplit('/').next().unwrap_or_default().to_string();
            Selection {
                inner: loc.inner,
                name,
            }
        })
        .collect();
    Ok(Some((archive, selections)))
}

/// Run [`extract`] on a thread and report it as an operation: progress and the
/// final outcome go out on `tx` the way a copy's do.
pub fn spawn_extract(
    id: u64,
    kind: OpKind,
    archive: PathBuf,
    selections: Vec<Selection>,
    dest_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<OpProgress>,
) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let send = |p: Progress,
                    error: Option<String>,
                    done: bool,
                    skipped: u64,
                    outcomes: Vec<(PathBuf, PathBuf)>| {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let message = if done {
                match error.as_deref() {
                    Some("cancelled") => "Cancelled",
                    Some(_) => "Failed",
                    None if kind == OpKind::Extract => "Extracted",
                    None => "Copied",
                }
            } else {
                "Extracting..."
            };
            let _ = tx.send(OpProgress {
                id,
                kind,
                message: message.into(),
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                files_done: p.files_done,
                files_total: p.files_total,
                error,
                done,
                bytes_per_sec: (p.bytes_done as f64 / elapsed) as u64,
                skipped,
                conflict: None,
                outcomes,
            });
        };

        let mut last_sent: Option<Instant> = None;
        let mut latest = Progress::default();
        let result = extract(
            &archive,
            &selections,
            &dest_dir,
            LIMITS,
            &cancel,
            &mut |p| {
                latest = p;
                if last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last_sent = Some(Instant::now());
                    send(p, None, false, 0, Vec::new());
                }
            },
        );
        match result {
            Ok(done) => send(latest, None, true, done.skipped, done.outcomes),
            Err(e) => send(latest, Some(e), true, 0, Vec::new()),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    static NO_CANCEL: AtomicBool = AtomicBool::new(false);

    fn tiny_limits(max_entries: usize, max_bytes: u64) -> Limits {
        Limits {
            max_entries,
            max_bytes,
        }
    }

    fn no_progress(_: Progress) {}

    /// A zip with `(name, Some(bytes))` for files, `(name, None)` for folders.
    fn make_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
        let file = File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            match data {
                Some(bytes) => {
                    zip.start_file(*name, opts).expect("start file");
                    zip.write_all(bytes).expect("write entry");
                }
                None => zip.add_directory(*name, opts).expect("add dir"),
            }
        }
        zip.finish().expect("finish zip");
    }

    /// A zip symlink entry pointing at `target`.
    fn make_zip_with_symlink(path: &Path, name: &str, target: &str, extra: &[(&str, &[u8])]) {
        let file = File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.add_symlink(name, target, opts).expect("symlink");
        for (n, bytes) in extra {
            zip.start_file(*n, opts).expect("start file");
            zip.write_all(bytes).expect("write entry");
        }
        zip.finish().expect("finish zip");
    }

    /// Write `name` into the header's name field verbatim, so hostile names
    /// (`../x`, absolute paths) can be crafted without the tar crate's checks.
    fn raw_header(name: &str, size: u64, ty: tar::EntryType) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        let bytes = name.as_bytes();
        h.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        h.set_size(size);
        h.set_mode(0o644);
        h.set_entry_type(ty);
        h.set_cksum();
        h
    }

    /// Tar entries: files with their bytes, folders, and symlinks with their target.
    enum TarItem<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
    }

    /// Build the tar in memory, then compress it the way the format needs and
    /// finish the compressor explicitly.
    fn make_tar(path: &Path, format: Format, items: &[TarItem]) {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for item in items {
                match item {
                    TarItem::File(name, bytes) => {
                        let h = raw_header(name, bytes.len() as u64, tar::EntryType::Regular);
                        builder.append(&h, *bytes).expect("append file");
                    }
                    TarItem::Dir(name) => {
                        let h = raw_header(name, 0, tar::EntryType::Directory);
                        builder.append(&h, std::io::empty()).expect("append dir");
                    }
                    TarItem::Symlink(name, target) => {
                        let mut h = raw_header(name, 0, tar::EntryType::Symlink);
                        h.set_link_name_literal(target).expect("link name");
                        h.set_cksum();
                        builder.append(&h, std::io::empty()).expect("append link");
                    }
                }
            }
            builder.finish().expect("finish tar");
        }
        let file = File::create(path).expect("create tar");
        match format {
            Format::Tar => (&file).write_all(&raw).expect("write tar"),
            Format::TarGz => {
                let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
                enc.write_all(&raw).expect("gzip");
                enc.finish().expect("finish gzip");
            }
            Format::TarBz2 => {
                let mut enc = bzip2::write::BzEncoder::new(file, bzip2::Compression::fast());
                enc.write_all(&raw).expect("bzip2");
                enc.finish().expect("finish bzip2");
            }
            Format::TarXz => {
                let mut enc =
                    lzma_rust2::XzWriter::new(file, lzma_rust2::XzOptions::with_preset(1))
                        .expect("xz");
                enc.write_all(&raw).expect("xz write");
                enc.finish().expect("finish xz");
            }
            Format::Zip => panic!("not a tar"),
        }
    }

    fn names(nodes: &[VNode]) -> Vec<String> {
        let mut n: Vec<String> = nodes.iter().map(|x| x.name.clone()).collect();
        n.sort();
        n
    }

    fn loc(archive: &Path, inner: &str) -> Location {
        Location {
            archive: archive.to_path_buf(),
            inner: inner.to_string(),
        }
    }

    fn sel(inner: &str, name: &str) -> Selection {
        Selection {
            inner: inner.into(),
            name: name.into(),
        }
    }

    fn dir_is_empty(path: &Path) -> bool {
        fs::read_dir(path).expect("read dir").next().is_none()
    }

    #[test]
    fn clean_name_accepts_plain_relative_names_and_refuses_escapes() {
        assert_eq!(clean_name("a/b.txt").unwrap(), Some("a/b.txt".into()));
        assert_eq!(clean_name("./a//b/").unwrap(), Some("a/b".into()));
        assert_eq!(clean_name("./").unwrap(), None);
        assert!(clean_name("../evil").is_err());
        assert!(clean_name("a/../../evil").is_err());
        assert!(clean_name("/etc/passwd").is_err());
        assert!(clean_name("C:evil").is_err());
        assert!(clean_name("..\\evil").is_err());
        assert!(clean_name("a\0b").is_err());
    }

    #[test]
    fn civil_secs_matches_known_epochs() {
        assert_eq!(civil_secs(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(civil_secs(2000, 1, 1, 0, 0, 0), 946_684_800);
        assert_eq!(civil_secs(2024, 2, 29, 12, 0, 0), 1_709_208_000);
    }

    #[test]
    fn split_archive_name_knows_each_extension() {
        assert_eq!(split_archive_name("a.zip"), Some(("a", Format::Zip)));
        assert_eq!(split_archive_name("a.tar.gz"), Some(("a", Format::TarGz)));
        assert_eq!(split_archive_name("a.TGZ"), Some(("a", Format::TarGz)));
        assert_eq!(split_archive_name("a.tar.bz2"), Some(("a", Format::TarBz2)));
        assert_eq!(split_archive_name("a.tar.xz"), Some(("a", Format::TarXz)));
        assert_eq!(split_archive_name("a.tar"), Some(("a", Format::Tar)));
        assert_eq!(split_archive_name("notes.txt"), None);
    }

    #[test]
    fn locate_splits_a_path_at_the_archive_file() {
        let tmp = TempDir::new("locate");
        let archive = tmp.file("a.zip", b"not really read here");
        tmp.dir("plain.zip");

        let inside = locate(&archive.join("inner").join("dir")).expect("inside");
        assert_eq!(inside, loc(&archive, "inner/dir"));
        assert_eq!(locate(&archive).expect("root"), loc(&archive, ""));
        assert_eq!(locate(tmp.path()), None);
        // A real folder that happens to be named like an archive is just a folder.
        assert_eq!(locate(&tmp.path().join("plain.zip").join("x")), None);
    }

    #[test]
    fn locate_refuses_paths_with_parent_components() {
        let tmp = TempDir::new("locate_parent");
        let archive = tmp.file("a.zip", b"x");
        assert_eq!(locate(&archive.join("..")), None);
        assert_eq!(locate(&archive.join("inner").join("..")), None);
        assert_eq!(locate(&archive.join("..").join("a.zip")), None);
    }

    #[test]
    fn virtual_path_round_trips_through_locate() {
        let tmp = TempDir::new("virtual");
        let archive = tmp.file("a.tar.gz", b"x");
        let path = virtual_path(&archive, "one/two");
        assert_eq!(locate(&path), Some(loc(&archive, "one/two")));
    }

    #[test]
    fn lists_nested_entries_of_a_zip() {
        let tmp = TempDir::new("zip_list");
        let archive = tmp.path().join("a.zip");
        make_zip(
            &archive,
            &[
                ("top.txt", Some(b"hello")),
                ("sub/", None),
                ("sub/deep.txt", Some(b"12345678")),
                ("implied/x/y.txt", Some(b"z")),
            ],
        );

        let root = list(&loc(&archive, ""), LIMITS).expect("root");
        assert_eq!(names(&root), vec!["implied", "sub", "top.txt"]);
        let sub = root.iter().find(|n| n.name == "sub").expect("sub");
        assert!(sub.is_dir);
        assert!(sub.readonly);
        assert_eq!(sub.path, archive.join("sub"));
        let top = root.iter().find(|n| n.name == "top.txt").expect("top");
        assert!(!top.is_dir);
        assert_eq!(top.size, 5);
        assert_eq!(top.path, archive.join("top.txt"));

        let implied = root.iter().find(|n| n.name == "implied").expect("implied");
        assert!(
            implied.is_dir,
            "a folder with no entry of its own still shows"
        );

        let under_sub = list(&loc(&archive, "sub"), LIMITS).expect("sub");
        assert_eq!(names(&under_sub), vec!["deep.txt"]);
        assert_eq!(under_sub[0].size, 8);
        assert_eq!(under_sub[0].path, archive.join("sub").join("deep.txt"));

        let deep = list(&loc(&archive, "implied/x"), LIMITS).expect("nested");
        assert_eq!(names(&deep), vec!["y.txt"]);
    }

    #[test]
    fn lists_nested_entries_of_every_tar_format() {
        let tmp = TempDir::new("tar_list");
        for (format, file) in [
            (Format::Tar, "a.tar"),
            (Format::TarGz, "a.tar.gz"),
            (Format::TarBz2, "a.tar.bz2"),
            (Format::TarXz, "a.tar.xz"),
        ] {
            let archive = tmp.path().join(file);
            make_tar(
                &archive,
                format,
                &[
                    TarItem::Dir("sub"),
                    TarItem::File("sub/deep.txt", b"deep"),
                    TarItem::File("top.txt", b"top!!"),
                ],
            );
            let root = list(&loc(&archive, ""), LIMITS).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(names(&root), vec!["sub", "top.txt"], "{file}");
            let sub = list(&loc(&archive, "sub"), LIMITS).expect("sub");
            assert_eq!(names(&sub), vec!["deep.txt"], "{file}");
            assert_eq!(sub[0].size, 4, "{file}");
        }
    }

    #[test]
    fn listing_a_file_or_a_missing_name_is_an_error() {
        let tmp = TempDir::new("list_errors");
        let archive = tmp.path().join("a.zip");
        make_zip(&archive, &[("f.txt", Some(b"x"))]);
        assert!(list(&loc(&archive, "f.txt"), LIMITS).is_err());
        assert!(list(&loc(&archive, "nope"), LIMITS).is_err());
    }

    #[test]
    fn browsing_a_tar_stops_at_the_byte_cap() {
        let tmp = TempDir::new("tar_bomb");
        let archive = tmp.path().join("bomb.tar.gz");
        // 2 MiB of zeros is a few kilobytes once compressed.
        make_tar(
            &archive,
            Format::TarGz,
            &[TarItem::File("zeros.bin", &vec![0u8; 2 * 1024 * 1024])],
        );
        assert!(fs::metadata(&archive).expect("stat").len() < 64 * 1024);

        let err = list(&loc(&archive, ""), tiny_limits(100, 64 * 1024)).unwrap_err();
        assert!(err.contains(TOO_LARGE_TO_BROWSE), "{err}");
        assert!(list(&loc(&archive, ""), LIMITS).is_ok());
    }

    #[test]
    fn xz_reader_refuses_a_dictionary_over_its_cap() {
        let mut options = lzma_rust2::XzOptions::with_preset(1);
        options.lzma_options.dict_size = 8 << 20;
        let mut raw = Vec::new();
        {
            let mut enc = lzma_rust2::XzWriter::new(&mut raw, options).expect("xz");
            enc.write_all(b"hello, dictionary").expect("xz write");
            enc.finish().expect("finish xz");
        }

        // The same stream decodes under the real cap and is refused under a 1 MiB one.
        let mut out = Vec::new();
        lzma_rust2::XzReader::new_mem_limit(&raw[..], true, XZ_MEM_LIMIT_KB)
            .read_to_end(&mut out)
            .expect("decodes under the real cap");
        assert_eq!(out, b"hello, dictionary");

        let mut out = Vec::new();
        assert!(lzma_rust2::XzReader::new_mem_limit(&raw[..], true, 1024)
            .read_to_end(&mut out)
            .is_err());
    }

    #[test]
    fn preview_reads_a_text_entry_inside_a_zip() {
        let tmp = TempDir::new("preview");
        let archive = tmp.path().join("a.zip");
        make_zip(
            &archive,
            &[("notes/readme.txt", Some(b"first line\nsecond"))],
        );
        match preview(&loc(&archive, "notes/readme.txt")) {
            Preview::Text { lines, .. } => assert_eq!(lines, vec!["first line", "second"]),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn extracts_a_whole_archive_under_a_fresh_name() {
        let tmp = TempDir::new("extract_whole");
        let archive = tmp.path().join("pack.zip");
        make_zip(
            &archive,
            &[("a.txt", Some(b"AAA")), ("dir/b.txt", Some(b"BB"))],
        );
        let dest = tmp.dir("out");
        // A folder named like the archive already exists: nothing may be overwritten.
        let existing = tmp.file("out/pack/keep.txt", b"mine");

        let done = extract(
            &archive,
            &[sel("", "pack")],
            &dest,
            LIMITS,
            &NO_CANCEL,
            &mut no_progress,
        )
        .expect("extract");

        assert_eq!(done.outcomes.len(), 1);
        let (src, made) = &done.outcomes[0];
        assert_eq!(src, &archive);
        assert_eq!(made, &dest.join("pack copy"));
        assert_eq!(fs::read(made.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(fs::read(made.join("dir").join("b.txt")).unwrap(), b"BB");
        assert_eq!(
            fs::read(&existing).unwrap(),
            b"mine",
            "existing file untouched"
        );
        let leftovers: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".ferris-extract"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging folder removed: {leftovers:?}"
        );
    }

    #[test]
    fn copies_selected_entries_out_of_a_tar_gz_into_a_folder() {
        let tmp = TempDir::new("extract_selected");
        let archive = tmp.path().join("a.tar.gz");
        make_tar(
            &archive,
            Format::TarGz,
            &[
                TarItem::Dir("sub"),
                TarItem::File("sub/one.txt", b"1"),
                TarItem::File("sub/inner/two.txt", b"22"),
                TarItem::File("other.txt", b"skip me"),
            ],
        );
        let dest = tmp.dir("out");
        let done = extract(
            &archive,
            &[sel("sub", "sub"), sel("other.txt", "other.txt")],
            &dest,
            LIMITS,
            &NO_CANCEL,
            &mut no_progress,
        )
        .expect("extract");

        assert_eq!(fs::read(dest.join("sub").join("one.txt")).unwrap(), b"1");
        assert_eq!(
            fs::read(dest.join("sub").join("inner").join("two.txt")).unwrap(),
            b"22"
        );
        assert_eq!(fs::read(dest.join("other.txt")).unwrap(), b"skip me");
        assert_eq!(done.outcomes[0].0, archive.join("sub"));
        assert_eq!(done.outcomes[1].1, dest.join("other.txt"));
    }

    #[test]
    fn copying_a_file_out_never_overwrites_a_file_of_the_same_name() {
        let tmp = TempDir::new("extract_no_overwrite");
        let archive = tmp.path().join("a.zip");
        make_zip(&archive, &[("note.txt", Some(b"from archive"))]);
        let dest = tmp.dir("out");
        tmp.file("out/note.txt", b"original");

        let done = extract(
            &archive,
            &[sel("note.txt", "note.txt")],
            &dest,
            LIMITS,
            &NO_CANCEL,
            &mut no_progress,
        )
        .expect("extract");

        assert_eq!(fs::read(dest.join("note.txt")).unwrap(), b"original");
        assert_eq!(
            fs::read(dest.join("note copy.txt")).unwrap(),
            b"from archive"
        );
        assert_eq!(done.outcomes[0].1, dest.join("note copy.txt"));
    }

    #[test]
    fn zip_slip_entries_are_refused_and_nothing_is_written() {
        let tmp = TempDir::new("zip_slip");
        let dest = tmp.dir("dest");
        let abs = tmp.path().join("abs_evil.txt");
        let abs_name = abs.to_string_lossy().into_owned();

        let traversal = tmp.path().join("traversal.zip");
        make_zip(
            &traversal,
            &[("ok.txt", Some(b"fine")), ("../evil.txt", Some(b"pwned"))],
        );
        let absolute = tmp.path().join("absolute.zip");
        make_zip(
            &absolute,
            &[("ok.txt", Some(b"fine")), (&abs_name, Some(b"pwned"))],
        );

        for archive in [&traversal, &absolute] {
            assert!(
                list(&loc(archive, ""), LIMITS).is_err(),
                "{archive:?} listed"
            );
            let err = extract(
                archive,
                &[sel("", "x")],
                &dest,
                LIMITS,
                &NO_CANCEL,
                &mut no_progress,
            );
            assert!(err.is_err(), "{archive:?} extracted");
        }
        assert!(!tmp.path().join("evil.txt").exists(), "escaped via ..");
        assert!(!abs.exists(), "escaped via absolute path");
        assert!(dir_is_empty(&dest), "a refused archive writes nothing");
    }

    #[test]
    fn tar_slip_entries_are_refused_and_nothing_is_written() {
        let tmp = TempDir::new("tar_slip");
        let dest = tmp.dir("dest");
        let abs = tmp.path().join("abs_evil.txt");
        let abs_name = abs.to_string_lossy().into_owned();
        for (file, name) in [
            ("traversal.tar", "../evil.txt".to_string()),
            ("absolute.tar", abs_name.clone()),
        ] {
            let archive = tmp.path().join(file);
            make_tar(
                &archive,
                Format::Tar,
                &[
                    TarItem::File("ok.txt", b"fine"),
                    TarItem::File(&name, b"pwned"),
                ],
            );
            assert!(list(&loc(&archive, ""), LIMITS).is_err(), "{file} listed");
            let err = extract(
                &archive,
                &[sel("", "x")],
                &dest,
                LIMITS,
                &NO_CANCEL,
                &mut no_progress,
            );
            assert!(err.is_err(), "{file} extracted");
        }
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!abs.exists());
        assert!(dir_is_empty(&dest));
    }

    #[test]
    fn symlinks_are_never_recreated_or_followed() {
        let tmp = TempDir::new("symlinks");
        let outside = tmp.dir("outside");
        let dest = tmp.dir("dest");
        let target = outside.to_string_lossy().into_owned();

        let zipped = tmp.path().join("links.zip");
        make_zip_with_symlink(
            &zipped,
            "link",
            &target,
            &[("link/planted.txt", b"planted")],
        );
        let done = extract(
            &zipped,
            &[sel("", "links")],
            &dest,
            LIMITS,
            &NO_CANCEL,
            &mut no_progress,
        )
        .expect("zip with symlink extracts");
        assert_eq!(done.skipped, 1, "the symlink is skipped");
        assert!(dir_is_empty(&outside), "nothing written through the link");
        // The later "link/planted.txt" entry makes a real folder of that name,
        // which is fine; what must not happen is a link being recreated.
        let made = dest.join("links").join("link");
        assert!(!fs::symlink_metadata(&made).is_ok_and(|m| m.file_type().is_symlink()));

        let tarred = tmp.path().join("links.tar");
        make_tar(
            &tarred,
            Format::Tar,
            &[
                TarItem::Symlink("link", &target),
                TarItem::File("link/planted.txt", b"planted"),
            ],
        );
        let dest2 = tmp.dir("dest2");
        let done = extract(
            &tarred,
            &[sel("", "links")],
            &dest2,
            LIMITS,
            &NO_CANCEL,
            &mut no_progress,
        )
        .expect("tar with symlink extracts");
        assert_eq!(done.skipped, 1);
        assert!(
            dir_is_empty(&outside),
            "nothing written through the tar link"
        );
        let listed = list(&loc(&tarred, ""), LIMITS).expect("listing shows the link");
        assert!(listed
            .iter()
            .any(|n| n.name == "link" && n.is_symlink && !n.is_dir));
    }

    #[test]
    fn entry_count_cap_refuses_a_large_archive() {
        let tmp = TempDir::new("entry_cap");
        let archive = tmp.path().join("many.zip");
        let names: Vec<String> = (0..5).map(|i| format!("f{i}.txt")).collect();
        let entries: Vec<(&str, Option<&[u8]>)> = names
            .iter()
            .map(|n| (n.as_str(), Some(&b"x"[..])))
            .collect();
        make_zip(&archive, &entries);

        let small = tiny_limits(3, LIMITS.max_bytes);
        let err = list(&loc(&archive, ""), small).unwrap_err();
        assert!(err.contains("more than 3 entries"), "{err}");
        let dest = tmp.dir("out");
        assert!(extract(
            &archive,
            &[sel("", "many")],
            &dest,
            small,
            &NO_CANCEL,
            &mut no_progress
        )
        .is_err());
        assert!(dir_is_empty(&dest));
        assert!(list(&loc(&archive, ""), LIMITS).is_ok());
    }

    #[test]
    fn byte_cap_refuses_before_anything_is_written() {
        let tmp = TempDir::new("byte_cap");
        let archive = tmp.path().join("big.zip");
        make_zip(
            &archive,
            &[
                ("a.bin", Some(&[0u8; 10][..])),
                ("b.bin", Some(&[0u8; 10][..])),
            ],
        );
        let dest = tmp.dir("out");
        let err = extract(
            &archive,
            &[sel("", "big")],
            &dest,
            tiny_limits(100, 15),
            &NO_CANCEL,
            &mut no_progress,
        )
        .unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(dir_is_empty(&dest));
    }

    #[test]
    fn byte_cap_applies_to_tar_streams_too() {
        let tmp = TempDir::new("byte_cap_tar");
        let archive = tmp.path().join("big.tar.gz");
        make_tar(
            &archive,
            Format::TarGz,
            &[TarItem::File("a.bin", &[7u8; 40])],
        );
        let dest = tmp.dir("out");
        assert!(extract(
            &archive,
            &[sel("", "big")],
            &dest,
            tiny_limits(100, 16),
            &NO_CANCEL,
            &mut no_progress
        )
        .is_err());
        assert!(dir_is_empty(&dest));
    }

    #[test]
    fn cancelled_extraction_leaves_nothing_behind() {
        let tmp = TempDir::new("cancel");
        let archive = tmp.path().join("a.zip");
        make_zip(&archive, &[("a.txt", Some(b"a"))]);
        let dest = tmp.dir("out");
        let cancel = AtomicBool::new(true);
        let err = extract(
            &archive,
            &[sel("", "a")],
            &dest,
            LIMITS,
            &cancel,
            &mut no_progress,
        )
        .unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(dir_is_empty(&dest));
    }

    #[test]
    fn ensure_writable_refuses_paths_inside_but_not_the_archive_file() {
        let tmp = TempDir::new("writable");
        let archive = tmp.file("a.zip", b"x");
        assert!(ensure_writable(std::slice::from_ref(&archive)).is_ok());
        assert!(ensure_writable(&[archive.join("inner")]).is_err());
        assert!(any_inside(&[archive.join("inner")]));
        assert!(!any_inside(&[archive]));
    }

    #[test]
    fn copy_out_request_needs_one_archive_and_no_outside_files() {
        let tmp = TempDir::new("copy_out");
        let archive = tmp.file("a.zip", b"x");
        let other = tmp.file("b.zip", b"x");
        let (got, sels) = copy_out_request(&[archive.join("d"), archive.join("e.txt")])
            .expect("ok")
            .expect("some");
        assert_eq!(got, archive);
        assert_eq!(sels, vec![sel("d", "d"), sel("e.txt", "e.txt")]);
        assert!(copy_out_request(&[archive.join("d"), other.join("x")]).is_err());
        assert!(copy_out_request(&[archive.join("d"), tmp.file("plain.txt", b"")]).is_err());
        assert!(copy_out_request(&[tmp.file("plain2.txt", b"")])
            .expect("ok")
            .is_none());
        // The archive file itself is a normal file to copy.
        assert!(copy_out_request(&[archive]).expect("ok").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn place_all_does_not_replace_dangling_symlinks() {
        let tmp = TempDir::new("archive_place_dangling");
        let staging = tmp.dir("staging");
        std::fs::write(staging.join("a.txt"), b"new").unwrap();
        let dest_dir = tmp.dir("out");
        let outside = tmp.path().join("outside.txt");
        let outside_copy = tmp.path().join("outside_copy.txt");
        std::os::unix::fs::symlink(&outside, dest_dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(&outside_copy, dest_dir.join("a copy.txt")).unwrap();
        let selections = [Selection {
            inner: String::new(),
            name: "a.txt".into(),
        }];
        let archive_path = tmp.path().join("archive.zip");
        let placed = place_all(&archive_path, &selections, &staging, &dest_dir).unwrap();
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].1, dest_dir.join("a copy 2.txt"));
        assert_eq!(
            std::fs::read(dest_dir.join("a copy 2.txt")).unwrap(),
            b"new"
        );
        // Both dangling links are still links, and their targets were not created.
        for link in ["a.txt", "a copy.txt"] {
            assert!(std::fs::symlink_metadata(dest_dir.join(link))
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert!(!outside.exists());
        assert!(!outside_copy.exists());
    }
}
