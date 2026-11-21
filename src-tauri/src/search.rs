use crate::core::VNode;
use crate::git::GitFilter;
/// Identifies the frontend tab a background result belongs to.
pub type TabId = u64;
use std::path::{Path, PathBuf};

/// A batch of results from a background crawl, tagged with the tab that
/// requested them and the query they answer.
pub struct SearchUpdate {
    pub tab: TabId,
    pub query: String,
    pub results: Vec<VNode>,
    pub error: Option<String>,
    /// False for progressive walk batches so the UI keeps polling.
    pub complete: bool,
}

/// Known filter keys. Anything else of the form `key:value` is reported as
/// an error rather than silently becoming search text.
const KNOWN_FILTERS: &[&str] = &[
    "type",
    "ext",
    "size",
    "modified",
    "name",
    "content",
    "git",
    "duplicate",
    "here",
];

/// A parsed search query with optional filters and a text component.
#[derive(Debug, Clone, Default)]
pub struct ParsedQuery {
    /// The raw text portion for fuzzy name matching (filters stripped).
    pub text: String,
    /// Optional extension filter: e.g. "pdf", "rs", "jpg"
    pub ext: Option<String>,
    /// Additional filename constraint from `name:"Cargo"`.
    pub name: Option<String>,
    /// Substring to find inside file contents (`content:"TODO"`).
    pub content: Option<String>,
    /// Minimum file size in bytes.
    pub min_size: Option<u64>,
    /// Maximum file size in bytes.
    pub max_size: Option<u64>,
    /// Maximum age in seconds (files modified more recently than this).
    pub max_age_secs: Option<u64>,
    /// Minimum age in seconds (files modified longer ago than this).
    pub min_age_secs: Option<u64>,
    pub git: Option<GitFilter>,
    pub duplicates: bool,
    /// If true, search only the current directory without recursing into subdirs.
    pub scope_current_dir: bool,
    /// Resolved path to scope to — set by the caller when scope_current_dir is true.
    pub scope_path: Option<PathBuf>,
    /// Human-readable problems with the query (invalid sizes, unknown keys).
    pub errors: Vec<String>,
    /// Structured form if the input was rewritten from natural language.
    pub interpreted_as: Option<String>,
}

