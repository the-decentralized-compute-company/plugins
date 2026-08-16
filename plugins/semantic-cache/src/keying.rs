//! Cache keys.
//!
//! The whole correctness story of a semantic cache lives in this file. The
//! rule is: **semantic matching applies to exactly one thing — the trailing
//! user message — and everything else must match exactly.**
//!
//! So a lookup is two stages:
//!
//! 1. Build a *bucket* from every field that changes what the model would say:
//!    the model id, the sampling parameters, the tool set, the entire message
//!    prefix (which is where the system prompt lives), and the embedding model
//!    that produced the vectors. Two requests that disagree on any of these are
//!    in different buckets and can never see each other's answers.
//! 2. Inside one bucket, compare the trailing user message by cosine
//!    similarity.
//!
//! Getting stage 1 wrong is what makes semantic caches dangerous: serving a
//! GPT-4-with-tools answer to a small-model-without-tools request looks like a
//! hit and is simply wrong.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::policy::CANONICAL_KEY_VERSION;

/// One chat message. Text content only.
///
/// Multimodal content parts are deliberately not supported: an image is not
/// covered by a text embedding, so a cache that accepted them would compare
/// two requests on their captions and call that equivalence.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ChatMessage {
    /// Message role, e.g. `system`, `user`, `assistant`, `tool`.
    pub role: String,
    /// Message text. Only string content is supported.
    pub content: String,
}

/// Everything that identifies a request, split into the exact-match part and
/// the semantic part.
#[derive(Clone, Debug)]
pub struct RequestShape {
    /// SHA-256 of the canonical key string. Entries are grouped by this.
    pub bucket: String,
    /// Whitespace-normalized trailing user message: the only text that is ever
    /// compared by meaning.
    pub query: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ShapeError {
    NoMessages,
    LastMessageNotUser(String),
    EmptyQuery,
    NonFiniteSampling(&'static str),
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMessages => formatter.write_str("messages must not be empty"),
            Self::LastMessageNotUser(role) => write!(
                formatter,
                "the last message must have role \"user\" (the cached query), got {role:?}"
            ),
            Self::EmptyQuery => {
                formatter.write_str("the trailing user message has no text to match on")
            }
            Self::NonFiniteSampling(field) => {
                write!(formatter, "{field} must be a finite number")
            }
        }
    }
}

/// The request fields that go into the bucket, before hashing.
#[derive(Clone, Copy, Debug)]
pub struct KeyInputs<'a> {
    /// The completion model id. Different model, different answer.
    pub model: &'a str,
    /// The embedding model id. Vectors from two different embedders are not
    /// comparable, so entries produced under one must never be searched under
    /// another.
    pub embedding_model: &'a str,
    /// `None` is kept distinct from `Some(1.0)`: the server-side default for
    /// an omitted parameter is not knowable from here, and splitting a bucket
    /// costs a miss whereas merging one risks a wrong answer.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Tool definitions exactly as the caller would send them.
    pub tools: &'a [serde_json::Value],
    /// Free-form discriminator for anything else that changes the answer and
    /// is not modelled above — `response_format`, `seed`, a prompt-template
    /// version, a retrieval corpus id.
    pub extra: &'a str,
}

/// Build the bucket digest and the normalized query.
pub fn shape(inputs: KeyInputs<'_>, messages: &[ChatMessage]) -> Result<RequestShape, ShapeError> {
    if let Some(temperature) = inputs.temperature
        && !temperature.is_finite()
    {
        return Err(ShapeError::NonFiniteSampling("temperature"));
    }
    if let Some(top_p) = inputs.top_p
        && !top_p.is_finite()
    {
        return Err(ShapeError::NonFiniteSampling("top_p"));
    }

    let Some((last, prefix)) = messages.split_last() else {
        return Err(ShapeError::NoMessages);
    };
    if !last.role.eq_ignore_ascii_case("user") {
        return Err(ShapeError::LastMessageNotUser(last.role.clone()));
    }
    let query = normalize_query(&last.content);
    if query.is_empty() {
        return Err(ShapeError::EmptyQuery);
    }

    Ok(RequestShape {
        bucket: bucket_digest(inputs, prefix),
        query,
    })
}

