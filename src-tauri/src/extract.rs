//! Plain text out of documents, for `content:` search and the preview.
//!
//! PDF goes through `lopdf`. Office files (docx, xlsx, pptx) and OpenDocument
//! files (odt, ods, odp) are zip archives of XML, so only the parts that hold
//! text are read, through a streaming XML reader.
//!
//! Documents are untrusted. Every read is capped: the file size, the zip entry
//! count, the decompressed bytes per entry, the pages of a PDF and the text
//! kept. Parsing runs under `catch_unwind`, because PDF crates can panic on
//! malformed input, and the caller's cancel flag is checked between parts and
//! pages. A single PDF parse call cannot be interrupted, so the file-size cap
//! is what bounds it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use quick_xml::events::{BytesRef, Event};

/// Largest file whose text is extracted at all.
pub const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;
/// Largest text kept from one file. Anything past it is dropped.
pub const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;
/// Largest plain-text file that is searched. Same as the old search cap.
const MAX_PLAIN_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 100_000;
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 5_000;

const CACHE_ENTRIES: usize = 200;
const CACHE_BYTES: usize = 100 * 1024 * 1024;

const NOT_TEXT: &str = "not a readable text file";
const CORRUPT: &str = "document could not be read";
const TOO_LARGE: &str = "file too large to search inside";
const TOO_MANY_ENTRIES: &str = "document has too many parts";
const PART_TOO_LARGE: &str = "document part too large";
const TOO_MANY_PAGES: &str = "document has too many pages";
const CANCELLED: &str = "search cancelled";
const PANICKED: &str = "document parser failed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    /// odt, ods and odp: all three keep their text in `content.xml`.
    Odf,
}

fn format_of(path: &Path) -> Option<Format> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => Format::Pdf,
        "docx" => Format::Docx,
        "xlsx" => Format::Xlsx,
        "pptx" => Format::Pptx,
        "odt" | "ods" | "odp" => Format::Odf,
        _ => return None,
    })
}

/// Whether `path` is a document this module reads, rather than plain text.
pub fn is_document(path: &Path) -> bool {
    format_of(path).is_some()
}

/// The zip parts of a document that hold its text.
fn wanted_part(format: Format, name: &str) -> bool {
    match format {
        Format::Docx => name.strip_prefix("word/").is_some_and(|f| {
            f == "document.xml"
                || (f.ends_with(".xml")
                    && ["header", "footer", "footnotes", "endnotes"]
                        .iter()
                        .any(|p| f.starts_with(p)))
        }),
        Format::Xlsx => {
            name == "xl/sharedStrings.xml"
                || name
                    .strip_prefix("xl/worksheets/")
                    .is_some_and(|f| f.ends_with(".xml"))
        }
        Format::Pptx => name
            .strip_prefix("ppt/slides/")
            .is_some_and(|f| f.starts_with("slide") && f.ends_with(".xml")),
        Format::Odf => name == "content.xml",
        Format::Pdf => false,
    }
}

/// Caps applied to a zip-based document. Tests lower them.
#[derive(Clone, Copy, Debug)]
struct Limits {
    max_entries: usize,
    max_entry_bytes: u64,
}

const LIMITS: Limits = Limits {
    max_entries: MAX_ZIP_ENTRIES,
    max_entry_bytes: MAX_ENTRY_BYTES,
};

/// Collects text up to a byte cap, always cutting on a character boundary.
struct TextSink {
    text: String,
    cap: usize,
    full: bool,
}

impl TextSink {
    fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            cap,
            full: false,
        }
    }

    fn full(&self) -> bool {
        self.full
    }

    fn push_str(&mut self, s: &str) {
        if self.full {
            return;
        }
        let room = self.cap - self.text.len();
        if s.len() <= room {
            self.text.push_str(s);
            return;
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&s[..end]);
        self.full = true;
    }

    fn push(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.push_str(c.encode_utf8(&mut buf));
    }
}

