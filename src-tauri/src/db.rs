use rusqlite::{Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bumped after every index write, so the UI can tell the index changed.
static INDEX_GENERATION: AtomicU64 = AtomicU64::new(0);

/// How many index writes have finished in this process.
pub fn index_generation() -> u64 {
    INDEX_GENERATION.load(Ordering::Relaxed)
}

/// Marks one index write. Declared first in a write function so it drops
/// last, after the transaction or statements inside it have committed.
struct IndexWriteMark;

impl Drop for IndexWriteMark {
    fn drop(&mut self) {
        INDEX_GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Open a connection with foreign keys enabled. SQLite keeps them off unless
/// each connection sets the pragma, so `ON DELETE CASCADE` is otherwise a no-op.
fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Watcher refreshes and the index writer share the file; wait for the
    // lock instead of failing with SQLITE_BUSY at once.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Delete the index database and its WAL/SHM siblings. A leftover WAL next to
/// a newly created file of the same name can be replayed into it.
pub fn remove_index_files(db_path: &Path) -> std::io::Result<()> {
    let base = db_path.as_os_str();
    match std::fs::remove_file(db_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = base.to_os_string();
        sidecar.push(suffix);
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// True when an error means the database file itself is unusable (not a
/// database, malformed pages), as opposed to a transient failure like a locked
/// file. Only the former justifies throwing the index away.
pub fn is_corruption(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(f, _)
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
        )
    })
}

/// Move a damaged index (and WAL/SHM) aside as `<name>.corrupt` so it can be
/// rebuilt, keeping the evidence instead of deleting it.
pub fn quarantine_index(db_path: &Path) -> std::io::Result<()> {
    let mut quarantined = db_path.as_os_str().to_os_string();
    quarantined.push(".corrupt");
    let _ = std::fs::remove_file(&quarantined);
    std::fs::rename(db_path, &quarantined)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(&sidecar);
    }
    Ok(())
}

/// Escape `s` for a `LIKE` pattern. Every such query says `ESCAPE '!'`: the
/// escape character must not be `\`, which is the path separator on Windows.
fn escape_like(s: &str) -> String {
    s.replace('!', "!!").replace('%', "!%").replace('_', "!_")
}

/// `LIKE` pattern matching every path strictly below `path`. Uses the native
/// separator, because stored paths are built from `Path` on each platform.
fn descendants_pattern(path: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    format!("{}{sep}%", escape_like(path.trim_end_matches(sep)))
}

fn scope_conditions(
    path_expr: &str,
    parsed: &crate::search::ParsedQuery,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
) -> Option<String> {
    let scope = parsed.scope_path.as_ref()?;
    let mut scope_str = scope.to_string_lossy().into_owned();
    while scope_str.len() > 1 && scope_str.ends_with(std::path::MAIN_SEPARATOR) {
        scope_str.pop();
    }
    if parsed.scope_current_dir {
        params.push(Box::new(scope_str));
        Some(format!("{path_expr} = ?"))
    } else {
        let like = descendants_pattern(&scope_str);
        params.push(Box::new(scope_str));
        params.push(Box::new(like));
        Some(format!(
            "({path_expr} = ? OR {path_expr} LIKE ? ESCAPE '!')"
        ))
    }
}

pub const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA page_size = 4096;
PRAGMA mmap_size = 268435456;

CREATE TABLE IF NOT EXISTS directories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    direct_size INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER NOT NULL DEFAULT 0,
    indexed_at INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_dir_parent_name ON directories(parent_id, name);
CREATE INDEX IF NOT EXISTS idx_dir_parent ON directories(parent_id);
CREATE INDEX IF NOT EXISTS idx_dir_path ON directories(path);
CREATE INDEX IF NOT EXISTS idx_dir_size ON directories(direct_size);

CREATE TABLE IF NOT EXISTS index_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    directory_id INTEGER NOT NULL REFERENCES directories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    attributes INTEGER NOT NULL,
    extension TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_files_dir_name ON files(directory_id, name);
CREATE INDEX IF NOT EXISTS idx_files_directory ON files(directory_id);
CREATE INDEX IF NOT EXISTS idx_files_dir_size ON files(directory_id, size);
CREATE INDEX IF NOT EXISTS idx_files_name ON files(name);
CREATE INDEX IF NOT EXISTS idx_files_metadata ON files(extension, size, modified_at);

CREATE TABLE IF NOT EXISTS size_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    taken_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_size_snapshots_taken ON size_snapshots(taken_at);
CREATE TABLE IF NOT EXISTS size_snapshot_dirs (
    snapshot_id INTEGER NOT NULL REFERENCES size_snapshots(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, path)
) WITHOUT ROWID;
"#;

// ── Walk helpers ──────────────────────────────────────────────────────────────

use crate::search::is_skipped_dir as skip_dir;

enum WalkItem {
    Dir {
        path: PathBuf,
        parent_path: Option<PathBuf>,
        name: String,
        mtime: i64,
    },
    File {
        dir_path: PathBuf,
        name: String,
        size: i64,
        modified: i64,
        created: i64,
        extension: Option<String>,
    },
}

fn path_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .as_ref()
        .map(meta_mtime)
        .unwrap_or(0)
}

/// Directory-mtime used to skip unchanged folders. Microseconds so two
/// creates in the same second still invalidate the cached listing.
fn meta_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn meta_created(meta: &std::fs::Metadata) -> i64 {
    meta.created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn meta_mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// APFS firmlink: `/System/Volumes/Data/Users/...` is the same tree as
/// `/Users/...`. Prefer the short path so search hits and open/reveal work.
fn user_facing_path(path: PathBuf) -> PathBuf {
    const PREFIX: &str = "/System/Volumes/Data";
    match path.strip_prefix(PREFIX) {
        Ok(rest) => Path::new("/").join(rest),
        Err(_) => path,
    }
}

fn dedupe_index_nodes(nodes: Vec<crate::core::VNode>) -> Vec<crate::core::VNode> {
    let mut seen = std::collections::HashSet::new();
    nodes
        .into_iter()
        .filter(|n| seen.insert(n.path.clone()))
        .collect()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Paths we never index or watch, matching the walker skip list.
pub fn ignored_index_path(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s != "/" && skip_dir(s))
    })
}