/// Light normalization only: trim, and collapse runs of whitespace.
///
/// It is deliberately not lowercasing or stripping punctuation. Those look
/// like free wins on a paraphrase set, but they also erase the difference
/// between `rm -rf /` and `RM -RF /` in a shell-help prompt, and the embedder
/// already handles casing far better than a `to_lowercase()` does. The
/// normalization exists so that a re-indented prompt is an *exact* hit and
/// costs no embedding call — not to do semantic work.
pub fn normalize_query(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// SHA-256 over a canonical, length-prefixed rendering of the exact-match
/// fields.
fn bucket_digest(inputs: KeyInputs<'_>, prefix: &[ChatMessage]) -> String {
    let mut hasher = Sha256::new();
    let mut write = |label: &str, value: &str| {
        // Length prefixes make the encoding injective: without them a model
        // named `a` with system prompt `bc` and a model named `ab` with system
        // prompt `c` would hash the same bytes.
        hasher.update(label.as_bytes());
        hasher.update(b"=");
        hasher.update(value.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(value.as_bytes());
        hasher.update(b";");
    };

    write("v", CANONICAL_KEY_VERSION);
    write("model", inputs.model);
    write("embedder", inputs.embedding_model);
    write("temperature", &canonical_number(inputs.temperature));
    write("top_p", &canonical_number(inputs.top_p));
    write("extra", inputs.extra);
    write("tools", &canonical_tools(inputs.tools));
    write("prefix_len", &prefix.len().to_string());
    for (index, message) in prefix.iter().enumerate() {
        write(&format!("role{index}"), &message.role);
        // Prefix messages are compared verbatim, not normalized: history is an
        // exact-match field, and two conversations that differ only in
        // whitespace may still have been produced by different callers.
        write(&format!("content{index}"), &message.content);
    }

    format!("{:x}", hasher.finalize())
}

/// `{:?}` on `f64` round-trips, so `0.7` and `0.70` render identically while
/// `0.7` and `0.7000001` do not.
fn canonical_number(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:?}"),
        None => "unset".to_string(),
    }
}

