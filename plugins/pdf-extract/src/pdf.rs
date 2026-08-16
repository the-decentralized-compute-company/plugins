//! Opening a PDF without letting it open you.
//!
//! Every guard here exists because the file came from somewhere else and this
//! process runs on hardware somebody contributed:
//!
//! * the size ceiling is reached by a `stat`, before a byte is read
//! * the header is checked before the parser is handed the bytes, so a file
//!   that is not a PDF fails with "not a PDF" rather than with parser noise
//! * every compressed stream is bounded by `--max-decompressed-bytes`, which is
//!   the decompression-bomb guard: object streams inflate while the document
//!   loads, before any of this crate's code runs
//! * an encrypted document that will not open with an empty password is
//!   reported as encrypted rather than as empty
//!
//! And the classification that gives this plugin its point:
//! [`PageKind::ImageOnly`]. A scanned page carries no text, and a naive
//! extractor returns an empty string for it, which reads exactly like a
//! successful extraction of a blank page. That is the single most confusing
//! failure mode in this problem, so a page that draws an image and shows no
//! glyphs is labelled, counted, and — if it is the whole of what was asked for
//! — raised as an error that names OCR as the missing step.

use std::collections::BTreeMap;
use std::path::Path;

use lopdf::{Document, LoadOptions, Object, ObjectId};

use crate::budget::Deadline;
use crate::glyphs::{PageScan, ScanError, Scanner};
use crate::options::Limits;

/// Metadata strings are truncated here. A crafted `/Title` of ten megabytes is
/// not a title.
const MAX_METADATA_CHARS: usize = 2_000;

/// A PDF header must appear within this many bytes of the start. The
/// specification says byte zero; readers in the wild tolerate a preamble, and
/// so does this, up to a point.
const HEADER_SEARCH_BYTES: usize = 1_024;

#[derive(Debug)]
pub enum OpenError {
    TooLarge {
        bytes: u64,
        limit: u64,
    },
    Unreadable(String),
    NotAPdf,
    /// Encrypted with something other than an empty user password. Reading it
    /// would need a password, which this plugin deliberately does not accept.
    Encrypted,
    Damaged(String),
    NoPages,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => write!(
                formatter,
                "the file is {bytes} bytes, over the {limit}-byte ceiling. Raise \
                 --max-file-bytes in [[plugin]].args if this node should handle files this large."
            ),
            Self::Unreadable(reason) => write!(formatter, "the file could not be read: {reason}"),
            Self::NotAPdf => write!(
                formatter,
                "this file does not start with a %PDF- header, so it is not a PDF"
            ),
            Self::Encrypted => write!(
                formatter,
                "this PDF is encrypted and needs a password. pdf-extract does not accept \
                 passwords — decrypt the file first and point the plugin at the result."
            ),
            Self::Damaged(reason) => write!(
                formatter,
                "this PDF could not be parsed: {reason}. The file is damaged, or uses a \
                 construction this plugin does not read."
            ),
            Self::NoPages => write!(
                formatter,
                "this PDF declares no pages, so there is nothing to extract"
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// What one page turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// Text a reader can see.
    Text,
    /// Text, but all of it invisible over at least one image: what an OCR tool
    /// leaves on a scan. The text is real and is returned.
    OcrLayer,
    /// Images and no text at all. Extraction cannot produce anything here and
    /// says so instead of returning an empty string.
    ImageOnly,
    /// Neither text nor images.
    Empty,
}

impl PageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::OcrLayer => "ocr_layer",
            Self::ImageOnly => "image_only",
            Self::Empty => "empty",
        }
    }

    pub fn has_text(self) -> bool {
        matches!(self, Self::Text | Self::OcrLayer)
    }
}

pub fn classify(scan: &PageScan) -> PageKind {
    if scan.characters == 0 {
        if scan.images > 0 {
            PageKind::ImageOnly
        } else {
            PageKind::Empty
        }
    } else if scan.looks_like_ocr_layer() && scan.images > 0 {
        PageKind::OcrLayer
    } else {
        PageKind::Text
    }
}

/// The document information dictionary, plus what the file itself says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentInfo {
    pub pdf_version: String,
    pub pages: u32,
    pub encrypted: bool,
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
}

