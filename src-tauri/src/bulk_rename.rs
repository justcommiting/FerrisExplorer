//! Bulk rename: several items renamed as one batch.
//!
//! Every item first moves to a unique temporary name in its own folder, and
//! only then to its target. That makes swaps and cycles (a→b, b→a, or
//! a→b→c→a) safe, because no target is ever occupied by an item that has not
//! left yet. Nothing is renamed onto an existing file. If a step fails, every
//! item is put back where it started.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::desktop::{is_same_entry, rename_without_clobbering};

/// How a batch ended. After a failure the batch has been rolled back, so
/// `done` is empty and `stranded` lists anything the rollback could not restore.
#[derive(Debug, Default, serde::Serialize)]
pub struct RenameOutcome {
    /// The `(from, to)` pairs renamed by this batch. Unchanged pairs are not listed.
    pub done: Vec<(PathBuf, PathBuf)>,
    /// The item that stopped the batch, and why.
    pub failure: Option<(PathBuf, String)>,
    /// Items the rollback could not put back: where each one is now, and where it started.
    pub stranded: Vec<(PathBuf, PathBuf)>,
}

/// Where a moving item is during the batch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Still at its original name.
    Source,
    /// At its temporary name.
    Temp,
    /// At its target name.
    Target,
}

/// Check one new name. Refused: empty, `.`/`..`, path separators, NUL, and on
/// Windows: the characters `: * ? " < > |`, control characters, a trailing dot
/// or space, and the reserved device names (`CON`, `COM1`, ...), with or
/// without an extension.
fn check_name_for(name: &str, windows: bool) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("the name is empty".into());
    }
    if name == "." || name == ".." {
        return Err(format!("'{name}' is not a valid name"));
    }
    if name.contains(['/', '\\', '\0']) {
        return Err("a name cannot contain / or \\".into());
    }
    if windows {
        if name.chars().any(|c| ":*?\"<>|".contains(c) || c < ' ') {
            return Err(
                "a name cannot contain any of : * ? \" < > | or control characters on Windows"
                    .into(),
            );
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Err("a name cannot end with a dot or a space on Windows".into());
        }
        if is_windows_reserved(name) {
            return Err(format!("'{name}' is a reserved device name on Windows"));
        }
    }
    Ok(())
}

/// True for the names Windows reserves for devices: `CON`, `PRN`, `AUX`,
/// `NUL`, `COM1`-`COM9` and `LPT1`-`LPT9`, case-insensitively. The part before
/// the first dot counts, so `nul.txt` is reserved too.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let numbered = upper
                .strip_prefix("COM")
                .or_else(|| upper.strip_prefix("LPT"));
            matches!(
                numbered,
                Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            )
        }
    }
}

fn check_name(name: &str) -> Result<(), String> {
    check_name_for(name, cfg!(windows))
}

/// Turn `(item, new name)` requests into `(from, to)` pairs. Each new name is
/// checked, and the target is built in the item's own folder, so it cannot
/// leave that folder.
pub fn targets(items: &[(PathBuf, String)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    items
        .iter()
        .map(|(from, name)| {
            check_name(name).map_err(|e| format!("{}: {e}", from.display()))?;
            let parent = from
                .parent()
                .ok_or_else(|| format!("{} has no folder to rename in", from.display()))?;
            Ok((from.clone(), parent.join(name)))
        })
        .collect()
}

/// Refuse a batch that could not be applied as a whole. Nothing is touched here.
fn check_batch(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    // Names differing only in case are the same name on macOS and Windows.
    let folded = cfg!(any(target_os = "macos", windows));
    let key = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if folded {
            s.to_lowercase()
        } else {
            s
        }
    };

    let mut sources = HashSet::new();
    let mut targets = HashSet::new();
    for (from, to) in pairs {
        if !sources.insert(key(from)) {
            return Err(format!("{} is in the batch twice", from.display()));
        }
        if to.parent() != from.parent() {
            return Err(format!("{} would leave its folder", to.display()));
        }
        if std::fs::symlink_metadata(from).is_err() {
            return Err(format!("{} no longer exists", from.display()));
        }
    }
    for (_, to) in pairs {
        if !targets.insert(key(to)) {
            return Err(format!("two items would both be named {}", to.display()));
        }
    }

    // A target that exists is fine only when the batch itself moves that
    // entry away. An unchanged item keeps its name, so it blocks the target.
    let moving: Vec<&(PathBuf, PathBuf)> = pairs.iter().filter(|(f, t)| f != t).collect();
    for (_, to) in &moving {
        if std::fs::symlink_metadata(to).is_ok() {
            let vacated = moving.iter().any(|(f, _)| is_same_entry(f, to));
            if !vacated {
                return Err(format!("{} already exists", to.display()));
            }
        }
    }
    Ok(())
}

