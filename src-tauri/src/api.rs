//! Tauri commands: the boundary between the React UI and the Rust core.
//!
//! Everything that touches the disk runs on the blocking pool so the IPC
//! thread stays free. Long-running work (search, copies, indexing) starts
//! here and reports back through events emitted by `state::spawn_pump`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::State;

use crate::core::VNode;
use crate::ops::{self, OpKind, OpProgress};
use crate::persistence::{
    FavoritesStore, IndexStatus, SavedSearch, SavedSearchesStore, SearchHistoryStore,
};
use crate::recycle;
use crate::search::{ParsedQuery, TabId};
use crate::state::{AppState, IndexState};

type CmdResult<T> = Result<T, String>;

async fn blocking<T, F>(f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> CmdResult<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Locations ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct Places {
    home: PathBuf,
    drives: Vec<PathBuf>,
    separator: char,
}

#[tauri::command]
pub async fn places() -> CmdResult<Places> {
    blocking(|| {
        Ok(Places {
            home: crate::sys::get_home_dir(),
            drives: crate::sys::get_drives(),
            separator: std::path::MAIN_SEPARATOR,
        })
    })
    .await
}

#[derive(serde::Serialize)]
pub struct DriveUsage {
    path: PathBuf,
    total: u64,
    free: u64,
}

/// Capacity of each drive, for the sidebar's usage bars. Drives that cannot
/// be queried (an empty card reader, a disconnected share) are left out.
#[tauri::command]
pub async fn drive_usage() -> CmdResult<Vec<DriveUsage>> {
    blocking(|| {
        Ok(crate::sys::get_drives()
            .into_iter()
            .filter_map(|path| {
                let stats = fs4::statvfs(&path).ok()?;
                (stats.total_space() > 0).then(|| DriveUsage {
                    total: stats.total_space(),
                    free: stats.available_space(),
                    path,
                })
            })
            .collect())
    })
    .await
}

#[tauri::command]
pub async fn list_dir(path: PathBuf) -> CmdResult<Vec<VNode>> {
    blocking(move || {
        // An archive (or a folder inside one) is listed from its index instead.
        if !path.is_dir() {
            if let Some(loc) = crate::archive::locate(&path) {
                return crate::archive::list(&loc, crate::archive::LIMITS);
            }
        }
        crate::core::list_dir(&path).map_err(|e| crate::core::describe_list_error(&e))
    })
    .await
}

/// Payload of a `listing-chunk` event: one batch of a streamed listing.
#[derive(Clone, serde::Serialize)]
struct ListingChunk {
    tab: TabId,
    request: u64,
    /// The next batch of entries, in folder order. Empty on a failed or empty listing.
    entries: Vec<VNode>,
    /// True on the last event of the request.
    done: bool,
    error: Option<String>,
}

/// Lists `path` for `tab` in batches, sent to this window as `listing-chunk`
/// events. The first batch is small so it paints at once; a listing that is
/// cancelled or superseded sends nothing more. Returns immediately: the
/// listing runs on the blocking pool.
#[tauri::command]
pub async fn list_dir_stream(
    window: tauri::Window,
    state: State<'_, AppState>,
    tab: TabId,
    request: u64,
    path: PathBuf,
) -> CmdResult<()> {
    let cancel = state
        .listings
        .lock()
        .map_err(|e| e.to_string())?
        .begin(tab, request);
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::{Emitter, Manager};
        let emit = |entries: Vec<VNode>, done: bool, error: Option<String>| {
            // `emit` would reach every window; only this one is listing `path`.
            let _ = window.emit_to(
                window.label(),
                "listing-chunk",
                ListingChunk {
                    tab,
                    request,
                    entries,
                    done,
                    error,
                },
            );
        };
        // An archive is read from its index in one go; there is nothing to stream.
        if !path.is_dir() {
            if let Some(loc) = crate::archive::locate(&path) {
                match crate::archive::list(&loc, crate::archive::LIMITS) {
                    Ok(entries) => emit(entries, true, None),
                    Err(e) => emit(Vec::new(), true, Some(e)),
                }
                return;
            }
        }
        let outcome =
            crate::core::list_dir_chunked(&path, &cancel, |batch| emit(batch, false, None));
        match outcome {
            Ok(crate::core::ListEnd::Complete) => emit(Vec::new(), true, None),
            // Cancelled: the request is stale, so nothing more is sent.
            Ok(crate::core::ListEnd::Cancelled) => {}
            Err(e) => emit(Vec::new(), true, Some(crate::core::describe_list_error(&e))),
        }
        // Release the registry entry (a no-op if this request was superseded).
        let _ = window
            .app_handle()
            .state::<AppState>()
            .listings
            .lock()
            .map(|mut reg| reg.finish(tab, request));
    });
    Ok(())
}

/// Stops a streamed listing the UI no longer wants. A request that is no
/// longer the tab's current one is ignored.
#[tauri::command]
pub async fn cancel_listing(state: State<'_, AppState>, tab: TabId, request: u64) -> CmdResult<()> {
    state
        .listings
        .lock()
        .map_err(|e| e.to_string())?
        .cancel(tab, request);
    Ok(())
}

/// Whether a path is inside an archive, and which one. The archive's own root
/// counts as inside, with an empty `inner`.
#[tauri::command]
pub async fn archive_context(path: PathBuf) -> CmdResult<Option<crate::archive::Location>> {
    blocking(move || Ok(crate::archive::locate(&path))).await
}

/// Resolve what the user typed in the location bar to an existing folder.
#[tauri::command]
pub async fn resolve_location(raw: String) -> CmdResult<PathBuf> {
    blocking(move || {
        let path = crate::desktop::expand_user_path(&raw);
        if path.is_dir() {
            Ok(path)
        } else if path.exists() {
            Err(format!("{} is not a folder", path.display()))
        } else {
            Err(format!("{} does not exist", path.display()))
        }
    })
    .await
}

/// Whether a node should be entered as a folder (follows folder symlinks).
#[tauri::command]
pub async fn is_navigable(path: PathBuf) -> CmdResult<bool> {
    blocking(move || {
        Ok(std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false))
    })
    .await
}

/// Watch the folders a window's tabs show. Each window reports its own set;
/// the watcher follows their union. Async so it never runs on the main thread.
#[tauri::command]
pub async fn watch_dirs(
    state: State<'_, AppState>,
    window: tauri::Window,
    dirs: Vec<PathBuf>,
) -> CmdResult<()> {
    state
        .watch_sets
        .lock()
        .map_err(|e| e.to_string())?
        .insert(window.label().to_string(), dirs);
    state.sync_watches();
    Ok(())
}

/// Size these folders at visible priority, ahead of any background work.
#[tauri::command]
pub async fn request_dir_sizes(state: State<'_, AppState>, paths: Vec<PathBuf>) -> CmdResult<()> {
    let worker = state.dir_sizes.lock().map_err(|e| e.to_string())?;
    for p in paths {
        worker.spawn_visible(p);
    }
    Ok(())
}

// ── Search ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct SearchStarted {
    interpreted_as: Option<String>,
    errors: Vec<String>,
}

