//! Compare two folders, and sync one into the other.
//!
//! A comparison walks both roots without following symlinks and classifies
//! every relative path. A sync is planned from per-path picks, summarised,
//! then run. Each action re-checks what the comparison saw before it touches
//! the disk, and anything replaced or removed goes to the Trash first, so
//! every change can be undone from there.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crate::archive;
use crate::desktop;
use crate::duplicates;
use crate::ops;
use crate::recycle::{self, Mode, Trasher};

/// Why a path is left alone when it changed after the comparison.
const CHANGED: &str = "changed since the comparison";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompareBy {
    /// Size and modification time.
    Metadata,
    /// SHA-256 of the bytes, for files whose sizes match.
    Contents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
}

/// What a path looked like on one side. Folders have size 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    /// Whole seconds since the epoch, for display.
    pub modified_at: u64,
    /// Full precision, so a change since the comparison is always seen.
    #[serde(skip)]
    pub mtime_ns: u128,
}

impl Stat {
    /// Describe a regular file or folder. Symlinks and other kinds give None.
    fn of(meta: &fs::Metadata) -> Option<Stat> {
        let kind = if meta.is_dir() {
            Kind::Dir
        } else if meta.is_file() {
            Kind::File
        } else {
            return None;
        };
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Some(Stat {
            kind,
            size: if kind == Kind::File { meta.len() } else { 0 },
            modified_at: (mtime_ns / 1_000_000_000) as u64,
            mtime_ns,
        })
    }
}

/// How the two sides of one relative path differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    OnlyLeft,
    OnlyRight,
    Identical,
    /// Both sides are files and the left one was modified more recently.
    NewerLeft,
    NewerRight,
    /// Same modification time, different sizes.
    SameTimeSizeDiffers,
    /// Same modification time, and the bytes differ (contents mode).
    ContentDiffers,
    /// A file on one side and a folder on the other.
    TypeConflict,
}

impl Status {
    fn is_diff(self) -> bool {
        self != Status::Identical
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Entry {
    pub rel: String,
    pub status: Status,
    pub left: Option<Stat>,
    pub right: Option<Stat>,
}

/// A path that was not compared, and why.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Skip {
    pub rel: String,
    pub side: Side,
    pub reason: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Comparison {
    pub id: u64,
    pub left: PathBuf,
    pub right: PathBuf,
    pub by: CompareBy,
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skip>,
    /// Everything each side held, for checking a folder before it is trashed.
    #[serde(skip)]
    trees: Arc<Trees>,
}

pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Both roots, as canonical folders. Refused, with the reason, when a root is
/// missing, is a symlink, is inside an archive, or overlaps the other root.
pub fn check_roots(left: &Path, right: &Path) -> Result<(PathBuf, PathBuf), String> {
    archive::ensure_writable(&[left, right])?;
    let left = real_folder(left)?;
    let right = real_folder(right)?;
    if left == right {
        return Err("Both sides are the same folder".into());
    }
    if left.starts_with(&right) || right.starts_with(&left) {
        return Err(
            "One folder is inside the other. Choose two folders that do not overlap.".into(),
        );
    }
    Ok((left, right))
}

fn real_folder(path: &Path) -> Result<PathBuf, String> {
    // symlink_metadata does not follow, so a symlink to a folder is refused here.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::canonicalize(path).map_err(|e| e.to_string()),
        _ => Err(format!("{} is not a folder", path.display())),
    }
}

/// A relative path from a comparison or a request. It must name something
/// strictly below a root: no absolute paths, no `..`, nothing empty.
pub fn validate_rel(rel: &str) -> Result<(), String> {
    let refuse = || Err(format!("{rel:?} is not a path inside the folders"));
    // `Path::components` drops interior `.` segments, so those are checked here.
    if rel.is_empty()
        || rel.contains('\0')
        || rel
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return refuse();
    }
    let mut parts = 0;
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(_) => parts += 1,
            _ => return refuse(),
        }
    }
    if parts == 0 {
        return refuse();
    }
    Ok(())
}

/// Classify one path that exists on both sides. `same_contents` is only given
/// in contents mode, for two files: Some(true) or Some(false) after the bytes
/// were compared. None means compare by size and modification time.
pub fn classify(left: Stat, right: Stat, same_contents: Option<bool>) -> Status {
    if left.kind != right.kind {
        return Status::TypeConflict;
    }
    if left.kind == Kind::Dir {
        return Status::Identical;
    }
    let newer = match left.mtime_ns.cmp(&right.mtime_ns) {
        std::cmp::Ordering::Greater => Some(Status::NewerLeft),
        std::cmp::Ordering::Less => Some(Status::NewerRight),
        std::cmp::Ordering::Equal => None,
    };
    match same_contents {
        Some(true) => Status::Identical,
        Some(false) => newer.unwrap_or(Status::ContentDiffers),
        None => match newer {
            Some(status) => status,
            None if left.size == right.size => Status::Identical,
            None => Status::SameTimeSizeDiffers,
        },
    }
}

#[derive(Clone, Debug, Default)]
struct Walked {
    files: BTreeMap<String, Stat>,
    skipped: Vec<Skip>,
}

/// Both sides as the comparison walked them, so a folder can be checked
/// against what was seen under it before anything is trashed.
#[derive(Clone, Debug, Default)]
struct Trees {
    left: Walked,
    right: Walked,
}

impl Trees {
    fn of(&self, side: Side) -> &Walked {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }
}

/// Record a problem inside `prefix`. The folder is then left out of the
/// comparison as a whole, so nothing under it is judged. The root itself
/// cannot be left out, so a failure there is an error.
fn unreadable(out: &mut Walked, prefix: &str, side: Side, reason: String) -> Result<(), String> {
    if prefix.is_empty() {
        return Err(reason);
    }
    out.skipped.push(Skip {
        rel: prefix.to_string(),
        side,
        reason,
    });
    Ok(())
}

/// Every file and folder under `root`, keyed by relative path. Symlinks are
/// listed as skipped and never entered. None if cancelled.
fn walk(
    root: &Path,
    side: Side,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<Option<Walked>, String> {
    let mut out = Walked::default();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                unreadable(
                    &mut out,
                    &prefix,
                    side,
                    format!("could not read the folder: {e}"),
                )?;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    unreadable(
                        &mut out,
                        &prefix,
                        side,
                        format!("could not list the folder: {e}"),
                    )?;
                    break;
                }
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                unreadable(&mut out, &prefix, side, "a name is not valid UTF-8".into())?;
                break;
            };
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = dir.join(&name);
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) => {
                    out.skipped.push(Skip {
                        rel,
                        side,
                        reason: format!("could not read it: {e}"),
                    });
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                out.skipped.push(Skip {
                    rel,
                    side,
                    reason: "symlink, not followed".into(),
                });
                continue;
            }
            match Stat::of(&meta) {
                None => out.skipped.push(Skip {
                    rel,
                    side,
                    reason: "not a regular file or folder".into(),
                }),
                Some(stat) => {
                    out.files.insert(rel.clone(), stat);
                    if stat.kind == Kind::Dir {
                        stack.push((path, rel));
                    }
                }
            }
            progress(out.files.len() as u64);
        }
    }
    Ok(Some(out))
}

/// True when `rel` or one of the folders above it was skipped.
fn is_blocked(rel: &str, blocked: &HashSet<String>) -> bool {
    let mut prefix = rel;
    loop {
        if blocked.contains(prefix) {
            return true;
        }
        match prefix.rfind('/') {
            Some(i) => prefix = &rel[..i],
            None => return false,
        }
    }
}

