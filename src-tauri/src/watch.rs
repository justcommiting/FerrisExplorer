//! Directory watcher. One long-lived `notify` thread; the UI asks it to
//! watch the currently visible folders and receives debounced parent paths.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_millis(120);

/// FerrisExplorer's own files (`.ferris_explorer_index.db-wal`, settings…).
pub fn is_own_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".ferris_explorer"))
}

pub enum WatchCmd {
    Set(Vec<PathBuf>),
    Shutdown,
}

pub struct DirWatcher {
    pub cmd_tx: mpsc::Sender<WatchCmd>,
    pub rx: mpsc::Receiver<PathBuf>,
}

impl DirWatcher {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WatchCmd>();
        let (event_tx, event_rx) = mpsc::channel::<PathBuf>();

        std::thread::spawn(move || {
            let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = match RecommendedWatcher::new(raw_tx, notify::Config::default()) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("filesystem watcher unavailable: {e}");
                    return;
                }
            };
            let mut watched: HashSet<PathBuf> = HashSet::new();
            let mut pending: HashSet<PathBuf> = HashSet::new();
            let mut last = Instant::now();

            loop {
                let timeout = if pending.is_empty() {
                    Duration::from_millis(500)
                } else {
                    DEBOUNCE.saturating_sub(last.elapsed())
                };

                if let Ok(cmd) = cmd_rx.recv_timeout(timeout) {
                    match cmd {
                        WatchCmd::Shutdown => return,
                        WatchCmd::Set(paths) => {
                            let next: HashSet<PathBuf> = paths.into_iter().collect();
                            for old in watched.difference(&next) {
                                let _ = watcher.unwatch(old);
                            }
                            for new in next.difference(&watched) {
                                if watcher.watch(new, RecursiveMode::NonRecursive).is_err() {
                                    // Permissions / missing dir — skip.
                                }
                            }
                            watched = next;
                        }
                    }
                }

                while let Ok(ev) = raw_rx.try_recv() {
                    if let Ok(event) = ev {
                        for path in event.paths {
                            // The index and settings live in the home folder
                            // and are written constantly; reporting them would
                            // re-list home in a loop.
                            if is_own_file(&path) {
                                continue;
                            }
                            let dir = if path.is_dir() {
                                path
                            } else {
                                path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
                            };
                            pending.insert(dir);
                            last = Instant::now();
                        }
                    }
                }

                if !pending.is_empty() && last.elapsed() >= DEBOUNCE {
                    for dir in pending.drain() {
                        let _ = event_tx.send(dir);
                    }
                }
            }
        });

        Self {
            cmd_tx,
            rx: event_rx,
        }
    }

    pub fn watch_dirs(&self, dirs: Vec<PathBuf>) {
        let _ = self.cmd_tx.send(WatchCmd::Set(dirs));
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WatchCmd::Shutdown);
    }
}

impl Default for DirWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::Duration;

    #[test]
    fn watcher_notifies_on_new_file() {
        let tmp = TempDir::new("watch");
        let expected = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let watcher = DirWatcher::new();
        watcher.watch_dirs(vec![tmp.path().to_path_buf()]);
        // FSEvents often misses events that happen in the same instant as
        // registration; give the kernel a beat, then write twice.
        std::thread::sleep(Duration::from_millis(250));
        tmp.file("created.txt", b"hi");
        std::thread::sleep(Duration::from_millis(50));
        tmp.file("created2.txt", b"yo");
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut saw = false;
        while Instant::now() < deadline {
            if let Ok(dir) = watcher.rx.recv_timeout(Duration::from_millis(100)) {
                let got = dir.canonicalize().unwrap_or(dir);
                if got == expected || got.starts_with(&expected) {
                    saw = true;
                    break;
                }
            }
        }
        let _ = watcher.cmd_tx.send(WatchCmd::Shutdown);
        assert!(saw, "expected a watch event for the temp directory");
    }

    #[test]
    fn own_files_are_recognised() {
        use std::path::Path;
        assert!(is_own_file(Path::new(
            "/home/u/.ferris_explorer_index.db-wal"
        )));
        assert!(is_own_file(Path::new(
            ".ferris_explorer_saved_searches.json"
        )));
        assert!(!is_own_file(Path::new("/home/u/notes.txt")));
        assert!(!is_own_file(Path::new(
            "/home/u/.ferris_explorer_index.db/inner"
        )));
    }
}
