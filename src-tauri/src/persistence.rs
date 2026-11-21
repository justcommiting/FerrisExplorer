use std::path::{Path, PathBuf};

use crate::archive::{self, Location};
use crate::sys;

/// Write via a temp file and rename so a crash or full disk mid-write can't
/// leave a truncated settings file behind.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Move a settings file that does not parse aside as `<name>.corrupt`
/// (replacing an older backup), so the next save can't destroy what it held.
fn move_aside(path: &Path, err: impl std::fmt::Display) {
    log::warn!("{} is unreadable ({err}); moving it aside", path.display());
    let mut moved = path.as_os_str().to_os_string();
    moved.push(".corrupt");
    let moved = PathBuf::from(moved);
    let _ = std::fs::remove_file(&moved);
    if let Err(e) = std::fs::rename(path, &moved) {
        log::error!("Could not move {} aside: {e}", path.display());
    }
}

/// Read a JSON settings file. A missing file is `None`; one that does not
/// parse is moved aside (see `move_aside`) and is also `None`.
fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes)
        .map_err(|e| move_aside(path, e))
        .ok()
}

const MAX_SEARCH_HISTORY: usize = 30;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SavedSearch {
    pub name: String,
    pub query: String,
    /// Pinned searches are listed in the sidebar as smart folders.
    /// Settings written before this field existed load as unpinned.
    #[serde(default)]
    pub pinned: bool,
}

impl SavedSearch {
    pub fn new(name: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            query: query.into(),
            pinned: false,
        }
    }
}

pub struct SavedSearchesStore;

impl SavedSearchesStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_saved_searches.json")
    }

    pub fn defaults() -> Vec<SavedSearch> {
        vec![
            SavedSearch::new("Large files", "size:>1gb"),
            SavedSearch::new("Recently modified", "modified:<24h"),
            SavedSearch::new("Rust files", "ext:rs"),
            SavedSearch::new("TODOs", "content:\"TODO\""),
            SavedSearch::new("Git changes", "git:modified"),
        ]
    }

    pub fn load() -> Vec<SavedSearch> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> Vec<SavedSearch> {
        let Ok(raw) = std::fs::read(path) else {
            return Self::defaults();
        };
        if let Ok(v) = serde_json::from_slice::<Vec<SavedSearch>>(&raw) {
            if v.is_empty() {
                return Self::defaults();
            }
            return v;
        }
        if let Ok(v) = serde_json::from_slice::<Vec<String>>(&raw) {
            return v
                .into_iter()
                .map(|q| SavedSearch::new(q.clone(), q))
                .collect();
        }
        move_aside(path, "not a list of saved searches");
        Self::defaults()
    }

    pub fn save(searches: &[SavedSearch]) {
        if let Ok(json) = serde_json::to_string_pretty(searches) {
            if let Err(e) = write_atomic(&Self::path(), json.as_bytes()) {
                log::warn!("Failed to save searches: {e}");
            }
        }
    }
}

pub struct FavoritesStore;

impl FavoritesStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_favorites.json")
    }

    pub fn load() -> Vec<PathBuf> {
        load_json(&Self::path()).unwrap_or_default()
    }

    pub fn save(paths: &[PathBuf]) {
        if let Ok(json) = serde_json::to_string_pretty(paths) {
            let _ = write_atomic(&Self::path(), json.as_bytes());
        }
    }
}

pub struct SearchHistoryStore;

impl SearchHistoryStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_search_history.json")
    }

    pub fn load() -> Vec<String> {
        load_json(&Self::path()).unwrap_or_default()
    }

    fn save(history: &[String]) {
        if let Ok(json) = serde_json::to_string(history) {
            if let Err(e) = write_atomic(&Self::path(), json.as_bytes()) {
                log::warn!("Failed to save search history: {e}");
            }
        }
    }

    pub fn push(history: &mut Vec<String>, query: String) {
        if query.is_empty() {
            return;
        }
        if let Some(pos) = history.iter().position(|h| *h == query) {
            history.remove(pos);
        }
        history.insert(0, query);
        history.truncate(MAX_SEARCH_HISTORY);
        Self::save(history);
    }
}

