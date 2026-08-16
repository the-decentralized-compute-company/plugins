//! Test-only scaffolding: a throwaway directory tree, a directory-link helper,
//! and a small PDF writer.
//!
//! Hand-rolled rather than pulled from a crate so the plugin's release
//! dependency set stays as small as the thing it does — nothing here is
//! compiled into the shipped binary.
//!
//! The PDF writer matters more than it looks. Layout code that is only tested
//! against strings is testing itself; these helpers emit real PDF files, with
//! real content streams, that the same `lopdf` reader the plugin uses parses
//! back. A test can therefore place text at exact coordinates — two columns, a
//! rotated page, an image with no text at all — and assert on what the
//! extractor makes of the bytes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lopdf::{Document, Object, Stream, StringFormat, dictionary};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir that deletes itself on drop.
pub struct TempTree {
    path: PathBuf,
}

impl TempTree {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pdf-extract-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tree root as the plugin would hold it: canonical, so containment
    /// checks compare like with like.
    pub fn canonical_root(&self) -> PathBuf {
        std::fs::canonicalize(&self.path).expect("canonicalize temp tree")
    }

    /// Write a file at a `/`-separated relative path, creating parents.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        self.write_bytes(relative, contents.as_bytes())
    }

    pub fn write_bytes(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let mut target = self.path.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&target, contents).expect("write temp file");
        target
    }

    pub fn mkdir(&self, relative: &str) -> PathBuf {
        let mut target = self.path.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        std::fs::create_dir_all(&target).expect("create directory");
        target
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Best effort: a leaked temp directory is a nuisance, a panicking
        // destructor masking a real test failure is worse.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create a directory link at `link` pointing at `target`.
///
/// Unix gets a symlink. Windows tries a real symlink first — which needs
/// Developer Mode or `SeCreateSymbolicLinkPrivilege` — and falls back to a
/// directory junction, which an unprivileged user can create. Returns `Err(())`
/// when the platform allows neither, so a test can skip rather than fail on a
/// locked-down machine.
pub fn link_directory(target: &Path, link: &Path) -> Result<(), ()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|_| ())
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| ())?;
        if status.success() && link.exists() {
            Ok(())
        } else {
            Err(())
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(())
    }
}

// ---------------------------------------------------------------------------
// A very small PDF writer
// ---------------------------------------------------------------------------

/// US Letter, in points, which is what a PDF measures in.
pub const LETTER_WIDTH: f32 = 612.0;
pub const LETTER_HEIGHT: f32 = 792.0;

/// Every glyph in the test font is exactly half an em wide, so a test can
/// compute the extent of a string as `0.5 * size * len` and reason about
/// gutters and column boundaries in numbers it wrote itself.
pub const TEST_GLYPH_WIDTH: i64 = 500;

/// One page under construction.
pub struct TestPage {
    width: f32,
    height: f32,
    origin_x: f32,
    origin_y: f32,
    rotate: i64,
    content: String,
    images: u32,
}

impl TestPage {
    pub fn letter() -> Self {
        Self::sized(LETTER_WIDTH, LETTER_HEIGHT)
    }

    pub fn sized(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            origin_x: 0.0,
            origin_y: 0.0,
            rotate: 0,
            content: String::new(),
            images: 0,
        }
    }

    /// Move the bottom-left corner of the `/MediaBox` away from the origin,
    /// which real page boxes are allowed to do and extractors routinely
    /// forget.
    pub fn origin(mut self, x: f32, y: f32) -> Self {
        self.origin_x = x;
        self.origin_y = y;
        self
    }

    /// `/Rotate` on the page dictionary: the viewer turns the page clockwise by
    /// this many degrees before displaying it.
    pub fn rotated(mut self, degrees: i64) -> Self {
        self.rotate = degrees;
        self
    }

    /// Show `text` with its baseline origin at `(x, y)` in PDF user space,
    /// where `y` grows upward from the bottom of the page.
    pub fn text(self, x: f32, y: f32, size: f32, text: &str) -> Self {
        self.text_with_mode(x, y, size, text, 0)
    }

    /// The same, with an explicit text rendering mode. Mode 3 is invisible
    /// text, which is what an OCR layer over a scanned page looks like.
    pub fn text_with_mode(mut self, x: f32, y: f32, size: f32, text: &str, mode: i32) -> Self {
        self.content.push_str(&format!(
            "BT\n{mode} Tr\n/F1 {size} Tf\n{x} {y} Td\n({}) Tj\nET\n",
            escape_literal(text)
        ));
        self
    }

    /// Show several strings in one `TJ` array, with a thousandths-of-an-em
    /// adjustment after each. This is how real producers write kerned text and
    /// inter-word gaps, so it is how the extractor has to be able to read it.
    pub fn text_array(mut self, x: f32, y: f32, size: f32, parts: &[(&str, f32)]) -> Self {
        let mut array = String::new();
        for (text, adjustment) in parts {
            array.push_str(&format!("({}) {adjustment} ", escape_literal(text)));
        }
        self.content.push_str(&format!(
            "BT\n/F1 {size} Tf\n{x} {y} Td\n[{array}] TJ\nET\n"
        ));
        self
    }

    /// Several lines at one leading, placed with `TD`/`T*` the way a word
    /// processor emits a paragraph.
    pub fn paragraph(mut self, x: f32, y: f32, size: f32, leading: f32, lines: &[&str]) -> Self {
        self.content
            .push_str(&format!("BT\n/F1 {size} Tf\n{leading} TL\n{x} {y} Td\n"));
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                self.content.push_str("T*\n");
            }
            self.content
                .push_str(&format!("({}) Tj\n", escape_literal(line)));
        }
        self.content.push_str("ET\n");
        self
    }

    /// Paint the 1x1 grey image resource over a rectangle. A page with one of
    /// these and no text is what a scanner produces.
    pub fn image(mut self, x: f32, y: f32, width: f32, height: f32) -> Self {
        self.content
            .push_str(&format!("q\n{width} 0 0 {height} {x} {y} cm\n/Im1 Do\nQ\n"));
        self.images += 1;
        self
    }

    /// Raw content-stream operators, for the cases a helper would only obscure.
    pub fn raw(mut self, operators: &str) -> Self {
        self.content.push_str(operators);
        self.content.push('\n');
        self
    }
}