fn walk_root(
    root: &Path,
    tx: &std::sync::mpsc::Sender<WalkItem>,
    cancel: &std::sync::atomic::AtomicBool,
) {
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());

    if tx
        .send(WalkItem::Dir {
            path: root.to_path_buf(),
            parent_path: None,
            name: root_name,
            mtime: path_mtime(root),
        })
        .is_err()
    {
        return;
    }

    let mut stack = vec![root.to_path_buf()];

    while let Some(current_dir) = stack.pop() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        let entries = match std::fs::read_dir(&current_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }

            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if ft.is_dir() {
                if skip_dir(&name) {
                    continue;
                }
                let mtime = entry.metadata().as_ref().map(meta_mtime).unwrap_or(0);
                if tx
                    .send(WalkItem::Dir {
                        path: path.clone(),
                        parent_path: Some(current_dir.clone()),
                        name,
                        mtime,
                    })
                    .is_err()
                {
                    return;
                }
                stack.push(path);
            } else if ft.is_file() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase());
                let modified = meta_mtime_secs(&meta);
                let created = meta_created(&meta);
                if tx
                    .send(WalkItem::File {
                        dir_path: current_dir.clone(),
                        name,
                        size: meta.len() as i64,
                        modified,
                        created,
                        extension: ext,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn migrate_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS index_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    for sql in [
        "ALTER TABLE directories ADD COLUMN mtime INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE directories ADD COLUMN indexed_at INTEGER NOT NULL DEFAULT 0",
    ] {
        if let Err(e) = conn.execute(sql, []) {
            if !e.to_string().contains("duplicate column") {
                return Err(e);
            }
        }
    }
    Ok(())
}

pub struct SyncStats {
    pub dirs_checked: usize,
    pub dirs_listed: usize,
    pub files_upserted: usize,
    pub files_removed: usize,
}

struct ListedDir {
    upserted: usize,
    removed: usize,
    child_dirs: Vec<PathBuf>,
    /// The folder could not be read. Nothing was changed, and its mtime must
    /// not be stamped, so the next sync tries it again.
    unreadable: bool,
}

pub struct DbManager;

impl DbManager {
    pub fn init_db(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA_SQL)?;
        migrate_schema(conn)?;
        Ok(())
    }

    fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Candidate rows pulled from the index before in-process ranking.
    /// Larger than `MAX_VISIBLE_RESULTS` so `filter_and_rank` can pick the best.
    pub const INDEX_CANDIDATE_LIMIT: i64 = 2000;

    /// Path of every indexed file. Unlike `query_disk_index` this is not capped,
    /// so the duplicate finder sees the whole index.
    pub fn indexed_file_paths(
        db_path: &Path,
    ) -> Result<Vec<PathBuf>, Box<dyn std::error::Error + Send + Sync>> {
        let conn = open_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT d.path, f.name FROM files f JOIN directories d ON f.directory_id = d.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PathBuf::from(row.get::<_, String>(0)?).join(row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn query_disk_index(
        db_path: &Path,
        parsed: &crate::search::ParsedQuery,
    ) -> Result<Vec<crate::core::VNode>, Box<dyn std::error::Error + Send + Sync>> {
        let conn = open_db(db_path)?;
        let mut results = Vec::new();
        let want_folders = parsed.ext.as_deref() == Some("folder");
        let want_files = !want_folders;
        let want_dirs = parsed.ext.is_none() || want_folders;

        if want_files {
            results.extend(Self::query_indexed_files(&conn, parsed)?);
        }
        if want_dirs {
            results.extend(Self::query_indexed_dirs(&conn, parsed)?);
        }
        Ok(dedupe_index_nodes(results))
    }

    fn query_indexed_files(
        conn: &Connection,
        parsed: &crate::search::ParsedQuery,
    ) -> Result<Vec<crate::core::VNode>, Box<dyn std::error::Error + Send + Sync>> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if !parsed.text.is_empty() {
            let like_val = format!("%{}%", escape_like(&parsed.text));
            conditions.push("f.name LIKE ? ESCAPE '!'".to_string());
            param_values.push(Box::new(like_val));
        }
        if let Some(ref name) = parsed.name {
            let like_val = format!("%{}%", escape_like(name));
            conditions.push("f.name LIKE ? ESCAPE '!'".to_string());
            param_values.push(Box::new(like_val));
        }

        if let Some(ref ext) = parsed.ext {
            // Match the stored extension, and also names that end in `.ext`
            // in case an older row has a NULL extension column.
            let ext = ext.to_ascii_lowercase();
            conditions.push(
                "(LOWER(IFNULL(f.extension, '')) = ? OR LOWER(f.name) LIKE ? ESCAPE '!')"
                    .to_string(),
            );
            param_values.push(Box::new(ext.clone()));
            // `_` and `%` in the extension are literal, not LIKE wildcards.
            param_values.push(Box::new(format!("%.{}", escape_like(&ext))));
        }

        if let Some(min) = parsed.min_size {
            conditions.push("f.size >= ?".to_string());
            param_values.push(Box::new(min as i64));
        }
        if let Some(max) = parsed.max_size {
            conditions.push("f.size <= ?".to_string());
            param_values.push(Box::new(max as i64));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if let Some(max_age) = parsed.max_age_secs {
            conditions.push("f.modified_at >= ?".to_string());
            param_values.push(Box::new(now - max_age as i64));
        }
        if let Some(min_age) = parsed.min_age_secs {
            conditions.push("f.modified_at <= ?".to_string());
            param_values.push(Box::new(now - min_age as i64));
        }

        if let Some(clause) = scope_conditions("d.path", parsed, &mut param_values) {
            conditions.push(clause);
        }

        let where_clause = if conditions.is_empty() {
            "1=1".to_string()
        } else {
            conditions.join(" AND ")
        };

        let sql = format!(
            "SELECT f.name, f.size, f.modified_at, f.created_at, d.path
             FROM files f
             JOIN directories d ON f.directory_id = d.id
             WHERE {where_clause}
             ORDER BY f.modified_at DESC
             LIMIT {}",
            Self::INDEX_CANDIDATE_LIMIT
        );

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let nodes_iter = stmt.query_map(param_refs.as_slice(), |row| {
            let name: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let modified: i64 = row.get(2)?;
            let created: i64 = row.get(3)?;
            let parent_path: String = row.get(4)?;
            let full_path = user_facing_path(PathBuf::from(parent_path).join(&name));
            Ok(crate::core::VNode {
                name,
                path: full_path,
                size: size as u64,
                is_dir: false,
                is_symlink: false,
                modified_at: modified as u64,
                created_at: created as u64,
                readonly: false,
                hidden: false,
                mime_type: "application/octet-stream".to_string(),
            })
        })?;

        Ok(nodes_iter.flatten().collect())
    }

    fn query_indexed_dirs(
        conn: &Connection,
        parsed: &crate::search::ParsedQuery,
    ) -> Result<Vec<crate::core::VNode>, Box<dyn std::error::Error + Send + Sync>> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if !parsed.text.is_empty() {
            let like_val = format!("%{}%", escape_like(&parsed.text));
            conditions.push("d.name LIKE ? ESCAPE '!'".to_string());
            param_values.push(Box::new(like_val));
        }
        if let Some(ref name) = parsed.name {
            let like_val = format!("%{}%", escape_like(name));
            conditions.push("d.name LIKE ? ESCAPE '!'".to_string());
            param_values.push(Box::new(like_val));
        }

        if let Some(min) = parsed.min_size {
            conditions.push("d.direct_size >= ?".to_string());
            param_values.push(Box::new(min as i64));
        }
        if let Some(max) = parsed.max_size {
            conditions.push("d.direct_size <= ?".to_string());
            param_values.push(Box::new(max as i64));
        }

        if parsed.max_age_secs.is_some() || parsed.min_age_secs.is_some() {
            // Directories have no stored mtime; they cannot satisfy an age filter.
            return Ok(Vec::new());
        }

        if let Some(clause) = scope_conditions("d.path", parsed, &mut param_values) {
            conditions.push(clause);
        }

        let where_clause = if conditions.is_empty() {
            "1=1".to_string()
        } else {
            conditions.join(" AND ")
        };

        let sql = format!(
            "SELECT d.name, d.direct_size, d.path
             FROM directories d
             WHERE {where_clause}
             ORDER BY d.direct_size DESC
             LIMIT {}",
            Self::INDEX_CANDIDATE_LIMIT
        );

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let nodes_iter = stmt.query_map(param_refs.as_slice(), |row| {
            let name: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let path: String = row.get(2)?;
            Ok(crate::core::VNode {
                name,
                path: user_facing_path(PathBuf::from(path)),
                size: size as u64,
                is_dir: true,
                is_symlink: false,
                modified_at: 0,
                created_at: 0,
                readonly: false,
                hidden: false,
                mime_type: "inode/directory".to_string(),
            })
        })?;

        Ok(nodes_iter.flatten().collect())
    }

    pub fn index_multiple_to_sqlite(
        db_path: &Path,
        roots: &[PathBuf],
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        progress_tx: std::sync::mpsc::Sender<usize>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let _mark = IndexWriteMark;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let mut conn = open_db(db_path)?;
        DbManager::init_db(&conn)?;

        // Spawn one walk thread per root so drives are traversed in parallel.
        let (item_tx, item_rx) = std::sync::mpsc::channel::<WalkItem>();
        let handles: Vec<_> = roots
            .iter()
            .map(|root| {
                let root = root.clone();
                let tx = item_tx.clone();
                let cancel = cancel.clone();
                std::thread::spawn(move || walk_root(&root, &tx, &cancel))
            })
            .collect();
        drop(item_tx); // channel closes once all walk threads finish

        // Single writer: drain the channel and insert into SQLite.
        let db_tx = conn.transaction()?;
        // Overlapping roots (e.g. `/` and `/Volumes/Macintosh HD`) can insert
        // the same (parent, name); upsert instead of failing the unique index.
        let indexed_at = now_secs();
        let mut dir_stmt = db_tx.prepare(
            "INSERT INTO directories (parent_id, name, path, mtime, indexed_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(parent_id, name) DO UPDATE SET \
                path = excluded.path, mtime = excluded.mtime, indexed_at = excluded.indexed_at \
             RETURNING id",
        )?;
        // Same overlap one level down: `OR IGNORE` plus UNIQUE(directory_id, name)
        // so a file reached twice is not double-counted.
        let mut file_stmt = db_tx.prepare(
            "INSERT OR IGNORE INTO files (directory_id, name, size, modified_at, created_at, attributes, extension) \
             VALUES (?, ?, ?, ?, ?, 0, ?)",
        )?;

        let mut dir_map: std::collections::HashMap<PathBuf, i64> = std::collections::HashMap::new();
        let mut dir_sizes: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let mut total_indexed: usize = 0;

        for item in item_rx {
            match item {
                WalkItem::Dir {
                    path,
                    parent_path,
                    name,
                    mtime,
                } => {
                    let parent_id: Option<i64> =
                        parent_path.as_ref().and_then(|p| dir_map.get(p)).copied();
                    let path_str = path.to_string_lossy().into_owned();
                    let id: i64 = dir_stmt.query_row(
                        rusqlite::params![parent_id, name, path_str, mtime, indexed_at],
                        |row| row.get(0),
                    )?;
                    // First write wins: overlapping root walks can emit the
                    // same path twice (NULL parent_id is not unique).
                    dir_map.entry(path).or_insert(id);
                    dir_sizes.entry(id).or_insert(0);
                }
                WalkItem::File {
                    dir_path,
                    name,
                    size,
                    modified,
                    created,
                    extension,
                } => {
                    if let Some(&dir_id) = dir_map.get(&dir_path) {
                        let inserted = file_stmt.execute(rusqlite::params![
                            dir_id, name, size, modified, created, extension,
                        ])?;
                        // `execute` returns the row count actually changed,
                        // so a row skipped by `OR IGNORE` (already indexed
                        // under this directory) does not double-count its
                        // size or the progress total.
                        if inserted > 0 {
                            *dir_sizes.entry(dir_id).or_insert(0) += size;
                            total_indexed += 1;
                            if total_indexed.is_multiple_of(5000) {
                                let _ = progress_tx.send(total_indexed);
                            }
                        }
                    }
                }
            }
        }

        for handle in handles {
            let _ = handle.join();
        }

        dir_stmt.finalize()?;
        file_stmt.finalize()?;

        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            drop(db_tx);
            let _ = remove_index_files(db_path);
            return Err("Indexing cancelled".into());
        }

        let mut size_stmt = db_tx.prepare("UPDATE directories SET direct_size = ? WHERE id = ?")?;
        for (dir_id, total_size) in &dir_sizes {
            size_stmt.execute(rusqlite::params![total_size, dir_id])?;
        }
        size_stmt.finalize()?;

        db_tx.commit()?;
        let _ = Self::meta_set(&conn, "last_sync", &now_secs().to_string());
        let _ = Self::meta_set(
            &conn,
            "roots",
            &roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _ = progress_tx.send(total_indexed);
        Ok(total_indexed)
    }

    pub fn query_space_hoggers_from_sqlite(
        db_path: &Path,
    ) -> Result<Vec<(PathBuf, u64)>, Box<dyn std::error::Error + Send + Sync>> {
        let conn = open_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT path, direct_size FROM directories
             WHERE direct_size > 0
             ORDER BY direct_size DESC
             LIMIT 50",
        )?;

        let rows = stmt.query_map([], |row| {
            let path_str: String = row.get(0)?;
            let direct_size: i64 = row.get(1)?;
            Ok((PathBuf::from(path_str), direct_size as u64))
        })?;

        let results = rows.flatten().collect();
        Ok(results)
    }

    /// Open the index for size-history work, creating the snapshot tables if
    /// an older database lacks them.
    pub fn open_history(
        db_path: &Path,
    ) -> Result<Connection, Box<dyn std::error::Error + Send + Sync>> {
        let conn = open_db(db_path)?;
        DbManager::init_db(&conn)?;
        Ok(conn)
    }

    /// Store one snapshot: its time and the (path, bytes) rows, in one transaction.
    pub fn insert_size_snapshot(
        conn: &mut Connection,
        taken_at: i64,
        rows: &[(String, u64)],
    ) -> Result<i64> {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO size_snapshots (taken_at) VALUES (?1)",
            [taken_at],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO size_snapshot_dirs (snapshot_id, path, size) VALUES (?1, ?2, ?3)",
            )?;
            for (path, size) in rows {
                stmt.execute(rusqlite::params![id, path, *size as i64])?;
            }
        }
        tx.commit()?;
        Ok(id)
    }

    /// Every snapshot as (id, taken_at), oldest first.
    pub fn size_snapshot_ids(conn: &Connection) -> Result<Vec<(i64, i64)>> {
        let mut stmt = conn.prepare("SELECT id, taken_at FROM size_snapshots ORDER BY taken_at")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The (path, bytes) rows of one snapshot.
    pub fn size_snapshot_rows(conn: &Connection, id: i64) -> Result<Vec<(String, u64)>> {
        let mut stmt =
            conn.prepare("SELECT path, size FROM size_snapshot_dirs WHERE snapshot_id = ?1")?;
        let rows = stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        rows.collect()
    }

    /// Time of the oldest and newest snapshot, if there are any.
    pub fn size_snapshot_bounds(conn: &Connection) -> Result<Option<(i64, i64)>> {
        conn.query_row(
            "SELECT MIN(taken_at), MAX(taken_at) FROM size_snapshots",
            [],
            |r| {
                Ok(
                    match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?) {
                        (Some(oldest), Some(newest)) => Some((oldest, newest)),
                        _ => None,
                    },
                )
            },
        )
    }

    /// The snapshot taken closest to `target`, as (id, taken_at).
    pub fn nearest_size_snapshot(conn: &Connection, target: i64) -> Result<Option<(i64, i64)>> {
        match conn.query_row(
            "SELECT id, taken_at FROM size_snapshots ORDER BY ABS(taken_at - ?1) LIMIT 1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Ok(found) => Ok(Some(found)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Drop snapshots older than `max_age`. Those older than `keep_all` are
    /// thinned to the newest one per `bucket` seconds. Returns how many went.
    pub fn prune_size_snapshots(
        conn: &mut Connection,
        now: i64,
        keep_all: i64,
        max_age: i64,
        bucket: i64,
    ) -> Result<usize> {
        let tx = conn.transaction()?;
        let cutoff = now - max_age;
        tx.execute(
            "DELETE FROM size_snapshot_dirs WHERE snapshot_id IN
                (SELECT id FROM size_snapshots WHERE taken_at < ?1)",
            [cutoff],
        )?;
        let mut removed = tx.execute("DELETE FROM size_snapshots WHERE taken_at < ?1", [cutoff])?;

        let older: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id, taken_at FROM size_snapshots WHERE taken_at < ?1 ORDER BY taken_at DESC",
            )?;
            let rows = stmt.query_map([now - keep_all], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_>>()?
        };
        // Newest first, so the first snapshot seen in a bucket is the one kept.
        let mut kept_buckets = std::collections::HashSet::new();
        for (id, taken_at) in older {
            if kept_buckets.insert(taken_at.div_euclid(bucket)) {
                continue;
            }
            tx.execute(
                "DELETE FROM size_snapshot_dirs WHERE snapshot_id = ?1",
                [id],
            )?;
            removed += tx.execute("DELETE FROM size_snapshots WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Re-read one directory's immediate files into the index. Used by the
    /// filesystem watcher so a single create/delete does not rebuild the world.
    pub fn refresh_directory(
        db_path: &Path,
        dir: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _mark = IndexWriteMark;
        if !db_path.exists() {
            return Ok(());
        }
        let conn = open_db(db_path)?;
        let dir_str = dir.to_string_lossy();
        let dir_id: i64 = match conn.query_row(
            "SELECT id FROM directories WHERE path = ?1",
            [dir_str.as_ref()],
            |row| row.get(0),
        ) {
            Ok(id) => id,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // Gone: drop it from the index. Any other error leaves the rows as
            // they were; an unreadable folder is not an empty one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::delete_from_index_conn(&conn, dir)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if ft.is_dir() {
                continue;
            }
            live.insert(name.to_string());
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase());
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0) as i64;
            let created = meta
                .created()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0) as i64;
            conn.execute(
                "INSERT INTO files (directory_id, name, size, modified_at, created_at, attributes, extension)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT(directory_id, name) DO UPDATE SET
                    size = excluded.size,
                    modified_at = excluded.modified_at,
                    created_at = excluded.created_at,
                    extension = excluded.extension",
                rusqlite::params![dir_id, name, meta.len() as i64, modified, created, ext],
            )?;
        }

        let mut stmt = conn.prepare("SELECT name FROM files WHERE directory_id = ?1")?;
        let existing: Vec<String> = stmt
            .query_map([dir_id], |row| row.get(0))?
            .flatten()
            .collect();
        drop(stmt);
        for name in existing {
            if !live.contains(&name) {
                conn.execute(
                    "DELETE FROM files WHERE directory_id = ?1 AND name = ?2",
                    rusqlite::params![dir_id, name],
                )?;
            }
        }

        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM files WHERE directory_id = ?1",
            [dir_id],
            |row| row.get(0),
        )?;
        // Leave `mtime` alone: this pass does not reconcile subfolders, so the
        // directory is not fully synced. Keeping the old stamp makes the next
        // `sync_roots` list it and pick up any subfolder changes.
        conn.execute(
            "UPDATE directories SET direct_size = ?1, indexed_at = ?2 WHERE id = ?3",
            rusqlite::params![sum, now_secs(), dir_id],
        )?;
        Ok(())
    }

    /// Incremental catch-up: reuse the existing database, only re-list
    /// directories whose mtime changed (creates/deletes/renames). Used on
    /// startup so an index built once stays current after the app was closed.
    pub fn sync_roots(
        db_path: &Path,
        roots: &[PathBuf],
        cancel: &std::sync::atomic::AtomicBool,
        progress_tx: &std::sync::mpsc::Sender<usize>,
    ) -> Result<SyncStats, Box<dyn std::error::Error + Send + Sync>> {
        let _mark = IndexWriteMark;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = open_db(db_path)?;
        DbManager::init_db(&conn)?;

        let mut stats = SyncStats {
            dirs_checked: 0,
            dirs_listed: 0,
            files_upserted: 0,
            files_removed: 0,
        };
        let mut stack: Vec<PathBuf> = roots.to_vec();
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        while let Some(dir) = stack.pop() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("Indexing cancelled".into());
            }
            if ignored_index_path(&dir) {
                continue;
            }
            if !seen.insert(dir.clone()) {
                continue;
            }
            stats.dirs_checked += 1;
            if stats.dirs_checked.is_multiple_of(200) {
                let _ = progress_tx.send(stats.dirs_checked);
            }

            let live_meta = match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => m,
                Ok(_) => {
                    Self::delete_from_index_conn(&conn, &dir)?;
                    continue;
                }
                Err(_) => {
                    Self::delete_from_index_conn(&conn, &dir)?;
                    continue;
                }
            };
            let live_mtime = meta_mtime(&live_meta);
            let dir_id = Self::ensure_directory_conn(&conn, &dir)?;

            let indexed_mtime: i64 = conn
                .query_row(
                    "SELECT mtime FROM directories WHERE id = ?1",
                    [dir_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if indexed_mtime == live_mtime && indexed_mtime > 0 {
                let mut stmt = conn.prepare("SELECT path FROM directories WHERE parent_id = ?1")?;
                let children: Vec<PathBuf> = stmt
                    .query_map([dir_id], |row| {
                        let p: String = row.get(0)?;
                        Ok(PathBuf::from(p))
                    })?
                    .flatten()
                    .collect();
                drop(stmt);
                stack.extend(children);
                continue;
            }

            stats.dirs_listed += 1;
            let listed = Self::list_and_diff_dir(&conn, dir_id, &dir)?;
            if listed.unreadable {
                continue;
            }
            stats.files_upserted += listed.upserted;
            stats.files_removed += listed.removed;
            stack.extend(listed.child_dirs);
        }

        let _ = Self::meta_set(&conn, "last_sync", &now_secs().to_string());
        let _ = Self::meta_set(
            &conn,
            "roots",
            &roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _ = progress_tx.send(stats.dirs_checked);
        Ok(stats)
    }

    /// Apply a single filesystem change (create/modify/delete) to the index.
    pub fn apply_fs_change(
        db_path: &Path,
        path: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _mark = IndexWriteMark;
        if !db_path.exists() || ignored_index_path(path) {
            return Ok(());
        }
        let conn = open_db(db_path)?;
        DbManager::init_db(&conn)?;

        match std::fs::symlink_metadata(path) {
            Err(_) => {
                Self::delete_from_index_conn(&conn, path)?;
            }
            Ok(meta) if meta.is_dir() => {
                drop(conn);
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let (tx, _rx) = std::sync::mpsc::channel();
                let _ = Self::sync_roots(db_path, &[path.to_path_buf()], &cancel, &tx)?;
            }
            Ok(meta) => {
                let Some(parent) = path.parent() else {
                    return Ok(());
                };
                let dir_id = Self::ensure_directory_conn(&conn, parent)?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| "invalid name".to_string())?;
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase());
                conn.execute(
                    "INSERT INTO files (directory_id, name, size, modified_at, created_at, attributes, extension)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                     ON CONFLICT(directory_id, name) DO UPDATE SET
                        size = excluded.size,
                        modified_at = excluded.modified_at,
                        created_at = excluded.created_at,
                        extension = excluded.extension",
                    rusqlite::params![
                        dir_id,
                        name,
                        meta.len() as i64,
                        meta_mtime_secs(&meta),
                        meta_created(&meta),
                        ext
                    ],
                )?;
                let sum: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(size), 0) FROM files WHERE directory_id = ?1",
                    [dir_id],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "UPDATE directories SET direct_size = ?1 WHERE id = ?2",
                    rusqlite::params![sum, dir_id],
                )?;
            }
        }
        Ok(())
    }

    fn ensure_directory_conn(conn: &Connection, path: &Path) -> rusqlite::Result<i64> {
        let path_str = path.to_string_lossy();
        if let Ok(id) = conn.query_row(
            "SELECT id FROM directories WHERE path = ?1",
            [path_str.as_ref()],
            |row| row.get(0),
        ) {
            return Ok(id);
        }
        let parent_id = match path.parent() {
            Some(parent) if parent != path => Some(Self::ensure_directory_conn(conn, parent)?),
            _ => None,
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.into_owned());
        let path_owned = path.to_string_lossy().into_owned();
        conn.query_row(
            "INSERT INTO directories (parent_id, name, path, mtime, indexed_at)
             VALUES (?1, ?2, ?3, 0, 0)
             ON CONFLICT(parent_id, name) DO UPDATE SET path = excluded.path
             RETURNING id",
            rusqlite::params![parent_id, name, path_owned],
            |row| row.get(0),
        )
    }

    fn list_and_diff_dir(
        conn: &Connection,
        dir_id: i64,
        dir: &Path,
    ) -> Result<ListedDir, Box<dyn std::error::Error + Send + Sync>> {
        let mut live_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut live_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut child_dirs = Vec::new();
        let mut upserted = 0usize;

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Self::delete_from_index_conn(conn, dir)?;
                }
                return Ok(ListedDir {
                    upserted: 0,
                    removed: 0,
                    child_dirs,
                    unreadable: true,
                });
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if skip_dir(name) {
                    continue;
                }
                live_dirs.insert(name.to_string());
                let _ = Self::ensure_directory_conn(conn, &path);
                child_dirs.push(path);
            } else if ft.is_file() {
                live_files.insert(name.to_string());
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase());
                let changed = conn.execute(
                        "INSERT INTO files (directory_id, name, size, modified_at, created_at, attributes, extension)
                         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                         ON CONFLICT(directory_id, name) DO UPDATE SET
                            size = excluded.size,
                            modified_at = excluded.modified_at,
                            created_at = excluded.created_at,
                            extension = excluded.extension
                         WHERE files.size != excluded.size
                            OR files.modified_at != excluded.modified_at",
                        rusqlite::params![
                            dir_id,
                            name,
                            meta.len() as i64,
                            meta_mtime_secs(&meta),
                            meta_created(&meta),
                            ext
                        ],
                    )?;
                if changed > 0 {
                    upserted += 1;
                }
            }
        }

        let mut stmt = conn.prepare("SELECT name FROM files WHERE directory_id = ?1")?;
        let existing_files: Vec<String> = stmt
            .query_map([dir_id], |row| row.get(0))?
            .flatten()
            .collect();
        drop(stmt);
        let mut removed = 0usize;
        for name in existing_files {
            if !live_files.contains(&name) {
                conn.execute(
                    "DELETE FROM files WHERE directory_id = ?1 AND name = ?2",
                    rusqlite::params![dir_id, name],
                )?;
                removed += 1;
            }
        }

        let mut stmt =
            conn.prepare("SELECT id, name, path FROM directories WHERE parent_id = ?1")?;
        let existing_dirs: Vec<(i64, String, PathBuf)> = stmt
            .query_map([dir_id], |row| {
                let id: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                let path: String = row.get(2)?;
                Ok((id, name, PathBuf::from(path)))
            })?
            .flatten()
            .collect();
        drop(stmt);
        for (_id, name, path) in existing_dirs {
            if !live_dirs.contains(&name) {
                Self::delete_from_index_conn(conn, &path)?;
            }
        }

        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM files WHERE directory_id = ?1",
            [dir_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE directories SET direct_size = ?1, mtime = ?2, indexed_at = ?3 WHERE id = ?4",
            rusqlite::params![sum, path_mtime(dir), now_secs(), dir_id],
        )?;

        Ok(ListedDir {
            upserted,
            removed,
            child_dirs,
            unreadable: false,
        })
    }

    fn delete_from_index_conn(conn: &Connection, target_path: &Path) -> rusqlite::Result<()> {
        let path_str = target_path.to_string_lossy();
        let dir_result: rusqlite::Result<i64> = conn.query_row(
            "SELECT id FROM directories WHERE path = ?1",
            [path_str.as_ref()],
            |row| row.get(0),
        );
        if dir_result.is_ok() {
            let like = descendants_pattern(&path_str);
            conn.execute(
                "DELETE FROM files WHERE directory_id IN (
                     SELECT id FROM directories WHERE path = ?1 OR path LIKE ?2 ESCAPE '!'
                 )",
                rusqlite::params![path_str.as_ref(), like],
            )?;
            conn.execute(
                "DELETE FROM directories WHERE path = ?1 OR path LIKE ?2 ESCAPE '!'",
                rusqlite::params![path_str.as_ref(), like],
            )?;
        } else if let Some(parent) = target_path.parent() {
            let parent_str = parent.to_string_lossy();
            if let Ok(parent_id) = conn.query_row::<i64, _, _>(
                "SELECT id FROM directories WHERE path = ?1",
                [parent_str.as_ref()],
                |row| row.get(0),
            ) {
                let file_name = target_path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let size: i64 = conn
                    .query_row(
                        "SELECT size FROM files WHERE directory_id = ?1 AND name = ?2",
                        rusqlite::params![parent_id, file_name.as_ref()],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                conn.execute(
                    "DELETE FROM files WHERE directory_id = ?1 AND name = ?2",
                    rusqlite::params![parent_id, file_name.as_ref()],
                )?;
                if size > 0 {
                    conn.execute(
                        "UPDATE directories SET direct_size = MAX(0, direct_size - ?1) WHERE id = ?2",
                        rusqlite::params![size, parent_id],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Remove a path (file or directory) from the SQLite index.
    /// Nested directory rows and their files are deleted too; file deletes
    /// decrement the parent directory's `direct_size`.
    pub fn delete_from_index(
        db_path: &Path,
        target_path: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _mark = IndexWriteMark;
        let conn = open_db(db_path)?;
        Self::delete_from_index_conn(&conn, target_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn indexing_the_same_root_twice_does_not_violate_the_unique_constraint() {
        // Regression: two independently walked roots that reach the same
        // logical directory tree (as happens on macOS, where `/Volumes`
        // usually contains an entry that is the boot volume mounted a
        // second time) used to crash the whole index build with
        // "UNIQUE constraint failed: directories.parent_id, directories.name"
        // the moment both walks inserted the same (parent, name) pair.
        let tmp = TempDir::new("db_overlap");
        tmp.file("a.txt", b"hello");
        tmp.file("sub/b.txt", b"world");
        tmp.dir("sub/empty");

        // Kept outside the indexed tree -- see the sibling file-count test
        // for why.
        let db_dir = TempDir::new("db_overlap_db");
        let db_path = db_dir.path().join("index.db");
        let roots = vec![tmp.path().to_path_buf(), tmp.path().to_path_buf()];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();

        let result = DbManager::index_multiple_to_sqlite(&db_path, &roots, cancel, tx);
        assert!(
            result.is_ok(),
            "indexing overlapping roots failed: {result:?}"
        );

        // No directory row exists twice under the same *non-root* parent.
        // (Two root rows sharing a name are fine: SQLite's unique index
        // treats each NULL `parent_id` as distinct from every other, same as
        // the constraint that was violated for real, non-null parents.)
        let conn = Connection::open(&db_path).expect("open");
        let dupes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT parent_id, name, COUNT(*) c FROM directories
                     WHERE parent_id IS NOT NULL
                     GROUP BY parent_id, name HAVING c > 1
                 )",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(dupes, 0);
    }

    #[test]
    fn indexing_the_same_root_twice_does_not_double_count_files() {
        // Regression: the directory upsert above stopped the crash, but
        // `files` had no equivalent dedup, so a file reached through both
        // overlapping walks was inserted twice under the same (now-shared)
        // directory id -- doubling both the file count and every size total
        // derived from it (the index, and the space-hogger scan).
        let tmp = TempDir::new("db_overlap_files");
        tmp.file("a.txt", &[0u8; 100]);
        tmp.file("sub/b.txt", &[0u8; 250]);
        // The db (plus its `-wal`/`-shm` siblings) must live outside the
        // tree being indexed, or the walk would pick up SQLite's own files
        // as extra, spuriously "unindexed-until-now" entries.
        let db_dir = TempDir::new("db_overlap_files_db");
        let db_path = db_dir.path().join("index.db");
        let roots = vec![tmp.path().to_path_buf(), tmp.path().to_path_buf()];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();

        let indexed = DbManager::index_multiple_to_sqlite(&db_path, &roots, cancel, tx)
            .expect("indexing overlapping roots");
        assert_eq!(indexed, 2, "each real file must be counted exactly once");

        let conn = Connection::open(&db_path).expect("open");
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .expect("query");
        assert_eq!(file_count, 2);

        let total_size: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM files", [], |row| {
                row.get(0)
            })
            .expect("query");
        assert_eq!(total_size, 350, "sizes must not be doubled");
    }

    fn index_into(tmp: &TempDir, db_dir: &TempDir) -> PathBuf {
        let db_path = db_dir.path().join("index.db");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();
        DbManager::index_multiple_to_sqlite(&db_path, &[tmp.path().to_path_buf()], cancel, tx)
            .expect("index");
        db_path
    }

    #[test]
    fn query_disk_index_finds_directories_and_type_folder() {
        let tmp = TempDir::new("db_query_dirs");
        tmp.file("notes.txt", b"hi");
        tmp.dir("src");
        tmp.file("src/main.rs", b"fn main() {}");
        let db_dir = TempDir::new("db_query_dirs_db");
        let db_path = index_into(&tmp, &db_dir);

        let folders = DbManager::query_disk_index(
            &db_path,
            &crate::search::ParsedQuery::parse("type:folder"),
        )
        .expect("query");
        assert!(
            folders.iter().any(|n| n.is_dir && n.name == "src"),
            "type:folder must return indexed directories, got {folders:?}"
        );
        assert!(folders.iter().all(|n| n.is_dir));

        let named =
            DbManager::query_disk_index(&db_path, &crate::search::ParsedQuery::parse("src"))
                .expect("query");
        assert!(
            named.iter().any(|n| n.is_dir && n.name == "src"),
            "a name search must hit directories, got {named:?}"
        );
    }

    #[test]
    fn query_disk_index_matches_name_filter() {
        let tmp = TempDir::new("db_query_name");
        tmp.file("Cargo.toml", b"[package]");
        tmp.file("main.rs", b"fn main() {}");
        let db_dir = TempDir::new("db_query_name_db");
        let db_path = index_into(&tmp, &db_dir);
        let hits = DbManager::query_disk_index(
            &db_path,
            &crate::search::ParsedQuery::parse(r#"name:"Cargo""#),
        )
        .expect("query");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Cargo.toml");
    }

    #[test]
    fn refresh_directory_picks_up_new_and_deleted_files() {
        let tmp = TempDir::new("db_refresh");
        tmp.file("keep.txt", b"k");
        tmp.file("gone.txt", b"g");
        let db_dir = TempDir::new("db_refresh_db");
        let db_path = index_into(&tmp, &db_dir);

        std::fs::remove_file(tmp.path().join("gone.txt")).unwrap();
        tmp.file("new.txt", b"n");
        DbManager::refresh_directory(&db_path, tmp.path()).expect("refresh");

        let hits = DbManager::query_disk_index(&db_path, &crate::search::ParsedQuery::parse("txt"))
            .expect("query");
        let mut names: Vec<_> = hits.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["keep.txt", "new.txt"]);
    }

    #[test]
    fn query_disk_index_type_rust_finds_rs_files() {
        let tmp = TempDir::new("db_query_rust");
        tmp.file("src/main.rs", b"fn main() {}");
        tmp.file("src/lib.rs", b"pub fn x() {}");
        tmp.file("readme.md", b"# hi");
        let db_dir = TempDir::new("db_query_rust_db");
        let db_path = index_into(&tmp, &db_dir);

        let hits =
            DbManager::query_disk_index(&db_path, &crate::search::ParsedQuery::parse("type:rust"))
                .expect("query");
        let mut names: Vec<_> = hits.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["lib.rs", "main.rs"]);
        assert!(hits.iter().all(|n| !n.is_dir));
    }

    #[test]
    fn query_disk_index_matches_mixed_case_extensions() {
        let tmp = TempDir::new("db_query_ext");
        tmp.file("Report.PDF", b"%PDF");
        let db_dir = TempDir::new("db_query_ext_db");
        let db_path = index_into(&tmp, &db_dir);

        let hits =
            DbManager::query_disk_index(&db_path, &crate::search::ParsedQuery::parse("type:pdf"))
                .expect("query");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Report.PDF");
        assert!(!hits[0].is_dir);
    }

    #[test]
    fn query_disk_index_treats_extension_wildcards_literally() {
        let tmp = TempDir::new("db_query_ext_wild");
        tmp.file("x.aXb", b"no");
        tmp.file("y.a_b", b"yes");
        let db_dir = TempDir::new("db_query_ext_wild_db");
        let db = index_into(&tmp, &db_dir);
        sync(&db, tmp.path());
        assert_eq!(names_in_index(&db, "ext:a_b"), vec!["y.a_b"]);
    }

    #[test]
    fn query_disk_index_scopes_to_a_subtree() {
        let tmp = TempDir::new("db_query_scope");
        tmp.file("root.txt", b"root");
        tmp.file("sub/nested.txt", b"nest");
        let db_dir = TempDir::new("db_query_scope_db");
        let db_path = index_into(&tmp, &db_dir);

        let mut parsed = crate::search::ParsedQuery::parse("txt");
        parsed.scope_path = Some(tmp.path().join("sub"));
        let hits = DbManager::query_disk_index(&db_path, &parsed).expect("query");
        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert_eq!(hits[0].name, "nested.txt");
    }

    #[test]
    fn delete_from_index_removes_nested_directories_and_files() {
        let tmp = TempDir::new("db_del_tree");
        tmp.file("keep.txt", b"k");
        tmp.file("gone/a.txt", b"a");
        tmp.file("gone/deep/b.txt", b"b");
        let db_dir = TempDir::new("db_del_tree_db");
        let db_path = index_into(&tmp, &db_dir);

        DbManager::delete_from_index(&db_path, &tmp.path().join("gone")).expect("delete");

        let leftovers =
            DbManager::query_disk_index(&db_path, &crate::search::ParsedQuery::parse(""))
                .expect("query");
        let names: Vec<&str> = leftovers.iter().map(|n| n.name.as_str()).collect();
        assert!(
            !names
                .iter()
                .any(|n| *n == "a.txt" || *n == "b.txt" || *n == "gone" || *n == "deep"),
            "nested rows must be gone, got {names:?}"
        );
        assert!(names.contains(&"keep.txt"), "unrelated file must remain");
    }

    #[test]
    fn delete_from_index_decrements_directory_direct_size() {
        let tmp = TempDir::new("db_del_size");
        tmp.file("a.bin", &[0u8; 100]);
        tmp.file("b.bin", &[0u8; 50]);
        let db_dir = TempDir::new("db_del_size_db");
        let db_path = index_into(&tmp, &db_dir);

        let before = DbManager::query_space_hoggers_from_sqlite(&db_path).expect("hoggers");
        let root_size = before
            .iter()
            .find(|(p, _)| p == tmp.path())
            .map(|(_, s)| *s)
            .expect("root in hoggers");
        assert_eq!(root_size, 150);

        DbManager::delete_from_index(&db_path, &tmp.path().join("a.bin")).expect("delete");

        let after = DbManager::query_space_hoggers_from_sqlite(&db_path).expect("hoggers");
        let root_size = after
            .iter()
            .find(|(p, _)| p == tmp.path())
            .map(|(_, s)| *s)
            .expect("root in hoggers");
        assert_eq!(root_size, 50);
    }

    #[test]
    fn cancelled_index_does_not_commit() {
        let tmp = TempDir::new("db_cancel");
        tmp.file("a.txt", b"hello");
        let db_dir = TempDir::new("db_cancel_db");
        let db_path = db_dir.path().join("index.db");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (tx, _rx) = std::sync::mpsc::channel();
        let result =
            DbManager::index_multiple_to_sqlite(&db_path, &[tmp.path().to_path_buf()], cancel, tx);
        assert!(result.is_err(), "cancelled index must not report success");
        // The transaction is rolled back; an empty-or-missing files table is fine,
        // a committed file row is not.
        if db_path.exists() {
            let conn = Connection::open(&db_path).expect("open");
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
                .unwrap_or(0);
            assert_eq!(count, 0, "cancelled rebuild must not commit file rows");
        }
    }

    #[test]
    fn remove_index_files_deletes_wal_and_shm() {
        let tmp = TempDir::new("db_wal");
        let db = tmp.path().join("index.db");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(tmp.path().join("index.db-wal"), b"wal").unwrap();
        std::fs::write(tmp.path().join("index.db-shm"), b"shm").unwrap();
        remove_index_files(&db).expect("remove");
        assert!(!db.exists());
        assert!(!tmp.path().join("index.db-wal").exists());
        assert!(!tmp.path().join("index.db-shm").exists());
    }

    fn names_in_index(db: &Path, query: &str) -> Vec<String> {
        let mut names: Vec<String> =
            DbManager::query_disk_index(db, &crate::search::ParsedQuery::parse(query))
                .expect("query")
                .into_iter()
                .map(|n| n.name)
                .collect();
        names.sort();
        names
    }

    fn sync(db: &Path, root: &Path) {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();
        DbManager::sync_roots(db, &[root.to_path_buf()], &cancel, &tx).expect("sync");
    }

    #[test]
    fn sync_roots_picks_up_files_added_while_offline() {
        let tmp = TempDir::new("sync_add");
        tmp.file("old.txt", b"old");
        let db_dir = TempDir::new("sync_add_db");
        let db = index_into(&tmp, &db_dir);

        tmp.file("new.txt", b"new");
        tmp.file("nested/deep.rs", b"fn x() {}");
        sync(&db, tmp.path());

        let names = names_in_index(&db, "txt");
        assert!(names.contains(&"old.txt".into()), "{names:?}");
        assert!(names.contains(&"new.txt".into()), "{names:?}");
        let rs = names_in_index(&db, "ext:rs");
        assert_eq!(rs, vec!["deep.rs"]);
    }

    #[test]
    fn sync_roots_drops_files_deleted_while_offline() {
        let tmp = TempDir::new("sync_del");
        tmp.file("keep.txt", b"k");
        tmp.file("gone.txt", b"g");
        tmp.file("dir/a.txt", b"a");
        let db_dir = TempDir::new("sync_del_db");
        let db = index_into(&tmp, &db_dir);

        std::fs::remove_file(tmp.path().join("gone.txt")).unwrap();
        std::fs::remove_dir_all(tmp.path().join("dir")).unwrap();
        sync(&db, tmp.path());

        let names = names_in_index(&db, "txt");
        assert_eq!(names, vec!["keep.txt"]);
    }

    #[test]
    fn apply_fs_change_inserts_and_deletes_a_file() {
        let tmp = TempDir::new("apply_fs");
        tmp.file("seed.txt", b"s");
        let db_dir = TempDir::new("apply_fs_db");
        let db = index_into(&tmp, &db_dir);

        let created = tmp.file("live.txt", b"hello");
        DbManager::apply_fs_change(&db, &created).expect("apply create");
        assert!(names_in_index(&db, "live").contains(&"live.txt".into()));

        std::fs::remove_file(&created).unwrap();
        DbManager::apply_fs_change(&db, &created).expect("apply delete");
        assert!(!names_in_index(&db, "live").contains(&"live.txt".into()));
    }

    #[test]
    fn sync_roots_does_not_wipe_unrelated_files() {
        let tmp = TempDir::new("sync_keep");
        tmp.file("a.txt", b"a");
        tmp.file("b.rs", b"b");
        let db_dir = TempDir::new("sync_keep_db");
        let db = index_into(&tmp, &db_dir);
        sync(&db, tmp.path());
        assert_eq!(names_in_index(&db, "ext:rs"), vec!["b.rs"]);
        assert_eq!(names_in_index(&db, "a.txt").len(), 1);
    }

    #[test]
    fn corrupt_file_is_detected_and_quarantined() {
        let tmp = TempDir::new("db_corrupt");
        let db_path = tmp.path().join("index.db");
        std::fs::write(&db_path, vec![0xABu8; 8192]).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (tx, _rx) = std::sync::mpsc::channel();
        let err = DbManager::sync_roots(&db_path, &[tmp.path().to_path_buf()], &cancel, &tx)
            .err()
            .expect("garbage is not a database");
        assert!(is_corruption(err.as_ref()), "got: {err}");

        quarantine_index(&db_path).unwrap();
        assert!(!db_path.exists());
        assert!(tmp.path().join("index.db.corrupt").exists());
        // A rebuild into the vacated path now succeeds.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();
        DbManager::index_multiple_to_sqlite(&db_path, &[tmp.path().to_path_buf()], cancel, tx)
            .expect("rebuild");
    }

    #[test]
    fn locked_database_is_not_treated_as_corruption() {
        let e: Box<dyn std::error::Error + Send + Sync> = Box::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(!is_corruption(e.as_ref()));
    }

    #[test]
    fn old_schema_is_migrated_in_place_without_losing_rows() {
        let tmp = TempDir::new("db_migrate");
        let db_path = tmp.path().join("index.db");
        {
            // The pre-`mtime`/`indexed_at` layout.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE directories (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    parent_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
                    name TEXT NOT NULL, path TEXT NOT NULL,
                    direct_size INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO directories (name, path, direct_size) VALUES ('old', '/old', 7);",
            )
            .unwrap();
        }
        let conn = Connection::open(&db_path).unwrap();
        DbManager::init_db(&conn).expect("migrate");
        DbManager::init_db(&conn).expect("migration is idempotent");
        let (size, mtime): (i64, i64) = conn
            .query_row(
                "SELECT direct_size, mtime FROM directories WHERE path = '/old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((size, mtime), (7, 0));
    }

    #[test]
    fn size_snapshots_round_trip_and_find_the_nearest() {
        let tmp = TempDir::new("db_snap_rw");
        let db_path = tmp.path().join("index.db");
        let mut conn = DbManager::open_history(&db_path).unwrap();
        let a = vec![("/x".to_string(), 100u64), ("/x/y".to_string(), 60)];
        let id_a = DbManager::insert_size_snapshot(&mut conn, 1_000, &a).unwrap();
        let id_b = DbManager::insert_size_snapshot(&mut conn, 5_000, &[]).unwrap();

        assert_eq!(DbManager::size_snapshot_rows(&conn, id_a).unwrap(), a);
        assert!(DbManager::size_snapshot_rows(&conn, id_b)
            .unwrap()
            .is_empty());
        assert_eq!(
            DbManager::size_snapshot_bounds(&conn).unwrap(),
            Some((1_000, 5_000))
        );
        assert_eq!(
            DbManager::nearest_size_snapshot(&conn, 1_900).unwrap(),
            Some((id_a, 1_000))
        );
        assert_eq!(
            DbManager::nearest_size_snapshot(&conn, 4_000).unwrap(),
            Some((id_b, 5_000))
        );
        assert_eq!(
            DbManager::size_snapshot_ids(&conn).unwrap(),
            vec![(id_a, 1_000), (id_b, 5_000)]
        );
    }

    #[test]
    fn empty_history_has_no_bounds_or_nearest() {
        let tmp = TempDir::new("db_snap_empty");
        let conn = DbManager::open_history(&tmp.path().join("index.db")).unwrap();
        assert_eq!(DbManager::size_snapshot_bounds(&conn).unwrap(), None);
        assert_eq!(DbManager::nearest_size_snapshot(&conn, 10).unwrap(), None);
    }

    #[test]
    fn pruning_drops_old_snapshots_and_thins_the_middle_to_weekly() {
        const DAY: i64 = 86_400;
        const WEEK: i64 = 7 * DAY;
        let tmp = TempDir::new("db_snap_prune");
        let mut conn = DbManager::open_history(&tmp.path().join("index.db")).unwrap();
        let now = 100 * WEEK;
        let row = vec![("/big".to_string(), 1u64)];

        // Two snapshots in the same week, both older than 30 days: keep the newer.
        let week_start = (now - 40 * DAY).div_euclid(WEEK) * WEEK;
        let older_in_week =
            DbManager::insert_size_snapshot(&mut conn, week_start + 100, &row).unwrap();
        let newer_in_week =
            DbManager::insert_size_snapshot(&mut conn, week_start + 200, &row).unwrap();
        // Older than 90 days: dropped outright.
        let too_old = DbManager::insert_size_snapshot(&mut conn, now - 100 * DAY, &row).unwrap();
        // Within 30 days: all kept.
        let recent_a = DbManager::insert_size_snapshot(&mut conn, now - 10 * DAY, &row).unwrap();
        let recent_b =
            DbManager::insert_size_snapshot(&mut conn, now - 10 * DAY + 3600, &row).unwrap();

        let removed =
            DbManager::prune_size_snapshots(&mut conn, now, 30 * DAY, 90 * DAY, WEEK).unwrap();
        assert_eq!(removed, 2);
        let left: Vec<i64> = DbManager::size_snapshot_ids(&conn)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(left, vec![newer_in_week, recent_a, recent_b]);
        assert!(!left.contains(&older_in_week) && !left.contains(&too_old));
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM size_snapshot_dirs WHERE snapshot_id NOT IN (SELECT id FROM size_snapshots)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "rows of pruned snapshots are removed too");
    }

    #[test]
    fn old_index_without_snapshot_tables_upgrades_in_place() {
        let tmp = TempDir::new("db_snap_upgrade");
        let db_path = tmp.path().join("index.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE directories (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    parent_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
                    name TEXT NOT NULL, path TEXT NOT NULL,
                    direct_size INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO directories (name, path, direct_size) VALUES ('old', '/old', 7);",
            )
            .unwrap();
        }
        let mut conn = DbManager::open_history(&db_path).expect("upgrade");
        let id =
            DbManager::insert_size_snapshot(&mut conn, 42, &[("/old".into(), 7)]).expect("write");
        assert_eq!(
            DbManager::size_snapshot_rows(&conn, id).unwrap(),
            vec![("/old".to_string(), 7)]
        );
        let size: i64 = conn
            .query_row(
                "SELECT direct_size FROM directories WHERE path = '/old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(size, 7, "existing rows are untouched");
    }

    /// Scale check, run on demand:
    /// `cargo test --release scale_index -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scale_index_one_million_files() {
        let tmp = TempDir::new("db_scale");
        let dirs = 1_000;
        let per_dir = 1_000;
        let t = std::time::Instant::now();
        for d in 0..dirs {
            let dir = tmp
                .path()
                .join(format!("d{:04}/sub", d % 100))
                .join(format!("{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..per_dir {
                std::fs::File::create(dir.join(format!("file_{f}.rs"))).unwrap();
            }
        }
        println!("created {} files in {:?}", dirs * per_dir, t.elapsed());

        let db_path = tmp.path().join("scale.db");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, _rx) = std::sync::mpsc::channel();
        let t = std::time::Instant::now();
        DbManager::index_multiple_to_sqlite(&db_path, &[tmp.path().to_path_buf()], cancel, tx)
            .expect("index");
        println!("indexed in {:?}", t.elapsed());

        let parsed = crate::search::ParsedQuery::parse("ext:rs file_99");
        let t = std::time::Instant::now();
        let hits = DbManager::query_disk_index(&db_path, &parsed).expect("query");
        println!("query: {} hits in {:?}", hits.len(), t.elapsed());
        println!(
            "db size: {} MB",
            std::fs::metadata(&db_path).unwrap().len() / 1_048_576
        );

        let t = std::time::Instant::now();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (tx, _rx) = std::sync::mpsc::channel();
        DbManager::sync_roots(&db_path, &[tmp.path().to_path_buf()], &cancel, &tx).expect("sync");
        println!("no-op resync in {:?}", t.elapsed());
    }

    #[test]
    fn refresh_directory_does_not_stop_sync_from_indexing_new_subfolders() {
        let tmp = TempDir::new("db_refresh_subdir");
        tmp.file("a.txt", b"a");
        let db_dir = TempDir::new("db_refresh_subdir_db");
        let db = index_into(&tmp, &db_dir);

        // A new subfolder changes the parent's mtime. The refresh re-reads only
        // files, so it must leave the stored mtime stale for sync to notice.
        tmp.file("fresh/inner.rs", b"fn x() {}");
        DbManager::refresh_directory(&db, tmp.path()).expect("refresh");
        sync(&db, tmp.path());

        assert_eq!(names_in_index(&db, "ext:rs"), vec!["inner.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folder_keeps_its_indexed_rows() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("db_unreadable");
        tmp.file("locked/keep.txt", b"k");
        tmp.file("open.txt", b"o");
        let db_dir = TempDir::new("db_unreadable_db");
        let db = index_into(&tmp, &db_dir);

        let locked = tmp.path().join("locked");
        let restore = || std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Root, or a filesystem without permission checks: cannot be unreadable.
            restore().unwrap();
            return;
        }

        let refreshed = DbManager::refresh_directory(&db, &locked);
        sync(&db, tmp.path());
        restore().unwrap();

        assert!(refreshed.is_err());
        let names = names_in_index(&db, "txt");
        assert_eq!(names, vec!["keep.txt", "open.txt"], "{names:?}");
    }

    #[test]
    fn escape_like_round_trips_wildcards_and_the_escape_char() {
        let conn = Connection::open_in_memory().unwrap();
        for s in ["plain", "a%b", "a_b", "a!b", "!%_!!", "a\\b"] {
            let matched: bool = conn
                .query_row(
                    "SELECT ?1 LIKE ?2 ESCAPE '!'",
                    rusqlite::params![s, escape_like(s)],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(matched, "{s:?} should match its own escaped pattern");
        }
    }

    #[test]
    fn descendants_pattern_matches_only_strict_descendants() {
        let conn = Connection::open_in_memory().unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        let scope = format!("{sep}data{sep}100%_done!x");
        let cases = [
            (format!("{scope}{sep}a.txt"), true),
            (format!("{scope}{sep}deep{sep}b.txt"), true),
            (scope.clone(), false),
            (format!("{sep}data{sep}100X_done!x{sep}a.txt"), false),
            (format!("{sep}data{sep}100ab_done!x{sep}a.txt"), false),
            (format!("{sep}data{sep}100%_done!x2{sep}a.txt"), false),
            (format!("{scope}!{sep}a"), false),
        ];
        let pattern = descendants_pattern(&scope);
        for (path, expect) in cases {
            let matched: bool = conn
                .query_row(
                    "SELECT ?1 LIKE ?2 ESCAPE '!'",
                    rusqlite::params![path, pattern],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(matched, expect, "{path:?}");
        }
    }
}
