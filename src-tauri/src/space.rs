//! Space usage: folder sizes as a tree, for the treemap view.
//!
//! The index already records every folder's parent and the bytes of the files
//! directly inside it, so whole-subtree sizes come from one table scan plus a
//! bottom-up sum: no disk walk. The result is cached so drilling in and out
//! is instant. Without an index the view falls back to scanning one level.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::db::DbManager;

/// How long a loaded index snapshot is reused before re-reading SQLite.
pub const CACHE_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, serde::Serialize)]
pub struct SpaceNode {
    pub name: String,
    pub path: PathBuf,
    /// Bytes in this folder and everything beneath it.
    pub size: u64,
    /// Bytes of the files directly inside this folder.
    pub direct: u64,
    pub children: Vec<SpaceNode>,
    /// Folders folded into one "N others" tile.
    pub other_count: u64,
    pub other_size: u64,
}

struct DirNode {
    path: PathBuf,
    name: String,
    direct: u64,
    total: u64,
    children: Vec<usize>,
}

pub struct SpaceIndex {
    nodes: Vec<DirNode>,
    by_key: HashMap<String, usize>,
    pub loaded_at: Instant,
}

/// Comparison key for a path: separators unified, trailing separator
/// dropped (except on a root), and case-folded on Windows.
pub fn path_key(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.replace('\\', "/")
    }
}

impl SpaceIndex {
    pub fn load(db: &Path) -> Result<Self, String> {
        let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id, parent_id, name, path, direct_size FROM directories")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        let mut nodes = Vec::new();
        let mut parents = Vec::new();
        let mut by_id = HashMap::new();
        for row in rows.flatten() {
            let (id, parent, name, path, direct) = row;
            by_id.insert(id, nodes.len());
            parents.push(parent);
            let direct = direct.max(0) as u64;
            nodes.push(DirNode {
                path: PathBuf::from(path),
                name,
                direct,
                total: direct,
                children: Vec::new(),
            });
        }
        Ok(Self::assemble(nodes, &parents, &by_id))
    }

    fn assemble(
        mut nodes: Vec<DirNode>,
        parents: &[Option<i64>],
        by_id: &HashMap<i64, usize>,
    ) -> Self {
        let parent_idx: Vec<Option<usize>> = parents
            .iter()
            .map(|p| p.and_then(|id| by_id.get(&id).copied()))
            .collect();
        for (i, p) in parent_idx.iter().enumerate() {
            if let Some(p) = *p {
                nodes[p].children.push(i);
            }
        }
        // Post-order without recursion: a DFS order, then sum in reverse.
        let mut order = Vec::with_capacity(nodes.len());
        let mut stack: Vec<usize> = (0..nodes.len())
            .filter(|&i| parent_idx[i].is_none())
            .collect();
        while let Some(i) = stack.pop() {
            order.push(i);
            stack.extend(nodes[i].children.iter().copied());
        }
        for &i in order.iter().rev() {
            if let Some(p) = parent_idx[i] {
                nodes[p].total = nodes[p].total.saturating_add(nodes[i].total);
            }
        }
        let by_key = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (path_key(&n.path), i))
            .collect();
        Self {
            nodes,
            by_key,
            loaded_at: Instant::now(),
        }
    }

    /// Whole-subtree size of one folder, if the index covers it.
    pub fn size_of(&self, path: &Path) -> Option<u64> {
        self.by_key
            .get(&path_key(path))
            .map(|&i| self.nodes[i].total)
    }

    /// Whole-subtree size of every folder at least `min` bytes, largest first,
    /// at most `max` of them.
    pub fn folder_sizes(&self, min: u64, max: usize) -> Vec<(String, u64)> {
        let mut sizes: Vec<(String, u64)> = self
            .nodes
            .iter()
            .filter(|n| n.total > 0 && n.total >= min)
            .map(|n| (n.path.to_string_lossy().into_owned(), n.total))
            .collect();
        sizes.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
        sizes.truncate(max);
        sizes
    }

    pub fn tree(&self, root: &Path, depth: u8, max_children: usize) -> Option<SpaceNode> {
        let idx = *self.by_key.get(&path_key(root))?;
        Some(self.build(idx, depth, max_children))
    }

    fn build(&self, idx: usize, depth: u8, max_children: usize) -> SpaceNode {
        let n = &self.nodes[idx];
        let mut kids: Vec<usize> = n.children.clone();
        kids.sort_by_key(|&c| std::cmp::Reverse(self.nodes[c].total));
        let mut children = Vec::new();
        let (mut other_count, mut other_size) = (0u64, 0u64);
        for (rank, &c) in kids.iter().enumerate() {
            if self.nodes[c].total == 0 {
                continue;
            }
            if depth == 0 || rank >= max_children {
                other_count += 1;
                other_size = other_size.saturating_add(self.nodes[c].total);
            } else {
                children.push(self.build(c, depth - 1, (max_children / 3).max(4)));
            }
        }
        SpaceNode {
            name: if n.name.is_empty() {
                n.path.to_string_lossy().into_owned()
            } else {
                n.name.clone()
            },
            path: n.path.clone(),
            size: n.total,
            direct: n.direct,
            children,
            other_count,
            other_size,
        }
    }
}