impl ParsedQuery {
    /// Parse a raw search string into structured filters + text.
    ///
    /// Supports: `type:pdf` `ext:rs` `size:>1mb` `size:<500kb`
    /// `modified:<30d` `modified:today` `name:"Cargo"` `content:"TODO"`
    /// `git:modified` `duplicate:true` `here:`
    ///
    /// Size units are binary (1 KB = 1024 B) and accept fractions ("1.5gb").
    /// Duration units are w/d/h/m/s, defaulting to seconds. A filter whose
    /// value does not parse is dropped rather than applied as `0`.
    pub fn parse(raw: &str) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self::parse_at(raw, now)
    }

    pub fn parse_at(raw: &str, now_secs: u64) -> Self {
        let (source, interpreted_as) = match crate::nl::rewrite_if_natural_language(raw) {
            Some(rewritten) => (rewritten.clone(), Some(rewritten)),
            None => (raw.to_string(), None),
        };

        let mut text_parts = Vec::new();
        let mut ext: Option<String> = None;
        let mut name: Option<String> = None;
        let mut content: Option<String> = None;
        let mut min_size: Option<u64> = None;
        let mut max_size: Option<u64> = None;
        let mut max_age_secs: Option<u64> = None;
        let mut min_age_secs: Option<u64> = None;
        let mut git: Option<GitFilter> = None;
        let mut duplicates = false;
        let mut scope_current_dir = false;
        let mut errors = Vec::new();

        for token in tokenize(&source) {
            let lower = token.to_lowercase();
            if let Some(val) = lower.strip_prefix("type:") {
                if val.is_empty() {
                    errors.push("type: needs an extension, e.g. type:pdf".into());
                } else {
                    ext = Some(canonicalize_ext(val));
                }
            } else if let Some(val) = lower.strip_prefix("ext:") {
                if val.is_empty() {
                    errors.push("ext: needs an extension, e.g. ext:rs".into());
                } else {
                    ext = Some(canonicalize_ext(val));
                }
            } else if let Some(val) = lower.strip_prefix("*.") {
                if val.is_empty() {
                    errors.push("*. needs an extension, e.g. *.rs".into());
                } else {
                    ext = Some(canonicalize_ext(val));
                }
            } else if let Some(val) = lower.strip_prefix("size:>") {
                match parse_size(val) {
                    Some(n) => min_size = Some(n),
                    None => errors.push(format!("Invalid size: {val}")),
                }
            } else if let Some(val) = lower.strip_prefix("size:<") {
                match parse_size(val) {
                    Some(n) => max_size = Some(n),
                    None => errors.push(format!("Invalid size: {val}")),
                }
            } else if let Some(val) = lower.strip_prefix("modified:") {
                match parse_modified(val, now_secs) {
                    Ok((min_age, max_age)) => {
                        if let Some(v) = min_age {
                            min_age_secs = Some(v);
                        }
                        if let Some(v) = max_age {
                            max_age_secs = Some(v);
                        }
                    }
                    Err(e) => errors.push(e),
                }
            } else if let Some(val) = strip_prefix_ci(&token, "name:") {
                if val.is_empty() {
                    errors.push("name: needs a filename, e.g. name:\"Cargo\"".into());
                } else {
                    name = Some(val.to_string());
                }
            } else if let Some(val) = strip_prefix_ci(&token, "content:") {
                if val.is_empty() {
                    errors.push("content: needs a string, e.g. content:\"TODO\"".into());
                } else {
                    content = Some(val.to_string());
                }
            } else if let Some(val) = lower.strip_prefix("git:") {
                match GitFilter::parse(val) {
                    Some(g) => git = Some(g),
                    None => errors.push(format!(
                        "Unknown git: filter '{val}' (use modified, untracked, ignored)"
                    )),
                }
            } else if let Some(val) = lower.strip_prefix("duplicate:") {
                duplicates = val == "true" || val == "yes" || val == "1";
                if !duplicates && val != "false" && val != "no" && val != "0" {
                    errors.push(format!("duplicate: expected true or false, got '{val}'"));
                }
            } else if lower == "here:" {
                scope_current_dir = true;
            } else if let Some((key, rest)) = split_filter(&lower) {
                if rest.starts_with("//") {
                    // URL-like (https://...), keep as filename text.
                    text_parts.push(token);
                } else if key == "here" {
                    // `here:foo` is not the scope marker; treat as text.
                    text_parts.push(token);
                } else if KNOWN_FILTERS.contains(&key) {
                    errors.push(format!("Could not parse filter '{token}'"));
                } else if let Some(near) = suggest_filter(key) {
                    errors.push(format!(
                        "Unknown filter '{key}:' was not applied. Did you mean '{near}:'?"
                    ));
                } else {
                    errors.push(format!(
                        "Unknown filter '{key}:'. Try type, ext, size, modified, name, content, git, duplicate, here"
                    ));
                }
            } else {
                text_parts.push(token);
            }
        }

        ParsedQuery {
            text: text_parts.join(" "),
            ext,
            name,
            content,
            min_size,
            max_size,
            max_age_secs,
            min_age_secs,
            git,
            duplicates,
            scope_current_dir,
            scope_path: None,
            errors,
            interpreted_as,
        }
    }

    /// Check if a VNode matches all cheap filter criteria (name, extension,
    /// size, age). Content / git / duplicate filters are applied by the
    /// search pipeline, not here — they need I/O.
    pub fn matches(&self, node: &VNode, now_secs: u64) -> bool {
        if !self.text.is_empty() && !fuzzy_match(&node.name, &self.text) {
            return false;
        }
        if let Some(ref name) = self.name {
            if !fuzzy_match(&node.name, name) {
                return false;
            }
        }
        if let Some(ref ext_filter) = self.ext {
            let node_ext = node
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if node_ext != *ext_filter && !(ext_filter == "folder" && node.is_dir) {
                return false;
            }
        }
        if let Some(min) = self.min_size {
            if node.size < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if node.size > max {
                return false;
            }
        }
        if let Some(max_age) = self.max_age_secs {
            if node.modified_at == 0 || now_secs.saturating_sub(node.modified_at) > max_age {
                return false;
            }
        }
        if let Some(min_age) = self.min_age_secs {
            if node.modified_at == 0 || now_secs.saturating_sub(node.modified_at) < min_age {
                return false;
            }
        }
        true
    }

    /// Drop tokens whose lowercased form matches `drop_if`. Used by filter chips
    /// so `TYPE:PDF` is removed the same way `type:pdf` is.
    #[allow(dead_code)] // kept for filter chips in the search box
    pub fn strip_filter_tokens(raw: &str, drop_if: impl Fn(&str) -> bool) -> String {
        tokenize(raw)
            .into_iter()
            .filter(|t| !drop_if(&t.to_lowercase()))
            .map(|t| {
                if t.contains(char::is_whitespace) {
                    format!("\"{t}\"")
                } else {
                    t
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Returns true if there are any active filters beyond plain text.
    pub fn has_filters(&self) -> bool {
        self.ext.is_some()
            || self.name.is_some()
            || self.content.is_some()
            || self.min_size.is_some()
            || self.max_size.is_some()
            || self.max_age_secs.is_some()
            || self.min_age_secs.is_some()
            || self.git.is_some()
            || self.duplicates
            || self.scope_current_dir
    }

    pub fn needs_walk(&self) -> bool {
        self.content.is_some() || self.duplicates
    }

    /// Filter-only queries (`ext:rs`, `size:>1gb`, …) search the whole index.
    /// Free-text and `here:` stay scoped to the current folder tree.
    pub fn is_index_wide(&self) -> bool {
        self.text.is_empty()
            && !self.scope_current_dir
            && (self.has_filters() || self.git.is_some() || self.duplicates)
    }
}

/// The known filter closest to a mistyped key, if it is a plausible typo
/// (edit distance 1, or 2 for keys of five letters or more).
pub fn suggest_filter(key: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut cur = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != *cb);
                cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
        }
        prev[b.len()]
    }
    KNOWN_FILTERS
        .iter()
        .map(|k| (distance(key, k), *k))
        .filter(|(d, k)| *d > 0 && (*d == 1 || (*d == 2 && k.len() >= 5)))
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Map language names and globs onto the stored file extension.
pub fn canonicalize_ext(raw: &str) -> String {
    let e = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    match e.as_str() {
        "rust" | "rust-source" | "rustsource" => "rs".into(),
        "python" => "py".into(),
        "javascript" => "js".into(),
        "typescript" => "ts".into(),
        "markdown" => "md".into(),
        "text" => "txt".into(),
        "jpeg" => "jpg".into(),
        other => other.to_string(),
    }
}

/// Split on whitespace, keeping `"quoted phrases"` as a single token.
/// Quotes around a value (`name:"Cargo.toml"`) are stripped.
fn tokenize(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in raw.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn strip_prefix_ci<'a>(token: &'a str, prefix: &str) -> Option<&'a str> {
    if token.len() >= prefix.len() && token[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&token[prefix.len()..])
    } else {
        None
    }
}

fn split_filter(lower: &str) -> Option<(&str, &str)> {
    let (key, rest) = lower.split_once(':')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((key, rest))
}

