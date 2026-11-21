//! Move items to the OS trash, and bring them back for undo.
//!
//! The OS calls sit behind [`Trasher`] so the dispatch and the guards can be
//! tested against a stub. Tests never reach the real trash.

use std::path::{Path, PathBuf};

/// What undo says on macOS, where the `trash` crate cannot put items back.
#[cfg(target_os = "macos")]
pub const RESTORE_UNSUPPORTED: &str = "Restore from Trash in Finder";

/// Whether [`delete_items`] trashes or removes for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Trash,
    Permanent,
}

/// The OS side of trashing. Implemented for real by [`OsTrasher`].
pub trait Trasher {
    fn trash(&self, path: &Path) -> Result<(), String>;
    /// Put back the most recently trashed item that came from `original`.
    fn restore(&self, original: &Path) -> Result<(), String>;
}

pub struct OsTrasher;

impl Trasher for OsTrasher {
    fn trash(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    fn restore(&self, original: &Path) -> Result<(), String> {
        let newest = trash::os_limited::list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|item| item.original_path() == original)
            .max_by_key(|item| item.time_deleted)
            .ok_or_else(|| format!("{} is not in the Trash", original.display()))?;
        trash::os_limited::restore_all([newest]).map_err(|e| e.to_string())
    }

    #[cfg(target_os = "macos")]
    fn restore(&self, _original: &Path) -> Result<(), String> {
        Err(RESTORE_UNSUPPORTED.into())
    }
}

