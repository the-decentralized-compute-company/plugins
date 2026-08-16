//! Walking a page's content stream into positioned text runs.
//!
//! A PDF is a page-description format, not a document format. There is no
//! paragraph, no column, no reading order — only instructions that put glyphs
//! at coordinates. Anything that reads a content stream in operator order and
//! concatenates the strings it finds will interleave the columns of a
//! two-column page and produce text no model can follow. That is the failure
//! this module exists to avoid: it keeps the *position* of every string, and
//! leaves the question of what order they should be read in to
//! [`crate::layout`], which can only answer it because the positions survived.
//!
//! What is tracked, from PDF 32000-1 chapter 9:
//!
//! * the graphics state stack (`q`, `Q`) and the current transformation matrix
//!   (`cm`), so text inside a scaled or translated form lands where it is drawn
//! * the text matrix and line matrix (`BT`, `Tm`, `Td`, `TD`, `T*`, `'`, `"`)
//! * font size, character spacing, word spacing, horizontal scaling, leading,
//!   rise, and render mode (`Tf`, `Tc`, `Tw`, `Tz`, `TL`, `Ts`, `Tr`)
//! * glyph widths, from `/Widths` for simple fonts and `/W` and `/DW` for
//!   composite ones, so the advance after a string is a real measurement rather
//!   than a guess
//! * form XObjects (`Do`), recursively and with a depth cap, because plenty of
//!   producers put the whole page inside one
//! * images, both `Do` on an image XObject and inline `BI`, which is how a
//!   page with no text at all is distinguished from a scan
//!
//! Text drawn in render mode 3 or 7 is invisible. It is still collected: an
//! invisible text layer over a page image is exactly what an OCR tool leaves
//! behind, and it is the text the caller wants. It is counted separately so
//! [`crate::pdf`] can say the page looks OCRed rather than typeset.

use std::collections::HashMap;

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Encoding, Object, ObjectId};

use crate::budget::Deadline;

/// Glyph advance assumed for a font that declares no width for a code. Half an
/// em is close to the average for Latin text in a proportional face; it is
/// wrong for a monospace font, which is why it is only ever a fallback.
const FALLBACK_GLYPH_WIDTH: f32 = 500.0;

/// Depth limit on form XObject recursion. A form that draws itself, directly or
/// through a chain, would otherwise not terminate; the visited set catches the
/// direct cycle and this catches the rest.
const MAX_FORM_DEPTH: u32 = 12;

/// Runs kept per page. Reached only by a page with hundreds of thousands of
/// separately positioned strings, which is a generated document or an attack.
const MAX_RUNS_PER_PAGE: usize = 200_000;

/// Operators between deadline checks. Small enough that a hostile stream is
/// abandoned promptly, large enough that the check is not the cost.
const OPERATORS_PER_DEADLINE_CHECK: usize = 512;

/// A 2x3 affine transform in PDF's row-vector convention:
/// `(x, y) -> (a*x + c*y + e, b*x + d*y + f)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub fn translation(x: f32, y: f32) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, x, y)
    }

    /// `self` applied first, then `other`.
    pub fn then(&self, other: &Self) -> Self {
        Self {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// How much this transform scales a horizontal unit vector.
    pub fn horizontal_scale(&self) -> f32 {
        (self.a * self.a + self.b * self.b).sqrt()
    }

    /// How much this transform scales a vertical unit vector.
    pub fn vertical_scale(&self) -> f32 {
        (self.c * self.c + self.d * self.d).sqrt()
    }
}

/// One string, decoded, with the box it occupies on the normalized page.
///
/// `y` is the baseline and grows upward; `x` grows rightward; both are in
/// points after the page's `/MediaBox` origin and `/Rotate` have been folded
/// in, so a caller never has to think about either.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    /// Effective font size in page units — the run's height for layout
    /// purposes, not the exact glyph bounding box.
    pub height: f32,
    pub text: String,
    /// Drawn in render mode 3 or 7, so a reader sees nothing there.
    pub invisible: bool,
}

impl Run {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }
}

/// Everything one page's content stream said, before any reading order is
/// imposed on it.
#[derive(Clone, Debug, Default)]
pub struct PageScan {
    pub runs: Vec<Run>,
    /// Page size after `/Rotate`, in points.
    pub width: f32,
    pub height: f32,
    /// Image XObjects painted plus inline images.
    pub images: u32,
    /// Non-whitespace characters found, over all runs.
    pub characters: usize,
    /// Non-whitespace characters that a reader would actually see.
    pub visible_characters: usize,
    /// A cap stopped the walk early, so the page holds more than this.
    pub truncated: bool,
}

impl PageScan {
    /// Every character on the page is invisible: the signature of a scanned
    /// page carrying an OCR text layer.
    pub fn looks_like_ocr_layer(&self) -> bool {
        self.characters > 0 && self.visible_characters == 0
    }
}

