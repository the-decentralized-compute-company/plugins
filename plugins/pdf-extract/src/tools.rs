//! The whole contribution surface of `pdf-extract` in one declaration, and the
//! work behind it.
//!
//! Five MCP tools and nothing else: no config schema, no web UI, no HTTP
//! routes, no mesh channels, no events. Declaring the smallest set that does
//! the job matters more than usual here — this plugin reads documents on
//! hardware somebody else contributed.
//!
//! There is deliberately no `config_schema`. The console would render the
//! settings and the host would store them, but `[plugin.settings]` is never
//! delivered to the plugin process, so a `roots` setting would look
//! authoritative and do nothing. The roots arrive through `[[plugin]].args`
//! instead; see [`crate::options`].
//!
//! Every handler runs its work on `spawn_blocking` and races it against the
//! configured timeout. Parsing a PDF is synchronous, CPU-bound, and — on a file
//! written to be hostile — potentially slow, and the control connection has to
//! keep answering health checks while it happens.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{
    PluginError, PluginMetadata, PluginResult, SimplePlugin, capability, mcp, plugin,
    plugin_server_info,
};

use crate::budget::Deadline;
use crate::glyphs::PageScan;
use crate::layout::{self, LayoutMode};
use crate::listing;
use crate::options::{Limits, Options, PLUGIN_NAME, PLUGIN_VERSION};
use crate::paths::{PathError, Roots};
use crate::pdf::{self, DocumentInfo, PageKind, Pdf};
use crate::tables::{self, TableOptions};

/// Documents one `list_documents` call will return.
const DEFAULT_LIST_LIMIT: u32 = 200;
const MAX_LIST_LIMIT: u32 = 2_000;

/// Grace on top of the cooperative deadline before the handler stops waiting
/// for its own blocking task. The cooperative check should always fire first;
/// this is the backstop for time spent inside a single `lopdf` call.
const RACE_GRACE_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

/// How to turn the glyphs on a page into lines of text.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutChoice {
    /// Detect columns and read them one after another. Right for articles,
    /// papers, and anything with a multi-column body. This is the default.
    #[default]
    Auto,
    /// Treat the page as a single column: group text into lines by position and
    /// read the lines down the page. Use this when `auto` reported more columns
    /// than the page has — a form or a list of labels and values can look like
    /// two columns to any detector.
    Single,
    /// Keep the horizontal positions by padding with spaces, drawing the page
    /// into a fixed-width character grid. Alignment survives; it is not prose.
    /// Use it for receipts, statements, and anything where the columns carry
    /// the meaning.
    Preserve,
}

impl From<LayoutChoice> for LayoutMode {
    fn from(choice: LayoutChoice) -> Self {
        match choice {
            LayoutChoice::Auto => LayoutMode::Auto,
            LayoutChoice::Single => LayoutMode::Single,
            LayoutChoice::Preserve => LayoutMode::Preserve,
        }
    }
}

