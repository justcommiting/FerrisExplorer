//! User plugins: declarative manifests in `~/.ferris_explorer/plugins/*.toml`.
//!
//! A manifest names shell templates and file rules. It never holds code that
//! runs in the webview, and every template goes through `shell.rs`, which does
//! the quoting. A plugin does nothing until the user enables the exact bytes of
//! its file (see `PluginTrust`), and a changed file is disabled again.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive;
use crate::persistence::{PluginTrust, PluginTrustStore};
use crate::shell::{self, Captured, Quoting, ShellRun};
use crate::sys;

/// Largest manifest read, in bytes.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Plugin files past this count are reported and not loaded.
pub const MAX_PLUGINS: usize = 50;
const MAX_ACTIONS: usize = 50;
const MAX_PREVIEWERS: usize = 20;
const MAX_EXTENSIONS: usize = 50;
const MAX_COMMAND_BYTES: usize = 1000;
const DEFAULT_TIMEOUT_MS: u64 = 3000;
const MAX_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_OUTPUT: usize = 256 * 1024;
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
/// Placeholder paths for checking a template before any file is picked.
const SAMPLE_DIR: &str = if cfg!(windows) {
    "C:\\sample"
} else {
    "/sample"
};

// ── Manifest ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    action: Vec<RawAction>,
    #[serde(default)]
    previewer: Vec<RawPreviewer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    id: String,
    label: String,
    command: String,
    #[serde(default)]
    when: RawWhen,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    shortcut: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWhen {
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    min: Option<usize>,
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreviewer {
    extensions: Vec<String>,
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output: Option<usize>,
}

/// What the selection must be made of, for `when`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Any,
    Files,
    Dirs,
}

/// Which selections an action applies to. Validated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct When {
    /// Lowercase, without the dot. Empty means any name.
    pub extensions: Vec<String>,
    pub kind: Kind,
    pub min: usize,
    pub max: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub command: String,
    pub when: When,
    pub confirm: bool,
    pub shortcut: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Previewer {
    /// Lowercase, without the dot.
    pub extensions: Vec<String>,
    pub command: String,
    pub timeout_ms: u64,
    pub max_output: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub actions: Vec<Action>,
    pub previewers: Vec<Previewer>,
}

/// One entry of the selection, as `When` sees it.
pub struct Entry<'a> {
    pub name: &'a str,
    pub is_dir: bool,
}

impl When {
    pub fn matches(&self, picks: &[Entry]) -> bool {
        if picks.len() < self.min || self.max.is_some_and(|m| picks.len() > m) {
            return false;
        }
        match self.kind {
            Kind::Any => {}
            Kind::Files if picks.iter().any(|p| p.is_dir) => return false,
            Kind::Dirs if picks.iter().any(|p| !p.is_dir) => return false,
            _ => {}
        }
        self.extensions.is_empty()
            || picks
                .iter()
                .all(|p| !p.is_dir && self.extensions.contains(&ext_of(p.name)))
    }
}

impl Previewer {
    pub fn matches(&self, name: &str) -> bool {
        self.extensions.contains(&ext_of(name))
    }
}

/// The lowercase extension of a file name, or an empty string.
pub fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Parse and validate a manifest. Every problem is an error, so nothing
/// half-valid is ever loaded.
pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let raw: RawManifest = toml::from_str(text).map_err(|e| e.to_string())?;
    check_text("name", &raw.name, 80)?;
    check_text("version", &raw.version, 40)?;
    if !raw.description.is_empty() {
        check_text("description", &raw.description, 500)?;
    }
    if raw.action.len() > MAX_ACTIONS {
        return Err(format!("More than {MAX_ACTIONS} actions"));
    }
    if raw.previewer.len() > MAX_PREVIEWERS {
        return Err(format!("More than {MAX_PREVIEWERS} previewers"));
    }
    let mut actions: Vec<Action> = Vec::new();
    for a in raw.action {
        let at = |e: String| format!("action '{}': {e}", a.id);
        valid_id(&a.id).map_err(at)?;
        if actions.iter().any(|x| x.id == a.id) {
            return Err(at("the id is used twice".into()));
        }
        check_text("label", &a.label, 80).map_err(at)?;
        check_command("command", &a.command, false).map_err(at)?;
        if let Some(s) = &a.shortcut {
            check_text("shortcut", s, 40).map_err(at)?;
        }
        let when = validate_when(a.when).map_err(at)?;
        actions.push(Action {
            id: a.id,
            label: a.label,
            command: a.command,
            when,
            confirm: a.confirm,
            shortcut: a.shortcut,
        });
    }
    let mut previewers = Vec::new();
    for (i, p) in raw.previewer.into_iter().enumerate() {
        let at = |e: String| format!("previewer {}: {e}", i + 1);
        previewers.push(validate_previewer(p).map_err(at)?);
    }
    Ok(Manifest {
        name: raw.name,
        version: raw.version,
        description: raw.description,
        actions,
        previewers,
    })
}

