use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

use crate::analyzer::{StorageAnalyzer, TreemapNode};
use crate::db::DbManager;
use crate::search::TabId;
use crate::sys;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

// ── Directory-size worker ─────────────────────────────────────────────────

/// Number of concurrent directory-size scans.
///
/// These scans only fill in one column of the file list, so the pool is
/// deliberately small: the work is IO-bound, and running many recursive walks
/// at once thrashes the disk without finishing any of them sooner.
const DIR_SIZE_WORKERS: usize = 4;

/// Total size of the files beneath `path`.
///
/// Returns an error if `path` itself cannot be read, so the UI can show that
/// the size is unknown rather than displaying a confident "0 B". A
/// *subdirectory* that cannot be read is skipped, which makes the total a
/// best-effort lower bound.
fn directory_size(path: &Path) -> Result<u64, String> {
    // Probe the root separately so an unreadable directory is distinguishable
    // from an empty one.
    std::fs::read_dir(path).map_err(|e| e.to_string())?;

    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                // `DirEntry::metadata` does not follow symlinks, so a link
                // pointing at an ancestor cannot send this into a loop.
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// How urgently a directory should be sized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizePriority {
    /// A row the user can see, or the folder being shown. Always served first.
    Visible,
    /// Speculative work, such as sizing every folder when sorting by size.
    Background,
}

/// Directories waiting to be sized: visible rows ahead of background ones.
///
/// A path is queued at most once. Asking for a path that is already waiting
/// as background work moves it to the visible queue.
#[derive(Default, Debug)]
pub struct SizeQueue {
    visible: std::collections::VecDeque<PathBuf>,
    background: std::collections::VecDeque<PathBuf>,
    queued: std::collections::HashMap<PathBuf, SizePriority>,
}

impl SizeQueue {
    /// Queues `path`. Returns false when it was already waiting at this
    /// priority or higher.
    pub fn push(&mut self, path: PathBuf, priority: SizePriority) -> bool {
        match self.queued.get(&path).copied() {
            Some(SizePriority::Visible) => return false,
            Some(SizePriority::Background) if priority == SizePriority::Background => return false,
            Some(SizePriority::Background) => {
                // Promote: take it out of the background line first.
                self.background.retain(|p| p != &path);
            }
            None => {}
        }
        match priority {
            SizePriority::Visible => self.visible.push_back(path.clone()),
            SizePriority::Background => self.background.push_back(path.clone()),
        }
        self.queued.insert(path, priority);
        true
    }

    /// The next directory to size: visible ones first, then background, in
    /// the order they were asked for.
    pub fn pop(&mut self) -> Option<PathBuf> {
        let path = self
            .visible
            .pop_front()
            .or_else(|| self.background.pop_front())?;
        self.queued.remove(&path);
        Some(path)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.queued.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }
}

/// Shared between the pool's handle and its threads.
struct SizeQueueState {
    queue: Mutex<(SizeQueue, bool)>,
    ready: std::sync::Condvar,
}

/// A fixed pool of threads that compute directory sizes off the UI thread.
///
/// Requests used to get a dedicated `std::thread` each, so opening a folder
/// containing a thousand subdirectories spawned a thousand threads at once.
/// Requests are now queued and served by [`DIR_SIZE_WORKERS`] long-lived
/// threads, visible rows first (see [`SizeQueue`]).
pub struct DirSizeWorker {
    shared: Arc<SizeQueueState>,
    pub rx: mpsc::Receiver<(PathBuf, Result<u64, String>)>,
}

impl DirSizeWorker {
    pub fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let shared = Arc::new(SizeQueueState {
            queue: Mutex::new((SizeQueue::default(), false)),
            ready: std::sync::Condvar::new(),
        });