#[derive(Debug)]
pub enum ScanError {
    /// The content stream could not be decoded at all.
    Content(String),
    /// The wall-clock budget ran out.
    TimedOut,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Content(reason) => {
                write!(formatter, "content stream could not be read: {reason}")
            }
            Self::TimedOut => write!(formatter, "ran out of time"),
        }
    }
}

/// Glyph widths for one font, in thousandths of an em.
#[derive(Clone, Debug, Default)]
struct Widths {
    first_char: u32,
    simple: Vec<f32>,
    composite: HashMap<u32, f32>,
    default_width: f32,
}

impl Widths {
    fn width_of(&self, code: u32, composite: bool) -> f32 {
        if composite {
            return *self
                .composite
                .get(&code)
                .unwrap_or(&self.default_width.max(1.0));
        }
        code.checked_sub(self.first_char)
            .and_then(|index| self.simple.get(index as usize))
            .copied()
            .filter(|width| *width > 0.0)
            .unwrap_or(if self.default_width > 0.0 {
                self.default_width
            } else {
                FALLBACK_GLYPH_WIDTH
            })
    }
}

struct Font<'a> {
    encoding: Encoding<'a>,
    /// A Type0 font, whose codes are consumed two bytes at a time.
    composite: bool,
    widths: Widths,
}

impl Font<'_> {
    /// Split a show-string into character codes.
    ///
    /// Composite fonts are assumed to use a two-byte CMap, which `Identity-H`
    /// and `Identity-V` — between them the overwhelming majority of composite
    /// fonts in the wild — do. A one-byte or mixed-width CMap yields the wrong
    /// *widths* here; the decoded *text* is unaffected, because that goes
    /// through the font's real CMap in `lopdf`.
    fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        if self.composite {
            bytes
                .chunks(2)
                .map(|pair| match pair {
                    [high, low] => (u32::from(*high) << 8) | u32::from(*low),
                    [single] => u32::from(*single),
                    _ => 0,
                })
                .collect()
        } else {
            bytes.iter().map(|byte| u32::from(*byte)).collect()
        }
    }
}

/// The text-object state of PDF 32000-1 section 9.3.
#[derive(Clone, Copy, Debug)]
struct TextState {
    font_size: f32,
    character_spacing: f32,
    word_spacing: f32,
    horizontal_scale: f32,
    leading: f32,
    rise: f32,
    render_mode: i32,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            font_size: 0.0,
            character_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
        }
    }
}

/// Resource dictionaries in lookup order: a form's own resources first, then
/// whatever it inherited.
type Scope<'a> = Vec<&'a Dictionary>;

pub struct Scanner<'a> {
    document: &'a Document,
    deadline: Deadline,
    max_decompressed_bytes: usize,
    scan: PageScan,
    operators_seen: usize,
    visited_forms: Vec<ObjectId>,
}

impl<'a> Scanner<'a> {
    pub fn new(document: &'a Document, deadline: Deadline, max_decompressed_bytes: usize) -> Self {
        Self {
            document,
            deadline,
            max_decompressed_bytes,
            scan: PageScan::default(),
            operators_seen: 0,
            visited_forms: Vec::new(),
        }
    }

    /// Walk one page and return everything it draws.
    ///
    /// A page whose content stream will not decode is an error rather than an
    /// empty page: "this file is damaged" and "this page is blank" are
    /// different answers and a caller has to be able to tell them apart.
    pub fn scan_page(mut self, page_id: ObjectId) -> Result<PageScan, ScanError> {
        // Between pages, before anything is read: a budget that is already
        // spent must not buy one more page's worth of parsing.
        if self.deadline.expired_now() {
            return Err(ScanError::TimedOut);
        }

        let page = self
            .document
            .get_dictionary(page_id)
            .map_err(|error| ScanError::Content(error.to_string()))?;

        let (media_box, rotation) = page_geometry(self.document, page);
        let (base, width, height) = normalizing_transform(media_box, rotation);
        self.scan.width = width;
        self.scan.height = height;

        let content = self
            .document
            .get_page_content_with_limit(page_id, self.max_decompressed_bytes)
            .map_err(|error| ScanError::Content(error.to_string()))?;

        let mut scope: Scope<'a> = Vec::new();
        let (direct, inherited) = self
            .document
            .get_page_resources(page_id)
            .unwrap_or((None, Vec::new()));
        if let Some(direct) = direct {
            scope.push(direct);
        }
        for id in inherited {
            if let Ok(dictionary) = self.document.get_dictionary(id) {
                scope.push(dictionary);
            }
        }

        self.run_stream(&content, &scope, base, 0)?;
        Ok(self.scan)
    }

