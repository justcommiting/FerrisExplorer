//! Backend state shared by the Tauri commands, and the pump thread that
//! forwards background results to the webview as events.
//!
//! The workers are the same ones the egui app used; they report over `mpsc`
//! channels that the egui frame loop polled every repaint. Here one thread
//! does that polling and turns each result into an event instead.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::core::VNode;
use crate::ops::OpProgress;
use crate::persistence::IndexStatus;
use crate::search::{SearchUpdate, TabId};
use crate::watch::DirWatcher;
use crate::workers::{DirSizeWorker, IndexMaintainer, IndexWorker};

const PUMP_INTERVAL: Duration = Duration::from_millis(60);

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct IndexState {
    pub message: String,
    pub indexing: bool,
    pub syncing: bool,
    pub exists: bool,
}

#[derive(Default)]
pub struct IndexWorkers {
    pub status: IndexState,
    pub worker: Option<IndexWorker>,
    pub maintainer: Option<IndexMaintainer>,
}

impl IndexWorkers {
    pub fn start_live(&mut self) {
        if self.status.indexing {
            return;
        }
        let db = IndexStatus::db_path();
        if !db.exists() {
            return;
        }
        if let Some(m) = self.maintainer.take() {
            m.cancel();
        }
        self.status.syncing = true;
        self.status.message = "Syncing index…".into();
        self.maintainer = Some(IndexMaintainer::start_live(&db));
    }

    pub fn start_full(&mut self) {
        if let Some(m) = self.maintainer.take() {
            m.cancel();
        }
        if let Some(w) = self.worker.take() {
            w.cancel();
        }
        self.status.indexing = true;
        self.status.syncing = false;
        self.status.message = "Starting indexer…".into();
        let db = IndexStatus::db_path();
        // The rebuild deletes the index file; keep its size history.
        let _ = crate::space::stash_history(&db);
        self.worker = Some(IndexWorker::start(&db));
    }
}

/// The folder listing each tab is waiting on, as a request id and its cancel
/// flag. Starting a listing cancels the tab's previous one, so a folder the
/// user has navigated away from stops being read.
#[derive(Default)]
pub struct ListingRegistry {
    current: HashMap<TabId, (u64, Arc<AtomicBool>)>,
}

impl ListingRegistry {
    /// Registers `request` as the tab's listing and returns its cancel flag.
    /// Any earlier listing for the tab is cancelled.
    pub fn begin(&mut self, tab: TabId, request: u64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Some((_, previous)) = self.current.insert(tab, (request, flag.clone())) {
            previous.store(true, Ordering::Relaxed);
        }
        flag
    }

    /// Cancels `request` if it is still the tab's current listing. A request
    /// that has already been superseded or finished is left alone.
    pub fn cancel(&mut self, tab: TabId, request: u64) -> bool {
        match self.current.get(&tab) {
            Some((current, flag)) if *current == request => {
                flag.store(true, Ordering::Relaxed);
                self.current.remove(&tab);
                true
            }
            _ => false,
        }
    }

    /// Forgets `request` once it has completed, if it is still the tab's current listing.
    pub fn finish(&mut self, tab: TabId, request: u64) {
        if self
            .current
            .get(&tab)
            .is_some_and(|(current, _)| *current == request)
        {
            self.current.remove(&tab);
        }
    }
}

pub struct AppState {
    pub search_tx: Sender<SearchUpdate>,
    pub search_cancels: Mutex<HashMap<TabId, Arc<AtomicBool>>>,
    pub dir_sizes: Mutex<DirSizeWorker>,
    /// Streamed folder listings in progress, keyed by tab.
    pub listings: Mutex<ListingRegistry>,
    pub watcher: Mutex<DirWatcher>,
    pub op_tx: Sender<OpProgress>,
    pub op_cancels: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// Where a paused transfer waits for its conflict decision.
    pub op_decisions: Mutex<HashMap<u64, Sender<crate::ops::Decision>>>,
    pub index: Mutex<IndexWorkers>,
    pub space_cache: Arc<crate::space::SpaceCache>,
    pub space_scan_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The running duplicate scan, if any.
    pub dup_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The latest finished duplicate scan. Trashing checks its requests against this.
    pub dup_scan: Mutex<Option<crate::duplicates::Scan>>,
    /// The running folder comparison, if any.
    pub compare_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The latest finished folder comparison. Sync plans are built from it.
    pub comparison: Mutex<Option<crate::compare::Comparison>>,
    /// The running sync, if any.
    pub sync_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The planned sync waiting to run. Running it takes it.
    pub sync_plan: Mutex<Option<crate::compare::Plan>>,
    /// Folders each window wants watched, keyed by window label.
    pub watch_sets: Mutex<HashMap<String, Vec<PathBuf>>>,
    /// The latest command run on the selection, running or finished. One at a time.
    pub shell_run: Mutex<Option<Arc<crate::shell::ShellRun>>>,
}

