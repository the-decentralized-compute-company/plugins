//! The tool implementations, and the state they share.
//!
//! Every operation here goes through the same two gates before it touches
//! anything: the path is resolved inside the configured root
//! ([`crate::paths`]), and the file is one the index accepted
//! ([`crate::filters`]). Nothing reaches the filesystem by any other route.
//!
//! Tool arguments live next to the operation they drive. Their doc comments
//! become the `description` fields in the JSON Schema the host advertises, so
//! they are written for the caller, not for a maintainer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{PluginError, PluginResult};

use crate::index::{Index, RefreshReport, SkipCounts};
use crate::options::Options;
use crate::paths::{self, PathError};
use crate::search::{self, MAX_SNIPPET_BYTES};
use crate::tree;

/// Lines returned by `read` when the caller does not name an end line.
pub const DEFAULT_READ_LINES: u32 = 400;
/// Hard ceiling on one `read`, whatever the caller asks for.
pub const MAX_READ_LINES: u32 = 2000;
/// Byte ceiling on one `read`, which bites first on files with long lines.
pub const MAX_READ_BYTES: usize = 192 * 1024;

pub const DEFAULT_TREE_DEPTH: u32 = 3;
pub const MAX_TREE_DEPTH: u32 = 12;
pub const DEFAULT_TREE_ENTRIES: u32 = 400;
pub const MAX_TREE_ENTRIES: u32 = 2000;

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    /// Scan the text of every indexed file.
    Content,
    /// Match declaration names recorded in the index. No file is read unless a
    /// name matches, so this is much cheaper than a content scan.
    Symbol,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to look for. Treated as literal text unless `regex` is true.
    pub query: String,
    /// `content` searches file text; `symbol` searches declaration names such
    /// as functions, structs, and classes. Defaults to `content`.
    #[serde(default)]
    pub kind: Option<SearchKind>,
    /// Interpret `query` as a Rust `regex` crate pattern instead of literal
    /// text. Defaults to false.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Require an exact-case match. Defaults to false.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Restrict the search to matching paths. A pattern without a `/` matches
    /// the file name at any depth (`*.rs`); a pattern with a `/` matches the
    /// whole root-relative path (`src/**/*.rs`).
    #[serde(default)]
    pub path_glob: Option<String>,
    /// Maximum matches to return, 1-200. Defaults to 40.
    #[serde(default)]
    pub max_results: Option<u32>,
    /// Lines of surrounding context to include with each content match, 0-5.
    /// Defaults to 0.
    #[serde(default)]
    pub context_lines: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Root-relative path of the file to read, for example `src/main.rs`.
    /// Absolute paths and `..` segments are refused.
    pub path: String,
    /// First line to return, 1-based. Defaults to 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Last line to return, inclusive. Defaults to 400 lines after
    /// `start_line`, and is capped at 2000 lines or 192 KiB per call.
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TreeArgs {
    /// Root-relative directory to draw. Defaults to the root itself.
    #[serde(default)]
    pub path: Option<String>,
    /// How many path components deep to draw, 1-12. Defaults to 3.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Maximum lines of tree to return, 1-2000. Defaults to 400.
    #[serde(default)]
    pub max_entries: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReindexArgs {
    /// Reread and reparse every file instead of trusting size and mtime. Use
    /// this after an edit that preserved both. Defaults to false.
    #[serde(default)]
    pub force: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tool responses
// ---------------------------------------------------------------------------

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    /// Root-relative path, forward slashes on every platform.
    pub path: String,
    pub line: u32,
    /// `path:line`, ready to quote.
    pub citation: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// The source line was longer than the snippet limit and was windowed.
    #[serde(skip_serializing_if = "is_false")]
    pub line_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub kind: &'static str,
    pub query: String,
    pub files_considered: u64,
    pub files_read: u64,
    pub matches: usize,
    /// True when a cap stopped the search early, so more matches may exist.
    pub truncated: bool,
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadResponse {
    pub path: String,
    /// `path:start-end`, ready to quote.
    pub citation: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    /// True when a cap shortened the range that was asked for. Compare
    /// `end_line` with `total_lines` to see whether more of the file remains.
    pub truncated: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeResponse {
    /// The directory drawn, root-relative. Empty means the root itself.
    pub path: String,
    pub depth: u32,
    pub files: u64,
    pub entries: usize,
    pub truncated: bool,
    pub tree: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    /// Final component of the configured root. The absolute path is written to
    /// the plugin's stderr at startup and is not returned to callers.
    pub root_name: String,
    pub files: u64,
    pub bytes: u64,
    pub lines: u64,
    pub symbols: u64,
    pub skipped: SkipCounts,
    pub last_indexed_unix: Option<u64>,
    pub max_file_bytes: u64,
    pub refresh_secs: u64,
    pub include_hidden: bool,
    pub include_vendored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReindexResponse {
    pub forced: bool,
    pub files: u64,
    pub report: RefreshReport,
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

pub struct Workspace {
    /// Canonical. Every resolved path is proven to sit under this.
    root: PathBuf,
    options: Options,
    index: Mutex<Index>,
}

impl Workspace {
    /// Canonicalize the configured root and take ownership of it.
    ///
    /// The index is built lazily on the first tool call rather than here, so a
    /// large repository cannot delay the control connection past the host's
    /// `connect_timeout_secs`.
    pub fn open(options: Options) -> Result<Arc<Self>> {
        let root = std::fs::canonicalize(&options.root).with_context(|| {
            format!(
                "configured root {} could not be resolved",
                options.root.display()
            )
        })?;
        if !root.is_dir() {
            bail!("configured root {} is not a directory", root.display());
        }
        Ok(Arc::new(Self {
            root,
            options,
            index: Mutex::new(Index::new()),
        }))
    }

    /// A workspace with no root, for `--print-package-manifest`.
    ///
    /// Building the manifest requires a value for the handlers to capture, but
    /// no handler runs on this path. [`Self::ensure_open`] refuses every
    /// operation anyway, so a mistake here fails loudly instead of resolving
    /// paths against the process working directory.
    pub fn for_manifest_only() -> Arc<Self> {
        Arc::new(Self {
            root: PathBuf::new(),
            options: Options {
                root: PathBuf::new(),
                max_file_bytes: 0,
                refresh_secs: 0,
                include_hidden: false,
                include_vendored: false,
            },
            index: Mutex::new(Index::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_open(&self) -> PluginResult<()> {
        if self.root.as_os_str().is_empty() {
            return Err(PluginError::internal(
                "code-context has no configured root; pass --root in [[plugin]].args",
            ));
        }
        Ok(())
    }

    /// A poisoned lock means a handler panicked mid-refresh. The index is a
    /// cache with no cross-field invariant, so recovering it and reindexing is
    /// better than making every later request fail.
    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refresh_if_stale(&self, index: &mut Index) {
        if index.is_stale(self.options.refresh_secs) {
            index.refresh(&self.root, &self.options, false);
        }
    }

    pub fn search(&self, args: SearchArgs) -> PluginResult<SearchResponse> {
        self.ensure_open()?;

        let kind = args.kind.unwrap_or(SearchKind::Content);
        let matcher = search::build_matcher(
            &args.query,
            args.regex.unwrap_or(false),
            args.case_sensitive.unwrap_or(false),
        )
        .map_err(PluginError::invalid_params)?;
        let filter = match args.path_glob.as_deref() {
            Some(pattern) => {
                Some(search::compile_path_filter(pattern).map_err(PluginError::invalid_params)?)
            }
            None => None,
        };
        let max_results = search::clamp_max_results(args.max_results);
        let context_lines = search::clamp_context_lines(args.context_lines);

        // Collect the candidate list, then drop the index lock: the file reads
        // below are the slow part and nothing about them needs the index.
        let candidates: Vec<(String, Vec<crate::symbols::Symbol>)> = {
            let mut index = self.lock();
            self.refresh_if_stale(&mut index);
            index
                .files()
                .filter(|record| {
                    filter
                        .as_ref()
                        .is_none_or(|filter| filter.matches(&record.relative))
                })
                .map(|record| {
                    let symbols = match kind {
                        SearchKind::Symbol => record.symbols.clone(),
                        SearchKind::Content => Vec::new(),
                    };
                    (record.relative.clone(), symbols)
                })
                .collect()
        };

        let files_considered = candidates.len() as u64;
        let (results, files_read, truncated) = match kind {
            SearchKind::Content => {
                self.scan_content(&candidates, &matcher, max_results, context_lines)
            }
            SearchKind::Symbol => self.scan_symbols(&candidates, &matcher, max_results),
        };

        Ok(SearchResponse {
            kind: match kind {
                SearchKind::Content => "content",
                SearchKind::Symbol => "symbol",
            },
            query: args.query,
            files_considered,
            files_read,
            matches: results.len(),
            truncated,
            results,
        })
    }

    fn scan_content(
        &self,
        candidates: &[(String, Vec<crate::symbols::Symbol>)],
        matcher: &regex::Regex,
        max_results: usize,
        context_lines: usize,
    ) -> (Vec<SearchHit>, u64, bool) {
        let mut results: Vec<SearchHit> = Vec::new();
        let mut files_read = 0u64;
        let mut scanned_bytes = 0u64;
        let mut payload_bytes = 0usize;
        let mut truncated = false;

        for (relative, _) in candidates {
            if results.len() >= max_results
                || payload_bytes >= search::SEARCH_RESPONSE_BUDGET_BYTES
                || scanned_bytes >= search::SEARCH_SCAN_BUDGET_BYTES
            {
                truncated = true;
                break;
            }
            // Re-resolve rather than trusting the index: a path recorded a
            // moment ago may have become a symlink since.
            let Some(text) = self.read_indexed_file(relative) else {
                continue;
            };
            files_read += 1;
            scanned_bytes += text.len() as u64;

            let lines: Vec<&str> = text.lines().collect();
            for (offset, line) in lines.iter().enumerate() {
                if results.len() >= max_results
                    || payload_bytes >= search::SEARCH_RESPONSE_BUDGET_BYTES
                {
                    truncated = true;
                    break;
                }
                let Some(found) = matcher.find(line) else {
                    continue;
                };
                let snippet = search::snippet(line, found.start(), MAX_SNIPPET_BYTES);
                payload_bytes += snippet.text.len();
                let line_number = offset as u32 + 1;
                results.push(SearchHit {
                    citation: format!("{relative}:{line_number}"),
                    path: relative.clone(),
                    line: line_number,
                    text: snippet.text,
                    symbol: None,
                    symbol_kind: None,
                    before: context_window(&lines, offset.saturating_sub(context_lines), offset),
                    after: context_window(
                        &lines,
                        offset + 1,
                        (offset + 1 + context_lines).min(lines.len()),
                    ),
                    line_truncated: snippet.truncated,
                });
            }
        }

        (results, files_read, truncated)
    }

    fn scan_symbols(
        &self,
        candidates: &[(String, Vec<crate::symbols::Symbol>)],
        matcher: &regex::Regex,
        max_results: usize,
    ) -> (Vec<SearchHit>, u64, bool) {
        let mut results: Vec<SearchHit> = Vec::new();
        let mut files_read = 0u64;
        let mut truncated = false;

        for (relative, symbols) in candidates {
            if results.len() >= max_results {
                truncated = true;
                break;
            }
            let matched: Vec<&crate::symbols::Symbol> = symbols
                .iter()
                .filter(|symbol| matcher.is_match(&symbol.name))
                .collect();
            if matched.is_empty() {
                continue;
            }

            // One read per file with a hit, so the declaration line can come
            // back with the name. Symbols the file no longer has are dropped.
            let text = self.read_indexed_file(relative);
            if text.is_some() {
                files_read += 1;
            }
            let lines: Vec<&str> = text
                .as_deref()
                .map(|text| text.lines().collect())
                .unwrap_or_default();

            for symbol in matched {
                if results.len() >= max_results {
                    truncated = true;
                    break;
                }
                let declaration = lines
                    .get(symbol.line as usize - 1)
                    .map(|line| search::snippet(line, 0, MAX_SNIPPET_BYTES));
                results.push(SearchHit {
                    citation: format!("{relative}:{}", symbol.line),
                    path: relative.clone(),
                    line: symbol.line,
                    text: declaration
                        .as_ref()
                        .map(|snippet| snippet.text.trim().to_string())
                        .unwrap_or_default(),
                    symbol: Some(symbol.name.clone()),
                    symbol_kind: Some(symbol.kind),
                    before: Vec::new(),
                    after: Vec::new(),
                    line_truncated: declaration.is_some_and(|snippet| snippet.truncated),
                });
            }
        }

        (results, files_read, truncated)
    }

    /// Read a file the index accepted, re-proving containment first.
    ///
    /// Returns `None` for anything that has changed underneath us — moved,
    /// deleted, replaced by a link out of the root, or no longer valid UTF-8.
    /// A search skips it; that is a stale-index miss, not an error worth
    /// failing the whole call for.
    fn read_indexed_file(&self, relative: &str) -> Option<String> {
        let absolute = paths::resolve_within(&self.root, relative).ok()?;
        std::fs::read_to_string(absolute).ok()
    }

    pub fn read(&self, args: ReadArgs) -> PluginResult<ReadResponse> {
        self.ensure_open()?;
        let relative = normalize(&args.path)?;

        let known = {
            let mut index = self.lock();
            self.refresh_if_stale(&mut index);
            index.get(&relative).is_some()
        };
        if !known {
            return Err(PluginError::invalid_params(format!(
                "{relative} is not in the index. It may be gitignored, vendored, binary, larger \
                 than the configured max_file_bytes, excluded by the secret-file policy, or \
                 simply absent. Call the tree tool to see what is available."
            )));
        }

        let absolute = paths::resolve_within(&self.root, &relative)
            .map_err(|error| path_error(&relative, error))?;
        let text = std::fs::read_to_string(&absolute).map_err(|error| {
            PluginError::internal(format!("could not read {relative}: {}", error.kind()))
        })?;

        let slice = slice_lines(&text, args.start_line, args.end_line)
            .map_err(PluginError::invalid_params)?;

        Ok(ReadResponse {
            citation: format!("{relative}:{}-{}", slice.start_line, slice.end_line),
            path: relative,
            start_line: slice.start_line,
            end_line: slice.end_line,
            total_lines: slice.total_lines,
            truncated: slice.truncated,
            content: slice.content,
        })
    }

    pub fn tree(&self, args: TreeArgs) -> PluginResult<TreeResponse> {
        self.ensure_open()?;
        let prefix = normalize(args.path.as_deref().unwrap_or(""))?;
        if !prefix.is_empty() {
            let absolute = paths::resolve_within(&self.root, &prefix)
                .map_err(|error| path_error(&prefix, error))?;
            if !absolute.is_dir() {
                return Err(PluginError::invalid_params(format!(
                    "{prefix} is not a directory"
                )));
            }
        }

        let depth = args
            .depth
            .unwrap_or(DEFAULT_TREE_DEPTH)
            .clamp(1, MAX_TREE_DEPTH);
        let max_entries = args
            .max_entries
            .unwrap_or(DEFAULT_TREE_ENTRIES)
            .clamp(1, MAX_TREE_ENTRIES);

        let relatives: Vec<String> = {
            let mut index = self.lock();
            self.refresh_if_stale(&mut index);
            index
                .files()
                .filter_map(|record| strip_directory_prefix(&record.relative, &prefix))
                .collect()
        };

        let node = tree::build(&relatives, depth as usize);
        let (lines, truncated) = tree::render(&node, max_entries as usize);

        Ok(TreeResponse {
            path: prefix,
            depth,
            files: relatives.len() as u64,
            entries: lines.len(),
            truncated,
            tree: lines.join("\n"),
        })
    }

    pub fn status(&self, _args: StatusArgs) -> PluginResult<StatusResponse> {
        self.ensure_open()?;
        let mut index = self.lock();
        self.refresh_if_stale(&mut index);

        Ok(StatusResponse {
            root_name: self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            files: index.file_count(),
            bytes: index.total_bytes(),
            lines: index.total_lines(),
            symbols: index.symbol_count(),
            skipped: index.skipped(),
            last_indexed_unix: index.last_refresh_unix(),
            max_file_bytes: self.options.max_file_bytes,
            refresh_secs: self.options.refresh_secs,
            include_hidden: self.options.include_hidden,
            include_vendored: self.options.include_vendored,
        })
    }

    pub fn reindex(&self, args: ReindexArgs) -> PluginResult<ReindexResponse> {
        self.ensure_open()?;
        let forced = args.force.unwrap_or(false);
        let mut index = self.lock();
        let report = index.refresh(&self.root, &self.options, forced);
        Ok(ReindexResponse {
            forced,
            files: index.file_count(),
            report,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn path_error(relative: &str, error: PathError) -> PluginError {
    PluginError::invalid_params(format!("{relative}: {error}"))
}

fn normalize(input: &str) -> PluginResult<String> {
    paths::normalize_relative(input)
        .map_err(|error| PluginError::invalid_params(format!("{input:?}: {error}")))
}

/// `Some(remainder)` when `relative` is inside `prefix`, `None` otherwise. An
/// empty prefix means the root, which everything is inside.
fn strip_directory_prefix(relative: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(relative.to_string());
    }
    relative
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
}

fn context_window(lines: &[&str], start: usize, end: usize) -> Vec<String> {
    lines[start.min(lines.len())..end.min(lines.len())]
        .iter()
        .map(|line| search::snippet(line, 0, MAX_SNIPPET_BYTES).text)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub struct LineSlice {
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub truncated: bool,
}

/// Cut a 1-based, inclusive line range out of `text`, applying the read caps.
///
/// Pure so the caps can be tested without a file: the point of this function is
/// that no combination of caller-supplied numbers produces an unbounded reply.
pub fn slice_lines(
    text: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<LineSlice, String> {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len() as u32;

    let start = start_line.unwrap_or(1).max(1);
    if total_lines == 0 {
        return Ok(LineSlice {
            content: String::new(),
            start_line: start,
            end_line: start.saturating_sub(1),
            total_lines: 0,
            truncated: false,
        });
    }
    if start > total_lines {
        return Err(format!(
            "start_line {start} is past the end of the file ({total_lines} lines)"
        ));
    }

    let requested_end = end_line.unwrap_or_else(|| start.saturating_add(DEFAULT_READ_LINES - 1));
    if requested_end < start {
        return Err(format!(
            "end_line {requested_end} is before start_line {start}"
        ));
    }

    let mut truncated = false;
    let mut end = requested_end.min(total_lines);
    if end - start + 1 > MAX_READ_LINES {
        end = start + MAX_READ_LINES - 1;
        truncated = true;
    }

    let mut content = String::new();
    let mut last = start;
    for (offset, line) in lines[(start - 1) as usize..=(end - 1) as usize]
        .iter()
        .enumerate()
    {
        if content.len() + line.len() + 1 > MAX_READ_BYTES && offset > 0 {
            truncated = true;
            break;
        }
        content.push_str(line);
        content.push('\n');
        last = start + offset as u32;
    }

    Ok(LineSlice {
        content,
        start_line: start,
        end_line: last,
        total_lines,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempTree;

    fn workspace(tree: &TempTree) -> Arc<Workspace> {
        Workspace::open(Options {
            root: tree.path().to_path_buf(),
            max_file_bytes: 64 * 1024,
            refresh_secs: 0,
            include_hidden: false,
            include_vendored: false,
        })
        .expect("open workspace")
    }

    fn sample_tree(tag: &str) -> TempTree {
        let tree = TempTree::new(tag);
        tree.write(
            "src/main.rs",
            "use crate::lib;\n\nfn main() {\n    // TODO: wire up the mesh\n    lib::run();\n}\n",
        );
        tree.write(
            "src/lib.rs",
            "pub fn run() -> usize {\n    42\n}\n\npub struct Runner;\n",
        );
        tree.write("README.md", "# Sample\n\nTODO: write docs\n");
        tree.write("secrets/service.pem", "definitely a key\n");
        tree
    }

    #[test]
    fn content_search_returns_citable_paths_and_line_numbers() {
        let tree = sample_tree("workspace-search");
        let workspace = workspace(&tree);

        let response = workspace
            .search(SearchArgs {
                query: "TODO".to_string(),
                kind: None,
                regex: None,
                case_sensitive: Some(true),
                path_glob: None,
                max_results: None,
                context_lines: None,
            })
            .expect("search runs");

        let citations: Vec<&str> = response
            .results
            .iter()
            .map(|hit| hit.citation.as_str())
            .collect();
        assert_eq!(citations, vec!["README.md:3", "src/main.rs:4"]);
        assert!(!response.truncated);
    }

    #[test]
    fn path_glob_narrows_the_search() {
        let tree = sample_tree("workspace-glob");
        let workspace = workspace(&tree);

        let response = workspace
            .search(SearchArgs {
                query: "TODO".to_string(),
                kind: None,
                regex: None,
                case_sensitive: Some(true),
                path_glob: Some("*.rs".to_string()),
                max_results: None,
                context_lines: Some(1),
            })
            .expect("search runs");

        assert_eq!(response.results.len(), 1);
        let hit = &response.results[0];
        assert_eq!(hit.path, "src/main.rs");
        assert_eq!(hit.before, vec!["fn main() {"]);
        assert_eq!(hit.after, vec!["    lib::run();"]);
    }

    #[test]
    fn symbol_search_finds_declarations_without_scanning_every_file() {
        let tree = sample_tree("workspace-symbols");
        let workspace = workspace(&tree);

        let response = workspace
            .search(SearchArgs {
                query: "run".to_string(),
                kind: Some(SearchKind::Symbol),
                regex: None,
                case_sensitive: Some(false),
                path_glob: None,
                max_results: None,
                context_lines: None,
            })
            .expect("search runs");

        let found: Vec<(&str, &str)> = response
            .results
            .iter()
            .map(|hit| {
                (
                    hit.symbol.as_deref().unwrap_or_default(),
                    hit.symbol_kind.unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(found, vec![("run", "fn"), ("Runner", "struct")]);
        assert_eq!(response.results[0].citation, "src/lib.rs:1");
        assert_eq!(response.results[0].text, "pub fn run() -> usize {");
        // Only the one file holding a match was opened.
        assert_eq!(response.files_read, 1);
    }

    #[test]
    fn an_invalid_pattern_is_an_error_rather_than_an_empty_success() {
        let tree = sample_tree("workspace-bad-pattern");
        let workspace = workspace(&tree);

        let error = workspace
            .search(SearchArgs {
                query: "fn (".to_string(),
                kind: None,
                regex: Some(true),
                case_sensitive: None,
                path_glob: None,
                max_results: None,
                context_lines: None,
            })
            .expect_err("malformed regex");
        assert!(error.message.contains("invalid pattern"), "{error}");
    }

    #[test]
    fn read_returns_the_requested_range_with_a_citation() {
        let tree = sample_tree("workspace-read");
        let workspace = workspace(&tree);

        let response = workspace
            .read(ReadArgs {
                path: "src/lib.rs".to_string(),
                start_line: Some(1),
                end_line: Some(2),
            })
            .expect("read runs");

        assert_eq!(response.citation, "src/lib.rs:1-2");
        assert_eq!(response.content, "pub fn run() -> usize {\n    42\n");
        assert_eq!(response.total_lines, 5);
        assert!(!response.truncated);
    }

    #[test]
    fn read_refuses_a_path_outside_the_root_before_it_looks_at_the_disk() {
        let tree = sample_tree("workspace-escape");
        let workspace = workspace(&tree);

        for path in ["../secrets.txt", "/etc/passwd", r"..\..\Windows\win.ini"] {
            let error = workspace
                .read(ReadArgs {
                    path: path.to_string(),
                    start_line: None,
                    end_line: None,
                })
                .expect_err("escape refused");
            assert!(
                error.message.contains("relative") || error.message.contains(".."),
                "path {path}: {error}"
            );
        }
    }

    #[test]
    fn read_refuses_a_file_the_index_excluded() {
        let tree = sample_tree("workspace-secret-read");
        let workspace = workspace(&tree);

        let error = workspace
            .read(ReadArgs {
                path: "secrets/service.pem".to_string(),
                start_line: None,
                end_line: None,
            })
            .expect_err("secret files are not readable");
        assert!(error.message.contains("not in the index"), "{error}");
    }

    #[test]
    fn tree_draws_only_what_the_index_holds() {
        let tree = sample_tree("workspace-tree");
        let workspace = workspace(&tree);

        let response = workspace
            .tree(TreeArgs {
                path: None,
                depth: None,
                max_entries: None,
            })
            .expect("tree runs");

        assert_eq!(response.files, 3);
        assert!(response.tree.contains("src/"), "{}", response.tree);
        assert!(response.tree.contains("main.rs"), "{}", response.tree);
        assert!(!response.tree.contains("service.pem"), "{}", response.tree);
    }

    #[test]
    fn tree_can_be_scoped_to_a_subdirectory() {
        let tree = sample_tree("workspace-subtree");
        let workspace = workspace(&tree);

        let response = workspace
            .tree(TreeArgs {
                path: Some("src".to_string()),
                depth: None,
                max_entries: None,
            })
            .expect("tree runs");

        assert_eq!(response.files, 2);
        assert_eq!(response.tree, "├── lib.rs\n└── main.rs");
    }

    #[test]
    fn status_and_reindex_report_what_happened() {
        let tree = sample_tree("workspace-status");
        let workspace = workspace(&tree);

        let status = workspace.status(StatusArgs {}).expect("status runs");
        assert_eq!(status.files, 3);
        assert!(status.symbols >= 3, "{status:?}");
        assert_eq!(status.skipped.secret, 1);

        let reindex = workspace
            .reindex(ReindexArgs { force: Some(true) })
            .expect("reindex runs");
        assert!(reindex.forced);
        assert_eq!(reindex.files, 3);
        assert_eq!(reindex.report.unchanged, 0);
    }

    #[test]
    fn a_workspace_without_a_root_refuses_every_operation() {
        let workspace = Workspace::for_manifest_only();
        let error = workspace.status(StatusArgs {}).expect_err("no root");
        assert!(error.message.contains("no configured root"), "{error}");
    }

    #[test]
    fn slice_lines_applies_the_default_window() {
        let text: String = (1..=1000).map(|n| format!("line {n}\n")).collect();
        let slice = slice_lines(&text, Some(10), None).expect("slice");
        assert_eq!(slice.start_line, 10);
        assert_eq!(slice.end_line, 10 + DEFAULT_READ_LINES - 1);
        assert_eq!(slice.total_lines, 1000);
        // No cap fired — the window is the documented default, and the caller
        // can see from end_line vs total_lines that the file continues.
        assert!(!slice.truncated);
    }

    #[test]
    fn slice_lines_caps_an_over_long_request() {
        let text: String = (1..=6000).map(|n| format!("line {n}\n")).collect();
        let slice = slice_lines(&text, Some(1), Some(6000)).expect("slice");
        assert_eq!(slice.end_line, MAX_READ_LINES);
        assert!(slice.truncated);
    }

    #[test]
    fn slice_lines_rejects_impossible_ranges() {
        let text = "one\ntwo\nthree\n";
        assert!(slice_lines(text, Some(99), None).is_err());
        assert!(slice_lines(text, Some(3), Some(1)).is_err());
        let whole = slice_lines(text, None, None).expect("whole file");
        assert_eq!(whole.content, text);
        assert_eq!(whole.end_line, 3);
        assert!(!whole.truncated);
    }

    #[test]
    fn slice_lines_handles_an_empty_file() {
        let slice = slice_lines("", None, None).expect("empty file");
        assert_eq!(slice.total_lines, 0);
        assert!(slice.content.is_empty());
    }
}