fn check_text(what: &str, value: &str, max: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if value.len() > max {
        return Err(format!("{what} is longer than {max} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{what} contains a control character"));
    }
    Ok(())
}

fn valid_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err("the id must be 1 to 40 characters: a-z, 0-9, _ or -".into())
    }
}

/// A template must be one line and must plan with a sample path, which catches
/// bad placeholders (such as quoted ones) when the manifest loads.
fn check_command(what: &str, command: &str, needs_path: bool) -> Result<(), String> {
    check_text(what, command, MAX_COMMAND_BYTES)?;
    if needs_path && !command.contains("{}") {
        return Err(format!("{what} must contain {{}} for the file path"));
    }
    let dir = Path::new(SAMPLE_DIR);
    shell::plan(command, &[dir.join("sample.txt")], dir, Quoting::native())
        .map(|_| ())
        .map_err(|e| format!("{what}: {e}"))
}

fn normalize_ext(raw: &str) -> Result<String, String> {
    let e = raw.strip_prefix('.').unwrap_or(raw).to_ascii_lowercase();
    if e.is_empty() || e.len() > 16 || !e.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("'{raw}' is not a file extension"));
    }
    Ok(e)
}

fn extensions(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.len() > MAX_EXTENSIONS {
        return Err(format!("More than {MAX_EXTENSIONS} extensions"));
    }
    raw.iter().map(|e| normalize_ext(e)).collect()
}

fn validate_when(raw: RawWhen) -> Result<When, String> {
    let min = raw.min.unwrap_or(1);
    if min == 0 {
        return Err("when.min must be at least 1".into());
    }
    if raw.max.is_some_and(|m| m < min) {
        return Err("when.max is less than when.min".into());
    }
    Ok(When {
        extensions: extensions(&raw.extensions)?,
        kind: raw.kind,
        min,
        max: raw.max,
    })
}

fn validate_previewer(raw: RawPreviewer) -> Result<Previewer, String> {
    let extensions = extensions(&raw.extensions)?;
    if extensions.is_empty() {
        return Err("extensions is empty".into());
    }
    check_command("command", &raw.command, true)?;
    let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!("timeout_ms must be from 1 to {MAX_TIMEOUT_MS}"));
    }
    let max_output = raw.max_output.unwrap_or(DEFAULT_MAX_OUTPUT);
    if max_output == 0 || max_output > MAX_OUTPUT {
        return Err(format!("max_output must be from 1 to {MAX_OUTPUT} bytes"));
    }
    Ok(Previewer {
        extensions,
        command: raw.command,
        timeout_ms,
        max_output,
    })
}

// ── Files and trust ───────────────────────────────────────────────────────

/// A manifest read from disk, with the SHA-256 of its exact bytes.
#[derive(Debug)]
pub struct PluginFile {
    pub hash: String,
    pub manifest: Manifest,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Open a manifest only if it is a regular file. Symlinks are followed, as in
/// `scan`. A FIFO, socket or device is refused, and never opened: opening a
/// FIFO for reading blocks until a writer appears.
fn open_regular(path: &Path) -> Result<std::fs::File, String> {
    let read_err = |e: std::io::Error| format!("Could not read the file: {e}");
    if !std::fs::metadata(path).map_err(read_err)?.is_file() {
        return Err("Only regular files can be plugins".into());
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Non-blocking, so a file swapped for a FIFO after the check above
        // cannot hang the open. The fstat below refuses it.
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts.open(path).map_err(read_err)?;
    if !file.metadata().map_err(read_err)?.is_file() {
        return Err("Only regular files can be plugins".into());
    }
    Ok(file)
}

/// Read, size-check and parse one manifest file.
fn load_file(path: &Path) -> Result<PluginFile, String> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Could not read the file: {e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "The manifest is larger than {} KiB",
            MAX_MANIFEST_BYTES / 1024
        ));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "The manifest is not valid UTF-8".to_string())?;
    let manifest = parse_manifest(text)?;
    Ok(PluginFile {
        hash: sha256_hex(&bytes),
        manifest,
    })
}

/// A plain `*.toml` file name, with no folders in it.
fn is_plugin_file_name(name: &str) -> bool {
    Path::new(name).file_name() == Some(OsStr::new(name))
        && name.ends_with(".toml")
        && !name.starts_with('.')
}

/// Every `.toml` file in `dir`, sorted by name, each with its load result.
/// Files past the limit get an error instead of a load.
fn scan(dir: &Path) -> Vec<(String, Result<PluginFile, String>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".toml"))
        .collect();
    names.sort();
    names
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let loaded = if i < MAX_PLUGINS {
                load_file(&dir.join(&name))
            } else {
                Err(format!(
                    "Not loaded: over the limit of {MAX_PLUGINS} plugin files"
                ))
            };
            (name, loaded)
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct PluginView {
    pub file: String,
    pub hash: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub manifest: Manifest,
}