pub type SpaceCache = std::sync::Mutex<Option<std::sync::Arc<SpaceIndex>>>;

/// The cached index snapshot, re-read when missing, older than
/// [`CACHE_TTL`], or when `refresh` is set.
pub fn cached(
    cache: &SpaceCache,
    db: &Path,
    refresh: bool,
) -> Result<std::sync::Arc<SpaceIndex>, String> {
    let mut guard = cache.lock().map_err(|e| e.to_string())?;
    let stale = guard
        .as_ref()
        .is_none_or(|c| refresh || c.loaded_at.elapsed() > CACHE_TTL);
    if stale {
        *guard = Some(std::sync::Arc::new(SpaceIndex::load(db)?));
    }
    Ok(guard.as_ref().expect("just filled").clone())
}

/// One level of sizes straight from the disk, for when there is no index.
pub fn scan(root: &Path, max_children: usize, cancel: &AtomicBool) -> SpaceNode {
    let scanned = crate::analyzer::StorageAnalyzer::scan_directory(root, cancel);
    let mut direct = 0u64;
    let mut dirs: Vec<SpaceNode> = Vec::new();
    for c in scanned.children.unwrap_or_default() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if c.is_dir {
            dirs.push(SpaceNode {
                name: c.name,
                path: c.path,
                size: c.size,
                direct: 0,
                children: Vec::new(),
                other_count: 0,
                other_size: 0,
            });
        } else {
            direct = direct.saturating_add(c.size);
        }
    }
    dirs.retain(|d| d.size > 0);
    dirs.sort_by_key(|d| std::cmp::Reverse(d.size));
    let rest: Vec<SpaceNode> = dirs.split_off(dirs.len().min(max_children));
    SpaceNode {
        name: scanned.name,
        path: scanned.path,
        size: scanned.size,
        direct,
        children: dirs,
        other_count: rest.len() as u64,
        other_size: rest.iter().map(|d| d.size).sum(),
    }
}

// ── Size history ──────────────────────────────────────────────────────────

/// Folders smaller than this are not recorded in a snapshot.
pub const HISTORY_MIN_BYTES: u64 = 50 * 1024 * 1024;
/// A snapshot keeps at most this many folders, largest first.
const HISTORY_MAX_DIRS: usize = 5000;
/// Changes below this are noise and left out of the report.
const HISTORY_MIN_DELTA: u64 = 1024 * 1024;
/// Rows per list in the report.
const HISTORY_LIMIT: usize = 50;

const DAY_SECS: i64 = 86_400;
const WEEK_SECS: i64 = 7 * DAY_SECS;
/// Timer-driven snapshots are at least this far apart.
const SNAPSHOT_EVERY: i64 = DAY_SECS;
/// Snapshots are kept in full for this long, then one a week.
const KEEP_ALL_SECS: i64 = 30 * DAY_SECS;
/// Snapshots older than this are deleted.
const MAX_AGE_SECS: i64 = 90 * DAY_SECS;

