//! Matching, path filtering, and the caps that keep a response small enough to
//! be useful.
//!
//! Everything here is a pure function over text so the limits are testable
//! without a filesystem, a host, or a model.

use globset::GlobBuilder;
use regex::{Regex, RegexBuilder};

pub const DEFAULT_MAX_RESULTS: u32 = 40;
pub const MAX_RESULTS_CEILING: u32 = 200;
pub const MAX_CONTEXT_LINES: u32 = 5;

/// A single returned line is trimmed to this. Long enough to read a statement,
/// short enough that 200 of them still fit in a reply.
pub const MAX_SNIPPET_BYTES: usize = 400;
/// Total snippet payload for one search before it reports itself truncated.
pub const SEARCH_RESPONSE_BUDGET_BYTES: usize = 96 * 1024;
/// Total file bytes one content search will read before it stops early.
pub const SEARCH_SCAN_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Ceiling on the compiled size of a caller-supplied pattern.
///
/// The `regex` crate is linear-time, so this is not about catastrophic
/// backtracking — it is about a pattern like `[\s\S]{1,5000}` compiling into
/// hundreds of megabytes of automaton on someone else's machine.
const REGEX_SIZE_LIMIT_BYTES: usize = 1 << 20;

/// Build the matcher for a query.
///
/// A literal query is escaped into a pattern rather than handled separately, so
/// literal and regex searches share one code path and one set of limits.
pub fn build_matcher(query: &str, use_regex: bool, case_sensitive: bool) -> Result<Regex, String> {
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }
    let pattern = if use_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_SIZE_LIMIT_BYTES)
        .build()
        .map_err(|error| format!("invalid pattern: {error}"))
}

/// A compiled `path_glob` argument.
///
/// A pattern without a `/` matches the file name, so `*.rs` means what a
/// caller expects it to mean. A pattern with a `/` matches the whole
/// root-relative path, so `src/**/*.rs` also means what a caller expects.
pub struct PathFilter {
    matcher: globset::GlobMatcher,
    name_only: bool,
}

impl PathFilter {
    pub fn matches(&self, relative_path: &str) -> bool {
        let candidate = if self.name_only {
            relative_path.rsplit('/').next().unwrap_or(relative_path)
        } else {
            relative_path
        };
        self.matcher.is_match(candidate)
    }
}

pub fn compile_path_filter(pattern: &str) -> Result<PathFilter, String> {
    if pattern.is_empty() {
        return Err("path_glob must not be empty".to_string());
    }
    let name_only = !pattern.contains('/');
    let glob = GlobBuilder::new(pattern)
        .literal_separator(!name_only)
        .build()
        .map_err(|error| format!("invalid path_glob: {error}"))?;
    Ok(PathFilter {
        matcher: glob.compile_matcher(),
        name_only,
    })
}

pub fn clamp_max_results(requested: Option<u32>) -> usize {
    requested
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CEILING) as usize
}

pub fn clamp_context_lines(requested: Option<u32>) -> usize {
    requested.unwrap_or(0).min(MAX_CONTEXT_LINES) as usize
}

/// A line trimmed for transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// True when the original line was longer than [`MAX_SNIPPET_BYTES`].
    pub truncated: bool,
}