        for _ in 0..DIR_SIZE_WORKERS {
            let shared = Arc::clone(&shared);
            let results = result_tx.clone();
            std::thread::spawn(move || loop {
                let path = {
                    let Ok(mut guard) = shared.queue.lock() else {
                        return; // another worker panicked mid-walk
                    };
                    loop {
                        let (queue, closed) = &mut *guard;
                        if let Some(path) = queue.pop() {
                            break path;
                        }
                        if *closed {
                            return; // the app is shutting down
                        }
                        guard = match shared.ready.wait(guard) {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                    }
                };
                let outcome = directory_size(&path);
                if results.send((path, outcome)).is_err() {
                    return; // nobody is listening any more
                }
            });
        }

        Self {
            shared,
            rx: result_rx,
        }
    }

    /// Queue a directory the user can see (or is about to). Returns without blocking.
    pub fn spawn_visible(&self, path: PathBuf) {
        self.enqueue(path, SizePriority::Visible);
    }

    /// Queue a directory whose size is only needed eventually, such as every
    /// folder in a listing when sorting by size.
    pub fn spawn_background(&self, path: PathBuf) {
        self.enqueue(path, SizePriority::Background);
    }

    fn enqueue(&self, path: PathBuf, priority: SizePriority) {
        if let Ok(mut guard) = self.shared.queue.lock() {
            if guard.0.push(path, priority) {
                self.shared.ready.notify_one();
            }
        }
    }
}

impl Drop for DirSizeWorker {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.shared.queue.lock() {
            guard.1 = true;
        }
        self.shared.ready.notify_all();
    }
}

impl Default for DirSizeWorker {
    fn default() -> Self {
        Self::new()
    }
}

// ── Index worker ──────────────────────────────────────────────────────────

pub struct IndexWorker {
    pub cancel: Option<Arc<AtomicBool>>,
    pub progress_rx: Option<mpsc::Receiver<usize>>,
    pub done_rx: Option<mpsc::Receiver<Result<usize, String>>>,
}

impl IndexWorker {
    pub fn start(db_path: &Path) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (p_tx, p_rx) = mpsc::channel();
        let (d_tx, d_rx) = mpsc::channel();

        let cancel_clone = cancel.clone();
        let db = db_path.to_path_buf();
        let roots = sys::get_drives();

        std::thread::spawn(move || {
            if db.exists() {
                if let Err(e) = crate::db::remove_index_files(&db) {
                    let _ = d_tx.send(Err(format!("Failed to remove old index: {e}")));
                    return;
                }
            }
            let res = DbManager::index_multiple_to_sqlite(&db, &roots, cancel_clone, p_tx)
                .map_err(|e| e.to_string());
            let _ = d_tx.send(res);
        });

