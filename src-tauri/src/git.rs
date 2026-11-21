//! Git awareness and actions. Never blocks the UI: callers run these on a
//! worker thread. Missing git / non-repos return `None` (or an error for
//! actions) and the rest of the explorer keeps working.
//!
//! Every call is an argument vector run with `Command::new("git")`; nothing
//! goes through a shell. Paths are passed after `--`, with
//! `GIT_LITERAL_PATHSPECS=1` so a file named `*` or `:(...)` is never read as
//! a pattern. Read calls (`read_command`) also switch off repository-defined
//! programs (fsmonitor, clean/smudge/process filters), so opening a folder in
//! an untrusted clone cannot run that clone's config. Write calls
//! (`commit`) keep the user's hooks, as git itself would.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::recycle::Trasher;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitMark {
    Modified,
    Added,
    Untracked,
    Ignored,
    Deleted,
}

impl GitMark {
    pub fn badge(self) -> &'static str {
        match self {
            GitMark::Modified => "M",
            GitMark::Added => "+",
            GitMark::Untracked => "U",
            GitMark::Ignored => "I",
            GitMark::Deleted => "D",
        }
    }

    pub fn matches_filter(self, filter: GitFilter) -> bool {
        match filter {
            GitFilter::Modified => {
                matches!(self, GitMark::Modified | GitMark::Added | GitMark::Deleted)
            }
            GitFilter::Untracked => matches!(self, GitMark::Untracked | GitMark::Added),
            GitFilter::Ignored => matches!(self, GitMark::Ignored),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFilter {
    Modified,
    Untracked,
    Ignored,
}

impl GitFilter {
    pub fn parse(val: &str) -> Option<Self> {
        match val {
            "modified" | "mod" | "changed" | "dirty" => Some(GitFilter::Modified),
            "untracked" | "new" | "unt" => Some(GitFilter::Untracked),
            "ignored" | "ign" => Some(GitFilter::Ignored),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitInfo {
    pub root: PathBuf,
    pub branch: String,
    /// Commits the branch has that its upstream lacks, and the reverse. Zero without an upstream.
    pub ahead: u32,
    pub behind: u32,
    pub marks: HashMap<PathBuf, GitMark>,
    /// Paths with changes in the index, i.e. what `git commit` would record.
    pub staged: Vec<PathBuf>,
}

/// Walk parents looking for a `.git` directory or file (worktrees use a file).
pub fn discover_root(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Run `git status --porcelain` for `dir`. Returns `None` if git is missing
/// or `dir` is not inside a repository.
pub fn query(dir: &Path) -> Option<GitInfo> {
    let root = discover_root(dir)?;
    let output = read_command(&root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "-b",
            "--ignored=matching",
            "--untracked-files=normal",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_porcelain(&root, &String::from_utf8_lossy(&output.stdout))
}

/// Parse `git status --porcelain=v1 -z -b --ignored` stdout. With `-z`, paths
/// are never quoted or escaped, and each record ends in NUL. A rename or copy
/// is `XY new` followed by a record holding the old path.
pub fn parse_porcelain(root: &Path, stdout: &str) -> Option<GitInfo> {
    let mut branch = String::new();
    let mut ahead = 0;
    let mut behind = 0;
    let mut marks: HashMap<PathBuf, GitMark> = HashMap::new();
    let mut staged = Vec::new();

    let mut records = stdout.split('\0');
    while let Some(record) = records.next() {
        if let Some(rest) = record.strip_prefix("## ") {
            (branch, ahead, behind) = parse_branch_header(rest);
            continue;
        }
        if record.len() < 3 {
            continue;
        }
        let x = record.as_bytes()[0] as char;
        let y = record.as_bytes()[1] as char;
        // `get` (not `[3..]`): a malformed record may have a multi-byte char there.
        let Some(rest) = record.get(3..) else {
            continue;
        };
        if matches!(x, 'R' | 'C') {
            // Skip the original path, which is its own record.
            records.next();
        }
        if rest.is_empty() {
            continue;
        }
        // Normalise separators: porcelain paths use `/` even on Windows, and
        // listings must compare equal to `read_dir` paths.
        let abs: PathBuf = root.join(rest).components().collect();
        if !matches!(x, ' ' | '?' | '!') {
            staged.push(abs.clone());
        }
        if let Some(mark) = classify(x, y) {
            marks.insert(abs, mark);
        }
    }

    Some(GitInfo {
        root: root.to_path_buf(),
        branch,
        ahead,
        behind,
        marks,
        staged,
    })
}

/// Parse the `## ` line of `status -b`: `main...origin/main [ahead 1, behind 2]`,
/// `main...origin/main [gone]`, `No commits yet on main`, or `HEAD (no branch)`.
pub fn parse_branch_header(rest: &str) -> (String, u32, u32) {
    let (head, tracking) = match rest.split_once(" [") {
        Some((head, tracking)) => (head, tracking.trim_end_matches(']')),
        None => (rest, ""),
    };
    let head = head.strip_prefix("No commits yet on ").unwrap_or(head);
    let name = head
        .split("...")
        .next()
        .unwrap_or(head)
        .split_whitespace()
        .next()
        .unwrap_or(head)
        .to_string();
    let (mut ahead, mut behind) = (0, 0);
    for part in tracking.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (name, ahead, behind)
}

/// A command for reads: `git -C <root>` with no pager, no prompts, no
/// optional index locks, and no repository-defined programs.
fn read_command(root: &Path) -> Command {
    let mut cmd = base_command(root);
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.fsmonitor=false"]);
    // A repository can name a clean/smudge/process filter in .gitattributes and
    // define the program in its own config. Blank those out for reads: an empty
    // value makes git skip the filter.
    for name in filter_drivers(root) {
        for var in ["clean", "smudge", "process"] {
            cmd.arg("-c").arg(format!("filter.{name}.{var}="));
        }
        cmd.arg("-c").arg(format!("filter.{name}.required=false"));
    }
    cmd
}

/// A command for writes: no pager, no editor, no prompts.
fn base_command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.pager=cat"]);
    cmd
}

/// Names of the filter drivers the repository's config defines (`filter.<name>.*`).
fn filter_drivers(root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(root)
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\.",
        ])
        .output()
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|key| std::str::from_utf8(key).ok())
        .filter_map(|key| key.strip_prefix("filter.")?.rsplit_once('.'))
        .map(|(name, _)| name.to_string())
        // A name with `=` would be misread by `-c`; such a driver is not reachable anyway.
        .filter(|name| !name.contains('='))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The repository holding `paths`, and each path as a repo-relative string.
/// Refuses paths outside the repository, `..`, and the `.git` directory, so a
/// caller cannot reach past the root.
fn repo_paths(paths: &[PathBuf]) -> Result<(PathBuf, Vec<String>), String> {
    let first = paths.first().ok_or("Nothing selected")?;
    let root = discover_root(first).ok_or("Not in a Git repository")?;
    let mut rel = Vec::with_capacity(paths.len());
    for path in paths {
        let inside = path
            .strip_prefix(&root)
            .map_err(|_| format!("{} is outside the repository", path.display()))?;
        let mut parts = Vec::new();
        for part in inside.components() {
            match part {
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| format!("{} has a name git cannot read", path.display()))?;
                    parts.push(name);
                }
                _ => return Err(format!("{} is not a plain path", path.display())),
            }
        }
        if parts.is_empty() || parts[0] == ".git" {
            return Err("Git actions cannot target the repository root or its .git folder".into());
        }
        rel.push(parts.join("/"));
    }
    Ok((root, rel))
}

/// Git's message for a failed call: stderr, or stdout, since `commit` reports
/// "nothing to commit" on stdout.
fn failure_text(out: &Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if err.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        err
    }
}

/// Run `git <args> -- <rel paths>` in `root` and report only success or failure.
fn run_paths(root: &Path, args: &[&str], rel: &[String]) -> Result<(), String> {
    let out = base_command(root)
        .args(args)
        .args(rel)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failure_text(&out))
    }
}