/// A document under construction.
pub struct TestPdf {
    pages: Vec<TestPage>,
    info: Vec<(&'static str, String)>,
}

impl Default for TestPdf {
    fn default() -> Self {
        Self::new()
    }
}

impl TestPdf {
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            info: Vec::new(),
        }
    }

    pub fn page(mut self, page: TestPage) -> Self {
        self.pages.push(page);
        self
    }

    /// An entry in the document information dictionary, such as `Title`.
    pub fn info(mut self, key: &'static str, value: &str) -> Self {
        self.info.push((key, value.to_string()));
        self
    }

    /// Serialize to PDF bytes.
    pub fn build(self) -> Vec<u8> {
        let mut document = Document::with_version("1.7");

        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
            "FirstChar" => 32_i64,
            "LastChar" => 255_i64,
            "Widths" => Object::Array(
                (32..=255).map(|_| Object::Integer(TEST_GLYPH_WIDTH)).collect(),
            ),
        });
        // A single pixel of mid-grey. Small enough to be uninteresting, real
        // enough that a viewer would paint it.
        let image_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1_i64,
                "Height" => 1_i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i64,
            },
            vec![0x80],
        ));

        let pages_id = document.new_object_id();
        let mut page_ids = Vec::with_capacity(self.pages.len());
        for page in &self.pages {
            let content_id = document.add_object(Stream::new(
                dictionary! {},
                page.content.clone().into_bytes(),
            ));
            let mut resources = dictionary! {
                "Font" => dictionary! { "F1" => font_id },
            };
            if page.images > 0 {
                resources.set("XObject", dictionary! { "Im1" => image_id });
            }
            let resources_id = document.add_object(resources);

            let mut dictionary = dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => Object::Array(vec![
                    Object::Real(page.origin_x),
                    Object::Real(page.origin_y),
                    Object::Real(page.origin_x + page.width),
                    Object::Real(page.origin_y + page.height),
                ]),
            };
            if page.rotate != 0 {
                dictionary.set("Rotate", page.rotate);
            }
            page_ids.push(document.add_object(dictionary));
        }

        let count = page_ids.len() as i64;
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => count,
                "Kids" => Object::Array(page_ids.into_iter().map(Object::Reference).collect()),
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        if !self.info.is_empty() {
            let mut info = lopdf::Dictionary::new();
            for (key, value) in &self.info {
                info.set(
                    *key,
                    Object::String(value.clone().into_bytes(), StringFormat::Literal),
                );
            }
            let info_id = document.add_object(info);
            document.trailer.set("Info", info_id);
        }

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("write test pdf");
        bytes
    }
}

/// Escape a string for a PDF literal string operand.
fn escape_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '(' => escaped.push_str("\\("),
            ')' => escaped.push_str("\\)"),
            // WinAnsiEncoding is a one-byte encoding; anything outside Latin-1
            // is written as an octal escape of its Latin-1 byte, and anything
            // beyond that is not something these tests need.
            character if (character as u32) > 127 && (character as u32) < 256 => {
                escaped.push_str(&format!("\\{:03o}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// A one-page PDF holding a single line of text, for tests that only need a
/// file that parses.
pub fn simple_pdf(text: &str) -> Vec<u8> {
    TestPdf::new()
        .page(TestPage::letter().text(72.0, 700.0, 12.0, text))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builder_emits_a_document_lopdf_can_read_back() {
        let bytes = TestPdf::new()
            .info("Title", "Quarterly report")
            .page(TestPage::letter().text(72.0, 700.0, 12.0, "Hello"))
            .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
            .build();

        let document = Document::load_mem(&bytes).expect("the writer produces a valid PDF");
        assert_eq!(document.get_pages().len(), 2);
    }

    #[test]
    fn literal_strings_are_escaped_so_the_content_stream_stays_parseable() {
        assert_eq!(escape_literal("a(b)c\\d"), "a\\(b\\)c\\\\d");
        assert_eq!(escape_literal("caf\u{e9}"), "caf\\351");
    }
}
