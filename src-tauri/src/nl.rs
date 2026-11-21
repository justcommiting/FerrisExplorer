//! Deterministic natural-language → query rewrite.
//!
//! No LLM. Common phrases become the structured syntax the parser already
//! understands. Queries that already contain a filter (`key:`) are left alone.

/// If `raw` looks like a natural-language request, return a structured query.
/// Otherwise return `None` so the original string is parsed as-is.
pub fn rewrite_if_natural_language(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Already structured, or a quoted name search — don't rewrite.
    if trimmed.contains(':') {
        return None;
    }

    let s = trimmed.to_lowercase();
    let mut parts: Vec<String> = Vec::new();

    if let Some(ext) = detect_type(&s) {
        parts.push(format!("type:{ext}"));
    }
    if let Some(size) = detect_min_size(&s) {
        parts.push(format!("size:>{size}"));
    }
    if let Some(modi) = detect_modified(&s) {
        parts.push(modi);
    }
    if s.contains("untracked") {
        parts.push("git:untracked".into());
    } else if s.contains("git")
        || s.contains("modified files")
        || (s.contains("changed") && s.contains("file"))
    {
        parts.push("git:modified".into());
    }
    if s.contains("duplicate") {
        parts.push("duplicate:true".into());
    }
    if s.contains("todo") {
        parts.push("content:\"TODO\"".into());
    }
    if let Some(name) = detect_named(&s) {
        parts.push(format!("name:\"{name}\""));
    }

    if parts.is_empty() {
        return None;
    }
    let rewritten = parts.join(" ");
    if rewritten.eq_ignore_ascii_case(trimmed) {
        return None;
    }
    Some(rewritten)
}

fn detect_type(s: &str) -> Option<&'static str> {
    const KINDS: &[(&str, &str)] = &[
        ("pdf", "pdf"),
        ("rust", "rs"),
        ("python", "py"),
        ("javascript", "js"),
        ("typescript", "ts"),
        ("markdown", "md"),
        ("json", "json"),
        ("toml", "toml"),
        ("image", "png"),
        ("png", "png"),
        ("jpeg", "jpg"),
        ("jpg", "jpg"),
        ("zip", "zip"),
        ("folder", "folder"),
        ("directories", "folder"),
    ];
    for (word, ext) in KINDS {
        if s.contains(word) {
            return Some(*ext);
        }
    }
    None
}

fn detect_min_size(s: &str) -> Option<String> {
    for cue in [
        "larger than ",
        "bigger than ",
        "greater than ",
        "over ",
        "more than ",
        "at least ",
        "> ",
    ] {
        if let Some(rest) = find_after(s, cue) {
            if let Some(tok) = take_size_token(rest) {
                return Some(tok);
            }
        }
    }
    if s.contains("large file") {
        return Some("1gb".into());
    }
    None
}

fn detect_modified(s: &str) -> Option<String> {
    if s.contains("yesterday") {
        return Some("modified:yesterday".into());
    }
    if s.contains("today") {
        return Some("modified:today".into());
    }
    if s.contains("last week") || s.contains("past week") {
        return Some("modified:<7d".into());
    }
    if s.contains("last hour") || s.contains("past hour") {
        return Some("modified:<1h".into());
    }
    if s.contains("recent") {
        return Some("modified:<24h".into());
    }
    for cue in ["in the last ", "modified in the last ", "past "] {
        if let Some(rest) = find_after(s, cue) {
            if let Some(dur) = take_duration_token(rest) {
                return Some(format!("modified:<{dur}"));
            }
        }
    }
    None
}

fn detect_named(s: &str) -> Option<String> {
    for cue in ["named ", "called ", "name "] {
        if let Some(rest) = find_after(s, cue) {
            let name = rest
                .trim()
                .trim_matches('"')
                .split_whitespace()
                .next()?
                .to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn find_after<'a>(s: &'a str, cue: &str) -> Option<&'a str> {
    let idx = s.find(cue)?;
    Some(&s[idx + cue.len()..])
}

fn take_size_token(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let mut chars = rest.chars().peekable();
    let mut num = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() || *c == '.' {
            num.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let unit: String = chars
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_lowercase();
    let unit = match unit.as_str() {
        "k" | "kb" | "kib" | "kilobytes" | "kilobyte" => "kb",
        "m" | "mb" | "mib" | "megabytes" | "megabyte" => "mb",
        "g" | "gb" | "gib" | "gigabytes" | "gigabyte" => "gb",
        "t" | "tb" | "tib" => "tb",
        "b" | "bytes" | "byte" | "" => "b",
        _ => return None,
    };
    Some(format!("{num}{unit}"))
}

fn take_duration_token(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let mut chars = rest.chars().peekable();
    let mut num = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            num.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let unit: String = chars
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_lowercase();
    let unit = match unit.as_str() {
        "d" | "day" | "days" => "d",
        "h" | "hour" | "hours" => "h",
        "m" | "min" | "mins" | "minute" | "minutes" => "m",
        "w" | "week" | "weeks" => "w",
        "s" | "sec" | "secs" | "second" | "seconds" => "s",
        _ => return None,
    };
    Some(format!("{num}{unit}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_queries_are_left_alone() {
        assert_eq!(rewrite_if_natural_language("type:pdf size:>1mb"), None);
        assert_eq!(rewrite_if_natural_language("main.rs"), None);
    }

    #[test]
    fn find_pdfs_larger_than_100mb() {
        assert_eq!(
            rewrite_if_natural_language("find PDFs larger than 100MB"),
            Some("type:pdf size:>100mb".into())
        );
    }

    #[test]
    fn files_modified_yesterday() {
        assert_eq!(
            rewrite_if_natural_language("files modified yesterday"),
            Some("modified:yesterday".into())
        );
    }

    #[test]
    fn rust_files_and_todos() {
        assert_eq!(
            rewrite_if_natural_language("rust files"),
            Some("type:rs".into())
        );
        assert_eq!(
            rewrite_if_natural_language("TODOs"),
            Some("content:\"TODO\"".into())
        );
    }

    #[test]
    fn large_files_and_git_changes() {
        assert_eq!(
            rewrite_if_natural_language("large files"),
            Some("size:>1gb".into())
        );
        assert_eq!(
            rewrite_if_natural_language("git changes"),
            Some("git:modified".into())
        );
    }

    #[test]
    fn last_n_days_and_named() {
        assert_eq!(
            rewrite_if_natural_language("files modified in the last 7 days"),
            Some("modified:<7d".into())
        );
        assert_eq!(
            rewrite_if_natural_language("files named Cargo"),
            Some("name:\"cargo\"".into())
        );
    }

    #[test]
    fn spaces_in_size_units() {
        assert_eq!(
            rewrite_if_natural_language("images bigger than 2 gb"),
            Some("type:png size:>2gb".into())
        );
    }
}