const MAX_SHELL_HISTORY: usize = 20;

/// Commands run with Run Command on Selection, newest first.
pub struct ShellHistoryStore;

impl ShellHistoryStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_shell_history.json")
    }

    pub fn load() -> Vec<String> {
        load_json(&Self::path()).unwrap_or_default()
    }

    pub fn push(history: &mut Vec<String>, command: String) {
        remember(history, command, MAX_SHELL_HISTORY);
        if let Ok(json) = serde_json::to_string(history) {
            if let Err(e) = write_atomic(&Self::path(), json.as_bytes()) {
                log::warn!("Failed to save command history: {e}");
            }
        }
    }
}

/// Put `entry` first, dropping any older copy, and keep at most `max` entries.
/// Blank entries are ignored.
fn remember(history: &mut Vec<String>, entry: String, max: usize) {
    if entry.trim().is_empty() {
        return;
    }
    history.retain(|h| *h != entry);
    history.insert(0, entry);
    history.truncate(max);
}

/// Folders kept in the frecency store. The lowest-scoring ones are dropped first.
const FRECENCY_CAP: usize = 500;
/// A visit loses half its weight after this long without a repeat.
const FRECENCY_HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;
/// Entries that have decayed below this weight are dropped on the next visit.
const FRECENCY_FLOOR: f64 = 0.01;
/// Most folders the jump list returns.
const JUMP_LIMIT: usize = 50;

/// Seconds since the Unix epoch, for visit times.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn one() -> f64 {
    1.0
}

/// A folder the user has visited. `count` is decayed to the time of `last`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrecencyEntry {
    pub path: String,
    #[serde(default = "one")]
    pub count: f64,
    /// Unix seconds of the last visit.
    #[serde(default)]
    pub last: u64,
}

impl FrecencyEntry {
    /// Visit weight as of `now`: frequency multiplied by a half-life decay.
    pub fn score(&self, now: u64) -> f64 {
        let age = now.saturating_sub(self.last) as f64;
        self.count * 0.5f64.powf(age / FRECENCY_HALF_LIFE_SECS)
    }
}

/// Note a visit to `path` at `now`. Decays the list and keeps it under the cap.
pub fn record_visit(entries: &mut Vec<FrecencyEntry>, path: &str, now: u64) {
    match entries.iter_mut().find(|e| e.path == path) {
        Some(e) => {
            e.count = e.score(now) + 1.0;
            e.last = now;
        }
        None => entries.push(FrecencyEntry {
            path: path.to_string(),
            count: 1.0,
            last: now,
        }),
    }
    entries.retain(|e| e.score(now) >= FRECENCY_FLOOR);
    if entries.len() > FRECENCY_CAP {
        entries.sort_by(|a, b| {
            b.score(now)
                .total_cmp(&a.score(now))
                .then_with(|| a.path.cmp(&b.path))
        });
        entries.truncate(FRECENCY_CAP);
    }
}

/// Read the frecency file; `None` if it does not parse. A file written before
/// entries had counts or times is read as one visit each.
pub fn parse_frecency(raw: &[u8]) -> Option<Vec<FrecencyEntry>> {
    if let Ok(v) = serde_json::from_slice::<Vec<FrecencyEntry>>(raw) {
        return Some(v);
    }
    let paths = serde_json::from_slice::<Vec<String>>(raw).ok()?;
    Some(
        paths
            .into_iter()
            .map(|path| FrecencyEntry {
                path,
                count: 1.0,
                last: 0,
            })
            .collect(),
    )
}

