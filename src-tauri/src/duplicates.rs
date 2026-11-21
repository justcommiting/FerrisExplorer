//! Duplicate finder: sets of files whose contents are byte-for-byte identical.
//!
//! Each stage only looks at what the stage before could not rule out:
//! 1. group by size (zero-byte files, symlinks and anything that is not a
//!    regular file are never candidates);
//! 2. group by a SHA-256 of the first and last 64 KiB;
//! 3. group by a SHA-256 of the whole file.
//!
//! Only stage 3 decides what is a copy, so a partial hash can never merge
//! two files that differ. The size grouping and the walk follow the same
//! rules as the `duplicate:true` search (`search::filter_duplicates`).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::core::node_from_metadata;
use crate::search::is_skipped_dir;

/// Bytes read from each end of a file for the partial hash.
pub const EDGE_BYTES: u64 = 64 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Member {
    pub path: PathBuf,
    pub size: u64,
    pub modified_at: u64,
    /// Full-precision modification time, used to spot changes since the scan.
    #[serde(skip)]
    pub mtime_ns: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Group {
    pub size: u64,
    /// Bytes that keeping one copy would free.
    pub wasted: u64,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Scan {
    pub id: u64,
    pub groups: Vec<Group>,
    pub reclaimable: u64,
    pub files_checked: u64,
    pub cancelled: bool,
}

pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

fn member_from(path: PathBuf, meta: &fs::Metadata) -> Member {
    let node = node_from_metadata(path.clone(), meta);
    Member {
        path,
        size: node.size,
        modified_at: node.modified_at,
        mtime_ns: mtime_ns(meta),
    }
}

/// Size and modification time of a regular file as it is on disk now.
/// Symlinks and folders give None.
pub fn disk_stat(path: &Path) -> Option<(u64, u128)> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((meta.len(), mtime_ns(&meta)))
}

/// Every regular, non-empty file under `root`. Symlinks are neither followed
/// nor reported, and the folders the search skips are left out. None if cancelled.
pub fn collect_folder(root: &Path, cancel: &AtomicBool) -> Option<Vec<Member>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // DirEntry::metadata does not follow symlinks.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let path = entry.path();
            if meta.is_dir() {
                if !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_skipped_dir)
                {
                    stack.push(path);
                }
            } else if meta.is_file() && meta.len() > 0 {
                out.push(member_from(path, &meta));
            }
        }
    }
    Some(out)
}

/// Every file the index knows about, checked against the disk. Rows whose
/// file is gone, is now a symlink, or is empty are dropped. Ok(None) if cancelled.
pub fn collect_index(db: &Path, cancel: &AtomicBool) -> Result<Option<Vec<Member>>, String> {
    let paths = crate::db::DbManager::indexed_file_paths(db).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() && meta.len() > 0 {
            out.push(member_from(path, &meta));
        }
    }
    Ok(Some(out))
}

fn partial_ranges(size: u64) -> Vec<(u64, u64)> {
    let mut ranges = vec![(0, size.min(EDGE_BYTES))];
    if size > EDGE_BYTES {
        ranges.push((size.saturating_sub(EDGE_BYTES).max(EDGE_BYTES), size));
    }
    ranges
}

/// SHA-256 of the given byte ranges, or of the whole file when `ranges` is
/// None. None if the file cannot be read or the scan was cancelled.
fn digest_file(
    path: &Path,
    ranges: Option<&[(u64, u64)]>,
    cancel: &AtomicBool,
) -> Option<[u8; 32]> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_BYTES];
    match ranges {
        Some(ranges) => {
            for &(start, end) in ranges {
                file.seek(SeekFrom::Start(start)).ok()?;
                let mut left = end - start;
                while left > 0 {
                    let want = left.min(CHUNK_BYTES as u64) as usize;
                    file.read_exact(&mut buf[..want]).ok()?;
                    hasher.update(&buf[..want]);
                    left -= want as u64;
                }
            }
        }
        None => loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let n = file.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        },
    }
    Some(hasher.finalize().into())
}

/// SHA-256 of a whole file, as the duplicate finder computes it. None if the
/// file cannot be read or the cancel flag is raised first.
pub fn whole_file_digest(path: &Path, cancel: &AtomicBool) -> Option<[u8; 32]> {
    digest_file(path, None, cancel)
}

