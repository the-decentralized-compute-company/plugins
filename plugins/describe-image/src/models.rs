//! Finding a model on this mesh that can actually look at a picture.
//!
//! # Why this is not a hard-coded name
//!
//! Nobody knows what is loaded on a given node. Contributors bring their own
//! hardware and their own weights, the set changes as peers come and go, and a
//! plugin that shipped `"llava-1.5"` in a constant would be wrong on almost
//! every node in the mesh. So the model is discovered from the node's own
//! `GET /v1/models`, which is the same list the console and every OpenAI client
//! sees.
//!
//! # What the node tells us
//!
//! A TDCC node annotates each entry with what it inferred about the model:
//!
//! ```jsonc
//! { "id": "Qwen3-VL-4B-Instruct", "capabilities": ["text", "multimodal", "vision"],
//!   "vision_status": "supported", "multimodal_status": "supported" }
//! ```
//!
//! `vision_status` is `supported` when the node has hard evidence — a projector
//! file beside the weights, a `vision_config` in the model's `config.json` — and
//! `likely` when it only has a name that reads like a vision model. Both are
//! used here, in that order of preference, and which one was used comes back in
//! the tool result so a caller can see the difference.
//!
//! # When the endpoint is not a TDCC node
//!
//! Point `--api-base` at a bare llama.cpp or vLLM server and `/v1/models`
//! carries an id and nothing else. Refusing outright would be unhelpful, and
//! guessing silently would be dishonest, so: the name heuristic runs **only**
//! when no entry in the whole list carries any capability metadata at all, and
//! the result is labelled `name-heuristic` with a caveat. The name signals are
//! the same ones the host itself uses, so the plugin and the node agree about
//! what "vl" means.

use serde_json::Value;

/// One entry from `GET /v1/models`, reduced to what matters here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub display_name: Option<String>,
    pub capabilities: Vec<String>,
    pub vision_status: Option<String>,
}

impl ModelEntry {
    /// The strings a name heuristic gets to look at.
    fn name_signals(&self) -> Vec<&str> {
        let mut signals = vec![self.id.as_str()];
        if let Some(display) = &self.display_name {
            signals.push(display.as_str());
        }
        signals
    }

    /// Whether the node said anything at all about this model's abilities.
    pub fn carries_metadata(&self) -> bool {
        !self.capabilities.is_empty() || self.vision_status.is_some()
    }

    /// What the node declared about vision, if anything.
    pub fn declared_vision(&self) -> Option<Confidence> {
        match self.vision_status.as_deref() {
            Some("supported") => return Some(Confidence::Declared),
            Some("likely") => return Some(Confidence::DeclaredLikely),
            _ => {}
        }
        if self
            .capabilities
            .iter()
            .any(|capability| capability.eq_ignore_ascii_case("vision"))
        {
            return Some(Confidence::Declared);
        }
        None
    }
}

/// How sure we are that the chosen model can see, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// The node has hard evidence: a projector file, a `vision_config`, or an
    /// explicit `vision` capability.
    Declared,
    /// The node inferred it from the model's name.
    DeclaredLikely,
    /// The endpoint published no capability metadata at all and this plugin
    /// recognised the name. A guess, and reported as one.
    NameHeuristic,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::DeclaredLikely => "declared-likely",
            Self::NameHeuristic => "name-heuristic",
        }
    }

    /// A sentence for the tool result when the choice was not certain.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Self::Declared => None,
            Self::DeclaredLikely => Some(
                "the node inferred this model's vision support from its name rather than from a \
                 projector file or its config, so the request may be rejected or answered from \
                 the prompt text alone",
            ),
            Self::NameHeuristic => Some(
                "this endpoint publishes no capability metadata, so the model was chosen because \
                 its name looks like a vision model's; pin one with --model if that is wrong",
            ),
        }
    }
}