/// zoxide-style match: each fragment must appear in the path, in order, and
/// the last one must appear in the final path component. Case-insensitive.
pub fn frecency_matches(path: &str, fragments: &[String]) -> bool {
    let lower = path.to_lowercase();
    let base_start = lower.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let mut pos = 0;
    for (i, frag) in fragments.iter().enumerate() {
        let from = if i + 1 == fragments.len() {
            pos.max(base_start)
        } else {
            pos
        };
        match lower.get(from..).and_then(|rest| rest.find(frag.as_str())) {
            Some(at) => pos = from + at + frag.len(),
            None => return false,
        }
    }
    true
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct JumpHit {
    pub path: String,
    pub score: f64,
}

/// Folders matching `query`, best first. An empty query lists everything.
pub fn rank_jumps(entries: &[FrecencyEntry], query: &str, now: u64) -> Vec<JumpHit> {
    let fragments: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<JumpHit> = entries
        .iter()
        .filter(|e| frecency_matches(&e.path, &fragments))
        .map(|e| JumpHit {
            path: e.path.clone(),
            score: e.score(now),
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    hits.truncate(JUMP_LIMIT);
    hits
}

pub struct FrecencyStore;

impl FrecencyStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_frecency.json")
    }

    pub fn load() -> Vec<FrecencyEntry> {
        let path = Self::path();
        let Ok(raw) = std::fs::read(&path) else {
            return Vec::new();
        };
        parse_frecency(&raw).unwrap_or_else(|| {
            move_aside(&path, "not a list of folder visits");
            Vec::new()
        })
    }

    pub fn record(path: &str, now: u64) {
        let mut entries = Self::load();
        record_visit(&mut entries, path, now);
        if let Ok(json) = serde_json::to_string(&entries) {
            if let Err(e) = write_atomic(&Self::path(), json.as_bytes()) {
                log::warn!("Failed to save folder visits: {e}");
            }
        }
    }
}

/// Keyboard preferences. Missing or unreadable settings use the defaults.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KeyPrefs {
    #[serde(default)]
    pub vim_mode: bool,
}

pub struct KeyPrefsStore;

impl KeyPrefsStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_keys.json")
    }

    pub fn load() -> KeyPrefs {
        load_json(&Self::path()).unwrap_or_default()
    }

    pub fn save(prefs: &KeyPrefs) {
        if let Ok(json) = serde_json::to_string_pretty(prefs) {
            if let Err(e) = write_atomic(&Self::path(), json.as_bytes()) {
                log::warn!("Failed to save key preferences: {e}");
            }
        }
    }
}

pub struct IndexStatus;

impl IndexStatus {
    pub fn db_path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_index.db")
    }

    pub fn compute() -> String {
        let db = Self::db_path();
        if db.exists() {
            if let Ok(meta) = std::fs::metadata(&db) {
                let kb = meta.len() / 1024;
                if kb == 0 {
                    "Index ✓".to_string()
                } else {
                    format!("Index ✓ ({} KB)", kb)
                }
            } else {
                "Index ✓".to_string()
            }
        } else {
            "Not indexed".to_string()
        }
    }
}

/// The hidden workspace that holds the session autosaved for "Restore tabs on
/// launch". It is never listed, and no user workspace may take the name.
pub const LAST_SESSION: &str = "__last__";
const MAX_WORKSPACE_NAME: usize = 100;

/// One tab of a workspace. Every field defaults, so files written by older or
/// newer versions still load.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WorkspaceTab {
    /// A folder or archive path, or a smart folder reference (`smart:` then JSON).
    pub path: String,
    /// "list" or "columns". Empty, as in files from before the field existed, means list.
    pub view: String,
    /// "name", "size", "modified" or "kind", with "asc" or "desc" as the direction.
    pub sort_col: String,
    pub sort_dir: String,
    /// Columns view: the folders opened after `path`.
    pub col_path: Vec<String>,
    /// Names of the selected entries in this folder.
    pub selected: Vec<String>,
    /// True for the tab shown in the split pane.
    pub split: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub name: String,
    /// Tabs in tab-bar order, the split pane's tab included.
    pub tabs: Vec<WorkspaceTab>,
    /// Index into `tabs` of the active tab.
    pub active: usize,
    /// True when the split pane had focus.
    pub focused_split: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WorkspacesFile {
    pub restore_on_launch: bool,
    pub workspaces: Vec<Workspace>,
}

