//! Run a command the user types on the selected files.
//!
//! Placeholders in the command are replaced with shell-quoted paths. Everything
//! else is passed to the shell exactly as typed. Output streams back as events
//! and is kept in a bounded window; one run goes at a time.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::archive;
use crate::state::AppState;

/// Output kept per run, in bytes. The oldest lines go first.
pub const OUTPUT_CAP: usize = 2 * 1024 * 1024;
/// Longest piece of a line sent at once. A runaway line is split so it can't pile up in memory.
const MAX_LINE: u64 = 64 * 1024;
const FLUSH_LINES: usize = 200;
const FLUSH_EVERY: Duration = Duration::from_millis(50);
/// How long output may keep arriving after the command exits or is cancelled.
/// A background process that inherited the pipes can hold them open forever.
const EXIT_GRACE: Duration = Duration::from_millis(500);
const REAP_POLL: Duration = Duration::from_millis(10);
/// Characters cmd.exe can't pass through a double-quoted path (see `quote_cmd`).
const CMD_UNSAFE: &[char] = &['"', '%', '^', '&', '!', '\r', '\n', '\0'];

// ── Quoting ───────────────────────────────────────────────────────────────

/// How substituted paths are quoted for the shell that runs the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quoting {
    /// `$SHELL -c` on Unix: single quotes, with `'` written as `'\''`.
    Posix,
    /// `cmd /D /S /C` on Windows: double quotes, with some characters refused.
    Cmd,
}

impl Quoting {
    pub const fn native() -> Self {
        if cfg!(windows) {
            Quoting::Cmd
        } else {
            Quoting::Posix
        }
    }
}

/// Single quotes make every byte literal except `'` itself, so `'` is closed,
/// escaped, and reopened. Newlines and `$` stay literal inside single quotes.
pub fn quote_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double quotes stop cmd from splitting on spaces and `& | < > ( )`. But cmd
/// still expands `%VAR%` and `!VAR!` inside them, and a `"` can't be escaped
/// inside them at all. So a path containing any of those is refused rather
/// than risk running part of its name.
pub fn quote_cmd(s: &str) -> Result<String, String> {
    if let Some(c) = s.chars().find(|c| CMD_UNSAFE.contains(c)) {
        return Err(format!(
            "{s} contains {c:?}, which cmd.exe can't pass safely, so it can't be used in a command"
        ));
    }
    Ok(format!("\"{s}\""))
}

fn quote(s: &str, q: Quoting) -> Result<String, String> {
    match q {
        Quoting::Posix => Ok(quote_posix(s)),
        Quoting::Cmd => quote_cmd(s),
    }
}

fn text(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| {
        format!(
            "{} is not valid UTF-8, so it can't be put in a command",
            p.display()
        )
    })
}

// ── Template ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ph {
    /// `{}`: the path, once per file.
    Each,
    /// `{+}`: every path, in one invocation.
    All,
    /// `{dir}`: the folder the command runs in.
    Dir,
    /// `{name}`: the file name, once per file.
    Name,
    /// `{stem}`: the file name without its last extension. Unlike `{name}`, a
    /// leading `-` is not guarded with `./`, so `-x.txt` gives `-x`.
    Stem,
    /// `{ext}`: the extension, without the dot. A leading `-` is not guarded
    /// either (see `Stem`).
    Ext,
}

const PLACEHOLDERS: [(&str, Ph); 6] = [
    ("{}", Ph::Each),
    ("{+}", Ph::All),
    ("{dir}", Ph::Dir),
    ("{name}", Ph::Name),
    ("{stem}", Ph::Stem),
    ("{ext}", Ph::Ext),
];

#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Ph(Ph),
}

/// Split the template on placeholders. Single pass, so a substituted value is
/// never scanned for placeholders again. Unknown `{...}` stays literal text.
fn pieces(template: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < template.len() {
        let tail = &template[i..];
        if let Some((token, ph)) = PLACEHOLDERS.iter().find(|(t, _)| tail.starts_with(t)) {
            if text_start < i {
                out.push(Piece::Text(&template[text_start..i]));
            }
            out.push(Piece::Ph(*ph));
            i += token.len();
            text_start = i;
        } else {
            i += tail.chars().next().map_or(1, char::len_utf8);
        }
    }
    if text_start < template.len() {
        out.push(Piece::Text(&template[text_start..]));
    }
    out
}

/// Refuses placeholders that `render`'s quoting can't protect. That quoting is
/// only right when a placeholder is a bare word, so anything that could change
/// how the inserted text parses is refused rather than guessed at. Only the
/// template is scanned, never the paths, so a quote in a file name can't
/// confuse this.
/// True when a POSIX template has an unquoted `<<` (a here-document, `<<-`, or a
/// here-string). An unquoted here-document body expands `$()` and backticks,
/// and quotes in it are literal, so a placeholder there cannot be quoted safely.
fn has_heredoc(template: &str) -> bool {
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if single {
            single = c != '\'';
        } else if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            double = !double;
        } else if c == '\'' && !double {
            single = true;
        } else if c == '<' && !double && chars.peek() == Some(&'<') {
            return true;
        }
    }
    false
}