impl AppState {
    /// Point the watcher at every folder any window is showing.
    pub fn sync_watches(&self) {
        let mut all: Vec<PathBuf> = match self.watch_sets.lock() {
            Ok(sets) => sets.values().flatten().cloned().collect(),
            Err(_) => return,
        };
        all.sort();
        all.dedup();
        if let Ok(w) = self.watcher.lock() {
            w.watch_dirs(all);
        }
    }

    pub fn forget_window(&self, label: &str) {
        if let Ok(mut sets) = self.watch_sets.lock() {
            sets.remove(label);
        }
        self.sync_watches();
    }
}

/// Receivers that only the pump thread reads.
pub struct PumpChannels {
    search_rx: Receiver<SearchUpdate>,
    op_rx: Receiver<OpProgress>,
}

impl AppState {
    pub fn new() -> (Self, PumpChannels) {
        let (search_tx, search_rx) = mpsc::channel();
        let (op_tx, op_rx) = mpsc::channel();
        let index = IndexWorkers {
            status: IndexState {
                message: IndexStatus::compute(),
                exists: IndexStatus::db_path().exists(),
                ..Default::default()
            },
            ..Default::default()
        };
        (
            Self {
                search_tx,
                search_cancels: Mutex::new(HashMap::new()),
                dir_sizes: Mutex::new(DirSizeWorker::new()),
                listings: Mutex::new(ListingRegistry::default()),
                watcher: Mutex::new(DirWatcher::new()),
                op_tx,
                op_cancels: Mutex::new(HashMap::new()),
                op_decisions: Mutex::new(HashMap::new()),
                index: Mutex::new(index),
                space_cache: Arc::new(Mutex::new(None)),
                space_scan_cancel: Mutex::new(None),
                dup_cancel: Mutex::new(None),
                dup_scan: Mutex::new(None),
                compare_cancel: Mutex::new(None),
                comparison: Mutex::new(None),
                sync_cancel: Mutex::new(None),
                sync_plan: Mutex::new(None),
                watch_sets: Mutex::new(HashMap::new()),
                shell_run: Mutex::new(None),
            },
            PumpChannels { search_rx, op_rx },
        )
    }
}

#[derive(Clone, serde::Serialize)]
struct SearchEvent {
    tab: TabId,
    query: String,
    results: Vec<VNode>,
    error: Option<String>,
    complete: bool,
}

#[derive(Clone, serde::Serialize)]
struct DirSizeEvent {
    path: PathBuf,
    size: Option<u64>,
}

/// Start the thread that drains worker channels and emits events.
pub fn spawn_pump(app: AppHandle, channels: PumpChannels) {
    std::thread::Builder::new()
        .name("event-pump".into())
        .spawn(move || {
            let mut index_notice = IndexChangeNotifier::default();
            let mut snapshot_check = None;
            loop {
                pump_once(&app, &channels);
                if index_notice.due(crate::db::index_generation(), Instant::now()) {
                    let _ = app.emit("index-changed", ());
                }
                snapshot_tick(&app, &mut snapshot_check, Instant::now());
                std::thread::sleep(PUMP_INTERVAL);
            }
        })
        .expect("spawn event pump");
}

/// Set when a full index build finishes. The next tick takes a snapshot.
static SNAPSHOT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Held while a timer-driven snapshot is being written.
static SNAPSHOT_RUNNING: AtomicBool = AtomicBool::new(false);
/// How often the timer checks whether a daily snapshot is due.
const SNAPSHOT_CHECK_GAP: Duration = Duration::from_secs(600);