#[derive(Debug, Serialize)]
pub struct PluginError {
    pub file: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct PluginReport {
    pub dir: String,
    pub plugins: Vec<PluginView>,
    pub errors: Vec<PluginError>,
}

/// The plugin folder, which may not exist.
pub fn plugin_dir() -> PathBuf {
    sys::get_home_dir().join(".ferris_explorer").join("plugins")
}

/// Everything in `dir`, enabled or not, with invalid files listed as errors.
pub fn report(dir: &Path, trust: &PluginTrust) -> PluginReport {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    for (file, loaded) in scan(dir) {
        match loaded {
            Ok(pf) => {
                let enabled = trust.enabled.get(&file) == Some(&pf.hash);
                plugins.push(PluginView {
                    file,
                    hash: pf.hash,
                    enabled,
                    manifest: pf.manifest,
                });
            }
            Err(error) => errors.push(PluginError { file, error }),
        }
    }
    PluginReport {
        dir: dir.display().to_string(),
        plugins,
        errors,
    }
}

/// Enable or disable `file`. `hash` must be the hash the user was shown, so
/// what gets enabled is exactly what they reviewed.
pub fn set_enabled_in(
    dir: &Path,
    trust: &mut PluginTrust,
    file: &str,
    hash: &str,
    enabled: bool,
) -> Result<(), String> {
    if !is_plugin_file_name(file) {
        return Err("That is not a plugin file name".into());
    }
    let loaded = load_file(&dir.join(file))?;
    if loaded.hash != hash {
        return Err(
            "The file changed since it was listed. Reload the plugins and check it again.".into(),
        );
    }
    if enabled {
        trust.enabled.insert(file.to_string(), loaded.hash);
    } else {
        trust.enabled.remove(file);
    }
    Ok(())
}

/// The manifest of `file`, which must be enabled as it is on disk now.
fn enabled_plugin(dir: &Path, trust: &PluginTrust, file: &str) -> Result<PluginFile, String> {
    if !is_plugin_file_name(file) {
        return Err("That is not a plugin file name".into());
    }
    let loaded = load_file(&dir.join(file))?;
    if trust.enabled.get(file) != Some(&loaded.hash) {
        return Err("This plugin is not enabled. Enable it in the Plugins window first.".into());
    }
    Ok(loaded)
}

pub fn set_enabled(file: &str, hash: &str, enabled: bool) -> Result<PluginReport, String> {
    let dir = plugin_dir();
    let mut trust = PluginTrustStore::load();
    set_enabled_in(&dir, &mut trust, file, hash, enabled)?;
    PluginTrustStore::save(&trust)
        .map_err(|e| format!("Could not save the plugin settings: {e}"))?;
    Ok(report(&dir, &trust))
}

// ── Actions ───────────────────────────────────────────────────────────────

/// What running an action would do. The UI shows `lines` before a confirmed run.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ActionPlan {
    pub template: String,
    pub lines: Vec<String>,
    pub confirm: bool,
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Check that the action applies to `paths`, then plan its command with shell.rs.
pub fn plan_action_in(
    dir: &Path,
    trust: &PluginTrust,
    file: &str,
    action_id: &str,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<ActionPlan, String> {
    let pf = enabled_plugin(dir, trust, file)?;
    plan_loaded(&pf, action_id, paths, cwd)
}

/// Plan a run that the user confirmed. `hash` is the hash they were shown, so
/// the run is refused unless the plugin is enabled and unchanged since then.
pub fn plan_run_in(
    dir: &Path,
    trust: &PluginTrust,
    file: &str,
    hash: &str,
    action_id: &str,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<ActionPlan, String> {
    let pf = enabled_plugin(dir, trust, file)?;
    if pf.hash != hash {
        return Err(
            "The file changed since it was listed. Reload the plugins and check it again.".into(),
        );
    }
    plan_loaded(&pf, action_id, paths, cwd)
}

/// Plan a confirmed run from the current plugin settings and folder.
pub fn plan_run(
    file: &str,
    hash: &str,
    action_id: &str,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<ActionPlan, String> {
    plan_run_in(
        &plugin_dir(),
        &PluginTrustStore::load(),
        file,
        hash,
        action_id,
        paths,
        cwd,
    )
}

fn plan_loaded(
    pf: &PluginFile,
    action_id: &str,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<ActionPlan, String> {
    let action = pf
        .manifest
        .actions
        .iter()
        .find(|a| a.id == action_id)
        .ok_or_else(|| format!("No action '{action_id}' in this plugin"))?;
    let names: Vec<String> = paths.iter().map(|p| file_name_of(p)).collect();
    let picks: Vec<Entry> = names
        .iter()
        .zip(paths)
        .map(|(name, p)| Entry {
            name,
            is_dir: p.is_dir(),
        })
        .collect();
    if !action.when.matches(&picks) {
        return Err("This action doesn't apply to the selected files".into());
    }
    let lines = shell::checked_plan(&action.command, paths, cwd)?;
    Ok(ActionPlan {
        template: action.command.clone(),
        lines,
        confirm: action.confirm,
    })
}

pub fn plan_action(
    file: &str,
    action_id: &str,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<ActionPlan, String> {
    plan_action_in(
        &plugin_dir(),
        &PluginTrustStore::load(),
        file,
        action_id,
        paths,
        cwd,
    )
}

// ── Previewers ────────────────────────────────────────────────────────────

/// Plain text from an enabled previewer. Never HTML: the UI shows it as text.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct PluginPreview {
    pub plugin: String,
    pub text: String,
    pub truncated: bool,
    pub error: Option<String>,
    /// Stopped because the selection moved on. The UI ignores the result.
    pub cancelled: bool,
}

/// The first enabled previewer, in file-name order, for this file's extension.
fn previewer_for(dir: &Path, trust: &PluginTrust, path: &Path) -> Option<(String, Previewer)> {
    let name = path.file_name()?.to_str()?;
    for (file, loaded) in scan(dir) {
        let Ok(pf) = loaded else { continue };
        if trust.enabled.get(&file) != Some(&pf.hash) {
            continue;
        }
        let plugin = pf.manifest.name.clone();
        if let Some(p) = pf.manifest.previewers.into_iter().find(|p| p.matches(name)) {
            return Some((plugin, p));
        }
    }
    None
}

/// Run a matching previewer in the background and turn its output into text.
/// `run` is the caller's handle, so the caller can cancel it. Returns None
/// when no enabled previewer matches.
pub fn preview_in(
    dir: &Path,
    trust: &PluginTrust,
    path: &Path,
    run: &ShellRun,
) -> Result<Option<PluginPreview>, String> {
    if archive::locate(path).is_some() {
        return Ok(None);
    }
    let Some((plugin, previewer)) = previewer_for(dir, trust, path) else {
        return Ok(None);
    };
    let folder = path
        .parent()
        .ok_or_else(|| "This file has no folder".to_string())?;
    let lines = shell::checked_plan(&previewer.command, &[path.to_path_buf()], folder)?;
    let line = lines
        .first()
        .ok_or_else(|| "The previewer has no command to run".to_string())?;
    let cap: Captured = shell::capture(
        run,
        line,
        folder,
        Duration::from_millis(previewer.timeout_ms),
        previewer.max_output,
    )?;
    let error = if cap.timed_out {
        Some(format!("Timed out after {} ms", previewer.timeout_ms))
    } else if cap.cancelled || cap.overflow {
        None
    } else if cap.code != Some(0) {
        let err = cap.stderr.trim();
        Some(if err.is_empty() {
            format!("The previewer exited with code {:?}", cap.code)
        } else {
            err.to_string()
        })
    } else {
        None
    };
    Ok(Some(PluginPreview {
        plugin,
        text: cap.stdout,
        truncated: cap.overflow,
        error,
        cancelled: cap.cancelled,
    }))
}

/// How long a cancel for a not-yet-started preview is remembered. A queued
/// preview starts within moments; past this the cancel is stale (the UI's
/// request ids are per window, so an old one may name a newer request).
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct PreviewRuns {
    /// Previews currently running, by request id.
    running: HashMap<u64, Arc<ShellRun>>,
    /// Cancels that arrived before their preview started. The blocking task
    /// for a request can sit in the pool queue after the UI has moved on.
    early_cancels: HashMap<u64, Instant>,
}

/// Preview runs by request id, so the UI can cancel one when the selection moves on.
fn runs() -> &'static Mutex<PreviewRuns> {
    static RUNS: OnceLock<Mutex<PreviewRuns>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(PreviewRuns::default()))
}

pub fn preview(path: &Path, request: u64) -> Result<Option<PluginPreview>, String> {
    let run = Arc::new(ShellRun::new(request));
    // Register before any disk work, and consume an early cancel under the
    // same lock, so a cancel lands either here or on the registered run.
    if let Ok(mut m) = runs().lock() {
        if m.early_cancels
            .remove(&request)
            .is_some_and(|at| at.elapsed() < EARLY_CANCEL_TTL)
        {
            return Ok(Some(PluginPreview {
                plugin: String::new(),
                text: String::new(),
                truncated: false,
                error: None,
                cancelled: true,
            }));
        }
        m.running.insert(request, run.clone());
    }
    let out = preview_in(&plugin_dir(), &PluginTrustStore::load(), path, &run);
    if let Ok(mut m) = runs().lock() {
        m.running.remove(&request);
    }
    out
}

/// Stop a preview. A request that has not started yet is remembered and
/// cancelled when it does. Its own timeout still bounds it if it never starts.
pub fn cancel_preview(request: u64) {
    if let Ok(mut m) = runs().lock() {
        if let Some(run) = m.running.get(&request) {
            run.cancel();
        } else {
            let now = Instant::now();
            m.early_cancels
                .retain(|_, at| now.duration_since(*at) < EARLY_CANCEL_TTL);
            m.early_cancels.insert(request, now);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const VALID: &str = r#"
name = "Demo"
version = "1.2.0"
description = "A demo plugin"

[[action]]
id = "hash"
label = "Checksum"
command = "shasum {}"
when = { extensions = ["TXT", ".md"], kind = "files", min = 1, max = 3 }
confirm = true
shortcut = "Ctrl+Alt+H"

[[action]]
id = "list"
label = "List folder"
command = "ls -l {dir}"

[[previewer]]
extensions = ["json"]
command = "jq . {}"
timeout_ms = 1500
max_output = 4096
"#;

    fn entry(name: &str, is_dir: bool) -> Entry<'_> {
        Entry { name, is_dir }
    }

    #[test]
    fn parses_a_valid_manifest() {
        let m = parse_manifest(VALID).unwrap();
        assert_eq!(m.name, "Demo");
        assert_eq!(m.version, "1.2.0");
        assert_eq!(m.actions.len(), 2);
        let a = &m.actions[0];
        assert_eq!(a.when.extensions, vec!["txt", "md"]);
        assert_eq!(a.when.kind, Kind::Files);
        assert_eq!((a.when.min, a.when.max), (1, Some(3)));
        assert!(a.confirm);
        assert_eq!(a.shortcut.as_deref(), Some("Ctrl+Alt+H"));
        let b = &m.actions[1];
        assert_eq!(
            b.when,
            When {
                extensions: vec![],
                kind: Kind::Any,
                min: 1,
                max: None
            }
        );
        assert!(!b.confirm);
        assert_eq!(b.shortcut, None);
        let p = &m.previewers[0];
        assert_eq!((p.timeout_ms, p.max_output), (1500, 4096));
    }

    #[test]
    fn previewer_defaults_apply() {
        let m = parse_manifest(
            "name = \"x\"\nversion = \"1\"\n[[previewer]]\nextensions = [\"log\"]\ncommand = \"tail {}\"\n",
        )
        .unwrap();
        assert_eq!(m.previewers[0].timeout_ms, 3000);
        assert_eq!(m.previewers[0].max_output, 256 * 1024);
        assert!(m.actions.is_empty());
    }

    #[test]
    fn rejects_invalid_manifests() {
        let base = |extra: &str| format!("name = \"x\"\nversion = \"1\"\n{extra}");
        let action = |cmd: &str| {
            base(&format!(
                "[[action]]\nid = \"a\"\nlabel = \"A\"\ncommand = \"{cmd}\"\n"
            ))
        };
        let previewer = |fields: &str| {
            base(&format!(
                "[[previewer]]\nextensions = [\"txt\"]\ncommand = \"cat {{}}\"\n{fields}\n"
            ))
        };
        let cases: Vec<(String, &str)> = vec![
            (base("bogus = 1\n"), "unknown field"),
            ("version = \"1\"\n".to_string(), "name"),
            ("name = \"\"\nversion = \"1\"\n".to_string(), "name is empty"),
            (base("[[action]]\nid = \"Bad Id\"\nlabel = \"A\"\ncommand = \"ls\"\n"), "a-z"),
            // Double quotes are refused by both quoting modes; single quotes only
            // count as quoting on POSIX shells, not under cmd.
            (action("echo \\\"{}\\\""), "quoted automatically"),
            (action("ls\\nrm"), "control character"),
            (action(""), "command is empty"),
            (
                base("[[action]]\nid = \"a\"\nlabel = \"A\"\ncommand = \"ls\"\nwhen = { min = 0 }\n"),
                "at least 1",
            ),
            (
                base("[[action]]\nid = \"a\"\nlabel = \"A\"\ncommand = \"ls\"\nwhen = { min = 3, max = 2 }\n"),
                "less than",
            ),
            (
                base("[[action]]\nid = \"a\"\nlabel = \"A\"\ncommand = \"ls\"\nwhen = { extensions = [\"a/b\"] }\n"),
                "not a file extension",
            ),
            (
                base("[[action]]\nid = \"a\"\nlabel = \"A\"\ncommand = \"ls\"\nwhen = { kind = \"links\" }\n"),
                "unknown variant",
            ),
            (previewer("timeout_ms = 0"), "timeout_ms"),
            (previewer("timeout_ms = 10001"), "timeout_ms"),
            (previewer("max_output = 0"), "max_output"),
            (previewer(&format!("max_output = {}", MAX_OUTPUT + 1)), "max_output"),
            (
                base("[[previewer]]\nextensions = []\ncommand = \"cat {}\"\n"),
                "extensions is empty",
            ),
            (
                base("[[previewer]]\nextensions = [\"txt\"]\ncommand = \"cat\"\n"),
                "must contain",
            ),
        ];
        for (text, want) in cases {
            match parse_manifest(&text) {
                Ok(_) => panic!("accepted an invalid manifest with {want:?}:\n{text}"),
                Err(e) => assert!(e.contains(want), "error {e:?} lacks {want:?}"),
            }
        }
    }

    #[test]
    fn rejects_too_many_actions_and_bad_lines() {
        let many: String = (0..=MAX_ACTIONS)
            .map(|i| format!("[[action]]\nid = \"a{i}\"\nlabel = \"A\"\ncommand = \"ls\"\n"))
            .collect();
        let text = format!("name = \"x\"\nversion = \"1\"\n{many}");
        assert!(parse_manifest(&text).unwrap_err().contains("More than"));
    }

    #[test]
    fn when_matches_extensions_kinds_and_counts() {
        let txt = When {
            extensions: vec!["txt".into()],
            kind: Kind::Any,
            min: 1,
            max: None,
        };
        assert!(txt.matches(&[entry("A.TXT", false)]));
        assert!(!txt.matches(&[entry("a.png", false)]));
        assert!(!txt.matches(&[entry("folder", true)]));
        assert!(!txt.matches(&[entry("a.txt", false), entry("b.png", false)]));

        let files = When {
            extensions: vec![],
            kind: Kind::Files,
            min: 1,
            max: Some(2),
        };
        assert!(files.matches(&[entry("x", false), entry("y.bin", false)]));
        assert!(!files.matches(&[entry("d", true)]));
        assert!(!files.matches(&[entry("a", false), entry("b", false), entry("c", false)]));
        assert!(!files.matches(&[]));

        let dirs = When {
            extensions: vec![],
            kind: Kind::Dirs,
            min: 1,
            max: None,
        };
        assert!(dirs.matches(&[entry("d", true), entry("e", true)]));
        assert!(!dirs.matches(&[entry("d", true), entry("a.txt", false)]));
    }

    #[test]
    fn scan_loads_at_most_the_limit_and_rejects_large_files() {
        let tmp = TempDir::new("plugins_limits");
        let dir = tmp.dir("plugins");
        for i in 0..=MAX_PLUGINS {
            let text = format!("name = \"p{i}\"\nversion = \"1\"\n");
            std::fs::write(dir.join(format!("p{i:02}.toml")), text).unwrap();
        }
        let r = report(&dir, &PluginTrust::default());
        assert_eq!(r.plugins.len(), MAX_PLUGINS);
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].error.contains("over the limit"));

        let big = tmp.file("big.toml", &vec![b'#'; MAX_MANIFEST_BYTES as usize + 1]);
        assert!(load_file(&big).unwrap_err().contains("larger than"));
    }

    #[test]
    fn new_plugins_are_disabled_until_enabled_and_changes_disable_them_again() {
        let tmp = TempDir::new("plugins_trust");
        let dir = tmp.dir("plugins");
        std::fs::write(dir.join("demo.toml"), VALID).unwrap();
        let mut trust = PluginTrust::default();

        let r = report(&dir, &trust);
        assert_eq!(r.plugins.len(), 1);
        assert!(!r.plugins[0].enabled, "a new plugin must start disabled");
        let hash = r.plugins[0].hash.clone();

        // The UI passes the hash it showed. A wrong one is refused.
        assert!(set_enabled_in(&dir, &mut trust, "demo.toml", "0000", true).is_err());
        set_enabled_in(&dir, &mut trust, "demo.toml", &hash, true).unwrap();
        assert!(report(&dir, &trust).plugins[0].enabled);

        // Settings round-trip through the store file.
        let store = tmp.path().join("trust.json");
        PluginTrustStore::save_to(&store, &trust).unwrap();
        assert_eq!(PluginTrustStore::load_from(&store), trust);

        // Unchanged content stays enabled, even when reloaded.
        assert!(report(&dir, &PluginTrustStore::load_from(&store)).plugins[0].enabled);

        // Any edit to the manifest disables it again.
        std::fs::write(dir.join("demo.toml"), format!("{VALID}\n# edited\n")).unwrap();
        let changed = report(&dir, &trust);
        assert!(
            !changed.plugins[0].enabled,
            "a changed plugin must be disabled"
        );
        assert!(set_enabled_in(&dir, &mut trust, "demo.toml", &hash, true)
            .unwrap_err()
            .contains("changed since"));
        assert!(enabled_plugin(&dir, &trust, "demo.toml").is_err());
    }

    #[test]
    fn invalid_or_misnamed_files_cannot_be_enabled() {
        let tmp = TempDir::new("plugins_refuse");
        let dir = tmp.dir("plugins");
        std::fs::write(dir.join("bad.toml"), "name = ").unwrap();
        std::fs::write(dir.join("demo.toml"), VALID).unwrap();
        let mut trust = PluginTrust::default();
        assert!(set_enabled_in(&dir, &mut trust, "bad.toml", "x", true).is_err());
        assert!(set_enabled_in(&dir, &mut trust, "../demo.toml", "x", true).is_err());
        assert!(set_enabled_in(&dir, &mut trust, "demo.txt", "x", true).is_err());
        assert!(trust.enabled.is_empty());
    }

    #[test]
    fn disabling_removes_the_hash() {
        let tmp = TempDir::new("plugins_disable");
        let dir = tmp.dir("plugins");
        std::fs::write(dir.join("demo.toml"), VALID).unwrap();
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "demo.toml", &hash, true).unwrap();
        set_enabled_in(&dir, &mut trust, "demo.toml", &hash, false).unwrap();
        assert!(!report(&dir, &trust).plugins[0].enabled);
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn fifos_are_refused_without_blocking() {
        let tmp = TempDir::new("plugins_fifo");
        let dir = tmp.dir("plugins");
        make_fifo(&dir.join("pipe.toml"));
        let mut trust = PluginTrust::default();
        // A plain open would wait for a writer forever, so a regression hangs here.
        let err = set_enabled_in(&dir, &mut trust, "pipe.toml", "x", true).unwrap_err();
        assert!(err.contains("regular file"), "{err}");
        assert!(report(&dir, &trust).plugins.is_empty());
        assert!(trust.enabled.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_manifests_are_followed_like_scan() {
        let tmp = TempDir::new("plugins_link");
        let dir = tmp.dir("plugins");
        let target = tmp.file("real.toml", VALID.as_bytes());
        std::os::unix::fs::symlink(&target, dir.join("linked.toml")).unwrap();
        let r = report(&dir, &PluginTrust::default());
        assert_eq!(r.plugins.len(), 1);
        let hash = r.plugins[0].hash.clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "linked.toml", &hash, true).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_runs_are_refused_when_disabled_or_changed() {
        let tmp = TempDir::new("plugins_run");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("echo.toml"),
            "name = \"e\"\nversion = \"1\"\n[[action]]\nid = \"echo\"\nlabel = \"Echo\"\ncommand = \"echo {}\"\n",
        )
        .unwrap();
        let file = tmp.file("a.txt", b"x");
        let paths = std::slice::from_ref(&file);
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        let plan = |trust: &PluginTrust, hash: &str| {
            plan_run_in(&dir, trust, "echo.toml", hash, "echo", paths, tmp.path())
        };
        assert!(plan(&trust, &hash).unwrap_err().contains("not enabled"));

        set_enabled_in(&dir, &mut trust, "echo.toml", &hash, true).unwrap();
        let ok = plan(&trust, &hash).unwrap();
        assert_eq!(ok.lines, vec![format!("echo '{}'", file.display())]);
        // A hash other than the one shown is refused, even while enabled.
        assert!(plan(&trust, "0000").unwrap_err().contains("changed since"));

        // Disabling after the confirm dialog opened refuses the run.
        set_enabled_in(&dir, &mut trust, "echo.toml", &hash, false).unwrap();
        assert!(plan(&trust, &hash).unwrap_err().contains("not enabled"));

        // Editing the file after enabling refuses the run too.
        set_enabled_in(&dir, &mut trust, "echo.toml", &hash, true).unwrap();
        std::fs::write(dir.join("echo.toml"), "name = \"e\"\nversion = \"2\"\n").unwrap();
        assert!(plan(&trust, &hash).unwrap_err().contains("not enabled"));
    }

    #[cfg(unix)]
    #[test]
    fn action_template_is_planned_through_shell_quoting() {
        let tmp = TempDir::new("plugins_plan");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("echo.toml"),
            "name = \"e\"\nversion = \"1\"\n[[action]]\nid = \"echo\"\nlabel = \"Echo\"\ncommand = \"echo {}\"\n",
        )
        .unwrap();
        let file = tmp.file("with space.txt", b"x");
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        assert!(plan_action_in(
            &dir,
            &trust,
            "echo.toml",
            "echo",
            std::slice::from_ref(&file),
            tmp.path()
        )
        .is_err());
        set_enabled_in(&dir, &mut trust, "echo.toml", &hash, true).unwrap();
        let plan = plan_action_in(
            &dir,
            &trust,
            "echo.toml",
            "echo",
            std::slice::from_ref(&file),
            tmp.path(),
        )
        .unwrap();
        assert_eq!(plan.lines, vec![format!("echo '{}'", file.display())]);
        assert!(!plan.confirm);
        assert!(plan_action_in(&dir, &trust, "echo.toml", "missing", &[file], tmp.path()).is_err());
    }

    #[test]
    fn action_checks_the_selection_before_planning() {
        let tmp = TempDir::new("plugins_when");
        let dir = tmp.dir("plugins");
        std::fs::write(dir.join("demo.toml"), VALID).unwrap();
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "demo.toml", &hash, true).unwrap();
        let png = tmp.file("pic.png", b"x");
        let err =
            plan_action_in(&dir, &trust, "demo.toml", "hash", &[png], tmp.path()).unwrap_err();
        assert!(err.contains("doesn't apply"));
    }

    #[cfg(unix)]
    #[test]
    fn previewer_output_is_returned_as_text() {
        let tmp = TempDir::new("plugins_preview_ok");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("cat.toml"),
            "name = \"c\"\nversion = \"1\"\n[[previewer]]\nextensions = [\"log\"]\ncommand = \"cat {}\"\n",
        )
        .unwrap();
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "cat.toml", &hash, true).unwrap();
        let log = tmp.file("app.LOG", b"line one\n<b>not html</b>\n");
        let run = ShellRun::new(1);
        let p = preview_in(&dir, &trust, &log, &run).unwrap().unwrap();
        assert_eq!(p.text, "line one\n<b>not html</b>\n");
        assert_eq!(p.error, None);
        assert!(!p.truncated);
        // Files with no matching previewer fall through to the built-in preview.
        let other = tmp.file("notes.md", b"x");
        assert!(preview_in(&dir, &trust, &other, &run).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn slow_previewer_is_killed_at_its_timeout() {
        let tmp = TempDir::new("plugins_timeout");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("slow.toml"),
            "name = \"s\"\nversion = \"1\"\n[[previewer]]\nextensions = [\"txt\"]\ncommand = \"sleep 30; : {}\"\ntimeout_ms = 300\n",
        )
        .unwrap();
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "slow.toml", &hash, true).unwrap();
        let file = tmp.file("a.txt", b"x");
        let started = Instant::now();
        let p = preview_in(&dir, &trust, &file, &ShellRun::new(2))
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the sleep was not killed"
        );
        assert!(p.error.unwrap().contains("Timed out after 300 ms"));
        assert!(p.text.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn previewer_output_is_capped() {
        let tmp = TempDir::new("plugins_cap");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("flood.toml"),
            "name = \"f\"\nversion = \"1\"\n[[previewer]]\nextensions = [\"txt\"]\ncommand = \"head -c 100000 /dev/zero | tr '\\\\0' x; : {}\"\nmax_output = 1024\n",
        )
        .unwrap();
        let hash = report(&dir, &PluginTrust::default()).plugins[0]
            .hash
            .clone();
        let mut trust = PluginTrust::default();
        set_enabled_in(&dir, &mut trust, "flood.toml", &hash, true).unwrap();
        let file = tmp.file("a.txt", b"x");
        let p = preview_in(&dir, &trust, &file, &ShellRun::new(3))
            .unwrap()
            .unwrap();
        assert!(p.truncated);
        assert_eq!(p.text.len(), 1024);
        assert!(p.text.bytes().all(|b| b == b'x'));
    }

    #[cfg(unix)]
    #[test]
    fn disabled_previewers_do_not_run() {
        let tmp = TempDir::new("plugins_off");
        let dir = tmp.dir("plugins");
        std::fs::write(
            dir.join("cat.toml"),
            "name = \"c\"\nversion = \"1\"\n[[previewer]]\nextensions = [\"txt\"]\ncommand = \"cat {}\"\n",
        )
        .unwrap();
        let file = tmp.file("a.txt", b"secret");
        let p = preview_in(&dir, &PluginTrust::default(), &file, &ShellRun::new(4)).unwrap();
        assert!(p.is_none());
    }

    #[test]
    fn bundled_example_manifest_is_valid() {
        let m = parse_manifest(include_str!("../../docs/plugins/example.toml")).unwrap();
        assert_eq!(m.actions.len(), 2);
        assert_eq!(m.previewers.len(), 1);
        assert!(m.actions[1].confirm);
    }

    #[test]
    fn cancel_that_arrives_before_the_preview_starts_wins() {
        let request = 9_100_000_001;
        cancel_preview(request);
        let out = preview(Path::new("/nonexistent/example.txt"), request).expect("preview");
        assert!(out.is_some_and(|p| p.cancelled));
        let m = runs().lock().expect("runs lock");
        assert!(!m.running.contains_key(&request));
        assert!(!m.early_cancels.contains_key(&request));
    }
}
