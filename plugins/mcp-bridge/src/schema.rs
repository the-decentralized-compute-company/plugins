//! Forwarding an upstream tool's JSON Schema, and what to do when there is not
//! one worth forwarding.
//!
//! **The upstream owns its contract.** A schema that describes the upstream's
//! arguments is passed through byte for byte — not regenerated, not
//! "normalised", not annotated. Rewriting it would mean this plugin quietly
//! disagreeing with the server it is bridging, and the disagreement would only
//! show up as a rejected call.
//!
//! There are three cases where there is nothing to forward, and in each of them
//! the substitute is the same permissive object schema the host would have
//! produced anyway — so no call is lost, and `mcp-bridge.tools` reports which
//! case a given tool fell into:
//!
//! | Upstream `inputSchema` | What is declared | Why |
//! | --- | --- | --- |
//! | `{"type": "object", …}` | that object, verbatim | the normal case |
//! | has `properties`, no `type` | that object, verbatim | the host adds `"type": "object"`; the properties are the contract |
//! | `{}` or missing | `{"type":"object","additionalProperties":true}` | a tool with no described arguments still has to be callable |
//! | `{"type": "array"}` and similar | the same permissive object | MCP requires an object here; forwarding a contradiction helps nobody |
//! | larger than [`MAX_SCHEMA_BYTES`] | the same permissive object | a manifest is held in memory by the host for the life of the node |

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

/// Largest upstream schema that is forwarded. A schema is documentation for a
/// model, and one bigger than this is a bug or an attack rather than a
/// contract.
pub const MAX_SCHEMA_BYTES: usize = 128 * 1_024;

/// What happened to one upstream schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaNote {
    /// Passed through unchanged.
    Forwarded,
    /// Passed through unchanged, but it did not say `"type": "object"`; the
    /// host adds that when it projects the tool.
    ForwardedWithoutType,
    /// The upstream published no schema, so a permissive object is declared.
    ReplacedEmpty,
    /// The upstream published something that is not an object schema.
    ReplacedNotAnObject,
    /// The upstream schema was over [`MAX_SCHEMA_BYTES`].
    ReplacedTooLarge,
}

impl SchemaNote {
    pub fn is_verbatim(&self) -> bool {
        matches!(self, Self::Forwarded | Self::ForwardedWithoutType)
    }

    /// A sentence for the `tools` response, so nobody has to guess why a tool's
    /// arguments look different from the upstream's documentation.
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::Forwarded => "the upstream server's own schema, forwarded unchanged",
            Self::ForwardedWithoutType => {
                "the upstream server's own schema, forwarded unchanged; it omits \
                 \"type\": \"object\", which the host adds when it projects the tool"
            }
            Self::ReplacedEmpty => {
                "the upstream server published no argument schema for this tool, so any JSON \
                 object is accepted and passed through"
            }
            Self::ReplacedNotAnObject => {
                "the upstream server published a schema that is not an object schema, which MCP \
                 requires here, so any JSON object is accepted and passed through"
            }
            Self::ReplacedTooLarge => {
                "the upstream server's schema was larger than mcp-bridge will forward, so any \
                 JSON object is accepted and passed through"
            }
        }
    }
}

/// The schema this plugin declares for one bridged tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDecision {
    /// Goes straight into the manifest's `input_schema_json`.
    pub json: String,
    pub note: SchemaNote,
}

/// The schema declared when an upstream one cannot be used: accept any object
/// and hand it through untouched.
pub fn permissive_schema_json() -> String {
    r#"{"type":"object","additionalProperties":true}"#.to_string()
}

