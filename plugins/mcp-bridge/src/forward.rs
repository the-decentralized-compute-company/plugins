//! The pure half of forwarding one call: bounding the answer, and stamping it
//! with where it came from.
//!
//! The result itself is passed through — content blocks, `structuredContent`,
//! and `isError` all reach the caller exactly as the upstream server produced
//! them. A tool that reformatted its upstream's answers would be a second,
//! undocumented contract sitting between a model and the server it thinks it is
//! talking to.
//!
//! Two things are added, and both are additive:
//!
//! * a **size bound**, because an upstream can return as many bytes as it
//!   likes and this node holds them in memory on the way through; and
//! * a **`_meta` stamp** naming the server and the upstream tool, so a caller
//!   reading a transcript can tell which of several bridged servers answered
//!   without reverse-engineering the tool name.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::Value;

/// `_meta` key carrying the operator's alias for the server that answered.
pub const META_SERVER: &str = "tdcc.mcp-bridge/server";
/// `_meta` key carrying the tool name as the upstream server spells it.
pub const META_TOOL: &str = "tdcc.mcp-bridge/tool";

/// Turn the arguments the host handed this plugin into MCP call arguments.
///
/// The arguments are **not** validated against the upstream's schema here. The
/// upstream owns that contract and enforces it; a second, approximate copy of
/// somebody else's validation is a source of disagreement, not of safety. What
/// this does enforce is the one thing MCP itself requires: tool arguments are a
/// JSON object or nothing at all.
pub fn arguments_from_value(value: &Value) -> Result<Option<JsonObject>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(map) if map.is_empty() => Ok(None),
        Value::Object(map) => Ok(Some(map.clone())),
        other => Err(format!(
            "tool arguments must be a JSON object, not {}",
            match other {
                Value::Array(_) => "an array",
                Value::String(_) => "a string",
                Value::Number(_) => "a number",
                Value::Bool(_) => "a boolean",
                _ => "that",
            }
        )),
    }
}