/// Passes a zip entry through, failing once it yields more than `left` bytes.
/// The declared size is not trusted: a small entry can inflate to anything.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // One byte past the cap is asked for, so an exact fit and an overrun can be told apart.
        let want = buf
            .len()
            .min(usize::try_from(self.left.saturating_add(1)).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n as u64 > self.left {
            return Err(std::io::Error::other(PART_TOO_LARGE));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Ooxml,
    Odf,
}

/// Reads the text of one XML part into `sink`.
///
/// OOXML keeps text in `<t>` elements, with paragraphs in `<p>` (docx, pptx),
/// `<si>` and `<row>` (xlsx). ODF keeps it directly inside `<p>` and `<h>`.
fn xml_text(
    reader: impl BufRead,
    dialect: Dialect,
    cancelled: &dyn Fn() -> bool,
    sink: &mut TextSink,
) -> Result<(), String> {
    let mut xml = quick_xml::Reader::from_reader(reader);
    let mut buf = Vec::new();
    // Depth inside a text-bearing element, and inside a phonetic run (`rPh`), whose text is not wanted.
    let mut text_depth = 0usize;
    let mut skip_depth = 0usize;
    loop {
        if sink.full() || cancelled() {
            break;
        }
        let event = xml
            .read_event_into(&mut buf)
            .map_err(|_| CORRUPT.to_string())?;
        match event {
            Event::Start(e) => match (dialect, e.local_name().as_ref()) {
                (Dialect::Ooxml, "t") => text_depth += 1,
                (Dialect::Ooxml, "rPh") => skip_depth += 1,
                (Dialect::Odf, "p" | "h") => text_depth += 1,
                _ => {}
            },
            Event::Empty(e) => match (dialect, e.local_name().as_ref()) {
                (Dialect::Ooxml, "br") => sink.push('\n'),
                (Dialect::Ooxml, "ta") => sink.push('\t'),
                (Dialect::Odf, "s") if text_depth > 0 => sink.push(' '),
                (Dialect::Odf, "ta") if text_depth > 0 => sink.push('\t'),
                (Dialect::Odf, "line-break") if text_depth > 0 => sink.push('\n'),
                _ => {}
            },
            Event::End(e) => match (dialect, e.local_name().as_ref()) {
                (Dialect::Ooxml, "t") => text_depth = text_depth.saturating_sub(1),
                (Dialect::Ooxml, "rPh") => skip_depth = skip_depth.saturating_sub(1),
                (Dialect::Ooxml, "p" | "si" | "row" | "tr") => sink.push('\n'),
                (Dialect::Ooxml, "tc" | "c") => sink.push('\t'),
                (Dialect::Odf, "p" | "h") => {
                    text_depth = text_depth.saturating_sub(1);
                    sink.push('\n');
                }
                (Dialect::Odf, "table-row") => sink.push('\n'),
                (Dialect::Odf, "table-cell") => sink.push('\t'),
                _ => {}
            },
            Event::Text(e) if text_depth > 0 && skip_depth == 0 => {
                sink.push_str(&e.xml10_content());
            }
            Event::GeneralRef(e) if text_depth > 0 && skip_depth == 0 => {
                if let Some(c) = reference_char(&e) {
                    sink.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// The character an entity or character reference stands for.
fn reference_char(r: &BytesRef<'_>) -> Option<char> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c);
    }
    match r.xml10_content().as_ref() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

fn zip_text(
    path: &Path,
    format: Format,
    limits: Limits,
    cancelled: &dyn Fn() -> bool,
    sink: &mut TextSink,
) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|_| CORRUPT.to_string())?;
    if zip.len() > limits.max_entries {
        return Err(TOO_MANY_ENTRIES.to_string());
    }
    // Sorted by name, so slides and sheets come out in a stable order.
    let mut parts: Vec<(String, usize)> = (0..zip.len())
        .filter_map(|i| {
            zip.name_for_index(i)
                .and_then(|name| name.ok())
                .filter(|name| wanted_part(format, name))
                .map(|name| (name.to_string(), i))
        })
        .collect();
    if parts.is_empty() {
        return Err(CORRUPT.to_string());
    }
    parts.sort();

    let dialect = if format == Format::Odf {
        Dialect::Odf
    } else {
        Dialect::Ooxml
    };
    for (_, index) in parts {
        if sink.full() || cancelled() {
            break;
        }
        let entry = zip.by_index(index).map_err(|_| CORRUPT.to_string())?;
        if entry.size() > limits.max_entry_bytes {
            return Err(PART_TOO_LARGE.to_string());
        }
        let capped = Capped {
            inner: entry,
            left: limits.max_entry_bytes,
        };
        xml_text(BufReader::new(capped), dialect, cancelled, sink)?;
    }
    Ok(())
}

fn pdf_text(path: &Path, cancelled: &dyn Fn() -> bool, sink: &mut TextSink) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let doc = lopdf::Document::load_mem(&bytes).map_err(|_| CORRUPT.to_string())?;
    let pages = doc.get_pages();
    if pages.len() > MAX_PDF_PAGES {
        return Err(TOO_MANY_PAGES.to_string());
    }
    if pages.is_empty() {
        return Err(CORRUPT.to_string());
    }
    for page in pages.keys() {
        if sink.full() || cancelled() {
            break;
        }
        // A page that will not decode (odd fonts, broken streams) is skipped
        // rather than failing the whole document.
        if let Ok(text) = doc.extract_text_with_limit(&[*page], MAX_TEXT_BYTES) {
            sink.push_str(&text);
            sink.push('\n');
        }
    }
    Ok(())
}

fn extract_document(
    path: &Path,
    format: Format,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    let mut sink = TextSink::new(MAX_TEXT_BYTES);
    match format {
        Format::Pdf => pdf_text(path, cancelled, &mut sink)?,
        _ => zip_text(path, format, LIMITS, cancelled, &mut sink)?,
    }
    if cancelled() {
        return Err(CANCELLED.to_string());
    }
    Ok(sink.text)
}

fn read_plain(path: &Path, len: u64) -> Result<String, String> {
    if len > MAX_PLAIN_TEXT_BYTES {
        return Err(TOO_LARGE.to_string());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err(NOT_TEXT.to_string());
    }
    String::from_utf8(bytes).map_err(|_| NOT_TEXT.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

struct CacheEntry {
    text: Arc<str>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<CacheKey, CacheEntry>,
    clock: u64,
    bytes: usize,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn lock_cache() -> MutexGuard<'static, Option<Cache>> {
    // The cache holds only finished text, so a poisoned lock is still usable.
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cache_get(key: &CacheKey) -> Option<Arc<str>> {
    let mut guard = lock_cache();
    let cache = guard.get_or_insert_with(Cache::default);
    cache.clock += 1;
    let clock = cache.clock;
    let entry = cache.entries.get_mut(key)?;
    entry.used = clock;
    Some(entry.text.clone())
}

fn cache_put(key: CacheKey, text: Arc<str>) {
    if text.len() > CACHE_BYTES {
        return;
    }
    let mut guard = lock_cache();
    let cache = guard.get_or_insert_with(Cache::default);
    cache.clock += 1;
    if let Some(old) = cache.entries.remove(&key) {
        cache.bytes -= old.text.len();
    }
    while !cache.entries.is_empty()
        && (cache.entries.len() >= CACHE_ENTRIES || cache.bytes + text.len() > CACHE_BYTES)
    {
        let Some(oldest) = cache
            .entries
            .iter()
            .min_by_key(|(_, e)| e.used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        if let Some(gone) = cache.entries.remove(&oldest) {
            cache.bytes -= gone.text.len();
        }
    }
    cache.bytes += text.len();
    let used = cache.clock;
    cache.entries.insert(key, CacheEntry { text, used });
}

/// The searchable text of `path`: extracted for documents, read as UTF-8 for
/// plain text. Results are cached per (path, size, mtime), so a repeated
/// search does not parse the same file again. `cancelled` is polled while
/// parsing; a cancelled extraction returns an error and is not cached.
pub fn text_for_search(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<Arc<str>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(NOT_TEXT.to_string());
    }
    let key = CacheKey {
        path: path.to_path_buf(),
        size: meta.len(),
        modified: meta.modified().ok(),
    };
    if let Some(hit) = cache_get(&key) {
        return Ok(hit);
    }

    let text = match format_of(path) {
        Some(format) => {
            if meta.len() > MAX_INPUT_BYTES {
                return Err(TOO_LARGE.to_string());
            }
            // A PDF crate may panic on malformed input. That must cost one file, not the search.
            catch_unwind(AssertUnwindSafe(|| {
                extract_document(path, format, cancelled)
            }))
            .unwrap_or_else(|_| Err(PANICKED.to_string()))?
        }
        None => read_plain(path, meta.len())?,
    };
    let text: Arc<str> = Arc::from(text);
    cache_put(key, text.clone());
    Ok(text)
}

/// Lowercases one character, which can expand to several.
fn lower_chars(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase()
}

/// Byte range of the first case-insensitive occurrence of `needle` in `hay`.
/// The range indexes `hay`, so it can slice the original text. An empty needle has no match.
pub fn find_ci(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let needle: Vec<char> = needle.chars().flat_map(lower_chars).collect();
    if needle.is_empty() {
        return None;
    }
    hay.char_indices()
        .find_map(|(start, _)| match_at(hay, start, &needle).map(|end| (start, end)))
}

/// End byte of a match of `needle` (already lowercased) starting at `start`.
fn match_at(hay: &str, start: usize, needle: &[char]) -> Option<usize> {
    let mut next = 0;
    for (offset, c) in hay[start..].char_indices() {
        let end = start + offset + c.len_utf8();
        for lowered in lower_chars(c) {
            if needle[next] != lowered {
                return None;
            }
            next += 1;
            if next == needle.len() {
                return Some(end);
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::io::{Cursor, Write};
    use std::path::PathBuf;

    use crate::testutil::TempDir;

    /// A zip archive holding `parts` as (name, text) pairs.
    pub fn zip_bytes(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    pub fn docx(tmp: &TempDir, name: &str, body_xml: &str) -> PathBuf {
        let doc = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body_xml}</w:body></w:document>"#
        );
        tmp.file(
            name,
            &zip_bytes(&[
                ("[Content_Types].xml", "<Types/>"),
                ("word/document.xml", &doc),
            ]),
        )
    }

    pub fn pdf(lines: &[&str]) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 24.into()]),
            Operation::new("Td", vec![72.into(), 700.into()]),
        ];
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                ops.push(Operation::new("Td", vec![0.into(), (-30).into()]));
            }
            ops.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
        }
        ops.push(Operation::new("ET", vec![]));
        let content = Content { operations: ops }.encode().unwrap();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }
            .into(),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{docx, pdf, zip_bytes};
    use super::*;
    use crate::testutil::TempDir;

    fn no_cancel() -> bool {
        false
    }

    fn text_of(path: &Path) -> String {
        text_for_search(path, &no_cancel).unwrap().to_string()
    }

    #[test]
    fn docx_text_keeps_paragraphs_and_unescapes() {
        let tmp = TempDir::new("extract_docx");
        let path = docx(
            &tmp,
            "a.docx",
            "<w:p><w:r><w:t>Hello &amp; welcome</w:t></w:r></w:p>\
             <w:p><w:r><w:t xml:space=\"preserve\">Second </w:t></w:r><w:r><w:t>line</w:t></w:r></w:p>",
        );
        let text = text_of(&path);
        assert_eq!(text, "Hello & welcome\nSecond line\n");
    }

    #[test]
    fn xlsx_reads_shared_and_inline_strings() {
        let tmp = TempDir::new("extract_xlsx");
        let shared = r#"<sst><si><t>Alpha</t></si><si><t>Beta</t></si></sst>"#;
        let sheet = r#"<worksheet><sheetData><row><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>Gamma</t></is></c></row></sheetData></worksheet>"#;
        let path = tmp.file(
            "a.xlsx",
            &zip_bytes(&[
                ("xl/sharedStrings.xml", shared),
                ("xl/worksheets/sheet1.xml", sheet),
            ]),
        );
        let text = text_of(&path);
        assert!(text.contains("Alpha"), "{text:?}");
        assert!(text.contains("Beta"), "{text:?}");
        assert!(text.contains("Gamma"), "{text:?}");
    }

    #[test]
    fn pptx_reads_slide_text() {
        let tmp = TempDir::new("extract_pptx");
        let slide = r#"<p:sld xmlns:a="x" xmlns:p="y"><a:p><a:r><a:t>Slide title</a:t></a:r></a:p><a:p><a:r><a:t>Bullet</a:t></a:r></a:p></p:sld>"#;
        let path = tmp.file("a.pptx", &zip_bytes(&[("ppt/slides/slide1.xml", slide)]));
        assert_eq!(text_of(&path), "Slide title\nBullet\n");
    }

    #[test]
    fn odt_reads_headings_and_paragraphs() {
        let tmp = TempDir::new("extract_odt");
        let content = r#"<?xml version="1.0"?>
<office:document-content xmlns:office="o" xmlns:text="t">
  <office:body><office:text>
    <text:h>Heading</text:h>
    <text:p>Body <text:span>text</text:span> here</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let path = tmp.file("a.odt", &zip_bytes(&[("content.xml", content)]));
        assert_eq!(text_of(&path), "Heading\nBody text here\n");
    }

    #[test]
    fn pdf_text_is_extracted() {
        let tmp = TempDir::new("extract_pdf");
        let path = tmp.file("a.pdf", &pdf(&["Hello PDF", "Second page line"]));
        let text = text_of(&path);
        assert!(text.contains("Hello PDF"), "{text:?}");
        assert!(text.contains("Second page line"), "{text:?}");
    }

    #[test]
    fn garbage_pdf_is_an_error_not_a_crash() {
        let tmp = TempDir::new("extract_garbage");
        let path = tmp.file("bad.pdf", b"%PDF-1.4\n\x00\xff garbage << >> endobj stream");
        assert!(text_for_search(&path, &no_cancel).is_err());
        let junk = tmp.file("junk.pdf", &vec![0x25u8; 4096]);
        assert!(text_for_search(&junk, &no_cancel).is_err());
    }

    #[test]
    fn corrupt_zip_is_an_error() {
        let tmp = TempDir::new("extract_zipjunk");
        let path = tmp.file("bad.docx", b"PK\x03\x04 not really a zip");
        assert!(text_for_search(&path, &no_cancel).is_err());
    }

    #[test]
    fn zip_without_text_parts_is_an_error() {
        let tmp = TempDir::new("extract_noparts");
        let path = tmp.file("a.docx", &zip_bytes(&[("readme.txt", "hi")]));
        assert!(text_for_search(&path, &no_cancel).is_err());
    }

    #[test]
    fn oversized_file_is_refused_before_reading() {
        let tmp = TempDir::new("extract_size");
        let path = tmp.path().join("huge.pdf");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_INPUT_BYTES + 1).unwrap();
        let err = text_for_search(&path, &no_cancel).unwrap_err();
        assert_eq!(err, TOO_LARGE);
    }

    #[test]
    fn zip_entry_count_is_capped() {
        let tmp = TempDir::new("extract_entries");
        let path = tmp.file(
            "a.docx",
            &zip_bytes(&[
                ("word/document.xml", "<w:t>one</w:t>"),
                ("word/header1.xml", "<w:t>two</w:t>"),
            ]),
        );
        let mut sink = TextSink::new(MAX_TEXT_BYTES);
        let tight = Limits {
            max_entries: 1,
            max_entry_bytes: MAX_ENTRY_BYTES,
        };
        let err = zip_text(&path, Format::Docx, tight, &no_cancel, &mut sink).unwrap_err();
        assert_eq!(err, TOO_MANY_ENTRIES);
    }

    #[test]
    fn decompressed_entry_size_is_capped() {
        let tmp = TempDir::new("extract_entry_bytes");
        let body = format!("<w:t>{}</w:t>", "x".repeat(5_000));
        let path = tmp.file("a.docx", &zip_bytes(&[("word/document.xml", &body)]));
        let mut sink = TextSink::new(MAX_TEXT_BYTES);
        let tight = Limits {
            max_entries: MAX_ZIP_ENTRIES,
            max_entry_bytes: 1_000,
        };
        assert!(zip_text(&path, Format::Docx, tight, &no_cancel, &mut sink).is_err());
    }

    #[test]
    fn kept_text_is_capped_on_a_character_boundary() {
        let mut sink = TextSink::new(10);
        sink.push_str("ééééééé"); // 14 bytes
        assert!(sink.full());
        assert!(sink.text.len() <= 10);
        assert!(sink.text.is_char_boundary(sink.text.len()));
        sink.push_str("more");
        assert!(sink.text.len() <= 10);
    }

    #[test]
    fn cancelled_extraction_is_an_error_and_not_cached() {
        let tmp = TempDir::new("extract_cancel");
        let path = docx(&tmp, "c.docx", "<w:p><w:r><w:t>cancel me</w:t></w:r></w:p>");
        assert!(text_for_search(&path, &|| true).is_err());
        assert!(text_for_search(&path, &no_cancel)
            .unwrap()
            .contains("cancel me"));
    }

    #[test]
    fn cache_returns_the_same_text_until_the_file_changes() {
        let tmp = TempDir::new("extract_cache");
        let path = docx(&tmp, "c.docx", "<w:p><w:r><w:t>first</w:t></w:r></w:p>");
        let a = text_for_search(&path, &no_cancel).unwrap();
        let b = text_for_search(&path, &no_cancel).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn plain_text_is_read_and_binary_is_refused() {
        let tmp = TempDir::new("extract_plain");
        let text = tmp.file("notes.txt", b"plain words here");
        let bin = tmp.file("a.bin", b"hello\0world");
        assert_eq!(text_of(&text), "plain words here");
        assert!(text_for_search(&bin, &no_cancel).is_err());
    }

    #[test]
    fn find_ci_is_case_insensitive_and_returns_original_offsets() {
        let hay = "Größe TODO end";
        let (s, e) = find_ci(hay, "todo").unwrap();
        assert_eq!(&hay[s..e], "TODO");
        assert_eq!(find_ci("Hello", "xyz"), None);
        assert_eq!(find_ci("Hello", ""), None);
        let (s, e) = find_ci("ÉCOLE", "école").unwrap();
        assert_eq!(&"ÉCOLE"[s..e], "ÉCOLE");
    }
}