    fn run_stream(
        &mut self,
        content: &[u8],
        scope: &Scope<'a>,
        base_ctm: Matrix,
        depth: u32,
    ) -> Result<(), ScanError> {
        if self.deadline.expired_now() {
            return Err(ScanError::TimedOut);
        }
        let operations = Content::decode(content)
            .map_err(|error| ScanError::Content(error.to_string()))?
            .operations;

        let fonts = self.load_fonts(scope);

        let mut ctm_stack: Vec<Matrix> = Vec::new();
        let mut ctm = base_ctm;
        let mut text_stack: Vec<TextState> = Vec::new();
        let mut text = TextState::default();
        let mut matrix = Matrix::IDENTITY;
        let mut line_matrix = Matrix::IDENTITY;
        let mut font: Option<&Font<'a>> = None;

        for operation in &operations {
            self.operators_seen += 1;
            if self
                .operators_seen
                .is_multiple_of(OPERATORS_PER_DEADLINE_CHECK)
                && self.deadline.expired_now()
            {
                return Err(ScanError::TimedOut);
            }
            if self.scan.truncated {
                return Ok(());
            }

            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => {
                    ctm_stack.push(ctm);
                    text_stack.push(text);
                }
                "Q" => {
                    if let Some(previous) = ctm_stack.pop() {
                        ctm = previous;
                    }
                    if let Some(previous) = text_stack.pop() {
                        text = previous;
                    }
                }
                "cm" => {
                    if let Some(applied) = matrix_operand(operands) {
                        ctm = applied.then(&ctm);
                    }
                }

                "BT" => {
                    matrix = Matrix::IDENTITY;
                    line_matrix = Matrix::IDENTITY;
                }
                "ET" => {}

                "Tf" => {
                    font = operands
                        .first()
                        .and_then(|operand| operand.as_name().ok())
                        .and_then(|name| fonts.get(name));
                    text.font_size = number(operands.get(1)).unwrap_or(text.font_size);
                }
                "Tc" => text.character_spacing = number(operands.first()).unwrap_or(0.0),
                "Tw" => text.word_spacing = number(operands.first()).unwrap_or(0.0),
                "Tz" => {
                    text.horizontal_scale = number(operands.first()).unwrap_or(100.0) / 100.0;
                }
                "TL" => text.leading = number(operands.first()).unwrap_or(0.0),
                "Ts" => text.rise = number(operands.first()).unwrap_or(0.0),
                "Tr" => {
                    text.render_mode = number(operands.first()).unwrap_or(0.0) as i32;
                }

                "Td" => {
                    let x = number(operands.first()).unwrap_or(0.0);
                    let y = number(operands.get(1)).unwrap_or(0.0);
                    line_matrix = Matrix::translation(x, y).then(&line_matrix);
                    matrix = line_matrix;
                }
                "TD" => {
                    let x = number(operands.first()).unwrap_or(0.0);
                    let y = number(operands.get(1)).unwrap_or(0.0);
                    text.leading = -y;
                    line_matrix = Matrix::translation(x, y).then(&line_matrix);
                    matrix = line_matrix;
                }
                "Tm" => {
                    if let Some(applied) = matrix_operand(operands) {
                        line_matrix = applied;
                        matrix = applied;
                    }
                }
                "T*" => {
                    line_matrix = Matrix::translation(0.0, -text.leading).then(&line_matrix);
                    matrix = line_matrix;
                }

                "Tj" => {
                    if let Some(bytes) = operands.first().and_then(|operand| operand.as_str().ok())
                    {
                        self.show(bytes, font, &text, &mut matrix, &ctm);
                    }
                }
                "'" => {
                    line_matrix = Matrix::translation(0.0, -text.leading).then(&line_matrix);
                    matrix = line_matrix;
                    if let Some(bytes) = operands.first().and_then(|operand| operand.as_str().ok())
                    {
                        self.show(bytes, font, &text, &mut matrix, &ctm);
                    }
                }
                "\"" => {
                    text.word_spacing = number(operands.first()).unwrap_or(text.word_spacing);
                    text.character_spacing =
                        number(operands.get(1)).unwrap_or(text.character_spacing);
                    line_matrix = Matrix::translation(0.0, -text.leading).then(&line_matrix);
                    matrix = line_matrix;
                    if let Some(bytes) = operands.get(2).and_then(|operand| operand.as_str().ok()) {
                        self.show(bytes, font, &text, &mut matrix, &ctm);
                    }
                }
                "TJ" => {
                    let Some(elements) =
                        operands.first().and_then(|operand| operand.as_array().ok())
                    else {
                        continue;
                    };
                    for element in elements {
                        match element {
                            Object::String(bytes, _) => {
                                self.show(bytes, font, &text, &mut matrix, &ctm);
                            }
                            // A positioning adjustment, in thousandths of an em,
                            // subtracted from the pen position. This is how
                            // producers write the space between two words, so it
                            // has to move the pen exactly, not approximately.
                            other => {
                                if let Some(adjustment) = number(Some(other)) {
                                    let shift = -adjustment / 1000.0
                                        * text.font_size
                                        * text.horizontal_scale;
                                    matrix = Matrix::translation(shift, 0.0).then(&matrix);
                                }
                            }
                        }
                    }
                }

                "Do" => {
                    if let Some(name) = operands.first().and_then(|operand| operand.as_name().ok())
                    {
                        self.draw_xobject(name, scope, ctm, depth)?;
                    }
                }
                // An inline image. `lopdf` gives the whole `BI … ID … EI` block
                // back as one operation, so it counts once and its binary data
                // never reaches the operand parser.
                "BI" => self.scan.images += 1,

                _ => {}
            }
        }