/// The model a call will use, and why it was picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub id: String,
    pub confidence: Option<Confidence>,
    /// `configured`, `declared`, `declared-likely`, or `name-heuristic`.
    pub selected_by: &'static str,
}

/// Parse the body of `GET /v1/models`.
///
/// Tolerant about extra fields and about entries that are not objects, strict
/// about the two things it needs: a `data` array, and an `id` on each entry.
pub fn parse_models(body: &str) -> Result<Vec<ModelEntry>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("the model list is not JSON ({error})"))?;

    let Some(data) = value.get("data").and_then(Value::as_array) else {
        // An error object is the common shape here — a wrong path, an auth
        // failure — and quoting it beats "no data array".
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return Err(format!(
                "the model list endpoint returned an error: {message}"
            ));
        }
        return Err("the model list has no `data` array".to_string());
    };

    let mut entries = Vec::new();
    for entry in data {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.trim().is_empty() {
            continue;
        }
        entries.push(ModelEntry {
            id: id.to_string(),
            display_name: entry
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            capabilities: entry
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            vision_status: entry
                .get("vision_status")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(entries)
}

/// Pick the model a call should use.
///
/// A pinned id must be present in the list. That is deliberate: a typo in
/// `--model` would otherwise surface as an opaque 404 from the inference server
/// on every call, and the operator would have no way to tell that from a model
/// that failed to load.
pub fn select(entries: &[ModelEntry], pinned: Option<&str>) -> Result<Selection, String> {
    if let Some(pinned) = pinned.map(str::trim).filter(|id| !id.is_empty()) {
        let Some(entry) = entries.iter().find(|entry| entry.id == pinned) else {
            return Err(format!(
                "the pinned model `{pinned}` is not being served. {}",
                available_summary(entries)
            ));
        };
        return Ok(Selection {
            id: entry.id.clone(),
            confidence: entry.declared_vision(),
            selected_by: "configured",
        });
    }

    if entries.is_empty() {
        return Err(
            "this node is serving no models at all, so there is nothing to send an image to. \
             Install and start a vision-capable model on the mesh, or point --api-base at an \
             endpoint that serves one."
                .to_string(),
        );
    }

    // Declared evidence first, strongest wins, and among equals the endpoint's
    // own ordering is kept — this plugin has no basis for ranking one node's
    // vision model above another's.
    let best = entries
        .iter()
        .filter_map(|entry| {
            entry
                .declared_vision()
                .map(|confidence| (confidence, entry))
        })
        .min_by_key(|(confidence, _)| *confidence);
    if let Some((confidence, entry)) = best {
        return Ok(Selection {
            id: entry.id.clone(),
            confidence: Some(confidence),
            selected_by: confidence.label(),
        });
    }

    // Nothing declared. If *anything* in the list carried metadata, the node
    // has told us its answer and the answer is no.
    if entries.iter().any(ModelEntry::carries_metadata) {
        return Err(format!(
            "none of the models being served can accept an image. {} Install a vision-capable \
             model (its name usually carries `VL`, `vision`, or `llava`) and make sure its \
             projector file sits beside the weights, or pin one with `--model` / \
             {} if you know it can see.",
            available_summary(entries),
            crate::config::ENV_MODEL
        ));
    }

    if let Some(entry) = entries
        .iter()
        .find(|entry| strong_vision_name_signal(entry.name_signals()))
        .or_else(|| {
            entries
                .iter()
                .find(|entry| likely_vision_name_signal(entry.name_signals()))
        })
    {
        return Ok(Selection {
            id: entry.id.clone(),
            confidence: Some(Confidence::NameHeuristic),
            selected_by: Confidence::NameHeuristic.label(),
        });
    }

    Err(format!(
        "this endpoint publishes no capability metadata and none of its model names look like a \
         vision model's, so there is nothing to send an image to. {} Pin one with `--model` / {} \
         if you know which of them can see.",
        available_summary(entries),
        crate::config::ENV_MODEL
    ))
}

/// The "here is what you do have" half of every failure message.
///
/// Capped so a node serving fifty models does not turn one error into a wall of
/// text in a model's context window.
fn available_summary(entries: &[ModelEntry]) -> String {
    if entries.is_empty() {
        return "No models are being served.".to_string();
    }
    const SHOWN: usize = 12;
    let listed: Vec<String> = entries
        .iter()
        .take(SHOWN)
        .map(|entry| match entry.vision_status.as_deref() {
            Some(status) => format!("{} (vision: {status})", entry.id),
            None => entry.id.clone(),
        })
        .collect();
    let suffix = if entries.len() > SHOWN {
        format!(" and {} more", entries.len() - SHOWN)
    } else {
        String::new()
    };
    format!("Served now: {}{suffix}.", listed.join(", "))
}

/// Names that mean a vision model with near-certainty.
///
/// The same list the host uses in `tdcc-types`, so a node and this plugin never
/// disagree about what a name implies. Only consulted when the endpoint
/// published no metadata of its own.
fn strong_vision_name_signal(values: Vec<&str>) -> bool {
    const NEEDLES: &[&str] = &[
        "vision",
        "qwen3-vl",
        "qwen3_vl",
        "qwen3vl",
        "qwen2-vl",
        "qwen2_vl",
        "qwen2.5-vl",
        "qwen2_5_vl",
        "llava",
        "mllama",
        "paligemma",
        "idefics",
        "molmo",
        "internvl",
        "glm-4v",
        "glm4v",
        "ovis",
        "florence",
    ];
    values.iter().any(|value| {
        let value = value.to_lowercase();
        NEEDLES.iter().any(|needle| value.contains(needle))
    })
}

/// Names that suggest a vision model without settling it.
fn likely_vision_name_signal(values: Vec<&str>) -> bool {
    values.iter().any(|value| {
        let value = value.to_lowercase();
        value.contains("-vl")
            || value.contains("vl-")
            || value.contains("_vl")
            || value.contains("video")
            || value.contains("multimodal")
            || value.contains("image")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, capabilities: &[&str], vision_status: Option<&str>) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            display_name: None,
            capabilities: capabilities.iter().map(|c| (*c).to_string()).collect(),
            vision_status: vision_status.map(str::to_string),
        }
    }

    /// The shape a real TDCC node answers with, trimmed to the fields this
    /// module reads. Cross-checked against
    /// `crates/tdcc-host-runtime/src/network/openai/response/models.rs`.
    const NODE_BODY: &str = r#"{
        "object": "list",
        "data": [
            { "id": "Llama-3.1-8B-Instruct", "display_name": "Llama 3.1 8B Instruct",
              "object": "model", "owned_by": "tdcc", "capabilities": ["text", "reasoning"],
              "multimodal_status": "none", "vision_status": "none",
              "audio_status": "none", "reasoning_status": "likely",
              "metadata": { "context_length": 8192 } },
            { "id": "Qwen3-VL-4B-Instruct", "display_name": "Qwen3 VL 4B Instruct",
              "object": "model", "owned_by": "tdcc",
              "capabilities": ["text", "multimodal", "vision"],
              "multimodal_status": "supported", "vision_status": "supported",
              "audio_status": "none", "reasoning_status": "none" },
            { "id": "mesh", "display_name": "Mesh (MoA)", "object": "model",
              "owned_by": "tdcc", "capabilities": ["text"],
              "multimodal_status": "unsupported", "vision_status": "unsupported",
              "audio_status": "unsupported", "reasoning_status": "unknown" }
        ]
    }"#;

    #[test]
    fn a_real_node_response_parses_into_ids_and_capabilities() {
        let entries = parse_models(NODE_BODY).expect("parses");

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].id, "Qwen3-VL-4B-Instruct");
        assert_eq!(
            entries[1].display_name.as_deref(),
            Some("Qwen3 VL 4B Instruct")
        );
        assert_eq!(entries[1].vision_status.as_deref(), Some("supported"));
        assert!(entries[1].capabilities.contains(&"vision".to_string()));
    }

    #[test]
    fn the_vision_model_is_chosen_out_of_a_real_node_response() {
        let entries = parse_models(NODE_BODY).expect("parses");
        let selection = select(&entries, None).expect("one model can see");

        assert_eq!(selection.id, "Qwen3-VL-4B-Instruct");
        assert_eq!(selection.selected_by, "declared");
        assert_eq!(selection.confidence, Some(Confidence::Declared));
        assert!(selection.confidence.expect("set").caveat().is_none());
    }

    #[test]
    fn the_virtual_mesh_model_is_never_mistaken_for_a_vision_model() {
        // It advertises `vision_status: "unsupported"`, which is neither
        // "supported" nor "likely" and must not be read as either.
        let entries = parse_models(NODE_BODY).expect("parses");
        let mesh = entries
            .iter()
            .find(|entry| entry.id == "mesh")
            .expect("present");
        assert_eq!(mesh.declared_vision(), None);
    }

    #[test]
    fn a_declared_capability_wins_over_a_merely_likely_one() {
        let entries = vec![
            entry("some-vl-model", &["text"], Some("likely")),
            entry("qwen3-vl-8b", &["text", "vision"], Some("supported")),
        ];

        let selection = select(&entries, None).expect("selects");
        assert_eq!(selection.id, "qwen3-vl-8b");
        assert_eq!(selection.selected_by, "declared");
    }

    #[test]
    fn a_likely_model_is_used_when_it_is_all_there_is_and_the_caveat_is_returned() {
        let entries = vec![
            entry("llama-3-8b", &["text"], Some("none")),
            entry("some-vl-model", &["text"], Some("likely")),
        ];

        let selection = select(&entries, None).expect("selects");
        assert_eq!(selection.id, "some-vl-model");
        assert_eq!(selection.selected_by, "declared-likely");
        assert!(
            selection
                .confidence
                .expect("set")
                .caveat()
                .expect("a guess has a caveat")
                .contains("inferred")
        );
    }

    #[test]
    fn a_vision_capability_without_a_status_field_still_counts() {
        let entries = vec![entry("some-model", &["text", "vision"], None)];
        let selection = select(&entries, None).expect("selects");
        assert_eq!(selection.selected_by, "declared");
    }

    #[test]
    fn a_node_that_says_no_model_can_see_produces_an_error_naming_what_it_does_serve() {
        let entries = vec![
            entry("llama-3-8b", &["text"], Some("none")),
            entry("mistral-7b", &["text"], Some("none")),
        ];

        let error = select(&entries, None).expect_err("nothing can see");
        assert!(error.contains("llama-3-8b"), "{error}");
        assert!(error.contains("--model"), "{error}");
        assert!(error.contains("projector"), "{error}");
    }

    #[test]
    fn an_empty_mesh_says_so_rather_than_failing_obscurely_later() {
        let error = select(&[], None).expect_err("no models at all");
        assert!(error.contains("no models"), "{error}");
    }

    #[test]
    fn a_plain_openai_server_falls_back_to_the_name_heuristic_and_admits_it() {
        // No `capabilities`, no `*_status` — a bare llama.cpp or vLLM server.
        let body = r#"{"object":"list","data":[
            {"id":"gpt-3.5-turbo","object":"model"},
            {"id":"llava-v1.6-mistral-7b","object":"model"}
        ]}"#;
        let entries = parse_models(body).expect("parses");

        let selection = select(&entries, None).expect("the name is recognised");
        assert_eq!(selection.id, "llava-v1.6-mistral-7b");
        assert_eq!(selection.selected_by, "name-heuristic");
        assert!(
            selection
                .confidence
                .expect("set")
                .caveat()
                .expect("a guess has a caveat")
                .contains("no capability metadata")
        );
    }

    #[test]
    fn the_name_heuristic_never_runs_when_the_node_published_an_answer() {
        // The id says "vl" but the node says vision is `none`. The node wins:
        // it looked at the weights, and this plugin only looked at a string.
        let entries = vec![entry("something-vl-flavoured", &["text"], Some("none"))];

        let error = select(&entries, None).expect_err("the node's answer is authoritative");
        assert!(error.contains("none of the models"), "{error}");
    }

    #[test]
    fn a_bare_server_with_no_vision_shaped_name_is_an_honest_failure() {
        let body = r#"{"data":[{"id":"gpt-3.5-turbo"},{"id":"mistral-7b"}]}"#;
        let entries = parse_models(body).expect("parses");

        let error = select(&entries, None).expect_err("nothing looks like a vision model");
        assert!(error.contains("no capability metadata"), "{error}");
        assert!(error.contains("gpt-3.5-turbo"), "{error}");
    }

    #[test]
    fn a_pinned_model_is_used_and_reported_as_configured() {
        let entries = parse_models(NODE_BODY).expect("parses");
        let selection = select(&entries, Some("Llama-3.1-8B-Instruct")).expect("pinned");

        assert_eq!(selection.id, "Llama-3.1-8B-Instruct");
        assert_eq!(selection.selected_by, "configured");
        // Reported honestly: the operator pinned a model the node says is
        // text-only, and the tool result will say so rather than pretend.
        assert_eq!(selection.confidence, None);
    }

    #[test]
    fn a_pinned_model_that_is_not_served_fails_with_the_list_rather_than_a_404_later() {
        let entries = parse_models(NODE_BODY).expect("parses");
        let error = select(&entries, Some("Qwen3-VL-4B-Instrukt")).expect_err("typo");

        assert!(error.contains("is not being served"), "{error}");
        assert!(error.contains("Qwen3-VL-4B-Instruct"), "{error}");
    }

    #[test]
    fn the_available_list_in_an_error_is_bounded() {
        let entries: Vec<ModelEntry> = (0..40)
            .map(|index| entry(&format!("model-{index}"), &["text"], Some("none")))
            .collect();

        let error = select(&entries, None).expect_err("nothing can see");
        assert!(error.contains("and 28 more"), "{error}");
        assert!(error.len() < 1_200, "an error is not a document: {error}");
    }

    #[test]
    fn a_malformed_model_list_says_what_is_wrong() {
        assert!(
            parse_models("not json")
                .expect_err("must fail")
                .contains("not JSON")
        );
        assert!(
            parse_models(r#"{"object":"list"}"#)
                .expect_err("must fail")
                .contains("`data` array")
        );
    }

    #[test]
    fn an_error_body_from_the_endpoint_is_quoted_back() {
        let body = r#"{"error":{"message":"invalid api key","type":"auth"}}"#;
        let error = parse_models(body).expect_err("must fail");
        assert!(error.contains("invalid api key"), "{error}");
    }

    #[test]
    fn entries_without_a_usable_id_are_skipped_rather_than_failing_the_whole_list() {
        let body = r#"{"data":[{"object":"model"},{"id":"  "},{"id":"real-model"}]}"#;
        let entries = parse_models(body).expect("parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "real-model");
    }

    #[test]
    fn the_name_signals_match_the_hosts_own() {
        // Cross-checked against `strong_vision_name_signal` /
        // `likely_vision_name_signal` in tdcc-types.
        assert!(strong_vision_name_signal(vec![
            "Qwen3-VL-2B-Instruct-Q4_K_M"
        ]));
        assert!(strong_vision_name_signal(vec!["llava-v1.6"]));
        assert!(strong_vision_name_signal(vec!["InternVL2-8B"]));
        assert!(!strong_vision_name_signal(vec!["mistral-7b"]));

        assert!(likely_vision_name_signal(vec!["some-vl-thing"]));
        assert!(likely_vision_name_signal(vec!["a-multimodal-model"]));
        assert!(!likely_vision_name_signal(vec!["mistral-7b"]));
    }
}