pub struct Pdf {
    document: Document,
    pages: BTreeMap<u32, ObjectId>,
    file_bytes: u64,
}

/// Hand-written so a `{:?}` in a log line or a panic message reports the shape
/// of the document and never any of its content.
impl std::fmt::Debug for Pdf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pdf")
            .field("pages", &self.pages.len())
            .field("file_bytes", &self.file_bytes)
            .finish()
    }
}

impl Pdf {
    /// Read and parse a file that has already been proven to be inside a
    /// configured root.
    pub fn open(path: &Path, limits: &Limits) -> Result<Self, OpenError> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| OpenError::Unreadable(error.kind().to_string()))?;
        let file_bytes = metadata.len();
        if file_bytes > limits.max_file_bytes {
            return Err(OpenError::TooLarge {
                bytes: file_bytes,
                limit: limits.max_file_bytes,
            });
        }

        let bytes =
            std::fs::read(path).map_err(|error| OpenError::Unreadable(error.kind().to_string()))?;
        if !looks_like_pdf(&bytes) {
            return Err(OpenError::NotAPdf);
        }

        let document = Document::load_mem_with_options(
            &bytes,
            LoadOptions {
                max_decompressed_size: Some(limits.max_decompressed_usize()),
                ..LoadOptions::default()
            },
        )
        .map_err(|error| OpenError::Damaged(error.to_string()))?;

        // `lopdf` tries the empty user password on its own. If the document is
        // still encrypted afterwards, it needs a real one.
        if document.is_encrypted() && !document.was_encrypted() {
            return Err(OpenError::Encrypted);
        }

        let pages = document.get_pages();
        if pages.is_empty() {
            return Err(OpenError::NoPages);
        }

        Ok(Self {
            document,
            pages,
            file_bytes,
        })
    }

    pub fn page_count(&self) -> u32 {
        self.pages.len() as u32
    }

    pub fn file_bytes(&self) -> u64 {
        self.file_bytes
    }

    pub fn info(&self) -> DocumentInfo {
        let information = self
            .document
            .trailer
            .get(b"Info")
            .and_then(|object| self.document.dereference(object).map(|(_, object)| object))
            .and_then(Object::as_dict)
            .ok();
        let entry = |key: &[u8]| -> Option<String> {
            let dictionary = information?;
            let object = dictionary.get_deref(key, &self.document).ok()?;
            let text = lopdf::decode_text_string(object).ok()?;
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(text.chars().take(MAX_METADATA_CHARS).collect())
        };

        DocumentInfo {
            pdf_version: self.document.version.clone(),
            pages: self.page_count(),
            encrypted: self.document.was_encrypted(),
            title: entry(b"Title"),
            author: entry(b"Author"),
            subject: entry(b"Subject"),
            keywords: entry(b"Keywords"),
            creator: entry(b"Creator"),
            producer: entry(b"Producer"),
            created: entry(b"CreationDate").map(|raw| normalize_date(&raw)),
            modified: entry(b"ModDate").map(|raw| normalize_date(&raw)),
        }
    }

    /// Walk one page's content stream.
    pub fn scan(
        &self,
        page_number: u32,
        deadline: Deadline,
        limits: &Limits,
    ) -> Result<PageScan, ScanError> {
        let Some(page_id) = self.pages.get(&page_number).copied() else {
            return Err(ScanError::Content(format!(
                "page {page_number} is not in this document"
            )));
        };
        Scanner::new(&self.document, deadline, limits.max_decompressed_usize()).scan_page(page_id)
    }
}

/// A PDF starts `%PDF-`. Checking this before parsing turns "somebody pointed
/// the tool at a Word document" into one clear sentence rather than a parser
/// error nobody can act on.
fn looks_like_pdf(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(HEADER_SEARCH_BYTES)];
    window.windows(5).any(|candidate| candidate == b"%PDF-")
}