fn parse_modified(val: &str, now_secs: u64) -> Result<(Option<u64>, Option<u64>), String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("modified: needs a value, e.g. modified:<7d or modified:today".into());
    }
    let secs_today = now_secs % 86_400;
    if val == "today" {
        return Ok((None, Some(secs_today)));
    }
    if val == "yesterday" {
        return Ok((
            Some(secs_today.max(1)),
            Some(secs_today.saturating_add(86_400)),
        ));
    }
    if let Some(v) = val.strip_prefix('<') {
        return parse_duration(v)
            .map(|d| (None, Some(d)))
            .ok_or_else(|| format!("Invalid duration: {v}"));
    }
    if let Some(v) = val.strip_prefix('>') {
        return parse_duration(v)
            .map(|d| (Some(d), None))
            .ok_or_else(|| format!("Invalid duration: {v}"));
    }
    parse_duration(val)
        .map(|d| (None, Some(d)))
        .ok_or_else(|| format!("Invalid modified: value '{val}'"))
}

/// Characters that count as a word boundary in a file name, so that "main"
/// scores well against "my_main.rs".
const WORD_SEPARATORS: [char; 3] = [' ', '_', '-'];

/// Query-wide state hoisted out of the per-node scoring loop: the lowercased
/// query text and the reference timestamp are the same for every node, so
/// computing them once per sort rather than once per comparison turns an
/// O(n log n) pile of allocations and `SystemTime::now()` syscalls into O(n).
struct RankContext {
    text_lower: String,
    now_secs: i64,
}

impl RankContext {
    fn new(query: &ParsedQuery) -> Self {
        Self {
            text_lower: query.text.to_lowercase(),
            now_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        }
    }

    /// How well the node's name matches the query text. Allocation-free: the
    /// word-boundary test inspects the character before each match instead of
    /// building `" {text}"`, `"_{text}"` and `"-{text}"` needles.
    fn text_score(&self, name_lower: &str) -> i64 {
        if self.text_lower.is_empty() {
            return 0;
        }
        if name_lower == self.text_lower {
            return 1000;
        }
        if name_lower.starts_with(&self.text_lower) {
            return 500;
        }
        let mut matched = false;
        for (idx, _) in name_lower.match_indices(&self.text_lower) {
            matched = true;
            if name_lower[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| WORD_SEPARATORS.contains(&c))
            {
                return 300;
            }
        }
        if matched {
            100
        } else {
            0
        }
    }

    fn score(&self, node: &VNode) -> i64 {
        let mut score = self.text_score(&node.name.to_lowercase());

        // Boost for shorter names (less path noise)
        score -= (node.name.len() as i64) * 2;

        // Penalize deep paths (users typically search near their working dir)
        score -= (node.path.components().count() as i64) * 5;

        // Boost for recent files
        if node.modified_at > 0 {
            let age_days = (self.now_secs - node.modified_at as i64) / 86400;
            if age_days < 1 {
                score += 200;
            } else if age_days < 7 {
                score += 100;
            } else if age_days < 30 {
                score += 50;
            }
        }

        // Boost for smaller files (faster to open/preview)
        if !node.is_dir && node.size > 0 && node.size < 100_000 {
            score += 30;
        }

        score
    }
}

/// Sort nodes by rank score descending, then by name alphabetically.
pub fn sort_by_rank(nodes: &mut [VNode], query: &ParsedQuery) {
    let ctx = RankContext::new(query);
    // Name is the tie-break. Sorting by name first and then applying a *stable*
    // sort on the score yields the same order as a combined comparator while
    // letting `sort_by_cached_key` score each node exactly once.
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes.sort_by_cached_key(|node| std::cmp::Reverse(ctx.score(node)));
}

/// Upper bound on how many ranked results are handed to the UI. Only applies
/// to *search* results — a plain directory listing is never truncated.
pub const MAX_VISIBLE_RESULTS: usize = 200;

/// Filter a node list by the parsed query criteria.
pub fn filter_and_rank(nodes: Vec<VNode>, query: &ParsedQuery, now_secs: u64) -> Vec<VNode> {
    let mut results: Vec<VNode> = nodes
        .into_iter()
        .filter(|n| query.matches(n, now_secs))
        .collect();
    sort_by_rank(&mut results, query);
    results.truncate(MAX_VISIBLE_RESULTS);
    results
}

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;
const GIB: f64 = 1024.0 * MIB;
const TIB: f64 = 1024.0 * GIB;

/// Size suffixes, longest first so that `tib` is not mistaken for `b`.
/// Units are binary (1 KB = 1024 B) so that a `size:` filter agrees with the
/// sizes rendered in the file list.
const SIZE_UNITS: [(&str, f64); 9] = [
    ("tib", TIB),
    ("gib", GIB),
    ("mib", MIB),
    ("kib", KIB),
    ("tb", TIB),
    ("gb", GIB),
    ("mb", MIB),
    ("kb", KIB),
    ("b", 1.0),
];

/// Duration suffixes, in descending magnitude.
const DURATION_UNITS: [(char, f64); 5] = [
    ('w', 604_800.0),
    ('d', 86_400.0),
    ('h', 3_600.0),
    ('m', 60.0),
    ('s', 1.0),
];

/// Scale a parsed quantity by a unit multiplier, rejecting values that are not
/// finite, non-negative numbers and saturating instead of wrapping on overflow.
fn scale(value: &str, multiplier: f64) -> Option<u64> {
    let n = value.trim().parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let scaled = n * multiplier;
    if scaled >= u64::MAX as f64 {
        return Some(u64::MAX);
    }
    Some(scaled.round() as u64)
}

/// Parse a human-readable size string like "1mb", "1.5gb", "500kb" or a bare
/// byte count into bytes. Returns `None` for values that are not a
/// non-negative number, so that a malformed filter is ignored rather than
/// silently becoming `0` (which would match every file).
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    for (suffix, multiplier) in SIZE_UNITS {
        if let Some(val) = s.strip_suffix(suffix) {
            return scale(val, multiplier);
        }
    }
    scale(&s, 1.0)
}

/// Parse a duration string like "30d", "12h", "2w" into seconds. A bare number
/// is interpreted as seconds. Returns `None` for malformed values.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    for (suffix, multiplier) in DURATION_UNITS {
        if let Some(val) = s.strip_suffix(suffix) {
            return scale(val, multiplier);
        }
    }
    scale(&s, 1.0)
}