/// The folders above a relative path, nearest first.
fn ancestors(rel: &str) -> impl Iterator<Item = &str> + '_ {
    let mut cur = rel;
    std::iter::from_fn(move || {
        let i = cur.rfind('/')?;
        cur = &rel[..i];
        Some(cur)
    })
}

/// Only files of both sides are read, and only when their sizes match: a
/// size mismatch already settles the bytes.
fn contents_pair(a: Stat, b: Stat) -> bool {
    a.kind == Kind::File && b.kind == Kind::File
}

fn contents_match(left: &Path, right: &Path, cancel: &AtomicBool) -> Result<bool, (Side, String)> {
    let a = duplicates::whole_file_digest(left, cancel).ok_or((
        Side::Left,
        "could not be read to compare contents".to_string(),
    ))?;
    let b = duplicates::whole_file_digest(right, cancel).ok_or((
        Side::Right,
        "could not be read to compare contents".to_string(),
    ))?;
    Ok(a == b)
}

/// Compare the two folders. Returns None if cancelled. `progress` is called
/// as ("scan", entries seen, 0) while walking and ("hash", done, total) while
/// comparing contents.
pub fn compare(
    id: u64,
    left: &Path,
    right: &Path,
    by: CompareBy,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<Option<Comparison>, String> {
    let (left, right) = check_roots(left, right)?;
    let Some(l) = walk(&left, Side::Left, cancel, &mut |n| progress("scan", n, 0))? else {
        return Ok(None);
    };
    let Some(r) = walk(&right, Side::Right, cancel, &mut |n| progress("scan", n, 0))? else {
        return Ok(None);
    };

    let mut skipped = l.skipped.clone();
    skipped.extend(r.skipped.clone());
    let blocked: HashSet<String> = skipped.iter().map(|s| s.rel.clone()).collect();

    let all: BTreeSet<&str> = l
        .files
        .keys()
        .chain(r.files.keys())
        .map(String::as_str)
        .collect();
    let rows: Vec<(&str, Option<Stat>, Option<Stat>)> = all
        .into_iter()
        .filter(|rel| !is_blocked(rel, &blocked))
        .map(|rel| (rel, l.files.get(rel).copied(), r.files.get(rel).copied()))
        .collect();

    let hash_total = if by == CompareBy::Contents {
        rows.iter()
            .filter(|(_, a, b)| match (a, b) {
                (Some(a), Some(b)) => contents_pair(*a, *b) && a.size == b.size,
                _ => false,
            })
            .count() as u64
    } else {
        0
    };

    let mut entries = Vec::with_capacity(rows.len());
    let mut hashed = 0;
    for (rel, left_stat, right_stat) in rows {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (a, b) = match (left_stat, right_stat) {
            (Some(a), None) => {
                entries.push(Entry {
                    rel: rel.to_string(),
                    status: Status::OnlyLeft,
                    left: Some(a),
                    right: None,
                });
                continue;
            }
            (None, Some(b)) => {
                entries.push(Entry {
                    rel: rel.to_string(),
                    status: Status::OnlyRight,
                    left: None,
                    right: Some(b),
                });
                continue;
            }
            (Some(a), Some(b)) => (a, b),
            (None, None) => continue,
        };
        let same = if by == CompareBy::Contents && contents_pair(a, b) {
            if a.size != b.size {
                Some(false)
            } else {
                hashed += 1;
                progress("hash", hashed, hash_total);
                match contents_match(&left.join(rel), &right.join(rel), cancel) {
                    Ok(same) => Some(same),
                    Err((side, reason)) => {
                        if cancel.load(Ordering::Relaxed) {
                            return Ok(None);
                        }
                        skipped.push(Skip {
                            rel: rel.to_string(),
                            side,
                            reason,
                        });
                        continue;
                    }
                }
            }
        } else {
            None
        };
        entries.push(Entry {
            rel: rel.to_string(),
            status: classify(a, b, same),
            left: Some(a),
            right: Some(b),
        });
    }
    skipped.sort_by(|a, b| a.rel.cmp(&b.rel));

    Ok(Some(Comparison {
        id,
        left,
        right,
        by,
        entries,
        skipped,
        trees: Arc::new(Trees { left: l, right: r }),
    }))
}

/// A user's choice for one row, or a bulk rule's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    CopyRight,
    CopyLeft,
    DeleteRight,
    DeleteLeft,
}

/// The bulk rules. Mirror makes one side match the other, deletions included.
/// Update copies only what is new or newer, and deletes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bulk {
    MirrorLeftRight,
    MirrorRightLeft,
    UpdateRight,
    UpdateLeft,
}

/// The picks a bulk rule makes for `c`, one per row it applies to.
pub fn bulk_picks(c: &Comparison, bulk: Bulk) -> Vec<(String, Pick)> {
    c.entries
        .iter()
        .filter_map(|e| {
            let pick = match (bulk, e.status) {
                (Bulk::MirrorLeftRight, Status::OnlyLeft) => Pick::CopyRight,
                (Bulk::MirrorLeftRight, Status::OnlyRight) => Pick::DeleteRight,
                (Bulk::MirrorLeftRight, s) if s.is_diff() => Pick::CopyRight,
                (Bulk::UpdateRight, Status::OnlyLeft | Status::NewerLeft) => Pick::CopyRight,
                (Bulk::MirrorRightLeft, Status::OnlyRight) => Pick::CopyLeft,
                (Bulk::MirrorRightLeft, Status::OnlyLeft) => Pick::DeleteLeft,
                (Bulk::MirrorRightLeft, s) if s.is_diff() => Pick::CopyLeft,
                (Bulk::UpdateLeft, Status::OnlyRight | Status::NewerRight) => Pick::CopyLeft,
                _ => return None,
            };
            Some((e.rel.clone(), pick))
        })
        .collect()
}