/// Take size snapshots for the "what grew" view: one after each full build,
/// and otherwise at most one a day while the app runs. Writes happen on a
/// thread so the pump never waits on them.
fn snapshot_tick(app: &AppHandle, next_check: &mut Option<Instant>, now: Instant) {
    let requested = SNAPSHOT_REQUESTED.swap(false, Ordering::AcqRel);
    let timer_due = next_check.is_none_or(|t| now >= t);
    if !requested && !timer_due {
        return;
    }
    if timer_due {
        *next_check = Some(now + SNAPSHOT_CHECK_GAP);
    }
    let db = IndexStatus::db_path();
    let idle = app
        .state::<AppState>()
        .index
        .lock()
        .map(|i| !i.status.indexing)
        .unwrap_or(false);
    if !idle || !db.exists() {
        return;
    }
    let unix_now = crate::space::now_unix();
    if requested {
        std::thread::spawn(move || {
            let _ = crate::space::take_snapshot(&db, unix_now);
        });
    } else if !SNAPSHOT_RUNNING.swap(true, Ordering::AcqRel) {
        std::thread::spawn(move || {
            if crate::space::snapshot_due(&db, unix_now) {
                let _ = crate::space::take_snapshot(&db, unix_now);
            }
            SNAPSHOT_RUNNING.store(false, Ordering::Release);
        });
    }
}

/// Emit `index-changed` at most once per `INDEX_CHANGED_GAP`. A change that
/// lands inside the gap is announced by a later poll, so the last one is
/// never dropped.
const INDEX_CHANGED_GAP: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct IndexChangeNotifier {
    seen: u64,
    last_emit: Option<Instant>,
}

impl IndexChangeNotifier {
    /// True when the index generation has moved since the last announcement
    /// and the gap since that announcement has passed.
    pub fn due(&mut self, generation: u64, now: Instant) -> bool {
        if generation == self.seen {
            return false;
        }
        if self
            .last_emit
            .is_some_and(|t| now.duration_since(t) < INDEX_CHANGED_GAP)
        {
            return false;
        }
        self.seen = generation;
        self.last_emit = Some(now);
        true
    }
}

fn pump_once(app: &AppHandle, ch: &PumpChannels) {
    let state = app.state::<AppState>();

    while let Ok(u) = ch.search_rx.try_recv() {
        if u.complete {
            if let Ok(mut map) = state.search_cancels.lock() {
                map.remove(&u.tab);
            }
        }
        let _ = app.emit(
            "search-update",
            SearchEvent {
                tab: u.tab,
                query: u.query,
                results: u.results,
                error: u.error,
                complete: u.complete,
            },
        );
    }

    while let Ok(p) = ch.op_rx.try_recv() {
        if p.done {
            if let Ok(mut map) = state.op_cancels.lock() {
                map.remove(&p.id);
            }
            if let Ok(mut map) = state.op_decisions.lock() {
                map.remove(&p.id);
            }
        }
        let _ = app.emit("op-progress", p);
    }

    if let Ok(worker) = state.dir_sizes.lock() {
        while let Ok((path, res)) = worker.rx.try_recv() {
            let _ = app.emit(
                "dir-size",
                DirSizeEvent {
                    path,
                    size: res.ok(),
                },
            );
        }
    }

    let mut changed = Vec::new();
    if let Ok(watcher) = state.watcher.lock() {
        while let Ok(dir) = watcher.rx.try_recv() {
            changed.push(dir);
        }
    }
    if !changed.is_empty() {
        changed.sort();
        changed.dedup();
        let db = IndexStatus::db_path();
        for dir in changed {
            if db.exists() {
                queue_index_refresh(db.clone(), dir.clone());
            }
            let _ = app.emit("dir-changed", dir);
        }
    }

    pump_index(app, &state);
}

/// Hand a changed folder to the refresh worker. Events are not each given a
/// thread, so a burst of them cannot open a SQLite connection apiece.
fn queue_index_refresh(db: PathBuf, dir: PathBuf) {
    static TX: OnceLock<Sender<(PathBuf, PathBuf)>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(PathBuf, PathBuf)>();
        std::thread::Builder::new()
            .name("index-refresh".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Everything queued while the last batch ran, once each.
                    let mut batch = BTreeSet::new();
                    batch.insert(first);
                    while let Ok(next) = rx.try_recv() {
                        batch.insert(next);
                    }
                    for (db, dir) in batch {
                        let _ = crate::db::DbManager::refresh_directory(&db, &dir);
                    }
                }
            })
            .expect("spawn index refresh worker");
        tx
    });
    let _ = tx.send((db, dir));
}