        Ok(())
    }

    /// Emit one run for a shown string and advance the text matrix past it.
    fn show(
        &mut self,
        bytes: &[u8],
        font: Option<&Font<'a>>,
        text: &TextState,
        matrix: &mut Matrix,
        ctm: &Matrix,
    ) {
        let Some(font) = font else {
            // No `Tf` yet, or one naming a resource that is not there. Anything
            // decoded now would be guesswork about the encoding, and guessed
            // text is worse than absent text.
            return;
        };

        let codes = font.codes(bytes);
        let mut advance = 0.0;
        for code in &codes {
            let glyph = font.widths.width_of(*code, font.composite) / 1000.0 * text.font_size;
            let word = if !font.composite && *code == 32 {
                text.word_spacing
            } else {
                0.0
            };
            advance += (glyph + text.character_spacing + word) * text.horizontal_scale;
        }

        let decoded = font.encoding.bytes_to_string(bytes).unwrap_or_default();
        let invisible = matches!(text.render_mode, 3 | 7);

        if !decoded.is_empty() {
            // trm = [size*scale, 0, 0, size, 0, rise] x Tm x CTM
            let parameters = Matrix::new(
                text.font_size * text.horizontal_scale,
                0.0,
                0.0,
                text.font_size,
                0.0,
                text.rise,
            );
            let placement = matrix.then(ctm);
            let rendered = parameters.then(&placement);
            let non_whitespace = decoded.chars().filter(|c| !c.is_whitespace()).count();

            if self.scan.runs.len() >= MAX_RUNS_PER_PAGE {
                self.scan.truncated = true;
            } else {
                self.scan.characters += non_whitespace;
                if !invisible {
                    self.scan.visible_characters += non_whitespace;
                }
                let (x, y) = rendered.apply(0.0, 0.0);
                self.scan.runs.push(Run {
                    x,
                    y,
                    width: advance * placement.horizontal_scale(),
                    height: text.font_size * placement.vertical_scale(),
                    text: decoded,
                    invisible,
                });
            }
        }

        *matrix = Matrix::translation(advance, 0.0).then(matrix);
    }

    /// `Do`: either paint an image, or step into a form and keep walking.
    fn draw_xobject(
        &mut self,
        name: &[u8],
        scope: &Scope<'a>,
        ctm: Matrix,
        depth: u32,
    ) -> Result<(), ScanError> {
        let Some((id, stream)) = self.lookup_xobject(name, scope) else {
            return Ok(());
        };
        let subtype = stream
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .unwrap_or(b"");

        if subtype == b"Image" {
            self.scan.images += 1;
            return Ok(());
        }
        if subtype != b"Form" {
            return Ok(());
        }
        if depth >= MAX_FORM_DEPTH || self.visited_forms.contains(&id) {
            return Ok(());
        }

        let Ok(content) = stream.decompressed_content() else {
            return Ok(());
        };
        if content.len() > self.max_decompressed_bytes {
            return Ok(());
        }

        // A form has its own matrix and, usually, its own resources; anything
        // it does not declare it inherits from the scope that drew it.
        let form_matrix = stream
            .dict
            .get(b"Matrix")
            .and_then(Object::as_array)
            .ok()
            .and_then(|array| matrix_operand(array))
            .unwrap_or(Matrix::IDENTITY);
        let mut inner: Scope<'a> = Vec::new();
        if let Ok(resources) = stream.dict.get_deref(b"Resources", self.document)
            && let Ok(dictionary) = resources.as_dict()
        {
            inner.push(dictionary);
        }
        inner.extend(scope.iter().copied());

        self.visited_forms.push(id);
        let result = self.run_stream(&content, &inner, form_matrix.then(&ctm), depth + 1);
        self.visited_forms.pop();
        result
    }

    fn lookup_xobject(
        &self,
        name: &[u8],
        scope: &Scope<'a>,
    ) -> Option<(ObjectId, &'a lopdf::Stream)> {
        for resources in scope {
            let Ok(xobjects) = resources
                .get_deref(b"XObject", self.document)
                .and_then(Object::as_dict)
            else {
                continue;
            };
            let Ok(entry) = xobjects.get(name) else {
                continue;
            };
            // The object id is what makes cycle detection possible, so an
            // XObject written inline rather than as a reference is skipped
            // rather than walked.
            let Ok(id) = entry.as_reference() else {
                continue;
            };
            if let Ok(stream) = self.document.get_object(id).and_then(Object::as_stream) {
                return Some((id, stream));
            }
        }
        None
    }

    /// Resolve every font named in this resource scope, once per stream.
    fn load_fonts(&self, scope: &Scope<'a>) -> HashMap<Vec<u8>, Font<'a>> {
        let document = self.document;
        let mut fonts: HashMap<Vec<u8>, Font<'a>> = HashMap::new();
        for resources in scope {
            let Ok(font_dictionary) = resources
                .get_deref(b"Font", document)
                .and_then(Object::as_dict)
            else {
                continue;
            };
            for (name, entry) in font_dictionary.iter() {
                if fonts.contains_key(name) {
                    // The innermost scope wins, matching how a form's own
                    // resources shadow the page's.
                    continue;
                }
                let Ok((_, object)) = document.dereference(entry) else {
                    continue;
                };
                let Ok(font) = object.as_dict() else {
                    continue;
                };
                let Ok(encoding) = font.get_font_encoding(document) else {
                    continue;
                };
                let composite = font
                    .get(b"Subtype")
                    .and_then(Object::as_name)
                    .map(|subtype| subtype == b"Type0")
                    .unwrap_or(false);
                fonts.insert(
                    name.clone(),
                    Font {
                        encoding,
                        composite,
                        widths: read_widths(document, font, composite),
                    },
                );
            }
        }
        fonts
    }
}