/// Render a PDF date string as ISO-8601 when it is well formed, and hand back
/// whatever was there when it is not.
///
/// PDF dates look like `D:20241102153000+01'00'`. A model reading `2024-11-02`
/// knows what it has; a model reading `D:20241102153000` has to guess.
pub fn normalize_date(raw: &str) -> String {
    let digits: String = raw
        .trim()
        .trim_start_matches("D:")
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if digits.len() < 8 {
        return raw.trim().to_string();
    }

    let part = |start: usize, length: usize| -> Option<&str> { digits.get(start..start + length) };
    let mut rendered = format!(
        "{}-{}-{}",
        part(0, 4).unwrap_or("0000"),
        part(4, 2).unwrap_or("01"),
        part(6, 2).unwrap_or("01")
    );
    if digits.len() >= 14 {
        rendered.push_str(&format!(
            "T{}:{}:{}",
            part(8, 2).unwrap_or("00"),
            part(10, 2).unwrap_or("00"),
            part(12, 2).unwrap_or("00")
        ));
    } else if digits.len() >= 12 {
        rendered.push_str(&format!(
            "T{}:{}",
            part(8, 2).unwrap_or("00"),
            part(10, 2).unwrap_or("00")
        ));
    }

    // The offset, if the producer wrote one: `+01'00'`, `-0500`, or `Z`.
    let tail = raw.trim().trim_start_matches("D:")[digits.len()..].trim();
    match tail.chars().next() {
        Some('Z') => rendered.push('Z'),
        Some(sign @ ('+' | '-')) => {
            let offset: String = tail[1..]
                .chars()
                .filter(|character| character.is_ascii_digit())
                .take(4)
                .collect();
            if offset.len() == 4 {
                rendered.push(sign);
                rendered.push_str(&offset[..2]);
                rendered.push(':');
                rendered.push_str(&offset[2..]);
            }
        }
        _ => {}
    }
    rendered
}

/// Parse a page selection such as `3`, `1-5`, `2,4,7-9`, or `10-`.
///
/// Returned in ascending order with duplicates removed, because a caller who
/// wrote `5,1-3,5` wants four pages once each, in the order they appear in the
/// document.
pub fn parse_page_selection(specification: &str, page_count: u32) -> Result<Vec<u32>, String> {
    let specification = specification.trim();
    if specification.is_empty() || specification.eq_ignore_ascii_case("all") {
        return Ok((1..=page_count).collect());
    }

    let mut selected: Vec<u32> = Vec::new();
    for piece in specification.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (first, last) = match piece.split_once('-') {
            Some((start, end)) => {
                let start = parse_page_number(start.trim(), 1)?;
                let end = parse_page_number(end.trim(), page_count)?;
                (start, end)
            }
            None => {
                let only = parse_page_number(piece, 0)?;
                (only, only)
            }
        };
        if first == 0 || last == 0 {
            return Err(format!(
                "`{piece}` is not a page range. Pages are numbered from 1; write `3`, `1-5`, \
                 `2,4,7-9`, or `10-`."
            ));
        }
        if first > last {
            return Err(format!(
                "`{piece}` runs backwards: page {first} comes after page {last}."
            ));
        }
        if first > page_count {
            return Err(format!(
                "`{piece}` starts past the end of the document, which has {page_count} page(s)."
            ));
        }
        for page in first..=last.min(page_count) {
            if !selected.contains(&page) {
                selected.push(page);
            }
        }
    }

    if selected.is_empty() {
        return Err(format!(
            "`{specification}` selected no pages. Write `3`, `1-5`, `2,4,7-9`, or `10-`."
        ));
    }
    selected.sort_unstable();
    Ok(selected)
}