/// A name in `dir` that nothing occupies yet.
fn unique_temp(dir: &Path) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = dir.join(format!(".ferris-rename-{}-{n}.tmp", std::process::id()));
        if std::fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
}

/// Apply a checked batch of `(from, to)` pairs. Refuses a batch that could not
/// be applied whole (see `check_batch`); after that, a failing step rolls back.
pub fn run(pairs: &[(PathBuf, PathBuf)]) -> Result<RenameOutcome, String> {
    run_with(pairs, |_| {})
}

/// `run`, calling `before_step` with the index of each step before it runs.
/// Steps `0..n` move each item to its temporary name, and steps `n..2n` move
/// each one from its temporary name to its target.
fn run_with(
    pairs: &[(PathBuf, PathBuf)],
    mut before_step: impl FnMut(usize),
) -> Result<RenameOutcome, String> {
    check_batch(pairs)?;
    let moving: Vec<(PathBuf, PathBuf)> = pairs.iter().filter(|(f, t)| f != t).cloned().collect();
    if moving.is_empty() {
        return Ok(RenameOutcome::default());
    }
    let n = moving.len();
    let temps: Vec<PathBuf> = moving
        .iter()
        .map(|(from, _)| unique_temp(from.parent().unwrap_or_else(|| Path::new("."))))
        .collect();
    let mut at = vec![Place::Source; n];

    for k in 0..n {
        before_step(k);
        let (from, _) = &moving[k];
        if let Err(e) = rename_without_clobbering(from, &temps[k]) {
            return Ok(rollback(&moving, &temps, &mut at, (from.clone(), e)));
        }
        at[k] = Place::Temp;
    }

    for k in 0..n {
        before_step(n + k);
        let (from, to) = &moving[k];
        if let Err(e) = rename_without_clobbering(&temps[k], to) {
            return Ok(rollback(&moving, &temps, &mut at, (from.clone(), e)));
        }
        at[k] = Place::Target;
    }

    Ok(RenameOutcome {
        done: moving,
        ..Default::default()
    })
}