/// The name as it will be stored: trimmed, non-empty, short, and not the reserved name.
pub fn checked_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the workspace a name".into());
    }
    if name.chars().count() > MAX_WORKSPACE_NAME {
        return Err(format!(
            "Keep the name under {MAX_WORKSPACE_NAME} characters"
        ));
    }
    if name.eq_ignore_ascii_case(LAST_SESSION) {
        return Err("That name is reserved".into());
    }
    Ok(name.to_string())
}

/// Names collide ignoring case and surrounding spaces, so "Work" and "work" are one workspace.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Store `ws`, replacing the workspace with the same name. Returns true when one was replaced.
pub fn upsert(list: &mut Vec<Workspace>, ws: Workspace) -> bool {
    match list.iter_mut().find(|w| same_name(&w.name, &ws.name)) {
        Some(slot) => {
            *slot = ws;
            true
        }
        None => {
            list.push(ws);
            false
        }
    }
}

/// Drop the workspace with this name. Returns true when one was removed.
pub fn remove(list: &mut Vec<Workspace>, name: &str) -> bool {
    let before = list.len();
    list.retain(|w| !same_name(&w.name, name));
    list.len() != before
}

/// The workspaces the user sees: everything except the hidden last session.
pub fn listed(list: &[Workspace]) -> Vec<Workspace> {
    list.iter()
        .filter(|w| !same_name(&w.name, LAST_SESSION))
        .cloned()
        .collect()
}

pub fn last_session(list: &[Workspace]) -> Option<Workspace> {
    list.iter()
        .find(|w| same_name(&w.name, LAST_SESSION))
        .cloned()
}

/// Serialises read-modify-write cycles on the workspaces file, so an autosave
/// and a user's save can't overwrite each other.
static WORKSPACES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct WorkspacesStore;

impl WorkspacesStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_workspaces.json")
    }

    pub fn load() -> WorkspacesFile {
        Self::load_from(&Self::path())
    }

    /// A missing file is empty. A file that does not parse is moved aside as
    /// `<name>.corrupt` and read as empty, so the evidence is kept.
    pub fn load_from(path: &Path) -> WorkspacesFile {
        load_json(path).unwrap_or_default()
    }

    /// Apply `change` to the stored file and write it back.
    pub fn modify<T>(change: impl FnOnce(&mut WorkspacesFile) -> T) -> std::io::Result<T> {
        Self::modify_at(&Self::path(), change)
    }

    fn modify_at<T>(
        path: &Path,
        change: impl FnOnce(&mut WorkspacesFile) -> T,
    ) -> std::io::Result<T> {
        let _guard = WORKSPACES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = Self::load_from(path);
        let out = change(&mut file);
        let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        write_atomic(path, &json)?;
        Ok(out)
    }
}

/// The folder to open for a saved path that may have moved or been deleted: the
/// path itself if it still exists, else its nearest ancestor that does. An
/// archive path (`archive.zip/inner`) exists while its entry is in the archive.
/// Plugin manifests the user has enabled: file name to the SHA-256 of the
/// contents that were enabled. A file whose contents no longer match is
/// disabled again.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginTrust {
    #[serde(default)]
    pub enabled: std::collections::BTreeMap<String, String>,
}

pub struct PluginTrustStore;

impl PluginTrustStore {
    fn path() -> PathBuf {
        sys::get_home_dir().join(".ferris_explorer_plugins.json")
    }

    pub fn load() -> PluginTrust {
        Self::load_from(&Self::path())
    }

    /// A missing or unreadable file means nothing is enabled.
    pub fn load_from(path: &Path) -> PluginTrust {
        load_json(path).unwrap_or_default()
    }

    pub fn save(trust: &PluginTrust) -> std::io::Result<()> {
        Self::save_to(&Self::path(), trust)
    }

    pub fn save_to(path: &Path, trust: &PluginTrust) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(trust).map_err(std::io::Error::other)?;
        write_atomic(path, json.as_bytes())
    }
}

pub fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| !p.as_os_str().is_empty() && (p.is_dir() || archive_entry_exists(p)))
        .map(Path::to_path_buf)
}

