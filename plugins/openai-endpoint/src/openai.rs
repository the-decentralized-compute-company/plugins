//! The parts of "OpenAI-compatible" that are not compatible in practice.
//!
//! Every function here is pure: it takes bytes or JSON that some server
//! produced and reports what the host and a client will make of it. Nothing in
//! this module performs I/O, so the whole compatibility matrix is unit-tested
//! without a running backend.
//!
//! Two of these functions deliberately mirror host code rather than improve on
//! it — [`models_probe_url`] mirrors the host's endpoint health probe and
//! [`forward_path`] mirrors its request path mapping. If they drift, the
//! diagnostics lie, so their tests use the host's own cases.

use serde::Serialize;
use serde_json::Value;
use url::Url;

/// Finish reasons OpenAI itself emits. Anything else is a dialect.
const CANONICAL_FINISH_REASONS: [&str; 5] = [
    "stop",
    "length",
    "tool_calls",
    "content_filter",
    "function_call",
];

// ---------------------------------------------------------------------------
// URL and path handling, mirrored from the host
// ---------------------------------------------------------------------------

/// The exact URL the host's endpoint health probe will request.
///
/// Mirrors `endpoint_models_url` in the host's plugin health module. Knowing
/// this precisely is the whole point of the `health` tool: the endpoint becomes
/// routable only if *this* URL answers 2xx to an unauthenticated GET.
pub fn models_probe_url(address: &Url) -> Url {
    let mut url = address.clone();
    let mut path = url.path().trim_end_matches('/').to_string();
    if path.is_empty() {
        path = "/v1".into();
    }
    if !path.ends_with("/models") {
        if path.ends_with("/v1") || path.ends_with("/api/v1") {
            path.push_str("/models");
        } else {
            path.push_str("/v1/models");
        }
    }
    url.set_path(&path);
    url.set_query(None);
    url
}

/// The upstream path a caller's request path is rewritten to.
///
/// Mirrors `endpoint_forward_path` in the host's external-endpoint relay.
/// `base_path` is the endpoint address's path with no trailing slash.
pub fn forward_path(base_path: &str, request_path: &str) -> String {
    let (path_only, query) = request_path
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((request_path, None));
    let base_path = base_path.trim_end_matches('/');
    let mapped = if base_path.is_empty() || base_path == "/" {
        path_only.to_string()
    } else if let Some(suffix) = path_only.strip_prefix("/v1") {
        if base_path.ends_with("/v1") {
            format!("{base_path}{suffix}")
        } else {
            format!("{base_path}/v1{suffix}")
        }
    } else if let Some(suffix) = path_only.strip_prefix("/models") {
        format!("{base_path}{suffix}")
    } else {
        format!("{base_path}{path_only}")
    };
    match query {
        Some(query) if !query.is_empty() => format!("{mapped}?{query}"),
        _ => mapped,
    }
}

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

/// What a `/v1/models` body actually contained.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelsReport {
    /// Ids the host will see, in response order. Routing matches a request's
    /// `model` against these exactly.
    pub ids: Vec<String>,
    /// Ids that appeared more than once. Harmless for routing, but a sign the
    /// server is merging catalogs.
    pub duplicate_ids: Vec<String>,
    /// Whether the top-level `data` array — the only place the host looks —
    /// was present.
    pub data_array_present: bool,
    /// Entries in `data` that carried no string `id`, so the host skips them.
    pub entries_without_id: usize,
    /// A recognised non-OpenAI list key, when `data` was absent. Ollama's
    /// native `/api/tags` uses `models`, for instance.
    pub alternate_list_key: Option<String>,
}

