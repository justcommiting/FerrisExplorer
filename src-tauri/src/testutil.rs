//! Test-only helpers shared across module test suites.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A uniquely named temporary directory, removed when it goes out of scope.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("ferris_test_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Create a file relative to the root, creating parent directories.
    pub fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&p, contents).expect("write temp file");
        p
    }

    /// Create a directory relative to the root.
    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::create_dir_all(&p).expect("create temp subdir");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Restore permissions first, or an unreadable directory left behind by
        // a permissions test would defeat the cleanup.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