        Self {
            cancel: Some(cancel),
            progress_rx: Some(p_rx),
            done_rx: Some(d_rx),
        }
    }

    pub fn cancel(&self) {
        if let Some(ref flag) = self.cancel {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn poll_progress(&mut self) -> Option<usize> {
        self.progress_rx.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    pub fn poll_done(&mut self) -> Option<Result<usize, String>> {
        if let Some(rx) = &self.done_rx {
            if let Ok(res) = rx.try_recv() {
                self.done_rx = None;
                self.progress_rx = None;
                self.cancel = None;
                return Some(res);
            }
        }
        None
    }
}

// ── Live index maintainer ─────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct IndexLiveStatus {
    pub message: String,
    pub syncing: bool,
}

/// Catch-up + recursive watch so a one-time index stays current.
pub struct IndexMaintainer {
    pub cancel: Option<Arc<AtomicBool>>,
    pub status_rx: Option<mpsc::Receiver<IndexLiveStatus>>,
}

impl IndexMaintainer {
    pub fn start_live(db_path: &Path) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (status_tx, status_rx) = mpsc::channel();
        let db = db_path.to_path_buf();
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            run_index_maintainer(db, cancel_thread, status_tx);
        });
        Self {
            cancel: Some(cancel),
            status_rx: Some(status_rx),
        }
    }

    pub fn cancel(&self) {
        if let Some(ref flag) = self.cancel {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn poll_status(&mut self) -> Option<IndexLiveStatus> {
        self.status_rx.as_ref().and_then(|rx| {
            let mut last = None;
            while let Ok(ev) = rx.try_recv() {
                last = Some(ev);
            }
            last
        })
    }
}

impl Drop for IndexMaintainer {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn index_watch_roots() -> Vec<PathBuf> {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        sys::get_drives()
    } else {
        let mut roots = vec![sys::get_home_dir()];
        for drive in sys::get_drives() {
            if drive != Path::new("/") {
                roots.push(drive);
            }
        }
        roots
    }
}

fn run_index_maintainer(
    db: PathBuf,
    cancel: Arc<AtomicBool>,
    status_tx: mpsc::Sender<IndexLiveStatus>,
) {
    let send = |message: String, syncing: bool| {
        let _ = status_tx.send(IndexLiveStatus { message, syncing });
    };

    if !db.exists() {
        send("Not indexed".into(), false);
        return;
    }

    send("Syncing index…".into(), true);
    let roots = index_watch_roots();
    let (p_tx, p_rx) = mpsc::channel();
    let cancel_sync = cancel.clone();
    let db_sync = db.clone();
    let roots_sync = roots.clone();
    let handle = std::thread::spawn(move || {
        DbManager::sync_roots(&db_sync, &roots_sync, &cancel_sync, &p_tx)
    });
    while !handle.is_finished() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        while let Ok(n) = p_rx.try_recv() {
            send(format!("Syncing index… {n} folders"), true);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let sync_result = handle.join();
    while let Ok(n) = p_rx.try_recv() {
        send(format!("Syncing index… {n} folders"), true);
    }
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    match sync_result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) if crate::db::is_corruption(e.as_ref()) => {
            log::warn!("index is damaged ({e}); moving it aside");
            if let Err(e) = crate::db::quarantine_index(&db) {
                log::error!("could not quarantine damaged index: {e}");
            }
            send("Index damaged — use Rebuild Index".into(), false);
            return;
        }
        Ok(Err(e)) => {
            log::error!("index sync failed: {e}");
            send("Index sync failed — results may be stale".into(), false);
            return;
        }
        Err(_) => {
            send("Index sync crashed — results may be stale".into(), false);
            return;
        }
    }

    send("Index ✓ live".into(), false);

    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = match RecommendedWatcher::new(raw_tx, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("index watcher unavailable: {e}");
            return;
        }
    };
    for root in &roots {
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            log::warn!("index watch {}: {e}", root.display());
        }
    }

    let mut pending: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut last = std::time::Instant::now();
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);
    const PERIODIC: std::time::Duration = std::time::Duration::from_secs(300);
    let mut last_periodic = std::time::Instant::now();

    while !cancel.load(Ordering::Relaxed) {
        let timeout = if pending.is_empty() {
            std::time::Duration::from_millis(500)
        } else {
            DEBOUNCE.saturating_sub(last.elapsed())
        };
        match raw_rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                for path in event.paths {
                    if crate::db::ignored_index_path(&path) {
                        continue;
                    }
                    pending.insert(path);
                    last = std::time::Instant::now();
                }
            }
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if !pending.is_empty() && last.elapsed() >= DEBOUNCE {
            let batch: Vec<PathBuf> = pending.drain().collect();
            let mut dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for path in batch {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if !path.exists() || path.is_dir() {
                    let _ = DbManager::apply_fs_change(&db, &path);
                } else if let Some(parent) = path.parent() {
                    dirs.insert(parent.to_path_buf());
                }
            }
            for dir in dirs {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let _ = DbManager::apply_fs_change(&db, &dir);
            }
        }

        if last_periodic.elapsed() >= PERIODIC {
            last_periodic = std::time::Instant::now();
            let cancel_flag = cancel.clone();
            let (tx, _rx) = mpsc::channel();
            let _ = DbManager::sync_roots(&db, &roots, &cancel_flag, &tx);
        }
    }

    drop(watcher);
}