/// Undo a partly applied batch after `failure`. Targets go back to their
/// temporary names first. A temporary name is free, because the item just
/// left it, so once every item is at a temporary name each original name is
/// free too. Then each item returns to its original name.
fn rollback(
    moving: &[(PathBuf, PathBuf)],
    temps: &[PathBuf],
    at: &mut [Place],
    failure: (PathBuf, String),
) -> RenameOutcome {
    let mut stranded = Vec::new();
    for k in (0..moving.len()).rev() {
        if at[k] == Place::Target {
            let (from, to) = &moving[k];
            match rename_without_clobbering(to, &temps[k]) {
                Ok(()) => at[k] = Place::Temp,
                Err(_) => stranded.push((to.clone(), from.clone())),
            }
        }
    }
    for k in (0..moving.len()).rev() {
        if at[k] == Place::Temp {
            let (from, _) = &moving[k];
            if rename_without_clobbering(&temps[k], from).is_err() {
                stranded.push((temps[k].clone(), from.clone()));
            }
        }
    }
    RenameOutcome {
        done: Vec::new(),
        failure: Some(failure),
        stranded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn request(p: &Path, name: &str) -> (PathBuf, String) {
        (p.to_path_buf(), name.to_string())
    }

    /// Names in a folder, sorted, so a test can see exactly what is there.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn read(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    /// Whether the temp folder ignores case, probed with a real file.
    fn case_insensitive(dir: &Path) -> bool {
        std::fs::write(dir.join("ci_probe.txt"), b"x").unwrap();
        dir.join("CI_PROBE.TXT").exists()
    }

    #[test]
    fn a_case_only_rename_in_a_batch_works() {
        let tmp = TempDir::new("bulk_case");
        if !case_insensitive(tmp.path()) {
            eprintln!("skipped: temp dir is case-sensitive");
            return;
        }
        let a = tmp.file("a.txt", b"a");
        let pairs = vec![(a.clone(), tmp.path().join("A.txt"))];
        run(&pairs).unwrap();
        assert!(names(tmp.path()).contains(&"A.txt".to_string()));
        assert_eq!(read(&tmp.path().join("A.txt")), b"a");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_renamed_onto_its_target_is_refused_in_a_batch() {
        let tmp = TempDir::new("bulk_symlink");
        let t = tmp.file("t.txt", b"precious");
        let l = tmp.path().join("l");
        std::os::unix::fs::symlink(&t, &l).unwrap();
        let x = tmp.file("x.txt", b"x");
        let pairs = vec![
            (x.clone(), tmp.path().join("y.txt")),
            (l.clone(), t.clone()),
        ];
        assert!(run(&pairs).is_err());
        assert_eq!(read(&t), b"precious");
        assert_eq!(read(&x), b"x");
        assert!(std::fs::symlink_metadata(&l)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_simple_batch_renames_every_item() {
        let tmp = TempDir::new("bulk_simple");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let pairs = targets(&[request(&a, "one.txt"), request(&b, "two.txt")]).unwrap();
        let out = run(&pairs).unwrap();
        assert!(out.failure.is_none() && out.stranded.is_empty());
        assert_eq!(out.done.len(), 2);
        assert_eq!(read(&tmp.path().join("one.txt")), b"a");
        assert_eq!(read(&tmp.path().join("two.txt")), b"b");
        assert_eq!(names(tmp.path()), vec!["one.txt", "two.txt"]);
    }

    #[test]
    fn a_swap_exchanges_the_names() {
        let tmp = TempDir::new("bulk_swap");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let pairs = targets(&[request(&a, "b.txt"), request(&b, "a.txt")]).unwrap();
        let out = run(&pairs).unwrap();
        assert!(out.failure.is_none());
        assert_eq!(read(&tmp.path().join("b.txt")), b"a");
        assert_eq!(read(&tmp.path().join("a.txt")), b"b");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn a_three_cycle_rotates_the_names() {
        let tmp = TempDir::new("bulk_cycle");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let c = tmp.file("c.txt", b"c");
        // a→b, b→c, c→a
        let pairs = targets(&[
            request(&a, "b.txt"),
            request(&b, "c.txt"),
            request(&c, "a.txt"),
        ])
        .unwrap();
        let out = run(&pairs).unwrap();
        assert!(out.failure.is_none());
        assert_eq!(out.done.len(), 3);
        assert_eq!(read(&tmp.path().join("b.txt")), b"a");
        assert_eq!(read(&tmp.path().join("c.txt")), b"b");
        assert_eq!(read(&tmp.path().join("a.txt")), b"c");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn a_cycle_undone_by_its_reverse_pairs_restores_the_originals() {
        let tmp = TempDir::new("bulk_cycle_undo");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let c = tmp.file("c.txt", b"c");
        let pairs = targets(&[
            request(&a, "b.txt"),
            request(&b, "c.txt"),
            request(&c, "a.txt"),
        ])
        .unwrap();
        run(&pairs).unwrap();
        let back: Vec<(PathBuf, PathBuf)> =
            pairs.iter().map(|(f, t)| (t.clone(), f.clone())).collect();
        let out = run(&back).unwrap();
        assert!(out.failure.is_none());
        assert_eq!(read(&a), b"a");
        assert_eq!(read(&b), b"b");
        assert_eq!(read(&c), b"c");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn a_collision_with_an_existing_file_refuses_the_whole_batch() {
        let tmp = TempDir::new("bulk_collide");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let taken = tmp.file("taken.txt", b"keep");
        let pairs = targets(&[request(&a, "taken.txt"), request(&b, "x.txt")]).unwrap();
        let err = run(&pairs).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(read(&a), b"a");
        assert_eq!(read(&b), b"b");
        assert_eq!(read(&taken), b"keep");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt", "taken.txt"]);
    }

    #[test]
    fn an_unchanged_item_blocks_a_target_with_its_name() {
        let tmp = TempDir::new("bulk_unchanged");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let pairs = vec![
            (a.clone(), tmp.path().join("b.txt")),
            (b.clone(), b.clone()),
        ];
        // The unchanged b.txt also claims that name, so the batch is refused as a duplicate target.
        let err = run(&pairs).unwrap_err();
        assert!(err.contains("b.txt"), "{err}");
        assert_eq!(read(&a), b"a");
        assert_eq!(read(&b), b"b");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn invalid_names_are_refused_before_anything_moves() {
        let tmp = TempDir::new("bulk_names");
        let a = tmp.file("a.txt", b"a");
        for bad in ["", "   ", ".", "..", "a/b", "../escape", "sub\\x", "x\0y"] {
            assert!(
                targets(&[request(&a, bad)]).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(check_name_for("a:b", true).is_err());
        assert!(check_name_for("a:b", false).is_ok());
        assert_eq!(read(&a), b"a");
        assert_eq!(names(tmp.path()), vec!["a.txt"]);
    }

    /// The default check, on the platform the tests run on.
    #[cfg(windows)]
    #[test]
    fn windows_names_are_refused_by_default_on_windows() {
        assert!(check_name("CON.txt").is_err());
        assert!(check_name("draft.").is_err());
        assert!(check_name("draft").is_ok());
    }

    #[test]
    fn windows_only_names_are_refused_on_windows_and_allowed_elsewhere() {
        for bad in [
            "file.",
            "file ",
            "CON",
            "con.txt",
            "Nul",
            "aux.tar.gz",
            "PRN",
            "COM1",
            "com9.log",
            "LPT1",
            "lpt5.txt",
            "a\u{1}b",
            "tab\there",
        ] {
            assert!(
                check_name_for(bad, true).is_err(),
                "{bad:?} should be refused on Windows"
            );
        }
        for ok in [
            "console.txt",
            "CONSOLE",
            "COM10",
            "LPT0",
            "com1x",
            "my nul",
            "a.con",
            "file.txt",
        ] {
            assert!(
                check_name_for(ok, true).is_ok(),
                "{ok:?} should be allowed on Windows"
            );
        }
        for elsewhere in ["CON", "file.", "file "] {
            assert!(
                check_name_for(elsewhere, false).is_ok(),
                "{elsewhere:?} is fine off Windows"
            );
        }
    }

    #[test]
    fn a_target_outside_the_parent_folder_is_refused() {
        let tmp = TempDir::new("bulk_outside");
        let a = tmp.file("a.txt", b"a");
        let sub = tmp.dir("sub");
        let pairs = vec![(a.clone(), sub.join("moved.txt"))];
        assert!(run(&pairs).unwrap_err().contains("leave its folder"));
        assert_eq!(read(&a), b"a");
        assert!(names(&sub).is_empty());
    }

    #[test]
    fn a_source_that_vanishes_mid_batch_rolls_everything_back() {
        let tmp = TempDir::new("bulk_vanish");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let c = tmp.file("c.txt", b"c");
        let pairs = targets(&[
            request(&a, "one.txt"),
            request(&b, "two.txt"),
            request(&c, "three.txt"),
        ])
        .unwrap();
        // b disappears after the check and before its step.
        let out = run_with(&pairs, |step| {
            if step == 1 {
                std::fs::remove_file(&b).unwrap();
            }
        })
        .unwrap();
        let (failed, _) = out.failure.expect("the batch should fail");
        assert_eq!(failed, b);
        assert!(out.done.is_empty());
        assert!(out.stranded.is_empty());
        assert_eq!(read(&a), b"a");
        assert_eq!(read(&c), b"c");
        assert_eq!(names(tmp.path()), vec!["a.txt", "c.txt"]);
    }

    #[test]
    fn a_failure_after_some_targets_are_placed_moves_them_back() {
        let tmp = TempDir::new("bulk_placed");
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let pairs = targets(&[request(&a, "one.txt"), request(&b, "two.txt")]).unwrap();
        // Steps 0 and 1 move both items aside, steps 2 and 3 place them. Step 3
        // places b, so a file appearing at two.txt first makes that placement fail.
        // a has already been placed at one.txt by then and must go back.
        let out = run_with(&pairs, |step| {
            if step == 3 {
                std::fs::write(tmp.path().join("two.txt"), b"blocker").unwrap();
            }
        })
        .unwrap();
        let (failed, _) = out.failure.expect("placing b should fail");
        assert_eq!(failed, b);
        assert!(out.done.is_empty());
        assert!(out.stranded.is_empty());
        assert_eq!(read(&a), b"a");
        assert_eq!(read(&b), b"b");
        assert_eq!(read(&tmp.path().join("two.txt")), b"blocker");
        assert_eq!(names(tmp.path()), vec!["a.txt", "b.txt", "two.txt"]);
    }
}