/// Read a `/v1/models` body the way the host reads it: `data[].id`, nothing
/// else. Deliberately without a fallback — inventing one here would make the
/// plugin report models the host will never route to.
pub fn read_models(body: &Value) -> ModelsReport {
    let mut report = ModelsReport::default();

    let Some(entries) = body.get("data").and_then(Value::as_array) else {
        report.alternate_list_key = ["models", "results", "items"]
            .into_iter()
            .find(|key| body.get(*key).and_then(Value::as_array).is_some())
            .map(str::to_string);
        return report;
    };

    report.data_array_present = true;
    for entry in entries {
        match entry.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => {
                if report.ids.iter().any(|seen| seen == id) {
                    if !report.duplicate_ids.iter().any(|seen| seen == id) {
                        report.duplicate_ids.push(id.to_string());
                    }
                } else {
                    report.ids.push(id.to_string());
                }
            }
            _ => report.entries_without_id += 1,
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the server names its events. Chat completion
    /// streams leave this unset; `/v1/responses`-style streams do not.
    pub event: Option<String>,
    /// Concatenated `data:` field values, newline-joined per the SSE spec.
    pub data: String,
}

impl SseEvent {
    /// The terminator OpenAI clients wait for.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }

    /// Parse the payload as JSON. `[DONE]` and comment frames are not JSON, so
    /// this returns `None` rather than an error.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(self.data.trim()).ok()
    }
}

/// Incremental SSE parser.
///
/// Fed one network read at a time, so it has to tolerate an event split across
/// reads — which is exactly the case that matters, because a stream that
/// *never* splits is the buffered failure mode this plugin exists to detect.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    keepalives: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Comment frames (`: ping`) seen so far. Several servers send these to
    /// hold a connection open, and a stream of nothing but keepalives is a
    /// distinct diagnosis from a stream that produced no bytes at all.
    pub fn keepalives(&self) -> usize {
        self.keepalives
    }

    /// Feed one read; get back every event that completed within it.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        loop {
            // A trailing lone '\r' may be the first half of a "\r\n" that the
            // next read completes. Never treat it as a boundary yet.
            let searchable_len = if self.buffer.ends_with('\r') {
                self.buffer.len() - 1
            } else {
                self.buffer.len()
            };
            let Some((end, separator)) = find_event_boundary(&self.buffer[..searchable_len]) else {
                break;
            };
            let raw = self.buffer[..end].to_string();
            self.buffer.drain(..end + separator);
            match parse_event(&raw) {
                Some(event) => events.push(event),
                None => self.keepalives += 1,
            }
        }
        events
    }

    /// Flush a final event that the server ended without a blank line. Some
    /// servers close the connection straight after the last frame.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let raw = std::mem::take(&mut self.buffer);
        if raw.trim().is_empty() {
            return None;
        }
        match parse_event(&raw) {
            Some(event) => Some(event),
            None => {
                self.keepalives += 1;
                None
            }
        }
    }
}

/// Earliest event separator, as `(offset, separator length)`.
fn find_event_boundary(text: &str) -> Option<(usize, usize)> {
    ["\r\n\r\n", "\n\n", "\r\r"]
        .into_iter()
        .filter_map(|separator| text.find(separator).map(|at| (at, separator.len())))
        .min_by_key(|(at, _)| *at)
}