fn check_placeholders(template: &str, q: Quoting) -> Result<(), String> {
    let heredoc = q == Quoting::Posix && has_heredoc(template);
    // Quote state as the shell sees it. POSIX: `'` is literal inside single
    // quotes, and `\` escapes outside them and inside double quotes. cmd: `"`
    // toggles, and `^` escapes outside double quotes.
    let (mut single, mut double, mut escaped) = (false, false, false);
    // POSIX only. `backtick` is an open `...` command substitution. `comment`
    // is a `#` comment, which starts at a word start outside quotes and runs to
    // the end of the line, so its quotes do not count.
    let (mut backtick, mut comment) = (false, false);
    let mut word_start = true;
    let mut i = 0;
    while let Some(c) = template[i..].chars().next() {
        let tail = &template[i..];
        if let Some((token, _)) = PLACEHOLDERS.iter().find(|(t, _)| tail.starts_with(t)) {
            if single || double {
                return Err(format!(
                    "Placeholders are quoted automatically — remove the quotes around {token}"
                ));
            }
            if heredoc {
                return Err(format!(
                    "{token} can't go in a template with a here-document (<<), whose body expands $() and backticks"
                ));
            }
            // A line break in a file name would end a comment early, and a path
            // inside backticks would run as part of a command.
            if comment {
                return Err(format!(
                    "{token} can't go in a # comment, so write it outside one"
                ));
            }
            if backtick {
                return Err(format!(
                    "{token} can't go inside `...` command substitution, so write it outside the backticks"
                ));
            }
            let before = template[..i].chars().next_back();
            let after = template[i + token.len()..].chars().next();
            let near_special = match q {
                Quoting::Posix => matches!(before, Some('\\' | '$')) || after == Some('$'),
                Quoting::Cmd => {
                    matches!(before, Some('^' | '%' | '!')) || matches!(after, Some('%' | '!'))
                }
            };
            if escaped || near_special {
                return Err(format!(
                    "{token} can't be next to a backslash, $, ^, % or !, so write it as a bare word"
                ));
            }
            if q == Quoting::Posix && template[..i].contains("((") {
                return Err(format!(
                    "{token} can't go inside (( )) arithmetic, so write it outside it"
                ));
            }
            i += token.len();
            word_start = false;
            continue;
        }
        let (was_quoted, was_escaped) = (single || double, escaped);
        if comment {
            // Quotes and backticks are ignored until the line ends.
            if c == '\n' {
                comment = false;
            }
        } else if single {
            single = c != '\'';
        } else if escaped {
            escaped = false;
        } else if (q == Quoting::Posix && c == '\\') || (q == Quoting::Cmd && c == '^' && !double) {
            escaped = true;
        } else if c == '"' {
            double = !double;
        } else if q == Quoting::Posix && c == '\'' && !double {
            single = true;
        } else if q == Quoting::Posix && c == '`' {
            backtick = !backtick;
        } else if q == Quoting::Posix && c == '#' && word_start && !double {
            comment = true;
        }
        // A word starts after an unquoted, unescaped blank or operator.
        word_start = !was_quoted
            && !was_escaped
            && (c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'));
        i += c.len_utf8();
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// One invocation per file.
    PerFile,
    /// One invocation with every path. Paths are appended when there is no `{+}`.
    Batch,
}

fn mode_of(pieces: &[Piece]) -> Result<Mode, String> {
    let has = |want: Ph| pieces.contains(&Piece::Ph(want));
    let each = has(Ph::Each);
    let all = has(Ph::All);
    let per_file = each || has(Ph::Name) || has(Ph::Stem) || has(Ph::Ext);
    match (all, per_file) {
        (true, true) if each => {
            Err("Use {} (one run per file) or {+} (one run for all), not both".into())
        }
        (true, true) => {
            Err("{name}, {stem} and {ext} work per file, so use {} rather than {+}".into())
        }
        (true, false) => Ok(Mode::Batch),
        (false, true) => Ok(Mode::PerFile),
        (false, false) => Ok(Mode::Batch),
    }
}

/// The part of a file's path a placeholder stands for.
fn file_part(path: &Path, ph: Ph) -> Result<&str, String> {
    let part = match ph {
        Ph::Name => path.file_name(),
        Ph::Stem => path.file_stem(),
        Ph::Ext => path.extension(),
        _ => Some(path.as_os_str()),
    };
    match part {
        Some(p) => p.to_str().ok_or_else(|| {
            format!(
                "{} is not valid UTF-8, so it can't be put in a command",
                path.display()
            )
        }),
        None => Ok(""),
    }
}

fn render(
    pieces: &[Piece],
    q: Quoting,
    dir_quoted: &str,
    file: Option<&Path>,
    all_quoted: &str,
) -> Result<String, String> {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Ph(Ph::Dir) => out.push_str(dir_quoted),
            Piece::Ph(Ph::All) => out.push_str(all_quoted),
            Piece::Ph(ph) => {
                let Some(file) = file else {
                    return Err("{} and {name} need one run per file".into());
                };
                let part = file_part(file, *ph)?;
                // A bare `-name` would read as an option to the command.
                let value = if *ph == Ph::Name && part.starts_with('-') {
                    format!("./{part}")
                } else {
                    part.to_string()
                };
                out.push_str(&quote(&value, q)?);
            }
        }
    }
    Ok(out)
}

/// The exact command lines that would run, in order, without running them.
pub fn plan(
    template: &str,
    paths: &[PathBuf],
    dir: &Path,
    q: Quoting,
) -> Result<Vec<String>, String> {
    if template.trim().is_empty() {
        return Err("Type a command to run".into());
    }
    if paths.is_empty() {
        return Err("Select at least one file first".into());
    }
    check_placeholders(template, q)?;
    let pieces = pieces(template);
    let dir_quoted = quote(text(dir)?, q)?;
    match mode_of(&pieces)? {
        Mode::PerFile => paths
            .iter()
            .map(|p| render(&pieces, q, &dir_quoted, Some(p), ""))
            .collect(),
        Mode::Batch => {
            let all = paths
                .iter()
                .map(|p| quote(text(p)?, q))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ");
            let has_all = pieces.contains(&Piece::Ph(Ph::All));
            let line = render(&pieces, q, &dir_quoted, None, &all)?;
            Ok(vec![if has_all {
                line
            } else {
                format!("{} {all}", line.trim_end())
            }])
        }
    }
}