/// Split each group by `digest`, keeping only the parts that still have two
/// or more files. None if cancelled.
fn refine(
    groups: Vec<Vec<Member>>,
    phase: &'static str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&'static str, u64, u64),
    digest: impl Fn(&Member) -> Option<[u8; 32]>,
) -> Option<Vec<Vec<Member>>> {
    let total: u64 = groups.iter().map(|g| g.len() as u64).sum();
    let mut done = 0;
    let mut out = Vec::new();
    for group in groups {
        let mut split: HashMap<[u8; 32], Vec<Member>> = HashMap::new();
        for m in group {
            let d = digest(&m);
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            done += 1;
            progress(phase, done, total);
            if let Some(d) = d {
                split.entry(d).or_default().push(m);
            }
        }
        out.extend(split.into_values().filter(|g| g.len() > 1));
    }
    Some(out)
}

fn sort_groups(groups: &mut [Group]) {
    groups.sort_by(|a, b| {
        b.wasted
            .cmp(&a.wasted)
            .then(b.size.cmp(&a.size))
            .then_with(|| a.members[0].path.cmp(&b.members[0].path))
    });
}

/// Find the sets of identical files among `candidates`. Stops early, with
/// `cancelled` set, once `cancel` is raised. `progress` is called as
/// (phase, done, total) for each file hashed.
pub fn group_copies(
    id: u64,
    candidates: Vec<Member>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Scan {
    let mut scan = Scan {
        id,
        files_checked: candidates.len() as u64,
        ..Scan::default()
    };
    let mut by_size: HashMap<u64, Vec<Member>> = HashMap::new();
    for m in candidates.into_iter().filter(|m| m.size > 0) {
        by_size.entry(m.size).or_default().push(m);
    }
    let by_size: Vec<Vec<Member>> = by_size.into_values().filter(|g| g.len() > 1).collect();

    let by_partial = refine(by_size, "partial", cancel, progress, |m| {
        digest_file(&m.path, Some(&partial_ranges(m.size)), cancel)
    });
    let Some(by_partial) = by_partial else {
        scan.cancelled = true;
        return scan;
    };
    let by_full = refine(by_partial, "full", cancel, progress, |m| {
        digest_file(&m.path, None, cancel)
    });
    let Some(by_full) = by_full else {
        scan.cancelled = true;
        return scan;
    };

    let mut groups: Vec<Group> = by_full
        .into_iter()
        .map(|mut members| {
            members.sort_by(|a, b| a.path.cmp(&b.path));
            let size = members[0].size;
            let wasted = size.saturating_mul(members.len() as u64 - 1);
            Group {
                size,
                wasted,
                members,
            }
        })
        .collect();
    sort_groups(&mut groups);
    scan.reclaimable = groups.iter().fold(0u64, |a, g| a.saturating_add(g.wasted));
    scan.groups = groups;
    scan
}

/// Collect and group the files under `root`. None if cancelled.
pub fn scan_folder(
    id: u64,
    root: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Option<Scan> {
    let candidates = collect_folder(root, cancel)?;
    Some(group_copies(id, candidates, cancel, progress))
}

/// Files the user picked to trash, after checking each against the scan.
#[derive(Debug, Default, PartialEq)]
pub struct TrashPlan {
    pub to_trash: Vec<PathBuf>,
    /// Requested files left alone, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Why a file is skipped because trashing it would leave its set with no
/// intact copy.
const NO_COPY_KEPT: &str = "no unchanged copy would be kept in its set";

/// Decide which requested files may be trashed. `stat` reads a file's size
/// and modification time on disk now. A file is skipped if it is gone or has
/// changed since the scan. Refuses, trashing nothing, if the result would
/// remove every copy in a set. A set is also skipped, with a reason, unless
/// some member that is not being trashed still matches the scan: otherwise
/// trashing its requested members could remove the last copy.
pub fn plan_trash(
    scan: &Scan,
    requested: &[PathBuf],
    stat: &dyn Fn(&Path) -> Option<(u64, u128)>,
) -> Result<TrashPlan, String> {
    let mut known: HashMap<&Path, &Member> = HashMap::new();
    for g in &scan.groups {
        for m in &g.members {
            known.insert(m.path.as_path(), m);
        }
    }
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut plan = TrashPlan::default();
    for path in requested {
        if !seen.insert(path.as_path()) {
            continue;
        }
        let Some(&member) = known.get(path.as_path()) else {
            plan.skipped
                .push((path.clone(), "not in a duplicate set".into()));
            continue;
        };
        match stat(path) {
            None => plan.skipped.push((path.clone(), "no longer exists".into())),
            Some((size, mtime)) if size != member.size || mtime != member.mtime_ns => plan
                .skipped
                .push((path.clone(), "changed since the scan".into())),
            Some(_) => plan.to_trash.push(path.clone()),
        }
    }

    let trashing: HashSet<&Path> = plan.to_trash.iter().map(|p| p.as_path()).collect();
    for g in &scan.groups {
        if g.members
            .iter()
            .all(|m| trashing.contains(m.path.as_path()))
        {
            let name = g.members[0]
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(format!(
                "Refused: that would remove every copy of {name}. Keep at least one file in each set."
            ));
        }
    }

    // A set keeps its content only if a member that is not being trashed
    // still matches the scan. Otherwise its requested members are skipped.
    let mut unsafe_paths: HashSet<PathBuf> = HashSet::new();
    for g in &scan.groups {
        let kept = g.members.iter().any(|m| {
            !trashing.contains(m.path.as_path())
                && stat(m.path.as_path()) == Some((m.size, m.mtime_ns))
        });
        if kept {
            continue;
        }
        for m in &g.members {
            if trashing.contains(m.path.as_path()) {
                unsafe_paths.insert(m.path.clone());
            }
        }
    }
    if !unsafe_paths.is_empty() {
        let (keep, skip): (Vec<PathBuf>, Vec<PathBuf>) = plan
            .to_trash
            .drain(..)
            .partition(|p| !unsafe_paths.contains(p));
        plan.to_trash = keep;
        plan.skipped
            .extend(skip.into_iter().map(|p| (p, NO_COPY_KEPT.to_string())));
    }
    Ok(plan)
}

/// Drop trashed files from the scan, and any set left with a single copy.
pub fn forget(scan: &mut Scan, removed: &[PathBuf]) {
    let gone: HashSet<&Path> = removed.iter().map(|p| p.as_path()).collect();
    for g in &mut scan.groups {
        g.members.retain(|m| !gone.contains(m.path.as_path()));
        g.wasted = g
            .size
            .saturating_mul(g.members.len().saturating_sub(1) as u64);
    }
    scan.groups.retain(|g| g.members.len() > 1);
    sort_groups(&mut scan.groups);
    scan.reclaimable = scan
        .groups
        .iter()
        .fold(0u64, |a, g| a.saturating_add(g.wasted));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn no_progress(_: &'static str, _: u64, _: u64) {}

    fn run(root: &Path) -> Scan {
        let cancel = AtomicBool::new(false);
        scan_folder(1, root, &cancel, &mut no_progress).expect("not cancelled")
    }

    fn names(group: &Group, root: &Path) -> Vec<String> {
        group
            .members
            .iter()
            .map(|m| {
                m.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// Two files of the same size and identical first and last 64 KiB, that
    /// differ only in the middle or at the very end.
    fn same_size_same_edges(tail: u8, middle: u8) -> Vec<u8> {
        let mut v = vec![7u8; 300 * 1024];
        v[150 * 1024] = middle;
        let n = v.len();
        v[n - 1] = tail;
        v
    }

    #[test]
    fn copies_are_confirmed_by_content_not_by_size_or_prefix() {
        let tmp = TempDir::new("dup_pipeline");
        let root = tmp.path();
        tmp.file("a.bin", &same_size_same_edges(1, 1));
        tmp.file("c.bin", &same_size_same_edges(1, 1));
        // Same size and same prefix and suffix, differs only at the very end.
        tmp.file("end_differs.bin", &same_size_same_edges(2, 1));
        // Same size, same edges, differs only in the middle: the partial hash
        // cannot see it, so only the full hash can rule it out.
        tmp.file("middle_differs.bin", &same_size_same_edges(1, 9));

        let scan = run(root);
        assert_eq!(scan.groups.len(), 1, "only a.bin and c.bin are copies");
        assert_eq!(names(&scan.groups[0], root), ["a.bin", "c.bin"]);
        assert_eq!(scan.groups[0].wasted, 300 * 1024);
        assert_eq!(scan.reclaimable, 300 * 1024);
        assert_eq!(scan.files_checked, 4);
    }

    #[test]
    fn empty_files_and_symlinks_are_never_candidates() {
        let tmp = TempDir::new("dup_skip");
        let root = tmp.path();
        tmp.file("empty1", b"");
        tmp.file("empty2", b"");
        let real = tmp.file("real.txt", b"hello world");
        tmp.file("real_copy.txt", b"hello world");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, root.join("link.txt")).unwrap();
        #[cfg(not(unix))]
        drop(real);

        let cancel = AtomicBool::new(false);
        let found = collect_folder(root, &cancel).unwrap();
        let paths: Vec<_> = found
            .iter()
            .map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(!paths.iter().any(|p| p.starts_with("empty")), "{paths:?}");
        assert!(!paths.iter().any(|p| p == "link.txt"), "{paths:?}");

        let scan = run(root);
        assert_eq!(scan.groups.len(), 1);
        assert_eq!(names(&scan.groups[0], root), ["real.txt", "real_copy.txt"]);
    }

    #[test]
    fn raised_cancel_flag_stops_the_scan_with_no_groups() {
        let tmp = TempDir::new("dup_cancel");
        tmp.file("a", b"same");
        tmp.file("b", b"same");
        let cancel = AtomicBool::new(true);
        assert!(collect_folder(tmp.path(), &cancel).is_none());
        let found = collect_folder(tmp.path(), &AtomicBool::new(false)).unwrap();
        let scan = group_copies(1, found, &cancel, &mut no_progress);
        assert!(scan.cancelled);
        assert!(scan.groups.is_empty());
    }

    #[test]
    fn cancel_raised_during_hashing_stops_before_the_full_stage() {
        let tmp = TempDir::new("dup_cancel_mid");
        tmp.file("a", &vec![1u8; 4096]);
        tmp.file("b", &vec![1u8; 4096]);
        let found = collect_folder(tmp.path(), &AtomicBool::new(false)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let scan = group_copies(1, found, &cancel, &mut |_, _, _| {
            calls += 1;
            cancel.store(true, Ordering::Relaxed);
        });
        assert!(scan.cancelled);
        assert!(scan.groups.is_empty());
        assert_eq!(calls, 1);
    }

    fn member(path: &str, size: u64, mtime: u128) -> Member {
        Member {
            path: PathBuf::from(path),
            size,
            modified_at: 0,
            mtime_ns: mtime,
        }
    }

    fn scan_of(groups: Vec<Vec<Member>>) -> Scan {
        let groups = groups
            .into_iter()
            .map(|members| Group {
                size: members[0].size,
                wasted: members[0].size * (members.len() as u64 - 1),
                members,
            })
            .collect();
        Scan {
            id: 1,
            groups,
            ..Scan::default()
        }
    }

    fn stat_from(scan: &Scan) -> impl Fn(&Path) -> Option<(u64, u128)> + '_ {
        move |p: &Path| {
            scan.groups
                .iter()
                .flat_map(|g| g.members.iter())
                .find(|m| m.path == p)
                .map(|m| (m.size, m.mtime_ns))
        }
    }

    #[test]
    fn refuses_to_trash_every_copy_of_a_set() {
        let scan = scan_of(vec![vec![
            member("/d/a", 10, 1),
            member("/d/b", 10, 2),
            member("/d/c", 10, 3),
        ]]);
        let stat = stat_from(&scan);
        let all = [
            PathBuf::from("/d/a"),
            PathBuf::from("/d/b"),
            PathBuf::from("/d/c"),
        ];
        let err = plan_trash(&scan, &all, &stat).unwrap_err();
        assert!(err.contains("every copy"), "{err}");

        let some = [PathBuf::from("/d/a"), PathBuf::from("/d/b")];
        let plan = plan_trash(&scan, &some, &stat).unwrap();
        assert_eq!(plan.to_trash, some);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_changed_copy_is_skipped_and_the_intact_kept_copy_survives() {
        let scan = scan_of(vec![vec![
            member("/d/a", 10, 1),
            member("/d/b", 10, 2),
            member("/d/c", 10, 3),
        ]]);
        // b was modified after the scan, so it is not trashed. c is intact and kept.
        let stat = |p: &Path| match p.to_string_lossy().as_ref() {
            "/d/a" => Some((10, 1)),
            "/d/b" => Some((10, 99)),
            "/d/c" => Some((10, 3)),
            _ => None,
        };
        let plan = plan_trash(
            &scan,
            &[PathBuf::from("/d/a"), PathBuf::from("/d/b")],
            &stat,
        )
        .unwrap();
        assert_eq!(plan.to_trash, [PathBuf::from("/d/a")]);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].0, PathBuf::from("/d/b"));
        assert_eq!(plan.skipped[0].1, "changed since the scan");
    }

    #[test]
    fn a_set_whose_kept_copy_is_gone_is_skipped_not_trashed() {
        let scan = scan_of(vec![
            vec![member("/d/a", 10, 1), member("/d/b", 10, 2)],
            vec![member("/e/x", 5, 1), member("/e/y", 5, 2)],
        ]);
        // b, the only copy not requested, was deleted outside the app.
        let stat = |p: &Path| match p.to_string_lossy().as_ref() {
            "/d/a" => Some((10, 1)),
            "/e/x" => Some((5, 1)),
            "/e/y" => Some((5, 2)),
            _ => None,
        };
        let plan = plan_trash(
            &scan,
            &[PathBuf::from("/d/a"), PathBuf::from("/e/y")],
            &stat,
        )
        .unwrap();
        assert_eq!(plan.to_trash, [PathBuf::from("/e/y")]);
        assert_eq!(
            plan.skipped,
            [(PathBuf::from("/d/a"), NO_COPY_KEPT.to_string())]
        );
    }

    #[test]
    fn a_set_whose_kept_copy_changed_is_skipped_not_trashed() {
        let scan = scan_of(vec![vec![member("/d/a", 10, 1), member("/d/b", 10, 2)]]);
        // b is the kept copy, but it was modified after the scan.
        let stat = |p: &Path| match p.to_string_lossy().as_ref() {
            "/d/a" => Some((10, 1)),
            "/d/b" => Some((10, 99)),
            _ => None,
        };
        let plan = plan_trash(&scan, &[PathBuf::from("/d/a")], &stat).unwrap();
        assert!(plan.to_trash.is_empty());
        assert_eq!(
            plan.skipped,
            [(PathBuf::from("/d/a"), NO_COPY_KEPT.to_string())]
        );
    }

    #[test]
    fn changed_on_disk_is_detected_against_a_real_scan() {
        let tmp = TempDir::new("dup_changed");
        let a = tmp.file("a.txt", b"same bytes");
        let b = tmp.file("b.txt", b"same bytes");
        // c is the intact copy that stays behind.
        tmp.file("c.txt", b"same bytes");
        let scan = run(tmp.path());
        assert_eq!(scan.groups.len(), 1);

        // Rewrite b with a different length, so its size no longer matches.
        std::fs::write(&b, b"changed, longer content").unwrap();
        let plan = plan_trash(&scan, &[a.clone(), b.clone()], &disk_stat).unwrap();
        assert_eq!(plan.to_trash, [a]);
        assert_eq!(plan.skipped, [(b, "changed since the scan".to_string())]);
    }

    #[test]
    fn unknown_and_missing_paths_are_skipped_not_trashed() {
        let scan = scan_of(vec![vec![
            member("/d/a", 10, 1),
            member("/d/b", 10, 2),
            member("/d/c", 10, 3),
        ]]);
        let stat = |p: &Path| match p.to_string_lossy().as_ref() {
            "/d/a" => Some((10, 1)),
            "/d/c" => Some((10, 3)),
            _ => None,
        };
        let plan = plan_trash(
            &scan,
            &[
                PathBuf::from("/d/a"),
                PathBuf::from("/d/b"),
                PathBuf::from("/other"),
            ],
            &stat,
        )
        .unwrap();
        assert_eq!(plan.to_trash, [PathBuf::from("/d/a")]);
        assert_eq!(plan.skipped.len(), 2);
        assert_eq!(plan.skipped[0].1, "no longer exists");
        assert_eq!(plan.skipped[1].1, "not in a duplicate set");
    }

    #[test]
    fn forget_drops_trashed_files_and_sets_left_with_one_copy() {
        let mut scan = scan_of(vec![
            vec![
                member("/d/a", 10, 1),
                member("/d/b", 10, 2),
                member("/d/c", 10, 3),
            ],
            vec![member("/e/x", 5, 1), member("/e/y", 5, 2)],
        ]);
        forget(&mut scan, &[PathBuf::from("/d/b"), PathBuf::from("/e/y")]);
        assert_eq!(scan.groups.len(), 1);
        assert_eq!(scan.groups[0].members.len(), 2);
        assert_eq!(scan.groups[0].wasted, 10);
        assert_eq!(scan.reclaimable, 10);
    }
}