/// Serialize each tool and sort the results.
///
/// Sorting is what makes `[read, write]` and `[write, read]` the same tool
/// set — the model sees the same capabilities either way. `serde_json`'s
/// default map is a `BTreeMap`, so object keys within each tool are already
/// emitted in a stable order.
fn canonical_tools(tools: &[serde_json::Value]) -> String {
    let mut rendered: Vec<String> = tools
        .iter()
        .map(|tool| serde_json::to_string(tool).unwrap_or_else(|_| "null".to_string()))
        .collect();
    rendered.sort();
    rendered.join("\u{1f}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn inputs<'a>(model: &'a str, tools: &'a [serde_json::Value]) -> KeyInputs<'a> {
        KeyInputs {
            model,
            embedding_model: "embed-1",
            temperature: Some(0.0),
            top_p: None,
            tools,
            extra: "",
        }
    }

    fn bucket_for(inputs: KeyInputs<'_>, messages: &[ChatMessage]) -> String {
        shape(inputs, messages).expect("valid shape").bucket
    }

    #[test]
    fn the_same_request_lands_in_the_same_bucket() {
        let messages = [
            message("system", "be terse"),
            message("user", "what is 2+2?"),
        ];
        assert_eq!(
            bucket_for(inputs("m", &[]), &messages),
            bucket_for(inputs("m", &[]), &messages)
        );
    }

    #[test]
    fn a_different_model_is_a_different_bucket() {
        let messages = [message("user", "what is 2+2?")];
        assert_ne!(
            bucket_for(inputs("small", &[]), &messages),
            bucket_for(inputs("large", &[]), &messages)
        );
    }

    #[test]
    fn a_different_system_prompt_is_a_different_bucket() {
        let terse = [
            message("system", "be terse"),
            message("user", "explain TLS"),
        ];
        let verbose = [
            message("system", "be verbose"),
            message("user", "explain TLS"),
        ];
        assert_ne!(
            bucket_for(inputs("m", &[]), &terse),
            bucket_for(inputs("m", &[]), &verbose)
        );
    }

    #[test]
    fn a_missing_system_prompt_is_a_different_bucket() {
        let with = [
            message("system", "be terse"),
            message("user", "explain TLS"),
        ];
        let without = [message("user", "explain TLS")];
        assert_ne!(
            bucket_for(inputs("m", &[]), &with),
            bucket_for(inputs("m", &[]), &without)
        );
    }

    #[test]
    fn a_different_temperature_is_a_different_bucket() {
        let messages = [message("user", "pick a colour")];
        let mut cold = inputs("m", &[]);
        cold.temperature = Some(0.0);
        let mut hot = inputs("m", &[]);
        hot.temperature = Some(0.9);
        assert_ne!(bucket_for(cold, &messages), bucket_for(hot, &messages));
    }

    #[test]
    fn an_unset_temperature_is_not_the_same_as_the_openai_default() {
        let messages = [message("user", "pick a colour")];
        let mut unset = inputs("m", &[]);
        unset.temperature = None;
        let mut explicit = inputs("m", &[]);
        explicit.temperature = Some(1.0);
        assert_ne!(
            bucket_for(unset, &messages),
            bucket_for(explicit, &messages),
            "the server-side default is not knowable here, so the buckets stay split"
        );
    }

    #[test]
    fn equal_temperatures_written_differently_are_the_same_bucket() {
        let messages = [message("user", "pick a colour")];
        let mut first = inputs("m", &[]);
        first.temperature = Some(0.70);
        let mut second = inputs("m", &[]);
        second.temperature = Some(0.7);
        assert_eq!(bucket_for(first, &messages), bucket_for(second, &messages));
    }

    #[test]
    fn a_different_top_p_is_a_different_bucket() {
        let messages = [message("user", "pick a colour")];
        let mut narrow = inputs("m", &[]);
        narrow.top_p = Some(0.1);
        let mut wide = inputs("m", &[]);
        wide.top_p = Some(0.9);
        assert_ne!(bucket_for(narrow, &messages), bucket_for(wide, &messages));
    }

    #[test]
    fn a_different_tool_set_is_a_different_bucket() {
        let messages = [message("user", "what is the weather?")];
        let none: [serde_json::Value; 0] = [];
        let weather = [json!({"type": "function", "function": {"name": "get_weather"}})];
        assert_ne!(
            bucket_for(inputs("m", &none), &messages),
            bucket_for(inputs("m", &weather), &messages)
        );
    }

    #[test]
    fn tool_order_does_not_change_the_bucket() {
        let messages = [message("user", "do the thing")];
        let read = json!({"type": "function", "function": {"name": "read"}});
        let write = json!({"type": "function", "function": {"name": "write"}});
        let forward = [read.clone(), write.clone()];
        let backward = [write, read];
        assert_eq!(
            bucket_for(inputs("m", &forward), &messages),
            bucket_for(inputs("m", &backward), &messages),
            "the model is offered the same capabilities either way"
        );
    }

    #[test]
    fn a_different_embedding_model_is_a_different_bucket() {
        let messages = [message("user", "hello")];
        let mut first = inputs("m", &[]);
        first.embedding_model = "embed-1";
        let mut second = inputs("m", &[]);
        second.embedding_model = "embed-2";
        assert_ne!(
            bucket_for(first, &messages),
            bucket_for(second, &messages),
            "vectors from two embedders are not comparable"
        );
    }

    #[test]
    fn conversation_history_is_part_of_the_bucket() {
        // "and the second one?" means nothing without the turn before it.
        let first = [
            message("user", "list the planets"),
            message("assistant", "Mercury, Venus, Earth"),
            message("user", "and the second one?"),
        ];
        let second = [
            message("user", "list the oceans"),
            message("assistant", "Pacific, Atlantic, Indian"),
            message("user", "and the second one?"),
        ];
        assert_ne!(
            bucket_for(inputs("m", &[]), &first),
            bucket_for(inputs("m", &[]), &second)
        );
    }

    #[test]
    fn the_extra_discriminator_splits_buckets() {
        let messages = [message("user", "hello")];
        let mut plain = inputs("m", &[]);
        plain.extra = "";
        let mut structured = inputs("m", &[]);
        structured.extra = r#"{"response_format":"json_object"}"#;
        assert_ne!(
            bucket_for(plain, &messages),
            bucket_for(structured, &messages)
        );
    }

    #[test]
    fn field_boundaries_cannot_be_forged_by_shifting_text() {
        // Without length prefixes these two would hash identical bytes.
        let messages = [message("user", "hello")];
        let mut first = inputs("ab", &[]);
        first.embedding_model = "c";
        let mut second = inputs("a", &[]);
        second.embedding_model = "bc";
        assert_ne!(bucket_for(first, &messages), bucket_for(second, &messages));
    }

    #[test]
    fn queries_are_whitespace_normalized_but_not_case_folded() {
        assert_eq!(normalize_query("  what   is\n\tTLS?  "), "what is TLS?");
        assert_ne!(normalize_query("rm -rf /"), normalize_query("RM -RF /"));
    }

    #[test]
    fn whitespace_only_differences_produce_the_same_query() {
        let compact =
            shape(inputs("m", &[]), &[message("user", "what is TLS?")]).expect("valid shape");
        let padded =
            shape(inputs("m", &[]), &[message("user", "  what  is\nTLS?  ")]).expect("valid shape");
        assert_eq!(compact.query, padded.query);
        assert_eq!(compact.bucket, padded.bucket);
    }

    #[test]
    fn a_conversation_must_end_on_a_user_turn() {
        let error = shape(
            inputs("m", &[]),
            &[message("user", "hi"), message("assistant", "hello")],
        )
        .expect_err("an assistant-terminated conversation has no query");
        assert_eq!(
            error,
            ShapeError::LastMessageNotUser("assistant".to_string())
        );
    }

    #[test]
    fn an_empty_conversation_is_rejected() {
        assert_eq!(
            shape(inputs("m", &[]), &[]).unwrap_err(),
            ShapeError::NoMessages
        );
    }

    #[test]
    fn a_blank_query_is_rejected() {
        assert_eq!(
            shape(inputs("m", &[]), &[message("user", "   \n ")]).unwrap_err(),
            ShapeError::EmptyQuery
        );
    }

    #[test]
    fn non_finite_sampling_parameters_are_rejected() {
        let messages = [message("user", "hello")];
        let mut broken = inputs("m", &[]);
        broken.temperature = Some(f64::NAN);
        assert_eq!(
            shape(broken, &messages).unwrap_err(),
            ShapeError::NonFiniteSampling("temperature")
        );
    }

    #[test]
    fn role_matching_on_the_query_is_case_insensitive() {
        assert!(shape(inputs("m", &[]), &[message("User", "hello")]).is_ok());
    }
}