/// `/Widths` for a simple font, `/W` and `/DW` on the descendant for a
/// composite one.
fn read_widths(document: &Document, font: &Dictionary, composite: bool) -> Widths {
    if !composite {
        let first_char = font
            .get_deref(b"FirstChar", document)
            .ok()
            .and_then(|object| object.as_i64().ok())
            .unwrap_or(0)
            .max(0) as u32;
        let simple = font
            .get_deref(b"Widths", document)
            .and_then(Object::as_array)
            .map(|array| {
                array
                    .iter()
                    .map(|entry| number(Some(entry)).unwrap_or(0.0))
                    .collect()
            })
            .unwrap_or_default();
        return Widths {
            first_char,
            simple,
            composite: HashMap::new(),
            default_width: 0.0,
        };
    }

    let mut widths = Widths {
        default_width: 1000.0,
        ..Widths::default()
    };
    let Some(descendant) = font
        .get_deref(b"DescendantFonts", document)
        .and_then(Object::as_array)
        .ok()
        .and_then(|array| array.first())
        .and_then(|entry| document.dereference(entry).ok())
        .and_then(|(_, object)| object.as_dict().ok())
    else {
        return widths;
    };
    if let Some(default_width) = descendant
        .get_deref(b"DW", document)
        .ok()
        .and_then(|object| number(Some(object)))
    {
        widths.default_width = default_width;
    }
    let Ok(entries) = descendant
        .get_deref(b"W", document)
        .and_then(Object::as_array)
    else {
        return widths;
    };

    // `/W` alternates between two shapes: `c [w …]` gives consecutive widths
    // from `c`, and `cFirst cLast w` gives one width for a whole range.
    let mut index = 0;
    while index < entries.len() {
        let Some(first) = number(entries.get(index)) else {
            break;
        };
        let first = first.max(0.0) as u32;
        match entries.get(index + 1) {
            Some(Object::Array(list)) => {
                for (offset, entry) in list.iter().enumerate() {
                    if let Some(width) = number(Some(entry)) {
                        widths.composite.insert(first + offset as u32, width);
                    }
                }
                index += 2;
            }
            Some(_) => {
                let Some(last) = number(entries.get(index + 1)) else {
                    break;
                };
                let Some(width) = number(entries.get(index + 2)) else {
                    break;
                };
                let last = last.max(0.0) as u32;
                // Bounded so a crafted `/W` cannot ask for a map of four
                // billion entries.
                for code in first..=last.min(first.saturating_add(65_535)) {
                    widths.composite.insert(code, width);
                }
                index += 3;
            }
            None => break,
        }
    }
    widths
}

/// `/MediaBox` and `/Rotate`, following `/Parent` for either if the page does
/// not carry it.
fn page_geometry(document: &Document, page: &Dictionary) -> ([f32; 4], i64) {
    let mut media_box = None;
    let mut rotation = None;
    let mut node = page;
    // Bounded: a `/Parent` chain that loops would otherwise not terminate.
    for _ in 0..32 {
        if media_box.is_none() {
            media_box = node
                .get_deref(b"MediaBox", document)
                .and_then(Object::as_array)
                .ok()
                .and_then(|array| {
                    let values: Vec<f32> = array
                        .iter()
                        .filter_map(|entry| number(Some(entry)))
                        .collect();
                    <[f32; 4]>::try_from(values.as_slice()).ok()
                });
        }
        if rotation.is_none() {
            rotation = node
                .get_deref(b"Rotate", document)
                .ok()
                .and_then(|object| object.as_i64().ok());
        }
        if media_box.is_some() && rotation.is_some() {
            break;
        }
        let Some(parent) = node
            .get(b"Parent")
            .and_then(Object::as_reference)
            .ok()
            .and_then(|id| document.get_dictionary(id).ok())
        else {
            break;
        };
        node = parent;
    }

    // US Letter, the default a viewer falls back to when a page declares no
    // box of its own.
    (
        media_box.unwrap_or([0.0, 0.0, 612.0, 792.0]),
        rotation.unwrap_or(0),
    )
}