/// Checks if a file name matches a query string using substring fuzzy matching.
pub fn fuzzy_match(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query_lower = query.to_lowercase();
    let name_lower = name.to_lowercase();
    name_lower.contains(&query_lower)
}

/// Case-insensitive substring search inside a file's text. Documents (PDF,
/// Office, OpenDocument) are searched through their extracted text. Missing,
/// oversized, and binary files are skipped rather than failing the whole query.
pub fn file_contains_text(path: &Path, needle: &str) -> bool {
    file_contains_text_until(path, needle, &|| false)
}

fn file_contains_text_until(path: &Path, needle: &str, cancelled: &dyn Fn() -> bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    match crate::extract::text_for_search(path, cancelled) {
        Ok(text) => crate::extract::find_ci(&text, needle).is_some(),
        Err(_) => false,
    }
}

/// Keep files whose contents (cheap fingerprint) appear more than once.
/// Directories are dropped. Empty files are treated as duplicates of each other.
pub fn filter_duplicates(nodes: Vec<VNode>) -> Vec<VNode> {
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};

    let mut by_size: HashMap<u64, Vec<VNode>> = HashMap::new();
    for n in nodes {
        if n.is_dir {
            continue;
        }
        by_size.entry(n.size).or_default().push(n);
    }
    let mut out = Vec::new();
    for (size, group) in by_size {
        if group.len() < 2 {
            continue;
        }
        if size == 0 {
            out.extend(group);
            continue;
        }
        let mut by_hash: HashMap<u64, Vec<VNode>> = HashMap::new();
        for n in group {
            let fp = file_fingerprint(&n.path).unwrap_or_else(|| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                n.path.hash(&mut h);
                h.finish()
            });
            by_hash.entry(fp).or_default().push(n);
        }
        for g in by_hash.into_values() {
            if g.len() >= 2 {
                out.extend(g);
            }
        }
    }
    out
}

fn file_fingerprint(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    len.hash(&mut hasher);
    let mut buf = [0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    buf[..n].hash(&mut hasher);
    if len > 8192 {
        let back = 8192u64.min(len);
        file.seek(SeekFrom::End(-(back as i64))).ok()?;
        let n = file.read(&mut buf).ok()?;
        buf[..n].hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Directories that are never descended into during a search or an index
/// build: dotfiles plus the usual high-volume, low-value build/cache trees.
pub fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || name == "Library" || name == "node_modules" || name == "target"
}

/// Stop the crawl once this many matches have been collected.
const MAX_WALK_RESULTS: usize = 1000;

/// Minimum wall-clock gap between progressive updates pushed to the UI.
///
/// This used to fire every 20 matches, but each update clones and re-ranks the
/// entire result set, which made a long crawl quadratic in the number of
/// matches. Throttling by time bounds the ranking work regardless of how fast
/// matches arrive.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// Rank a snapshot of the results and cut it down to what the UI will show.
fn ranked_snapshot(results: &[VNode], query: &ParsedQuery) -> Vec<VNode> {
    let mut batch = results.to_vec();
    sort_by_rank(&mut batch, query);
    batch.truncate(MAX_VISIBLE_RESULTS);
    batch
}

/// Live filesystem crawler: walks directories, filters + ranks results.
pub fn walk_dir_search(
    path: PathBuf,
    query: ParsedQuery,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<SearchUpdate>,
    tab: TabId,
    query_id: String,
) {
    let update = |results: &[VNode], query_str: String, complete: bool| SearchUpdate {
        tab,
        query: query_str,
        results: ranked_snapshot(results, &query),
        error: None,
        complete,
    };
    let mut stack = vec![path];
    let mut results: Vec<VNode> = Vec::new();
    let mut last_sent = std::time::Instant::now();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    while let Some(current_path) = stack.pop() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let entries = match std::fs::read_dir(&current_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }

            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let is_dir = meta.is_dir();
            let node = crate::core::node_from_metadata(path.clone(), &meta);

            if query.matches(&node, now_secs) {
                let content_ok = match query.content.as_deref() {
                    Some(needle) => {
                        !is_dir
                            && file_contains_text_until(&path, needle, &|| {
                                cancel.load(std::sync::atomic::Ordering::Relaxed)
                            })
                    }
                    None => true,
                };
                if content_ok {
                    results.push(node);

                    if results.len() >= MAX_WALK_RESULTS {
                        let _ = tx.send(update(&results, query_id, true));
                        return;
                    }
                    if last_sent.elapsed() >= PROGRESS_INTERVAL {
                        let _ = tx.send(update(&results, query_id.clone(), false));
                        last_sent = std::time::Instant::now();
                    }
                }
            }

            // `meta` comes from the directory entry and so does not follow
            // symlinks. Descending through symlinked directories risks looping
            // forever on a link that points back at an ancestor.
            if is_dir && !query.scope_current_dir {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !is_skipped_dir(name) {
                        stack.push(path);
                    }
                }
            }
        }
    }

    if query.duplicates {
        results = filter_duplicates(results);
    }
    let _ = tx.send(update(&results, query_id, true));
}