/// Measure a result the way it will be sent onward.
pub fn result_size_bytes(result: &CallToolResult) -> usize {
    serde_json::to_vec(result)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

/// Refuse a result that is over the server's `max_result_bytes`.
///
/// Refusing rather than truncating is deliberate: a silently shortened file, or
/// a JSON document cut in half, is worse than an error, because the caller has
/// no way to tell it happened. The message names the setting that raises the
/// bound.
pub fn check_result_size(
    result: &CallToolResult,
    alias: &str,
    upstream_tool: &str,
    limit: usize,
) -> Result<usize, String> {
    let size = result_size_bytes(result);
    if size > limit {
        return Err(format!(
            "MCP server '{alias}' returned {size} bytes from its '{upstream_tool}' tool, over the \
             {limit}-byte limit for this server. The result was discarded rather than truncated, \
             because a silently shortened answer is worse than a refused one. Raise \
             max_result_bytes for this server in the mcp-bridge server list if the tool really \
             does return this much."
        ));
    }
    Ok(size)
}

/// Stamp a forwarded result with the server and upstream tool that produced it.
///
/// The two keys are namespaced under `tdcc.mcp-bridge/` and are written
/// unconditionally: an upstream server does not get to claim it is a different
/// server by setting them itself.
pub fn stamp_provenance(
    mut result: CallToolResult,
    alias: &str,
    upstream_tool: &str,
) -> CallToolResult {
    let mut meta = result.meta.take().unwrap_or_default();
    meta.0
        .insert(META_SERVER.to_string(), Value::String(alias.to_string()));
    meta.0.insert(
        META_TOOL.to_string(),
        Value::String(upstream_tool.to_string()),
    );
    result.meta = Some(meta);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Content, Meta};
    use serde_json::json;

    fn text_result(body: &str) -> CallToolResult {
        CallToolResult::success(vec![Content::text(body)])
    }

    #[test]
    fn arguments_reach_the_upstream_exactly_as_they_arrived() {
        let value = json!({ "path": "/etc/hosts", "nested": { "deep": [1, 2, 3] } });

        let arguments = arguments_from_value(&value)
            .expect("an object is valid")
            .expect("a non-empty object is passed through");

        assert_eq!(Value::Object(arguments), value);
    }

    #[test]
    fn no_arguments_and_an_empty_object_both_become_nothing() {
        assert_eq!(arguments_from_value(&Value::Null), Ok(None));
        assert_eq!(arguments_from_value(&json!({})), Ok(None));
    }

    #[test]
    fn arguments_that_are_not_an_object_are_refused_by_shape() {
        for value in [json!([1, 2]), json!("text"), json!(7), json!(true)] {
            let error = arguments_from_value(&value).expect_err("only objects are arguments");
            assert!(error.contains("must be a JSON object"), "{error}");
        }
    }

    /// This plugin does not re-implement the upstream's validation: a field the
    /// upstream's schema has never heard of is still forwarded, and the
    /// upstream is the thing that rejects it.
    #[test]
    fn unknown_fields_are_forwarded_rather_than_second_guessed() {
        let value = json!({ "definitely_not_in_any_schema": true });

        let arguments = arguments_from_value(&value)
            .expect("valid")
            .expect("present");

        assert!(arguments.contains_key("definitely_not_in_any_schema"));
    }

    #[test]
    fn a_result_within_the_bound_passes_and_reports_its_size() {
        let result = text_result("hello");

        let size = check_result_size(&result, "files", "read_file", 1_000_000)
            .expect("a short result is within any sane bound");

        assert!(size > 0);
        assert_eq!(size, result_size_bytes(&result));
    }

    #[test]
    fn an_oversized_result_is_refused_and_names_the_setting_that_raises_the_bound() {
        let result = text_result(&"x".repeat(5_000));

        let error = check_result_size(&result, "files", "read_file", 1_024)
            .expect_err("an oversized result is refused");

        assert!(error.contains("files"), "{error}");
        assert!(error.contains("read_file"), "{error}");
        assert!(error.contains("max_result_bytes"), "{error}");
        assert!(error.contains("1024"), "{error}");
    }

    #[test]
    fn an_oversized_result_is_refused_rather_than_truncated() {
        let result = text_result(&"x".repeat(5_000));
        let error = check_result_size(&result, "files", "read_file", 1_024).unwrap_err();

        // The message says so, because a caller that reads only the error must
        // still learn that nothing was silently cut.
        assert!(error.contains("rather than truncated"), "{error}");
    }

    #[test]
    fn the_content_of_a_forwarded_result_is_not_touched() {
        let mut original = CallToolResult::success(vec![Content::text("body")]);
        original.structured_content = Some(json!({ "rows": [1, 2, 3] }));
        original.is_error = Some(false);

        let stamped = stamp_provenance(original.clone(), "files", "read_file");

        assert_eq!(stamped.content.len(), original.content.len());
        assert_eq!(stamped.structured_content, original.structured_content);
        assert_eq!(stamped.is_error, original.is_error);
    }

    #[test]
    fn an_upstream_error_result_stays_an_error_result() {
        let original = CallToolResult::error(vec![Content::text("no such file")]);

        let stamped = stamp_provenance(original, "files", "read_file");

        assert_eq!(stamped.is_error, Some(true));
    }

    #[test]
    fn the_stamp_says_which_server_answered() {
        let stamped = stamp_provenance(text_result("body"), "files", "read_file");

        let meta = stamped.meta.expect("a stamp was added");
        assert_eq!(meta.0.get(META_SERVER), Some(&json!("files")));
        assert_eq!(meta.0.get(META_TOOL), Some(&json!("read_file")));
    }

    #[test]
    fn a_server_cannot_claim_to_be_a_different_server() {
        let mut original = text_result("body");
        let mut meta = Meta::new();
        meta.0
            .insert(META_SERVER.to_string(), json!("some-other-server"));
        meta.0.insert("upstreamKey".to_string(), json!("kept"));
        original.meta = Some(meta);

        let stamped = stamp_provenance(original, "files", "read_file");

        let meta = stamped.meta.expect("a stamp was added");
        assert_eq!(meta.0.get(META_SERVER), Some(&json!("files")));
        // The upstream's own metadata is preserved beside it.
        assert_eq!(meta.0.get("upstreamKey"), Some(&json!("kept")));
    }
}
