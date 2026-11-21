//! Asynchronous file operations: copy, move, delete, duplicate, compress.
//! All work runs off the UI thread; progress is pushed over an mpsc channel.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum OpKind {
    Copy,
    Move,
    Delete,
    Duplicate,
    Compress,
    Extract,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct OpProgress {
    pub id: u64,
    #[allow(dead_code)]
    pub kind: OpKind,
    pub message: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub error: Option<String>,
    pub done: bool,
    pub bytes_per_sec: u64,
    /// Items left alone because the user chose "Skip" on a name clash.
    pub skipped: u64,
    /// Set while the operation is paused waiting for a conflict decision.
    pub conflict: Option<Conflict>,
    /// `(source, result)` for each item that finished, so the UI can undo it.
    /// Filled in only on the final message.
    pub outcomes: Vec<(PathBuf, PathBuf)>,
}

/// A destination name that already exists, described for the UI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Conflict {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub src_is_dir: bool,
    pub src_size: u64,
    pub src_modified: u64,
    pub dest_size: u64,
    pub dest_modified: u64,
    /// Items after this one, so the UI can offer "apply to all".
    pub remaining: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    KeepBoth,
    Skip,
    Cancel,
}

#[derive(Clone, Copy, Debug)]
pub struct Decision {
    pub resolution: Resolution,
    pub apply_to_all: bool,
}

fn describe(path: &Path) -> (bool, u64, u64) {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m.is_dir(), m.len(), modified)
        }
        Err(_) => (false, 0, 0),
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Reject names that would escape the destination directory.
pub fn safe_child(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty name".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("name must not contain a path separator".into());
    }
    if name == "." || name == ".." {
        return Err("invalid name".into());
    }
    Ok(dir.join(name))
}

/// True when anything is at `path`, including a symlink whose target is
/// missing. `exists()` follows links and would call that free.
pub(crate) fn path_occupied(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Unique sibling path: `name copy.ext`, then `name copy 2.ext`, …
pub fn unique_sibling(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let candidate = parent.join(format!("{stem} copy{ext}"));
    if !path_occupied(&candidate) {
        return candidate;
    }
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem} copy {n}{ext}"));
        if !path_occupied(&candidate) {
            return candidate;
        }
    }
    // Exhausted the numbered names: a timestamp keeps this from ever
    // returning a path that already exists (and would be overwritten).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!("{stem} copy {nanos}{ext}"))
}

/// Copy `src` to `dst`, which must not exist yet (a dangling symlink counts as
/// existing). If the copy fails or is cancelled, the entries this call created
/// are removed again. Anything that was already at `dst` is never touched.
pub fn copy_recursive(
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
    on_file: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let mut created = Vec::new();
    let result = copy_tree(src, dst, cancel, on_file, &mut created);
    if result.is_err() {
        // Deepest entries first, so each directory is empty when it is removed.
        for path in created.iter().rev() {
            remove_created(path);
        }
    }
    result
}

/// Remove an entry that a failed copy created. Directories are removed only
/// when empty, so anything someone else put inside them stays.
fn remove_created(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
    on_file: &mut dyn FnMut(u64),
    created: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    if dst.starts_with(src) && dst != src {
        return Err("cannot copy a directory into itself".into());
    }
    if is_same_file(src, dst) {
        // `File::create` would truncate the source before reading a byte.
        return Err("source and destination are the same".into());
    }
    let meta = std::fs::symlink_metadata(src).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        copy_symlink(src, dst)?;
        created.push(dst.to_path_buf());
        on_file(meta.len());
        return Ok(());
    }
    if meta.is_dir() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // `create_dir`, not `create_dir_all`: an existing directory at `dst`
        // must fail the copy, not be merged into and later removed.
        std::fs::create_dir(dst).map_err(|e| e.to_string())?;
        created.push(dst.to_path_buf());
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            copy_tree(&entry.path(), &dst.join(name), cancel, on_file, created)?;
        }
        Ok(())
    } else {
        copy_file(src, dst, cancel, on_file, created)
    }
}