/// Refuse to run in an archive, whose contents are read-only virtual paths.
pub fn refuse_archive(dir: &Path, paths: &[PathBuf]) -> Result<(), String> {
    if archive::locate(dir).is_some() {
        return Err(format!(
            "{} is inside an archive, and archives are read-only, so commands can't run here",
            dir.display()
        ));
    }
    archive::ensure_writable(paths)
}

/// `plan`, after the archive check. This is what preview and start both use.
pub fn checked_plan(template: &str, paths: &[PathBuf], dir: &Path) -> Result<Vec<String>, String> {
    refuse_archive(dir, paths)?;
    plan(template, paths, dir, Quoting::native())
}

// ── Process handling ──────────────────────────────────────────────────────

/// The shell for `-c`. Only shells whose single quotes are literal are used,
/// because `quote_posix` depends on that. Anything else falls back to /bin/sh.
#[cfg(not(windows))]
fn posix_shell() -> PathBuf {
    if let Some(s) = std::env::var_os("SHELL").map(PathBuf::from) {
        let known = matches!(
            s.file_name().and_then(|n| n.to_str()),
            Some("sh" | "bash" | "zsh" | "dash" | "ksh")
        );
        if s.is_absolute() && known && s.is_file() {
            return s;
        }
    }
    PathBuf::from("/bin/sh")
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn command_for(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("cmd");
    // raw_arg hands cmd the line untouched. Rust's own argument quoting
    // would add escapes cmd doesn't understand.
    c.arg("/D")
        .arg("/S")
        .arg("/C")
        .raw_arg(format!("\"{line}\""))
        .creation_flags(CREATE_NO_WINDOW);
    c
}

#[cfg(not(windows))]
fn command_for(line: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(posix_shell());
    // Its own process group, so cancel can signal the whole tree at once.
    c.arg("-c").arg(line).process_group(0);
    c
}

fn spawn(line: &str, dir: &Path) -> std::io::Result<Child> {
    let mut cmd = command_for(line);
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Kill a command and everything it started.
#[cfg(windows)]
fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Kill a command and everything it started. spawn gave the child its own
/// process group, so a negative pid reaches the whole tree.
#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    // SAFETY: plain syscall with no pointers. A stale pgid just returns ESRCH.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// One run: its cancel flag, the process to kill, and whether it has finished.
pub struct ShellRun {
    pub id: u64,
    cancelled: AtomicBool,
    done: AtomicBool,
    pid: Mutex<Option<u32>>,
}

impl ShellRun {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            pid: Mutex::new(None),
        }
    }

    /// Stop the run. The flag is set first, so a process that starts just after
    /// this is killed in `attach`.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(pid) = self.pid.lock() {
            if let Some(p) = *pid {
                kill_tree(p);
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn attach(&self, pid: u32) {
        if let Ok(mut slot) = self.pid.lock() {
            *slot = Some(pid);
            if self.is_cancelled() {
                kill_tree(pid);
            }
        }
    }

    /// Reap the child if it has exited, clearing the pid in the same lock hold.
    /// `cancel` signals under that lock, so it never sees a pid that has been
    /// reaped and possibly reused. Returns the exit code once reaped.
    fn reap(&self, child: &mut Child) -> Option<Option<i32>> {
        let mut slot = self.pid.lock().unwrap_or_else(|e| e.into_inner());
        match child.try_wait() {
            Ok(Some(status)) => {
                *slot = None;
                Some(status.code())
            }
            Ok(None) => None,
            Err(_) => {
                *slot = None;
                Some(None)
            }
        }
    }
}

// ── Output ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Line {
    /// Position in the whole run, counting from 0. Lines below the first kept one were dropped.
    pub seq: u64,
    /// Came from stderr.
    pub err: bool,
    pub text: String,
}

/// Lines kept for one run, newest last. Once the byte count passes `cap`, the
/// oldest lines are dropped. The newest line is always kept.
pub struct Retained {
    lines: VecDeque<Line>,
    bytes: usize,
    cap: usize,
    next_seq: u64,
}

impl Retained {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            cap,
            next_seq: 0,
        }
    }

    pub fn push(&mut self, err: bool, text: String) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes += text.len();
        self.lines.push_back(Line { seq, err, text });
        while self.bytes > self.cap && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.text.len();
            }
        }
        seq
    }

    /// Sequence number of the oldest line still kept.
    pub fn first_seq(&self) -> u64 {
        self.lines.front().map_or(self.next_seq, |l| l.seq)
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The kept lines from `from` on.
    pub fn since(&self, from: u64) -> Vec<Line> {
        let start = self.lines.partition_point(|l| l.seq < from);
        self.lines.range(start..).cloned().collect()
    }
}

#[derive(Clone, Serialize)]
pub struct LinesEvent {
    pub run: u64,
    pub inv: usize,
    /// Lines below this were dropped from the window, so the client should drop them too.
    pub first_seq: u64,
    pub lines: Vec<Line>,
}

#[derive(Clone, Serialize)]
pub struct ExitEvent {
    pub run: u64,
    pub inv: usize,
    pub code: Option<i32>,
    pub cancelled: bool,
}