/// Start a search for `tab`. Results arrive as `search-update` events.
#[tauri::command]
pub fn start_search(
    state: State<'_, AppState>,
    tab: TabId,
    query: String,
    current_dir: PathBuf,
) -> CmdResult<SearchStarted> {
    cancel_tab_search(&state, tab);
    let mut parsed = ParsedQuery::parse(&query);
    let started = SearchStarted {
        interpreted_as: parsed.interpreted_as.clone(),
        errors: parsed.errors.clone(),
    };
    if query.trim().is_empty() {
        return Ok(started);
    }
    // Filter-only queries (`ext:rs`, `size:>1gb`) run against the whole
    // index. Free text and `here:` stay in the current tree.
    if !parsed.is_index_wide() {
        parsed.scope_path = Some(current_dir);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    state
        .search_cancels
        .lock()
        .map_err(|e| e.to_string())?
        .insert(tab, cancel.clone());

    let db = IndexStatus::db_path();
    let indexing = state
        .index
        .lock()
        .map(|i| i.status.indexing)
        .unwrap_or(true);
    let index_ready = db.exists() && !indexing;
    let db_opt = db.exists().then_some(db);
    let tx = state.search_tx.clone();
    std::thread::spawn(move || {
        crate::search::run_search(parsed, db_opt, index_ready, cancel, tx, tab, query);
    });
    Ok(started)
}

fn cancel_tab_search(state: &AppState, tab: TabId) {
    if let Ok(mut map) = state.search_cancels.lock() {
        if let Some(flag) = map.remove(&tab) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

#[tauri::command]
pub fn cancel_search(state: State<'_, AppState>, tab: TabId) {
    cancel_tab_search(&state, tab);
}

/// Index-only lookup for the Ctrl+P "go to file" overlay.
#[tauri::command]
pub async fn quick_search(query: String) -> CmdResult<Vec<VNode>> {
    blocking(move || {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let db = IndexStatus::db_path();
        if !db.exists() {
            return Ok(Vec::new());
        }
        let parsed = ParsedQuery::parse(&query);
        let res =
            crate::db::DbManager::query_disk_index(&db, &parsed).map_err(|e| e.to_string())?;
        let mut ranked = crate::search::filter_and_rank(res, &parsed, now_secs());
        ranked.truncate(50);
        Ok(ranked)
    })
    .await
}

#[derive(serde::Serialize)]
pub struct SmartCount {
    count: usize,
    /// The index returns at most `INDEX_CANDIDATE_LIMIT` rows per table, so
    /// a count at that limit is a lower bound.
    capped: bool,
}

/// Match count for a smart folder's sidebar badge. None when the query
/// needs a folder walk, or there is no index to count from.
#[tauri::command]
pub async fn count_smart_folder(query: String) -> CmdResult<Option<SmartCount>> {
    blocking(move || {
        let parsed = ParsedQuery::parse(&query);
        let db = IndexStatus::db_path();
        if !parsed.is_index_wide() || !db.exists() {
            return Ok(None);
        }
        let res =
            crate::db::DbManager::query_disk_index(&db, &parsed).map_err(|e| e.to_string())?;
        let limit = crate::db::DbManager::INDEX_CANDIDATE_LIMIT as usize;
        Ok(Some(SmartCount {
            count: res.len(),
            capped: res.len() >= limit,
        }))
    })
    .await
}

// ── Index ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn index_status(state: State<'_, AppState>) -> CmdResult<IndexState> {
    Ok(state
        .index
        .lock()
        .map_err(|e| e.to_string())?
        .status
        .clone())
}

#[tauri::command]
pub fn rebuild_index(state: State<'_, AppState>) -> CmdResult<()> {
    state.index.lock().map_err(|e| e.to_string())?.start_full();
    Ok(())
}

#[tauri::command]
pub async fn space_hoggers() -> CmdResult<Vec<(PathBuf, u64)>> {
    blocking(|| {
        let db = IndexStatus::db_path();
        if !db.exists() {
            return Err("No index yet. Build the index first.".into());
        }
        crate::db::DbManager::query_space_hoggers_from_sqlite(&db).map_err(|e| e.to_string())
    })
    .await
}

/// Folder sizes under `root` (default: home) for the treemap. Uses a cached
/// snapshot of the index; `refresh` re-reads it. Without an index, or for a
/// folder the index does not cover, scans one level from disk instead.
#[tauri::command]
pub async fn space_tree(
    state: State<'_, AppState>,
    root: Option<PathBuf>,
    refresh: bool,
) -> CmdResult<SpaceTree> {
    let root = root.unwrap_or_else(crate::sys::get_home_dir);
    let cache = state.space_cache.clone();
    // A new request supersedes any disk scan still running for an old one.
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = state.space_scan_cancel.lock() {
        if let Some(old) = slot.replace(cancel.clone()) {
            old.store(true, Ordering::Relaxed);
        }
    }
    blocking(move || {
        let db = IndexStatus::db_path();
        if db.exists() {
            let idx = crate::space::cached(&cache, &db, refresh)?;
            if let Some(tree) = idx.tree(&root, 2, 36) {
                return Ok(SpaceTree {
                    tree,
                    from_index: true,
                });
            }
        }
        let tree = crate::space::scan(&root, 36, &cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        Ok(SpaceTree {
            tree,
            from_index: false,
        })
    })
    .await
}

/// Folder size changes over `period`: live sizes against the snapshot nearest
/// one period ago. Needs the index, since snapshots are kept in it.
#[tauri::command]
pub async fn space_changes(
    state: State<'_, AppState>,
    period: crate::space::Period,
) -> CmdResult<crate::space::Changes> {
    let cache = state.space_cache.clone();
    blocking(move || {
        let db = IndexStatus::db_path();
        if !db.exists() {
            return Err("No index yet. Build the index first.".into());
        }
        let live = crate::space::cached(&cache, &db, false)?;
        crate::space::changes(&db, &live, period, crate::space::now_unix())
    })
    .await
}

/// Stop a space-usage disk scan, e.g. when its window is closed.
#[tauri::command]
pub fn cancel_space_scan(state: State<'_, AppState>) {
    if let Ok(mut slot) = state.space_scan_cancel.lock() {
        if let Some(flag) = slot.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

// ── Duplicate finder ──────────────────────────────────────────────────────

const DUP_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// What the duplicate finder searches.
#[derive(serde::Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum DupScope {
    /// The folder and everything under it.
    Folder,
    /// Every file in the search index.
    Index,
}

#[derive(serde::Serialize, Clone)]
struct DupProgressEvent {
    id: u64,
    phase: &'static str,
    done: u64,
    total: u64,
}

#[derive(serde::Serialize, Clone)]
struct DupDoneEvent {
    id: u64,
    scan: crate::duplicates::Scan,
    error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct DuplicateTrashOutcome {
    pub removed: recycle::Removed,
    /// Requested files left alone, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    /// The scan without the files that were trashed.
    pub scan: crate::duplicates::Scan,
}

/// Start a duplicate scan on a background thread and return its id. Progress
/// arrives as `duplicates-progress` events and the result as `duplicates-done`.
/// Starting a scan cancels any scan still running.
#[tauri::command]
pub fn start_duplicate_scan(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    scope: DupScope,
    root: PathBuf,
) -> CmdResult<u64> {
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = state.dup_cancel.lock() {
        if let Some(old) = slot.replace(cancel.clone()) {
            old.store(true, Ordering::Relaxed);
        }
    }
    let id = crate::duplicates::next_id();
    let db = IndexStatus::db_path();
    std::thread::Builder::new()
        .name("duplicate-finder".into())
        .spawn(move || {
            use tauri::{Emitter, Manager};

            let mut last = std::time::Instant::now();
            let mut progress = |phase: &'static str, done: u64, total: u64| {
                if last.elapsed() >= DUP_PROGRESS_INTERVAL || done == total {
                    last = std::time::Instant::now();
                    let _ = app.emit(
                        "duplicates-progress",
                        DupProgressEvent {
                            id,
                            phase,
                            done,
                            total,
                        },
                    );
                }
            };
            let found = match scope {
                DupScope::Folder => Ok(crate::duplicates::scan_folder(
                    id,
                    &root,
                    &cancel,
                    &mut progress,
                )),
                DupScope::Index => crate::duplicates::collect_index(&db, &cancel).map(|found| {
                    found.map(|c| crate::duplicates::group_copies(id, c, &cancel, &mut progress))
                }),
            };
            let (scan, error) = match found {
                Ok(Some(scan)) => (scan, None),
                Ok(None) => (
                    crate::duplicates::Scan {
                        id,
                        cancelled: true,
                        ..Default::default()
                    },
                    None,
                ),
                Err(e) => (
                    crate::duplicates::Scan {
                        id,
                        ..Default::default()
                    },
                    Some(e),
                ),
            };
            // A scan that was cancelled or superseded must not replace the current result.
            if !cancel.load(Ordering::Relaxed) {
                let state = app.state::<AppState>();
                let mut slot = state.dup_scan.lock().ok();
                if let Some(slot) = slot.as_deref_mut() {
                    *slot = Some(scan.clone());
                }
            }
            let _ = app.emit("duplicates-done", DupDoneEvent { id, scan, error });
        })
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Stop the running duplicate scan, if there is one.
#[tauri::command]
pub fn cancel_duplicate_scan(state: State<'_, AppState>) {
    if let Ok(mut slot) = state.dup_cancel.lock() {
        if let Some(flag) = slot.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Move the chosen copies to the Trash, undoable like any other trash. The
/// request is checked against scan `id`: files changed since the scan are
/// skipped, and a request that would remove every copy in a set is refused.
#[tauri::command]
pub async fn trash_duplicates(
    state: State<'_, AppState>,
    id: u64,
    paths: Vec<PathBuf>,
) -> CmdResult<DuplicateTrashOutcome> {
    let mut scan = {
        let slot = state.dup_scan.lock().map_err(|e| e.to_string())?;
        match slot.as_ref() {
            Some(s) if s.id == id => s.clone(),
            _ => return Err("These duplicates are out of date. Run the search again.".into()),
        }
    };
    let plan = crate::duplicates::plan_trash(&scan, &paths, &crate::duplicates::disk_stat)?;
    crate::archive::ensure_writable(&plan.to_trash)?;
    let to_trash = plan.to_trash;
    let removed = blocking(move || {
        Ok(recycle::delete_items(
            &recycle::OsTrasher,
            recycle::Mode::Trash,
            &to_trash,
        ))
    })
    .await?;
    forget_in_index(&removed.done);
    crate::duplicates::forget(&mut scan, &removed.done);
    if let Ok(mut slot) = state.dup_scan.lock() {
        if let Some(s) = slot.as_mut().filter(|s| s.id == id) {
            *s = scan.clone();
        }
    }
    Ok(DuplicateTrashOutcome {
        removed,
        skipped: plan.skipped,
        scan,
    })
}

// ── Folder compare and sync ───────────────────────────────────────────────

const COMPARE_STALE: &str = "This comparison is out of date. Compare the folders again.";
const PLAN_STALE: &str = "This sync is out of date. Review it again.";

#[derive(serde::Serialize, Clone)]
struct CompareProgressEvent {
    id: u64,
    phase: &'static str,
    done: u64,
    total: u64,
}

#[derive(serde::Serialize, Clone)]
struct CompareDoneEvent {
    id: u64,
    /// None when the comparison was cancelled.
    comparison: Option<crate::compare::Comparison>,
    error: Option<String>,
}

#[derive(serde::Serialize, Clone)]
struct SyncProgressEvent {
    id: u64,
    done: u64,
    total: u64,
}

#[derive(serde::Serialize, Clone)]
struct SyncDoneEvent {
    id: u64,
    outcome: Option<crate::compare::Outcome>,
    error: Option<String>,
}

/// Compare two folders on a background thread and return its id. Progress
/// arrives as `compare-progress` and the result as `compare-done`. Starting a
/// comparison cancels any still running. Folders that cannot be compared are
/// refused here, before anything starts.
#[tauri::command]
pub fn start_compare(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    left: PathBuf,
    right: PathBuf,
    by: crate::compare::CompareBy,
) -> CmdResult<u64> {
    crate::compare::check_roots(&left, &right)?;
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = state.compare_cancel.lock() {
        if let Some(old) = slot.replace(cancel.clone()) {
            old.store(true, Ordering::Relaxed);
        }
    }
    let id = crate::compare::next_id();
    std::thread::Builder::new()
        .name("folder-compare".into())
        .spawn(move || {
            use tauri::{Emitter, Manager};

            let mut last = std::time::Instant::now();
            let mut progress = |phase: &'static str, done: u64, total: u64| {
                if last.elapsed() >= DUP_PROGRESS_INTERVAL || (total > 0 && done == total) {
                    last = std::time::Instant::now();
                    let _ = app.emit(
                        "compare-progress",
                        CompareProgressEvent {
                            id,
                            phase,
                            done,
                            total,
                        },
                    );
                }
            };
            let (comparison, error) =
                match crate::compare::compare(id, &left, &right, by, &cancel, &mut progress) {
                    Ok(found) => (found, None),
                    Err(e) => (None, Some(e)),
                };
            // A comparison that was cancelled or superseded must not replace the current one.
            if let Some(c) = comparison
                .as_ref()
                .filter(|_| !cancel.load(Ordering::Relaxed))
            {
                let state = app.state::<AppState>();
                let stored = c.clone();
                if let Ok(mut slot) = state.comparison.lock() {
                    *slot = Some(stored);
                };
            }
            let _ = app.emit(
                "compare-done",
                CompareDoneEvent {
                    id,
                    comparison,
                    error,
                },
            );
        })
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Stop the running comparison, if there is one.
#[tauri::command]
pub fn cancel_compare(state: State<'_, AppState>) {
    if let Ok(mut slot) = state.compare_cancel.lock() {
        if let Some(flag) = slot.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// The picks a bulk rule makes for comparison `id`. The client replaces its
/// picks with these.
#[tauri::command]
pub fn compare_bulk(
    state: State<'_, AppState>,
    id: u64,
    bulk: crate::compare::Bulk,
) -> CmdResult<Vec<(String, crate::compare::Pick)>> {
    let slot = state.comparison.lock().map_err(|e| e.to_string())?;
    match slot.as_ref() {
        Some(c) if c.id == id => Ok(crate::compare::bulk_picks(c, bulk)),
        _ => Err(COMPARE_STALE.into()),
    }
}

/// Plan the sync of comparison `id` from the picks. Nothing on disk changes
/// yet. The summary says what `run_sync` will do, and replaces any earlier plan.
#[tauri::command]
pub async fn plan_sync(
    state: State<'_, AppState>,
    id: u64,
    picks: Vec<(String, crate::compare::Pick)>,
) -> CmdResult<crate::compare::SyncSummary> {
    let comparison = {
        let slot = state.comparison.lock().map_err(|e| e.to_string())?;
        match slot.as_ref() {
            Some(c) if c.id == id => c.clone(),
            _ => return Err(COMPARE_STALE.into()),
        }
    };
    let plan_id = crate::compare::next_id();
    let plan = blocking(move || crate::compare::plan(&comparison, plan_id, &picks)).await?;
    let summary = plan.summary();
    if let Ok(mut slot) = state.sync_plan.lock() {
        *slot = Some(plan);
    }
    Ok(summary)
}

/// Run the planned sync `plan_id` on a background thread and return its id.
/// Progress arrives as `sync-progress` and the outcome as `sync-done`. Every
/// item that is replaced or removed goes to the Trash.
#[tauri::command]
pub fn run_sync(app: tauri::AppHandle, state: State<'_, AppState>, plan_id: u64) -> CmdResult<u64> {
    let plan = match state.sync_plan.lock().map_err(|e| e.to_string())?.take() {
        Some(p) if p.id == plan_id => p,
        _ => return Err(PLAN_STALE.into()),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = state.sync_cancel.lock() {
        if let Some(old) = slot.replace(cancel.clone()) {
            old.store(true, Ordering::Relaxed);
        }
    }
    let id = plan.id;
    std::thread::Builder::new()
        .name("folder-sync".into())
        .spawn(move || {
            use tauri::Emitter;

            let mut last = std::time::Instant::now();
            let mut progress = |done: u64, total: u64| {
                if last.elapsed() >= DUP_PROGRESS_INTERVAL || done == total {
                    last = std::time::Instant::now();
                    let _ = app.emit("sync-progress", SyncProgressEvent { id, done, total });
                }
            };
            let (outcome, error) =
                match crate::compare::execute(&recycle::OsTrasher, &plan, &cancel, &mut progress) {
                    Ok(outcome) => (Some(outcome), None),
                    Err(e) => (None, Some(e)),
                };
            if let Some(o) = &outcome {
                forget_in_index(&o.trashed);
            }
            let _ = app.emit("sync-done", SyncDoneEvent { id, outcome, error });
        })
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Stop the running sync between actions. Finished actions stay done and can be undone.
#[tauri::command]
pub fn cancel_sync(state: State<'_, AppState>) {
    if let Ok(mut slot) = state.sync_cancel.lock() {
        if let Some(flag) = slot.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Total size of each folder in `paths`. Folders the index covers are
/// answered at once from it; the rest (or all, with `fresh`) are measured
/// on the background pool and arrive later as `dir-size` events. With
/// `background`, the rest queue behind visible rows (used when sorting by
/// size, where every folder is needed).
#[tauri::command]
pub async fn folder_sizes(
    state: State<'_, AppState>,
    paths: Vec<PathBuf>,
    fresh: bool,
    background: Option<bool>,
) -> CmdResult<Vec<(PathBuf, u64)>> {
    let cache = state.space_cache.clone();
    // A half-built index reports zeros, so measure from disk while it builds.
    let indexing = state
        .index
        .lock()
        .map(|i| i.status.indexing)
        .unwrap_or(true);
    let (known, unknown) = blocking(move || {
        let db = IndexStatus::db_path();
        if fresh || indexing || !db.exists() {
            return Ok((Vec::new(), paths));
        }
        let idx = crate::space::cached(&cache, &db, false)?;
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for p in paths {
            match idx.size_of(&p) {
                // Zero may mean "not indexed yet" rather than "empty"; an
                // empty folder costs nothing to confirm on disk.
                Some(size) if size > 0 => known.push((p, size)),
                _ => unknown.push(p),
            }
        }
        Ok((known, unknown))
    })
    .await?;
    let worker = state.dir_sizes.lock().map_err(|e| e.to_string())?;
    let background = background.unwrap_or(false);
    for p in unknown {
        if background {
            worker.spawn_background(p);
        } else {
            worker.spawn_visible(p);
        }
    }
    Ok(known)
}

/// Builder for every explorer window, the first one included, so they all
/// share one frame. The tab strip doubles as the title bar: on Windows the
/// native frame is removed and the UI draws its own caption buttons; on
/// macOS the title bar is transparent and the traffic lights sit in the tab
/// strip. Linux keeps the native frame.
pub fn explorer_window(
    app: &tauri::AppHandle,
    label: String,
) -> tauri::WebviewWindowBuilder<'_, tauri::Wry, tauri::AppHandle> {
    let builder =
        tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::App("index.html".into()))
            .title("FerrisExplorer")
            .inner_size(1180.0, 740.0)
            .min_inner_size(800.0, 500.0);

    // `shadow` keeps the drop shadow, the 1px system border and (on
    // Windows 11) the rounded corners of an undecorated window.
    #[cfg(target_os = "windows")]
    let builder = builder.decorations(false).shadow(true);

    // Vertically centres the traffic lights on the 40px tab strip.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 20.0));

    builder
}

/// Open another explorer window starting in `dir`, optionally with its
/// top-left corner at `at` (logical pixels).
fn spawn_window(app: &tauri::AppHandle, dir: &Path, at: Option<(f64, f64)>) -> CmdResult<()> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let label = format!("explorer-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let start = serde_json::to_string(dir).map_err(|e| e.to_string())?;
    let mut builder = explorer_window(app, label)
        .initialization_script(format!("window.__FERRIS_START_DIR__ = {start};"));
    if let Some((x, y)) = at {
        builder = builder.position(x, y);
    }
    builder.build().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn open_window(app: tauri::AppHandle, dir: PathBuf) -> CmdResult<()> {
    spawn_window(&app, &dir, None)
}

/// Where a tab dragged out of its window should go. `x`/`y` are the
/// pointer's screen position in physical pixels. Over another FerrisExplorer
/// window, that window adopts the tab ("merged"); elsewhere a new window
/// opens there ("opened"), unless `allow_new` is false ("none").
#[tauri::command]
pub async fn drop_tab(
    app: tauri::AppHandle,
    window: tauri::Window,
    dir: PathBuf,
    x: f64,
    y: f64,
    allow_new: bool,
) -> CmdResult<&'static str> {
    use tauri::{Emitter, Manager};
    for (label, target) in app.webview_windows() {
        if label == window.label() {
            continue;
        }
        let (Ok(pos), Ok(size)) = (target.outer_position(), target.outer_size()) else {
            continue;
        };
        let inside = x >= pos.x as f64
            && x < pos.x as f64 + size.width as f64
            && y >= pos.y as f64
            && y < pos.y as f64 + size.height as f64;
        if inside {
            app.emit_to(
                tauri::EventTarget::webview_window(&label),
                "tab-received",
                &dir,
            )
            .map_err(|e| e.to_string())?;
            let _ = target.set_focus();
            return Ok("merged");
        }
    }
    if !allow_new {
        return Ok("none");
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    // Put the new window's tab strip roughly under the pointer.
    spawn_window(&app, &dir, Some((x / scale - 120.0, y / scale - 18.0)))?;
    Ok("opened")
}

#[derive(serde::Serialize)]
pub struct SpaceTree {
    tree: crate::space::SpaceNode,
    from_index: bool,
}

#[tauri::command]
pub async fn delete_summary(paths: Vec<PathBuf>) -> CmdResult<crate::space::DeleteSummary> {
    blocking(move || {
        Ok(crate::space::delete_summary(
            &paths,
            std::time::Duration::from_millis(1500),
        ))
    })
    .await
}

// ── Git ───────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct GitStatus {
    root: PathBuf,
    branch: String,
    /// Path → one-letter badge (M, +, U, I, D).
    marks: HashMap<PathBuf, &'static str>,
    ahead: u32,
    behind: u32,
    /// Paths with changes in the index, for the commit dialog.
    staged: Vec<PathBuf>,
}

const GIT_IN_ARCHIVE: &str = "Git actions don't apply inside an archive, which is read-only";

#[tauri::command]
pub async fn git_status(dir: PathBuf) -> CmdResult<Option<GitStatus>> {
    blocking(move || {
        // A folder inside an archive is not part of the repository around the archive.
        if crate::archive::any_inside(std::slice::from_ref(&dir)) {
            return Ok(None);
        }
        Ok(crate::git::query(&dir).map(|info| GitStatus {
            root: info.root,
            branch: info.branch,
            ahead: info.ahead,
            behind: info.behind,
            staged: info.staged,
            marks: info
                .marks
                .into_iter()
                .map(|(p, m)| (p, m.badge()))
                .collect(),
        }))
    })
    .await
}

#[tauri::command]
pub async fn git_diff(path: PathBuf) -> CmdResult<()> {
    blocking(move || crate::git::open_diff(&path).map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn git_stage(paths: Vec<PathBuf>) -> CmdResult<()> {
    blocking(move || {
        if crate::archive::any_inside(&paths) {
            return Err(GIT_IN_ARCHIVE.into());
        }
        crate::git::stage(&paths)
    })
    .await
}

#[tauri::command]
pub async fn git_unstage(paths: Vec<PathBuf>) -> CmdResult<()> {
    blocking(move || {
        if crate::archive::any_inside(&paths) {
            return Err(GIT_IN_ARCHIVE.into());
        }
        crate::git::unstage(&paths)
    })
    .await
}

/// Revert files to their index version. Each file is copied to the Trash first.
#[tauri::command]
pub async fn git_discard(paths: Vec<PathBuf>) -> CmdResult<crate::git::Discarded> {
    blocking(move || {
        if crate::archive::any_inside(&paths) {
            return Err(GIT_IN_ARCHIVE.into());
        }
        Ok(crate::git::discard(&recycle::OsTrasher, &paths))
    })
    .await
}

/// Commit the index with `message`. This runs the repository's commit hooks. It never pushes or amends.
#[tauri::command]
pub async fn git_commit(dir: PathBuf, message: String) -> CmdResult<String> {
    blocking(move || {
        if crate::archive::any_inside(std::slice::from_ref(&dir)) {
            return Err(GIT_IN_ARCHIVE.into());
        }
        crate::git::commit(&dir, &message)
    })
    .await
}

/// Which of `paths` git ignores. One call per listing, not one per file.
#[tauri::command]
pub async fn git_ignored(dir: PathBuf, paths: Vec<PathBuf>) -> CmdResult<Vec<PathBuf>> {
    blocking(move || {
        if crate::archive::any_inside(std::slice::from_ref(&dir)) {
            return Ok(Vec::new());
        }
        crate::git::ignored_among(&dir, &paths)
    })
    .await
}

#[tauri::command]
pub async fn git_blame(path: PathBuf) -> CmdResult<crate::git::Blame> {
    blocking(move || {
        if crate::archive::any_inside(std::slice::from_ref(&path)) {
            return Err(GIT_IN_ARCHIVE.into());
        }
        crate::git::blame(&path)
    })
    .await
}

// ── Preview ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn preview(path: PathBuf) -> CmdResult<crate::preview::Preview> {
    blocking(move || match crate::archive::locate(&path) {
        Some(loc) if !loc.inner.is_empty() => Ok(crate::archive::preview(&loc)),
        _ => Ok(crate::preview::decode_preview(&path)),
    })
    .await
}

#[tauri::command]
pub async fn content_matches(
    path: PathBuf,
    needle: String,
) -> CmdResult<crate::preview::ContentMatches> {
    blocking(move || crate::preview::content_matches(&path, &needle)).await
}

// ── Launching ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn open_path(path: PathBuf) -> CmdResult<()> {
    blocking(move || crate::desktop::launch_file(&path)).await
}

#[tauri::command]
pub async fn reveal_path(path: PathBuf) -> CmdResult<()> {
    blocking(move || crate::sys::reveal_in_finder(&path).map_err(|e| e.to_string())).await
}

/// Opens the OS log folder, creating it first so the open never fails on a
/// fresh install where the logger has not written yet.
#[tauri::command]
pub async fn open_log_dir(app: tauri::AppHandle) -> CmdResult<()> {
    use tauri::Manager;
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    blocking(move || {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        crate::desktop::launch_file(&dir)
    })
    .await
}

/// Cap on each frontend-reported string, so a runaway error loop cannot grow
/// the log without bound.
const FRONTEND_LOG_CAP: usize = 8 * 1024;

/// Truncates to at most `max` bytes, backing up to a char boundary.
fn clip_utf8(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Records an error the webview hit (uncaught exception, rejected promise,
/// render crash). Fire-and-forget: it never fails and never panics.
#[tauri::command]
pub fn log_frontend_error(message: String, stack: Option<String>) {
    let message = clip_utf8(message, FRONTEND_LOG_CAP);
    match stack {
        Some(stack) => {
            let stack = clip_utf8(stack, FRONTEND_LOG_CAP);
            log::error!(target: "frontend", "{message}\n{stack}");
        }
        None => log::error!(target: "frontend", "{message}"),
    }
}

#[tauri::command]
pub async fn open_terminal(dir: PathBuf) -> CmdResult<()> {
    blocking(move || {
        ops::open_terminal(&dir);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn open_in_editor(path: PathBuf) -> CmdResult<()> {
    blocking(move || {
        ops::open_in_editor(&path);
        Ok(())
    })
    .await
}

// ── File operations ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn rename_path(path: PathBuf, new_name: String) -> CmdResult<PathBuf> {
    blocking(move || {
        crate::archive::ensure_writable(&[&path])?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let dest = ops::safe_child(parent, &new_name)?;
        crate::desktop::rename_without_clobbering(&path, &dest)?;
        Ok(dest)
    })
    .await
}

/// Rename several items as one batch. Each request is `(item, new name)`; the
/// name is checked and placed in the item's own folder. Cycles and swaps are
/// fine, nothing is overwritten, and a failure rolls the batch back.
#[tauri::command]
pub async fn rename_batch(
    renames: Vec<(PathBuf, String)>,
) -> CmdResult<crate::bulk_rename::RenameOutcome> {
    let sources: Vec<PathBuf> = renames.iter().map(|(path, _)| path.clone()).collect();
    crate::archive::ensure_writable(&sources)?;
    blocking(move || {
        let pairs = crate::bulk_rename::targets(&renames)?;
        crate::bulk_rename::run(&pairs)
    })
    .await
}

#[tauri::command]
pub async fn create_folder(dir: PathBuf, name: String) -> CmdResult<PathBuf> {
    blocking(move || {
        crate::archive::ensure_writable(&[&dir])?;
        let dest = ops::safe_child(&dir, &name)?;
        std::fs::create_dir(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        Ok(dest)
    })
    .await
}

/// Whether a folder has no entries at all. Reads the folder directly rather
/// than through `list_dir`, which skips any entry whose metadata cannot be read.
#[tauri::command]
pub async fn dir_is_empty(path: PathBuf) -> CmdResult<bool> {
    blocking(move || {
        let mut entries =
            std::fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(entries.next().is_none())
    })
    .await
}

/// Drop removed paths from the search index so they stop showing up in search.
fn forget_in_index(paths: &[PathBuf]) {
    let db = IndexStatus::db_path();
    if db.exists() {
        for p in paths {
            let _ = crate::db::DbManager::delete_from_index(&db, p);
        }
    }
}

/// Move to the OS trash. Undo puts the items back with `restore_from_trash`.
#[tauri::command]
pub async fn trash_paths(paths: Vec<PathBuf>) -> CmdResult<recycle::Removed> {
    crate::archive::ensure_writable(&paths)?;
    blocking(move || {
        let removed = recycle::delete_items(&recycle::OsTrasher, recycle::Mode::Trash, &paths);
        forget_in_index(&removed.done);
        Ok(removed)
    })
    .await
}

/// Permanently delete, with no way back. Returns the paths that could not be removed.
#[tauri::command]
pub async fn delete_permanently(paths: Vec<PathBuf>) -> CmdResult<Vec<(PathBuf, String)>> {
    crate::archive::ensure_writable(&paths)?;
    blocking(move || {
        let removed = recycle::delete_items(&recycle::OsTrasher, recycle::Mode::Permanent, &paths);
        forget_in_index(&removed.done);
        Ok(removed.failures)
    })
    .await
}

/// Undo a trash: put each item back where it was. Refuses if one of those
/// places is occupied again.
#[tauri::command]
pub async fn restore_from_trash(paths: Vec<PathBuf>) -> CmdResult<recycle::RestoreOutcome> {
    blocking(move || Ok(recycle::restore_items(&recycle::OsTrasher, &paths))).await
}

/// Undo a sync's overwrites and writes in one call: trash the written items still as
/// the sync left them, then put each overwritten old version back at its real path.
#[tauri::command]
pub async fn trash_unchanged_created(
    items: Vec<crate::compare::CreatedItem>,
    set_aside: Vec<crate::compare::SetAside>,
) -> CmdResult<crate::compare::WrittenUndo> {
    blocking(move || {
        Ok(crate::compare::undo_sync(
            &recycle::OsTrasher,
            &items,
            &set_aside,
        ))
    })
    .await
}

/// Undo or redo a move: each `(from, to)` pair is moved, refusing if any `to` exists.
/// Reports the pairs that moved, so the caller can keep the rest.
#[tauri::command]
pub async fn move_exact(pairs: Vec<(PathBuf, PathBuf)>) -> CmdResult<ops::MoveOutcome> {
    let both: Vec<PathBuf> = pairs
        .iter()
        .flat_map(|(a, b)| [a.clone(), b.clone()])
        .collect();
    crate::archive::ensure_writable(&both)?;
    blocking(move || Ok(ops::move_exact(&pairs))).await
}

/// Copy or move `sources` into `dest_dir`. Progress arrives as `op-progress`.
/// Items inside an archive are extracted into `dest_dir`; they can only be copied.
#[tauri::command]
pub fn transfer_paths(
    state: State<'_, AppState>,
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    cut: bool,
) -> CmdResult<u64> {
    if !dest_dir.is_dir() {
        return Err(format!("{} is not a folder", dest_dir.display()));
    }
    let id = ops::next_id();
    let cancel = Arc::new(AtomicBool::new(false));
    if let Some((archive, selections)) = crate::archive::copy_out_request(&sources)? {
        if cut {
            return Err("items inside an archive can only be copied, not moved".into());
        }
        state
            .op_cancels
            .lock()
            .map_err(|e| e.to_string())?
            .insert(id, cancel.clone());
        crate::archive::spawn_extract(
            id,
            OpKind::Copy,
            archive,
            selections,
            dest_dir,
            cancel,
            state.op_tx.clone(),
        );
        return Ok(id);
    }
    crate::archive::ensure_writable(&[&dest_dir])?;
    state
        .op_cancels
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, cancel.clone());
    let (decide_tx, decide_rx) = std::sync::mpsc::channel();
    state
        .op_decisions
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, decide_tx);
    ops::spawn_copy(
        id,
        sources,
        dest_dir,
        cut,
        cancel,
        state.op_tx.clone(),
        Some(decide_rx),
    );
    Ok(id)
}

/// Where items dropped from another app land. `hovered` is the folder row
/// under the pointer, if any; `pane_dir` is the folder the pane shows.
#[tauri::command]
pub async fn resolve_drop(
    sources: Vec<PathBuf>,
    hovered: Option<PathBuf>,
    pane_dir: PathBuf,
) -> CmdResult<ops::DropPlan> {
    blocking(move || {
        let mut targets = vec![pane_dir.clone()];
        targets.extend(hovered.clone());
        crate::archive::ensure_writable(&targets)?;
        ops::plan_drop(&sources, hovered.as_deref(), &pane_dir)
    })
    .await
}

/// Extract a whole archive into `dest_dir`, as a folder named after the archive.
#[tauri::command]
pub fn extract_archive(
    state: State<'_, AppState>,
    archive: PathBuf,
    dest_dir: PathBuf,
) -> CmdResult<u64> {
    let selection = crate::archive::whole_archive(&archive)?;
    if !dest_dir.is_dir() {
        return Err(format!("{} is not a folder", dest_dir.display()));
    }
    let id = ops::next_id();
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .op_cancels
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, cancel.clone());
    crate::archive::spawn_extract(
        id,
        OpKind::Extract,
        archive,
        vec![selection],
        dest_dir,
        cancel,
        state.op_tx.clone(),
    );
    Ok(id)
}

/// Drag `paths` out of the window into another app (Finder, Explorer, a mail
/// composer). The other app receives the real files and they are copied.
#[tauri::command]
pub async fn start_drag(window: tauri::WebviewWindow, paths: Vec<PathBuf>) -> CmdResult<()> {
    if crate::archive::any_inside(&paths) {
        return Err(
            "dragging items out of an archive isn't supported yet; copy them instead".into(),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (window, paths);
        Err("dragging files out of FerrisExplorer is not supported on Linux yet".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if paths.is_empty() {
            return Err("nothing to drag".into());
        }
        // The native drag must start on the main thread, and it is reported
        // back here once it has been handed to the system.
        let (tx, rx) = std::sync::mpsc::channel();
        let on_main = window.clone();
        window
            .run_on_main_thread(move || {
                // The preview is the app icon: the drag crate needs an image
                // it can decode, and a folder has no image of its own.
                let image = drag::Image::Raw(include_bytes!("../icons/32x32.png").to_vec());
                let result = drag::start_drag(
                    &on_main,
                    drag::DragItem::Files(paths),
                    image,
                    |_, _| {},
                    drag::Options::default(),
                )
                .map_err(|e| e.to_string());
                let _ = tx.send(result);
            })
            .map_err(|e| e.to_string())?;
        blocking(move || rx.recv().map_err(|e| e.to_string())?).await
    }
}

/// Answer a paused transfer's name clash.
#[tauri::command]
pub fn resolve_conflict(
    state: State<'_, AppState>,
    id: u64,
    resolution: ops::Resolution,
    apply_to_all: bool,
) -> CmdResult<()> {
    let map = state.op_decisions.lock().map_err(|e| e.to_string())?;
    let tx = map
        .get(&id)
        .ok_or_else(|| "that operation has already finished".to_string())?;
    tx.send(ops::Decision {
        resolution,
        apply_to_all,
    })
    .map_err(|_| "that operation has already finished".to_string())
}

#[tauri::command]
pub fn cancel_op(state: State<'_, AppState>, id: u64) {
    if let Ok(map) = state.op_cancels.lock() {
        if let Some(flag) = map.get(&id) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn finished(
    id: u64,
    kind: OpKind,
    ok_msg: &str,
    fail_msg: &str,
    n: u64,
    err: Option<String>,
    outcomes: Vec<(PathBuf, PathBuf)>,
) -> OpProgress {
    OpProgress {
        id,
        kind,
        message: if err.is_some() { fail_msg } else { ok_msg }.into(),
        bytes_done: 0,
        bytes_total: 0,
        files_done: n,
        files_total: n,
        error: err,
        done: true,
        bytes_per_sec: 0,
        skipped: 0,
        conflict: None,
        outcomes,
    }
}

#[tauri::command]
pub fn duplicate_paths(state: State<'_, AppState>, sources: Vec<PathBuf>) -> CmdResult<u64> {
    crate::archive::ensure_writable(&sources)?;
    let id = ops::next_id();
    // Registered under the op id so `cancel_op` can reach it. The pump drops the
    // entry when the final progress message arrives.
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .op_cancels
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, cancel.clone());
    let tx = state.op_tx.clone();
    std::thread::spawn(move || {
        let mut first_err = None;
        let mut outcomes = Vec::new();
        for src in &sources {
            let dest = ops::unique_sibling(src);
            // A failed copy removes what it made, so no partial copy is left.
            if let Err(e) = ops::copy_recursive(src, &dest, &cancel, &mut |_| {}) {
                first_err = Some(e);
                break;
            }
            outcomes.push((src.clone(), dest));
        }
        let n = sources.len() as u64;
        let _ = tx.send(finished(
            id,
            OpKind::Duplicate,
            "Duplicated",
            "Duplicate failed",
            n,
            first_err,
            outcomes,
        ));
    });
    Ok(id)
}

#[tauri::command]
pub fn compress_path(state: State<'_, AppState>, path: PathBuf) -> CmdResult<u64> {
    crate::archive::ensure_writable(&[&path])?;
    let id = ops::next_id();
    // Registered under the op id so `cancel_op` can stop the archiver.
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .op_cancels
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, cancel.clone());
    let tx = state.op_tx.clone();
    std::thread::spawn(move || {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".into());
        let wanted = parent.join(format!("{stem}.zip"));
        let dest = if ops::path_occupied(&wanted) {
            ops::unique_sibling(&wanted)
        } else {
            wanted
        };
        let res = ops::compress(&path, &dest, &cancel);
        // `dest` was chosen to be free, so a partial archive there is ours.
        if res.is_err() {
            let _ = ops::delete_path(&dest);
        }
        let _ = tx.send(finished(
            id,
            OpKind::Compress,
            "Compressed",
            "Compress failed",
            1,
            res.err(),
            Vec::new(),
        ));
    });
    Ok(id)
}

// ── Settings ──────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct Settings {
    favorites: Vec<PathBuf>,
    saved_searches: Vec<SavedSearch>,
    history: Vec<String>,
}

#[tauri::command]
pub async fn load_settings() -> CmdResult<Settings> {
    blocking(|| {
        Ok(Settings {
            favorites: FavoritesStore::load(),
            saved_searches: SavedSearchesStore::load(),
            history: SearchHistoryStore::load(),
        })
    })
    .await
}

#[tauri::command]
pub async fn save_favorites(favorites: Vec<PathBuf>) -> CmdResult<()> {
    blocking(move || {
        FavoritesStore::save(&favorites);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn save_searches(searches: Vec<SavedSearch>) -> CmdResult<()> {
    blocking(move || {
        SavedSearchesStore::save(&searches);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn push_history(query: String) -> CmdResult<Vec<String>> {
    blocking(move || {
        let mut history = SearchHistoryStore::load();
        SearchHistoryStore::push(&mut history, query);
        Ok(history)
    })
    .await
}

// ── Run a command on the selection ────────────────────────────────────────

/// The exact command lines a run would execute, for the preview.
#[tauri::command]
pub async fn plan_shell(
    template: String,
    paths: Vec<PathBuf>,
    dir: PathBuf,
) -> CmdResult<Vec<String>> {
    blocking(move || crate::shell::checked_plan(&template, &paths, &dir)).await
}

/// Start a run. The client picks `run_id`, so it can match events that arrive
/// before this returns. Returns the command lines that will run.
#[tauri::command]
pub fn start_shell(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    run_id: u64,
    template: String,
    paths: Vec<PathBuf>,
    dir: PathBuf,
    stop_on_error: bool,
) -> CmdResult<Vec<String>> {
    crate::shell::start(app, &state, run_id, &template, &paths, &dir, stop_on_error)
}

/// Stop the running command, if there is one.
#[tauri::command]
pub fn cancel_shell(state: State<'_, AppState>) {
    crate::shell::cancel(&state);
}

#[tauri::command]
pub async fn load_shell_history() -> CmdResult<Vec<String>> {
    blocking(|| Ok(crate::persistence::ShellHistoryStore::load())).await
}

#[tauri::command]
pub async fn push_shell_history(command: String) -> CmdResult<Vec<String>> {
    blocking(move || {
        let mut history = crate::persistence::ShellHistoryStore::load();
        crate::persistence::ShellHistoryStore::push(&mut history, command);
        Ok(history)
    })
    .await
}

// ── Keyboard layer: folder visits, jump list, key preferences ─────────────

#[tauri::command]
pub async fn record_visit(path: String) -> CmdResult<()> {
    blocking(move || {
        crate::persistence::FrecencyStore::record(&path, crate::persistence::now_secs());
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn jump_folders(query: String) -> CmdResult<Vec<crate::persistence::JumpHit>> {
    blocking(move || {
        let entries = crate::persistence::FrecencyStore::load();
        Ok(crate::persistence::rank_jumps(
            &entries,
            &query,
            crate::persistence::now_secs(),
        ))
    })
    .await
}

#[tauri::command]
pub async fn load_key_prefs() -> CmdResult<crate::persistence::KeyPrefs> {
    blocking(|| Ok(crate::persistence::KeyPrefsStore::load())).await
}

#[tauri::command]
pub async fn save_key_prefs(prefs: crate::persistence::KeyPrefs) -> CmdResult<()> {
    blocking(move || {
        crate::persistence::KeyPrefsStore::save(&prefs);
        Ok(())
    })
    .await
}

// ── Workspaces ────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct WorkspaceList {
    restore_on_launch: bool,
    workspaces: Vec<crate::persistence::Workspace>,
}

#[tauri::command]
pub async fn load_workspaces() -> CmdResult<WorkspaceList> {
    blocking(|| {
        let file = crate::persistence::WorkspacesStore::load();
        Ok(WorkspaceList {
            restore_on_launch: file.restore_on_launch,
            workspaces: crate::persistence::listed(&file.workspaces),
        })
    })
    .await
}

#[tauri::command]
pub async fn load_last_session() -> CmdResult<Option<crate::persistence::Workspace>> {
    blocking(|| {
        let file = crate::persistence::WorkspacesStore::load();
        Ok(crate::persistence::last_session(&file.workspaces))
    })
    .await
}

/// Save a named workspace. Returns true when it replaced one of the same name.
#[tauri::command]
pub async fn save_workspace(mut workspace: crate::persistence::Workspace) -> CmdResult<bool> {
    blocking(move || {
        workspace.name = crate::persistence::checked_name(&workspace.name)?;
        crate::persistence::WorkspacesStore::modify(|f| {
            crate::persistence::upsert(&mut f.workspaces, workspace)
        })
        .map_err(|e| e.to_string())
    })
    .await
}

/// Autosave the current layout as the hidden last session.
#[tauri::command]
pub async fn save_last_session(mut workspace: crate::persistence::Workspace) -> CmdResult<()> {
    blocking(move || {
        workspace.name = crate::persistence::LAST_SESSION.to_string();
        crate::persistence::WorkspacesStore::modify(|f| {
            crate::persistence::upsert(&mut f.workspaces, workspace)
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .await
}

/// Delete a named workspace. Returns true when one was removed.
#[tauri::command]
pub async fn delete_workspace(name: String) -> CmdResult<bool> {
    blocking(move || {
        crate::persistence::WorkspacesStore::modify(|f| {
            crate::persistence::remove(&mut f.workspaces, &name)
        })
        .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
pub async fn set_restore_on_launch(on: bool) -> CmdResult<()> {
    blocking(move || {
        crate::persistence::WorkspacesStore::modify(|f| f.restore_on_launch = on)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await
}

/// For each saved folder, the folder to open instead when it no longer exists.
/// Falls back to the home folder when nothing up the path exists.
#[tauri::command]
pub async fn resolve_restore_dirs(paths: Vec<PathBuf>) -> CmdResult<Vec<PathBuf>> {
    blocking(move || {
        let home = crate::sys::get_home_dir();
        Ok(paths
            .iter()
            .map(|p| crate::persistence::nearest_existing_dir(p).unwrap_or_else(|| home.clone()))
            .collect())
    })
    .await
}

// ── Plugins ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn load_plugins() -> CmdResult<crate::plugins::PluginReport> {
    blocking(|| {
        let trust = crate::persistence::PluginTrustStore::load();
        Ok(crate::plugins::report(
            &crate::plugins::plugin_dir(),
            &trust,
        ))
    })
    .await
}

/// Enable or disable a plugin file. `hash` is the one the user was shown.
#[tauri::command]
pub async fn set_plugin_enabled(
    file: String,
    hash: String,
    enabled: bool,
) -> CmdResult<crate::plugins::PluginReport> {
    blocking(move || crate::plugins::set_enabled(&file, &hash, enabled)).await
}

/// Plan an action of an enabled plugin on the selection, without running it.
#[tauri::command]
pub async fn plan_plugin_action(
    file: String,
    action: String,
    paths: Vec<PathBuf>,
    dir: PathBuf,
) -> CmdResult<crate::plugins::ActionPlan> {
    blocking(move || crate::plugins::plan_action(&file, &action, &paths, &dir)).await
}

/// Which plugin action to run: its file, the hash the user was shown, and its id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRunRef {
    file: String,
    hash: String,
    action_id: String,
}

/// Run a confirmed plugin action. The plugin is checked again here, against
/// `hash`, and the run is not added to shell history. Returns the command lines.
#[tauri::command]
pub fn run_plugin_action(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    run_id: u64,
    plugin: PluginRunRef,
    paths: Vec<PathBuf>,
    dir: PathBuf,
) -> CmdResult<Vec<String>> {
    let plan =
        crate::plugins::plan_run(&plugin.file, &plugin.hash, &plugin.action_id, &paths, &dir)?;
    crate::shell::start_planned(app, &state, run_id, plan.lines, &dir, false)
}

/// Run the enabled previewer for `path`, if one matches. `request` lets the UI cancel it.
#[tauri::command]
pub async fn plugin_preview(
    path: PathBuf,
    request: u64,
) -> CmdResult<Option<crate::plugins::PluginPreview>> {
    blocking(move || crate::plugins::preview(&path, request)).await
}

#[tauri::command]
pub fn cancel_plugin_preview(request: u64) {
    crate::plugins::cancel_preview(request);
}

// ── Updates ───────────────────────────────────────────────────────────────

/// A newer release on the update endpoint. Shown to the user before anything installs.
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
    date: Option<String>,
}

fn updater(app: &tauri::AppHandle) -> CmdResult<tauri_plugin_updater::Updater> {
    use tauri_plugin_updater::UpdaterExt;
    app.updater().map_err(|e| e.to_string())
}

/// Only the main window may check or install updates.
fn require_main_window(window: &tauri::WebviewWindow) -> CmdResult<()> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Updates are only available from the main window".to_string())
    }
}

/// Ask the update endpoint whether a newer release exists. Never installs.
#[tauri::command]
pub async fn check_for_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> CmdResult<Option<UpdateInfo>> {
    require_main_window(&window)?;
    let update = updater(&app)?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateInfo {
        version: u.version,
        current_version: u.current_version,
        notes: u.body,
        date: u.date.map(|d| d.to_string()),
    }))
}

/// Download, verify and install the latest release, then restart. The plugin checks
/// the signature against the pubkey in tauri.conf.json; nothing here skips that check.
/// `expected_version` is the release the user was shown; a different one is not installed.
#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    expected_version: String,
) -> CmdResult<()> {
    require_main_window(&window)?;
    let update = updater(&app)?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "No update available".to_string())?;
    if update.version != expected_version {
        return Err(format!(
            "A different update ({}) is now available; check again",
            update.version
        ));
    }
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::clip_utf8;

    #[test]
    fn clip_utf8_keeps_short_strings_whole() {
        assert_eq!(clip_utf8("boom".into(), 8), "boom");
        assert_eq!(clip_utf8("boom".into(), 4), "boom");
    }

    #[test]
    fn clip_utf8_cuts_at_the_limit() {
        assert_eq!(clip_utf8("abcdef".into(), 3), "abc");
    }

    #[test]
    fn clip_utf8_backs_up_to_a_char_boundary() {
        // "é" is two bytes, so a cut at byte 1 must back up to 0.
        assert_eq!(clip_utf8("é".into(), 1), "");
        // Three-byte char "€" starting at byte 2; cutting at 3 backs up to 2.
        assert_eq!(clip_utf8("ab€c".into(), 3), "ab");
    }
}
