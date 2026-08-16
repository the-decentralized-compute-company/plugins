//! Path allowlist matching and path-template expansion.
//!
//! Two small pure functions carry most of this plugin's confinement:
//!
//! * [`matches`] decides whether a concrete request path is inside one of the
//!   patterns the operator wrote. It runs on the path of the URL that is about
//!   to be sent, not on anything a caller typed, so a parameter value cannot
//!   change what it sees.
//! * [`expand`] turns `/repos/{owner}/{repo}/issues` into a concrete path,
//!   refusing any value that would add a segment or a dot segment.
//!
//! Nothing here touches the network, the filesystem, or the clock.

use std::collections::BTreeMap;

/// Longest path this plugin will construct. A cap here means a caller cannot
/// grow a request line without bound by supplying enormous parameter values,
/// independent of the per-parameter length limits an operator may have set.
pub const MAX_PATH_LEN: usize = 2_048;

/// Does `path` fall inside `pattern`?
///
/// Both are `/`-separated. Within one segment, `*` matches any run of
/// characters other than `/`; a whole segment of `**` matches zero or more
/// segments. Everything else is literal and case-sensitive, because URL paths
/// are.
///
/// ```text
/// /repos/*/*/issues     matches /repos/rust-lang/rust/issues
///                       not     /repos/rust-lang/rust/issues/1
/// /repos/**             matches both
/// /v1/models*           matches /v1/models and /v1/models_beta
/// ```
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<&str> = split_segments(pattern);
    let path_segments: Vec<&str> = split_segments(path);
    match_segments(&pattern_segments, &path_segments)
}

/// True when `path` is inside any of `patterns`.
pub fn matches_any(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|pattern| matches(pattern, path))
}

fn split_segments(value: &str) -> Vec<&str> {
    value.trim_start_matches('/').split('/').collect()
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty() || path == [""],
        Some((&"**", rest)) => {
            // `**` is greedy-but-checked: try consuming 0, 1, 2 … segments.
            (0..=path.len()).any(|taken| match_segments(rest, &path[taken..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) if match_segment(head, first) => match_segments(rest, tail),
            _ => false,
        },
    }
}

/// Match one segment, where `*` stands for any run of non-`/` characters.
fn match_segment(pattern: &str, segment: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == segment;
    }

    let mut rest = segment;
    // A leading literal must be at the very start; a trailing one at the very
    // end. Everything between may float.
    if let Some(first) = parts.first()
        && !first.is_empty()
    {
        match rest.strip_prefix(first) {
            Some(remainder) => rest = remainder,
            None => return false,
        }
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
    {
        match rest.strip_suffix(last) {
            Some(remainder) => rest = remainder,
            None => return false,
        }
    }
    for middle in &parts[1..parts.len().saturating_sub(1)] {
        if middle.is_empty() {
            continue;
        }
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

/// The `{placeholder}` names in a path template, in the order they appear.
///
/// A `{` that is never closed, or a `}` with no `{`, is a malformed template
/// and produces an error rather than a silently literal brace.
pub fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("path template {template:?} has an unclosed `{{`"))?;
        let name = &after[..close];
        if name.is_empty() || name.contains('{') || name.contains('/') {
            return Err(format!(
                "path template {template:?} has an invalid placeholder `{{{name}}}`"
            ));
        }
        names.push(name.to_string());
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("path template {template:?} has an unmatched `}}`"));
    }
    Ok(names)
}

/// Substitute `{name}` placeholders with percent-encoded values.
///
/// Every value is checked before it is encoded, and the checks are the point:
/// a value may not be empty, may not contain `/`, and may not be `.` or `..`.
/// Percent-encoding alone would not save us there — `..` contains only
/// unreserved characters, so it survives encoding intact and would be resolved
/// as a parent directory by any URL parser downstream.
pub fn expand(template: &str, values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("path template {template:?} has an unclosed `{{`"))?;
        let name = &after[..close];
        let value = values
            .get(name)
            .ok_or_else(|| format!("path template {template:?} has no value for `{name}`"))?;
        out.push_str(&encode_path_segment(name, value)?);
        rest = &after[close + 1..];
    }
    out.push_str(rest);

    if out.len() > MAX_PATH_LEN {
        return Err(format!(
            "the request path would be {} characters, over the {MAX_PATH_LEN}-character limit",
            out.len()
        ));
    }
    Ok(out)
}

/// Percent-encode one path parameter, rejecting anything that would change the
/// shape of the path rather than fill a hole in it.
fn encode_path_segment(name: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!(
            "path parameter `{name}` is empty; a path parameter has to fill its segment"
        ));
    }
    if value == "." || value == ".." {
        return Err(format!(
            "path parameter `{name}` is `{value}`, which is a relative path segment. Values that \
             navigate the path are refused."
        ));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(format!(
            "path parameter `{name}` contains a path separator. A parameter fills one segment; it \
             cannot add another."
        ));
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(format!(
            "path parameter `{name}` contains a control character."
        ));
    }
    Ok(percent_encode(value))
}