#[derive(Clone, Serialize)]
pub struct DoneEvent {
    pub run: u64,
    pub cancelled: bool,
    /// Stopped early because an invocation failed and stop-on-error was set.
    pub stopped: bool,
}

pub enum ShellEvent {
    Lines(LinesEvent),
    Exit(ExitEvent),
    Done(DoneEvent),
}

/// Batches lines into events, so a chatty command doesn't flood the UI.
struct Sink<'a> {
    run: u64,
    inv: usize,
    kept: Retained,
    /// Next sequence number not yet sent.
    sent: u64,
    pending: usize,
    last: Instant,
    emit: &'a mut dyn FnMut(ShellEvent),
}

impl Sink<'_> {
    fn push(&mut self, err: bool, text: String) {
        self.kept.push(err, text);
        self.pending += 1;
    }

    fn due(&self) -> bool {
        self.pending >= FLUSH_LINES || self.last.elapsed() >= FLUSH_EVERY
    }

    fn flush(&mut self) {
        let first = self.kept.first_seq();
        let lines = self.kept.since(self.sent.max(first));
        self.sent = self.kept.next_seq();
        self.pending = 0;
        self.last = Instant::now();
        if !lines.is_empty() {
            (self.emit)(ShellEvent::Lines(LinesEvent {
                run: self.run,
                inv: self.inv,
                first_seq: first,
                lines,
            }));
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: R,
    err: bool,
    tx: mpsc::Sender<(bool, String)>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut buf = Vec::new();
            match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
            }
            if tx
                .send((err, String::from_utf8_lossy(&buf).into_owned()))
                .is_err()
            {
                break;
            }
        }
    })
}

/// Run one command line and stream its output. Returns the exit code, or None
/// if it was killed by a signal or could not start.
fn run_one(run: &ShellRun, sink: &mut Sink, line: &str, dir: &Path) -> Option<i32> {
    let mut child = match spawn(line, dir) {
        Ok(c) => c,
        Err(e) => {
            sink.push(true, format!("Could not start the shell: {e}"));
            sink.flush();
            return None;
        }
    };
    run.attach(child.id());
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_reader(out, false, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_reader(err, true, tx.clone()));
    }
    drop(tx);
    let mut exit: Option<Option<i32>> = None;
    let mut stop_at: Option<Instant> = None;
    let mut truncated = false;
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok((err, text)) => {
                sink.push(err, text);
                if sink.due() {
                    sink.flush();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => sink.flush(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if exit.is_none() {
            if let Some(code) = run.reap(&mut child) {
                exit = Some(code);
                stop_at = Some(Instant::now() + EXIT_GRACE);
            }
        }
        if stop_at.is_none() && run.is_cancelled() {
            stop_at = Some(Instant::now() + EXIT_GRACE);
        }
        if stop_at.is_some_and(|t| Instant::now() >= t) {
            // A process outside the group still holds a pipe. Stop waiting for it.
            truncated = true;
            break;
        }
    }
    let code = match exit {
        Some(code) => code,
        None => loop {
            if let Some(code) = run.reap(&mut child) {
                break code;
            }
            std::thread::sleep(REAP_POLL);
        },
    };
    if truncated {
        sink.push(
            true,
            "output truncated: a background process kept the output open".into(),
        );
        // The readers are left to finish on their own. Joining would block on the pipe.
    } else {
        for r in readers {
            let _ = r.join();
        }
    }
    sink.flush();
    code
}

/// Run each command in turn in `dir`, sending events through `emit`. Stops
/// early when cancelled, or after a failure if `stop_on_error` is set.
pub fn execute(
    run: &ShellRun,
    commands: &[String],
    dir: &Path,
    stop_on_error: bool,
    emit: &mut dyn FnMut(ShellEvent),
) {
    let mut sink = Sink {
        run: run.id,
        inv: 0,
        kept: Retained::new(OUTPUT_CAP),
        sent: 0,
        pending: 0,
        last: Instant::now(),
        emit,
    };
    let mut stopped = false;
    for (inv, line) in commands.iter().enumerate() {
        if run.is_cancelled() {
            break;
        }
        sink.inv = inv;
        let code = run_one(run, &mut sink, line, dir);
        let cancelled = run.is_cancelled();
        (sink.emit)(ShellEvent::Exit(ExitEvent {
            run: run.id,
            inv,
            code,
            cancelled,
        }));
        if cancelled {
            break;
        }
        if code != Some(0) && stop_on_error {
            stopped = true;
            break;
        }
    }
    run.done.store(true, Ordering::SeqCst);
    (sink.emit)(ShellEvent::Done(DoneEvent {
        run: run.id,
        cancelled: run.is_cancelled(),
        stopped,
    }));
}

// ── Capture ───────────────────────────────────────────────────────────────

/// Stderr kept by `capture`. Enough for a short error message.
const CAPTURE_STDERR: usize = 4 * 1024;
/// How long `capture` waits after stopping a run before it gives up on the child.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// What `capture` got back. Output is kept up to the caps, and any early stop is reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
    /// Stdout was longer than the cap, so the command was stopped.
    pub overflow: bool,
    pub code: Option<i32>,
    pub timed_out: bool,
    /// Stopped through `ShellRun::cancel` by the caller.
    pub cancelled: bool,
}

#[derive(Default)]
struct Pipe {
    kept: Vec<u8>,
    over: bool,
}