/// How far back the Changes view compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    pub fn secs(self) -> i64 {
        match self {
            Period::Day => DAY_SECS,
            Period::Week => WEEK_SECS,
            Period::Month => 30 * DAY_SECS,
        }
    }
}

/// One folder's size then and now. `delta` is `new_size - old_size`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Change {
    pub path: String,
    pub old_size: u64,
    pub new_size: u64,
    pub delta: i64,
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Diff {
    pub grown: Vec<Change>,
    pub shrunk: Vec<Change>,
    pub new: Vec<Change>,
    pub deleted: Vec<Change>,
}

/// What changed over one period, for the Changes view.
#[derive(Debug, serde::Serialize)]
pub struct Changes {
    pub period: Period,
    /// The snapshot the live sizes were compared with. None when no snapshot
    /// is old enough yet.
    pub baseline_at: Option<i64>,
    /// The oldest snapshot on record.
    pub first_snapshot_at: Option<i64>,
    pub now: i64,
    pub diff: Diff,
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compare folder sizes recorded in a snapshot (`old`) with live sizes (`new`).
/// A folder in `old` with no live size is deleted. A folder in `new` above the
/// threshold that `old` never recorded is new.
pub fn diff_sizes(old: &[(String, u64)], new: &[(String, u64)], limit: usize) -> Diff {
    let old_sizes: HashMap<&str, u64> = old.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let new_sizes: HashMap<&str, u64> = new.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let mut d = Diff::default();
    for (path, was) in old {
        let live = new_sizes.get(path.as_str()).copied().unwrap_or(0);
        let change = Change {
            path: path.clone(),
            old_size: *was,
            new_size: live,
            delta: live as i64 - *was as i64,
        };
        if live == 0 {
            d.deleted.push(change);
        } else if change.delta.unsigned_abs() >= HISTORY_MIN_DELTA {
            if change.delta > 0 {
                d.grown.push(change);
            } else {
                d.shrunk.push(change);
            }
        }
    }
    for (path, size) in new {
        if *size >= HISTORY_MIN_BYTES && !old_sizes.contains_key(path.as_str()) {
            d.new.push(Change {
                path: path.clone(),
                old_size: 0,
                new_size: *size,
                delta: *size as i64,
            });
        }
    }
    d.grown.sort_by_key(|c| std::cmp::Reverse(c.delta));
    d.shrunk.sort_by_key(|c| c.delta);
    d.new.sort_by_key(|c| std::cmp::Reverse(c.new_size));
    d.deleted.sort_by_key(|c| std::cmp::Reverse(c.old_size));
    d.grown.truncate(limit);
    d.shrunk.truncate(limit);
    d.new.truncate(limit);
    d.deleted.truncate(limit);
    d
}

/// Where snapshots wait while a full rebuild replaces the index file, since
/// the rebuild deletes the index and everything in it.
fn history_sidecar(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push(".history");
    PathBuf::from(p)
}

/// Copy snapshots from `from` into `to`, skipping any `to` already has.
fn copy_snapshots(from: &Connection, to: &mut Connection) -> Result<(), String> {
    for (id, taken_at) in DbManager::size_snapshot_ids(from).map_err(|e| e.to_string())? {
        let present: bool = to
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM size_snapshots WHERE taken_at = ?1)",
                [taken_at],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if present {
            continue;
        }
        let rows = DbManager::size_snapshot_rows(from, id).map_err(|e| e.to_string())?;
        DbManager::insert_size_snapshot(to, taken_at, &rows).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Open the index for history work. Snapshots parked by [`stash_history`] are
/// moved back in and the sidecar removed.
fn open_history(db: &Path) -> Result<Connection, String> {
    let mut conn = DbManager::open_history(db).map_err(|e| e.to_string())?;
    let side = history_sidecar(db);
    if side.exists() {
        {
            let parked = DbManager::open_history(&side).map_err(|e| e.to_string())?;
            copy_snapshots(&parked, &mut conn)?;
        }
        crate::db::remove_index_files(&side).map_err(|e| e.to_string())?;
    }
    Ok(conn)
}

/// Before a full rebuild deletes the index, copy its snapshots to a sidecar so
/// the history survives the rebuild.
pub fn stash_history(db: &Path) -> Result<(), String> {
    if !db.exists() {
        return Ok(());
    }
    let from = DbManager::open_history(db).map_err(|e| e.to_string())?;
    let mut to = DbManager::open_history(&history_sidecar(db)).map_err(|e| e.to_string())?;
    copy_snapshots(&from, &mut to)
}

/// True when the index has no snapshot from the last day.
pub fn snapshot_due(db: &Path, now: i64) -> bool {
    if !db.exists() {
        return false;
    }
    let newest = open_history(db)
        .ok()
        .and_then(|conn| DbManager::size_snapshot_bounds(&conn).ok().flatten());
    match newest {
        Some((_, newest)) => now - newest >= SNAPSHOT_EVERY,
        None => true,
    }
}

/// Record the folder totals in the index, then prune old snapshots.
pub fn take_snapshot(db: &Path, now: i64) -> Result<(), String> {
    let index = SpaceIndex::load(db)?;
    let rows = index.folder_sizes(HISTORY_MIN_BYTES, HISTORY_MAX_DIRS);
    let mut conn = open_history(db)?;
    DbManager::insert_size_snapshot(&mut conn, now, &rows).map_err(|e| e.to_string())?;
    DbManager::prune_size_snapshots(&mut conn, now, KEEP_ALL_SECS, MAX_AGE_SECS, WEEK_SECS)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Live folder sizes against the snapshot nearest one `period` before `now`.
/// That snapshot must be at least half the period old, otherwise there is no
/// baseline yet and the diff is empty.
pub fn changes(db: &Path, live: &SpaceIndex, period: Period, now: i64) -> Result<Changes, String> {
    let conn = open_history(db)?;
    let first = DbManager::size_snapshot_bounds(&conn)
        .map_err(|e| e.to_string())?
        .map(|(oldest, _)| oldest);
    let mut out = Changes {
        period,
        baseline_at: None,
        first_snapshot_at: first,
        now,
        diff: Diff::default(),
    };
    let nearest =
        DbManager::nearest_size_snapshot(&conn, now - period.secs()).map_err(|e| e.to_string())?;
    let Some((id, taken_at)) = nearest.filter(|&(_, at)| at <= now - period.secs() / 2) else {
        return Ok(out);
    };
    let old = DbManager::size_snapshot_rows(&conn, id).map_err(|e| e.to_string())?;
    out.baseline_at = Some(taken_at);
    out.diff = diff_sizes(&old, &live.folder_sizes(1, usize::MAX), HISTORY_LIMIT);
    Ok(out)
}

// ── Delete summary ────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Serialize)]
pub struct DeleteSummary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// The walk hit its time budget; the counts are a lower bound.
    pub truncated: bool,
}

/// Count what deleting `paths` would remove, within a time budget so a huge
/// tree cannot keep the confirmation dialog waiting.
pub fn delete_summary(paths: &[PathBuf], budget: Duration) -> DeleteSummary {
    let started = Instant::now();
    let mut out = DeleteSummary::default();
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        if started.elapsed() > budget {
            out.truncated = true;
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            out.dirs += 1;
            if let Ok(entries) = std::fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else {
            out.files += 1;
            out.bytes = out.bytes.saturating_add(meta.len());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn node(path: &str, direct: u64) -> DirNode {
        DirNode {
            path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            direct,
            total: direct,
            children: Vec::new(),
        }
    }

    fn sample() -> SpaceIndex {
        // root(1) -> a(10) -> a1(100), a2(5); root -> b(50)
        let nodes = vec![
            node("/r", 1),
            node("/r/a", 10),
            node("/r/a/a1", 100),
            node("/r/a/a2", 5),
            node("/r/b", 50),
        ];
        let parents = vec![None, Some(1), Some(2), Some(2), Some(1)];
        let by_id = (0..5).map(|i| (i as i64 + 1, i)).collect();
        SpaceIndex::assemble(nodes, &parents, &by_id)
    }

    #[test]
    fn subtree_sizes_sum_bottom_up() {
        let t = sample().tree(Path::new("/r"), 2, 10).expect("root");
        assert_eq!(t.size, 166);
        assert_eq!(t.direct, 1);
        assert_eq!(t.children[0].name, "a", "largest first");
        assert_eq!(t.children[0].size, 115);
        assert_eq!(t.children[0].children[0].size, 100);
        assert_eq!(t.children[1].size, 50);
    }

    #[test]
    fn extra_children_fold_into_others() {
        let t = sample().tree(Path::new("/r"), 1, 1).expect("root");
        assert_eq!(t.children.len(), 1);
        assert_eq!(t.other_count, 1);
        assert_eq!(t.other_size, 50);
    }

    #[test]
    fn size_of_returns_whole_subtree_totals() {
        let idx = sample();
        assert_eq!(idx.size_of(Path::new("/r/a")), Some(115));
        assert_eq!(idx.size_of(Path::new("/r/b/")), Some(50));
        assert_eq!(idx.size_of(Path::new("/elsewhere")), None);
    }

    #[test]
    fn lookup_ignores_trailing_separators() {
        assert!(sample().tree(Path::new("/r/a/"), 0, 4).is_some());
        assert!(sample().tree(Path::new("/nope"), 0, 4).is_none());
    }

    const MIB: u64 = 1024 * 1024;
    const DAY: i64 = 86_400;

    fn sized(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|(p, s)| (p.to_string(), *s)).collect()
    }

    /// Set each folder's size in a test index, adding the folder if needed.
    fn set_sizes(db: &Path, sizes: &[(&str, u64)]) {
        let conn = Connection::open(db).unwrap();
        DbManager::init_db(&conn).unwrap();
        for (path, size) in sizes {
            let n = conn
                .execute(
                    "UPDATE directories SET direct_size = ?1 WHERE path = ?2",
                    rusqlite::params![*size as i64, path],
                )
                .unwrap();
            if n == 0 {
                conn.execute(
                    "INSERT INTO directories (parent_id, name, path, direct_size) VALUES (NULL, ?1, ?1, ?2)",
                    rusqlite::params![path, *size as i64],
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn diff_reports_grown_shrunk_new_and_deleted() {
        let old = sized(&[
            ("/a", 100 * MIB),
            ("/b", 200 * MIB),
            ("/gone", 80 * MIB),
            ("/same", 300 * MIB),
            ("/tiny", 50 * MIB),
        ]);
        let new = sized(&[
            ("/a", 150 * MIB),
            ("/b", 120 * MIB),
            ("/same", 300 * MIB + 10),
            ("/tiny", 50 * MIB + 100),
            ("/fresh", 70 * MIB),
            ("/small", 10 * MIB),
        ]);
        let d = diff_sizes(&old, &new, 10);
        let change = |path: &str, old_size: u64, new_size: u64| Change {
            path: path.into(),
            old_size,
            new_size,
            delta: new_size as i64 - old_size as i64,
        };
        assert_eq!(d.grown, vec![change("/a", 100 * MIB, 150 * MIB)]);
        assert_eq!(d.shrunk, vec![change("/b", 200 * MIB, 120 * MIB)]);
        assert_eq!(d.deleted, vec![change("/gone", 80 * MIB, 0)]);
        assert_eq!(d.new, vec![change("/fresh", 0, 70 * MIB)]);
    }

    #[test]
    fn diff_is_sorted_and_capped() {
        let old = sized(&[("/a", 100 * MIB), ("/b", 100 * MIB)]);
        let new = sized(&[("/a", 200 * MIB), ("/b", 400 * MIB)]);
        let d = diff_sizes(&old, &new, 1);
        assert_eq!(d.grown.len(), 1);
        assert_eq!(d.grown[0].path, "/b", "biggest growth first");
    }

    #[test]
    fn changes_compare_live_sizes_with_the_snapshot_a_week_back() {
        let tmp = TempDir::new("space_changes");
        let db = tmp.path().join("index.db");
        let now = 1_000 * DAY;
        set_sizes(&db, &[("/data", 200 * MIB), ("/cache", 60 * MIB)]);
        take_snapshot(&db, now - 8 * DAY).unwrap();
        set_sizes(
            &db,
            &[("/data", 260 * MIB), ("/cache", 0), ("/new", 90 * MIB)],
        );

        let live = SpaceIndex::load(&db).unwrap();
        let c = changes(&db, &live, Period::Week, now).unwrap();
        assert_eq!(c.baseline_at, Some(now - 8 * DAY));
        assert_eq!(c.first_snapshot_at, Some(now - 8 * DAY));
        assert_eq!(c.diff.grown.len(), 1);
        assert_eq!(
            (c.diff.grown[0].path.as_str(), c.diff.grown[0].delta),
            ("/data", (60 * MIB) as i64)
        );
        assert_eq!(c.diff.deleted.len(), 1);
        assert_eq!(c.diff.deleted[0].path, "/cache");
        assert_eq!(c.diff.new.len(), 1);
        assert_eq!(c.diff.new[0].path, "/new");
        assert!(c.diff.shrunk.is_empty());
    }

    #[test]
    fn no_baseline_until_a_snapshot_is_old_enough() {
        let tmp = TempDir::new("space_no_baseline");
        let db = tmp.path().join("index.db");
        let now = 1_000 * DAY;
        set_sizes(&db, &[("/data", 200 * MIB)]);
        let live = SpaceIndex::load(&db).unwrap();
        let none = changes(&db, &live, Period::Week, now).unwrap();
        assert_eq!(
            (none.baseline_at, none.first_snapshot_at),
            (None, None),
            "no history at all"
        );

        take_snapshot(&db, now - 2 * 3600).unwrap();
        let young = changes(&db, &live, Period::Week, now).unwrap();
        assert_eq!(
            young.baseline_at, None,
            "an hour-old snapshot is no week-long baseline"
        );
        assert_eq!(young.first_snapshot_at, Some(now - 2 * 3600));
        assert_eq!(young.diff, Diff::default());
    }

    #[test]
    fn snapshot_is_due_once_a_day() {
        let tmp = TempDir::new("space_due");
        let db = tmp.path().join("index.db");
        let now = 1_000 * DAY;
        assert!(
            !snapshot_due(&db, now),
            "no index file, nothing to snapshot"
        );
        set_sizes(&db, &[("/data", 200 * MIB)]);
        assert!(snapshot_due(&db, now), "index but no snapshot yet");
        take_snapshot(&db, now).unwrap();
        assert!(!snapshot_due(&db, now + 3600));
        assert!(snapshot_due(&db, now + DAY));
    }

    #[test]
    fn history_survives_a_full_rebuild() {
        let tmp = TempDir::new("space_stash");
        let db = tmp.path().join("index.db");
        let now = 1_000 * DAY;
        set_sizes(&db, &[("/data", 200 * MIB)]);
        take_snapshot(&db, now - 8 * DAY).unwrap();

        stash_history(&db).unwrap();
        crate::db::remove_index_files(&db).unwrap(); // what a rebuild does first
        set_sizes(&db, &[("/data", 300 * MIB)]);
        take_snapshot(&db, now).unwrap();

        let conn = DbManager::open_history(&db).unwrap();
        let times: Vec<i64> = DbManager::size_snapshot_ids(&conn)
            .unwrap()
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert_eq!(times, vec![now - 8 * DAY, now]);
        assert!(
            !history_sidecar(&db).exists(),
            "parked history is moved back and removed"
        );
    }

    #[test]
    fn delete_summary_counts_a_tree() {
        let tmp = TempDir::new("del_summary");
        tmp.file("x/a.txt", b"12345");
        tmp.file("x/sub/b.txt", b"123");
        let s = delete_summary(&[tmp.path().join("x")], Duration::from_secs(5));
        assert_eq!((s.files, s.dirs, s.bytes, s.truncated), (2, 2, 8, false));
    }
}