/// Whether a pick makes sense for a row's status.
fn applies(status: Status, pick: Pick) -> bool {
    match pick {
        Pick::CopyRight => status.is_diff() && status != Status::OnlyRight,
        Pick::CopyLeft => status.is_diff() && status != Status::OnlyLeft,
        Pick::DeleteRight => status == Status::OnlyRight,
        Pick::DeleteLeft => status == Status::OnlyLeft,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Copy from `from` over the other side. `replaces` when the destination
    /// already holds something, which is moved to the Trash first.
    Copy { from: Side, replaces: bool },
    /// Move the destination item to the Trash.
    Remove,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub rel: String,
    /// The side that is written: a copy's destination, or the side emptied.
    pub to: Side,
    pub op: Op,
    pub is_dir: bool,
    /// What the comparison saw on the source side (copies).
    pub from_stat: Option<Stat>,
    /// What the comparison saw on the destination (replacements and removals).
    pub to_stat: Option<Stat>,
}

/// A planned sync, checked against the comparison it came from.
#[derive(Clone, Debug)]
pub struct Plan {
    pub id: u64,
    pub left: PathBuf,
    pub right: PathBuf,
    pub actions: Vec<Action>,
    /// Picks left out at planning time, with why.
    pub skipped: Vec<(String, String)>,
    trees: Arc<Trees>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SyncSummary {
    pub id: u64,
    pub copies: u64,
    pub overwrites: u64,
    pub deletions: u64,
    pub bytes_written: u64,
    pub bytes_trashed: u64,
    pub skipped: Vec<(String, String)>,
}

fn file_bytes(stat: Option<Stat>) -> u64 {
    stat.filter(|s| s.kind == Kind::File).map_or(0, |s| s.size)
}

impl Plan {
    pub fn summary(&self) -> SyncSummary {
        let mut s = SyncSummary {
            id: self.id,
            copies: 0,
            overwrites: 0,
            deletions: 0,
            bytes_written: 0,
            bytes_trashed: 0,
            skipped: self.skipped.clone(),
        };
        for a in &self.actions {
            match a.op {
                Op::Copy {
                    replaces: false, ..
                } => {
                    s.copies += 1;
                    s.bytes_written += file_bytes(a.from_stat);
                }
                Op::Copy { replaces: true, .. } => {
                    s.overwrites += 1;
                    s.bytes_written += file_bytes(a.from_stat);
                    s.bytes_trashed += file_bytes(a.to_stat);
                }
                Op::Remove => {
                    s.deletions += 1;
                    s.bytes_trashed += file_bytes(a.to_stat);
                }
            }
        }
        s
    }
}

/// Turn picks into actions, in path order so a folder is handled before what
/// is inside it. Picks that do not apply to their row, or that name nothing
/// in the comparison, are skipped with a reason. A removal inside a folder
/// already being trashed is covered by that folder, and a copy inside a
/// folder being removed has nowhere to go; both are skipped.
pub fn plan(c: &Comparison, id: u64, picks: &[(String, Pick)]) -> Result<Plan, String> {
    let (left, right) = check_roots(&c.left, &c.right)?;
    let index: HashMap<&str, &Entry> = c.entries.iter().map(|e| (e.rel.as_str(), e)).collect();
    let mut chosen: BTreeMap<&str, Pick> = BTreeMap::new();
    for (rel, pick) in picks {
        chosen.insert(rel.as_str(), *pick);
    }

    let mut skipped = Vec::new();
    let mut candidates: Vec<Action> = Vec::new();
    for (rel, pick) in chosen {
        if let Err(e) = validate_rel(rel) {
            skipped.push((rel.to_string(), e));
            continue;
        }
        let Some(entry) = index.get(rel) else {
            skipped.push((rel.to_string(), "not in the comparison".into()));
            continue;
        };
        if !applies(entry.status, pick) {
            skipped.push((
                rel.to_string(),
                "that action does not apply to this row".into(),
            ));
            continue;
        }
        candidates.push(action_for(entry, pick));
    }

    let trashed: HashSet<String> = candidates
        .iter()
        .filter(|a| matches!(a.op, Op::Remove | Op::Copy { replaces: true, .. }))
        .map(|a| a.rel.clone())
        .collect();
    let removed: HashSet<String> = candidates
        .iter()
        .filter(|a| a.op == Op::Remove)
        .map(|a| a.rel.clone())
        .collect();

    let mut actions = Vec::new();
    for a in candidates {
        let blocker = match a.op {
            Op::Remove => ancestors(&a.rel)
                .find(|p| trashed.contains(*p))
                .map(|p| format!("inside {p}, which is already being moved to the Trash")),
            Op::Copy { .. } => ancestors(&a.rel)
                .find(|p| removed.contains(*p))
                .map(|p| format!("inside {p}, which is being removed")),
        };
        match blocker {
            Some(reason) => skipped.push((a.rel.clone(), reason)),
            None => actions.push(a),
        }
    }

    Ok(Plan {
        id,
        left,
        right,
        actions,
        skipped,
        trees: c.trees.clone(),
    })
}

fn action_for(e: &Entry, pick: Pick) -> Action {
    let stat_of = |s: Side| match s {
        Side::Left => e.left,
        Side::Right => e.right,
    };
    let (to, from) = match pick {
        Pick::CopyRight => (Side::Right, Some(Side::Left)),
        Pick::CopyLeft => (Side::Left, Some(Side::Right)),
        Pick::DeleteRight => (Side::Right, None),
        Pick::DeleteLeft => (Side::Left, None),
    };
    let to_stat = stat_of(to);
    let from_stat = from.and_then(stat_of);
    let op = match from {
        Some(from) => Op::Copy {
            from,
            replaces: to_stat.is_some(),
        },
        None => Op::Remove,
    };
    Action {
        rel: e.rel.clone(),
        to,
        op,
        is_dir: from_stat.or(to_stat).is_some_and(|s| s.kind == Kind::Dir),
        from_stat,
        to_stat,
    }
}

/// What a run did. `created` and `trashed` are what undo needs.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Outcome {
    pub id: u64,
    pub copied: u64,
    pub overwritten: u64,
    pub removed: u64,
    /// Items this run wrote, deepest first, so undo trashes children before their folders.
    pub created: Vec<PathBuf>,
    /// The same items as they were left, for undo to check before trashing them.
    pub written: Vec<CreatedItem>,
    /// Removed items now in the Trash, under their original paths.
    pub trashed: Vec<PathBuf>,
    /// Old versions overwritten by this run. Each is in the Trash under its `aside`
    /// name, not its real path, so undo restores it from there and renames it back.
    pub set_aside: Vec<SetAside>,
    /// Actions that went wrong, with why.
    pub failures: Vec<(String, String)>,
    /// Actions left alone because the item changed since the comparison.
    pub skipped: Vec<(String, String)>,
    /// Actions not reached because the run was cancelled.
    pub not_run: u64,
    pub cancelled: bool,
}

fn root_of(plan: &Plan, side: Side) -> &Path {
    match side {
        Side::Left => &plan.left,
        Side::Right => &plan.right,
    }
}

/// Whether `path` still matches `expected`. Sizes are compared for files only,
/// and modification times to the nanosecond.
fn still(expected: Option<Stat>, path: &Path) -> bool {
    let now = fs::symlink_metadata(path).ok().and_then(|m| Stat::of(&m));
    now == expected
}

/// Refuse a path whose folders above the root are not all real folders.
/// Symlinks are never followed.
fn parents_ok(root: &Path, rel: &str) -> Result<(), String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut cur = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err("a folder above it is no longer a folder".into()),
            Err(_) => return Err("the folder above it is missing".into()),
        }
    }
    Ok(())
}

/// Move `path` to the Trash and note it for undo.
fn trash_into(trasher: &dyn Trasher, path: &Path, out: &mut Outcome) -> Result<(), String> {
    trash_one(trasher, path)?;
    out.trashed.push(path.to_path_buf());
    Ok(())
}

/// Move `path` to the Trash.
fn trash_one(trasher: &dyn Trasher, path: &Path) -> Result<(), String> {
    let removed = recycle::delete_items(trasher, Mode::Trash, &[path.to_path_buf()]);
    if let Some((_, why)) = removed.failures.into_iter().next() {
        return Err(format!("could not move it to the Trash: {why}"));
    }
    Ok(())
}