/// Turn one raw frame into an event, or `None` if it carried no `data:` field.
fn parse_event(raw: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in raw.split(['\r', '\n']).filter(|line| !line.is_empty()) {
        // A line starting with ':' is a comment; servers use it for keepalives.
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        // The spec strips exactly one leading space from the value.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => data.push(value),
            "event" => event = Some(value.to_string()),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

// ---------------------------------------------------------------------------
// Streaming verdict
// ---------------------------------------------------------------------------

/// Whether tokens actually arrived progressively.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamVerdict {
    /// Multiple events across multiple network reads. This is what the chat
    /// surface needs.
    Incremental,
    /// Every event arrived in a single read: the server, or something between
    /// it and here, buffered the whole response. A client sees a long pause and
    /// then the entire answer at once.
    Buffered,
    /// One event only — too short to tell streaming from buffering. Ask for
    /// more tokens and run it again.
    SingleEvent,
    /// No SSE events at all: not a stream, or the request failed.
    NoEvents,
}

/// Classify a stream from its shape rather than its timing.
///
/// Timing thresholds are flaky on a loaded machine; the number of distinct
/// network reads is not. A server that streams produces many reads, and one
/// that buffers produces one, regardless of how fast the box is.
pub fn classify_stream(reads: usize, events: usize) -> StreamVerdict {
    match (reads, events) {
        (_, 0) => StreamVerdict::NoEvents,
        (_, 1) => StreamVerdict::SingleEvent,
        (0..=1, _) => StreamVerdict::Buffered,
        _ => StreamVerdict::Incremental,
    }
}

// ---------------------------------------------------------------------------
// Finish reasons
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct FinishReasonReport {
    /// Exactly what the server sent. The host does not rewrite this on the
    /// success path, so this is also what a client receives.
    pub raw: String,
    /// The closest OpenAI value, for operators comparing backends.
    pub normalized: String,
    /// Whether `raw` was already an OpenAI value.
    pub canonical: bool,
}

/// Map a server's finish reason onto the OpenAI set.
///
/// This is a *report*, not a rewrite: the host byte-relays successful
/// responses, so a client still sees `raw`. Knowing the mapping is what lets an
/// operator decide whether their client can cope.
pub fn normalize_finish_reason(raw: &str) -> FinishReasonReport {
    let trimmed = raw.trim();
    let canonical = CANONICAL_FINISH_REASONS.contains(&trimmed);
    let normalized = match trimmed.to_ascii_lowercase().as_str() {
        // llama.cpp, TGI, and several bridges end turns with their own names.
        "eos_token" | "end_turn" | "stop_sequence" | "complete" | "completed" | "finished" => {
            "stop"
        }
        "max_tokens" | "length_capped" | "token_limit" => "length",
        "tool_use" | "tool_call" => "tool_calls",
        "content_filtered" | "safety" => "content_filter",
        _ if canonical => trimmed,
        _ => "unknown",
    };
    FinishReasonReport {
        raw: trimmed.to_string(),
        normalized: normalized.to_string(),
        canonical,
    }
}

// ---------------------------------------------------------------------------
// Usage accounting
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageReport {
    /// Whether a `usage` object was present at all.
    pub present: bool,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// `total_tokens` was absent and has been added up here.
    pub total_derived: bool,
    /// Non-OpenAI key names that had to be read instead, e.g. the
    /// `input_tokens` / `output_tokens` pair some bridges emit.
    pub alternate_keys: Vec<String>,
}

impl UsageReport {
    /// Whether all three OpenAI counters could be produced.
    pub fn complete(&self) -> bool {
        self.prompt_tokens.is_some() && self.completion_tokens.is_some()
    }
}

/// Read a `usage` object, tolerating the two key spellings seen in the wild.
///
/// A missing `usage` is normal on a *stream* unless the caller asked for
/// `stream_options: {"include_usage": true}`, and many servers ignore that
/// option entirely; on a non-streaming response it means the server does not
/// account tokens at all.
pub fn normalize_usage(usage: Option<&Value>) -> UsageReport {
    let mut report = UsageReport::default();
    let Some(usage) = usage.filter(|value| value.is_object()) else {
        return report;
    };
    report.present = true;

    let mut read = |canonical: &str, alternate: &str| -> Option<u64> {
        if let Some(value) = usage.get(canonical).and_then(Value::as_u64) {
            return Some(value);
        }
        let value = usage.get(alternate).and_then(Value::as_u64)?;
        report.alternate_keys.push(alternate.to_string());
        Some(value)
    };

    report.prompt_tokens = read("prompt_tokens", "input_tokens");
    report.completion_tokens = read("completion_tokens", "output_tokens");
    report.total_tokens = usage.get("total_tokens").and_then(Value::as_u64);

    if report.total_tokens.is_none()
        && let (Some(prompt), Some(completion)) = (report.prompt_tokens, report.completion_tokens)
    {
        report.total_tokens = Some(prompt + completion);
        report.total_derived = true;
    }

    report
}

// ---------------------------------------------------------------------------
// Error shapes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorShape {
    /// `{"error": {"message": "…", "type": "…"}}` — OpenAI's own shape.
    OpenAiObject,
    /// `{"error": {"message": "…"}}` with no `type`. Close, but the host still
    /// rewrites it.
    OpenAiObjectWithoutType,
    /// `{"error": "…"}` — a bare string, as older llama.cpp builds send.
    OpenAiStringError,
    /// `{"detail": …}` — FastAPI's default, which vLLM and TGI inherit.
    FastApiDetail,
    /// `{"message": "…"}` at the top level.
    BareMessage,
    /// Valid JSON in none of the shapes above.
    UnknownJson,
    /// Not JSON at all — an HTML error page from a reverse proxy, usually.
    PlainText,
    /// No body.
    Empty,
}