/// Build the transform that turns PDF user space into a page whose origin is
/// the bottom-left of the visible page and whose axes run the way a reader
/// sees them, and return the page size that results.
///
/// Folding `/Rotate` in here rather than rotating coordinates afterwards is
/// what makes rotated pages work at all: a landscape page is usually authored
/// in portrait user space with `/Rotate 90`, so its text runs along user-space
/// `+y`. Rotating only the run origins would leave every run claiming to be
/// horizontal; rotating the transform rotates the advance direction too.
pub fn normalizing_transform(media_box: [f32; 4], rotation: i64) -> (Matrix, f32, f32) {
    let x0 = media_box[0].min(media_box[2]);
    let y0 = media_box[1].min(media_box[3]);
    let x1 = media_box[0].max(media_box[2]);
    let y1 = media_box[1].max(media_box[3]);
    let width = (x1 - x0).max(1.0);
    let height = (y1 - y0).max(1.0);

    // `/Rotate` is defined as a multiple of 90; anything else is rounded to
    // one rather than refused, because a viewer would show something too.
    let quarter_turns = (((rotation as f64 / 90.0).round() as i64).rem_euclid(4)) as u8;
    match quarter_turns {
        1 => (Matrix::new(0.0, -1.0, 1.0, 0.0, -y0, x1), height, width),
        2 => (Matrix::new(-1.0, 0.0, 0.0, -1.0, x1, y1), width, height),
        3 => (Matrix::new(0.0, 1.0, -1.0, 0.0, y1, -x0), height, width),
        _ => (Matrix::new(1.0, 0.0, 0.0, 1.0, -x0, -y0), width, height),
    }
}

fn number(operand: Option<&Object>) -> Option<f32> {
    match operand? {
        Object::Integer(value) => Some(*value as f32),
        Object::Real(value) => Some(*value),
        _ => None,
    }
}