/// True when both paths exist and resolve to the same file.
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn copy_symlink(src: &Path, dst: &Path) -> Result<(), String> {
    let target = std::fs::read_link(src).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst).map_err(|e| e.to_string())
    }
    #[cfg(windows)]
    {
        // Recreate the link rather than copying what it points at. The link
        // kind has to match the target, and a dangling link is made as a file
        // link.
        let to_dir = std::fs::metadata(src).is_ok_and(|m| m.is_dir());
        let made = if to_dir {
            std::os::windows::fs::symlink_dir(&target, dst)
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
        };
        match made {
            Ok(()) => Ok(()),
            // ERROR_PRIVILEGE_NOT_HELD: creating links needs Developer Mode or
            // elevation. A file link then falls back to copying what it points
            // at, as before; a folder link has no such fallback.
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error() == Some(1314) =>
            {
                if to_dir {
                    Err(format!(
                        "cannot copy the folder link {}: creating symbolic links needs Developer Mode or administrator rights",
                        src.display()
                    ))
                } else {
                    std::fs::copy(src, dst)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn copy_file(
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
    on_file: &mut dyn FnMut(u64),
    created: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut inf = std::fs::File::open(src).map_err(|e| e.to_string())?;
    // `create_new` fails on any existing entry, a dangling symlink included, so
    // the copy never writes through a link that is already at `dst`.
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| e.to_string())?;
    created.push(dst.to_path_buf());
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = inf.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        on_file(n as u64);
    }
    Ok(())
}

fn is_cross_device(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::CrossesDevices {
        return true;
    }
    // Windows: ERROR_NOT_SAME_DEVICE
    #[cfg(windows)]
    if e.raw_os_error() == Some(17) {
        return true;
    }
    false
}

pub fn move_path(src: &Path, dst: &Path, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    // Look at the link itself: `exists` follows symlinks, so a dangling one at
    // `dst` would pass. This early check gives a clear message; the rename
    // below refuses an existing `dst` atomically, so a file created after the
    // check still cannot be replaced. `rename_noreplace` is used rather than
    // `rename_without_clobbering` because the cross-device fallback needs the
    // io::Error kind.
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(format!("{} already exists", dst.display()));
    }
    match crate::desktop::rename_noreplace(src, dst) {
        Ok(()) => Ok(()),
        // Only a cross-device move may fall back to copy + delete. Any other
        // failure (permissions, bad name, ...) must not delete the source.
        Err(e) if is_cross_device(&e) => {
            // A failed copy removes what it made, so nothing partial is left.
            let mut sink = |_n| {};
            copy_recursive(src, dst, cancel, &mut sink)?;
            // The copy is complete; only now is it safe to remove the source.
            delete_path(src)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Paths that must never be deleted recursively, whatever the caller asks.
/// A symlink is judged by itself, not by what it points at.
pub(crate) fn is_protected(path: &Path) -> bool {
    is_protected_in(path, &crate::sys::get_home_dir())
}

/// [`is_protected`] with the home folder given. The home folder may itself be
/// a symlink, so it is canonicalized too, falling back to the raw path.
fn is_protected_in(path: &Path, home: &Path) -> bool {
    if path.parent().is_none() {
        return true; // a filesystem root
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    std::fs::canonicalize(path).is_ok_and(|p| p.parent().is_none() || p == home)
}

/// Refuse the paths that must never be removed, whether to the trash or for good.
pub(crate) fn check_deletable(path: &Path) -> Result<(), String> {
    if is_protected(path) {
        return Err(format!("refusing to delete {}", path.display()));
    }
    Ok(())
}

pub fn delete_path(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    check_deletable(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else if is_dir_symlink(&meta) {
        // Windows removes a directory link with `remove_dir`; `remove_file` fails.
        std::fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
fn is_dir_symlink(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::FileTypeExt;
    meta.file_type().is_symlink_dir()
}

#[cfg(not(windows))]
fn is_dir_symlink(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Where items dropped from another app land, and which of them get copied.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct DropPlan {
    pub dest: PathBuf,
    pub sources: Vec<PathBuf>,
}

/// Work out where a drop from another app goes: into `hovered` when the
/// pointer was over a folder row, otherwise into the folder the pane shows.
/// Duplicates and items already in that folder are left out, and a folder
/// dropped into itself is refused.
pub fn plan_drop(
    sources: &[PathBuf],
    hovered: Option<&Path>,
    pane_dir: &Path,
) -> Result<DropPlan, String> {
    if sources.is_empty() {
        return Err("nothing to copy".into());
    }
    let dest = match hovered {
        Some(h) if h.is_dir() => h,
        _ => pane_dir,
    };
    if !dest.is_dir() {
        return Err(format!("{} is not a folder", dest.display()));
    }
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let dest_real = real(dest);
    let mut kept: Vec<PathBuf> = Vec::new();
    for src in sources {
        let src_real = real(src);
        if src_real.is_dir() && dest_real.starts_with(&src_real) {
            return Err("cannot copy a directory into itself".into());
        }
        let already_there = src.parent().map(real).as_deref() == Some(dest_real.as_path());
        if already_there || kept.contains(src) {
            continue;
        }
        kept.push(src.clone());
    }
    if kept.is_empty() {
        return Err("those items are already in that folder".into());
    }
    Ok(DropPlan {
        dest: dest.to_path_buf(),
        sources: kept,
    })
}

/// How far a batch of exact moves got. Pairs run in order and the run stops at
/// the first failure, so `done` lists the pairs that moved and the caller
/// keeps the rest. Nothing is rolled back.
#[derive(Debug, Default, serde::Serialize)]
pub struct MoveOutcome {
    pub done: Vec<(PathBuf, PathBuf)>,
    /// The item that stopped the run, and why.
    pub failure: Option<(PathBuf, String)>,
}

/// Move each `(from, to)` pair. A destination that already exists is refused
/// before anything moves. Used to undo and redo moves.
pub fn move_exact(pairs: &[(PathBuf, PathBuf)]) -> MoveOutcome {
    let mut out = MoveOutcome::default();
    if let Some((_, to)) = pairs
        .iter()
        .find(|(_, to)| std::fs::symlink_metadata(to).is_ok())
    {
        out.failure = Some((to.clone(), format!("{} already exists", to.display())));
        return out;
    }
    let cancel = AtomicBool::new(false);
    for (from, to) in pairs {
        match move_path(from, to, &cancel) {
            Ok(()) => out.done.push((from.clone(), to.clone())),
            Err(e) => {
                out.failure = Some((from.clone(), e));
                break;
            }
        }
    }
    out
}

/// Zip `src` into `dest_zip`. Cancelling kills the archiver, and the caller
/// removes the partial archive.
pub fn compress(src: &Path, dest_zip: &Path, cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let child = std::process::Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(src)
            .arg(dest_zip)
            .spawn()
            .map_err(|e| e.to_string())?;
        wait_or_cancel(child, cancel, "ditto")
    }
    // Windows has no `zip` command, but ships bsdtar, which picks the zip
    // format from the archive's extension with `-a`.
    #[cfg(target_os = "windows")]
    {
        let child = std::process::Command::new("tar")
            .args(["-a", "-c", "-f"])
            .arg(dest_zip)
            .arg(src.file_name().unwrap_or_default())
            .current_dir(src.parent().unwrap_or_else(|| Path::new(".")))
            .spawn()
            .map_err(|e| e.to_string())?;
        wait_or_cancel(child, cancel, "tar")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let child = std::process::Command::new("zip")
            .args(["-r", "-q"])
            .arg(dest_zip)
            .arg(src.file_name().unwrap_or_default())
            .current_dir(src.parent().unwrap_or_else(|| Path::new(".")))
            .spawn()
            .map_err(|e| e.to_string())?;
        wait_or_cancel(child, cancel, "zip")
    }
}

/// Wait for an external tool, checking `cancel` between polls. On cancel the
/// process is killed and reaped before returning.
fn wait_or_cancel(
    mut child: std::process::Child,
    cancel: &AtomicBool,
    tool: &str,
) -> Result<(), String> {
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("{tool} failed"))
            };
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

pub fn spawn_copy(
    id: u64,
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    cut: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<OpProgress>,
    // Where conflict decisions arrive. `None` keeps both without asking.
    decisions: Option<std::sync::mpsc::Receiver<Decision>>,
) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let kind = if cut { OpKind::Move } else { OpKind::Copy };
        let mut bytes_done = 0u64;
        let files_total = sources.len() as u64;
        let mut files_done = 0u64;
        let bytes_total = estimate_total(&sources);

        let mut skipped = 0u64;
        let mut remembered: Option<Resolution> = None;
        let mut outcomes: Vec<(PathBuf, PathBuf)> = Vec::new();
        let send = |bytes_done: u64,
                    files_done: u64,
                    skipped: u64,
                    error: Option<String>,
                    done: bool,
                    conflict: Option<Conflict>,
                    outcomes: &[(PathBuf, PathBuf)]| {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let _ = tx.send(OpProgress {
                id,
                kind,
                message: if done {
                    if error.as_deref() == Some("cancelled") {
                        "Cancelled".into()
                    } else if error.is_some() {
                        "Failed".into()
                    } else {
                        let verb = if cut { "Moved" } else { "Copied" };
                        if skipped > 0 {
                            format!("{verb} ({skipped} skipped)")
                        } else {
                            verb.into()
                        }
                    }
                } else if conflict.is_some() {
                    "Waiting: name already exists".into()
                } else if cut {
                    "Moving...".into()
                } else {
                    "Copying...".into()
                },
                bytes_done,
                bytes_total,
                files_done,
                files_total,
                error,
                done,
                bytes_per_sec: (bytes_done as f64 / elapsed) as u64,
                skipped,
                conflict,
                outcomes: outcomes.to_vec(),
            });
        };

        send(0, 0, 0, None, false, None, &[]);
        let mut first_err = None;
        for (i, src) in sources.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                first_err = Some("cancelled".into());
                break;
            }
            let name = src.file_name().unwrap_or_default();
            let dst = dest_dir.join(name);
            if dst == *src {
                files_done += 1;
                continue;
            }
            let dest = if path_occupied(&dst) {
                let resolution = match (remembered, &decisions) {
                    (Some(r), _) => r,
                    (None, None) => Resolution::KeepBoth,
                    (None, Some(rx)) => {
                        let (src_is_dir, src_size, src_modified) = describe(src);
                        let (_, dest_size, dest_modified) = describe(&dst);
                        send(
                            bytes_done,
                            files_done,
                            skipped,
                            None,
                            false,
                            Some(Conflict {
                                src: src.clone(),
                                dest: dst.clone(),
                                src_is_dir,
                                src_size,
                                src_modified,
                                dest_size,
                                dest_modified,
                                remaining: (sources.len() - i - 1) as u64,
                            }),
                            &[],
                        );
                        // Wait for the user, but stay responsive to Cancel.
                        let decision = loop {
                            if cancel.load(Ordering::Relaxed) {
                                break Decision {
                                    resolution: Resolution::Cancel,
                                    apply_to_all: false,
                                };
                            }
                            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                                Ok(d) => break d,
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                    break Decision {
                                        resolution: Resolution::Cancel,
                                        apply_to_all: false,
                                    }
                                }
                            }
                        };
                        if decision.apply_to_all {
                            remembered = Some(decision.resolution);
                        }
                        decision.resolution
                    }
                };
                match resolution {
                    Resolution::KeepBoth => unique_sibling(&dst),
                    Resolution::Skip => {
                        skipped += 1;
                        files_done += 1;
                        send(bytes_done, files_done, skipped, None, false, None, &[]);
                        continue;
                    }
                    Resolution::Cancel => {
                        first_err = Some("cancelled".into());
                        break;
                    }
                }
            } else {
                dst
            };
            let result = if cut {
                move_path(src, &dest, &cancel)
            } else {
                copy_recursive(src, &dest, &cancel, &mut |n| {
                    bytes_done = bytes_done.saturating_add(n);
                    send(bytes_done, files_done, skipped, None, false, None, &[]);
                })
            };
            if let Err(e) = result {
                // A failed copy has already removed what it created.
                first_err = Some(format!("{}: {e}", src.display()));
                break;
            }
            outcomes.push((src.clone(), dest));
            files_done += 1;
            send(bytes_done, files_done, skipped, None, false, None, &[]);
        }
        send(
            bytes_done, files_done, skipped, first_err, true, None, &outcomes,
        );
    });
}

