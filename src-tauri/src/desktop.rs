//! Small filesystem and OS helpers that used to live on the egui app struct.

use std::io;
use std::path::{Path, PathBuf};

/// Expand a leading `~` to the home directory.
pub fn expand_user_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        return crate::sys::get_home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return crate::sys::get_home_dir().join(rest);
    }
    PathBuf::from(raw)
}

/// Open a file, folder or URL with the OS default application.
pub fn launch_file(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        shell_open(path)
    }
    #[cfg(not(windows))]
    {
        let result = match std::env::consts::OS {
            "macos" => std::process::Command::new("open").arg(path).spawn(),
            _ => std::process::Command::new("xdg-open").arg(path).spawn(),
        };
        result
            .map(|_| ())
            .map_err(|e| format!("failed to open '{}': {e}", path.display()))
    }
}

/// `ShellExecuteW` with the "open" verb: the default-handler lookup a double
/// click uses, with no command line for cmd.exe to parse.
#[cfg(windows)]
fn shell_open(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `verb` and `file` are NUL-terminated and outlive the call. A null
    // window handle, parameters and directory are all allowed by the API.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    // ShellExecute returns a value above 32 on success, and 32 or less on error.
    if code > 32 {
        Ok(())
    } else {
        Err(format!(
            "failed to open '{}': ShellExecute returned {code}",
            path.display()
        ))
    }
}

/// True when `a` and `b` are the same directory entry under two spellings of
/// one name, so renaming `a` to `b` only changes its case. Symlinks are never
/// followed: a symlink to `b` and a hard link to `b` are different entries.
pub(crate) fn is_same_entry(a: &Path, b: &Path) -> bool {
    let (Some(a_name), Some(b_name)) = (a.file_name(), b.file_name()) else {
        return false;
    };
    if a_name.to_string_lossy().to_lowercase() != b_name.to_string_lossy().to_lowercase() {
        return false;
    }
    // Only the parent is canonicalized: it is a directory, so following links
    // there is harmless, and it makes `a` and `b` comparable.
    let parent = |p: &Path| {
        let dir = match p.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        std::fs::canonicalize(dir).ok()
    };
    match (parent(a), parent(b)) {
        (Some(x), Some(y)) if x == y => {}
        _ => return false,
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    // Windows: a case-insensitive name in the same folder is the same entry.
    // Names that differ only by case cannot be two files on that filesystem.
    #[cfg(not(unix))]
    {
        true
    }
}

/// Rename `from` to `to`, never replacing anything at `to`. If `to` exists the
/// call fails with `AlreadyExists` and both entries are left as they were.
///
/// The check and the rename are one atomic step where the OS has a no-replace
/// rename: `renamex_np(RENAME_EXCL)` on macOS, `renameat2(RENAME_NOREPLACE)` on
/// glibc Linux, and `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on
/// Windows. Elsewhere, or where the filesystem refuses the flag, see
/// `rename_excl_fallback`.
///
/// A case-only rename of one entry (`a.txt` -> `A.txt`) on a case-insensitive
/// disk also reports `AlreadyExists`, because `to` names `from`. That is
/// retried as a plain rename when `is_same_entry` holds.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_excl(from, to) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && is_same_entry(from, to) => {
            std::fs::rename(from, to)
        }
        other => other,
    }
}

/// `rename_noreplace` for callers that report errors as text. A refused
/// rename gets the message the user has always seen.
pub fn rename_without_clobbering(from: &Path, to: &Path) -> Result<(), String> {
    if from == to {
        return Ok(());
    }
    rename_noreplace(from, to).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            "a file with that name already exists".into()
        } else {
            e.to_string()
        }
    })
}

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
fn c_paths(from: &Path, to: &Path) -> io::Result<(std::ffi::CString, std::ffi::CString)> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
    };
    Ok((c(from)?, c(to)?))
}

#[cfg(target_os = "macos")]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (c_from, c_to) = c_paths(from, to)?;
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let r = unsafe { libc::renamex_np(c_from.as_ptr(), c_to.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Some volumes (FAT, exFAT, certain network mounts) lack RENAME_EXCL.
        Some(libc::ENOTSUP) | Some(libc::EINVAL) => rename_excl_fallback(from, to),
        _ => Err(err),
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (c_from, c_to) = c_paths(from, to)?;
    // SAFETY: both strings are NUL-terminated and outlive the call. AT_FDCWD
    // resolves relative paths against the working directory, as `rename` does.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // EINVAL: the filesystem does not support RENAME_NOREPLACE.
        // ENOSYS: the kernel has no renameat2.
        Some(libc::EINVAL) | Some(libc::ENOSYS) => rename_excl_fallback(from, to),
        _ => Err(err),
    }
}