fn archive_entry_exists(path: &Path) -> bool {
    let Some(loc) = archive::locate(path) else {
        return false;
    };
    if loc.inner.is_empty() {
        // The archive file itself: browsing it shows its root.
        return true;
    }
    let (parent, name) = loc
        .inner
        .rsplit_once('/')
        .unwrap_or(("", loc.inner.as_str()));
    let parent = Location {
        archive: loc.archive.clone(),
        inner: parent.to_string(),
    };
    archive::list(&parent, archive::LIMITS)
        .map(|rows| rows.iter().any(|row| row.name == name && row.is_dir))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_history_is_newest_first_unique_and_capped() {
        let mut h = Vec::new();
        for i in 0..25 {
            remember(&mut h, format!("cmd {i}"), MAX_SHELL_HISTORY);
        }
        assert_eq!(h.len(), MAX_SHELL_HISTORY);
        assert_eq!(h[0], "cmd 24");
        assert_eq!(h[19], "cmd 5");

        // Running an old command again moves it to the front, without a duplicate.
        remember(&mut h, "cmd 10".into(), MAX_SHELL_HISTORY);
        assert_eq!(h[0], "cmd 10");
        assert_eq!(h.iter().filter(|c| *c == "cmd 10").count(), 1);
        assert_eq!(h.len(), MAX_SHELL_HISTORY);

        remember(&mut h, "   ".into(), MAX_SHELL_HISTORY);
        assert_eq!(h[0], "cmd 10", "blank entries are ignored");
    }

    #[test]
    fn saved_search_defaults_cover_the_headline_queries() {
        let d = SavedSearchesStore::defaults();
        assert!(d.iter().any(|s| s.query == "size:>1gb"));
        assert!(d.iter().any(|s| s.query == "ext:rs"));
        assert!(d.iter().any(|s| s.query.contains("TODO")));
        assert!(d.iter().any(|s| s.query == "git:modified"));
    }

    #[test]
    fn saved_search_from_before_pinning_loads_unpinned() {
        let old: Vec<SavedSearch> =
            serde_json::from_str(r#"[{"name":"Rust files","query":"ext:rs"}]"#).unwrap();
        assert_eq!(old, vec![SavedSearch::new("Rust files", "ext:rs")]);
        assert!(!old[0].pinned);
    }

    #[test]
    fn pinned_saved_search_round_trips() {
        let mut s = SavedSearch::new("Big", "size:>1gb");
        s.pinned = true;
        let json = serde_json::to_string(&[s.clone()]).unwrap();
        let back: Vec<SavedSearch> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![s]);
    }

    const DAY: u64 = 24 * 3600;

    fn frags(q: &str) -> Vec<String> {
        q.split_whitespace().map(str::to_lowercase).collect()
    }

    #[test]
    fn frecency_records_a_visit_once_per_folder_and_counts_repeats() {
        let mut e = Vec::new();
        record_visit(&mut e, "/a", 1000);
        record_visit(&mut e, "/b", 1000);
        record_visit(&mut e, "/a", 1000);
        assert_eq!(e.len(), 2);
        let a = e.iter().find(|x| x.path == "/a").unwrap();
        assert!((a.count - 2.0).abs() < 1e-9);
        assert_eq!(a.last, 1000);
    }

    #[test]
    fn frecency_ranks_frequent_and_recent_folders_first() {
        let mut e = Vec::new();
        for _ in 0..5 {
            record_visit(&mut e, "/old-but-busy", 1000);
        }
        record_visit(&mut e, "/once-just-now", 1000 + 10 * DAY);
        // Five visits decayed over ten days (about 1.9) still beat one fresh visit (1.0).
        let hits = rank_jumps(&e, "", 1000 + 10 * DAY);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "/old-but-busy");
        assert!(hits[0].score > hits[1].score);
        // With equal counts, the more recent visit ranks first.
        let even = vec![
            FrecencyEntry {
                path: "/older".into(),
                count: 2.0,
                last: 0,
            },
            FrecencyEntry {
                path: "/newer".into(),
                count: 2.0,
                last: DAY,
            },
        ];
        assert_eq!(rank_jumps(&even, "", DAY)[0].path, "/newer");
    }

    #[test]
    fn frecency_score_halves_each_half_life() {
        let e = FrecencyEntry {
            path: "/x".into(),
            count: 4.0,
            last: 0,
        };
        let half = FRECENCY_HALF_LIFE_SECS as u64;
        assert!((e.score(0) - 4.0).abs() < 1e-9);
        assert!((e.score(half) - 2.0).abs() < 1e-9);
        assert!((e.score(2 * half) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn frecency_drops_entries_that_have_decayed_away() {
        let mut e = vec![FrecencyEntry {
            path: "/stale".into(),
            count: 1.0,
            last: 0,
        }];
        record_visit(&mut e, "/new", 400 * DAY);
        assert_eq!(
            e.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
            vec!["/new"]
        );
    }

    #[test]
    fn frecency_is_capped_and_keeps_the_best_scores() {
        let mut e = Vec::new();
        for i in 0..FRECENCY_CAP + 20 {
            record_visit(&mut e, &format!("/d{i}"), 1000 + i as u64);
        }
        assert_eq!(e.len(), FRECENCY_CAP);
        // The newest visits outscore the oldest ones that were dropped.
        assert!(e
            .iter()
            .any(|x| x.path == format!("/d{}", FRECENCY_CAP + 19)));
        assert!(!e.iter().any(|x| x.path == "/d0"));
    }

    #[test]
    fn missing_or_garbage_frecency_file_is_empty() {
        assert!(parse_frecency(b"").is_none());
        assert!(parse_frecency(b"{not json").is_none());
        assert_eq!(parse_frecency(b"[]"), Some(vec![]));
    }

    #[test]
    fn unparsable_saved_searches_are_kept_as_backup() {
        let tmp = crate::testutil::TempDir::new("ss_corrupt");
        let path = tmp.file("searches.json", b"{ not json");
        assert_eq!(
            SavedSearchesStore::load_from(&path),
            SavedSearchesStore::defaults()
        );
        assert!(!path.exists());
        let backup = tmp.path().join("searches.json.corrupt");
        assert_eq!(std::fs::read(backup).unwrap(), b"{ not json");

        // The old format of bare queries still loads and is left in place.
        std::fs::write(&path, br#"["ext:rs"]"#).unwrap();
        assert_eq!(SavedSearchesStore::load_from(&path)[0].query, "ext:rs");
        assert!(path.exists());
    }

    #[test]
    fn old_frecency_file_of_bare_paths_loads_as_one_visit_each() {
        let e = parse_frecency(br#"["/a","/b"]"#).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(
            e[0],
            FrecencyEntry {
                path: "/a".into(),
                count: 1.0,
                last: 0
            }
        );
    }

    #[test]
    fn frecency_entry_missing_fields_takes_defaults() {
        let e = parse_frecency(br#"[{"path":"/a"}]"#).unwrap();
        assert_eq!(
            e,
            vec![FrecencyEntry {
                path: "/a".into(),
                count: 1.0,
                last: 0
            }]
        );
    }

    #[test]
    fn jump_matches_fragments_in_order_with_last_in_the_folder_name() {
        let p = "/Users/x/projects/FerrisExplorer";
        assert!(frecency_matches(p, &frags("proj fer")));
        assert!(frecency_matches(p, &frags("users projects ferris")));
        assert!(frecency_matches(p, &frags("ferris")));
        assert!(!frecency_matches(p, &frags("fer proj")), "order matters");
        assert!(!frecency_matches(p, &frags("proj zz")));
        // The last fragment must be in the folder name, not a parent.
        assert!(!frecency_matches(p, &frags("proj")));
        assert!(!frecency_matches(p, &frags("users zz")));
        assert!(frecency_matches(p, &frags("")));
    }

    #[test]
    fn jump_query_is_case_insensitive_and_ignores_extra_spaces() {
        let p = "/Users/x/projects/FerrisExplorer";
        assert!(frecency_matches(p, &frags("  PROJ   FER ")));
    }

    #[test]
    fn rank_jumps_filters_then_orders_by_score() {
        let e = vec![
            FrecencyEntry {
                path: "/a/work/proj-one".into(),
                count: 1.0,
                last: 100,
            },
            FrecencyEntry {
                path: "/a/work/proj-two".into(),
                count: 3.0,
                last: 100,
            },
            FrecencyEntry {
                path: "/a/music".into(),
                count: 9.0,
                last: 100,
            },
        ];
        let hits = rank_jumps(&e, "proj", 100);
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["/a/work/proj-two", "/a/work/proj-one"]);
    }

    #[test]
    fn key_prefs_default_to_vim_off_when_missing_or_old() {
        assert!(!KeyPrefs::default().vim_mode);
        assert_eq!(
            serde_json::from_str::<KeyPrefs>("{}").unwrap(),
            KeyPrefs::default()
        );
        assert!(
            serde_json::from_str::<KeyPrefs>(r#"{"vim_mode":true}"#)
                .unwrap()
                .vim_mode
        );
    }

    fn ws(name: &str, paths: &[&str]) -> Workspace {
        Workspace {
            name: name.into(),
            tabs: paths
                .iter()
                .map(|p| WorkspaceTab {
                    path: (*p).into(),
                    view: "list".into(),
                    sort_col: "name".into(),
                    sort_dir: "asc".into(),
                    ..Default::default()
                })
                .collect(),
            active: 0,
            focused_split: false,
        }
    }

    #[test]
    fn workspaces_file_round_trips_through_disk() {
        let tmp = crate::testutil::TempDir::new("ws_round_trip");
        let path = tmp.path().join("workspaces.json");
        let split = WorkspaceTab {
            path: "/b".into(),
            view: "columns".into(),
            col_path: vec!["/b/sub".into()],
            selected: vec!["notes.txt".into()],
            split: true,
            ..Default::default()
        };
        let saved = Workspace {
            name: "Dev".into(),
            tabs: vec![
                WorkspaceTab {
                    path: "/a".into(),
                    ..Default::default()
                },
                split,
            ],
            active: 0,
            focused_split: true,
        };
        WorkspacesStore::modify_at(&path, |f| {
            f.restore_on_launch = true;
            upsert(&mut f.workspaces, saved.clone());
        })
        .unwrap();
        let back = WorkspacesStore::load_from(&path);
        assert!(back.restore_on_launch);
        assert_eq!(back.workspaces, vec![saved]);
    }

    #[test]
    fn missing_fields_take_serde_defaults() {
        let f: WorkspacesFile = serde_json::from_str(r#"{"workspaces":[{"name":"Old"}]}"#).unwrap();
        assert!(!f.restore_on_launch);
        assert_eq!(f.workspaces[0].tabs, Vec::new());
        assert_eq!(f.workspaces[0].active, 0);
        assert!(!f.workspaces[0].focused_split);

        let tab: WorkspaceTab = serde_json::from_str(r#"{"path":"/a"}"#).unwrap();
        assert_eq!(tab.path, "/a");
        assert_eq!(tab.view, "");
        assert!(tab.selected.is_empty() && !tab.split);

        let empty: WorkspacesFile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, WorkspacesFile::default());
    }

    #[test]
    fn unknown_fields_from_a_newer_version_are_ignored() {
        let f: WorkspacesFile =
            serde_json::from_str(r#"{"version":9,"workspaces":[{"name":"New","colour":"red"}]}"#)
                .unwrap();
        assert_eq!(f.workspaces[0].name, "New");
    }

    #[test]
    fn corrupt_workspaces_file_loads_empty_and_is_kept_as_backup() {
        let tmp = crate::testutil::TempDir::new("ws_corrupt");
        let path = tmp.file("workspaces.json", b"{ not json");
        assert_eq!(WorkspacesStore::load_from(&path), WorkspacesFile::default());
        assert!(!path.exists(), "the damaged file is moved aside");
        let backup = tmp.path().join("workspaces.json.corrupt");
        assert_eq!(std::fs::read(&backup).unwrap(), b"{ not json");

        // A second damaged file replaces the older backup rather than failing.
        std::fs::write(&path, b"[]]").unwrap();
        assert_eq!(WorkspacesStore::load_from(&path), WorkspacesFile::default());
        assert_eq!(std::fs::read(&backup).unwrap(), b"[]]");
    }

    #[test]
    fn missing_workspaces_file_is_empty_without_a_backup() {
        let tmp = crate::testutil::TempDir::new("ws_missing");
        let path = tmp.path().join("workspaces.json");
        assert_eq!(WorkspacesStore::load_from(&path), WorkspacesFile::default());
        assert!(!tmp.path().join("workspaces.json.corrupt").exists());
    }

    #[test]
    fn saving_the_same_name_replaces_it_ignoring_case_and_spaces() {
        let mut list = vec![ws("Work", &["/a"])];
        assert!(upsert(&mut list, ws(" work ", &["/b"])));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, " work ");
        assert_eq!(list[0].tabs[0].path, "/b");

        assert!(!upsert(&mut list, ws("Play", &["/c"])));
        assert_eq!(list.len(), 2);

        assert!(remove(&mut list, "PLAY"));
        assert!(!remove(&mut list, "PLAY"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn checked_name_trims_and_refuses_blank_long_or_reserved_names() {
        assert_eq!(checked_name("  Dev  ").unwrap(), "Dev");
        assert!(checked_name("   ").is_err());
        assert!(checked_name(&"x".repeat(MAX_WORKSPACE_NAME + 1)).is_err());
        assert!(checked_name(LAST_SESSION).is_err());
        assert!(
            checked_name("__Last__").is_err(),
            "reserved names ignore case"
        );
    }

    #[test]
    fn last_session_is_stored_but_never_listed() {
        let mut list = vec![ws("Dev", &["/a"]), ws(LAST_SESSION, &["/b"])];
        let shown = listed(&list);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name, "Dev");
        assert_eq!(last_session(&list).unwrap().tabs[0].path, "/b");

        // The autosave overwrites the hidden copy without touching the others.
        upsert(&mut list, ws(LAST_SESSION, &["/c"]));
        assert_eq!(list.len(), 2);
        assert_eq!(last_session(&list).unwrap().tabs[0].path, "/c");
        assert_eq!(last_session(&[]), None);
    }

    #[test]
    fn nearest_existing_dir_keeps_existing_paths_and_climbs_to_the_closest_one() {
        let tmp = crate::testutil::TempDir::new("nearest_dir");
        let folder = tmp.dir("a/b");
        assert_eq!(nearest_existing_dir(&folder), Some(folder.clone()));

        let gone = folder.join("c/d");
        assert_eq!(nearest_existing_dir(&gone), Some(folder.clone()));

        let file = tmp.file("a/note.txt", b"x");
        assert_eq!(
            nearest_existing_dir(&file.join("x")),
            Some(folder.parent().unwrap().to_path_buf())
        );
    }

    #[test]
    fn nearest_existing_dir_handles_archive_paths() {
        let tmp = crate::testutil::TempDir::new("nearest_archive");
        let zip_path = tmp.path().join("bundle.zip");
        {
            use std::io::Write;
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.add_directory("docs/", opts).unwrap();
            zip.start_file("docs/a.txt", opts).unwrap();
            zip.write_all(b"hi").unwrap();
            zip.finish().unwrap();
        }
        let inside = zip_path.join("docs");
        assert_eq!(nearest_existing_dir(&inside), Some(inside.clone()));
        // The nearest entry that still exists wins: a removed child of docs falls back to docs,
        // and a removed top-level folder falls back to the archive root.
        assert_eq!(
            nearest_existing_dir(&inside.join("gone")),
            Some(inside.clone())
        );
        assert_eq!(
            nearest_existing_dir(&zip_path.join("nope")),
            Some(zip_path.clone())
        );
        // A missing archive falls back to the folder that held it.
        let missing = tmp.path().join("lost.zip").join("docs");
        assert_eq!(
            nearest_existing_dir(&missing),
            Some(tmp.path().to_path_buf())
        );
    }
}