/// Orchestrate index / git / walk so the UI only has to spawn a thread.
pub fn run_search(
    parsed: ParsedQuery,
    db: Option<PathBuf>,
    index_ready: bool,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<SearchUpdate>,
    tab: TabId,
    query_id: String,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }

    if let Some(filter) = parsed.git {
        let scope = parsed
            .scope_path
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let results = git_candidates(&scope, filter, &parsed, now);
        let _ = tx.send(SearchUpdate {
            tab,
            query: query_id,
            results,
            error: None,
            complete: true,
        });
        return;
    }

    if index_ready && !parsed.needs_walk() {
        if let Some(ref db_path) = db {
            match crate::db::DbManager::query_disk_index(db_path, &parsed) {
                Ok(res) => {
                    let mut results = filter_and_rank(res, &parsed, now);
                    if parsed.duplicates {
                        results = filter_duplicates(results);
                    }
                    let empty = results.is_empty();
                    let _ = tx.send(SearchUpdate {
                        tab,
                        query: query_id.clone(),
                        results,
                        error: None,
                        complete: !empty || parsed.scope_path.is_none(),
                    });
                    // Indexed miss for a scoped query: walk the folder so
                    // local .rs files still show when the index is stale.
                    if empty && parsed.scope_path.is_some() {
                        // fall through
                    } else {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(SearchUpdate {
                        tab,
                        query: query_id.clone(),
                        results: Vec::new(),
                        error: Some(e.to_string()),
                        complete: false,
                    });
                }
            }
        }
    }

    let root = parsed
        .scope_path
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    walk_dir_search(root, parsed, cancel, tx, tab, query_id);
}