impl ErrorShape {
    /// Stable name for prose. Matches the value this serializes to, so a note
    /// and the JSON beside it never disagree.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiObject => "open_ai_object",
            Self::OpenAiObjectWithoutType => "open_ai_object_without_type",
            Self::OpenAiStringError => "open_ai_string_error",
            Self::FastApiDetail => "fast_api_detail",
            Self::BareMessage => "bare_message",
            Self::UnknownJson => "unknown_json",
            Self::PlainText => "plain_text",
            Self::Empty => "empty",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NormalizedError {
    pub status: u16,
    pub shape: ErrorShape,
    /// Best available human-readable message, extracted the way the host
    /// extracts it.
    pub message: String,
    pub error_type: Option<String>,
    pub code: Option<String>,
    /// Whether the host rewrites this body into OpenAI's error shape before the
    /// client sees it. The host rewrites any non-2xx body that is not already
    /// `{"error":{"message","type"}}`.
    pub rewritten_by_host: bool,
}

/// Classify an upstream error body and predict what the host will do with it.
///
/// The host *does* normalise error bodies — but only for non-2xx responses, and
/// only when the body is not already OpenAI-shaped. Predicting the outcome here
/// is what turns "my client got a weird error" into a one-line answer.
pub fn normalize_error(status: u16, body: &str) -> NormalizedError {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return NormalizedError {
            status,
            shape: ErrorShape::Empty,
            message: String::new(),
            error_type: None,
            code: None,
            rewritten_by_host: status >= 400,
        };
    }

    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return NormalizedError {
            status,
            shape: ErrorShape::PlainText,
            message: truncate(trimmed),
            error_type: None,
            code: None,
            rewritten_by_host: status >= 400,
        };
    };

    let (shape, message, error_type, code) = classify_error_json(&value);
    // Mirrors the host's `already_openai_error`: an `error` object carrying
    // both a string `message` and a string `type` passes through untouched.
    let already_openai = shape == ErrorShape::OpenAiObject;
    NormalizedError {
        status,
        shape,
        message,
        error_type,
        code,
        rewritten_by_host: status >= 400 && !already_openai,
    }
}

fn classify_error_json(value: &Value) -> (ErrorShape, String, Option<String>, Option<String>) {
    if let Some(error) = value.get("error").and_then(Value::as_object) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(truncate)
            .unwrap_or_default();
        let error_type = error
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        let code = error.get("code").and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| Some(code.to_string()))
        });
        let shape = if error_type.is_some() && !message.is_empty() {
            ErrorShape::OpenAiObject
        } else {
            ErrorShape::OpenAiObjectWithoutType
        };
        return (shape, message, error_type, code);
    }

    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return (ErrorShape::OpenAiStringError, truncate(message), None, None);
    }

    if let Some(detail) = value.get("detail") {
        let message = detail
            .as_str()
            .map(truncate)
            .unwrap_or_else(|| truncate(&detail.to_string()));
        return (ErrorShape::FastApiDetail, message, None, None);
    }

    if let Some(message) = value.get("message").and_then(Value::as_str) {
        let error_type = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        return (ErrorShape::BareMessage, truncate(message), error_type, None);
    }

    (
        ErrorShape::UnknownJson,
        truncate(&value.to_string()),
        None,
        None,
    )
}