/// What a delete did: the paths that went, and the ones that did not.
#[derive(Debug, Default, serde::Serialize)]
pub struct Removed {
    pub done: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

/// Delete `paths` the way `mode` asks. Both modes apply the same guards
/// (no filesystem roots, no home folder) and refuse paths that do not exist.
pub fn delete_items(trasher: &dyn Trasher, mode: Mode, paths: &[PathBuf]) -> Removed {
    let mut out = Removed::default();
    for path in paths {
        let result = match mode {
            Mode::Trash => trash_one(trasher, path),
            Mode::Permanent => crate::ops::delete_path(path),
        };
        match result {
            Ok(()) => out.done.push(path.clone()),
            Err(e) => out.failures.push((path.clone(), e)),
        }
    }
    out
}

fn trash_one(trasher: &dyn Trasher, path: &Path) -> Result<(), String> {
    std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    crate::ops::check_deletable(path)?;
    trasher.trash(path)
}

/// How far a batch of restores got. Restores run in order and stop at the
/// first failure, so `done` lists the paths that came back and the caller
/// keeps the rest.
#[derive(Debug, Default, serde::Serialize)]
pub struct RestoreOutcome {
    pub done: Vec<PathBuf>,
    /// The item that stopped the run, and why.
    pub failure: Option<(PathBuf, String)>,
}

/// Put `paths` back where they came from. Refuses, before restoring anything,
/// if any of those locations is occupied, so undo never overwrites a file.
pub fn restore_items(trasher: &dyn Trasher, paths: &[PathBuf]) -> RestoreOutcome {
    let mut out = RestoreOutcome::default();
    if let Some(p) = paths.iter().find(|p| std::fs::symlink_metadata(p).is_ok()) {
        out.failure = Some((p.clone(), format!("{} already exists", p.display())));
        return out;
    }
    for path in paths {
        match trasher.restore(path) {
            Ok(()) => out.done.push(path.clone()),
            Err(e) => {
                out.failure = Some((path.clone(), e));
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::cell::RefCell;

    /// Records calls instead of touching the OS trash.
    #[derive(Default)]
    struct Stub {
        trashed: RefCell<Vec<PathBuf>>,
        restored: RefCell<Vec<PathBuf>>,
    }

    impl Trasher for Stub {
        fn trash(&self, path: &Path) -> Result<(), String> {
            self.trashed.borrow_mut().push(path.to_path_buf());
            Ok(())
        }
        fn restore(&self, original: &Path) -> Result<(), String> {
            self.restored.borrow_mut().push(original.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn trash_mode_goes_to_the_trash_and_keeps_the_file() {
        let tmp = TempDir::new("recycle_trash");
        let f = tmp.file("a.txt", b"x");
        let stub = Stub::default();
        let out = delete_items(&stub, Mode::Trash, std::slice::from_ref(&f));
        assert_eq!(out.done, vec![f.clone()]);
        assert!(out.failures.is_empty());
        assert_eq!(*stub.trashed.borrow(), vec![f.clone()]);
        // The stub does not move anything, so the file is still here: the
        // trash path must not also delete it permanently.
        assert!(f.exists());
    }

    #[test]
    fn permanent_mode_removes_the_file_and_never_uses_the_trash() {
        let tmp = TempDir::new("recycle_perm");
        let f = tmp.file("a.txt", b"x");
        tmp.file("dir/inner.txt", b"y");
        let stub = Stub::default();
        let out = delete_items(&stub, Mode::Permanent, &[f.clone(), tmp.path().join("dir")]);
        assert_eq!(out.done.len(), 2);
        assert!(stub.trashed.borrow().is_empty());
        assert!(!f.exists());
        assert!(!tmp.path().join("dir").exists());
    }

    #[test]
    fn both_modes_refuse_the_home_folder_and_roots() {
        let stub = Stub::default();
        let home = crate::sys::get_home_dir();
        for mode in [Mode::Trash, Mode::Permanent] {
            let out = delete_items(&stub, mode, &[home.clone(), PathBuf::from("/")]);
            assert!(out.done.is_empty(), "{mode:?} removed something");
            assert_eq!(out.failures.len(), 2, "{mode:?}");
        }
        assert!(stub.trashed.borrow().is_empty());
    }

    #[test]
    fn trash_mode_reports_missing_paths_without_calling_the_os() {
        let tmp = TempDir::new("recycle_missing");
        let stub = Stub::default();
        let out = delete_items(&stub, Mode::Trash, &[tmp.path().join("nope")]);
        assert_eq!(out.failures.len(), 1);
        assert!(stub.trashed.borrow().is_empty());
    }

    #[test]
    fn restore_refuses_when_the_original_location_is_taken() {
        let tmp = TempDir::new("recycle_restore_taken");
        let original = tmp.file("a.txt", b"now here");
        let stub = Stub::default();
        let out = restore_items(&stub, std::slice::from_ref(&original));
        assert!(out.done.is_empty() && out.failure.is_some());
        assert!(stub.restored.borrow().is_empty());
        assert_eq!(std::fs::read(&original).unwrap(), b"now here");
    }

    #[test]
    fn restore_checks_every_location_before_restoring_any() {
        let tmp = TempDir::new("recycle_restore_all");
        let free = tmp.path().join("free.txt");
        let taken = tmp.file("taken.txt", b"x");
        let stub = Stub::default();
        let out = restore_items(&stub, &[free, taken]);
        assert!(out.done.is_empty() && out.failure.is_some());
        assert!(stub.restored.borrow().is_empty());
    }

    #[test]
    fn restore_asks_the_os_for_each_free_location() {
        let tmp = TempDir::new("recycle_restore_ok");
        let free = tmp.path().join("gone.txt");
        let stub = Stub::default();
        let out = restore_items(&stub, std::slice::from_ref(&free));
        assert!(out.failure.is_none());
        assert_eq!(*stub.restored.borrow(), vec![free]);
    }

    /// Refuses to restore one path, and records the rest.
    struct FailOn {
        path: PathBuf,
        restored: RefCell<Vec<PathBuf>>,
    }

    impl Trasher for FailOn {
        fn trash(&self, _path: &Path) -> Result<(), String> {
            Ok(())
        }
        fn restore(&self, original: &Path) -> Result<(), String> {
            if original == self.path {
                return Err("the Trash refused".into());
            }
            self.restored.borrow_mut().push(original.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn restore_stops_at_the_first_failure_and_reports_what_came_back() {
        let tmp = TempDir::new("recycle_restore_partial");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        let c = tmp.path().join("c.txt");
        let stub = FailOn {
            path: b.clone(),
            restored: RefCell::new(Vec::new()),
        };
        let out = restore_items(&stub, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(out.done, vec![a.clone()]);
        assert_eq!(out.failure.map(|(p, _)| p), Some(b));
        // The item after the failure was never asked for.
        assert_eq!(*stub.restored.borrow(), vec![a]);
    }
}