fn git_candidates(
    scope: &Path,
    filter: crate::git::GitFilter,
    parsed: &ParsedQuery,
    now: u64,
) -> Vec<VNode> {
    let Some(info) = crate::git::query(scope) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for (path, mark) in info.marks {
        if !mark.matches_filter(filter) {
            continue;
        }
        if let Some(ref root) = parsed.scope_path {
            if parsed.scope_current_dir {
                if path.parent() != Some(root.as_path()) {
                    continue;
                }
            } else if !path.starts_with(root) {
                continue;
            }
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let node = crate::core::node_from_metadata(path, &meta);
        if !parsed.matches(&node, now) {
            continue;
        }
        if let Some(ref needle) = parsed.content {
            if node.is_dir || !file_contains_text(&node.path, needle) {
                continue;
            }
        }
        nodes.push(node);
    }
    if parsed.duplicates {
        nodes = filter_duplicates(nodes);
    }
    filter_and_rank(nodes, parsed, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    /// Fixed "current time" so age assertions do not depend on the wall clock.
    const NOW: u64 = 1_700_000_000;

    fn node(name: &str, size: u64, modified_at: u64) -> VNode {
        VNode {
            name: name.to_string(),
            path: PathBuf::from("/tmp").join(name),
            size,
            is_dir: false,
            is_symlink: false,
            modified_at,
            created_at: modified_at,
            readonly: false,
            hidden: false,
            mime_type: "application/octet-stream".to_string(),
        }
    }

    fn dir(name: &str) -> VNode {
        VNode {
            is_dir: true,
            ..node(name, 0, NOW)
        }
    }

    /// Score a single node against a query, for asserting on scoring rules in
    /// isolation. Production code scores through `sort_by_rank`, which builds
    /// the context once for the whole slice.
    fn rank_result(node: &VNode, query: &ParsedQuery) -> i64 {
        RankContext::new(query).score(node)
    }

    // ── parse_size ────────────────────────────────────────────────────────

    #[test]
    fn parse_size_applies_unit_before_truncating() {
        // Regression: the multiplier used to be applied *after* casting the
        // parsed f64 to u64, so "1.5mb" collapsed to 1 MB.
        assert_eq!(parse_size("1.5mb"), Some(1_572_864));
        assert_eq!(parse_size("0.5kb"), Some(512));
        assert_eq!(parse_size("2.25gb"), Some(2_415_919_104));
    }

    #[test]
    fn parse_size_uses_binary_units() {
        assert_eq!(parse_size("1kb"), Some(1024));
        assert_eq!(parse_size("1mb"), Some(1024 * 1024));
        assert_eq!(parse_size("1gb"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1tb"), Some(1024_u64.pow(4)));
    }

    #[test]
    fn parse_size_accepts_explicit_binary_suffixes() {
        assert_eq!(parse_size("1kib"), parse_size("1kb"));
        assert_eq!(parse_size("3gib"), parse_size("3gb"));
        // "tib" must not be truncated to the "b" unit.
        assert_eq!(parse_size("1tib"), Some(1024_u64.pow(4)));
    }

    #[test]
    fn parse_size_handles_bytes_and_bare_numbers() {
        assert_eq!(parse_size("512b"), Some(512));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("0"), Some(0));
    }

    #[test]
    fn parse_size_is_case_and_whitespace_insensitive() {
        assert_eq!(parse_size(" 1MB "), Some(1024 * 1024));
        assert_eq!(parse_size("1Gb"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn parse_size_rejects_malformed_values() {
        // Previously these became 0, which silently matched every file.
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("mb"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("-5mb"), None);
        assert_eq!(parse_size("nan"), None);
        assert_eq!(parse_size("inf"), None);
    }

    #[test]
    fn parse_size_saturates_instead_of_wrapping() {
        assert_eq!(parse_size("99999999999tb"), Some(u64::MAX));
    }

    // ── parse_duration ────────────────────────────────────────────────────

    #[test]
    fn parse_duration_supports_all_units() {
        assert_eq!(parse_duration("30d"), Some(30 * DAY));
        assert_eq!(parse_duration("12h"), Some(12 * 3600));
        assert_eq!(parse_duration("2w"), Some(2 * 7 * DAY));
        assert_eq!(parse_duration("90m"), Some(90 * 60));
        assert_eq!(parse_duration("45s"), Some(45));
        assert_eq!(parse_duration("45"), Some(45));
    }

    #[test]
    fn parse_duration_supports_fractions_and_rejects_junk() {
        assert_eq!(parse_duration("1.5d"), Some(DAY + DAY / 2));
        assert_eq!(parse_duration("d"), None);
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration("-3d"), None);
    }

    // ── ParsedQuery::parse ────────────────────────────────────────────────

    #[test]
    fn parse_extracts_filters_and_leaves_text() {
        let q = ParsedQuery::parse("report type:pdf size:>1mb size:<2gb modified:<30d");
        assert_eq!(q.text, "report");
        assert_eq!(q.ext.as_deref(), Some("pdf"));
        assert_eq!(q.min_size, Some(1024 * 1024));
        assert_eq!(q.max_size, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(q.max_age_secs, Some(30 * DAY));
        assert!(!q.scope_current_dir);
    }

    #[test]
    fn parse_joins_multiple_text_tokens_and_lowercases_ext() {
        let q = ParsedQuery::parse("annual TYPE:PDF report");
        assert_eq!(q.text, "annual report");
        assert_eq!(q.ext.as_deref(), Some("pdf"));
    }

    #[test]
    fn parse_recognises_bare_here_marker_only() {
        assert!(ParsedQuery::parse("here:").scope_current_dir);
        // "here:foo" is not a scope marker, so it stays as search text.
        let q = ParsedQuery::parse("here:foo");
        assert!(!q.scope_current_dir);
        assert_eq!(q.text, "here:foo");
    }

    #[test]
    fn parse_drops_malformed_filters_instead_of_applying_zero() {
        let q = ParsedQuery::parse("size:>huge modified:<soon");
        assert_eq!(q.min_size, None);
        assert_eq!(q.max_age_secs, None);
        assert!(!q.has_filters());
        // The malformed tokens must not leak into the fuzzy-match text either.
        assert_eq!(q.text, "");
        assert!(q.errors.iter().any(|e| e.contains("Invalid size")));
        assert!(q.errors.iter().any(|e| e.contains("Invalid duration")));
    }

    #[test]
    fn parse_ext_is_an_alias_for_type() {
        let q = ParsedQuery::parse("ext:rs size:>100kb modified:<7d");
        assert_eq!(q.ext.as_deref(), Some("rs"));
        assert_eq!(q.min_size, Some(100 * 1024));
        assert_eq!(q.max_age_secs, Some(7 * DAY));
    }

    #[test]
    fn parse_type_rust_and_glob_map_to_rs() {
        assert_eq!(ParsedQuery::parse("type:rust").ext.as_deref(), Some("rs"));
        assert_eq!(ParsedQuery::parse("ext:RUST").ext.as_deref(), Some("rs"));
        assert_eq!(ParsedQuery::parse("*.rs").ext.as_deref(), Some("rs"));
        assert_eq!(ParsedQuery::parse("type:.rs").ext.as_deref(), Some("rs"));
        let q = ParsedQuery::parse("type:rust");
        assert!(q.matches(&node("main.rs", 1, NOW), NOW));
        assert!(!q.matches(&node("main.py", 1, NOW), NOW));
        assert!(q.is_index_wide());
    }

    #[test]
    fn rust_files_query_is_index_wide_unlike_filename_text() {
        assert!(ParsedQuery::parse("ext:rs").is_index_wide());
        assert!(ParsedQuery::parse("type:rust").is_index_wide());
        assert!(!ParsedQuery::parse("main ext:rs").is_index_wide());
        assert!(!ParsedQuery::parse("ext:rs here:").is_index_wide());
        assert!(!ParsedQuery::parse("main.rs").is_index_wide());
    }

    #[test]
    fn parse_quoted_name_and_content() {
        let q = ParsedQuery::parse(r#"name:"Cargo.toml" content:"TODO""#);
        assert_eq!(q.name.as_deref(), Some("Cargo.toml"));
        assert_eq!(q.content.as_deref(), Some("TODO"));
        assert_eq!(q.text, "");
    }

    #[test]
    fn parse_git_and_duplicate_predicates() {
        let q = ParsedQuery::parse("git:modified duplicate:true");
        assert_eq!(q.git, Some(crate::git::GitFilter::Modified));
        assert!(q.duplicates);
        let q = ParsedQuery::parse("git:untracked");
        assert_eq!(q.git, Some(crate::git::GitFilter::Untracked));
        let q = ParsedQuery::parse("git:ignored");
        assert_eq!(q.git, Some(crate::git::GitFilter::Ignored));
    }

    #[test]
    fn parse_modified_today_and_yesterday_at_a_fixed_now() {
        // NOW is 1_700_000_000; seconds since UTC midnight:
        let secs_today = NOW % DAY;
        let q = ParsedQuery::parse_at("modified:today", NOW);
        assert_eq!(q.max_age_secs, Some(secs_today));
        assert_eq!(q.min_age_secs, None);

        let q = ParsedQuery::parse_at("modified:yesterday", NOW);
        assert_eq!(q.min_age_secs, Some(secs_today.max(1)));
        assert_eq!(q.max_age_secs, Some(secs_today + DAY));
    }

    #[test]
    fn parse_unknown_filter_is_an_error_not_text() {
        let q = ParsedQuery::parse("foo:bar report");
        assert_eq!(q.text, "report");
        assert!(q.errors.iter().any(|e| e.contains("Unknown filter")));
    }

    #[test]
    fn parse_keeps_urls_as_text() {
        let q = ParsedQuery::parse("https://example.com/a.rs");
        assert_eq!(q.text, "https://example.com/a.rs");
        assert!(q.errors.is_empty());
    }

    #[test]
    fn parse_natural_language_rewrite() {
        let q = ParsedQuery::parse("find PDFs larger than 100MB");
        assert_eq!(q.ext.as_deref(), Some("pdf"));
        assert_eq!(q.min_size, Some(100 * 1024 * 1024));
        assert_eq!(q.interpreted_as.as_deref(), Some("type:pdf size:>100mb"));
    }

    #[test]
    fn matches_name_filter() {
        let q = ParsedQuery::parse(r#"name:"Cargo""#);
        assert!(q.matches(&node("Cargo.toml", 1, NOW), NOW));
        assert!(!q.matches(&node("main.rs", 1, NOW), NOW));
    }

    #[test]
    fn tokenize_preserves_quoted_phrases() {
        assert_eq!(
            tokenize(r#"name:"Cargo File" leftover"#),
            vec!["name:Cargo File", "leftover"]
        );
    }

    #[test]
    fn file_contains_text_is_case_insensitive_and_skips_binary() {
        let tmp = crate::testutil::TempDir::new("content");
        let text = tmp.file("a.rs", b"fn main() { let TODO = 1; }");
        let bin = tmp.file("a.bin", b"hello\0world");
        assert!(file_contains_text(&text, "todo"));
        assert!(!file_contains_text(&text, "missing"));
        assert!(!file_contains_text(&bin, "hello"));
    }

    #[test]
    fn content_search_reads_inside_docx() {
        let tmp = crate::testutil::TempDir::new("content_docx");
        let doc = crate::extract::fixtures::docx(
            &tmp,
            "memo.docx",
            "<w:p><w:r><w:t>The Quarterly Budget is due</w:t></w:r></w:p>",
        );
        assert!(file_contains_text(&doc, "quarterly budget"));
        assert!(!file_contains_text(&doc, "missing"));
        let q = ParsedQuery::parse(r#"content:"budget""#);
        assert_eq!(q.content.as_deref(), Some("budget"));
    }

    #[test]
    fn filter_duplicates_keeps_identical_files() {
        let tmp = crate::testutil::TempDir::new("dups");
        let a = tmp.file("a.txt", b"same");
        let b = tmp.file("b.txt", b"same");
        let c = tmp.file("c.txt", b"other");
        let nodes = vec![
            VNode {
                name: "a.txt".into(),
                path: a,
                size: 4,
                is_dir: false,
                is_symlink: false,
                modified_at: NOW,
                created_at: NOW,
                readonly: false,
                hidden: false,
                mime_type: String::new(),
            },
            VNode {
                name: "b.txt".into(),
                path: b,
                size: 4,
                is_dir: false,
                is_symlink: false,
                modified_at: NOW,
                created_at: NOW,
                readonly: false,
                hidden: false,
                mime_type: String::new(),
            },
            VNode {
                name: "c.txt".into(),
                path: c,
                size: 5,
                is_dir: false,
                is_symlink: false,
                modified_at: NOW,
                created_at: NOW,
                readonly: false,
                hidden: false,
                mime_type: String::new(),
            },
        ];
        let out = filter_duplicates(nodes);
        let mut names: Vec<_> = out.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn unknown_filters_suggest_the_closest_known_one() {
        assert_eq!(suggest_filter("sze"), Some("size"));
        assert_eq!(suggest_filter("modifed"), Some("modified"));
        assert_eq!(suggest_filter("extt"), Some("ext"));
        assert_eq!(suggest_filter("colour"), None);
        let q = ParsedQuery::parse_at("ops sze:>1mb", NOW);
        assert!(
            q.errors[0].contains("Did you mean 'size:'"),
            "{:?}",
            q.errors
        );
        assert!(q.min_size.is_none(), "a typo must not become a constraint");
    }

    #[test]
    fn strip_filter_tokens_is_case_insensitive() {
        let raw = "report TYPE:PDF SIZE:>1mb here: leftover";
        assert_eq!(
            ParsedQuery::strip_filter_tokens(raw, |t| t.starts_with("type:")),
            "report SIZE:>1mb here: leftover"
        );
        assert_eq!(
            ParsedQuery::strip_filter_tokens(raw, |t| t.starts_with("size:>")),
            "report TYPE:PDF here: leftover"
        );
        assert_eq!(
            ParsedQuery::strip_filter_tokens(raw, |t| t.starts_with("here:")),
            "report TYPE:PDF SIZE:>1mb leftover"
        );
    }

    #[test]
    fn parse_of_empty_query_has_no_filters() {
        let q = ParsedQuery::parse("   ");
        assert_eq!(q.text, "");
        assert!(!q.has_filters());
    }

    // ── matching ──────────────────────────────────────────────────────────

    #[test]
    fn matches_applies_size_bounds_inclusively() {
        let q = ParsedQuery::parse("size:>1kb size:<3kb");
        assert!(!q.matches(&node("a", 1023, NOW), NOW));
        assert!(q.matches(&node("b", 1024, NOW), NOW));
        assert!(q.matches(&node("c", 3072, NOW), NOW));
        assert!(!q.matches(&node("d", 3073, NOW), NOW));
    }

    #[test]
    fn matches_applies_age_bounds() {
        let recent = node("recent", 1, NOW - DAY);
        let old = node("old", 1, NOW - 90 * DAY);

        let newer_than_30d = ParsedQuery::parse("modified:<30d");
        assert!(newer_than_30d.matches(&recent, NOW));
        assert!(!newer_than_30d.matches(&old, NOW));

        let older_than_30d = ParsedQuery::parse("modified:>30d");
        assert!(!older_than_30d.matches(&recent, NOW));
        assert!(older_than_30d.matches(&old, NOW));
    }

    #[test]
    fn matches_treats_unknown_mtime_as_failing_age_filters() {
        let unknown = node("unknown", 1, 0);
        assert!(!ParsedQuery::parse("modified:<30d").matches(&unknown, NOW));
        assert!(!ParsedQuery::parse("modified:>30d").matches(&unknown, NOW));
        // With no age filter it still matches.
        assert!(ParsedQuery::parse("unknown").matches(&unknown, NOW));
    }

    #[test]
    fn matches_extension_filter_is_case_insensitive() {
        let q = ParsedQuery::parse("type:rs");
        assert!(q.matches(&node("main.rs", 1, NOW), NOW));
        assert!(q.matches(&node("MAIN.RS", 1, NOW), NOW));
        assert!(!q.matches(&node("main.py", 1, NOW), NOW));
        assert!(!q.matches(&node("Makefile", 1, NOW), NOW));
    }

    #[test]
    fn matches_folder_pseudo_extension() {
        let q = ParsedQuery::parse("type:folder");
        assert!(q.matches(&dir("src"), NOW));
        assert!(!q.matches(&node("main.rs", 1, NOW), NOW));
    }

    #[test]
    fn matches_text_is_case_insensitive_substring() {
        let q = ParsedQuery::parse("MAIN");
        assert!(q.matches(&node("main.rs", 1, NOW), NOW));
        assert!(q.matches(&node("DOMAIN.txt", 1, NOW), NOW));
        assert!(!q.matches(&node("lib.rs", 1, NOW), NOW));
    }

    #[test]
    fn fuzzy_match_empty_query_matches_everything() {
        assert!(fuzzy_match("anything.rs", ""));
    }

    // ── ranking ───────────────────────────────────────────────────────────

    #[test]
    fn rank_prefers_exact_then_prefix_then_substring() {
        let q = ParsedQuery::parse("main");
        let exact = rank_result(&node("main", 1, 0), &q);
        let prefix = rank_result(&node("main_x", 1, 0), &q);
        let substring = rank_result(&node("xxmainxx", 1, 0), &q);
        assert!(exact > prefix, "{exact} !> {prefix}");
        assert!(prefix > substring, "{prefix} !> {substring}");
    }

    #[test]
    fn filter_and_rank_filters_then_orders_by_score() {
        let items = vec![
            node("zzz.rs", 10, NOW),
            node("main.rs", 10, NOW),
            node("notes.txt", 10, NOW),
        ];
        let q = ParsedQuery::parse("main type:rs");
        let out = filter_and_rank(items, &q, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "main.rs");
    }

    #[test]
    fn filter_and_rank_caps_result_count() {
        let items: Vec<VNode> = (0..500)
            .map(|i| node(&format!("f{i}.rs"), 1, NOW))
            .collect();
        let out = filter_and_rank(items, &ParsedQuery::parse("type:rs"), NOW);
        assert_eq!(out.len(), 200);
    }

    #[test]
    fn rank_scores_word_boundary_above_bare_substring() {
        let q = ParsedQuery::parse("main");
        // Same name length, so only the text score differs.
        let boundary = rank_result(&node("my_main", 1, 0), &q);
        let embedded = rank_result(&node("xymainz", 1, 0), &q);
        assert!(boundary > embedded, "{boundary} !> {embedded}");
    }

    #[test]
    fn rank_word_boundary_accepts_each_separator() {
        let q = ParsedQuery::parse("main");
        let embedded = rank_result(&node("xymainz", 1, 0), &q);
        for name in ["a main", "a_main", "a-main"] {
            assert!(
                rank_result(&node(name, 1, 0), &q) > embedded,
                "{name} should outrank a bare substring match"
            );
        }
    }

    #[test]
    fn rank_ignores_text_score_for_filter_only_queries() {
        // With no text component every node must get the same text score, so
        // ordering falls through to the name/depth/recency signals.
        let q = ParsedQuery::parse("type:rs");
        let short = rank_result(&node("a.rs", 1, 0), &q);
        let long = rank_result(&node("a_very_long_name.rs", 1, 0), &q);
        assert!(
            short > long,
            "shorter names should still win: {short} !> {long}"
        );
    }

    #[test]
    fn sort_by_rank_matches_a_naive_comparator() {
        // Guards the decorate-sort-undecorate rewrite: the cached-key sort must
        // order identically to scoring inside the comparator.
        let q = ParsedQuery::parse("main");
        let items: Vec<VNode> = [
            ("main", 10_u64),
            ("main.rs", 20),
            ("my_main.rs", 30),
            ("domain.txt", 40),
            ("unrelated", 50),
            ("MAIN", 60),
        ]
        .iter()
        .map(|(n, s)| node(n, *s, 0))
        .collect();

        let mut actual = items.clone();
        sort_by_rank(&mut actual, &q);

        let mut expected = items;
        expected.sort_by(|a, b| {
            let (sa, sb) = (rank_result(a, &q), rank_result(b, &q));
            sb.cmp(&sa).then(a.name.cmp(&b.name))
        });

        let actual: Vec<&str> = actual.iter().map(|n| n.name.as_str()).collect();
        let expected: Vec<&str> = expected.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn parse_and_match_thousands_of_nodes_quickly() {
        let items: Vec<VNode> = (0..5_000)
            .map(|i| {
                let name = if i % 7 == 0 {
                    format!("f{i}.rs")
                } else {
                    format!("f{i}.txt")
                };
                node(&name, (i * 10) as u64, NOW)
            })
            .collect();
        let q = ParsedQuery::parse("ext:rs size:>100");
        let started = std::time::Instant::now();
        let out = filter_and_rank(items, &q, NOW);
        let elapsed = started.elapsed();
        assert!(!out.is_empty());
        assert!(out.iter().all(|n| n.name.ends_with(".rs")));
        assert!(
            elapsed.as_millis() < 250,
            "filter/rank of 5000 nodes took {elapsed:?}"
        );
    }

    #[test]
    fn is_skipped_dir_covers_dotfiles_and_build_trees() {
        assert!(is_skipped_dir(".git"));
        assert!(is_skipped_dir("node_modules"));
        assert!(is_skipped_dir("target"));
        assert!(is_skipped_dir("Library"));
        assert!(!is_skipped_dir("src"));
        assert!(!is_skipped_dir("targets"));
    }

    #[test]
    fn sort_by_rank_is_deterministic_for_equal_scores() {
        let q = ParsedQuery::parse("");
        let mut a = vec![node("b.rs", 1, 0), node("a.rs", 1, 0), node("c.rs", 1, 0)];
        let mut b = vec![node("c.rs", 1, 0), node("b.rs", 1, 0), node("a.rs", 1, 0)];
        sort_by_rank(&mut a, &q);
        sort_by_rank(&mut b, &q);
        let names_a: Vec<&str> = a.iter().map(|n| n.name.as_str()).collect();
        let names_b: Vec<&str> = b.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names_a, names_b);
        assert_eq!(names_a, vec!["a.rs", "b.rs", "c.rs"]);
    }
}