/// Trim `line` to a window around `match_start`, keeping some of what came
/// before the match so the result reads in context.
///
/// Window edges are moved to UTF-8 character boundaries, so this never panics
/// on a match that lands mid-codepoint.
pub fn snippet(line: &str, match_start: usize, max_bytes: usize) -> Snippet {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() <= max_bytes {
        return Snippet {
            text: line.to_string(),
            truncated: false,
        };
    }

    // Keep a third of the window as lead-in so the match is not flush left.
    let lead_in = max_bytes / 3;
    let start = floor_boundary(line, match_start.saturating_sub(lead_in).min(line.len()));
    let end = ceil_boundary(line, (start + max_bytes).min(line.len()));

    let mut text = String::new();
    if start > 0 {
        text.push('…');
    }
    text.push_str(&line[start..end]);
    if end < line.len() {
        text.push('…');
    }
    Snippet {
        text,
        truncated: true,
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_query_is_escaped_rather_than_compiled() {
        let matcher = build_matcher("a.b(c)", false, true).expect("literal query");
        assert!(matcher.is_match("value = a.b(c)"));
        // Would match if `.` and `(` had been treated as regex syntax.
        assert!(!matcher.is_match("value = axbXcY"));
    }

    #[test]
    fn regex_queries_are_compiled_as_written() {
        let matcher = build_matcher(r"fn\s+resolve_\w+", true, true).expect("regex query");
        assert!(matcher.is_match("pub fn resolve_within(root: &Path)"));
        assert!(!matcher.is_match("pub fn sanitize(root: &Path)"));
    }

    #[test]
    fn matching_is_case_insensitive_unless_asked_otherwise() {
        assert!(
            build_matcher("TODO", false, false)
                .unwrap()
                .is_match("todo!")
        );
        assert!(
            !build_matcher("TODO", false, true)
                .unwrap()
                .is_match("todo!")
        );
    }

    #[test]
    fn an_empty_query_is_rejected_instead_of_matching_everything() {
        assert!(build_matcher("", false, false).is_err());
        assert!(compile_path_filter("").is_err());
    }

    #[test]
    fn a_malformed_pattern_is_an_error_not_a_panic() {
        assert!(build_matcher("fn (", true, false).is_err());
        assert!(compile_path_filter("src/[").is_err());
    }

    #[test]
    fn a_pattern_that_would_compile_to_an_enormous_automaton_is_refused() {
        assert!(build_matcher(r"(?:[\s\S]{1,5000}){1,5000}", true, false).is_err());
    }

    #[test]
    fn a_bare_glob_matches_the_file_name_at_any_depth() {
        let filter = compile_path_filter("*.rs").expect("glob");
        assert!(filter.matches("main.rs"));
        assert!(filter.matches("crates/plugin/src/main.rs"));
        assert!(!filter.matches("crates/plugin/src/main.go"));
    }

    #[test]
    fn a_glob_with_a_slash_matches_the_whole_relative_path() {
        let filter = compile_path_filter("src/**/*.rs").expect("glob");
        assert!(filter.matches("src/index.rs"));
        assert!(filter.matches("src/util/paths.rs"));
        assert!(!filter.matches("tests/util/paths.rs"));
    }

    #[test]
    fn result_and_context_limits_are_clamped_not_trusted() {
        assert_eq!(clamp_max_results(None), DEFAULT_MAX_RESULTS as usize);
        assert_eq!(clamp_max_results(Some(0)), 1);
        assert_eq!(
            clamp_max_results(Some(100_000)),
            MAX_RESULTS_CEILING as usize
        );
        assert_eq!(clamp_context_lines(None), 0);
        assert_eq!(clamp_context_lines(Some(99)), MAX_CONTEXT_LINES as usize);
    }

    #[test]
    fn short_lines_come_back_whole_and_without_line_endings() {
        let result = snippet("let value = 1;\r\n", 4, MAX_SNIPPET_BYTES);
        assert_eq!(result.text, "let value = 1;");
        assert!(!result.truncated);
    }

    #[test]
    fn long_lines_are_windowed_around_the_match() {
        let line = format!("{}NEEDLE{}", "a".repeat(4000), "b".repeat(4000));
        let result = snippet(&line, 4000, 90);

        assert!(result.truncated);
        assert!(result.text.contains("NEEDLE"), "{}", result.text);
        assert!(result.text.starts_with('…') && result.text.ends_with('…'));
        // The `…` markers are 3 bytes each on top of the byte window.
        assert!(result.text.len() <= 90 + 6, "{}", result.text.len());
    }

    #[test]
    fn windowing_never_splits_a_codepoint() {
        // Multi-byte characters either side of the match.
        let line = format!("{}MATCH{}", "é".repeat(500), "ü".repeat(500));
        let result = snippet(&line, 1000, 60);
        assert!(result.text.contains("MATCH"));
        // Constructing the String at all proves the slice was valid UTF-8.
        assert!(result.truncated);
    }
}