impl LayoutChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Single => "single",
            Self::Preserve => "preserve",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractTextArgs {
    /// The document, written as `<root label>/<path inside that root>`, for
    /// example `docs/reports/q4.pdf`. Call `list_documents` to see the exact
    /// strings this plugin accepts; absolute paths and `..` are refused.
    pub path: String,

    /// Which pages to read: `3`, `1-5`, `2,4,7-9`, `10-` for page ten onwards,
    /// or `all`. Pages are numbered from 1. Defaults to the whole document, up
    /// to the operator's page ceiling.
    #[serde(default)]
    pub pages: Option<String>,

    /// How to order the text on each page. Defaults to `auto`, which detects
    /// columns. Try `single` if the result looks shuffled and `preserve` if the
    /// page is really a table or a form.
    #[serde(default)]
    pub layout: Option<LayoutChoice>,

    /// Stop after roughly this many characters across all returned pages.
    /// Clamped to the operator's ceiling; omit it to use that ceiling.
    #[serde(default)]
    pub max_chars: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentInfoArgs {
    /// The document, written as `<root label>/<path inside that root>`.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractTablesArgs {
    /// The document, written as `<root label>/<path inside that root>`.
    pub path: String,

    /// Which pages to look at: `3`, `1-5`, `2,4,7-9`, or `all`. Defaults to the
    /// whole document, up to the operator's page ceiling.
    #[serde(default)]
    pub pages: Option<String>,

    /// Minimum rows for a run of aligned lines to count as a table. 2 or more;
    /// defaults to 2. Raise it to stop a two-line header pair being reported.
    #[serde(default)]
    pub min_rows: Option<u32>,

    /// Minimum columns for a table. 2 or more; defaults to 2.
    #[serde(default)]
    pub min_columns: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDocumentsArgs {
    /// Restrict the listing to one directory, written as
    /// `<root label>/<path inside that root>`, for example `docs/reports`.
    /// Omit it to list every configured root.
    #[serde(default)]
    pub path: Option<String>,

    /// Only list files whose name contains this text, matched without regard
    /// to case.
    #[serde(default)]
    pub name_contains: Option<String>,

    /// Maximum documents to return, 1-2000. Defaults to 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {}

// ---------------------------------------------------------------------------
// Tool responses
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PageText {
    pub page: u32,
    /// `text`, `ocr_layer`, `image_only`, or `empty`. Only the first two carry
    /// characters; see the tool description for what the others mean.
    pub kind: &'static str,
    /// Column bands the layout detector cut this page into. More than one on a
    /// page that is not really multi-column is the signal to retry with
    /// `layout: "single"`.
    pub columns: usize,
    pub characters: usize,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct ExtractTextResponse {
    pub path: String,
    pub layout: &'static str,
    pub pages_in_document: u32,
    pub pages_returned: usize,
    /// Pages that hold an image and no text at all. Nothing was extracted from
    /// them, and OCR is out of scope for this plugin.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub image_only_pages: Vec<u32>,
    /// Pages with neither text nor images.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub empty_pages: Vec<u32>,
    /// True when a cap stopped the extraction early, so more text exists.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub pages: Vec<PageText>,
}

#[derive(Debug, Serialize)]
pub struct PageSummary {
    pub page: u32,
    pub kind: &'static str,
    pub width_points: f32,
    pub height_points: f32,
    pub characters: usize,
    pub images: u32,
}

#[derive(Debug, Serialize)]
pub struct DocumentInfoResponse {
    pub path: String,
    pub file_bytes: u64,
    pub pdf_version: String,
    pub pages: u32,
    /// The file was encrypted and opened with an empty password. A PDF needing
    /// a real password is refused when it is opened, not reported here.
    pub encrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    pub pages_examined: usize,
    pub text_pages: usize,
    pub ocr_layer_pages: usize,
    pub image_only_pages: usize,
    pub empty_pages: usize,
    /// False when every examined page is an image or blank: `extract_text` will
    /// refuse this document and say why.
    pub has_extractable_text: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub page_details: Vec<PageSummary>,
}

#[derive(Debug, Serialize)]
pub struct TableOnPage {
    pub page: u32,
    pub columns: usize,
    pub rows: usize,
    /// Mean fraction of rows in which each detected column carried a value.
    /// `1.0` is a table with no holes; a low number means the alignment was
    /// ragged and the grid should be read with suspicion.
    pub occupancy: f32,
    pub cells: Vec<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct ExtractTablesResponse {
    pub path: String,
    pub pages_examined: usize,
    pub tables_found: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub image_only_pages: Vec<u32>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub tables: Vec<TableOnPage>,
}

#[derive(Debug, Serialize)]
pub struct ListedDocument {
    /// Ready to pass straight to `extract_text`, `document_info`, or
    /// `extract_tables`.
    pub path: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ListDocumentsResponse {
    pub roots: Vec<String>,
    pub count: usize,
    pub directories_scanned: u64,
    pub truncated: bool,
    pub documents: Vec<ListedDocument>,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// The labels a path may start with. Absolute paths on the contributor's
    /// disk are deliberately not reported.
    pub roots: Vec<String>,
    pub max_file_bytes: u64,
    pub max_pages_per_call: u32,
    pub max_chars_per_call: u64,
    pub timeout_secs: u64,
    pub max_decompressed_bytes: u64,
    pub layout_modes: Vec<&'static str>,
    /// Stated plainly because it is the limitation people hit: this plugin
    /// reads text that is in the file and does not recognise text in an image.
    pub ocr: &'static str,
}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

pub struct Library {
    roots: Roots,
    limits: Limits,
}

impl Library {
    pub fn open(options: Options) -> Result<Arc<Self>, crate::paths::RootsError> {
        Ok(Arc::new(Self {
            roots: Roots::open(&options.roots)?,
            limits: options.limits,
        }))
    }

    /// A library with no roots, for `--print-package-manifest`.
    pub fn for_manifest_only() -> Arc<Self> {
        Arc::new(Self {
            roots: Roots::empty(),
            limits: Limits {
                max_file_bytes: 0,
                max_pages: 1,
                max_chars: 1_000,
                timeout: std::time::Duration::from_secs(1),
                max_decompressed_bytes: 1024 * 1024,
            },
        })
    }

    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn ensure_configured(&self) -> PluginResult<()> {
        if self.roots.is_empty() {
            return Err(PluginError::internal(
                "pdf-extract has no configured root; pass --root <dir> in [[plugin]].args",
            ));
        }
        Ok(())
    }

    fn open_document(&self, path: &str) -> PluginResult<(String, Pdf)> {
        self.ensure_configured()?;
        let resolved = self
            .roots
            .resolve_file(path)
            .map_err(|error| path_error(path, error))?;
        let pdf = Pdf::open(&resolved.absolute, &self.limits).map_err(|error| {
            PluginError::invalid_params(format!("{}: {error}", resolved.display))
        })?;
        Ok((resolved.display, pdf))
    }

    /// Resolve the pages a caller asked for, clamped to the operator's ceiling.
    fn select_pages(
        &self,
        pdf: &Pdf,
        specification: Option<&str>,
    ) -> PluginResult<(Vec<u32>, bool)> {
        let requested = pdf::parse_page_selection(specification.unwrap_or("all"), pdf.page_count())
            .map_err(PluginError::invalid_params)?;
        let ceiling = self.limits.max_pages as usize;
        let truncated = requested.len() > ceiling;
        Ok((requested.into_iter().take(ceiling).collect(), truncated))
    }

    pub fn status(&self) -> StatusResponse {
        StatusResponse {
            roots: self.roots.labels(),
            max_file_bytes: self.limits.max_file_bytes,
            max_pages_per_call: self.limits.max_pages,
            max_chars_per_call: self.limits.max_chars,
            timeout_secs: self.limits.timeout.as_secs(),
            max_decompressed_bytes: self.limits.max_decompressed_bytes,
            layout_modes: vec!["auto", "single", "preserve"],
            ocr: "not supported: pages that are images carry no text and are reported as \
                  image_only rather than as empty",
        }
    }

    pub fn extract_text(
        &self,
        args: ExtractTextArgs,
        deadline: Deadline,
    ) -> PluginResult<ExtractTextResponse> {
        let (display, pdf) = self.open_document(&args.path)?;
        let (selection, page_cap_hit) = self.select_pages(&pdf, args.pages.as_deref())?;
        let layout = args.layout.unwrap_or_default();
        let budget = args
            .max_chars
            .map(u64::from)
            .unwrap_or(self.limits.max_chars)
            .min(self.limits.max_chars)
            .max(1) as usize;

        let mut response = ExtractTextResponse {
            path: display,
            layout: layout.as_str(),
            pages_in_document: pdf.page_count(),
            pages_returned: 0,
            image_only_pages: Vec::new(),
            empty_pages: Vec::new(),
            truncated: page_cap_hit,
            notes: Vec::new(),
            pages: Vec::new(),
        };
        if page_cap_hit {
            response.notes.push(format!(
                "Only the first {} selected pages were read; the operator's --max-pages ceiling \
                 is {}.",
                selection.len(),
                self.limits.max_pages
            ));
        }

        let mut remaining = budget;
        let mut extracted = 0usize;
        for page_number in &selection {
            let scan = self.scan(&pdf, *page_number, deadline)?;
            let kind = pdf::classify(&scan);
            match kind {
                PageKind::ImageOnly => response.image_only_pages.push(*page_number),
                PageKind::Empty => response.empty_pages.push(*page_number),
                _ => {}
            }

            let rendered = layout::render_page(&scan.runs, layout.into(), remaining);
            extracted += rendered.text.chars().count();
            remaining = remaining.saturating_sub(rendered.text.chars().count());
            response.truncated |= rendered.truncated || scan.truncated;
            response.pages.push(PageText {
                page: *page_number,
                kind: kind.as_str(),
                columns: rendered.columns,
                characters: rendered.text.chars().count(),
                text: rendered.text,
            });
            if remaining == 0 {
                response.truncated = true;
                break;
            }
        }
        response.pages_returned = response.pages.len();

        // The failure this plugin exists to make loud. An empty string from a
        // scanned page is indistinguishable from an empty string from a blank
        // one, so when nothing at all came out, say which it was.
        if extracted == 0 {
            return Err(PluginError::invalid_params(no_text_message(
                &response.path,
                &selection,
                &response.image_only_pages,
                &response.empty_pages,
            )));
        }
        if !response.image_only_pages.is_empty() {
            response.notes.push(format!(
                "{} of the {} pages read are images with no text layer and produced nothing. \
                 pdf-extract does not do OCR; run the file through an OCR tool first if you need \
                 those pages.",
                response.image_only_pages.len(),
                response.pages.len()
            ));
        }
        if response
            .pages
            .iter()
            .any(|page| page.kind == PageKind::OcrLayer.as_str())
        {
            response.notes.push(
                "Some pages carry an invisible text layer over a page image, which is what an OCR \
                 tool leaves behind. That text is returned, and it is only as accurate as the OCR \
                 that produced it."
                    .to_string(),
            );
        }
        if layout == LayoutChoice::Auto && response.pages.iter().any(|page| page.columns > 1) {
            response.notes.push(
                "Column detection split at least one page into more than one column. If the text \
                 reads out of order, call again with layout \"single\", or \"preserve\" if the \
                 page is really a table."
                    .to_string(),
            );
        }
        Ok(response)
    }

    pub fn document_info(
        &self,
        args: DocumentInfoArgs,
        deadline: Deadline,
    ) -> PluginResult<DocumentInfoResponse> {
        let (display, pdf) = self.open_document(&args.path)?;
        let info: DocumentInfo = pdf.info();
        let (selection, page_cap_hit) = self.select_pages(&pdf, None)?;

        let mut details = Vec::with_capacity(selection.len());
        let mut counts = [0usize; 4];
        let mut has_extractable_text = false;
        for page_number in &selection {
            let scan = self.scan(&pdf, *page_number, deadline)?;
            let kind = pdf::classify(&scan);
            has_extractable_text |= kind.has_text();
            counts[match kind {
                PageKind::Text => 0,
                PageKind::OcrLayer => 1,
                PageKind::ImageOnly => 2,
                PageKind::Empty => 3,
            }] += 1;
            details.push(PageSummary {
                page: *page_number,
                kind: kind.as_str(),
                width_points: round_points(scan.width),
                height_points: round_points(scan.height),
                characters: scan.characters,
                images: scan.images,
            });
        }

        let mut notes = Vec::new();
        if page_cap_hit {
            notes.push(format!(
                "Only the first {} pages were examined; the operator's --max-pages ceiling is {}.",
                selection.len(),
                self.limits.max_pages
            ));
        }
        if counts[0] + counts[1] == 0 && counts[2] > 0 {
            notes.push(
                "Every examined page is an image with no text layer. This is a scan, and \
                 extract_text will refuse it: pdf-extract does not do OCR."
                    .to_string(),
            );
        }
        if counts[1] > 0 {
            notes.push(
                "Some pages carry an invisible text layer over a page image — the signature of a \
                 scan that has already been through OCR. The text is readable and is only as \
                 good as that OCR."
                    .to_string(),
            );
        }

        Ok(DocumentInfoResponse {
            path: display,
            file_bytes: pdf.file_bytes(),
            pdf_version: info.pdf_version,
            pages: info.pages,
            encrypted: info.encrypted,
            title: info.title,
            author: info.author,
            subject: info.subject,
            keywords: info.keywords,
            creator: info.creator,
            producer: info.producer,
            created: info.created,
            modified: info.modified,
            pages_examined: details.len(),
            text_pages: counts[0],
            ocr_layer_pages: counts[1],
            image_only_pages: counts[2],
            empty_pages: counts[3],
            has_extractable_text,
            notes,
            page_details: details,
        })
    }

    pub fn extract_tables(
        &self,
        args: ExtractTablesArgs,
        deadline: Deadline,
    ) -> PluginResult<ExtractTablesResponse> {
        let (display, pdf) = self.open_document(&args.path)?;
        let (selection, page_cap_hit) = self.select_pages(&pdf, args.pages.as_deref())?;
        let options = TableOptions {
            min_rows: args.min_rows.unwrap_or(2).max(2) as usize,
            min_columns: args.min_columns.unwrap_or(2).max(2) as usize,
        };

        let mut found: Vec<TableOnPage> = Vec::new();
        let mut image_only = Vec::new();
        let mut truncated = page_cap_hit;
        for page_number in &selection {
            let scan = self.scan(&pdf, *page_number, deadline)?;
            if pdf::classify(&scan) == PageKind::ImageOnly {
                image_only.push(*page_number);
                continue;
            }
            truncated |= scan.truncated;
            // Deliberately *not* run over the column regions `extract_text`
            // uses. A table's rows cross its columns, so the same cut that
            // makes prose readable would slice every table down the middle;
            // rows are assembled from the whole page instead, and
            // `crate::tables` carries the guards that keep a two-column page
            // from reading as a two-column table.
            for table in tables::find_tables(&layout::group_lines(&scan.runs), &options) {
                found.push(TableOnPage {
                    page: *page_number,
                    columns: table.columns,
                    rows: table.rows.len(),
                    occupancy: (table.occupancy * 100.0).round() / 100.0,
                    cells: table.rows,
                });
            }
        }

        let mut notes = Vec::new();
        if page_cap_hit {
            notes.push(format!(
                "Only the first {} selected pages were examined; the operator's --max-pages \
                 ceiling is {}.",
                selection.len(),
                self.limits.max_pages
            ));
        }
        if !image_only.is_empty() {
            notes.push(format!(
                "{} page(s) are images with no text layer and were skipped; a table in a scan is \
                 a picture of a table, and pdf-extract does not do OCR.",
                image_only.len()
            ));
        }
        if found.is_empty() {
            notes.push(
                "No table was found. Detection is based on text alignment, not on drawn borders, \
                 so a table whose cells do not line up is not recovered. Try extract_text with \
                 layout \"preserve\" to see the page as it is laid out."
                    .to_string(),
            );
        }

        Ok(ExtractTablesResponse {
            path: display,
            pages_examined: selection.len(),
            tables_found: found.len(),
            image_only_pages: image_only,
            truncated,
            notes,
            tables: found,
        })
    }

    pub fn list_documents(
        &self,
        args: ListDocumentsArgs,
        deadline: Deadline,
    ) -> PluginResult<ListDocumentsResponse> {
        self.ensure_configured()?;
        let scope = match args.path.as_deref() {
            Some(path) => Some(
                self.roots
                    .resolve_directory(path)
                    .map_err(|error| path_error(path, error))?,
            ),
            None => None,
        };
        let limit = args
            .limit
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT) as usize;

        let listing = listing::list(
            &self.roots,
            scope.as_ref(),
            args.name_contains.as_deref(),
            limit,
            deadline,
        );

        Ok(ListDocumentsResponse {
            roots: self.roots.labels(),
            count: listing.documents.len(),
            directories_scanned: listing.directories_scanned,
            truncated: listing.truncated,
            documents: listing
                .documents
                .into_iter()
                .map(|entry| ListedDocument {
                    path: entry.path,
                    bytes: entry.bytes,
                    modified_unix: entry.modified_unix,
                })
                .collect(),
        })
    }

    fn scan(&self, pdf: &Pdf, page_number: u32, deadline: Deadline) -> PluginResult<PageScan> {
        pdf.scan(page_number, deadline, &self.limits)
            .map_err(|error| match error {
                crate::glyphs::ScanError::TimedOut => timeout_error(deadline),
                other => PluginError::invalid_params(format!("page {page_number}: {other}")),
            })
    }
}

fn round_points(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

fn path_error(path: &str, error: PathError) -> PluginError {
    PluginError::invalid_params(format!("`{path}` was refused: {error}"))
}

fn timeout_error(deadline: Deadline) -> PluginError {
    PluginError::internal(format!(
        "pdf-extract ran out of its {}s budget on this document. Ask for fewer pages, or raise \
         --timeout-secs in [[plugin]].args.",
        deadline.budget().as_secs()
    ))
}

/// The message that keeps a scanned document from looking like a blank one.
fn no_text_message(path: &str, selection: &[u32], image_only: &[u32], empty: &[u32]) -> String {
    let pages = selection.len();
    if !image_only.is_empty() && empty.is_empty() {
        return format!(
            "`{path}`: all {pages} page(s) read are images with no text layer — this is a scan. \
             No text was extracted, and returning an empty result would look like an empty \
             document. pdf-extract does not do OCR; run the file through an OCR tool and extract \
             from its output."
        );
    }
    if !image_only.is_empty() {
        return format!(
            "`{path}`: no text on any of the {pages} page(s) read. {} are images with no text \
             layer (a scan, which needs OCR that pdf-extract does not do) and {} are genuinely \
             blank.",
            image_only.len(),
            empty.len()
        );
    }
    format!(
        "`{path}`: the {pages} page(s) read contain no text and no images — they are blank. If \
         you expected content here, check the page selection with document_info."
    )
}

// ---------------------------------------------------------------------------
// Declaration
// ---------------------------------------------------------------------------

/// Run one library operation on a blocking thread, bounded by the configured
/// timeout in both of the ways that matter.
macro_rules! bounded_tool {
    ($library:expr, $args:ident, $operation:ident) => {{
        let library = $library;
        Box::pin(async move {
            let deadline = Deadline::starting_now(library.limits().timeout);
            let race = deadline.budget() + std::time::Duration::from_secs(RACE_GRACE_SECS);
            let worker = library.clone();
            let task = tokio::task::spawn_blocking(move || worker.$operation($args, deadline));
            match tokio::time::timeout(race, task).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) if error.is_panic() => Err(PluginError::internal(format!(
                    "pdf-extract's {} worker panicked, which almost always means a malformed \
                     PDF reached the parser. The plugin is still running; the file is not \
                     readable by it.",
                    stringify!($operation)
                ))),
                Ok(Err(error)) => Err(PluginError::internal(format!(
                    "pdf-extract {} task failed: {error}",
                    stringify!($operation)
                ))),
                // The cooperative deadline should have fired first. Reaching
                // here means the time went somewhere the walk does not check,
                // so say what actually happened: the work is abandoned, not
                // cancelled, and it finishes on its own thread.
                Err(_) => Err(PluginError::internal(format!(
                    "pdf-extract gave up on this document after {}s. The parse is still \
                     finishing on a worker thread and will be discarded. Ask for fewer pages, \
                     or raise --timeout-secs in [[plugin]].args.",
                    deadline.budget().as_secs()
                ))),
            }
        })
    }};
}

pub fn pdf_extract_plugin(library: Arc<Library>) -> SimplePlugin {
    let for_extract_text = Arc::clone(&library);
    let for_document_info = Arc::clone(&library);
    let for_extract_tables = Arc::clone(&library);
    let for_list_documents = Arc::clone(&library);
    let for_status = Arc::clone(&library);
    let for_health = library;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "PDF extract",
                "Read text, metadata, and tables out of local PDF files",
                Some(
                    "Call list_documents to find a file and use the `path` it returns verbatim: \
                     every path is `<root label>/<path>`, and absolute paths are refused. Every \
                     extracted page comes back with its page number — cite those. A page \
                     reported as image_only is a scan with no text in the file at all; this \
                     plugin does not do OCR, so do not report its content as empty.",
                ),
            ),
        ),

        // A stable name for "something on this node can read a PDF", so a
        // caller can depend on the capability rather than on this plugin's id.
        provides: [capability("pdf-extract.v1")],

        mcp: [
            // Projected as `pdf-extract.extract_text` on the host MCP endpoint
            // and at POST /api/plugins/pdf-extract/tools/extract_text.
            mcp::tool("extract_text")
                .title("Extract text from a PDF")
                .description(
                    "Extract the text of a PDF, whole or by page range, preserving reading order \
                     and page boundaries. Each page comes back separately with its page number, \
                     so an answer can cite `page 4` exactly. Multi-column pages are detected and \
                     read one column at a time rather than interleaved. A page that is an image \
                     with no text layer is reported as image_only and produces nothing — this \
                     tool does no OCR, and it returns an error rather than an empty string when \
                     that is all there is.",
                )
                .input::<ExtractTextArgs>()
                .handle(move |args: ExtractTextArgs, _context| {
                    bounded_tool!(Arc::clone(&for_extract_text), args, extract_text)
                }),

            mcp::tool("document_info")
                .title("Describe a PDF")
                .description(
                    "Report a PDF's page count, PDF version, size, and document information \
                     (title, author, dates), then classify every page as text, ocr_layer, \
                     image_only, or empty. Call this first when a document might be a scan: \
                     has_extractable_text says whether extract_text will produce anything at \
                     all.",
                )
                .input::<DocumentInfoArgs>()
                .handle(move |args: DocumentInfoArgs, _context| {
                    bounded_tool!(Arc::clone(&for_document_info), args, document_info)
                }),

            mcp::tool("extract_tables")
                .title("Extract tables from a PDF")
                .description(
                    "Find tables on the pages of a PDF and return them as rows of cells with the \
                     page number. Detection is based on text alignment — several consecutive \
                     rows whose cells start at the same horizontal positions — and ignores drawn \
                     borders, so a bordered table whose text does not line up is not found. Each \
                     table reports an occupancy score for how complete its grid was. Returns an \
                     empty list, with a note, when a page has no aligned rows.",
                )
                .input::<ExtractTablesArgs>()
                .handle(move |args: ExtractTablesArgs, _context| {
                    bounded_tool!(Arc::clone(&for_extract_tables), args, extract_tables)
                }),

            mcp::tool("list_documents")
                .title("List available PDFs")
                .description(
                    "List the PDF files inside the configured roots, optionally under one \
                     directory or filtered by name. The `path` on each result is exactly the \
                     string the other tools take, so use it verbatim rather than building a path \
                     by hand. Symbolic links are not followed and the walk is bounded in depth \
                     and count.",
                )
                .input::<ListDocumentsArgs>()
                .handle(move |args: ListDocumentsArgs, _context| {
                    bounded_tool!(Arc::clone(&for_list_documents), args, list_documents)
                }),

            mcp::tool("status")
                .title("Report configuration and limits")
                .description(
                    "Report the root labels a path may start with and every limit in force: file \
                     size, pages and characters per call, timeout, and decompression ceiling. \
                     Touches no file, so it answers even when everything else is failing.",
                )
                .input::<StatusArgs>()
                .handle(move |_args: StatusArgs, _context| {
                    let library = Arc::clone(&for_status);
                    Box::pin(async move { Ok(library.status()) })
                }),
        ],

        // Health must stay fast and independent of any document, so it reports
        // configuration state and touches no file.
        health: move |_context| {
            let library = Arc::clone(&for_health);
            Box::pin(async move {
                Ok(format!(
                    "ok; {} root(s) configured, {} pages and {} chars per call",
                    library.roots().labels().len(),
                    library.limits().max_pages,
                    library.limits().max_chars
                ))
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::options::RootSpec;
    use crate::testsupport::{TempTree, TestPage, TestPdf, simple_pdf};
    use std::time::Duration;

    fn library(tree: &TempTree, labels: &[(&str, &str)]) -> Arc<Library> {
        Library::open(Options {
            roots: labels
                .iter()
                .map(|(label, relative)| RootSpec {
                    label: (*label).to_string(),
                    directory: tree.path().join(relative),
                })
                .collect(),
            limits: Limits {
                max_file_bytes: 32 * 1024 * 1024,
                max_pages: 200,
                max_chars: 200_000,
                timeout: Duration::from_secs(30),
                max_decompressed_bytes: 128 * 1024 * 1024,
            },
        })
        .expect("library opens")
    }

    fn extract(library: &Library, path: &str) -> PluginResult<ExtractTextResponse> {
        library.extract_text(
            ExtractTextArgs {
                path: path.to_string(),
                pages: None,
                layout: None,
                max_chars: None,
            },
            Deadline::unlimited(),
        )
    }

    #[test]
    fn text_comes_back_page_by_page_with_its_page_number() {
        let tree = TempTree::new("tools-text");
        tree.write_bytes(
            "docs/report.pdf",
            &TestPdf::new()
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "first page"))
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "second page"))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = extract(&library, "docs/report.pdf").expect("extracts");

        assert_eq!(response.path, "docs/report.pdf");
        assert_eq!(response.pages_in_document, 2);
        assert_eq!(response.pages_returned, 2);
        assert_eq!(response.pages[0].page, 1);
        assert_eq!(response.pages[0].text, "first page");
        assert_eq!(response.pages[1].page, 2);
        assert_eq!(response.pages[1].text, "second page");
        assert!(!response.truncated);
    }

    #[test]
    fn a_page_range_returns_only_those_pages() {
        let tree = TempTree::new("tools-range");
        let mut builder = TestPdf::new();
        for index in 1..=5 {
            builder =
                builder.page(TestPage::letter().text(72.0, 700.0, 10.0, &format!("page {index}")));
        }
        tree.write_bytes("docs/five.pdf", &builder.build());
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .extract_text(
                ExtractTextArgs {
                    path: "docs/five.pdf".to_string(),
                    pages: Some("2-3".to_string()),
                    layout: None,
                    max_chars: None,
                },
                Deadline::unlimited(),
            )
            .expect("extracts");

        let numbers: Vec<u32> = response.pages.iter().map(|page| page.page).collect();
        assert_eq!(numbers, vec![2, 3]);
        assert_eq!(response.pages[0].text, "page 2");
    }

    /// The single most confusing failure this plugin has to avoid.
    #[test]
    fn a_scanned_document_is_an_error_naming_ocr_rather_than_an_empty_success() {
        let tree = TempTree::new("tools-scan");
        tree.write_bytes(
            "docs/scan.pdf",
            &TestPdf::new()
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let error = extract(&library, "docs/scan.pdf")
            .expect_err("an image-only document must not return an empty success");

        assert!(error.message.contains("scan"), "{}", error.message);
        assert!(error.message.contains("OCR"), "{}", error.message);
    }

    #[test]
    fn a_blank_document_is_reported_as_blank_rather_than_as_a_scan() {
        let tree = TempTree::new("tools-blank");
        tree.write_bytes(
            "docs/blank.pdf",
            &TestPdf::new().page(TestPage::letter()).build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let error = extract(&library, "docs/blank.pdf").expect_err("nothing to extract");

        assert!(error.message.contains("blank"), "{}", error.message);
        assert!(!error.message.contains("OCR"), "{}", error.message);
    }

    #[test]
    fn a_document_that_is_only_partly_scanned_returns_its_text_and_names_the_scanned_pages() {
        let tree = TempTree::new("tools-mixed");
        tree.write_bytes(
            "docs/mixed.pdf",
            &TestPdf::new()
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "readable page"))
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = extract(&library, "docs/mixed.pdf").expect("extracts what it can");

        assert_eq!(response.image_only_pages, vec![2]);
        assert_eq!(response.pages[0].kind, "text");
        assert_eq!(response.pages[1].kind, "image_only");
        assert!(response.pages[1].text.is_empty());
        assert!(
            response.notes.iter().any(|note| note.contains("OCR")),
            "{:?}",
            response.notes
        );
    }

    #[test]
    fn document_info_classifies_every_page_and_says_whether_text_can_be_had() {
        let tree = TempTree::new("tools-info");
        tree.write_bytes(
            "docs/mixed.pdf",
            &TestPdf::new()
                .info("Title", "Mixed document")
                .page(TestPage::letter().text(72.0, 700.0, 10.0, "readable"))
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .page(TestPage::letter())
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .document_info(
                DocumentInfoArgs {
                    path: "docs/mixed.pdf".to_string(),
                },
                Deadline::unlimited(),
            )
            .expect("describes");

        assert_eq!(response.pages, 3);
        assert_eq!(response.title.as_deref(), Some("Mixed document"));
        assert_eq!(response.text_pages, 1);
        assert_eq!(response.image_only_pages, 1);
        assert_eq!(response.empty_pages, 1);
        assert!(response.has_extractable_text);
        assert_eq!(response.page_details.len(), 3);
        assert!((response.page_details[0].width_points - 612.0).abs() < 0.1);
        assert!((response.page_details[0].height_points - 792.0).abs() < 0.1);
        assert!(response.file_bytes > 0);
    }

    #[test]
    fn document_info_on_a_scan_says_extraction_will_produce_nothing() {
        let tree = TempTree::new("tools-info-scan");
        tree.write_bytes(
            "docs/scan.pdf",
            &TestPdf::new()
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .document_info(
                DocumentInfoArgs {
                    path: "docs/scan.pdf".to_string(),
                },
                Deadline::unlimited(),
            )
            .expect("describes");

        assert!(!response.has_extractable_text);
        assert!(
            response.notes.iter().any(|note| note.contains("OCR")),
            "{:?}",
            response.notes
        );
    }

    #[test]
    fn tables_come_back_as_rows_of_cells_with_their_page_number() {
        let tree = TempTree::new("tools-tables");
        let mut page = TestPage::letter();
        for (index, row) in [
            ["Item", "Quantity", "Total"],
            ["Widget", "2", "24.00"],
            ["Gasket", "10", "8.50"],
        ]
        .iter()
        .enumerate()
        {
            let y = 700.0 - 14.0 * index as f32;
            page = page
                .text(72.0, y, 10.0, row[0])
                .text(250.0, y, 10.0, row[1])
                .text(400.0, y, 10.0, row[2]);
        }
        tree.write_bytes("docs/invoice.pdf", &TestPdf::new().page(page).build());
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .extract_tables(
                ExtractTablesArgs {
                    path: "docs/invoice.pdf".to_string(),
                    pages: None,
                    min_rows: None,
                    min_columns: None,
                },
                Deadline::unlimited(),
            )
            .expect("extracts tables");

        assert_eq!(response.tables_found, 1, "{response:?}");
        let table = &response.tables[0];
        assert_eq!(table.page, 1);
        assert_eq!(table.columns, 3);
        assert_eq!(table.cells[0], vec!["Item", "Quantity", "Total"]);
        assert_eq!(table.cells[2], vec!["Gasket", "10", "8.50"]);
    }

    #[test]
    fn a_page_with_no_table_says_so_instead_of_returning_a_bare_empty_list() {
        let tree = TempTree::new("tools-no-tables");
        tree.write_bytes(
            "docs/prose.pdf",
            &TestPdf::new()
                .page(TestPage::letter().paragraph(
                    72.0,
                    700.0,
                    10.0,
                    14.0,
                    &[
                        "A paragraph of ordinary prose that runs on for a while",
                        "and continues onto a second line without any columns",
                        "at all, which is exactly what should not be a table.",
                    ],
                ))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .extract_tables(
                ExtractTablesArgs {
                    path: "docs/prose.pdf".to_string(),
                    pages: None,
                    min_rows: None,
                    min_columns: None,
                },
                Deadline::unlimited(),
            )
            .expect("returns a real answer");

        assert_eq!(response.tables_found, 0);
        assert!(
            response.notes.iter().any(|note| note.contains("alignment")),
            "{:?}",
            response.notes
        );
    }

    #[test]
    fn listing_returns_paths_the_other_tools_accept() {
        let tree = TempTree::new("tools-list");
        tree.write_bytes("docs/reports/q4.pdf", &simple_pdf("hello"));
        let library = library(&tree, &[("docs", "docs")]);

        let listing = library
            .list_documents(
                ListDocumentsArgs {
                    path: None,
                    name_contains: None,
                    limit: None,
                },
                Deadline::unlimited(),
            )
            .expect("lists");

        assert_eq!(listing.count, 1);
        assert_eq!(listing.documents[0].path, "docs/reports/q4.pdf");
        assert_eq!(listing.roots, vec!["docs".to_string()]);
        // The listed path round-trips through extraction.
        assert!(extract(&library, &listing.documents[0].path).is_ok());
    }

    #[test]
    fn a_path_outside_the_roots_is_refused_with_the_rule_that_refused_it() {
        let tree = TempTree::new("tools-escape");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        tree.write_bytes("secrets/payroll.pdf", &simple_pdf("secret"));
        let library = library(&tree, &[("docs", "docs")]);

        let error = extract(&library, "docs/../secrets/payroll.pdf").expect_err("traversal");
        assert!(error.message.contains(".."), "{}", error.message);

        let error = extract(&library, "/etc/passwd").expect_err("absolute");
        assert!(error.message.contains("root label"), "{}", error.message);

        let error = extract(&library, "secrets/payroll.pdf").expect_err("unknown root");
        assert!(error.message.contains("`docs/`"), "{}", error.message);
    }

    #[test]
    fn a_file_that_is_not_a_pdf_is_refused_by_name() {
        let tree = TempTree::new("tools-not-pdf");
        tree.write("docs/notes.pdf", "just some text\n");
        let library = library(&tree, &[("docs", "docs")]);

        let error = extract(&library, "docs/notes.pdf").expect_err("not a pdf");

        assert!(error.message.contains("%PDF-"), "{}", error.message);
    }

    #[test]
    fn a_character_budget_truncates_and_says_so() {
        let tree = TempTree::new("tools-budget");
        tree.write_bytes(
            "docs/long.pdf",
            &TestPdf::new()
                .page(TestPage::letter().paragraph(
                    72.0,
                    700.0,
                    10.0,
                    12.0,
                    &["a line of text that is not particularly short"; 40],
                ))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let response = library
            .extract_text(
                ExtractTextArgs {
                    path: "docs/long.pdf".to_string(),
                    pages: None,
                    layout: None,
                    max_chars: Some(60),
                },
                Deadline::unlimited(),
            )
            .expect("extracts");

        assert!(response.truncated);
        assert!(response.pages[0].characters <= 60, "{response:?}");
    }

    #[test]
    fn the_expired_budget_surfaces_as_an_error_naming_the_setting_that_fixes_it() {
        let tree = TempTree::new("tools-timeout");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        let library = library(&tree, &[("docs", "docs")]);

        let error = library
            .extract_text(
                ExtractTextArgs {
                    path: "docs/q4.pdf".to_string(),
                    pages: None,
                    layout: None,
                    max_chars: None,
                },
                Deadline::expired(),
            )
            .expect_err("no budget left");

        assert!(
            error.message.contains("--timeout-secs"),
            "{}",
            error.message
        );
    }

    #[test]
    fn status_reports_the_labels_and_limits_without_touching_a_file() {
        let tree = TempTree::new("tools-status");
        tree.mkdir("docs");
        tree.mkdir("papers");
        let library = library(&tree, &[("docs", "docs"), ("papers", "papers")]);

        let status = library.status();

        assert_eq!(status.roots, vec!["docs".to_string(), "papers".to_string()]);
        assert_eq!(status.max_pages_per_call, 200);
        assert_eq!(status.timeout_secs, 30);
        assert!(status.ocr.contains("not supported"));
        // No absolute path anywhere in the answer.
        let rendered = serde_json::to_string(&status).expect("serializes");
        assert!(
            !rendered.contains(&tree.path().to_string_lossy().replace('\\', "\\\\")),
            "status must not disclose where the roots live: {rendered}"
        );
    }

    #[test]
    fn a_library_with_no_roots_refuses_every_operation() {
        let library = Library::for_manifest_only();

        let error = extract(&library, "docs/q4.pdf").expect_err("nothing configured");

        assert!(error.message.contains("--root"), "{}", error.message);
    }

    /// The invoice the README shows, and the exact numbers it prints.
    ///
    /// The README's example payload is not typed by hand: it is what this test
    /// asserts, so a change to a field name, a note, or the table detector
    /// fails here before it can quietly make the document wrong.
    #[test]
    fn the_readme_example_is_what_the_tools_actually_return() {
        let tree = TempTree::new("readme");
        let mut invoice = TestPage::letter().text(72.0, 740.0, 14.0, "ACME Supply Co.");
        for (index, row) in [
            ["Item", "Quantity", "Total"],
            ["Widget", "2", "24.00"],
            ["Gasket", "10", "8.50"],
            ["Flange", "1", "119.00"],
        ]
        .iter()
        .enumerate()
        {
            let y = 700.0 - 16.0 * index as f32;
            invoice = invoice
                .text(72.0, y, 10.0, row[0])
                .text(250.0, y, 10.0, row[1])
                .text(400.0, y, 10.0, row[2]);
        }
        // Page two is a scan: the README's example is the mixed document,
        // because that is the case worth showing.
        tree.write_bytes(
            "docs/invoice.pdf",
            &TestPdf::new()
                .info("Title", "Invoice INV-4491")
                .info("Author", "ACME Supply Co.")
                .info("CreationDate", "D:20241102153000+01'00'")
                .page(invoice)
                .page(TestPage::letter().image(0.0, 0.0, 612.0, 792.0))
                .build(),
        );
        let library = library(&tree, &[("docs", "docs")]);

        let text = extract(&library, "docs/invoice.pdf").expect("extracts");
        assert_eq!(text.pages_in_document, 2);
        assert_eq!(text.pages_returned, 2);
        assert_eq!(text.image_only_pages, vec![2]);
        assert!(!text.truncated);
        assert_eq!(text.pages[0].kind, "text");
        assert_eq!(text.pages[0].columns, 1);
        assert_eq!(text.pages[0].characters, 82);
        assert_eq!(
            text.pages[0].text,
            "ACME Supply Co.\n\nItem Quantity Total\nWidget 2 24.00\nGasket 10 8.50\nFlange 1 119.00"
        );
        assert_eq!(text.pages[1].kind, "image_only");
        assert_eq!(text.pages[1].characters, 0);
        assert_eq!(text.notes.len(), 1);
        assert!(text.notes[0].starts_with("1 of the 2 pages read are images"));

        let tables = library
            .extract_tables(
                ExtractTablesArgs {
                    path: "docs/invoice.pdf".to_string(),
                    pages: Some("1".to_string()),
                    min_rows: None,
                    min_columns: None,
                },
                Deadline::unlimited(),
            )
            .expect("extracts tables");
        assert_eq!(tables.tables_found, 1);
        assert_eq!(tables.tables[0].page, 1);
        assert_eq!(tables.tables[0].columns, 3);
        assert_eq!(tables.tables[0].rows, 4);
        assert_eq!(tables.tables[0].occupancy, 1.0);
        assert_eq!(
            tables.tables[0].cells,
            vec![
                vec!["Item", "Quantity", "Total"],
                vec!["Widget", "2", "24.00"],
                vec!["Gasket", "10", "8.50"],
                vec!["Flange", "1", "119.00"],
            ]
        );

        let info = library
            .document_info(
                DocumentInfoArgs {
                    path: "docs/invoice.pdf".to_string(),
                },
                Deadline::unlimited(),
            )
            .expect("describes");
        assert_eq!(info.pdf_version, "1.7");
        assert_eq!(info.title.as_deref(), Some("Invoice INV-4491"));
        assert_eq!(info.created.as_deref(), Some("2024-11-02T15:30:00+01:00"));
        assert_eq!((info.text_pages, info.image_only_pages), (1, 1));
        assert!(info.has_extractable_text);
        assert_eq!(info.page_details[1].images, 1);

        // And the limits the README's `status` example prints.
        let status = library.status();
        assert_eq!(status.max_file_bytes, 33_554_432);
        assert_eq!(status.max_pages_per_call, 200);
        assert_eq!(status.max_chars_per_call, 200_000);
        assert_eq!(status.timeout_secs, 30);
        assert_eq!(status.max_decompressed_bytes, 134_217_728);
    }

    #[test]
    fn the_manifest_declares_exactly_the_five_documented_tools() {
        let manifest = pdf_extract_plugin(Library::for_manifest_only())
            .manifest()
            .expect("declarative plugins have a manifest");

        let mut names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "document_info",
                "extract_tables",
                "extract_text",
                "list_documents",
                "status"
            ],
            "tool names are part of the contract other people write down"
        );

        // Nothing else is contributed: no HTTP surface, no mesh channel, no
        // event subscription, no web UI, and no config schema to render.
        assert!(manifest.http_bindings.is_empty());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.config_schema.is_none());
        assert_eq!(manifest.capabilities, vec!["pdf-extract.v1".to_string()]);
    }

    #[test]
    fn every_tool_carries_a_description_and_an_input_schema_a_model_can_act_on() {
        let manifest = pdf_extract_plugin(Library::for_manifest_only())
            .manifest()
            .expect("manifest");

        for operation in &manifest.operations {
            assert!(
                operation.description.len() > 60,
                "{} needs a description a model can act on",
                operation.name
            );
            assert!(
                operation.input_schema_json.contains("\"object\""),
                "{} has no input schema the host can validate against",
                operation.name
            );
            // `status` takes no arguments, so it is the one tool with no
            // properties to describe.
            if operation.name != "status" {
                assert!(
                    operation.input_schema_json.contains("\"properties\""),
                    "{} declares arguments but describes none",
                    operation.name
                );
            }
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = pdf_extract_plugin(Library::for_manifest_only())
            .manifest()
            .expect("manifest");
        let extract = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "extract_text")
            .expect("extract_text is declared");

        let schema = &extract.input_schema_json;
        assert!(schema.contains("root label"), "{schema}");
        assert!(schema.contains("Pages are numbered from 1"), "{schema}");
        assert!(schema.contains("\"required\""), "{schema}");
    }
}