fn estimate_total(sources: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    for src in sources {
        total = total.saturating_add(quick_size(src));
    }
    total
}

fn quick_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        0
    } else {
        meta.len()
    }
}

pub fn open_terminal(dir: &Path) {
    let result = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .args(["-a", "Terminal"])
                .arg(dir)
                .spawn()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("wt")
                .args(["-d"])
                .arg(dir)
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("cmd")
                        .args(["/K", "cd", "/d"])
                        .arg(dir)
                        .spawn()
                })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            std::process::Command::new("x-terminal-emulator")
                .arg("--working-directory")
                .arg(dir)
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("gnome-terminal")
                        .arg(format!("--working-directory={}", dir.display()))
                        .spawn()
                })
                .or_else(|_| xterm_command(dir, &login_shell()).spawn())
        }
    };
    if let Err(e) = result {
        log::error!("Failed to open terminal: {e}");
    }
}

/// The shell to run in a terminal: `$SHELL`, or `sh` when it is unset.
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// `xterm` running `shell` in `dir`. The directory is the working directory and
/// never part of a shell command, so a name with quotes or `;` stays a path.
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
fn xterm_command(dir: &Path, shell: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("xterm");
    cmd.arg("-e").arg(shell).current_dir(dir);
    cmd
}

pub fn open_in_editor(path: &Path) {
    if let Ok(editor) = std::env::var("EDITOR") {
        if !editor.is_empty()
            && std::process::Command::new(&editor)
                .arg(path)
                .spawn()
                .is_ok()
        {
            return;
        }
    }
    if let Ok(visual) = std::env::var("VISUAL") {
        if !visual.is_empty()
            && std::process::Command::new(&visual)
                .arg(path)
                .spawn()
                .is_ok()
        {
            return;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .args(["-t"])
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("notepad").arg(path).spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn safe_child_rejects_traversal() {
        let dir = Path::new("/tmp/dest");
        assert!(safe_child(dir, "../etc/passwd").is_err());
        assert!(safe_child(dir, "a/b").is_err());
        assert!(safe_child(dir, "..").is_err());
        assert!(safe_child(dir, "").is_err());
        assert_eq!(safe_child(dir, "ok.txt").unwrap(), dir.join("ok.txt"));
    }

    #[test]
    fn unique_sibling_adds_copy_suffix() {
        let tmp = TempDir::new("ops_unique");
        let src = tmp.file("notes.txt", b"hi");
        let first = unique_sibling(&src);
        assert_eq!(first.file_name().unwrap(), "notes copy.txt");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_sibling(&src);
        assert_eq!(second.file_name().unwrap(), "notes copy 2.txt");
    }

    #[test]
    fn copy_recursive_copies_nested_tree() {
        let tmp = TempDir::new("ops_copy");
        tmp.file("src/a.txt", b"hello");
        tmp.file("src/sub/b.txt", b"world");
        let dest = tmp.path().join("dest");
        let cancel = AtomicBool::new(false);
        let mut bytes = 0u64;
        copy_recursive(&tmp.path().join("src"), &dest, &cancel, &mut |n| bytes += n).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("sub/b.txt")).unwrap(),
            "world"
        );
        assert_eq!(bytes, 10);
    }

    #[test]
    fn copy_into_self_is_rejected() {
        let tmp = TempDir::new("ops_self");
        tmp.file("src/a.txt", b"x");
        let src = tmp.path().join("src");
        let dest = src.join("nested");
        let cancel = AtomicBool::new(false);
        let mut sink = |_| {};
        assert!(copy_recursive(&src, &dest, &cancel, &mut sink).is_err());
    }

    #[test]
    fn cancel_stops_copy() {
        let tmp = TempDir::new("ops_cancel");
        tmp.file("src/a.txt", b"hello");
        let cancel = AtomicBool::new(true);
        let mut sink = |_| {};
        assert_eq!(
            copy_recursive(
                &tmp.path().join("src"),
                &tmp.path().join("dest"),
                &cancel,
                &mut sink
            ),
            Err("cancelled".into())
        );
    }

    #[test]
    fn move_path_renames_when_possible() {
        let tmp = TempDir::new("ops_move");
        let src = tmp.file("a.txt", b"data");
        let dst = tmp.path().join("b.txt");
        move_path(&src, &dst, &AtomicBool::new(false)).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn move_refuses_to_overwrite_existing_destination() {
        let tmp = TempDir::new("ops_move_exists");
        let src = tmp.file("a.txt", b"new");
        let dst = tmp.file("b.txt", b"precious");
        assert!(move_path(&src, &dst, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), b"precious");
        assert_eq!(std::fs::read(&src).unwrap(), b"new");
    }

    #[test]
    fn failed_move_keeps_the_source() {
        let tmp = TempDir::new("ops_move_fail");
        let src = tmp.file("a.txt", b"keep me");
        let dst = tmp.path().join("missing_dir/b.txt");
        assert!(move_path(&src, &dst, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), b"keep me");
    }

    #[test]
    fn copy_onto_itself_does_not_truncate() {
        let tmp = TempDir::new("ops_copy_self_file");
        let src = tmp.file("a.txt", b"contents");
        let mut sink = |_| {};
        assert!(copy_recursive(&src, &src, &AtomicBool::new(false), &mut sink).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), b"contents");
    }

    #[test]
    fn delete_path_removes_files_and_trees() {
        let tmp = TempDir::new("ops_delete");
        let f = tmp.file("f.txt", b"x");
        tmp.file("d/inner/g.txt", b"y");
        delete_path(&f).unwrap();
        delete_path(&tmp.path().join("d")).unwrap();
        assert!(!f.exists());
        assert!(!tmp.path().join("d").exists());
    }

    #[test]
    fn delete_path_refuses_roots_and_home() {
        assert!(delete_path(Path::new("/")).is_err());
        assert!(delete_path(&crate::sys::get_home_dir()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn delete_of_symlink_leaves_target_alone() {
        let tmp = TempDir::new("ops_delete_link");
        tmp.file("real/keep.txt", b"x");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("real"), &link).unwrap();
        delete_path(&link).unwrap();
        assert!(tmp.path().join("real/keep.txt").exists());
    }

    #[test]
    fn spawn_copy_cleans_up_after_failure_and_never_overwrites() {
        let tmp = TempDir::new("ops_spawn");
        let src = tmp.file("a.txt", b"one");
        let dest_dir = tmp.path().join("out");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("a.txt"), b"existing").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_copy(
            1,
            vec![src],
            dest_dir.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            tx,
            None,
        );
        let last = rx.iter().find(|p| p.done).expect("done message");
        assert!(last.error.is_none());
        assert_eq!(std::fs::read(dest_dir.join("a.txt")).unwrap(), b"existing");
        assert_eq!(std::fs::read(dest_dir.join("a copy.txt")).unwrap(), b"one");
    }

    #[test]
    fn spawn_copy_reports_where_each_item_landed() {
        let tmp = TempDir::new("ops_outcomes");
        let src = tmp.file("a.txt", b"one");
        let dest_dir = tmp.path().join("out");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("a.txt"), b"existing").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_copy(
            7,
            vec![src.clone()],
            dest_dir.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            tx,
            None,
        );
        let last = rx.iter().find(|p| p.done).expect("done message");
        // KeepBoth names the copy differently, so the outcome must say so.
        assert_eq!(last.outcomes, vec![(src, dest_dir.join("a copy.txt"))]);
    }

    #[test]
    fn drop_onto_folder_row_targets_that_folder() {
        let tmp = TempDir::new("ops_drop_row");
        let outside = tmp.file("outside/x.txt", b"x");
        let pane = tmp.dir("pane");
        let row = tmp.dir("pane/folder");
        let plan = plan_drop(std::slice::from_ref(&outside), Some(&row), &pane).unwrap();
        assert_eq!(plan.dest, row);
        assert_eq!(plan.sources, vec![outside]);
    }

    #[test]
    fn drop_on_a_file_row_or_empty_space_uses_the_pane_folder() {
        let tmp = TempDir::new("ops_drop_pane");
        let outside = tmp.file("outside/x.txt", b"x");
        let pane = tmp.dir("pane");
        let file_row = tmp.file("pane/notes.txt", b"n");
        let on_file = plan_drop(std::slice::from_ref(&outside), Some(&file_row), &pane).unwrap();
        assert_eq!(on_file.dest, pane);
        let on_nothing = plan_drop(std::slice::from_ref(&outside), None, &pane).unwrap();
        assert_eq!(on_nothing.dest, pane);
    }

    #[test]
    fn drop_skips_items_already_in_the_folder_and_duplicates() {
        let tmp = TempDir::new("ops_drop_skip");
        let pane = tmp.dir("pane");
        let here = tmp.file("pane/here.txt", b"h");
        let there = tmp.file("other/there.txt", b"t");
        let plan = plan_drop(&[here.clone(), there.clone(), there.clone()], None, &pane).unwrap();
        assert_eq!(plan.sources, vec![there]);
        assert!(plan_drop(&[here], None, &pane).is_err());
    }

    #[test]
    fn drop_refuses_a_folder_into_itself_and_missing_targets() {
        let tmp = TempDir::new("ops_drop_refuse");
        let folder = tmp.dir("a");
        let nested = tmp.dir("a/sub");
        assert!(plan_drop(std::slice::from_ref(&folder), Some(&nested), tmp.path()).is_err());
        assert!(plan_drop(std::slice::from_ref(&folder), Some(&folder), tmp.path()).is_err());
        let gone = tmp.path().join("gone");
        assert!(plan_drop(&[tmp.file("f.txt", b"f")], None, &gone).is_err());
        assert!(plan_drop(&[], None, tmp.path()).is_err());
    }

    #[test]
    fn move_exact_moves_pairs_and_refuses_existing_destinations() {
        let tmp = TempDir::new("ops_move_exact");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let a2 = tmp.path().join("a2.txt");
        let b2 = tmp.path().join("b2.txt");
        let out = move_exact(&[(a.clone(), a2.clone()), (b.clone(), b2.clone())]);
        assert!(out.failure.is_none());
        assert_eq!(out.done.len(), 2);
        assert!(!a.exists() && !b.exists());
        assert_eq!(std::fs::read(&a2).unwrap(), b"a");

        // One destination is taken: nothing at all may move.
        let c = tmp.file("c.txt", b"c");
        let taken = tmp.file("taken.txt", b"keep");
        let c2 = tmp.path().join("c2.txt");
        let out = move_exact(&[(c.clone(), c2.clone()), (b2.clone(), taken.clone())]);
        assert!(out.done.is_empty());
        assert_eq!(out.failure.map(|(p, _)| p), Some(taken.clone()));
        assert!(c.exists());
        assert!(!c2.exists());
        assert_eq!(std::fs::read(&taken).unwrap(), b"keep");
    }

    #[test]
    fn move_exact_reports_the_pairs_that_moved_and_stops_at_the_first_failure() {
        let tmp = TempDir::new("ops_move_partial");
        let a = tmp.file("a.txt", b"a");
        let a2 = tmp.path().join("a2.txt");
        let missing = tmp.path().join("missing.txt");
        let m2 = tmp.path().join("m2.txt");
        let c = tmp.file("c.txt", b"c");
        let c2 = tmp.path().join("c2.txt");
        let out = move_exact(&[
            (a.clone(), a2.clone()),
            (missing.clone(), m2.clone()),
            (c.clone(), c2.clone()),
        ]);
        assert_eq!(out.done, vec![(a.clone(), a2.clone())]);
        assert_eq!(out.failure.map(|(p, _)| p), Some(missing));
        assert!(!a.exists() && a2.exists());
        // The pair after the failure was never attempted.
        assert!(c.exists());
        assert!(!c2.exists());
    }

    #[test]
    fn move_never_overwrites_a_destination_created_after_the_check() {
        let tmp = TempDir::new("ops_move_race");
        let src = tmp.file("src.txt", b"mine");
        let dst = tmp.file("dst.txt", b"theirs");
        let cancel = AtomicBool::new(false);
        assert!(move_path(&src, &dst, &cancel).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), b"theirs");
        assert_eq!(std::fs::read(&src).unwrap(), b"mine");
    }

    #[cfg(unix)]
    #[test]
    fn move_does_not_replace_a_dangling_symlink_at_the_destination() {
        let tmp = TempDir::new("ops_move_dangling");
        let src = tmp.file("src.txt", b"mine");
        let dst = tmp.path().join("dst");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dst).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(move_path(&src, &dst, &cancel).is_err());
        assert!(std::fs::symlink_metadata(&dst)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&src).unwrap(), b"mine");
    }

    #[test]
    fn xterm_fallback_keeps_the_directory_out_of_the_command() {
        let dir = Path::new("/work/a'; touch pwned; echo '");
        let cmd = xterm_command(dir, "/bin/zsh");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-e", "/bin/zsh"]);
        assert_eq!(cmd.get_current_dir(), Some(dir));
    }

    #[cfg(unix)]
    #[test]
    fn unique_sibling_treats_a_dangling_symlink_as_taken() {
        let tmp = TempDir::new("ops_unique_dangling");
        let src = tmp.file("notes.txt", b"hi");
        std::os::unix::fs::symlink(
            tmp.path().join("nowhere"),
            tmp.path().join("notes copy.txt"),
        )
        .unwrap();
        assert_eq!(
            unique_sibling(&src).file_name().unwrap(),
            "notes copy 2.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_does_not_write_through_a_dangling_symlink() {
        let tmp = TempDir::new("ops_copy_dangling");
        let src = tmp.file("notes.txt", b"mine");
        let target = tmp.path().join("outside.txt");
        let dst = tmp.path().join("dst.txt");
        std::os::unix::fs::symlink(&target, &dst).unwrap();
        let cancel = AtomicBool::new(false);
        let mut sink = |_| {};
        assert!(copy_recursive(&src, &dst, &cancel, &mut sink).is_err());
        // The link's target is not created, and the link itself is not removed.
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(&dst)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&src).unwrap(), b"mine");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_copy_keeps_a_dangling_symlink_and_copies_beside_it() {
        let tmp = TempDir::new("ops_spawn_dangling");
        let src = tmp.file("a.txt", b"one");
        let dest_dir = tmp.path().join("out");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let target = tmp.path().join("outside.txt");
        std::os::unix::fs::symlink(&target, dest_dir.join("a.txt")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_copy(
            3,
            vec![src],
            dest_dir.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            tx,
            None,
        );
        let last = rx.iter().find(|p| p.done).expect("done message");
        assert!(last.error.is_none());
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(dest_dir.join("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(dest_dir.join("a copy.txt")).unwrap(), b"one");
    }

    #[cfg(unix)]
    #[test]
    fn protection_follows_a_symlinked_home_folder() {
        let tmp = TempDir::new("ops_home_link");
        let home = tmp.dir("real_home");
        let link = tmp.path().join("home_link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        // The home folder is given by its symlink, the path by its real name.
        assert!(is_protected_in(&home, &link));
        assert!(!is_protected_in(&tmp.dir("other"), &link));
    }

    /// Run a transfer that hits one clash and answer it with `resolution`.
    fn transfer_with_conflict(
        name: &str,
        cut: bool,
        resolution: Resolution,
    ) -> (TempDir, OpProgress) {
        let tmp = TempDir::new(name);
        let a = tmp.file("a.txt", b"one");
        let b = tmp.file("b.txt", b"two");
        let dest_dir = tmp.path().join("out");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("a.txt"), b"existing").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (dtx, drx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        spawn_copy(7, vec![a, b], dest_dir, cut, cancel, tx, Some(drx));
        let mut last = None;
        for p in rx.iter() {
            if let Some(c) = &p.conflict {
                assert!(c.dest.ends_with("a.txt"));
                assert_eq!(c.remaining, 1);
                assert_eq!(c.dest_size, 8);
                dtx.send(Decision {
                    resolution,
                    apply_to_all: false,
                })
                .unwrap();
            }
            if p.done {
                last = Some(p);
                break;
            }
        }
        (tmp, last.expect("done"))
    }

    #[test]
    fn conflict_skip_leaves_both_files_and_continues() {
        let (tmp, last) = transfer_with_conflict("ops_skip", true, Resolution::Skip);
        assert!(last.error.is_none());
        assert_eq!(last.skipped, 1);
        let out = tmp.path().join("out");
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"existing");
        assert!(
            tmp.path().join("a.txt").exists(),
            "skipped source stays put"
        );
        assert!(out.join("b.txt").exists(), "later items still move");
    }

    #[test]
    fn conflict_keep_both_copies_beside_the_existing_file() {
        let (tmp, last) = transfer_with_conflict("ops_keep", false, Resolution::KeepBoth);
        assert!(last.error.is_none());
        let out = tmp.path().join("out");
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"existing");
        assert_eq!(std::fs::read(out.join("a copy.txt")).unwrap(), b"one");
    }

    #[test]
    fn conflict_cancel_stops_before_touching_anything() {
        let (tmp, last) = transfer_with_conflict("ops_cancel", true, Resolution::Cancel);
        assert_eq!(last.error.as_deref(), Some("cancelled"));
        assert_eq!(last.message, "Cancelled");
        assert!(tmp.path().join("a.txt").exists());
        assert!(
            tmp.path().join("b.txt").exists(),
            "nothing after the clash moved"
        );
    }
}