#[cfg(windows)]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (w_from, w_to) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated and outlive the call. Without
    // MOVEFILE_REPLACE_EXISTING the call fails if `to` exists.
    let ok = unsafe { MoveFileExW(w_from.as_ptr(), w_to.as_ptr(), 0) };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(
    windows,
    target_os = "macos",
    all(target_os = "linux", target_env = "gnu")
)))]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    rename_excl_fallback(from, to)
}

/// Used where the no-replace rename is unavailable. A file is hard-linked to
/// `to` first: `link` fails with `AlreadyExists` if `to` exists, so the name is
/// claimed atomically, and then `from` is unlinked. A directory cannot be
/// hard-linked, and a filesystem without hard links fails `link` too; both use
/// `check_then_rename`, which can lose a race with a file created in between.
#[cfg(not(windows))]
fn rename_excl_fallback(from: &Path, to: &Path) -> io::Result<()> {
    let is_dir = std::fs::symlink_metadata(from).is_ok_and(|m| m.file_type().is_dir());
    if is_dir {
        return check_then_rename(from, to);
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => return check_then_rename(from, to),
    }
    if let Err(e) = std::fs::remove_file(from) {
        // Drop the new name, so the file is not left under two names.
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(windows))]
fn check_then_rename(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn expand_user_path_handles_tilde() {
        let home = crate::sys::get_home_dir();
        assert_eq!(expand_user_path("~"), home);
        assert_eq!(expand_user_path("~/Documents"), home.join("Documents"));
        assert_eq!(expand_user_path("/tmp/x"), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn rename_refuses_to_replace_an_existing_file() {
        let tmp = TempDir::new("rename_clobber");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"precious");
        assert!(rename_without_clobbering(&a, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"precious");
        assert!(a.exists());
    }

    #[test]
    fn rename_to_a_free_name_works() {
        let tmp = TempDir::new("rename_ok");
        let a = tmp.file("a.txt", b"a");
        let c = tmp.path().join("c.txt");
        rename_without_clobbering(&a, &c).unwrap();
        assert!(!a.exists() && c.exists());
    }

    #[test]
    fn rename_noreplace_refuses_an_existing_file_and_dir() {
        let tmp = TempDir::new("rename_noreplace");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let err = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");

        let d1 = tmp.dir("d1");
        let d2 = tmp.dir("d2");
        std::fs::write(d1.join("inner.txt"), b"one").unwrap();
        std::fs::write(d2.join("inner.txt"), b"two").unwrap();
        let err = rename_noreplace(&d1, &d2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d1.join("inner.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(d2.join("inner.txt")).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_refuses_an_existing_file_and_dir_and_moves_a_free_one() {
        let tmp = TempDir::new("rename_fallback");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let err = rename_excl_fallback(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");

        let d1 = tmp.dir("d1");
        let d2 = tmp.dir("d2");
        let err = rename_excl_fallback(&d1, &d2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(d1.is_dir() && d2.is_dir());

        let c = tmp.path().join("c.txt");
        rename_excl_fallback(&a, &c).unwrap();
        assert!(!a.exists() && c.exists());
    }

    /// Whether the temp folder ignores case, probed with a real file.
    fn case_insensitive(dir: &Path) -> bool {
        std::fs::write(dir.join("ci_probe.txt"), b"x").unwrap();
        dir.join("CI_PROBE.TXT").exists()
    }

    #[test]
    fn a_case_only_rename_of_the_same_file_works() {
        let tmp = TempDir::new("rename_case");
        if !case_insensitive(tmp.path()) {
            eprintln!("skipped: temp dir is case-sensitive");
            return;
        }
        let a = tmp.file("a.txt", b"a");
        let upper = tmp.path().join("A.txt");
        rename_without_clobbering(&a, &upper).unwrap();
        assert_eq!(std::fs::read(&upper).unwrap(), b"a");
        assert!(names_of(tmp.path()).contains(&"A.txt".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_renamed_onto_its_target_is_refused() {
        let tmp = TempDir::new("rename_symlink");
        let t = tmp.file("t.txt", b"precious");
        let l = tmp.path().join("l");
        std::os::unix::fs::symlink(&t, &l).unwrap();
        assert!(rename_without_clobbering(&l, &t).is_err());
        assert_eq!(std::fs::read(&t).unwrap(), b"precious");
        assert!(std::fs::symlink_metadata(&l)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_renamed_onto_its_target_is_refused() {
        let tmp = TempDir::new("rename_hardlink");
        let t = tmp.file("t.txt", b"precious");
        let h = tmp.path().join("h.txt");
        std::fs::hard_link(&t, &h).unwrap();
        assert!(rename_without_clobbering(&h, &t).is_err());
        assert_eq!(std::fs::read(&t).unwrap(), b"precious");
        assert!(h.exists());
    }

    fn names_of(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}