fn drain<R: Read>(mut r: R, cap: usize, sink: Arc<Mutex<Pipe>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut p) = sink.lock() {
                    let room = cap.saturating_sub(p.kept.len());
                    if n > room {
                        p.kept.extend_from_slice(&buf[..room]);
                        p.over = true;
                    } else {
                        p.kept.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Overflow,
    Timeout,
}

/// Run one command line in `dir` and return its output, rather than streaming it
/// as events. Stdout is kept up to `max_stdout` bytes. The run is stopped (its
/// whole process group is killed) if it outlives `timeout`, writes more than
/// `max_stdout`, or `run` is cancelled. The caller owns `run`, so it can cancel
/// from another thread. Blocks until the child has exited, or until the output
/// has stopped arriving after it exited.
pub fn capture(
    run: &ShellRun,
    line: &str,
    dir: &Path,
    timeout: Duration,
    max_stdout: usize,
) -> Result<Captured, String> {
    let mut child = spawn(line, dir).map_err(|e| format!("Could not start the shell: {e}"))?;
    run.attach(child.id());
    let out = Arc::new(Mutex::new(Pipe::default()));
    let err = Arc::new(Mutex::new(Pipe::default()));
    let mut readers: Vec<JoinHandle<()>> = Vec::new();
    if let Some(s) = child.stdout.take() {
        let sink = out.clone();
        readers.push(std::thread::spawn(move || drain(s, max_stdout, sink)));
    }
    if let Some(s) = child.stderr.take() {
        let sink = err.clone();
        readers.push(std::thread::spawn(move || drain(s, CAPTURE_STDERR, sink)));
    }
    let started = Instant::now();
    let mut stop: Option<(Stop, Instant)> = None;
    let mut code = None;
    let mut exited_at: Option<Instant> = None;
    loop {
        if exited_at.is_none() {
            if let Some(c) = run.reap(&mut child) {
                code = c;
                exited_at = Some(Instant::now());
            }
        }
        if stop.is_none() {
            let over = out.lock().map(|p| p.over).unwrap_or(false);
            let reason = if run.is_cancelled() {
                Some(Stop::Cancelled)
            } else if over {
                Some(Stop::Overflow)
            } else if exited_at.is_none() && started.elapsed() >= timeout {
                Some(Stop::Timeout)
            } else {
                None
            };
            if let Some(reason) = reason {
                run.cancel();
                stop = Some((reason, Instant::now()));
            }
        }
        if let Some(t) = exited_at {
            if readers.iter().all(|h| h.is_finished()) || t.elapsed() >= EXIT_GRACE {
                break;
            }
        }
        // A child that ignores the kill must not hold the caller forever.
        if stop.is_some_and(|(_, at)| at.elapsed() >= KILL_WAIT) && exited_at.is_none() {
            break;
        }
        std::thread::sleep(REAP_POLL);
    }
    let text = |p: &Arc<Mutex<Pipe>>| {
        p.lock()
            .map(|p| String::from_utf8_lossy(&p.kept).into_owned())
            .unwrap_or_default()
    };
    let reason = stop.map(|(r, _)| r);
    Ok(Captured {
        stdout: text(&out),
        stderr: text(&err),
        overflow: out.lock().map(|p| p.over).unwrap_or(false),
        code,
        timed_out: reason == Some(Stop::Timeout),
        cancelled: reason == Some(Stop::Cancelled),
    })
}

// ── Commands ──────────────────────────────────────────────────────────────

/// Validate, claim the single run slot, and start the run on a thread. The
/// id comes from the client, so it can register the run before this returns.
pub fn start(
    app: tauri::AppHandle,
    state: &AppState,
    run_id: u64,
    template: &str,
    paths: &[PathBuf],
    dir: &Path,
    stop_on_error: bool,
) -> Result<Vec<String>, String> {
    let commands = checked_plan(template, paths, dir)?;
    start_planned(app, state, run_id, commands, dir, stop_on_error)
}

/// Start command lines that were already planned, such as a confirmed plugin
/// action. Same single run slot and events as `start`.
pub fn start_planned(
    app: tauri::AppHandle,
    state: &AppState,
    run_id: u64,
    commands: Vec<String>,
    dir: &Path,
    stop_on_error: bool,
) -> Result<Vec<String>, String> {
    let run = Arc::new(ShellRun::new(run_id));
    {
        let mut slot = state
            .shell_run
            .lock()
            .map_err(|_| "Command state is unavailable".to_string())?;
        if slot.as_ref().is_some_and(|r| !r.is_done()) {
            return Err("A command is already running. Cancel it first.".into());
        }
        *slot = Some(run.clone());
    }
    let thread_run = run.clone();
    let thread_commands = commands.clone();
    let dir = dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("shell-run".into())
        .spawn(move || {
            use tauri::Emitter;
            let mut emit = |ev: ShellEvent| match ev {
                ShellEvent::Lines(p) => {
                    let _ = app.emit("shell-lines", p);
                }
                ShellEvent::Exit(p) => {
                    let _ = app.emit("shell-exit", p);
                }
                ShellEvent::Done(p) => {
                    let _ = app.emit("shell-done", p);
                }
            };
            execute(
                &thread_run,
                &thread_commands,
                &dir,
                stop_on_error,
                &mut emit,
            );
        });
    if let Err(e) = spawned {
        run.done.store(true, Ordering::SeqCst);
        return Err(e.to_string());
    }
    Ok(commands)
}

/// Stop the running command, if there is one.
pub fn cancel(state: &AppState) {
    if let Ok(slot) = state.shell_run.lock() {
        if let Some(run) = slot.as_ref() {
            run.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::testutil::TempDir;

    /// Names that have broken naive quoting before, plus unicode.
    const NASTY: &[&str] = &[
        "plain.txt",
        "a; rm -rf ~",
        "$(touch pwned)",
        "`touch pwned`",
        "it's",
        "say \"hi\"",
        "two\nlines",
        "-rf",
        "back\\slash",
        "dollar$HOME",
        "amp & pipe | <tag>",
        "héllo wörld 日本",
        "  spaces  ",
        "trailing\\",
    ];

    #[cfg(unix)]
    fn posix_round_trip(name: &str) {
        let quoted = quote_posix(name);
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("run /bin/sh");
        assert!(out.status.success(), "sh failed for {name:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            name,
            "round trip of {name:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn posix_quoting_round_trips_through_sh() {
        for name in NASTY {
            posix_round_trip(name);
        }
    }

    #[test]
    fn posix_quoting_wraps_and_escapes_single_quotes() {
        assert_eq!(quote_posix("a b"), "'a b'");
        assert_eq!(quote_posix("it's"), "'it'\\''s'");
        assert_eq!(quote_posix(""), "''");
    }

    #[test]
    fn cmd_quoting_wraps_safe_names() {
        assert_eq!(quote_cmd("C:\\a b\\c.txt").unwrap(), "\"C:\\a b\\c.txt\"");
        // Characters that are only special outside quotes stay usable inside them.
        assert_eq!(quote_cmd("file (1) | x").unwrap(), "\"file (1) | x\"");
        assert_eq!(quote_cmd("héllo.txt").unwrap(), "\"héllo.txt\"");
    }

    #[test]
    fn cmd_quoting_refuses_unsafe_characters() {
        for bad in [
            "100%",
            "a^b",
            "x&y",
            "say \"hi\"",
            "wow!",
            "two\nlines",
            "cr\rx",
        ] {
            assert!(quote_cmd(bad).is_err(), "should refuse {bad:?}");
        }
        let err = quote_cmd("100%.txt").unwrap_err();
        assert!(err.contains("100%.txt"), "error names the path: {err}");
    }

    #[test]
    fn native_quoting_matches_the_platform() {
        let expect = if cfg!(windows) {
            Quoting::Cmd
        } else {
            Quoting::Posix
        };
        assert_eq!(Quoting::native(), expect);
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from("/work").join(n))
            .collect()
    }

    /// The path `paths` builds for `name`, as a string. It uses `join` like the
    /// planner does, so the separator is `\` on Windows and `/` elsewhere.
    fn joined(name: &str) -> String {
        Path::new("/work").join(name).display().to_string()
    }

    #[test]
    fn per_file_placeholders_run_once_per_file() {
        let got = plan(
            "convert {} {stem}.webp",
            &paths(&["a.png", "b c.jpg"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                format!("convert '{}' 'a'.webp", joined("a.png")),
                format!("convert '{}' 'b c'.webp", joined("b c.jpg")),
            ]
        );
    }

    #[test]
    fn name_stem_ext_and_dir_expand() {
        let got = plan(
            "echo {name} {stem} {ext} {dir}",
            &paths(&["archive.tar.gz"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(
            got,
            vec!["echo 'archive.tar.gz' 'archive.tar' 'gz' '/work'"]
        );
    }

    #[test]
    fn all_placeholder_runs_once_with_every_path() {
        let got = plan(
            "tar czf out.tgz {+}",
            &paths(&["a", "b c"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![format!(
                "tar czf out.tgz '{}' '{}'",
                joined("a"),
                joined("b c")
            )]
        );
    }

    #[test]
    fn no_placeholder_appends_all_paths() {
        let got = plan(
            "wc -l",
            &paths(&["a", "b"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![format!("wc -l '{}' '{}'", joined("a"), joined("b"))]
        );
    }

    #[test]
    fn placeholders_in_the_value_are_not_expanded_again() {
        // A file literally named {dir} must come out as a name, not a substitution.
        let got = plan(
            "echo {}",
            &paths(&["{dir}"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo '{}'", joined("{dir}"))]);
    }

    #[test]
    fn unknown_braces_are_left_alone() {
        let got = plan(
            "awk '{print $1}' {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("awk '{{print $1}}' '{}'", joined("x"))]);
    }

    #[test]
    fn mixing_per_file_and_all_is_refused() {
        assert!(plan(
            "a {} {+}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix
        )
        .is_err());
        assert!(plan(
            "a {name} {+}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix
        )
        .is_err());
    }

    #[test]
    fn empty_command_and_empty_selection_are_refused() {
        assert!(plan("   ", &paths(&["x"]), Path::new("/work"), Quoting::Posix).is_err());
        assert!(plan("ls", &[], Path::new("/work"), Quoting::Posix).is_err());
    }

    #[test]
    fn cmd_plan_refuses_a_path_with_an_unsafe_character() {
        let dir = Path::new("C:\\w");
        let bad = vec![PathBuf::from("C:\\w\\100%.txt")];
        let err = plan("type {}", &bad, dir, Quoting::Cmd).unwrap_err();
        assert!(err.contains("100%.txt"));
        let good = vec![PathBuf::from("C:\\w\\notes.txt")];
        let ok = plan("type {}", &good, dir, Quoting::Cmd).unwrap();
        assert_eq!(ok, vec!["type \"C:\\w\\notes.txt\""]);
    }

    #[test]
    fn nasty_names_are_quoted_so_the_shell_cannot_run_them() {
        // The rendered line must hold each name as one quoted word.
        for name in NASTY {
            let line = plan(
                "printf %s {}",
                &paths(&[name]),
                Path::new("/work"),
                Quoting::Posix,
            )
            .unwrap()
            .remove(0);
            let expected = format!("printf %s {}", quote_posix(&joined(name)));
            assert_eq!(line, expected, "name {name:?}");
        }
    }

    #[test]
    fn retained_drops_oldest_lines_over_the_cap() {
        let mut r = Retained::new(10);
        for i in 0..5 {
            r.push(false, format!("line{i}")); // 5 bytes each
        }
        let kept_bytes: usize = r.since(0).iter().map(|l| l.text.len()).sum();
        assert!(kept_bytes <= 10, "kept {kept_bytes} bytes");
        assert_eq!(r.next_seq(), 5);
        assert_eq!(r.first_seq(), 3, "lines 0 to 2 were dropped");
        let kept: Vec<String> = r.since(0).into_iter().map(|l| l.text).collect();
        assert_eq!(kept, vec!["line3", "line4"]);
    }

    #[test]
    fn retained_always_keeps_the_newest_line() {
        let mut r = Retained::new(4);
        r.push(false, "way longer than the cap".into());
        assert_eq!(r.since(0).len(), 1);
        assert_eq!(r.first_seq(), 0);
    }

    #[test]
    fn retained_since_skips_what_was_sent() {
        let mut r = Retained::new(100);
        r.push(false, "a".into());
        r.push(true, "b".into());
        r.push(false, "c".into());
        let tail = r.since(1);
        assert_eq!(tail.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert!(tail[0].err);
    }

    /// Runs `commands` with a collector and returns every event.
    #[cfg(unix)]
    fn run_collect(commands: &[&str], stop_on_error: bool) -> (Arc<ShellRun>, Vec<ShellEvent>) {
        let dir = TempDir::new("shell_run");
        let run = Arc::new(ShellRun::new(7));
        let cmds: Vec<String> = commands.iter().map(|s| s.to_string()).collect();
        let mut events = Vec::new();
        execute(&run, &cmds, dir.path(), stop_on_error, &mut |ev| {
            events.push(ev)
        });
        (run, events)
    }

    #[cfg(unix)]
    fn all_lines(events: &[ShellEvent]) -> Vec<(bool, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                ShellEvent::Lines(l) => Some(l.lines.clone()),
                _ => None,
            })
            .flatten()
            .map(|l| (l.err, l.text))
            .collect()
    }

    #[cfg(unix)]
    fn exits(events: &[ShellEvent]) -> Vec<Option<i32>> {
        events
            .iter()
            .filter_map(|e| match e {
                ShellEvent::Exit(x) => Some(x.code),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn output_streams_stdout_and_stderr_separately() {
        let (_, events) = run_collect(&["printf 'one\\ntwo\\n'; echo oops >&2"], false);
        let lines = all_lines(&events);
        assert!(lines.contains(&(false, "one".into())));
        assert!(lines.contains(&(false, "two".into())));
        assert!(lines.contains(&(true, "oops".into())));
        assert_eq!(exits(&events), vec![Some(0)]);
    }

    #[test]
    #[cfg(unix)]
    fn stop_on_error_halts_after_a_failure() {
        let (_, events) = run_collect(&["false", "echo never"], true);
        assert_eq!(exits(&events), vec![Some(1)]);
        assert!(!all_lines(&events).iter().any(|(_, t)| t == "never"));
        let stopped = events
            .iter()
            .any(|e| matches!(e, ShellEvent::Done(d) if d.stopped));
        assert!(stopped);
    }

    #[test]
    #[cfg(unix)]
    fn without_stop_on_error_every_invocation_runs() {
        let (_, events) = run_collect(&["false", "echo still"], false);
        assert_eq!(exits(&events), vec![Some(1), Some(0)]);
        assert!(all_lines(&events).iter().any(|(_, t)| t == "still"));
    }

    #[test]
    #[cfg(unix)]
    fn cancel_kills_a_running_sleep() {
        let dir = TempDir::new("shell_cancel");
        let run = Arc::new(ShellRun::new(9));
        let canceller = {
            let run = run.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                run.cancel();
            })
        };
        let started = Instant::now();
        let mut events = Vec::new();
        execute(&run, &["sleep 30".into()], dir.path(), false, &mut |ev| {
            events.push(ev)
        });
        canceller.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel did not stop sleep"
        );
        let cancelled_exit = events
            .iter()
            .any(|e| matches!(e, ShellEvent::Exit(x) if x.cancelled));
        assert!(cancelled_exit);
        assert!(run.is_done());
    }

    #[test]
    #[cfg(unix)]
    fn cancel_kills_children_of_the_shell_too() {
        let dir = TempDir::new("shell_tree");
        let run = Arc::new(ShellRun::new(10));
        let canceller = {
            let run = run.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                run.cancel();
            })
        };
        let started = Instant::now();
        let mut events = Vec::new();
        // Two sleeps in one shell: killing only the shell would leave the pipe open.
        execute(
            &run,
            &["sleep 30 | cat; sleep 30".into()],
            dir.path(),
            false,
            &mut |ev| events.push(ev),
        );
        canceller.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "tree was not killed"
        );
    }

    #[test]
    fn plan_keeps_selection_order() {
        let got = plan(
            "echo {}",
            &paths(&["b", "a"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                format!("echo '{}'", joined("b")),
                format!("echo '{}'", joined("a")),
            ]
        );
    }

    #[test]
    fn quoted_placeholders_are_refused() {
        for template in [
            "echo \"{}\"",
            "echo '{}'",
            "echo \"a {} b\"",
            "echo \"{name}\"",
        ] {
            let err =
                plan(template, &paths(&["x"]), Path::new("/work"), Quoting::Posix).unwrap_err();
            assert!(
                err.contains("remove the quotes around"),
                "{template:?} gave {err:?}"
            );
        }
        let err = plan(
            "type \"{}\"",
            &paths(&["x"]),
            Path::new("C:\\w"),
            Quoting::Cmd,
        )
        .unwrap_err();
        assert!(err.contains("remove the quotes around {}"), "{err}");
    }

    #[test]
    fn placeholders_next_to_special_characters_are_refused() {
        for template in ["echo \\{}", "echo ${}", "echo $(( {} ))", "echo {}$"] {
            assert!(
                plan(template, &paths(&["x"]), Path::new("/work"), Quoting::Posix).is_err(),
                "should refuse {template:?}"
            );
        }
        for template in ["type ^{}", "type %{}", "type !{}"] {
            assert!(
                plan(template, &paths(&["x"]), Path::new("C:\\w"), Quoting::Cmd).is_err(),
                "should refuse {template:?}"
            );
        }
    }

    #[test]
    fn a_quote_that_is_closed_before_the_placeholder_is_fine() {
        let got = plan(
            "echo 'a' \"b\" {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo 'a' \"b\" '{}'", joined("x"))]);
        // A double quote inside single quotes does not open a quote.
        let got = plan(
            "echo '\"' {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo '\"' '{}'", joined("x"))]);
        let good = vec![PathBuf::from("C:\\w\\notes.txt")];
        let ok = plan("type {}", &good, Path::new("C:\\w"), Quoting::Cmd).unwrap();
        assert_eq!(ok, vec!["type \"C:\\w\\notes.txt\""]);
    }

    #[test]
    fn placeholders_in_backticks_are_refused() {
        for template in ["echo `basename {}`", "echo `cat {}`"] {
            let err = plan(
                template,
                &paths(&["a`id`b"]),
                Path::new("/work"),
                Quoting::Posix,
            )
            .unwrap_err();
            assert!(
                err.contains("command substitution"),
                "{template:?} gave {err:?}"
            );
        }
        // A backtick pair that closes before the placeholder is fine.
        let got = plan(
            "echo `date` {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo `date` '{}'", joined("x"))]);
    }

    #[test]
    fn a_hash_comment_does_not_affect_quote_tracking() {
        // The quote in the comment must not open a quote that hides the placeholder.
        let err = plan(
            "# it's\necho '{}'",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap_err();
        assert!(err.contains("remove the quotes around"), "{err:?}");
        let got = plan(
            "# it's\necho {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("# it's\necho '{}'", joined("x"))]);
        // A placeholder inside the comment is refused: a line break in a file
        // name would end the comment and run the rest of the name as a command.
        let err = plan(
            "# {}\necho hi",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap_err();
        assert!(err.contains("# comment"), "{err:?}");
        // A `#` inside a word, or inside double quotes, does not start a comment.
        let got = plan(
            "echo a#b \"#'\" {}",
            &paths(&["x"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo a#b \"#'\" '{}'", joined("x"))]);
    }

    #[test]
    fn placeholders_in_a_heredoc_are_refused() {
        for template in [
            "cat <<EOF\n{}\nEOF",
            "cat <<-EOF\n\t{}\nEOF",
            "cat <<EOF {}\nx\nEOF",
        ] {
            let err = check_placeholders(template, Quoting::Posix).unwrap_err();
            assert!(err.contains("here-document"), "{template:?} gave {err:?}");
        }
        // Without a placeholder a here-document is fine.
        assert!(check_placeholders("cat <<EOF\nhi\nEOF", Quoting::Posix).is_ok());
        // A `<<` inside quotes is literal text, not a here-document.
        assert!(check_placeholders("echo '<<' {}", Quoting::Posix).is_ok());
    }

    #[test]
    fn quote_characters_in_a_file_name_do_not_confuse_the_template() {
        let name = "it's \"x\" \\{}";
        let got = plan(
            "echo {}",
            &paths(&[name]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec![format!("echo {}", quote_posix(&joined(name)))]);
    }

    #[test]
    fn name_with_a_leading_dash_gets_a_dot_slash() {
        let got = plan(
            "rm {name}",
            &paths(&["-rf"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec!["rm './-rf'"]);
        // {stem} is not guarded, which the doc comment on Ph::Stem records.
        let got = plan(
            "echo {stem}",
            &paths(&["-x.txt"]),
            Path::new("/work"),
            Quoting::Posix,
        )
        .unwrap();
        assert_eq!(got, vec!["echo '-x'"]);
    }

    #[test]
    #[cfg(unix)]
    fn a_background_process_holding_the_output_does_not_hang_the_run() {
        // setsid detaches from the process group on Linux. macOS has no setsid,
        // so nohup is used there. Either way the grandchild keeps stdout open.
        let escape = if Command::new("setsid").arg("--help").output().is_ok() {
            "setsid"
        } else {
            "nohup"
        };
        let dir = TempDir::new("shell_bg");
        let run = Arc::new(ShellRun::new(11));
        let line = format!("sh -c '{escape} sleep 30 &'");
        let mut events = Vec::new();
        let started = Instant::now();
        execute(&run, &[line], dir.path(), false, &mut |ev| events.push(ev));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "run waited on the background process: {:?}",
            started.elapsed()
        );
        assert!(run.is_done());
        assert_eq!(exits(&events), vec![Some(0)]);
        assert!(all_lines(&events).contains(&(
            true,
            "output truncated: a background process kept the output open".into()
        )));
    }
}
