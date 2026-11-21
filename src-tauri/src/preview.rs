//! File previews for the details panel.
//!
//! Text is read line by line with hard caps so a multi-gigabyte log cannot
//! stall the backend. Images, audio and video are handed to the webview as a
//! data URL and decoded there, so the backend needs no codecs. Unknown binary
//! files get a hex dump of their first few bytes.
//!
//! Media and images are only read when they fit in `MAX_PREVIEW_BYTES`, so a
//! data URL never grows past that cap. The webview gets bytes, never a path,
//! so no file-serving scope is needed.

use std::io::Read;
use std::path::Path;

use base64::Engine;

pub(crate) const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes shown in the hex view of a binary file. Also the sample used to tell text from binary.
pub const HEX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_PREVIEW_LINES: usize = 30;
const MAX_PREVIEW_TEXT_BYTES: usize = 64 * 1024;
/// Most bytes of a Markdown file read for rendering.
const MAX_MARKDOWN_BYTES: usize = 64 * 1024;

/// Extensions shown as plain text.
const PLAIN_EXTENSIONS: &[&str] = &["txt", "log", "csv", "ini", "cfg"];

/// Source files shown with syntax highlighting. The second field is the
/// language name the webview's highlighter knows.
const CODE_LANGUAGES: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("py", "python"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("json", "json"),
    ("css", "css"),
    ("html", "xml"),
    ("xml", "xml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "ini"),
    ("sh", "bash"),
    ("bat", "dos"),
    ("ps1", "powershell"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("go", "go"),
    ("java", "java"),
];

/// Video and audio the webview can play, with the MIME type to label the data URL with.
const MEDIA: &[(&str, &str, bool)] = &[
    ("mp4", "video/mp4", false),
    ("m4v", "video/mp4", false),
    ("webm", "video/webm", false),
    ("mov", "video/quicktime", false),
    ("ogv", "video/ogg", false),
    ("mp3", "audio/mpeg", true),
    ("m4a", "audio/mp4", true),
    ("wav", "audio/wav", true),
    ("ogg", "audio/ogg", true),
    ("opus", "audio/ogg", true),
    ("flac", "audio/flac", true),
    ("aac", "audio/aac", true),
];

#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Preview {
    None,
    Text {
        lines: Vec<String>,
        truncated: bool,
    },
    Code {
        language: &'static str,
        lines: Vec<String>,
        truncated: bool,
    },
    /// Raw Markdown. The webview renders it with raw HTML disabled.
    Markdown {
        text: String,
        truncated: bool,
    },
    Image {
        data_url: String,
    },
    Video {
        data_url: String,
    },
    Audio {
        data_url: String,
    },
    /// The first `HEX_PREVIEW_BYTES` of a binary file, out of `size` bytes.
    Hex {
        bytes: Vec<u8>,
        size: u64,
    },
}

#[derive(Clone, Copy)]
enum TextKind {
    Plain,
    Code(&'static str),
    Markdown,
}

fn image_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default()
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn image_preview(mime: &str, bytes: &[u8]) -> Preview {
    Preview::Image {
        data_url: data_url(mime, bytes),
    }
}

fn media_preview(mime: &str, audio: bool, bytes: &[u8]) -> Preview {
    let data_url = data_url(mime, bytes);
    if audio {
        Preview::Audio { data_url }
    } else {
        Preview::Video { data_url }
    }
}

/// The whole file, if it is no bigger than `MAX_PREVIEW_BYTES`.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_PREVIEW_BYTES)
        .unwrap_or(true);
    if too_big {
        return None;
    }
    std::fs::read(path).ok()
}

pub fn decode_preview(path: &Path) -> Preview {
    let ext = extension_of(path);

    if let Some(mime) = image_mime(&ext) {
        return match read_small(path) {
            Some(bytes) => image_preview(mime, &bytes),
            None => Preview::None,
        };
    }
    if let Some((mime, audio)) = media_kind(&ext) {
        return match read_small(path) {
            Some(bytes) => media_preview(mime, audio, &bytes),
            None => Preview::None,
        };
    }

    let Ok(file) = std::fs::File::open(path) else {
        return Preview::None;
    };
    if let Some(kind) = text_kind(&ext) {
        return text_as(kind, file);
    }

    // Unknown types: text if the start of the file reads as text, else hex.
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut head = Vec::new();
    if file
        .take(HEX_PREVIEW_BYTES as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return Preview::None;
    }
    unknown_preview(&head, size)
}

/// The preview of an entry read out of an archive. `bytes` holds at most the
/// first `MAX_PREVIEW_BYTES` of the entry, whose full size is `size`.
pub fn decode_entry_preview(name: &Path, size: u64, bytes: &[u8]) -> Preview {
    let ext = extension_of(name);
    if let Some(mime) = image_mime(&ext) {
        if size > MAX_PREVIEW_BYTES {
            return Preview::None;
        }
        return image_preview(mime, bytes);
    }
    if let Some((mime, audio)) = media_kind(&ext) {
        if size > MAX_PREVIEW_BYTES {
            return Preview::None;
        }
        return media_preview(mime, audio, bytes);
    }
    if let Some(kind) = text_kind(&ext) {
        return text_as(kind, bytes);
    }
    unknown_preview(&bytes[..bytes.len().min(HEX_PREVIEW_BYTES)], size)
}

fn text_kind(ext: &str) -> Option<TextKind> {
    if ext == "md" || ext == "markdown" {
        return Some(TextKind::Markdown);
    }
    if let Some((_, language)) = CODE_LANGUAGES.iter().find(|(e, _)| *e == ext) {
        return Some(TextKind::Code(language));
    }
    PLAIN_EXTENSIONS.contains(&ext).then_some(TextKind::Plain)
}

fn media_kind(ext: &str) -> Option<(&'static str, bool)> {
    MEDIA
        .iter()
        .find(|(e, _, _)| *e == ext)
        .map(|(_, mime, audio)| (*mime, *audio))
}

fn text_as(kind: TextKind, reader: impl Read) -> Preview {
    match kind {
        TextKind::Plain => text_preview(std::io::BufReader::new(reader)),
        TextKind::Code(language) => match text_preview(std::io::BufReader::new(reader)) {
            Preview::Text { lines, truncated } => Preview::Code {
                language,
                lines,
                truncated,
            },
            other => other,
        },
        TextKind::Markdown => {
            let mut bytes = Vec::new();
            if reader
                .take(MAX_MARKDOWN_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return Preview::None;
            }
            markdown_preview(bytes)
        }
    }
}

fn markdown_preview(mut bytes: Vec<u8>) -> Preview {
    let truncated = bytes.len() > MAX_MARKDOWN_BYTES;
    bytes.truncate(MAX_MARKDOWN_BYTES);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.trim().is_empty() {
        Preview::None
    } else {
        Preview::Markdown { text, truncated }
    }
}

/// True when a sample reads as text: no NUL bytes and valid UTF-8. A character
/// cut off at the end of the sample still counts as text.
fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// `head` holds the first bytes of a file whose full size is `size`.
fn unknown_preview(head: &[u8], size: u64) -> Preview {
    if head.is_empty() {
        return Preview::None;
    }
    if looks_like_text(head) {
        return match text_preview(head) {
            Preview::Text { lines, truncated } => Preview::Text {
                lines,
                truncated: truncated || size > head.len() as u64,
            },
            other => other,
        };
    }
    hex_preview(head, size)
}

/// The first `HEX_PREVIEW_BYTES` of `bytes`, whatever the caller passed in.
fn hex_preview(bytes: &[u8], size: u64) -> Preview {
    Preview::Hex {
        bytes: bytes[..bytes.len().min(HEX_PREVIEW_BYTES)].to_vec(),
        size,
    }
}

fn text_preview(reader: impl std::io::BufRead) -> Preview {
    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for line in std::io::BufRead::lines(reader) {
        let Ok(line) = line else {
            break;
        };
        bytes = bytes.saturating_add(line.len() + 1);
        if lines.len() >= MAX_PREVIEW_LINES || bytes > MAX_PREVIEW_TEXT_BYTES {
            truncated = true;
            break;
        }
        lines.push(line);
    }
    if lines.is_empty() {
        Preview::None
    } else {
        Preview::Text { lines, truncated }
    }
}

/// Most matching lines shown in the details panel.
pub const CONTENT_MATCH_LIMIT: usize = 20;
/// Characters of context kept on each side of a match.
const CONTENT_CONTEXT_CHARS: usize = 40;
/// Characters of extracted text shown for a document.
const EXCERPT_CHARS: usize = 600;

/// One matching line: the text around the match, with the match split out so
/// the frontend can highlight it.
#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub struct Snippet {
    /// 1-based line number in the extracted text.
    pub line: usize,
    pub before: String,
    pub matched: String,
    pub after: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ContentMatches {
    /// Lines that contain the needle, across the whole file.
    pub matching_lines: usize,
    /// The first `CONTENT_MATCH_LIMIT` of them, in file order.
    pub snippets: Vec<Snippet>,
    /// Start of the extracted text. Set for documents only.
    pub excerpt: Option<String>,
}

/// Where a `content:` search matched inside `path`.
pub fn content_matches(path: &Path, needle: &str) -> Result<ContentMatches, String> {
    let text = crate::extract::text_for_search(path, &|| false)?;
    let mut out = ContentMatches {
        matching_lines: 0,
        snippets: Vec::new(),
        excerpt: crate::extract::is_document(path).then(|| excerpt(&text)),
    };
    if needle.is_empty() {
        return Ok(out);
    }
    for (index, line) in text.lines().enumerate() {
        let Some((start, end)) = crate::extract::find_ci(line, needle) else {
            continue;
        };
        out.matching_lines += 1;
        if out.snippets.len() < CONTENT_MATCH_LIMIT {
            out.snippets.push(Snippet {
                line: index + 1,
                before: last_chars(&line[..start], CONTENT_CONTEXT_CHARS).to_string(),
                matched: line[start..end].to_string(),
                after: first_chars(&line[end..], CONTENT_CONTEXT_CHARS).to_string(),
            });
        }
    }
    Ok(out)
}

/// The first lines of the extracted text, cut back to a whole line where possible.
fn excerpt(text: &str) -> String {
    let head = first_chars(text, EXCERPT_CHARS);
    let shown = if head.len() < text.len() {
        head.rfind('\n').map_or(head, |i| &head[..i])
    } else {
        head
    };
    shown.trim_end().to_string()
}

/// The first `n` characters of `s`, cut on a character boundary.
fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The last `n` characters of `s`, cut on a character boundary.
fn last_chars(s: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    match s.char_indices().rev().nth(n - 1) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn decode_preview_caps_text_at_thirty_lines() {
        let tmp = TempDir::new("preview_lines");
        let mut body = String::new();
        for i in 0..80 {
            body.push_str(&format!("line {i}\n"));
        }
        let path = tmp.file("big.log", body.as_bytes());
        match decode_preview(&path) {
            Preview::Text { lines, truncated } => {
                assert_eq!(lines.len(), 30);
                assert!(truncated);
                assert_eq!(lines[0], "line 0");
            }
            other => panic!("expected text preview, got {other:?}"),
        }
    }

    #[test]
    fn decode_preview_caps_text_by_bytes() {
        let tmp = TempDir::new("preview_bytes");
        let line = "x".repeat(8000);
        let mut body = String::new();
        for _ in 0..20 {
            body.push_str(&line);
            body.push('\n');
        }
        let path = tmp.file("wide.log", body.as_bytes());
        match decode_preview(&path) {
            Preview::Text { lines, truncated } => {
                assert!(truncated);
                assert!(lines.len() < 20);
            }
            other => panic!("expected text preview, got {other:?}"),
        }
    }

    #[test]
    fn images_become_data_urls() {
        let tmp = TempDir::new("preview_img");
        let path = tmp.file("a.png", b"\x89PNG fake");
        match decode_preview(&path) {
            Preview::Image { data_url } => assert!(data_url.starts_with("data:image/png;base64,")),
            other => panic!("expected image preview, got {other:?}"),
        }
    }

    #[test]
    fn unknown_binary_gets_a_hex_preview() {
        let tmp = TempDir::new("preview_bin");
        let path = tmp.file("a.exe", b"MZ\0\x01");
        match decode_preview(&path) {
            Preview::Hex { bytes, size } => {
                assert_eq!(bytes, b"MZ\0\x01");
                assert_eq!(size, 4);
            }
            other => panic!("expected hex preview, got {other:?}"),
        }
    }

    #[test]
    fn hex_preview_is_capped_at_four_kib() {
        let tmp = TempDir::new("preview_hex_cap");
        let mut body = vec![0u8; 10_000];
        body[0] = 0xFF;
        let path = tmp.file("big.bin", &body);
        match decode_preview(&path) {
            Preview::Hex { bytes, size } => {
                assert_eq!(bytes.len(), HEX_PREVIEW_BYTES);
                assert_eq!(bytes[0], 0xFF);
                assert_eq!(size, 10_000);
            }
            other => panic!("expected hex preview, got {other:?}"),
        }
    }

    #[test]
    fn hex_preview_cap_holds_for_archive_entries_too() {
        let mut bytes = vec![7u8; 9000];
        bytes[0] = 0;
        match decode_entry_preview(Path::new("data.bin"), 9000, &bytes) {
            Preview::Hex { bytes, size } => {
                assert_eq!(bytes.len(), HEX_PREVIEW_BYTES);
                assert_eq!(size, 9000);
            }
            other => panic!("expected hex preview, got {other:?}"),
        }
    }

    #[test]
    fn extensionless_text_is_shown_as_text() {
        let tmp = TempDir::new("preview_noext");
        let path = tmp.file("LICENSE", b"line one\nline two\n");
        match decode_preview(&path) {
            Preview::Text { lines, truncated } => {
                assert_eq!(lines, vec!["line one", "line two"]);
                assert!(!truncated);
            }
            other => panic!("expected text preview, got {other:?}"),
        }
    }

    #[test]
    fn extensionless_text_past_the_sample_is_marked_truncated() {
        let tmp = TempDir::new("preview_noext_long");
        let body = "word\n".repeat(2000);
        let path = tmp.file("notes", body.as_bytes());
        match decode_preview(&path) {
            Preview::Text { truncated, .. } => assert!(truncated),
            other => panic!("expected text preview, got {other:?}"),
        }
    }

    #[test]
    fn source_files_get_a_language_for_highlighting() {
        let tmp = TempDir::new("preview_code");
        let path = tmp.file("main.rs", b"fn main() {}\n");
        match decode_preview(&path) {
            Preview::Code {
                language,
                lines,
                truncated,
            } => {
                assert_eq!(language, "rust");
                assert_eq!(lines, vec!["fn main() {}"]);
                assert!(!truncated);
            }
            other => panic!("expected code preview, got {other:?}"),
        }
    }

    #[test]
    fn markdown_is_returned_raw_for_the_webview_to_render() {
        let tmp = TempDir::new("preview_md");
        let path = tmp.file("readme.md", b"# Title\n\n<script>x()</script>\n");
        match decode_preview(&path) {
            Preview::Markdown { text, truncated } => {
                assert!(text.starts_with("# Title"));
                assert!(text.contains("<script>"));
                assert!(!truncated);
            }
            other => panic!("expected markdown preview, got {other:?}"),
        }
    }

    #[test]
    fn markdown_is_capped_and_marked_truncated() {
        let tmp = TempDir::new("preview_md_cap");
        let body = "a".repeat(MAX_MARKDOWN_BYTES + 500);
        let path = tmp.file("long.md", body.as_bytes());
        match decode_preview(&path) {
            Preview::Markdown { text, truncated } => {
                assert!(truncated);
                assert_eq!(text.len(), MAX_MARKDOWN_BYTES);
            }
            other => panic!("expected markdown preview, got {other:?}"),
        }
    }

    #[test]
    fn video_and_audio_become_data_urls() {
        let tmp = TempDir::new("preview_media");
        let video = tmp.file("clip.mp4", b"\0\0\0 ftypisom");
        let audio = tmp.file("song.mp3", b"ID3");
        match decode_preview(&video) {
            Preview::Video { data_url } => assert!(data_url.starts_with("data:video/mp4;base64,")),
            other => panic!("expected video preview, got {other:?}"),
        }
        match decode_preview(&audio) {
            Preview::Audio { data_url } => assert!(data_url.starts_with("data:audio/mpeg;base64,")),
            other => panic!("expected audio preview, got {other:?}"),
        }
    }

    #[test]
    fn empty_unknown_file_has_no_preview() {
        let tmp = TempDir::new("preview_empty_bin");
        let path = tmp.file("empty.dat", b"");
        assert!(matches!(decode_preview(&path), Preview::None));
    }

    #[test]
    fn content_matches_gives_a_context_window_around_each_match() {
        let tmp = TempDir::new("content_window");
        let line = format!("{}Needle{}", "a".repeat(100), "z".repeat(100));
        let path = tmp.file("notes.txt", format!("first\n{line}\n").as_bytes());
        let found = content_matches(&path, "needle").unwrap();
        assert_eq!(found.matching_lines, 1);
        assert_eq!(found.snippets.len(), 1);
        let snip = &found.snippets[0];
        assert_eq!(snip.line, 2);
        assert_eq!(snip.matched, "Needle");
        assert_eq!(snip.before, "a".repeat(40));
        assert_eq!(snip.after, "z".repeat(40));
        assert!(found.excerpt.is_none(), "plain text has no excerpt");
    }

    #[test]
    fn content_matches_context_respects_multibyte_characters() {
        let tmp = TempDir::new("content_utf8");
        let line = format!("{}café{}", "é".repeat(50), "ü".repeat(50));
        let path = tmp.file("u.txt", format!("{line}\n").as_bytes());
        let found = content_matches(&path, "CAFÉ").unwrap();
        let snip = &found.snippets[0];
        assert_eq!(snip.matched, "café");
        assert_eq!(snip.before.chars().count(), 40);
        assert_eq!(snip.after.chars().count(), 40);
    }

    #[test]
    fn content_matches_caps_snippets_but_counts_every_line() {
        let tmp = TempDir::new("content_cap");
        let body: String = (0..30).map(|i| format!("row {i} needle\n")).collect();
        let path = tmp.file("many.txt", body.as_bytes());
        let found = content_matches(&path, "needle").unwrap();
        assert_eq!(found.matching_lines, 30);
        assert_eq!(found.snippets.len(), CONTENT_MATCH_LIMIT);
        assert_eq!(found.snippets[0].line, 1);
    }

    #[test]
    fn content_matches_reads_a_docx_and_returns_an_excerpt() {
        let tmp = TempDir::new("content_docx_preview");
        let path = crate::extract::fixtures::docx(
            &tmp,
            "report.docx",
            "<w:p><w:r><w:t>Intro paragraph</w:t></w:r></w:p>\
             <w:p><w:r><w:t>The Budget grows in Q3</w:t></w:r></w:p>",
        );
        let found = content_matches(&path, "budget").unwrap();
        assert_eq!(found.matching_lines, 1);
        assert_eq!(found.snippets[0].line, 2);
        assert_eq!(found.snippets[0].matched, "Budget");
        let excerpt = found.excerpt.expect("documents carry an excerpt");
        assert!(excerpt.starts_with("Intro paragraph"), "{excerpt:?}");
    }

    #[test]
    fn content_matches_with_empty_needle_has_no_snippets() {
        let tmp = TempDir::new("content_empty");
        let path = tmp.file("e.txt", b"anything\n");
        let found = content_matches(&path, "").unwrap();
        assert_eq!(found.matching_lines, 0);
        assert!(found.snippets.is_empty());
    }
}