/// Error bodies can be an entire HTML page. Keep tool output readable.
fn truncate(text: &str) -> String {
    const MAX: usize = 400;
    let text = text.trim();
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{head}… (truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test URL parses")
    }

    // -- mirrored host behaviour ------------------------------------------
    // These cases come from the host's own tests. They exist to catch drift:
    // if the host changes, these fail and the diagnostics get fixed with it.

    #[test]
    fn models_probe_url_matches_the_hosts_derivation() {
        assert_eq!(
            models_probe_url(&url("http://localhost:8000/v1")).as_str(),
            "http://localhost:8000/v1/models"
        );
        assert_eq!(
            models_probe_url(&url("http://localhost:8000/api/v1")).as_str(),
            "http://localhost:8000/api/v1/models"
        );
        assert_eq!(
            models_probe_url(&url("http://localhost:11434")).as_str(),
            "http://localhost:11434/v1/models"
        );
        // An address that already names /models is left alone.
        assert_eq!(
            models_probe_url(&url("http://localhost:8000/v1/models")).as_str(),
            "http://localhost:8000/v1/models"
        );
        // An unconventional base gets /v1/models appended, not substituted.
        assert_eq!(
            models_probe_url(&url("http://localhost:8000/openai")).as_str(),
            "http://localhost:8000/openai/v1/models"
        );
    }

    #[test]
    fn forward_path_matches_the_hosts_mapping() {
        assert_eq!(
            forward_path("/api/v1", "/v1/chat/completions?stream=true"),
            "/api/v1/chat/completions?stream=true"
        );
        assert_eq!(
            forward_path("/v1", "/v1/chat/completions"),
            "/v1/chat/completions"
        );
        // Root-mounted API: the caller's path is used unchanged.
        assert_eq!(
            forward_path("", "/v1/chat/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(forward_path("/", "/v1/models"), "/v1/models");
        // A `/models` request has its own prefix *replaced* by the base rather
        // than appended to it, so a bare `/models` maps to the base itself.
        // That is what the host does; mirroring it faithfully matters more here
        // than mapping it the way one might expect. In practice clients send
        // `/v1/models`, which takes the branch above.
        assert_eq!(forward_path("/api/v1", "/models"), "/api/v1");
        assert_eq!(forward_path("/api/v1", "/models/qwen"), "/api/v1/qwen");
    }

    // -- model discovery ---------------------------------------------------

    #[test]
    fn models_are_read_from_data_ids_in_order() {
        let report = read_models(&json!({
            "object": "list",
            "data": [{"id": "qwen3-8b"}, {"id": "llama3-70b"}]
        }));
        assert_eq!(report.ids, vec!["qwen3-8b", "llama3-70b"]);
        assert!(report.data_array_present);
        assert_eq!(report.entries_without_id, 0);
        assert!(report.alternate_list_key.is_none());
    }

    #[test]
    fn entries_without_a_usable_id_are_counted_not_guessed() {
        let report = read_models(&json!({
            "data": [{"id": "ok"}, {"name": "no-id"}, {"id": ""}, {"id": 7}]
        }));
        assert_eq!(report.ids, vec!["ok"]);
        assert_eq!(report.entries_without_id, 3);
    }

    #[test]
    fn duplicate_ids_are_reported_once_each() {
        let report = read_models(&json!({
            "data": [{"id": "a"}, {"id": "a"}, {"id": "a"}, {"id": "b"}]
        }));
        assert_eq!(report.ids, vec!["a", "b"]);
        assert_eq!(report.duplicate_ids, vec!["a"]);
    }

    #[test]
    fn a_non_openai_list_shape_yields_no_ids_but_names_the_key() {
        // Ollama's native /api/tags shape. The host reads only `data[].id`, so
        // reporting zero models here is the truthful answer.
        let report = read_models(&json!({"models": [{"name": "llama3:8b"}]}));
        assert!(report.ids.is_empty());
        assert!(!report.data_array_present);
        assert_eq!(report.alternate_list_key.as_deref(), Some("models"));
    }

    // -- SSE decoding ------------------------------------------------------

    #[test]
    fn events_split_across_reads_are_reassembled() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: {\"a\":").is_empty());
        assert!(decoder.push("1}").is_empty());
        let events = decoder.push("\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[0].json(), Some(json!({"a": 1})));
    }

    #[test]
    fn a_carriage_return_split_across_reads_is_not_a_boundary() {
        let mut decoder = SseDecoder::new();
        // The read ends mid-CRLF; treating the lone '\r' as a terminator would
        // emit a truncated event.
        assert!(decoder.push("data: one\r").is_empty());
        let events = decoder.push("\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one");
    }

    #[test]
    fn several_events_in_one_read_are_all_returned() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push("data: a\n\ndata: b\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 3);
        assert!(events[2].is_done());
    }

    #[test]
    fn comments_are_counted_as_keepalives_not_events() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(": ping\n\n: ping\n\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
        assert_eq!(decoder.keepalives(), 2);
    }

    #[test]
    fn named_events_and_multiline_data_follow_the_spec() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push("event: response.delta\ndata: line one\ndata: line two\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("response.delta"));
        // Multiple data fields join with a newline, and exactly one leading
        // space is stripped from each.
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn a_final_event_without_a_blank_line_is_flushed_by_finish() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: last").is_empty());
        let flushed = decoder.finish().expect("trailing event is flushed");
        assert_eq!(flushed.data, "last");
        assert!(decoder.finish().is_none());
    }

    // -- streaming verdict -------------------------------------------------

    #[test]
    fn streaming_is_judged_by_read_count_not_by_timing() {
        assert_eq!(classify_stream(12, 11), StreamVerdict::Incremental);
        // Everything in one read: the failure mode this plugin exists to catch.
        assert_eq!(classify_stream(1, 11), StreamVerdict::Buffered);
        assert_eq!(classify_stream(4, 1), StreamVerdict::SingleEvent);
        assert_eq!(classify_stream(3, 0), StreamVerdict::NoEvents);
        assert_eq!(classify_stream(0, 0), StreamVerdict::NoEvents);
    }

    // -- finish reasons ----------------------------------------------------

    #[test]
    fn canonical_finish_reasons_are_reported_as_canonical() {
        for raw in [
            "stop",
            "length",
            "tool_calls",
            "content_filter",
            "function_call",
        ] {
            let report = normalize_finish_reason(raw);
            assert!(report.canonical, "{raw}");
            assert_eq!(report.normalized, raw);
        }
    }

    #[test]
    fn dialect_finish_reasons_map_onto_the_openai_set() {
        let cases = [
            ("eos_token", "stop"),
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("tool_use", "tool_calls"),
        ];
        for (raw, expected) in cases {
            let report = normalize_finish_reason(raw);
            assert_eq!(report.normalized, expected, "{raw}");
            assert!(!report.canonical, "{raw} is not an OpenAI value");
            // The raw value is preserved: the host does not rewrite it, so a
            // client still sees it.
            assert_eq!(report.raw, raw);
        }
    }

    #[test]
    fn an_unrecognised_finish_reason_is_flagged_rather_than_guessed() {
        let report = normalize_finish_reason("banana");
        assert_eq!(report.normalized, "unknown");
        assert!(!report.canonical);
        assert_eq!(report.raw, "banana");
    }

    // -- usage -------------------------------------------------------------

    #[test]
    fn missing_usage_is_reported_as_absent() {
        let report = normalize_usage(None);
        assert!(!report.present);
        assert!(!report.complete());
        assert!(report.total_tokens.is_none());
    }

    #[test]
    fn a_complete_usage_object_is_read_verbatim() {
        let usage = json!({"prompt_tokens": 11, "completion_tokens": 5, "total_tokens": 16});
        let report = normalize_usage(Some(&usage));
        assert!(report.present && report.complete());
        assert_eq!(report.total_tokens, Some(16));
        assert!(!report.total_derived);
        assert!(report.alternate_keys.is_empty());
    }

    #[test]
    fn a_missing_total_is_derived_and_flagged() {
        let usage = json!({"prompt_tokens": 11, "completion_tokens": 5});
        let report = normalize_usage(Some(&usage));
        assert_eq!(report.total_tokens, Some(16));
        assert!(report.total_derived);
    }

    #[test]
    fn alternate_token_key_names_are_read_and_named() {
        let usage = json!({"input_tokens": 3, "output_tokens": 4});
        let report = normalize_usage(Some(&usage));
        assert_eq!(report.prompt_tokens, Some(3));
        assert_eq!(report.completion_tokens, Some(4));
        assert_eq!(report.total_tokens, Some(7));
        assert_eq!(report.alternate_keys, vec!["input_tokens", "output_tokens"]);
    }

    #[test]
    fn a_null_usage_field_is_treated_as_absent() {
        let usage = json!(null);
        assert!(!normalize_usage(Some(&usage)).present);
    }

    // -- error shapes ------------------------------------------------------

    #[test]
    fn a_full_openai_error_passes_through_the_host_unchanged() {
        let body = r#"{"error":{"message":"bad request","type":"invalid_request_error","code":"invalid_value"}}"#;
        let error = normalize_error(400, body);
        assert_eq!(error.shape, ErrorShape::OpenAiObject);
        assert_eq!(error.message, "bad request");
        assert_eq!(error.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(error.code.as_deref(), Some("invalid_value"));
        assert!(!error.rewritten_by_host);
    }

    #[test]
    fn an_error_object_without_a_type_is_rewritten_by_the_host() {
        let error = normalize_error(404, r#"{"error":{"message":"model missing"}}"#);
        assert_eq!(error.shape, ErrorShape::OpenAiObjectWithoutType);
        assert!(error.rewritten_by_host);
    }

    #[test]
    fn a_bare_string_error_is_recognised() {
        let error = normalize_error(500, r#"{"error":"context is full"}"#);
        assert_eq!(error.shape, ErrorShape::OpenAiStringError);
        assert_eq!(error.message, "context is full");
        assert!(error.rewritten_by_host);
    }

    #[test]
    fn fastapi_detail_bodies_are_recognised_in_both_forms() {
        let string_form = normalize_error(422, r#"{"detail":"model not found"}"#);
        assert_eq!(string_form.shape, ErrorShape::FastApiDetail);
        assert_eq!(string_form.message, "model not found");

        let list_form = normalize_error(
            422,
            r#"{"detail":[{"loc":["body","model"],"msg":"field required"}]}"#,
        );
        assert_eq!(list_form.shape, ErrorShape::FastApiDetail);
        assert!(list_form.message.contains("field required"));
    }

    #[test]
    fn a_top_level_message_body_is_recognised() {
        let error = normalize_error(
            404,
            r#"{"type":"not_found_error","message":"model missing"}"#,
        );
        assert_eq!(error.shape, ErrorShape::BareMessage);
        assert_eq!(error.message, "model missing");
        assert_eq!(error.error_type.as_deref(), Some("not_found_error"));
        assert!(error.rewritten_by_host);
    }

    #[test]
    fn non_json_and_empty_bodies_are_reported_honestly() {
        let html = normalize_error(502, "<html><body>Bad Gateway</body></html>");
        assert_eq!(html.shape, ErrorShape::PlainText);
        assert!(html.message.contains("Bad Gateway"));

        let empty = normalize_error(503, "   ");
        assert_eq!(empty.shape, ErrorShape::Empty);
        assert!(empty.message.is_empty());
    }

    #[test]
    fn every_error_shape_label_matches_its_serialized_name() {
        // Prose and JSON must never disagree about what a shape is called.
        for shape in [
            ErrorShape::OpenAiObject,
            ErrorShape::OpenAiObjectWithoutType,
            ErrorShape::OpenAiStringError,
            ErrorShape::FastApiDetail,
            ErrorShape::BareMessage,
            ErrorShape::UnknownJson,
            ErrorShape::PlainText,
            ErrorShape::Empty,
        ] {
            let serialized = serde_json::to_string(&shape).expect("shape serializes");
            assert_eq!(serialized, format!("\"{}\"", shape.label()));
        }
    }

    #[test]
    fn a_2xx_body_is_never_marked_as_rewritten() {
        // The host only remaps bodies on non-2xx responses.
        let error = normalize_error(200, r#"{"detail":"odd but successful"}"#);
        assert!(!error.rewritten_by_host);
    }

    #[test]
    fn oversized_error_messages_are_truncated() {
        let body = format!(r#"{{"error":"{}"}}"#, "x".repeat(1000));
        let error = normalize_error(500, &body);
        assert!(error.message.ends_with("… (truncated)"));
        assert!(error.message.chars().count() < 450);
    }
}