/// Percent-encode everything outside RFC 3986's unreserved set.
///
/// Deliberately stricter than a URL library's path encoder: `?`, `#`, `&`,
/// `;`, `:`, `@`, and `%` are all encoded, so a parameter cannot start a query
/// string, add userinfo, or smuggle a second percent-escape.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Reject a path that has already been assembled but is not in a shape we are
/// willing to send: no dot segments, no empty segments, no control characters.
pub fn check_assembled_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("assembled path {path:?} does not start with `/`"));
    }
    if path.contains("//") {
        return Err(format!("assembled path {path:?} has an empty segment"));
    }
    for segment in path.split('/') {
        if segment == "." || segment == ".." {
            return Err(format!("assembled path {path:?} has a `{segment}` segment"));
        }
    }
    if path.chars().any(|character| character.is_control()) {
        return Err(format!("assembled path {path:?} has a control character"));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(format!(
            "assembled path is {} characters, over the {MAX_PATH_LEN}-character limit",
            path.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_literal_pattern_matches_only_itself() {
        assert!(matches("/v1/models", "/v1/models"));
        assert!(!matches("/v1/models", "/v1/models/gpt"));
        assert!(!matches("/v1/models", "/v1"));
        assert!(!matches("/v1/models", "/V1/models"));
    }

    #[test]
    fn a_star_covers_one_segment_and_no_more() {
        assert!(matches("/repos/*/*/issues", "/repos/rust-lang/rust/issues"));
        assert!(!matches(
            "/repos/*/*/issues",
            "/repos/rust-lang/rust/issues/42"
        ));
        assert!(!matches("/repos/*/*/issues", "/repos/rust-lang/issues"));
    }

    #[test]
    fn a_star_may_be_combined_with_literals_inside_a_segment() {
        assert!(matches("/v1/models*", "/v1/models"));
        assert!(matches("/v1/models*", "/v1/models_beta"));
        assert!(matches("/files/*.json", "/files/report.json"));
        assert!(!matches("/files/*.json", "/files/report.yaml"));
        assert!(!matches("/files/*.json", "/files/a/b.json"));
    }

    #[test]
    fn a_double_star_segment_spans_any_number_of_segments() {
        assert!(matches("/repos/**", "/repos"));
        assert!(matches("/repos/**", "/repos/a"));
        assert!(matches("/repos/**", "/repos/a/b/c/d"));
        assert!(!matches("/repos/**", "/orgs/a"));
    }

    #[test]
    fn a_traversal_shaped_path_does_not_match_a_narrow_pattern() {
        // The dot-segment check in `check_assembled_path` is the real guard;
        // this pins that the matcher does not quietly help either.
        assert!(!matches("/repos/*/*/issues", "/repos/a/../../admin/issues"));
        assert!(check_assembled_path("/repos/a/../admin").is_err());
    }

    #[test]
    fn placeholders_are_listed_in_order_and_malformed_templates_are_refused() {
        assert_eq!(
            placeholders("/repos/{owner}/{repo}/issues").unwrap(),
            vec!["owner".to_string(), "repo".to_string()]
        );
        assert!(placeholders("/repos").unwrap().is_empty());
        assert!(placeholders("/repos/{owner").is_err());
        assert!(placeholders("/repos/owner}").is_err());
        assert!(placeholders("/repos/{}").is_err());
        assert!(placeholders("/repos/{a/b}").is_err());
    }

    #[test]
    fn expansion_percent_encodes_and_keeps_the_shape_of_the_template() {
        let expanded = expand(
            "/repos/{owner}/{repo}/issues",
            &values(&[("owner", "rust lang"), ("repo", "a+b")]),
        )
        .expect("ordinary values expand");

        assert_eq!(expanded, "/repos/rust%20lang/a%2Bb/issues");
    }

    #[test]
    fn a_parameter_cannot_add_a_segment_or_walk_upwards() {
        for bad in ["a/b", "..", ".", "a\\b", "", "a\nb"] {
            let error = expand("/repos/{owner}/x", &values(&[("owner", bad)]))
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(error.contains("owner"), "{error}");
        }
    }

    #[test]
    fn a_parameter_cannot_start_a_query_string_or_a_fragment() {
        let expanded = expand("/search/{term}", &values(&[("term", "a?b#c&d=e")]))
            .expect("the value is legal, it is just encoded");

        assert_eq!(expanded, "/search/a%3Fb%23c%26d%3De");
        assert!(!expanded.contains('?'));
        assert!(!expanded.contains('#'));
    }

    #[test]
    fn an_already_encoded_value_is_encoded_again_rather_than_passed_through() {
        // `%2F` decoded by a lenient server would be a `/`. Encoding the `%`
        // means the server sees the literal three characters the caller sent.
        let expanded = expand("/x/{id}", &values(&[("id", "a%2Fb")])).expect("the value is legal");
        assert_eq!(expanded, "/x/a%252Fb");
    }

    #[test]
    fn an_over_long_path_is_refused() {
        let error = expand("/x/{id}", &values(&[("id", &"a".repeat(MAX_PATH_LEN))]))
            .expect_err("over the cap");
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn assembled_paths_are_checked_for_shape() {
        assert!(check_assembled_path("/v1/models").is_ok());
        assert!(check_assembled_path("v1/models").is_err());
        assert!(check_assembled_path("/v1//models").is_err());
        assert!(check_assembled_path("/v1/./models").is_err());
        assert!(check_assembled_path("/v1/../models").is_err());
        assert!(check_assembled_path("/v1/mo\rdels").is_err());
    }

    #[test]
    fn percent_encoding_leaves_only_the_unreserved_set_alone() {
        assert_eq!(percent_encode("aZ0-._~"), "aZ0-._~");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