// ── Folder analyzer worker ────────────────────────────────────────────────

#[allow(dead_code)] // the treemap view lands in Phase 2
pub struct AnalyzerWorker {
    /// The tab that requested the scan, so the result goes back to it rather
    /// than to whichever tab happens to be active when the scan finishes.
    pub tab: TabId,
    /// The directory being scanned, so a result can be discarded if the tab
    /// has since been pointed somewhere else.
    pub target: PathBuf,
    pub rx: Option<mpsc::Receiver<TreemapNode>>,
}

#[allow(dead_code)]
impl AnalyzerWorker {
    pub fn start(tab: TabId, path: PathBuf, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = mpsc::channel();
        let target = path.clone();
        std::thread::spawn(move || {
            let res = StorageAnalyzer::scan_directory(&path, &cancel);
            let _ = tx.send(res);
        });
        Self {
            tab,
            target,
            rx: Some(rx),
        }
    }

    pub fn poll(&mut self) -> Option<TreemapNode> {
        if let Some(rx) = &self.rx {
            if let Ok(res) = rx.try_recv() {
                self.rx = None;
                return Some(res);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// Drain `count` results from the pool, failing rather than hanging.
    fn collect(worker: &DirSizeWorker, count: usize) -> HashMap<PathBuf, Result<u64, String>> {
        let mut out = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while out.len() < count {
            assert!(
                Instant::now() < deadline,
                "timed out after {} results",
                out.len()
            );
            match worker.rx.recv_timeout(Duration::from_millis(250)) {
                Ok((path, res)) => {
                    out.insert(path, res);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("pool disconnected"),
            }
        }
        out
    }

    #[test]
    fn directory_size_sums_nested_files() {
        let tmp = TempDir::new("dirsize");
        tmp.file("a.bin", &[0u8; 100]);
        tmp.file("sub/b.bin", &[0u8; 250]);
        tmp.file("sub/deep/c.bin", &[0u8; 400]);
        tmp.dir("empty");

        assert_eq!(directory_size(tmp.path()), Ok(750));
    }

    #[test]
    fn directory_size_of_an_empty_directory_is_zero() {
        let tmp = TempDir::new("dirsize_empty");
        assert_eq!(directory_size(tmp.path()), Ok(0));
    }

    #[test]
    fn directory_size_reports_an_unreadable_root_as_an_error() {
        // Regression: the previous implementation always reported Ok, so
        // DirectorySizeState::Failed was unreachable and an unreadable folder
        // displayed a confident "0 B".
        let tmp = TempDir::new("dirsize_missing");
        assert!(directory_size(&tmp.path().join("does_not_exist")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn directory_size_skips_an_unreadable_subdirectory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new("dirsize_perm");
        tmp.file("visible.bin", &[0u8; 64]);
        let locked = tmp.dir("locked");
        std::fs::write(locked.join("hidden.bin"), [0u8; 1000]).expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let total = directory_size(tmp.path());

        // Restore before asserting so cleanup always works.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));

        // Best-effort lower bound: the unreadable subtree is skipped, not fatal.
        assert_eq!(total, Ok(64));
    }

    #[test]
    #[cfg(unix)]
    fn directory_size_does_not_follow_symlinks_into_a_loop() {
        let tmp = TempDir::new("dirsize_loop");
        tmp.file("a.bin", &[0u8; 4096]);
        let sub = tmp.dir("sub");
        // A link pointing back at an ancestor would make a follow-symlinks
        // walk recurse until it ran out of stack or wall clock.
        std::os::unix::fs::symlink(tmp.path(), sub.join("loop")).expect("symlink");

        // Reaching this assertion at all is most of the point: the walk
        // terminates. The link itself is counted as its own small entry (the
        // length of the target path, as `du` does), so the total is the real
        // file plus a little -- not the tree counted over and over.
        let total = directory_size(tmp.path()).expect("size");
        assert!(
            (4096..4096 + 1024).contains(&total),
            "expected roughly one 4 KiB file, got {total} -- the link was followed"
        );
    }

    #[test]
    fn pool_serves_far_more_requests_than_it_has_threads() {
        // The point of the pool: this used to be one OS thread per request.
        let tmp = TempDir::new("dirsize_pool");
        let mut expected = Vec::new();
        for i in 0..64 {
            let dir = tmp.dir(&format!("d{i}"));
            std::fs::write(dir.join("f.bin"), vec![0u8; i]).expect("write");
            expected.push((dir, i as u64));
        }

        let worker = DirSizeWorker::new();
        for (dir, _) in &expected {
            worker.spawn_visible(dir.clone());
        }

        let results = collect(&worker, expected.len());
        for (dir, size) in expected {
            assert_eq!(results.get(&dir), Some(&Ok(size)), "for {}", dir.display());
        }
    }

    #[test]
    fn size_queue_serves_visible_rows_before_background_ones() {
        let mut q = SizeQueue::default();
        for i in 0..1_000 {
            q.push(PathBuf::from(format!("bg{i}")), SizePriority::Background);
        }
        q.push(PathBuf::from("row"), SizePriority::Visible);
        assert_eq!(
            q.pop(),
            Some(PathBuf::from("row")),
            "visible jumps the background line"
        );
        assert_eq!(
            q.pop(),
            Some(PathBuf::from("bg0")),
            "background is then served in order"
        );
    }

    #[test]
    fn size_queue_queues_each_path_once() {
        let mut q = SizeQueue::default();
        assert!(q.push(PathBuf::from("a"), SizePriority::Visible));
        assert!(!q.push(PathBuf::from("a"), SizePriority::Visible));
        assert!(
            !q.push(PathBuf::from("a"), SizePriority::Background),
            "already waiting"
        );
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn size_queue_promotes_background_work_when_a_row_becomes_visible() {
        let mut q = SizeQueue::default();
        q.push(PathBuf::from("x"), SizePriority::Background);
        q.push(PathBuf::from("y"), SizePriority::Background);
        assert!(q.push(PathBuf::from("y"), SizePriority::Visible));
        assert_eq!(q.pop(), Some(PathBuf::from("y")));
        assert_eq!(q.pop(), Some(PathBuf::from("x")));
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn size_queue_forgets_a_path_once_served_so_it_can_be_asked_again() {
        let mut q = SizeQueue::default();
        q.push(PathBuf::from("a"), SizePriority::Visible);
        assert_eq!(q.pop(), Some(PathBuf::from("a")));
        assert!(q.push(PathBuf::from("a"), SizePriority::Background));
    }

    #[test]
    fn a_100k_background_queue_does_not_delay_a_visible_request() {
        // The 100k-entry case from the performance notes: every subfolder queued
        // as background work, then the one row on screen asked for.
        let mut q = SizeQueue::default();
        for i in 0..100_000 {
            q.push(PathBuf::from(format!("sub{i}")), SizePriority::Background);
        }
        q.push(PathBuf::from("on_screen"), SizePriority::Visible);
        assert_eq!(q.pop(), Some(PathBuf::from("on_screen")));
        assert_eq!(q.len(), 100_000);
    }

    #[test]
    fn pool_reports_failures_without_dropping_later_work() {
        let tmp = TempDir::new("dirsize_mixed");
        let good = tmp.dir("good");
        std::fs::write(good.join("f.bin"), [0u8; 10]).expect("write");
        let missing = tmp.path().join("missing");

        let worker = DirSizeWorker::new();
        worker.spawn_visible(missing.clone());
        worker.spawn_visible(good.clone());

        let results = collect(&worker, 2);
        assert!(results[&missing].is_err());
        assert_eq!(results[&good], Ok(10));
    }
}