/// Decide what to declare for one upstream tool.
pub fn decide(upstream: &Map<String, Value>) -> SchemaDecision {
    if upstream.is_empty() {
        return SchemaDecision {
            json: permissive_schema_json(),
            note: SchemaNote::ReplacedEmpty,
        };
    }

    let declared_type = upstream.get("type").and_then(Value::as_str);
    let has_properties = upstream.contains_key("properties");

    let note = match (declared_type, has_properties) {
        (Some("object"), _) => SchemaNote::Forwarded,
        (None, true) => SchemaNote::ForwardedWithoutType,
        // `{"$ref": …}` and `{"allOf": […]}` describe an object without saying
        // so inline. There is nothing to disagree with, so forward it.
        (None, false)
            if upstream.contains_key("$ref")
                || upstream.contains_key("allOf")
                || upstream.contains_key("anyOf")
                || upstream.contains_key("oneOf") =>
        {
            SchemaNote::ForwardedWithoutType
        }
        _ => SchemaNote::ReplacedNotAnObject,
    };

    if !note.is_verbatim() {
        return SchemaDecision {
            json: permissive_schema_json(),
            note,
        };
    }

    // `to_string` on a `serde_json` value cannot fail for a value that came out
    // of `serde_json`, which every upstream schema did.
    let json = Value::Object(upstream.clone()).to_string();
    if json.len() > MAX_SCHEMA_BYTES {
        return SchemaDecision {
            json: permissive_schema_json(),
            note: SchemaNote::ReplacedTooLarge,
        };
    }

    SchemaDecision { json, note }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    #[test]
    fn an_ordinary_object_schema_is_forwarded_byte_for_byte() {
        let upstream = object(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute path to read" }
            },
            "required": ["path"],
            "additionalProperties": false
        }));

        let decision = decide(&upstream);

        assert_eq!(decision.note, SchemaNote::Forwarded);
        let round_tripped: Value = serde_json::from_str(&decision.json).expect("valid JSON");
        assert_eq!(round_tripped, Value::Object(upstream));
    }

    /// The upstream owns its contract, including the parts this plugin has
    /// never heard of.
    #[test]
    fn keywords_this_plugin_does_not_understand_survive_untouched() {
        let upstream = object(json!({
            "type": "object",
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "Mode": { "enum": ["fast", "thorough"] } },
            "properties": { "mode": { "$ref": "#/$defs/Mode" } },
            "unevaluatedProperties": false,
            "x-vendor-extension": { "anything": [1, 2, 3] }
        }));

        let decision = decide(&upstream);

        assert!(decision.note.is_verbatim());
        let round_tripped: Value = serde_json::from_str(&decision.json).expect("valid JSON");
        assert_eq!(round_tripped, Value::Object(upstream));
    }

    #[test]
    fn a_schema_with_properties_but_no_type_is_still_forwarded() {
        let upstream = object(json!({ "properties": { "query": { "type": "string" } } }));

        let decision = decide(&upstream);

        assert_eq!(decision.note, SchemaNote::ForwardedWithoutType);
        let round_tripped: Value = serde_json::from_str(&decision.json).expect("valid JSON");
        assert_eq!(round_tripped, Value::Object(upstream));
        assert!(decision.note.explanation().contains("the host adds"));
    }

    #[test]
    fn a_ref_only_schema_is_forwarded_rather_than_thrown_away() {
        for keyword in ["$ref", "allOf", "anyOf", "oneOf"] {
            let upstream = object(json!({ keyword: "#/$defs/Args" }));

            let decision = decide(&upstream);

            assert_eq!(
                decision.note,
                SchemaNote::ForwardedWithoutType,
                "{keyword} should be forwarded"
            );
        }
    }

    #[test]
    fn a_missing_schema_becomes_a_permissive_object_rather_than_an_uncallable_tool() {
        let decision = decide(&Map::new());

        assert_eq!(decision.note, SchemaNote::ReplacedEmpty);
        let parsed: Value = serde_json::from_str(&decision.json).expect("valid JSON");
        assert_eq!(parsed["type"], "object");
        assert_eq!(parsed["additionalProperties"], true);
    }

    #[test]
    fn a_schema_that_is_not_an_object_schema_is_replaced_and_reported() {
        let decision = decide(&object(
            json!({ "type": "array", "items": { "type": "string" } }),
        ));

        assert_eq!(decision.note, SchemaNote::ReplacedNotAnObject);
        assert_eq!(decision.json, permissive_schema_json());
        assert!(decision.note.explanation().contains("MCP"));
    }

    #[test]
    fn an_enormous_schema_is_refused_rather_than_held_for_the_life_of_the_node() {
        let filler = "x".repeat(MAX_SCHEMA_BYTES);
        let upstream = object(json!({
            "type": "object",
            "description": filler,
            "properties": {}
        }));

        let decision = decide(&upstream);

        assert_eq!(decision.note, SchemaNote::ReplacedTooLarge);
        assert_eq!(decision.json, permissive_schema_json());
    }

    #[test]
    fn a_schema_just_under_the_cap_is_still_forwarded() {
        let filler = "x".repeat(MAX_SCHEMA_BYTES / 2);
        let upstream = object(json!({ "type": "object", "description": filler }));

        assert_eq!(decide(&upstream).note, SchemaNote::Forwarded);
    }

    #[test]
    fn every_note_serializes_to_a_distinct_label_and_has_an_explanation() {
        let notes = [
            SchemaNote::Forwarded,
            SchemaNote::ForwardedWithoutType,
            SchemaNote::ReplacedEmpty,
            SchemaNote::ReplacedNotAnObject,
            SchemaNote::ReplacedTooLarge,
        ];
        let labels: std::collections::BTreeSet<String> = notes
            .iter()
            .map(|note| serde_json::to_string(note).expect("serializes"))
            .collect();

        assert_eq!(labels.len(), notes.len());
        assert!(labels.contains("\"forwarded\""), "{labels:?}");
        for note in &notes {
            assert!(note.explanation().len() > 30, "{note:?}");
        }
    }
}