fn pump_index(app: &AppHandle, state: &AppState) {
    let Ok(mut idx) = state.index.lock() else {
        return;
    };
    let before = idx.status.clone();

    if let Some(worker) = idx.worker.as_mut() {
        let mut progress = None;
        while let Some(count) = worker.poll_progress() {
            progress = Some(count);
        }
        let done = worker.poll_done();
        if let Some(count) = progress {
            idx.status.message = format!("Indexing: {count} files…");
        }
        if let Some(res) = done {
            idx.worker = None;
            idx.status.indexing = false;
            match res {
                Ok(count) => {
                    let kb = std::fs::metadata(IndexStatus::db_path())
                        .map(|m| m.len() / 1024)
                        .unwrap_or(0);
                    idx.status.message = format!("Index ✓ ({count} files, {kb} KB)");
                    idx.start_live();
                    SNAPSHOT_REQUESTED.store(true, Ordering::Release);
                    // Sizes read mid-build are wrong; the next request reloads.
                    if let Ok(mut cache) = state.space_cache.lock() {
                        *cache = None;
                    }
                }
                Err(e) => idx.status.message = format!("Index failed: {e}"),
            }
        }
    }

    if idx.maintainer.is_none() && !idx.status.indexing && IndexStatus::db_path().exists() {
        idx.start_live();
    }

    if let Some(m) = idx.maintainer.as_mut() {
        if let Some(ev) = m.poll_status() {
            idx.status.message = ev.message;
            idx.status.syncing = ev.syncing;
        }
    }

    idx.status.exists = IndexStatus::db_path().exists();
    if idx.status != before {
        let _ = app.emit("index-status", idx.status.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_a_listing_cancels_the_tabs_previous_one() {
        let mut reg = ListingRegistry::default();
        let first = reg.begin(7, 1);
        let second = reg.begin(7, 2);
        assert!(
            first.load(Ordering::Relaxed),
            "the superseded listing is cancelled"
        );
        assert!(!second.load(Ordering::Relaxed));
        assert!(
            reg.cancel(7, 2),
            "the second request is the one still registered"
        );
        assert!(!reg.cancel(7, 1), "the first was superseded");
    }

    #[test]
    fn a_stale_cancel_does_not_touch_the_current_listing() {
        let mut reg = ListingRegistry::default();
        let _old = reg.begin(7, 1);
        let current = reg.begin(7, 2);
        assert!(!reg.cancel(7, 1), "request 1 was already replaced");
        assert!(!current.load(Ordering::Relaxed));
        assert!(reg.cancel(7, 2));
        assert!(current.load(Ordering::Relaxed));
    }

    #[test]
    fn listings_in_other_tabs_are_independent() {
        let mut reg = ListingRegistry::default();
        let a = reg.begin(1, 10);
        let _b = reg.begin(2, 20);
        assert!(!a.load(Ordering::Relaxed));
        assert!(reg.cancel(2, 20));
        assert!(!a.load(Ordering::Relaxed), "tab 1 is unaffected");
    }

    #[test]
    fn a_finished_listing_cannot_be_cancelled_later() {
        let mut reg = ListingRegistry::default();
        let flag = reg.begin(3, 5);
        reg.finish(3, 5);
        assert!(!reg.cancel(3, 5));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn index_changes_are_throttled_and_the_last_one_is_announced() {
        let t0 = Instant::now();
        let mut n = IndexChangeNotifier::default();
        assert!(!n.due(0, t0), "no write yet, nothing to announce");
        assert!(n.due(1, t0), "the first change is announced at once");
        assert!(!n.due(2, t0 + Duration::from_millis(300)), "inside the gap");
        assert!(
            !n.due(3, t0 + Duration::from_millis(600)),
            "still inside the gap"
        );
        assert!(
            n.due(3, t0 + INDEX_CHANGED_GAP),
            "trailing change announced after the gap"
        );
        assert!(
            !n.due(3, t0 + INDEX_CHANGED_GAP * 3),
            "no new write, no new event"
        );
    }
}