/// A name beside `dst` that nothing uses yet. `tag` says what it is for.
fn beside(dst: &Path, plan_id: u64, tag: &str) -> Result<PathBuf, String> {
    let dir = dst.parent().ok_or("no folder to write into")?;
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("no file name")?;
    for n in 0..100 {
        let candidate = dir.join(format!(".{name}.ferris-sync-{plan_id}-{tag}{n}.tmp"));
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err("could not pick a temporary name".into())
}

fn occupied(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Note what was just written at `dst`, as it now is.
fn record_created(out: &mut Outcome, dst: &Path) {
    out.created.push(dst.to_path_buf());
    out.written.extend(CreatedItem::of(dst));
}

/// Whether the folder at `dst`, relative path `rel`, still holds exactly what
/// the comparison walked under it. Trashing a folder takes everything inside
/// it, so anything new, changed, or never judged keeps the folder in place.
fn subtree_unchanged(
    dst: &Path,
    rel: &str,
    side: Side,
    walked: &Walked,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    const UNJUDGED: &str = "it holds items the comparison could not judge";
    let prefix = format!("{rel}/");
    if walked.skipped.iter().any(|s| s.rel.starts_with(&prefix)) {
        return Ok(Some(UNJUDGED.into()));
    }
    let before: BTreeMap<&str, Stat> = walked
        .files
        .range(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .map(|(k, v)| (&k[prefix.len()..], *v))
        .collect();
    let now = walk(dst, side, cancel, &mut |_| {})?.ok_or("cancelled")?;
    if !now.skipped.is_empty() {
        return Ok(Some(UNJUDGED.into()));
    }
    let after: BTreeMap<&str, Stat> = now.files.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    Ok((before != after).then(|| CHANGED.to_string()))
}

/// Whether the item at `dst` may be trashed: it is still what the comparison
/// saw. For a folder, the whole tree under it must also still match.
fn unchanged_for_trash(
    plan: &Plan,
    side: Side,
    rel: &str,
    dst: &Path,
    expected: Option<Stat>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    match expected {
        Some(s) if s.kind == Kind::Dir => {
            if !is_real_dir(dst) {
                return Ok(Some(CHANGED.into()));
            }
            subtree_unchanged(dst, rel, side, plan.trees.of(side), cancel)
        }
        _ if still(expected, dst) => Ok(None),
        _ => Ok(Some(CHANGED.into())),
    }
}

/// Put something new at `dst` in place of what is there. The old version is
/// first moved aside, under a unique name in the same folder. `land` places the
/// new one, and if that fails the old version goes straight back. The old
/// version is trashed, under its aside name, only after the new one has landed.
fn swap_in(
    trasher: &dyn Trasher,
    dst: &Path,
    plan_id: u64,
    out: &mut Outcome,
    land: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let aside = beside(dst, plan_id, "old")?;
    desktop::rename_noreplace(dst, &aside)
        .map_err(|e| format!("could not set the old version aside: {e}"))?;
    if let Err(e) = land(dst) {
        return match desktop::rename_noreplace(&aside, dst) {
            Ok(()) => Err(e),
            Err(back) => Err(format!(
                "{e}; the old version could not be put back, and is at {}: {back}",
                aside.display()
            )),
        };
    }
    record_created(out, dst);
    trash_one(trasher, &aside)
        .map_err(|e| format!("{e}; the old version is still at {}", aside.display()))?;
    out.set_aside.push(SetAside {
        path: dst.to_path_buf(),
        aside,
    });
    Ok(())
}

/// Give `path` the modification time `mtime_ns`, so the copy compares equal.
fn set_mtime(path: &Path, mtime_ns: u128) -> std::io::Result<()> {
    let when = UNIX_EPOCH
        + Duration::new(
            (mtime_ns / 1_000_000_000) as u64,
            (mtime_ns % 1_000_000_000) as u32,
        );
    File::options().write(true).open(path)?.set_modified(when)
}

/// Run one action. Ok(None) when it was done; Ok(Some(reason)) when it was
/// left alone; Err when it failed.
fn apply(
    trasher: &dyn Trasher,
    plan: &Plan,
    a: &Action,
    cancel: &AtomicBool,
    place: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    out: &mut Outcome,
) -> Result<Option<String>, String> {
    validate_rel(&a.rel)?;
    let dst = root_of(plan, a.to).join(&a.rel);
    let target_ok = || unchanged_for_trash(plan, a.to, &a.rel, &dst, a.to_stat, cancel);
    match a.op {
        Op::Remove => {
            if let Err(reason) = parents_ok(root_of(plan, a.to), &a.rel) {
                return Ok(Some(reason));
            }
            if let Some(reason) = target_ok()? {
                return Ok(Some(reason));
            }
            trash_into(trasher, &dst, out)?;
            Ok(None)
        }
        Op::Copy { from, replaces } => {
            let src = root_of(plan, from).join(&a.rel);
            if let Err(reason) = parents_ok(root_of(plan, from), &a.rel)
                .and_then(|()| parents_ok(root_of(plan, a.to), &a.rel))
            {
                return Ok(Some(reason));
            }
            if !still(a.from_stat, &src) {
                return Ok(Some(CHANGED.into()));
            }
            if replaces {
                if let Some(reason) = target_ok()? {
                    return Ok(Some(reason));
                }
            } else if occupied(&dst) {
                return Ok(Some("something appeared there since the comparison".into()));
            }

            if a.is_dir {
                if replaces {
                    swap_in(trasher, &dst, plan.id, out, &|at| {
                        fs::create_dir(at).map_err(|e| e.to_string())
                    })?;
                } else {
                    fs::create_dir(&dst).map_err(|e| e.to_string())?;
                    record_created(out, &dst);
                }
                return Ok(None);
            }

            copy_then_swap(
                trasher, plan.id, &src, &dst, a, replaces, cancel, place, &target_ok, out,
            )
        }
    }
}

/// The file half of a copy: write a temporary beside `dst`, check the source did
/// not change while writing, then place it. A replacement goes through `swap_in`,
/// so the old version is never trashed before the new one has landed.
#[allow(clippy::too_many_arguments)]
fn copy_then_swap(
    trasher: &dyn Trasher,
    plan_id: u64,
    src: &Path,
    dst: &Path,
    a: &Action,
    replaces: bool,
    cancel: &AtomicBool,
    place: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    target_ok: &dyn Fn() -> Result<Option<String>, String>,
    out: &mut Outcome,
) -> Result<Option<String>, String> {
    let stat = a.from_stat.ok_or("no source to copy")?;
    let tmp = beside(dst, plan_id, "new")?;
    let written = ops::copy_recursive(src, &tmp, cancel, &mut |_| {})
        .and_then(|()| set_mtime(&tmp, stat.mtime_ns).map_err(|e| e.to_string()));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if !still(a.from_stat, src) {
        let _ = fs::remove_file(&tmp);
        return Ok(Some("changed while it was being copied".into()));
    }

    let mut left_alone = None;
    let placed = if replaces {
        match target_ok() {
            Ok(None) => swap_in(trasher, dst, plan_id, out, &|at| {
                place(&tmp, at).map_err(|e| e.to_string())
            }),
            Ok(Some(reason)) => {
                left_alone = Some(reason);
                Ok(())
            }
            Err(e) => Err(e),
        }
    } else {
        place(&tmp, dst)
            .map_err(|e| e.to_string())
            .map(|()| record_created(out, dst))
    };
    if let Some(reason) = left_alone {
        let _ = fs::remove_file(&tmp);
        return Ok(Some(reason));
    }
    match placed {
        Ok(()) => Ok(None),
        Err(e) => {
            // The temporary holds the only copy of the new bytes when the old
            // version is stranded aside, so it is kept in that case.
            if !replaces || occupied(dst) {
                let _ = fs::remove_file(&tmp);
            }
            Err(e)
        }
    }
}

/// Carry out the plan. Stops between actions when `cancel` is raised. The
/// roots are checked again first. `progress` gets (done, total).
pub fn execute(
    trasher: &dyn Trasher,
    plan: &Plan,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, String> {
    execute_with(trasher, plan, cancel, progress, &desktop::rename_noreplace)
}

/// `execute`, with the step that places a new file as a parameter, so a test
/// can make that step fail.
fn execute_with(
    trasher: &dyn Trasher,
    plan: &Plan,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
    place: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<Outcome, String> {
    check_roots(&plan.left, &plan.right)?;
    let mut out = Outcome {
        id: plan.id,
        ..Outcome::default()
    };
    let total = plan.actions.len() as u64;
    for (i, a) in plan.actions.iter().enumerate() {
        let done = i as u64;
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            out.not_run = total - done;
            break;
        }
        progress(done, total);
        match apply(trasher, plan, a, cancel, place, &mut out) {
            Ok(None) => match a.op {
                Op::Copy {
                    replaces: false, ..
                } => out.copied += 1,
                Op::Copy { replaces: true, .. } => out.overwritten += 1,
                Op::Remove => out.removed += 1,
            },
            Ok(Some(reason)) => out.skipped.push((a.rel.clone(), reason)),
            Err(_) if cancel.load(Ordering::Relaxed) => {
                out.cancelled = true;
                out.not_run = total - done;
                break;
            }
            Err(e) => out.failures.push((a.rel.clone(), e)),
        }
    }
    progress(total, total);
    out.created.sort_by(|a, b| b.cmp(a));
    out.written.sort_by(|a, b| b.path.cmp(&a.path));
    Ok(out)
}

/// An item a sync wrote, as it was left. Undo trashes it only while it still matches.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CreatedItem {
    pub path: PathBuf,
    pub dir: bool,
    pub size: u64,
    /// Full precision, as a string, since a JSON number cannot hold it exactly.
    pub mtime_ns: String,
}

impl CreatedItem {
    fn of(path: &Path) -> Option<CreatedItem> {
        let stat = Stat::of(&fs::symlink_metadata(path).ok()?)?;
        Some(CreatedItem {
            path: path.to_path_buf(),
            dir: stat.kind == Kind::Dir,
            size: stat.size,
            mtime_ns: stat.mtime_ns.to_string(),
        })
    }
}

/// An old version a sync moved aside to overwrite `path`. It sits in the Trash
/// under `aside`, the name it had when it was trashed.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SetAside {
    pub path: PathBuf,
    pub aside: PathBuf,
}

/// What undoing a sync's writes came to.
#[derive(Debug, Default, serde::Serialize)]
pub struct WrittenUndo {
    /// Written items trashed.
    pub done: Vec<PathBuf>,
    /// Old versions put back at their real paths.
    pub restored: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
    /// Items left in place, with why. They changed since the sync wrote them.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Whether an item is still as the sync left it. A folder must be empty now,
/// since its contents were undone first, and not counting them is what
/// keeps the folder's own time out of the check.
fn left_as_written(item: &CreatedItem) -> Result<(), String> {
    let now = fs::symlink_metadata(&item.path).map_err(|_| "is no longer there".to_string())?;
    if item.dir {
        if !now.is_dir() {
            return Err(CHANGED.into());
        }
        let mut entries = fs::read_dir(&item.path).map_err(|e| e.to_string())?;
        return match entries.next() {
            None => Ok(()),
            Some(_) => Err("is not empty".into()),
        };
    }
    let want: u128 = item
        .mtime_ns
        .parse()
        .map_err(|_| "has no recorded time".to_string())?;
    match Stat::of(&now) {
        Some(s) if s.kind == Kind::File && s.size == item.size && s.mtime_ns == want => Ok(()),
        _ => Err(CHANGED.into()),
    }
}

/// Trash what a sync wrote, but only the items still as the sync left them.
/// Deepest first, so a folder is trashed after what is inside it.
pub fn trash_unchanged(trasher: &dyn Trasher, written: &[CreatedItem]) -> WrittenUndo {
    let mut order: Vec<&CreatedItem> = written.iter().collect();
    order.sort_by(|a, b| b.path.cmp(&a.path));
    let mut out = WrittenUndo::default();
    for item in order {
        if let Err(why) = left_as_written(item) {
            out.skipped.push((item.path.clone(), why));
            continue;
        }
        let removed = recycle::delete_items(trasher, Mode::Trash, std::slice::from_ref(&item.path));
        out.done.extend(removed.done);
        out.failures.extend(removed.failures);
    }
    out
}

/// Undo a sync's overwrites. Trash each written new version that is still as the
/// sync left it, then put each old version back: restore it from the Trash to
/// its aside name, and rename it to the real path. An old version waits in the
/// Trash while its new version is still there, and is left there when the real
/// path is occupied. Each item stops only itself, and is reported.
pub fn undo_sync(
    trasher: &dyn Trasher,
    written: &[CreatedItem],
    set_aside: &[SetAside],
) -> WrittenUndo {
    let mut out = trash_unchanged(trasher, written);
    for pair in set_aside {
        let stuck = out
            .failures
            .iter()
            .chain(out.skipped.iter())
            .find(|(p, _)| *p == pair.path)
            .map(|(_, why)| why.clone());
        if let Some(why) = stuck {
            out.skipped.push((
                pair.path.clone(),
                format!("its new version was not trashed ({why})"),
            ));
            continue;
        }
        if occupied(&pair.path) {
            out.failures
                .push((pair.path.clone(), "something is there again".into()));
            continue;
        }
        if let Err(e) = trasher.restore(&pair.aside) {
            out.failures.push((
                pair.path.clone(),
                format!(
                    "{e} (the old version is in the Trash as {})",
                    pair.aside.display()
                ),
            ));
            continue;
        }
        match desktop::rename_noreplace(&pair.aside, &pair.path) {
            Ok(()) => out.restored.push(pair.path.clone()),
            Err(e) => out.failures.push((
                pair.path.clone(),
                format!("{e} (the old version is at {})", pair.aside.display()),
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::cell::RefCell;

    fn stat(kind: Kind, size: u64, mtime_ns: u128) -> Stat {
        Stat {
            kind,
            size,
            modified_at: (mtime_ns / 1_000_000_000) as u64,
            mtime_ns,
        }
    }

    /// Write a file and give it an exact modification time, in seconds.
    fn touch(path: &Path, bytes: &[u8], secs: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn run_compare(left: &Path, right: &Path, by: CompareBy) -> Comparison {
        compare(
            1,
            left,
            right,
            by,
            &AtomicBool::new(false),
            &mut |_, _, _| {},
        )
        .unwrap()
        .unwrap()
    }

    fn status_of(c: &Comparison, rel: &str) -> Option<Status> {
        c.entries.iter().find(|e| e.rel == rel).map(|e| e.status)
    }

    /// A trash that moves items into a folder of its own, so tests can see
    /// what would have been trashed. It never touches the real Trash.
    struct MoveTrash {
        dir: PathBuf,
        seq: RefCell<u32>,
        moved: RefCell<Vec<PathBuf>>,
        /// Where each trashed item is kept, with the path it came from.
        kept: RefCell<Vec<(PathBuf, PathBuf)>>,
    }

    impl MoveTrash {
        fn new(dir: &Path) -> Self {
            MoveTrash {
                dir: dir.to_path_buf(),
                seq: RefCell::new(0),
                moved: RefCell::new(Vec::new()),
                kept: RefCell::new(Vec::new()),
            }
        }
    }

    impl Trasher for MoveTrash {
        fn trash(&self, path: &Path) -> Result<(), String> {
            let n = {
                let mut s = self.seq.borrow_mut();
                *s += 1;
                *s
            };
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let kept = self.dir.join(format!("{n}-{name}"));
            fs::rename(path, &kept).map_err(|e| e.to_string())?;
            self.moved.borrow_mut().push(path.to_path_buf());
            self.kept.borrow_mut().push((kept, path.to_path_buf()));
            Ok(())
        }

        /// Put back the newest item that came from `original`, as the OS does.
        fn restore(&self, original: &Path) -> Result<(), String> {
            let mut kept = self.kept.borrow_mut();
            let at = kept
                .iter()
                .rposition(|(_, from)| from == original)
                .ok_or_else(|| format!("{} is not in the Trash", original.display()))?;
            let (now, _) = kept.remove(at);
            fs::rename(&now, original).map_err(|e| e.to_string())
        }
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// The contents of the one item in a stub trash whose name starts with `prefix`.
    fn trashed_bytes(trash: &Path, prefix: &str) -> Vec<u8> {
        let found: Vec<PathBuf> = fs::read_dir(trash)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        fs::read(&found[0]).unwrap()
    }

    fn trash_is_empty(trash: &Path) -> bool {
        fs::read_dir(trash).unwrap().next().is_none()
    }

    /// Plans report canonical roots, so tests compare against those too.
    fn canon(p: PathBuf) -> PathBuf {
        fs::canonicalize(p).unwrap()
    }

    #[test]
    fn classify_covers_every_status() {
        let f = |size, t| stat(Kind::File, size, t);
        assert_eq!(
            classify(f(1, 5), stat(Kind::Dir, 0, 5), None),
            Status::TypeConflict
        );
        assert_eq!(
            classify(stat(Kind::Dir, 0, 1), stat(Kind::Dir, 0, 9), None),
            Status::Identical
        );
        assert_eq!(classify(f(1, 9), f(1, 5), None), Status::NewerLeft);
        assert_eq!(classify(f(1, 5), f(9, 9), None), Status::NewerRight);
        assert_eq!(classify(f(4, 5), f(4, 5), None), Status::Identical);
        assert_eq!(
            classify(f(4, 5), f(7, 5), None),
            Status::SameTimeSizeDiffers
        );
        // Contents mode: the bytes decide, and the time only gives a direction.
        assert_eq!(classify(f(4, 5), f(4, 9), Some(true)), Status::Identical);
        assert_eq!(
            classify(f(4, 5), f(4, 5), Some(false)),
            Status::ContentDiffers
        );
        assert_eq!(classify(f(4, 9), f(4, 5), Some(false)), Status::NewerLeft);
    }

    #[test]
    fn compare_classifies_every_kind_by_metadata() {
        let tmp = TempDir::new("cmp_kinds");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("only_l.txt"), b"a", 100);
        touch(&r.join("only_r.txt"), b"a", 100);
        touch(&l.join("same.txt"), b"abc", 100);
        touch(&r.join("same.txt"), b"abc", 100);
        touch(&l.join("newer_l.txt"), b"abc", 200);
        touch(&r.join("newer_l.txt"), b"abc", 100);
        touch(&l.join("newer_r.txt"), b"abc", 100);
        touch(&r.join("newer_r.txt"), b"abc", 200);
        touch(&l.join("same_time.txt"), b"abcd", 100);
        touch(&r.join("same_time.txt"), b"ab", 100);
        touch(&l.join("clash"), b"file", 100);
        touch(&r.join("clash/inner.txt"), b"x", 100);
        touch(&l.join("d/f.txt"), b"x", 100);
        touch(&r.join("d/f.txt"), b"x", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        assert_eq!(status_of(&c, "only_l.txt"), Some(Status::OnlyLeft));
        assert_eq!(status_of(&c, "only_r.txt"), Some(Status::OnlyRight));
        assert_eq!(status_of(&c, "same.txt"), Some(Status::Identical));
        assert_eq!(status_of(&c, "newer_l.txt"), Some(Status::NewerLeft));
        assert_eq!(status_of(&c, "newer_r.txt"), Some(Status::NewerRight));
        assert_eq!(
            status_of(&c, "same_time.txt"),
            Some(Status::SameTimeSizeDiffers)
        );
        assert_eq!(status_of(&c, "clash"), Some(Status::TypeConflict));
        assert_eq!(status_of(&c, "d"), Some(Status::Identical));
        assert_eq!(status_of(&c, "d/f.txt"), Some(Status::Identical));
        assert!(c.skipped.is_empty());
    }

    #[test]
    fn contents_mode_reads_bytes_and_ignores_matching_times() {
        let tmp = TempDir::new("cmp_contents");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        // Same size and time, different bytes.
        touch(&l.join("a.bin"), b"aaaa", 100);
        touch(&r.join("a.bin"), b"aaab", 100);
        // Same bytes, different times.
        touch(&l.join("b.bin"), b"bbbb", 200);
        touch(&r.join("b.bin"), b"bbbb", 100);

        let meta = run_compare(&l, &r, CompareBy::Metadata);
        assert_eq!(status_of(&meta, "a.bin"), Some(Status::Identical));
        assert_eq!(status_of(&meta, "b.bin"), Some(Status::NewerLeft));

        let bytes = run_compare(&l, &r, CompareBy::Contents);
        assert_eq!(status_of(&bytes, "a.bin"), Some(Status::ContentDiffers));
        assert_eq!(status_of(&bytes, "b.bin"), Some(Status::Identical));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_and_their_subtree_is_not_judged() {
        let tmp = TempDir::new("cmp_links");
        let outside = TempDir::new("cmp_links_outside");
        touch(&outside.path().join("secret.txt"), b"x", 100);
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("keep.txt"), b"k", 100);
        touch(&r.join("keep.txt"), b"k", 100);
        // A link to a folder on the left, where the right has real contents.
        std::os::unix::fs::symlink(outside.path(), l.join("link")).unwrap();
        touch(&r.join("link/inner.txt"), b"x", 100);
        // A link to a file on the left, where the right has nothing.
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), l.join("file_link")).unwrap();

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let mut skipped: Vec<(String, Side)> =
            c.skipped.iter().map(|s| (s.rel.clone(), s.side)).collect();
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            skipped,
            vec![
                ("file_link".into(), Side::Left),
                ("link".into(), Side::Left)
            ]
        );
        // Nothing reached through the link, and nothing blocked by it, is judged.
        assert!(status_of(&c, "link").is_none());
        assert!(status_of(&c, "link/inner.txt").is_none());
        assert!(status_of(&c, "file_link").is_none());
        assert_eq!(status_of(&c, "keep.txt"), Some(Status::Identical));
        // Mirror therefore cannot delete the right side's inner.txt.
        let picks = bulk_picks(&c, Bulk::MirrorLeftRight);
        assert!(picks.iter().all(|(rel, _)| !rel.starts_with("link")));
    }

    #[test]
    fn rel_paths_must_stay_inside_the_roots() {
        for bad in ["", ".", "..", "../x", "a/../b", "/abs", "a/./b"] {
            assert!(validate_rel(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["a", "a/b", "a b/c.txt", "dir/..name"] {
            assert!(validate_rel(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn nested_or_identical_roots_are_refused() {
        let tmp = TempDir::new("roots");
        let outer = tmp.dir("outer");
        let inner = tmp.dir("outer/inner");
        let sibling = tmp.dir("sibling");
        assert!(check_roots(&outer, &inner).is_err());
        assert!(check_roots(&inner, &outer).is_err());
        assert!(check_roots(&outer, &outer).is_err());
        assert!(check_roots(&outer, &sibling).is_ok());
        assert!(check_roots(&outer, &tmp.path().join("missing")).is_err());
    }

    #[test]
    fn mirror_removes_right_only_items_into_the_trash() {
        let tmp = TempDir::new("mirror");
        let trash = TempDir::new("mirror_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("keep.txt"), b"k", 100);
        touch(&r.join("keep.txt"), b"k", 100);
        touch(&r.join("extra.txt"), b"gone", 100);
        touch(&r.join("old/inner.txt"), b"gone too", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let picks = bulk_picks(&c, Bulk::MirrorLeftRight);
        let plan = plan(&c, 7, &picks).unwrap();
        let summary = plan.summary();
        assert_eq!(
            (summary.deletions, summary.copies, summary.overwrites),
            (2, 0, 0)
        );
        // The folder covers its contents, so the inner file is not removed twice.
        assert!(summary
            .skipped
            .iter()
            .any(|(rel, _)| rel == "old/inner.txt"));

        let stub = MoveTrash::new(trash.path());
        let out = execute(&stub, &plan, &no_cancel(), &mut |_, _| {}).unwrap();
        assert_eq!(out.removed, 2);
        assert!(out.failures.is_empty() && out.skipped.is_empty());
        assert!(!r.join("extra.txt").exists());
        assert!(!r.join("old").exists());
        assert!(r.join("keep.txt").exists());
        assert!(l.join("keep.txt").exists());
        let mut trashed = out.trashed.clone();
        trashed.sort();
        assert_eq!(trashed, vec![r.join("extra.txt"), r.join("old")]);
        assert_eq!(fs::read(trash.path().join("1-extra.txt")).unwrap(), b"gone");
    }

    #[test]
    fn overwrite_moves_the_old_version_to_the_trash_and_keeps_mtime() {
        let tmp = TempDir::new("overwrite");
        let trash = TempDir::new("overwrite_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("a.txt"), b"new bytes", 200);
        touch(&r.join("a.txt"), b"old", 100);
        touch(&l.join("fresh.txt"), b"brand new", 150);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let picks = bulk_picks(&c, Bulk::UpdateRight);
        let plan = plan(&c, 8, &picks).unwrap();
        let summary = plan.summary();
        assert_eq!(
            (summary.copies, summary.overwrites, summary.deletions),
            (1, 1, 0)
        );
        assert_eq!(summary.bytes_written, 9 + 9);
        assert_eq!(summary.bytes_trashed, 3);

        let stub = MoveTrash::new(trash.path());
        let out = execute(&stub, &plan, &no_cancel(), &mut |_, _| {}).unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"new bytes");
        assert_eq!(fs::read(r.join("fresh.txt")).unwrap(), b"brand new");
        // The old version went to the trash, not to nowhere. It is recorded under
        // its set-aside name, since that is what the trash holds, and the real path too.
        assert_eq!(trashed_bytes(trash.path(), "1-"), b"old");
        assert!(out.trashed.is_empty(), "{:?}", out.trashed);
        assert_eq!(out.set_aside.len(), 1);
        assert_eq!(out.set_aside[0].path, r.join("a.txt"));
        assert_eq!(out.set_aside[0].aside.parent(), Some(r.as_path()));
        assert!(out.set_aside[0]
            .aside
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("ferris-sync"));
        assert_eq!(out.created.len(), 2);
        // No temporary file is left behind.
        let names: Vec<String> = fs::read_dir(&r)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains("ferris-sync")),
            "{names:?}"
        );
        // Copies keep the source's time, so the next comparison sees them as the same.
        let again = run_compare(&l, &r, CompareBy::Metadata);
        assert_eq!(status_of(&again, "a.txt"), Some(Status::Identical));
        assert_eq!(status_of(&again, "fresh.txt"), Some(Status::Identical));
    }

    /// Overwrite `a.txt` in the right folder, with the old version going to `stub`'s trash.
    /// Returns the right folder and the sync's outcome.
    fn overwrite_a(tmp: &TempDir, stub: &MoveTrash, id: u64) -> (PathBuf, Outcome) {
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("a.txt"), b"new bytes", 200);
        touch(&r.join("a.txt"), b"old", 100);
        let c = run_compare(&l, &r, CompareBy::Metadata);
        let plan = plan(&c, id, &bulk_picks(&c, Bulk::UpdateRight)).unwrap();
        let out = execute(stub, &plan, &no_cancel(), &mut |_, _| {}).unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        (r, out)
    }

    #[test]
    fn undo_puts_an_overwritten_version_back_through_its_set_aside_name() {
        let tmp = TempDir::new("undo_overwrite");
        let trash = TempDir::new("undo_overwrite_trash");
        let stub = MoveTrash::new(trash.path());
        let (r, out) = overwrite_a(&tmp, &stub, 12);

        let undo = undo_sync(&stub, &out.written, &out.set_aside);
        assert!(
            undo.failures.is_empty() && undo.skipped.is_empty(),
            "{:?} {:?}",
            undo.failures,
            undo.skipped
        );
        assert_eq!(undo.done, vec![r.join("a.txt")]);
        assert_eq!(undo.restored, vec![r.join("a.txt")]);
        assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"old");
        // The new version is in the Trash, and no set-aside name is left in the folder.
        assert_eq!(trashed_bytes(trash.path(), "2-a.txt"), b"new bytes");
        let names: Vec<String> = fs::read_dir(&r)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    #[test]
    fn undo_leaves_an_old_version_in_the_trash_when_its_real_path_is_taken() {
        let tmp = TempDir::new("undo_occupied");
        let trash = TempDir::new("undo_occupied_trash");
        let stub = MoveTrash::new(trash.path());
        let (r, out) = overwrite_a(&tmp, &stub, 13);
        // The new version is gone, and something else now sits at the real path.
        touch(&r.join("a.txt"), b"occupant", 300);

        let undo = undo_sync(&stub, &[], &out.set_aside);
        assert!(undo.restored.is_empty(), "{:?}", undo.restored);
        assert_eq!(undo.failures.len(), 1, "{:?}", undo.failures);
        assert_eq!(undo.failures[0].0, r.join("a.txt"));
        assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"occupant");
        // The old version is still in the Trash, where it can be restored by hand.
        assert_eq!(trashed_bytes(trash.path(), "1-"), b"old");
    }

    #[test]
    fn folders_are_created_before_their_contents_are_copied() {
        let tmp = TempDir::new("newdir");
        let trash = TempDir::new("newdir_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("d/sub/f.txt"), b"hi", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let picks = bulk_picks(&c, Bulk::UpdateRight);
        let plan = plan(&c, 9, &picks).unwrap();
        let out = execute(
            &MoveTrash::new(trash.path()),
            &plan,
            &no_cancel(),
            &mut |_, _| {},
        )
        .unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        assert_eq!(fs::read(r.join("d/sub/f.txt")).unwrap(), b"hi");
        assert_eq!(out.copied, 3);
        // Undo trashes the deepest item first.
        assert_eq!(out.created[0], r.join("d/sub/f.txt"));
    }

    #[test]
    fn items_changed_since_the_comparison_are_skipped() {
        let tmp = TempDir::new("changed");
        let trash = TempDir::new("changed_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("src.txt"), b"left v1", 200);
        touch(&r.join("src.txt"), b"right", 100);
        touch(&l.join("dst.txt"), b"left", 200);
        touch(&r.join("dst.txt"), b"right v1", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let picks = bulk_picks(&c, Bulk::UpdateRight);
        let plan = plan(&c, 10, &picks).unwrap();

        // After the comparison, the right copy of one file is edited and the left source of another grows.
        touch(&r.join("src.txt"), b"right edited", 300);
        touch(&l.join("dst.txt"), b"left grown!", 400);

        let stub = MoveTrash::new(trash.path());
        let out = execute(&stub, &plan, &no_cancel(), &mut |_, _| {}).unwrap();
        assert_eq!(out.overwritten, 0);
        assert_eq!(out.skipped.len(), 2, "{:?}", out.skipped);
        assert!(out.skipped.iter().all(|(_, why)| why == CHANGED));
        assert_eq!(fs::read(r.join("src.txt")).unwrap(), b"right edited");
        assert_eq!(fs::read(r.join("dst.txt")).unwrap(), b"right v1");
        assert!(out.trashed.is_empty());
        assert!(stub.moved.borrow().is_empty());
    }

    #[test]
    fn a_raised_cancel_flag_stops_before_the_first_action() {
        let tmp = TempDir::new("cancel_sync");
        let trash = TempDir::new("cancel_sync_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("a.txt"), b"a", 100);
        touch(&l.join("b.txt"), b"b", 100);
        let c = run_compare(&l, &r, CompareBy::Metadata);
        let plan = plan(&c, 11, &bulk_picks(&c, Bulk::MirrorLeftRight)).unwrap();
        let cancel = AtomicBool::new(true);
        let out = execute(
            &MoveTrash::new(trash.path()),
            &plan,
            &cancel,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(out.cancelled);
        assert_eq!(out.not_run, 2);
        assert_eq!(out.copied, 0);
        assert!(!r.join("a.txt").exists() && !r.join("b.txt").exists());
    }

    #[test]
    fn picks_that_do_not_fit_their_row_are_skipped() {
        let tmp = TempDir::new("picks");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("left_only.txt"), b"x", 100);
        let c = run_compare(&l, &r, CompareBy::Metadata);
        let picks = vec![
            ("left_only.txt".to_string(), Pick::DeleteRight),
            ("left_only.txt".to_string(), Pick::CopyLeft),
            ("left_only.txt".to_string(), Pick::CopyRight),
            ("nope.txt".to_string(), Pick::CopyRight),
            ("../escape".to_string(), Pick::CopyRight),
        ];
        let plan = plan(&c, 12, &picks).unwrap();
        // The last pick for a path wins, so this one copies.
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.skipped.len(), 2);
        assert_eq!(plan.summary().copies, 1);

        // A pick that does not fit its row is refused on its own.
        let refused =
            super::plan(&c, 14, &[("left_only.txt".to_string(), Pick::DeleteRight)]).unwrap();
        assert!(refused.actions.is_empty());
        assert_eq!(refused.skipped.len(), 1);
        assert!(refused.skipped[0].1.contains("does not apply"));
    }

    #[test]
    fn mirror_right_to_left_removes_left_only_items_to_the_trash() {
        let tmp = TempDir::new("mirror_rl");
        let trash = TempDir::new("mirror_rl_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("gone.txt"), b"left only", 100);
        touch(&r.join("new.txt"), b"right only", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let plan = plan(&c, 13, &bulk_picks(&c, Bulk::MirrorRightLeft)).unwrap();
        let summary = plan.summary();
        assert_eq!((summary.copies, summary.deletions), (1, 1));

        let stub = MoveTrash::new(trash.path());
        let out = execute(&stub, &plan, &no_cancel(), &mut |_, _| {}).unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        assert!(!l.join("gone.txt").exists());
        assert_eq!(out.trashed, vec![l.join("gone.txt")]);
        assert_eq!(
            fs::read(trash.path().join("1-gone.txt")).unwrap(),
            b"left only"
        );
        assert_eq!(fs::read(l.join("new.txt")).unwrap(), b"right only");
    }

    #[test]
    fn a_failed_placement_puts_the_old_version_back_and_trashes_nothing() {
        let tmp = TempDir::new("place_fail");
        let trash = TempDir::new("place_fail_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("a.txt"), b"new bytes", 200);
        touch(&r.join("a.txt"), b"old", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let plan = plan(&c, 15, &[("a.txt".to_string(), Pick::CopyRight)]).unwrap();
        let stub = MoveTrash::new(trash.path());
        let failing =
            |_: &Path, _: &Path| -> std::io::Result<()> { Err(std::io::Error::other("injected")) };
        let out = execute_with(&stub, &plan, &no_cancel(), &mut |_, _| {}, &failing).unwrap();

        assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
        assert!(out.failures[0].1.contains("injected"));
        assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"old");
        assert!(out.trashed.is_empty() && out.created.is_empty());
        assert!(stub.moved.borrow().is_empty());
        assert!(trash_is_empty(trash.path()));
        // The temporary copy is removed too, so only the old version is left.
        let names: Vec<String> = fs::read_dir(&r)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    #[test]
    fn a_folder_that_gained_a_nested_file_since_the_comparison_is_not_trashed() {
        let tmp = TempDir::new("nested_new");
        let trash = TempDir::new("nested_new_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&r.join("old/inner.txt"), b"gone", 100);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        // A file appears two folders deep, where the folder's own time does not show it.
        touch(&r.join("old/sub/new.txt"), b"unjudged", 300);

        let plan = plan(&c, 16, &bulk_picks(&c, Bulk::MirrorLeftRight)).unwrap();
        let out = execute(
            &MoveTrash::new(trash.path()),
            &plan,
            &no_cancel(),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(out.removed, 0);
        assert!(out
            .skipped
            .iter()
            .any(|(rel, why)| rel == "old" && why == CHANGED));
        assert!(out.trashed.is_empty());
        assert!(trash_is_empty(trash.path()));
        assert_eq!(fs::read(r.join("old/sub/new.txt")).unwrap(), b"unjudged");
        assert_eq!(fs::read(r.join("old/inner.txt")).unwrap(), b"gone");
    }

    #[test]
    fn a_sync_records_what_it_wrote_and_undo_leaves_changed_items_alone() {
        let tmp = TempDir::new("written");
        let trash = TempDir::new("written_trash");
        let undo_trash = TempDir::new("written_undo_trash");
        let (l, r) = (canon(tmp.dir("l")), canon(tmp.dir("r")));
        touch(&l.join("d/f.txt"), b"hi", 100);
        touch(&l.join("fresh.txt"), b"brand new", 150);

        let c = run_compare(&l, &r, CompareBy::Metadata);
        let plan = plan(&c, 17, &bulk_picks(&c, Bulk::UpdateRight)).unwrap();
        let out = execute(
            &MoveTrash::new(trash.path()),
            &plan,
            &no_cancel(),
            &mut |_, _| {},
        )
        .unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        assert_eq!(out.written.len(), 3, "{:?}", out.written);
        let fresh = out
            .written
            .iter()
            .find(|w| w.path == r.join("fresh.txt"))
            .unwrap();
        let on_disk = Stat::of(&fs::symlink_metadata(r.join("fresh.txt")).unwrap()).unwrap();
        assert_eq!(fresh.size, 9);
        assert!(!fresh.dir);
        assert_eq!(fresh.mtime_ns, on_disk.mtime_ns.to_string());

        // After the sync, one written file is edited.
        touch(&r.join("fresh.txt"), b"edited!", 500);
        let undone = trash_unchanged(&MoveTrash::new(undo_trash.path()), &out.written);
        assert_eq!(undone.skipped.len(), 1, "{:?}", undone.skipped);
        assert_eq!(
            undone.skipped[0],
            (r.join("fresh.txt"), CHANGED.to_string())
        );
        // The folder is undone after what is inside it.
        assert_eq!(undone.done, vec![r.join("d/f.txt"), r.join("d")]);
        assert!(undone.failures.is_empty());
        assert_eq!(fs::read(r.join("fresh.txt")).unwrap(), b"edited!");
        assert!(!r.join("d").exists());
    }
}