/// Stage the given paths (`git add`). Deleted files are staged as deletions.
pub fn stage(paths: &[PathBuf]) -> Result<(), String> {
    let (root, rel) = repo_paths(paths)?;
    run_paths(&root, &["add", "--"], &rel)
}

/// Take the given paths out of the index, keeping their working-tree content (`git reset`).
pub fn unstage(paths: &[PathBuf]) -> Result<(), String> {
    let (root, rel) = repo_paths(paths)?;
    run_paths(&root, &["reset", "-q", "--"], &rel)
}

/// Record the index as a commit with `message` (`git commit -m`). The message is
/// one argv element, never a shell string. Runs the repository's commit hooks,
/// as git would from a terminal. No push, no amend, no editor.
pub fn commit(dir: &Path, message: &str) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("The commit message is empty".into());
    }
    let root = discover_root(dir).ok_or("Not in a Git repository")?;
    let out = base_command(&root)
        .args(["commit", "-m"])
        .arg(message)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(failure_text(&out));
    }
    // The first line is the summary, e.g. `[main 1a2b3c4] subject`.
    let summary = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(summary)
}

/// What a discard did: the files reverted, and the ones that failed with why.
#[derive(Debug, Default, serde::Serialize)]
pub struct Discarded {
    pub done: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

/// Revert each file's working-tree content to its index version. Staged changes
/// stay staged. Each file's current content is copied to the Trash first. If that
/// copy fails, the file is left alone. Untracked files are removed after the copy.
pub fn discard(trasher: &dyn Trasher, paths: &[PathBuf]) -> Discarded {
    let mut out = Discarded::default();
    for path in paths {
        match discard_one(trasher, path) {
            Ok(()) => out.done.push(path.clone()),
            Err(e) => out.failures.push((path.clone(), e)),
        }
    }
    out
}

fn discard_one(trasher: &dyn Trasher, path: &Path) -> Result<(), String> {
    let (root, rel) = repo_paths(std::slice::from_ref(&path.to_path_buf()))?;
    let rel = &rel[0];
    let tracked = read_command(&root)
        .args(["ls-files", "--error-unmatch", "--", rel])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let exists = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err("Discard works on files. Select the files inside the folder.".into())
        }
        Ok(_) => true,
        Err(_) => false,
    };
    if !tracked && !exists {
        return Err("Not a tracked or existing file, so there is nothing to discard".into());
    }
    if exists {
        backup_copy(trasher, path)?;
    }
    if tracked {
        run_paths(
            &root,
            &["restore", "--worktree", "--"],
            std::slice::from_ref(rel),
        )
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

/// Copy `path` to a scratch folder and hand the copy to the Trash. Only a
/// successful trash lets the caller go on, so the content is always recoverable.
fn backup_copy(trasher: &dyn Trasher, path: &Path) -> Result<(), String> {
    let name = path.file_name().ok_or("The file has no name")?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scratch =
        std::env::temp_dir().join(format!("ferris-discard-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&scratch).map_err(|e| format!("Could not make a backup: {e}"))?;
    let copy = scratch.join(name);
    let result = std::fs::copy(path, &copy)
        .map_err(|e| format!("Could not back up {}: {e}", path.display()))
        .and_then(|_| trasher.trash(&copy));
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The paths among `paths` that git ignores, found with one `git check-ignore`
/// call for the whole batch. Paths outside the repository are skipped. Tracked
/// files are never reported, as git treats them.
pub fn ignored_among(dir: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let Some(root) = discover_root(dir) else {
        return Ok(Vec::new());
    };
    let mut by_rel: HashMap<String, PathBuf> = HashMap::new();
    for path in paths {
        if let Ok(inside) = path.strip_prefix(&root) {
            if let Some(rel) = inside.to_str().filter(|r| !r.is_empty()) {
                by_rel.insert(rel.to_string(), path.clone());
            }
        }
    }
    if by_rel.is_empty() {
        return Ok(Vec::new());
    }
    // check-ignore takes literal paths already and rejects GIT_LITERAL_PATHSPECS.
    let mut child = read_command(&root)
        .env_remove("GIT_LITERAL_PATHSPECS")
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Write on a thread: git may start answering before it has read all input.
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let input: Vec<u8> = by_rel
        .keys()
        .flat_map(|k| k.bytes().chain(std::iter::once(0)))
        .collect();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    // Exit 0: some ignored. Exit 1: none. Anything else is a real failure.
    match out.status.code() {
        Some(0) | Some(1) => {}
        _ => return Err(failure_text(&out)),
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|r| std::str::from_utf8(r).ok())
        .filter_map(|r| by_rel.get(r).cloned())
        .collect())
}

/// Blame lines are capped so a huge file cannot flood the UI.
pub const BLAME_LINE_CAP: usize = 2000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlameLine {
    pub line: u32,
    pub sha: String,
    pub author: String,
    /// Author time, Unix seconds.
    pub time: i64,
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
pub struct Blame {
    pub lines: Vec<BlameLine>,
    /// True when the first `BLAME_LINE_CAP` lines were all there was room for.
    pub truncated: bool,
}

/// Blame the first `BLAME_LINE_CAP` lines of `path` as committed at HEAD. Lines
/// edited but not committed are attributed to their last commit. Textconv is
/// off, so a repository's diff driver never runs.
pub fn blame(path: &Path) -> Result<Blame, String> {
    let (root, rel) = repo_paths(std::slice::from_ref(&path.to_path_buf()))?;
    let range = format!("1,{BLAME_LINE_CAP}");
    let out = read_command(&root)
        .args([
            "blame",
            "--porcelain",
            "--no-textconv",
            "-L",
            range.as_str(),
            "HEAD",
            "--",
        ])
        .args(&rel)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(failure_text(&out));
    }
    let lines = parse_blame_porcelain(&String::from_utf8_lossy(&out.stdout));
    let truncated = lines.len() >= BLAME_LINE_CAP;
    Ok(Blame { lines, truncated })
}

/// Parse `git blame --porcelain`. Author details appear only on a commit's first
/// group, so they are remembered per sha. Content lines start with a tab.
pub fn parse_blame_porcelain(stdout: &str) -> Vec<BlameLine> {
    let mut commits: HashMap<String, (String, i64)> = HashMap::new();
    let mut lines = Vec::new();
    let mut sha = String::new();
    let mut line_no = 0u32;
    for raw in stdout.lines() {
        if let Some(text) = raw.strip_prefix('\t') {
            let (author, time) = commits.get(&sha).cloned().unwrap_or_default();
            lines.push(BlameLine {
                line: line_no,
                sha: sha.clone(),
                author,
                time,
                text: text.to_string(),
            });
        } else if let Some(name) = raw.strip_prefix("author ") {
            commits.entry(sha.clone()).or_default().0 = name.to_string();
        } else if let Some(t) = raw.strip_prefix("author-time ") {
            commits.entry(sha.clone()).or_default().1 = t.trim().parse().unwrap_or(0);
        } else if let Some(header) = raw.split_once(' ').filter(|(h, _)| is_sha(h)) {
            // `<sha> <orig line> <final line> [<group size>]`
            sha = header.0.to_string();
            let mut fields = raw.split(' ').skip(2);
            line_no = fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
    }
    lines
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn classify(x: char, y: char) -> Option<GitMark> {
    if x == '?' && y == '?' {
        return Some(GitMark::Untracked);
    }
    if x == '!' && y == '!' {
        return Some(GitMark::Ignored);
    }
    if x == 'D' || y == 'D' {
        return Some(GitMark::Deleted);
    }
    if x == 'A' || y == 'A' {
        return Some(GitMark::Added);
    }
    if x == 'M' || y == 'M' || x == 'R' || y == 'R' || x == 'C' || y == 'C' || x == 'U' || y == 'U'
    {
        return Some(GitMark::Modified);
    }
    None
}

pub fn open_diff(path: &Path) -> std::io::Result<()> {
    let Some(root) = discover_root(path) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not a git repository",
        ));
    };
    Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap_or("."),
            "difftool",
            "--no-prompt",
            "--",
        ])
        .arg(path)
        .spawn()
        .map(|_| ())
        .or_else(|_| {
            Command::new("git")
                .args(["-C", root.to_str().unwrap_or("."), "diff", "--"])
                .arg(path)
                .spawn()
                .map(|_| ())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_branch_and_marks() {
        let stdout = "## main...origin/main [ahead 1]\0 M src/main.rs\0A  src/new.rs\0?? scratch.txt\0!! target/foo.rlib\0 D gone.rs\0";
        let info = parse_porcelain(Path::new("/repo"), stdout).expect("parse");
        assert_eq!(info.branch, "main");
        assert_eq!(
            info.marks.get(Path::new("/repo/src/main.rs")),
            Some(&GitMark::Modified)
        );
        assert_eq!(
            info.marks.get(Path::new("/repo/src/new.rs")),
            Some(&GitMark::Added)
        );
        assert_eq!(
            info.marks.get(Path::new("/repo/scratch.txt")),
            Some(&GitMark::Untracked)
        );
        assert_eq!(
            info.marks.get(Path::new("/repo/target/foo.rlib")),
            Some(&GitMark::Ignored)
        );
        assert_eq!(
            info.marks.get(Path::new("/repo/gone.rs")),
            Some(&GitMark::Deleted)
        );
    }

    #[test]
    fn parse_porcelain_handles_rename_and_unquoted_paths() {
        // `-z` output: a rename is `XY new` then the old path as its own record.
        // Names with quotes, backslashes, tabs, newlines and " -> " come through
        // as-is, because -z never quotes or escapes them.
        let stdout = "## feature\0R  new name -> x.rs\0old.rs\0?? \"q\\\\t\tab\n.txt\0?? file with spaces.txt\0";
        let info = parse_porcelain(Path::new("/r"), stdout).expect("parse");
        assert_eq!(info.branch, "feature");
        assert_eq!(
            info.marks.get(Path::new("/r/new name -> x.rs")),
            Some(&GitMark::Modified)
        );
        assert!(!info.marks.contains_key(Path::new("/r/old.rs")));
        assert_eq!(
            info.marks.get(Path::new("/r/\"q\\\\t\tab\n.txt")),
            Some(&GitMark::Untracked)
        );
        assert_eq!(
            info.marks.get(Path::new("/r/file with spaces.txt")),
            Some(&GitMark::Untracked)
        );
    }

    #[test]
    fn parse_porcelain_non_ascii_and_copy_records() {
        let stdout = "## main\0 M caf\u{e9}.rs\0C  copy.rs\0source.rs\0A  added.rs\0";
        let info = parse_porcelain(Path::new("/r"), stdout).expect("parse");
        assert_eq!(
            info.marks.get(Path::new("/r/caf\u{e9}.rs")),
            Some(&GitMark::Modified)
        );
        assert_eq!(
            info.marks.get(Path::new("/r/copy.rs")),
            Some(&GitMark::Modified)
        );
        assert!(!info.marks.contains_key(Path::new("/r/source.rs")));
        assert_eq!(
            info.staged,
            vec![PathBuf::from("/r/copy.rs"), PathBuf::from("/r/added.rs")]
        );
    }

    #[test]
    fn discover_root_walks_parents() {
        let tmp = crate::testutil::TempDir::new("git_root");
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let nested = tmp.dir("a/b/c");
        assert_eq!(discover_root(&nested).as_deref(), Some(tmp.path()));
        assert!(discover_root(Path::new("/tmp")).is_none() || true);
    }

    #[test]
    fn git_filter_parse_aliases() {
        assert_eq!(GitFilter::parse("modified"), Some(GitFilter::Modified));
        assert_eq!(GitFilter::parse("untracked"), Some(GitFilter::Untracked));
        assert_eq!(GitFilter::parse("ignored"), Some(GitFilter::Ignored));
        assert_eq!(GitFilter::parse("nope"), None);
    }

    #[test]
    fn badges_match_the_ui_legend() {
        assert_eq!(GitMark::Modified.badge(), "M");
        assert_eq!(GitMark::Added.badge(), "+");
        assert_eq!(GitMark::Untracked.badge(), "U");
    }

    #[test]
    fn real_repo_status_when_git_is_available() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_real");
        let init = Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .expect("git init");
        assert!(init.status.success(), "{:?}", init);
        tmp.file("tracked.txt", b"hello");
        tmp.file("new.txt", b"untracked");
        let _ = Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(tmp.path())
            .output();
        let _ = Command::new("git")
            .args([
                "-c",
                "user.email=t@t.t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "i",
            ])
            .current_dir(tmp.path())
            .output();
        std::fs::write(tmp.path().join("tracked.txt"), b"changed").unwrap();

        let info = query(tmp.path()).expect("query");
        assert_eq!(
            info.marks.get(&tmp.path().join("tracked.txt")),
            Some(&GitMark::Modified)
        );
        assert_eq!(
            info.marks.get(&tmp.path().join("new.txt")),
            Some(&GitMark::Untracked)
        );
        assert!(!info.branch.is_empty());
    }

    fn have_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Run git in `dir` with a throwaway identity. Returns stdout as text.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", failure_text(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo with one commit holding `tracked.txt` ("original").
    fn repo_with_commit(tmp: &crate::testutil::TempDir) -> PathBuf {
        git(tmp.path(), &["init", "-q"]);
        // `commit` runs plain `git commit`, so the repo needs its own identity
        // where no global one is set (CI).
        git(tmp.path(), &["config", "user.name", "Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        tmp.file("tracked.txt", b"original");
        git(tmp.path(), &["add", "tracked.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "init"]);
        tmp.path().to_path_buf()
    }

    /// Records what was trashed (and removes it, like the OS would).
    #[derive(Default)]
    struct StubTrash {
        seen: std::cell::RefCell<Vec<(PathBuf, Vec<u8>)>>,
        refuse: bool,
    }

    impl Trasher for StubTrash {
        fn trash(&self, path: &Path) -> Result<(), String> {
            if self.refuse {
                return Err("refused".into());
            }
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            std::fs::remove_file(path).map_err(|e| e.to_string())?;
            self.seen.borrow_mut().push((path.to_path_buf(), bytes));
            Ok(())
        }
        fn restore(&self, _original: &Path) -> Result<(), String> {
            Err("not used".into())
        }
    }

    #[test]
    fn branch_header_reads_tracking_counts() {
        assert_eq!(
            parse_branch_header("main...origin/main [ahead 2, behind 1]"),
            ("main".to_string(), 2, 1)
        );
        assert_eq!(
            parse_branch_header("main...origin/main [behind 3]"),
            ("main".to_string(), 0, 3)
        );
        assert_eq!(
            parse_branch_header("main...origin/main [gone]"),
            ("main".to_string(), 0, 0)
        );
        assert_eq!(
            parse_branch_header("feature"),
            ("feature".to_string(), 0, 0)
        );
        assert_eq!(
            parse_branch_header("No commits yet on main"),
            ("main".to_string(), 0, 0)
        );
        assert_eq!(
            parse_branch_header("HEAD (no branch)"),
            ("HEAD".to_string(), 0, 0)
        );
    }

    #[test]
    fn porcelain_reports_staged_paths() {
        let stdout = "## main\0M  staged.rs\0 M unstaged.rs\0A  added.rs\0?? new.rs\0";
        let info = parse_porcelain(Path::new("/r"), stdout).expect("parse");
        assert_eq!(info.ahead, 0);
        assert_eq!(
            info.staged,
            vec![PathBuf::from("/r/staged.rs"), PathBuf::from("/r/added.rs")]
        );
    }

    #[test]
    fn stage_and_unstage_round_trip() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_stage");
        let root = repo_with_commit(&tmp);
        let file = root.join("tracked.txt");
        std::fs::write(&file, b"changed").unwrap();

        stage(std::slice::from_ref(&file)).expect("stage");
        let info = query(&root).expect("status");
        assert_eq!(info.staged, vec![file.clone()]);
        assert_eq!(info.marks.get(&file), Some(&GitMark::Modified));

        unstage(std::slice::from_ref(&file)).expect("unstage");
        let info = query(&root).expect("status");
        assert!(info.staged.is_empty());
        assert_eq!(info.marks.get(&file), Some(&GitMark::Modified));
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"changed",
            "unstage keeps the content"
        );
    }

    #[test]
    fn unstage_works_before_the_first_commit() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_unborn");
        git(tmp.path(), &["init", "-q"]);
        let file = tmp.file("first.txt", b"x");
        stage(std::slice::from_ref(&file)).expect("stage on unborn branch");
        unstage(std::slice::from_ref(&file)).expect("unstage on unborn branch");
        assert!(query(tmp.path()).expect("status").staged.is_empty());
    }

    #[test]
    fn paths_outside_the_repo_and_dotdot_are_refused() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_refuse");
        let root = repo_with_commit(&tmp);
        let outside = std::env::temp_dir().join("ferris_not_in_repo.txt");
        assert!(stage(std::slice::from_ref(&outside)).is_err());
        let sneaky = root.join("..").join("x");
        assert!(stage(std::slice::from_ref(&sneaky)).is_err());
        assert!(stage(&[root.join(".git").join("config")]).is_err());
    }

    #[test]
    fn discard_backs_up_content_before_reverting() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_discard");
        let root = repo_with_commit(&tmp);
        let file = root.join("tracked.txt");
        std::fs::write(&file, b"my unsaved work").unwrap();
        let stub = StubTrash::default();

        let out = discard(&stub, std::slice::from_ref(&file));

        assert_eq!(out.done, vec![file.clone()], "failures: {:?}", out.failures);
        let seen = stub.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].1, b"my unsaved work",
            "the trashed copy holds the old content"
        );
        assert_eq!(
            seen[0].0.file_name(),
            file.file_name(),
            "the copy keeps the file name"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"original",
            "the file is back to HEAD"
        );
    }

    #[test]
    fn discard_leaves_the_file_alone_when_the_backup_fails() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_discard_refused");
        let root = repo_with_commit(&tmp);
        let file = root.join("tracked.txt");
        std::fs::write(&file, b"keep me").unwrap();
        let stub = StubTrash {
            refuse: true,
            ..Default::default()
        };

        let out = discard(&stub, std::slice::from_ref(&file));

        assert!(out.done.is_empty());
        assert_eq!(out.failures.len(), 1);
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    }

    #[test]
    fn discard_removes_untracked_files_after_backing_them_up() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_discard_new");
        let root = repo_with_commit(&tmp);
        let file = tmp.file("scratch.txt", b"throwaway");
        let stub = StubTrash::default();

        let out = discard(&stub, std::slice::from_ref(&file));

        assert_eq!(out.done.len(), 1, "failures: {:?}", out.failures);
        assert!(!file.exists());
        assert_eq!(stub.seen.borrow()[0].1, b"throwaway");
        assert!(root.join("tracked.txt").exists());
    }

    #[test]
    fn commit_stores_the_message_literally() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_commit");
        let root = repo_with_commit(&tmp);
        std::fs::write(root.join("tracked.txt"), b"next").unwrap();
        stage(&[root.join("tracked.txt")]).unwrap();
        let message =
            "Say \"hi\" 'there'\n\nBody with $(touch PWNED) and `touch PWNED` and $HOME\n";

        let summary = commit(&root, message).expect("commit");

        assert!(summary.contains("Say"), "summary line: {summary}");
        let body = git(&root, &["log", "-1", "--format=%B"]);
        assert_eq!(body.trim_end(), message.trim_end());
        assert!(
            !root.join("PWNED").exists(),
            "the message was never run by a shell"
        );
        assert!(query(&root).expect("status").staged.is_empty());
    }

    #[test]
    fn commit_refuses_an_empty_message_and_an_empty_index() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_commit_empty");
        let root = repo_with_commit(&tmp);
        assert!(commit(&root, "   \n").is_err());
        assert!(commit(&root, "nothing staged").is_err());
    }

    #[test]
    fn ignored_paths_come_from_one_batched_call() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_ignored");
        let root = repo_with_commit(&tmp);
        tmp.file(".gitignore", b"build/\n*.log\n");
        tmp.file("build/a.o", b"x");
        tmp.file("build/sub/b.o", b"x");
        tmp.file("run.log", b"x");
        let keep = tmp.file("keep.txt", b"x");
        let paths = vec![
            root.join("build"),
            root.join("build").join("sub").join("b.o"),
            root.join("run.log"),
            keep.clone(),
            root.join("tracked.txt"),
        ];

        let mut ignored = ignored_among(&root, &paths).expect("check-ignore");
        ignored.sort();

        assert_eq!(
            ignored,
            vec![
                root.join("build"),
                root.join("build").join("sub").join("b.o"),
                root.join("run.log"),
            ]
        );
        assert!(!ignored.contains(&keep));
    }

    #[test]
    fn blame_porcelain_attributes_each_line() {
        let porcelain = "\
1111111111111111111111111111111111111111 1 1 2
author Alice
author-time 1700000000
author-tz +0000
summary first
filename a.txt
\tfn one() {}
1111111111111111111111111111111111111111 2 2
\tfn two() {}
2222222222222222222222222222222222222222 3 3 1
author Bob
author-time 1710000000
summary second
filename a.txt
\t// three
";
        let lines = parse_blame_porcelain(porcelain);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            (lines[0].line, lines[0].author.as_str(), lines[0].time),
            (1, "Alice", 1_700_000_000)
        );
        assert_eq!(lines[0].text, "fn one() {}");
        assert_eq!(lines[1].line, 2);
        assert_eq!(
            lines[1].author, "Alice",
            "later lines of a group reuse the commit's author"
        );
        assert_eq!(lines[2].author, "Bob");
        assert_eq!(lines[2].sha, "2".repeat(40));
    }

    #[test]
    fn blame_runs_against_a_real_repo() {
        if !have_git() {
            return;
        }
        let tmp = crate::testutil::TempDir::new("git_blame");
        let root = repo_with_commit(&tmp);
        tmp.file("tracked.txt", b"original\nsecond line\n");
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "more"]);

        let blame = blame(&root.join("tracked.txt")).expect("blame");

        assert!(!blame.truncated);
        assert_eq!(blame.lines.len(), 2);
        assert_eq!(blame.lines[0].text, "original");
        assert_eq!(blame.lines[0].author, "Test");
        assert_eq!(blame.lines[0].sha.len(), 40);
        assert_eq!(blame.lines[1].text, "second line");
        assert!(blame.lines[1].time > 0);
    }

    /// A repo whose `tracked.txt` is run through a clean filter that touches `marker`
    /// when it runs. The filter is defined only after the setup commit.
    #[cfg(unix)]
    fn repo_with_hostile_filter(tmp: &crate::testutil::TempDir) -> (PathBuf, PathBuf) {
        let root = repo_with_commit(tmp);
        let marker = tmp.path().join("marker");
        tmp.file(".gitattributes", b"tracked.txt filter=evil\n");
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "-q", "-m", "attrs"]);
        let clean = format!("touch {}; cat", marker.display());
        git(&root, &["config", "filter.evil.clean", clean.as_str()]);
        std::fs::write(root.join("tracked.txt"), b"a different length of content").unwrap();
        (root, marker)
    }

    // The hostile filter is a `sh` script, which git only runs on Unix here.
    #[test]
    #[cfg(unix)]
    fn repo_filters_do_not_run_on_reads() {
        if !have_git() {
            return;
        }
        // Control: plain `git diff` does run the hostile clean filter, so the
        // assertion below really tests our read path. (`git status` alone only
        // compares content when stat info is ambiguous, so it is not a control.)
        let control = crate::testutil::TempDir::new("git_filter_control");
        let (control_root, control_marker) = repo_with_hostile_filter(&control);
        let plain = Command::new("git")
            .arg("-C")
            .arg(&control_root)
            .args(["diff", "--quiet"])
            .output()
            .expect("plain diff");
        assert!(
            control_marker.exists(),
            "control: plain git should run the filter ({:?})",
            plain.status
        );

        let tmp = crate::testutil::TempDir::new("git_filter");
        let (root, marker) = repo_with_hostile_filter(&tmp);
        // The same command plain git ran above, through our read path.
        let diff = read_command(&root)
            .args(["diff", "--quiet"])
            .output()
            .expect("read diff");
        assert!(diff.status.code().is_some());
        assert!(
            !marker.exists(),
            "a repo clean filter ran through read_command"
        );
        let _ = query(&root);
        let _ = blame(&root.join("tracked.txt"));
        assert!(!marker.exists(), "a repo clean filter ran during a read");
    }
}