fn parse_page_number(raw: &str, when_empty: u32) -> Result<u32, String> {
    if raw.is_empty() {
        return Ok(when_empty);
    }
    raw.parse::<u32>()
        .map_err(|_| format!("`{raw}` is not a page number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Limits, MIN_MAX_FILE_BYTES};
    use crate::testsupport::{TempTree, TestPage, TestPdf, simple_pdf};
    use std::time::Duration;

    fn limits() -> Limits {
        Limits {
            max_file_bytes: 32 * 1024 * 1024,
            max_pages: 200,
            max_chars: 200_000,
            timeout: Duration::from_secs(30),
            max_decompressed_bytes: 128 * 1024 * 1024,
        }
    }

    fn open(tree: &TempTree, name: &str, bytes: &[u8]) -> Result<Pdf, OpenError> {
        let path = tree.write_bytes(name, bytes);
        Pdf::open(&path, &limits())
    }

    #[test]
    fn a_document_reports_its_page_count_and_information_dictionary() {
        let tree = TempTree::new("info");
        let bytes = TestPdf::new()
            .info("Title", "Quarterly report")
            .info("Author", "Accounts")
            .info("CreationDate", "D:20241102153000+01'00'")
            .page(TestPage::letter().text(72.0, 700.0, 10.0, "one"))
            .page(TestPage::letter().text(72.0, 700.0, 10.0, "two"))
            .build();

        let pdf = open(&tree, "report.pdf", &bytes).expect("opens");
        let info = pdf.info();

        assert_eq!(info.pages, 2);
        assert_eq!(info.title.as_deref(), Some("Quarterly report"));
        assert_eq!(info.author.as_deref(), Some("Accounts"));
        assert_eq!(info.created.as_deref(), Some("2024-11-02T15:30:00+01:00"));
        assert!(!info.encrypted);
        assert!(info.pdf_version.starts_with('1'), "{info:?}");
    }

    #[test]
    fn a_document_with_no_information_dictionary_reports_none_rather_than_empty_strings() {
        let tree = TempTree::new("no-info");
        let pdf = open(&tree, "bare.pdf", &simple_pdf("hello")).expect("opens");

        let info = pdf.info();
        assert_eq!(info.title, None);
        assert_eq!(info.author, None);
        assert_eq!(info.pages, 1);
    }

    #[test]
    fn a_file_over_the_size_ceiling_is_refused_without_being_parsed() {
        let tree = TempTree::new("too-big");
        let path = tree.write_bytes("big.pdf", &simple_pdf("hello"));
        let mut small = limits();
        small.max_file_bytes = MIN_MAX_FILE_BYTES;

        let error = Pdf::open(&path, &small).expect_err("over the ceiling");

        assert!(matches!(error, OpenError::TooLarge { .. }), "{error}");
        assert!(error.to_string().contains("--max-file-bytes"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_pdf_says_so_rather_than_reporting_a_parser_error() {
        let tree = TempTree::new("not-a-pdf");
        let error =
            open(&tree, "notes.pdf", b"This is a plain text file.\n").expect_err("not a pdf");

        assert!(matches!(error, OpenError::NotAPdf), "{error}");
    }

    #[test]
    fn an_empty_file_is_not_a_pdf() {
        let tree = TempTree::new("empty-file");
        let error = open(&tree, "empty.pdf", b"").expect_err("empty");

        assert!(matches!(error, OpenError::NotAPdf), "{error}");
    }

    #[test]
    fn a_truncated_pdf_is_reported_as_damaged_rather_than_as_an_empty_document() {
        let tree = TempTree::new("damaged");
        let bytes = simple_pdf("hello");
        let Err(error) = open(&tree, "damaged.pdf", &bytes[..bytes.len() / 3]) else {
            panic!("a truncated file must not read as an empty one");
        };

        assert!(
            matches!(error, OpenError::Damaged(_) | OpenError::NoPages),
            "{error}"
        );
    }

    #[test]
    fn a_page_of_text_is_classified_as_text() {
        let tree = TempTree::new("classify-text");
        let pdf = open(&tree, "text.pdf", &simple_pdf("readable words")).expect("opens");

        let scan = pdf
            .scan(1, Deadline::unlimited(), &limits())
            .expect("scans");
        assert_eq!(classify(&scan), PageKind::Text);
        assert!(classify(&scan).has_text());
    }

    #[test]
    fn a_page_with_an_image_and_no_text_is_classified_image_only() {
        let tree = TempTree::new("classify-image");
        let bytes = TestPdf::new()
            .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
            .build();
        let pdf = open(&tree, "scan.pdf", &bytes).expect("opens");

        let scan = pdf
            .scan(1, Deadline::unlimited(), &limits())
            .expect("scans");
        assert_eq!(classify(&scan), PageKind::ImageOnly);
        assert!(!classify(&scan).has_text());
    }

    #[test]
    fn an_invisible_text_layer_over_an_image_is_classified_as_ocr_and_still_read() {
        let tree = TempTree::new("classify-ocr");
        let bytes = TestPdf::new()
            .page(
                TestPage::letter()
                    .image(0.0, 0.0, 612.0, 792.0)
                    .text_with_mode(72.0, 700.0, 10.0, "recognised words", 3),
            )
            .build();
        let pdf = open(&tree, "scanned.pdf", &bytes).expect("opens");

        let scan = pdf
            .scan(1, Deadline::unlimited(), &limits())
            .expect("scans");
        assert_eq!(classify(&scan), PageKind::OcrLayer);
        assert!(classify(&scan).has_text());
        assert_eq!(scan.runs[0].text, "recognised words");
    }

    #[test]
    fn a_page_with_nothing_on_it_is_classified_empty() {
        let tree = TempTree::new("classify-empty");
        let bytes = TestPdf::new().page(TestPage::letter()).build();
        let pdf = open(&tree, "blank.pdf", &bytes).expect("opens");

        let scan = pdf
            .scan(1, Deadline::unlimited(), &limits())
            .expect("scans");
        assert_eq!(classify(&scan), PageKind::Empty);
    }

    #[test]
    fn asking_for_a_page_that_is_not_there_is_an_error() {
        let tree = TempTree::new("missing-page");
        let pdf = open(&tree, "one.pdf", &simple_pdf("hello")).expect("opens");

        let error = pdf
            .scan(7, Deadline::unlimited(), &limits())
            .expect_err("no page seven");

        assert!(error.to_string().contains("page 7"), "{error}");
    }

    #[test]
    fn pdf_dates_are_rendered_as_iso_when_they_can_be_and_kept_when_they_cannot() {
        assert_eq!(
            normalize_date("D:20241102153000+01'00'"),
            "2024-11-02T15:30:00+01:00"
        );
        assert_eq!(normalize_date("D:20241102153000Z"), "2024-11-02T15:30:00Z");
        assert_eq!(normalize_date("D:20241102153000"), "2024-11-02T15:30:00");
        assert_eq!(normalize_date("D:202411021530"), "2024-11-02T15:30");
        assert_eq!(normalize_date("D:20241102"), "2024-11-02");
        assert_eq!(normalize_date("20241102"), "2024-11-02");
        // Not a date at all: hand back what the file said rather than inventing.
        assert_eq!(normalize_date("last Tuesday"), "last Tuesday");
        assert_eq!(normalize_date("D:2024"), "D:2024");
    }

    #[test]
    fn a_page_selection_accepts_the_forms_a_caller_would_write() {
        assert_eq!(parse_page_selection("3", 10).expect("single"), vec![3]);
        assert_eq!(
            parse_page_selection("1-3", 10).expect("range"),
            vec![1, 2, 3]
        );
        assert_eq!(
            parse_page_selection("2,4,7-9", 10).expect("mixed"),
            vec![2, 4, 7, 8, 9]
        );
        assert_eq!(
            parse_page_selection("8-", 10).expect("open end"),
            vec![8, 9, 10]
        );
        assert_eq!(
            parse_page_selection("-3", 10).expect("open start"),
            vec![1, 2, 3]
        );
        assert_eq!(
            parse_page_selection("", 3).expect("empty means all"),
            vec![1, 2, 3]
        );
        assert_eq!(
            parse_page_selection("all", 3).expect("all means all"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_page_selection_is_sorted_and_deduplicated() {
        assert_eq!(
            parse_page_selection("5,1-3,5,2", 10).expect("overlapping"),
            vec![1, 2, 3, 5]
        );
    }

    #[test]
    fn a_range_that_runs_past_the_end_is_clamped_and_one_that_starts_past_it_is_an_error() {
        assert_eq!(
            parse_page_selection("2-99", 3).expect("clamped"),
            vec![2, 3]
        );

        let error = parse_page_selection("9-12", 3).expect_err("starts past the end");
        assert!(error.contains("3 page(s)"), "{error}");
    }

    #[test]
    fn a_malformed_page_selection_explains_the_forms_that_work() {
        for specification in ["0", "abc", "5-2", "0-3", "2..4"] {
            let error = parse_page_selection(specification, 10)
                .expect_err(&format!("`{specification}` should be refused"));
            assert!(!error.is_empty(), "`{specification}` needs a reason");
        }
        assert!(
            parse_page_selection("abc", 10)
                .expect_err("not a number")
                .contains("not a page number")
        );
    }
}