fn matrix_operand(operands: &[Object]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    Some(Matrix::new(
        number(operands.first())?,
        number(operands.get(1))?,
        number(operands.get(2))?,
        number(operands.get(3))?,
        number(operands.get(4))?,
        number(operands.get(5))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{TestPage, TestPdf};

    fn scan(bytes: &[u8]) -> PageScan {
        scan_page_number(bytes, 1)
    }

    fn scan_page_number(bytes: &[u8], page_number: u32) -> PageScan {
        let document = Document::load_mem(bytes).expect("test pdf loads");
        let page_id = *document.get_pages().get(&page_number).expect("page exists");
        Scanner::new(&document, Deadline::unlimited(), 64 * 1024 * 1024)
            .scan_page(page_id)
            .expect("page scans")
    }

    fn texts(scan: &PageScan) -> Vec<&str> {
        scan.runs.iter().map(|run| run.text.as_str()).collect()
    }

    #[test]
    fn matrix_multiplication_follows_the_row_vector_convention() {
        let scale = Matrix::new(2.0, 0.0, 0.0, 3.0, 0.0, 0.0);
        let translate = Matrix::translation(10.0, 20.0);

        // Scale first, then translate.
        assert_eq!(scale.then(&translate).apply(1.0, 1.0), (12.0, 23.0));
        // Translate first, then scale: the translation is scaled too.
        assert_eq!(translate.then(&scale).apply(1.0, 1.0), (22.0, 63.0));
        assert_eq!(Matrix::IDENTITY.apply(3.0, 4.0), (3.0, 4.0));
    }

    #[test]
    fn a_string_is_placed_at_its_text_position_with_the_width_the_font_declares() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "Hello"))
                .build(),
        );

        assert_eq!(texts(&scan), vec!["Hello"]);
        let run = &scan.runs[0];
        assert!((run.x - 72.0).abs() < 0.01, "{run:?}");
        assert!((run.y - 700.0).abs() < 0.01, "{run:?}");
        // Five glyphs, each half an em of a 10pt font.
        assert!((run.width - 25.0).abs() < 0.01, "{run:?}");
        assert!((run.height - 10.0).abs() < 0.01, "{run:?}");
        assert_eq!(scan.characters, 5);
        assert_eq!(scan.visible_characters, 5);
        assert!(!scan.truncated);
    }

    #[test]
    fn the_pen_advances_across_a_tj_array_so_later_strings_land_further_right() {
        // "AB" then a -1000/1000 em adjustment (a full em of space at 10pt),
        // then "CD".
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().text_array(
                    100.0,
                    500.0,
                    10.0,
                    &[("AB", -1000.0), ("CD", 0.0)],
                ))
                .build(),
        );

        assert_eq!(texts(&scan), vec!["AB", "CD"]);
        // Two glyphs at 5pt each, then 10pt of adjustment.
        assert!((scan.runs[0].x - 100.0).abs() < 0.01, "{:?}", scan.runs);
        assert!((scan.runs[1].x - 120.0).abs() < 0.01, "{:?}", scan.runs);
        assert!((scan.runs[0].y - scan.runs[1].y).abs() < 0.01);
    }

    #[test]
    fn t_star_moves_down_by_the_leading_so_lines_stack() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().paragraph(
                    72.0,
                    700.0,
                    10.0,
                    14.0,
                    &["first", "second", "third"],
                ))
                .build(),
        );

        assert_eq!(texts(&scan), vec!["first", "second", "third"]);
        assert!((scan.runs[0].y - 700.0).abs() < 0.01);
        assert!((scan.runs[1].y - 686.0).abs() < 0.01);
        assert!((scan.runs[2].y - 672.0).abs() < 0.01);
        // The x position is the paragraph's, not carried forward from the
        // previous line's end.
        assert!(scan.runs.iter().all(|run| (run.x - 72.0).abs() < 0.01));
    }

    #[test]
    fn cm_and_the_graphics_stack_move_text_and_then_stop_moving_it() {
        let scan = scan(
            &TestPdf::new()
                .page(
                    TestPage::letter()
                        .raw("q 1 0 0 1 100 200 cm")
                        .text(10.0, 10.0, 10.0, "inside")
                        .raw("Q")
                        .text(10.0, 10.0, 10.0, "outside"),
                )
                .build(),
        );

        let inside = &scan.runs[0];
        let outside = &scan.runs[1];
        assert!((inside.x - 110.0).abs() < 0.01, "{inside:?}");
        assert!((inside.y - 210.0).abs() < 0.01, "{inside:?}");
        assert!((outside.x - 10.0).abs() < 0.01, "{outside:?}");
        assert!((outside.y - 10.0).abs() < 0.01, "{outside:?}");
    }

    #[test]
    fn a_scaling_cm_scales_the_run_width_and_height_too() {
        let scan = scan(
            &TestPdf::new()
                .page(
                    TestPage::letter()
                        .raw("q 2 0 0 2 0 0 cm")
                        .text(10.0, 10.0, 10.0, "Hello")
                        .raw("Q"),
                )
                .build(),
        );

        let run = &scan.runs[0];
        assert!((run.x - 20.0).abs() < 0.01, "{run:?}");
        assert!((run.width - 50.0).abs() < 0.01, "{run:?}");
        assert!((run.height - 20.0).abs() < 0.01, "{run:?}");
    }

    #[test]
    fn character_and_word_spacing_widen_the_advance() {
        let plain = scan(
            &TestPdf::new()
                .page(TestPage::letter().text(0.0, 100.0, 10.0, "a b"))
                .build(),
        );
        let spaced = scan(
            &TestPdf::new()
                .page(TestPage::letter().raw("BT 2 Tc 5 Tw /F1 10 Tf 0 100 Td (a b) Tj ET"))
                .build(),
        );

        // Three glyphs at 5pt.
        assert!(
            (plain.runs[0].width - 15.0).abs() < 0.01,
            "{:?}",
            plain.runs
        );
        // Plus 2pt per glyph and 5pt for the one space.
        assert!(
            (spaced.runs[0].width - 26.0).abs() < 0.01,
            "{:?}",
            spaced.runs
        );
    }

    #[test]
    fn horizontal_scaling_narrows_the_advance_without_changing_the_height() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().raw("BT 50 Tz /F1 10 Tf 0 100 Td (Hello) Tj ET"))
                .build(),
        );

        let run = &scan.runs[0];
        assert!((run.width - 12.5).abs() < 0.01, "{run:?}");
        assert!((run.height - 10.0).abs() < 0.01, "{run:?}");
    }

    #[test]
    fn a_media_box_that_does_not_start_at_the_origin_is_shifted_to_it() {
        // The page box starts at (20, 30); the text keeps its absolute
        // user-space position and must come back 20 left and 30 down of where
        // it would be on a box anchored at the origin.
        let scan = scan(
            &TestPdf::new()
                .page(
                    TestPage::letter()
                        .origin(20.0, 30.0)
                        .text(72.0, 700.0, 10.0, "Hello"),
                )
                .build(),
        );

        assert!((scan.runs[0].x - 52.0).abs() < 0.01, "{:?}", scan.runs);
        assert!((scan.runs[0].y - 670.0).abs() < 0.01, "{:?}", scan.runs);
        assert!((scan.width - 612.0).abs() < 0.01);
        assert!((scan.height - 792.0).abs() < 0.01);
    }

    #[test]
    fn a_rotated_page_reports_its_displayed_size_and_displayed_positions() {
        // Text along the left edge of a portrait page, near the top.
        let scan = scan(
            &TestPdf::new()
                .page(
                    TestPage::sized(400.0, 800.0)
                        .rotated(90)
                        .text(50.0, 700.0, 10.0, "Hello"),
                )
                .build(),
        );

        // Rotating the page a quarter turn swaps its dimensions.
        assert!((scan.width - 800.0).abs() < 0.01);
        assert!((scan.height - 400.0).abs() < 0.01);
        // (50, 700) in user space is near the top-left; a clockwise quarter
        // turn puts it near the top-right.
        let run = &scan.runs[0];
        assert!((run.x - 700.0).abs() < 0.01, "{run:?}");
        assert!((run.y - 350.0).abs() < 0.01, "{run:?}");
        // And the run still reads as horizontal text, because the rotation was
        // folded into the transform rather than applied to the origin alone.
        assert!((run.width - 25.0).abs() < 0.01, "{run:?}");
    }

    #[test]
    fn every_quarter_turn_keeps_the_page_within_its_own_displayed_box() {
        for rotation in [0, 90, 180, 270] {
            let scan = scan(
                &TestPdf::new()
                    .page(
                        TestPage::sized(400.0, 800.0)
                            .rotated(rotation)
                            .text(50.0, 700.0, 10.0, "Hello")
                            .text(350.0, 50.0, 10.0, "World"),
                    )
                    .build(),
            );

            for run in &scan.runs {
                assert!(
                    run.x >= -0.01 && run.x <= scan.width + 0.01,
                    "rotation {rotation}: {run:?} outside width {}",
                    scan.width
                );
                assert!(
                    run.y >= -0.01 && run.y <= scan.height + 0.01,
                    "rotation {rotation}: {run:?} outside height {}",
                    scan.height
                );
            }
        }
    }

    #[test]
    fn invisible_text_is_kept_and_counted_apart_because_it_is_what_ocr_leaves() {
        let scan = scan(
            &TestPdf::new()
                .page(
                    TestPage::letter()
                        .image(0.0, 0.0, 612.0, 792.0)
                        .text_with_mode(72.0, 700.0, 10.0, "scanned words", 3),
                )
                .build(),
        );

        assert_eq!(texts(&scan), vec!["scanned words"]);
        assert!(scan.runs[0].invisible);
        assert_eq!(scan.visible_characters, 0);
        assert_eq!(scan.characters, 12);
        assert_eq!(scan.images, 1);
        assert!(scan.looks_like_ocr_layer());
    }

    #[test]
    fn a_page_with_an_image_and_no_text_reports_the_image() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .build(),
        );

        assert!(scan.runs.is_empty());
        assert_eq!(scan.characters, 0);
        assert_eq!(scan.images, 1);
        assert!(!scan.looks_like_ocr_layer());
    }

    #[test]
    fn a_blank_page_has_neither_text_nor_images() {
        let scan = scan(&TestPdf::new().page(TestPage::letter()).build());

        assert!(scan.runs.is_empty());
        assert_eq!(scan.images, 0);
        assert_eq!(scan.characters, 0);
    }

    #[test]
    fn text_shown_before_any_font_is_selected_is_dropped_rather_than_guessed() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().raw("BT 100 100 Td (mystery) Tj ET"))
                .build(),
        );

        assert!(scan.runs.is_empty(), "{:?}", scan.runs);
    }

    #[test]
    fn win_ansi_bytes_decode_through_the_fonts_declared_encoding() {
        let scan = scan(
            &TestPdf::new()
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "caf\u{e9} cr\u{e8}me"))
                .build(),
        );

        assert_eq!(texts(&scan), vec!["caf\u{e9} cr\u{e8}me"]);
    }

    #[test]
    fn each_page_is_scanned_on_its_own() {
        let bytes = TestPdf::new()
            .page(TestPage::letter().text(72.0, 700.0, 10.0, "page one"))
            .page(TestPage::letter().text(72.0, 700.0, 10.0, "page two"))
            .build();

        assert_eq!(texts(&scan_page_number(&bytes, 1)), vec!["page one"]);
        assert_eq!(texts(&scan_page_number(&bytes, 2)), vec!["page two"]);
    }

    #[test]
    fn an_expired_deadline_stops_the_walk_instead_of_returning_partial_text() {
        let bytes = TestPdf::new()
            .page(TestPage::letter().paragraph(72.0, 700.0, 10.0, 12.0, &["a"; 4000]))
            .build();
        let document = Document::load_mem(&bytes).expect("loads");
        let page_id = *document.get_pages().get(&1).expect("page one");

        let error = Scanner::new(&document, Deadline::expired(), 64 * 1024 * 1024)
            .scan_page(page_id)
            .expect_err("an expired budget must stop the walk");

        assert!(matches!(error, ScanError::TimedOut), "{error}");
    }
}
