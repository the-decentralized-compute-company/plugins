//! The only part of this plugin that touches the network.
//!
//! Blast radius, stated precisely: every request below is issued to the single
//! base URL validated at startup, over cleartext http, with a bounded timeout.
//! Operation paths are literals from this file — no tool argument ever reaches
//! a URL — so no caller can steer a probe at a different host or walk out of
//! the configured path. There is no inbound listener, no filesystem access, and
//! no subprocess.
//!
//! None of this is on the data path. `tdcc` routes chat traffic to the endpoint
//! itself; these are diagnostics that answer "will routing work, and what will
//! it look like when it does".

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tdcc_plugin::PluginError;
use url::Url;

use crate::config::EndpointConfig;
use crate::openai::{
    self, FinishReasonReport, ModelsReport, NormalizedError, SseDecoder, StreamVerdict, UsageReport,
};

/// Prompt used by the live probes. Fixed rather than caller-supplied: these
/// tools run on someone else's hardware, and a probe is not a chat surface.
const PROBE_PROMPT: &str = "Reply with the single word: ok";
/// Default completion budget for a probe.
pub const DEFAULT_PROBE_MAX_TOKENS: u32 = 24;
/// Upper bound on what a probe may ask a contributor's GPU to generate.
pub const MAX_PROBE_MAX_TOKENS: u32 = 128;

/// A request that never reached an HTTP response, or one that did but failed.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeFailure {
    /// No HTTP response at all: refused, DNS failure, timeout, reset.
    Transport { detail: String },
    /// An HTTP response the backend reported as an error.
    Upstream(NormalizedError),
}

impl ProbeFailure {
    fn transport(context: &str, error: &reqwest::Error) -> Self {
        // reqwest's Display is terse about causes; walk the chain so a refused
        // connection does not surface as a bare "error sending request".
        let mut detail = format!("{context}: {error}");
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            detail.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        Self::Transport { detail }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Transport { detail } => detail.clone(),
            Self::Upstream(error) => {
                format!("upstream returned HTTP {}: {}", error.status, error.message)
            }
        }
    }

    /// Turn a failed probe into an MCP error carrying the full classification.
    ///
    /// A tool that cannot reach its backend must say so loudly; returning an
    /// empty success would make a dead endpoint look like an idle one.
    pub fn into_plugin_error(self, what: &str) -> PluginError {
        let mut error = PluginError::internal(format!("{what} failed: {}", self.message()));
        error.data_json = serde_json::to_string(&self).unwrap_or_else(|_| "{}".into());
        error
    }
}

// ---------------------------------------------------------------------------
// Cached probe result, for the health hook
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CachedProbe {
    summary: String,
    at: Instant,
}

/// Last known endpoint observation, shared between the tools and the plugin's
/// `health` hook.
///
/// The health hook must stay fast and independent of long-running work, so it
/// reads this cache instead of making a request. The host runs its own endpoint
/// probe every 15 seconds regardless; duplicating that here would be waste.
#[derive(Clone, Debug, Default)]
pub struct ProbeCache {
    inner: Arc<Mutex<Option<CachedProbe>>>,
}

impl ProbeCache {
    pub fn record(&self, summary: impl Into<String>) {
        let mut slot = self.lock();
        *slot = Some(CachedProbe {
            summary: summary.into(),
            at: Instant::now(),
        });
    }

    /// `(summary, age)` of the last observation, if there has been one.
    pub fn last(&self) -> Option<(String, Duration)> {
        self.lock()
            .as_ref()
            .map(|cached| (cached.summary.clone(), cached.at.elapsed()))
    }

    /// A poisoned lock means a handler panicked while recording. The cache is a
    /// single value with no invariant worth preserving, so recovering it keeps
    /// health reporting alive instead of poisoning every later call.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CachedProbe>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// One `GET .../models` attempt.
#[derive(Clone, Debug, Serialize)]
pub struct ModelsOutcome {
    pub url: String,
    pub status: u16,
    pub elapsed_ms: u128,
    pub authenticated: bool,
    #[serde(flatten)]
    pub report: ModelsReport,
}

/// The host's probe reproduced exactly, plus an authenticated comparison when
/// a key is configured.
#[derive(Clone, Debug, Serialize)]
pub struct HealthOutcome {
    /// The URL the host itself requests.
    pub probe_url: String,
    /// Result of the unauthenticated request — the one that decides routing.
    pub host_equivalent: ProbeAttempt,
    /// The same request with the configured bearer token, when there is one.
    pub authenticated: Option<ProbeAttempt>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeAttempt {
    pub ok: bool,
    pub status: Option<u16>,
    pub elapsed_ms: u128,
    pub models: usize,
    pub detail: String,
}

/// Everything one streaming request revealed.
#[derive(Clone, Debug, Serialize)]
pub struct StreamOutcome {
    pub model: String,
    pub status: u16,
    /// Whether the response advertised `text/event-stream`.
    pub content_type: Option<String>,
    pub verdict: StreamVerdict,
    /// Distinct network reads the body arrived in. One read plus many events is
    /// the buffered signature.
    pub reads: usize,
    pub events: usize,
    pub keepalives: usize,
    /// Whether the terminating `data: [DONE]` frame arrived.
    pub done_sentinel: bool,
    pub first_event_ms: Option<u128>,
    pub last_event_ms: Option<u128>,
    pub total_ms: u128,
    /// Characters of assistant text assembled from the deltas.
    pub content_chars: usize,
    pub finish_reason: Option<FinishReasonReport>,
    pub usage: UsageReport,
    /// Named SSE event types seen, for servers that use them.
    pub event_names: Vec<String>,
    /// An error frame delivered inside an otherwise-200 stream. Several servers
    /// report mid-generation failures this way instead of with a status code.
    pub in_stream_error: Option<NormalizedError>,
}

/// Everything one non-streaming request revealed.
#[derive(Clone, Debug, Serialize)]
pub struct CompletionOutcome {
    pub model: String,
    pub status: u16,
    pub elapsed_ms: u128,
    pub content_chars: usize,
    pub finish_reason: Option<FinishReasonReport>,
    pub usage: UsageReport,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Upstream {
    config: EndpointConfig,
    client: reqwest::Client,
    cache: ProbeCache,
}

impl Upstream {
    pub fn new(config: EndpointConfig, cache: ProbeCache) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            // Bounds the whole request, body included. A probe that cannot
            // finish inside the configured budget is a finding, not a hang.
            .timeout(config.timeout())
            .build()?;
        Ok(Self {
            config,
            client,
            cache,
        })
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    pub fn cache(&self) -> &ProbeCache {
        &self.cache
    }

    /// `GET <base>/models`, authenticated when a key is configured.
    pub async fn discover_models(&self) -> Result<ModelsOutcome, ProbeFailure> {
        let url = self.config.operation_url("models");
        let key = self.config.api_key();
        let started = Instant::now();
        let response = self
            .authorized_get(url.clone(), key.as_deref())
            .send()
            .await
            .map_err(|error| ProbeFailure::transport("GET models", &error))?;

        let status = response.status().as_u16();
        let body = read_body_text(response).await?;
        let elapsed_ms = started.elapsed().as_millis();

        if !(200..300).contains(&status) {
            let failure = ProbeFailure::Upstream(openai::normalize_error(status, &body));
            self.cache
                .record(format!("model discovery failed: {}", failure.message()));
            return Err(failure);
        }

        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let report = openai::read_models(&parsed);
        self.cache
            .record(format!("{} model(s) discovered at {url}", report.ids.len()));
        Ok(ModelsOutcome {
            url: url.to_string(),
            status,
            elapsed_ms,
            authenticated: key.is_some(),
            report,
        })
    }

    /// Reproduce the host's endpoint health probe, then repeat it with the
    /// configured key so an auth-gated endpoint is diagnosed rather than
    /// guessed at.
    pub async fn check_health(&self) -> HealthOutcome {
        let probe_url = openai::models_probe_url(self.config.base_url());
        let host_equivalent = self.attempt_probe(probe_url.clone(), None).await;
        let authenticated = match self.config.api_key() {
            Some(key) => Some(self.attempt_probe(probe_url.clone(), Some(&key)).await),
            None => None,
        };

        self.cache.record(if host_equivalent.ok {
            format!(
                "host-equivalent probe ok ({} model(s)) at {probe_url}",
                host_equivalent.models
            )
        } else {
            format!("host-equivalent probe failed: {}", host_equivalent.detail)
        });

        HealthOutcome {
            probe_url: probe_url.to_string(),
            host_equivalent,
            authenticated,
        }
    }

    async fn attempt_probe(&self, url: Url, key: Option<&str>) -> ProbeAttempt {
        let started = Instant::now();
        let response = match self.authorized_get(url.clone(), key).send().await {
            Ok(response) => response,
            Err(error) => {
                return ProbeAttempt {
                    ok: false,
                    status: None,
                    elapsed_ms: started.elapsed().as_millis(),
                    models: 0,
                    detail: ProbeFailure::transport(&format!("GET {url}"), &error).message(),
                };
            }
        };

        let status = response.status().as_u16();
        let body = read_body_text(response).await.unwrap_or_default();
        let elapsed_ms = started.elapsed().as_millis();
        let ok = (200..300).contains(&status);

        let models = if ok {
            serde_json::from_str::<Value>(&body)
                .map(|value| openai::read_models(&value).ids.len())
                .unwrap_or(0)
        } else {
            0
        };

        ProbeAttempt {
            ok,
            status: Some(status),
            elapsed_ms,
            models,
            detail: if ok {
                format!("GET {url} -> {status}")
            } else {
                openai::normalize_error(status, &body).message
            },
        }
    }

    /// Issue one streaming chat completion and watch how the body arrives.
    pub async fn verify_stream(
        &self,
        model: &str,
        max_tokens: u32,
        include_usage: bool,
    ) -> Result<StreamOutcome, ProbeFailure> {
        let url = self.config.operation_url("chat/completions");
        let mut body = probe_request_body(model, max_tokens, true);
        if include_usage {
            // Most servers need this to emit usage on a stream; a few reject
            // unknown request fields, which is why it is switchable.
            body["stream_options"] = json!({ "include_usage": true });
        }

        let started = Instant::now();
        let response = self
            .authorized_post(url, &body)
            .send()
            .await
            .map_err(|error| ProbeFailure::transport("POST chat/completions", &error))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if !(200..300).contains(&status) {
            let body = read_body_text(response).await?;
            let failure = ProbeFailure::Upstream(openai::normalize_error(status, &body));
            self.cache
                .record(format!("stream check failed: {}", failure.message()));
            return Err(failure);
        }

        let collected = self.collect_stream(response, started).await?;
        let outcome = collected.into_outcome(model, status, content_type, started);
        self.cache.record(format!(
            "stream check on '{model}': {:?} ({} event(s) over {} read(s))",
            outcome.verdict, outcome.events, outcome.reads
        ));
        Ok(outcome)
    }

    /// Read the SSE body one network read at a time.
    ///
    /// Counting reads is the measurement: a server that streams delivers many,
    /// and one that buffers delivers exactly one no matter how fast the machine
    /// is. Nothing here waits for the body to complete before looking at it —
    /// doing so would destroy the very property being tested.
    async fn collect_stream(
        &self,
        response: reqwest::Response,
        started: Instant,
    ) -> Result<StreamCollection, ProbeFailure> {
        use futures_util::StreamExt;

        let mut collected = StreamCollection::default();
        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ProbeFailure::transport("read stream", &error))?;
            if chunk.is_empty() {
                continue;
            }
            collected.reads += 1;
            let text = String::from_utf8_lossy(&chunk).into_owned();
            for event in decoder.push(&text) {
                collected.absorb(event, started);
            }
        }
        if let Some(event) = decoder.finish() {
            collected.absorb(event, started);
        }
        collected.keepalives = decoder.keepalives();
        Ok(collected)
    }

    /// Issue one ordinary, non-streaming chat completion.
    pub async fn probe_completion(
        &self,
        model: &str,
        max_tokens: u32,
    ) -> Result<CompletionOutcome, ProbeFailure> {
        let url = self.config.operation_url("chat/completions");
        let body = probe_request_body(model, max_tokens, false);

        let started = Instant::now();
        let response = self
            .authorized_post(url, &body)
            .send()
            .await
            .map_err(|error| ProbeFailure::transport("POST chat/completions", &error))?;
        let status = response.status().as_u16();
        let text = read_body_text(response).await?;
        let elapsed_ms = started.elapsed().as_millis();

        if !(200..300).contains(&status) {
            let failure = ProbeFailure::Upstream(openai::normalize_error(status, &text));
            self.cache
                .record(format!("completion check failed: {}", failure.message()));
            return Err(failure);
        }

        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let choice = parsed
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let content_chars = choice
            .and_then(|choice| choice.pointer("/message/content"))
            .and_then(Value::as_str)
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0);
        let finish_reason = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            .map(openai::normalize_finish_reason);

        self.cache
            .record(format!("completion check on '{model}' returned {status}"));
        Ok(CompletionOutcome {
            model: model.to_string(),
            status,
            elapsed_ms,
            content_chars,
            finish_reason,
            usage: openai::normalize_usage(parsed.get("usage")),
        })
    }

    /// Deliberately request a model that cannot exist, to capture the shape the
    /// backend uses for errors. Cheap: it never reaches a GPU.
    pub async fn probe_error_shape(&self) -> Result<NormalizedError, ProbeFailure> {
        let url = self.config.operation_url("chat/completions");
        let body = probe_request_body("tdcc-openai-endpoint-nonexistent-model-probe", 1, false);
        let response = self
            .authorized_post(url, &body)
            .send()
            .await
            .map_err(|error| ProbeFailure::transport("POST chat/completions", &error))?;
        let status = response.status().as_u16();
        let text = read_body_text(response).await?;
        Ok(openai::normalize_error(status, &text))
    }

    fn authorized_get(&self, url: Url, key: Option<&str>) -> reqwest::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        match key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn authorized_post(&self, url: Url, body: &Value) -> reqwest::RequestBuilder {
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "text/event-stream, application/json",
            )
            .body(body.to_string());
        match self.config.api_key() {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }
}

/// Accumulator for one streaming response.
#[derive(Debug, Default)]
struct StreamCollection {
    reads: usize,
    events: usize,
    keepalives: usize,
    done_sentinel: bool,
    first_event_ms: Option<u128>,
    last_event_ms: Option<u128>,
    content_chars: usize,
    finish_reason: Option<FinishReasonReport>,
    usage: UsageReport,
    event_names: Vec<String>,
    in_stream_error: Option<NormalizedError>,
}

impl StreamCollection {
    fn absorb(&mut self, event: openai::SseEvent, started: Instant) {
        self.events += 1;
        let elapsed = started.elapsed().as_millis();
        self.first_event_ms.get_or_insert(elapsed);
        self.last_event_ms = Some(elapsed);

        if let Some(name) = event.event.as_deref()
            && !self.event_names.iter().any(|seen| seen == name)
        {
            self.event_names.push(name.to_string());
        }

        if event.is_done() {
            self.done_sentinel = true;
            return;
        }

        let Some(payload) = event.json() else {
            return;
        };

        // A 200 response can still carry an error frame mid-stream.
        if payload.get("error").is_some() {
            self.in_stream_error
                .get_or_insert_with(|| openai::normalize_error(200, &payload.to_string()));
            return;
        }

        if let Some(usage) = payload.get("usage").filter(|usage| usage.is_object()) {
            self.usage = openai::normalize_usage(Some(usage));
        }

        let Some(choice) = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };

        if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
            self.content_chars += content.chars().count();
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(openai::normalize_finish_reason(reason));
        }
    }

    fn into_outcome(
        self,
        model: &str,
        status: u16,
        content_type: Option<String>,
        started: Instant,
    ) -> StreamOutcome {
        StreamOutcome {
            model: model.to_string(),
            status,
            content_type,
            verdict: openai::classify_stream(self.reads, self.events),
            reads: self.reads,
            events: self.events,
            keepalives: self.keepalives,
            done_sentinel: self.done_sentinel,
            first_event_ms: self.first_event_ms,
            last_event_ms: self.last_event_ms,
            total_ms: started.elapsed().as_millis(),
            content_chars: self.content_chars,
            finish_reason: self.finish_reason,
            usage: self.usage,
            event_names: self.event_names,
            in_stream_error: self.in_stream_error,
        }
    }
}

/// The probe request body. Deterministic and tiny by design.
fn probe_request_body(model: &str, max_tokens: u32, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": PROBE_PROMPT }],
        // `max_tokens` rather than `max_completion_tokens`: local servers
        // overwhelmingly accept the former and many reject the latter.
        "max_tokens": max_tokens.clamp(1, MAX_PROBE_MAX_TOKENS),
        "temperature": 0,
        "stream": stream,
    })
}

async fn read_body_text(response: reqwest::Response) -> Result<String, ProbeFailure> {
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ProbeFailure::transport("read response body", &error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_body_is_small_bounded_and_deterministic() {
        let body = probe_request_body("m", 10_000, true);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0);
        // A caller cannot talk a contributor's GPU into a long generation.
        assert_eq!(body["max_tokens"], MAX_PROBE_MAX_TOKENS);
        assert_eq!(body["messages"][0]["content"], PROBE_PROMPT);

        let floored = probe_request_body("m", 0, false);
        assert_eq!(floored["max_tokens"], 1);
        assert_eq!(floored["stream"], false);
    }

    #[test]
    fn the_probe_cache_reports_the_latest_observation() {
        let cache = ProbeCache::default();
        assert!(cache.last().is_none());
        cache.record("first");
        cache.record("second");
        let (summary, age) = cache.last().expect("an observation was recorded");
        assert_eq!(summary, "second");
        assert!(age < Duration::from_secs(5));
    }

    #[test]
    fn stream_collection_assembles_deltas_finish_reason_and_usage() {
        let started = Instant::now();
        let mut collected = StreamCollection {
            reads: 3,
            ..StreamCollection::default()
        };
        for payload in [
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"{"choices":[{"delta":{"content":"!"},"finish_reason":"eos_token"}]}"#,
            r#"{"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        ] {
            collected.absorb(
                openai::SseEvent {
                    event: None,
                    data: payload.to_string(),
                },
                started,
            );
        }
        collected.absorb(
            openai::SseEvent {
                event: None,
                data: "[DONE]".to_string(),
            },
            started,
        );

        let outcome = collected.into_outcome("m", 200, None, started);
        assert_eq!(outcome.verdict, StreamVerdict::Incremental);
        assert_eq!(outcome.events, 4);
        assert!(outcome.done_sentinel);
        assert_eq!(outcome.content_chars, 3);
        let finish = outcome.finish_reason.expect("a finish reason arrived");
        assert_eq!(finish.raw, "eos_token");
        assert_eq!(finish.normalized, "stop");
        assert_eq!(outcome.usage.total_tokens, Some(7));
        assert!(outcome.usage.total_derived);
        assert!(outcome.in_stream_error.is_none());
    }

    #[test]
    fn an_error_frame_inside_a_200_stream_is_captured() {
        let started = Instant::now();
        let mut collected = StreamCollection {
            reads: 2,
            ..StreamCollection::default()
        };
        collected.absorb(
            openai::SseEvent {
                event: None,
                data: r#"{"error":{"message":"context window exceeded","type":"server_error"}}"#
                    .to_string(),
            },
            started,
        );
        let outcome = collected.into_outcome("m", 200, None, started);
        let error = outcome.in_stream_error.expect("error frame captured");
        assert_eq!(error.message, "context window exceeded");
        // A 200 response is never rewritten by the host, so the client sees
        // this frame exactly as the backend wrote it.
        assert!(!error.rewritten_by_host);
    }

    #[test]
    fn a_single_read_carrying_every_event_is_reported_as_buffered() {
        let started = Instant::now();
        let mut collected = StreamCollection {
            reads: 1,
            ..StreamCollection::default()
        };
        for _ in 0..5 {
            collected.absorb(
                openai::SseEvent {
                    event: None,
                    data: r#"{"choices":[{"delta":{"content":"x"}}]}"#.to_string(),
                },
                started,
            );
        }
        let outcome = collected.into_outcome("m", 200, None, started);
        assert_eq!(outcome.verdict, StreamVerdict::Buffered);
        assert_eq!(outcome.content_chars, 5);
    }

    #[test]
    fn named_stream_events_are_recorded_once_each() {
        let started = Instant::now();
        let mut collected = StreamCollection {
            reads: 4,
            ..StreamCollection::default()
        };
        for name in ["response.created", "response.delta", "response.delta"] {
            collected.absorb(
                openai::SseEvent {
                    event: Some(name.to_string()),
                    data: "{}".to_string(),
                },
                started,
            );
        }
        let outcome = collected.into_outcome("m", 200, None, started);
        assert_eq!(
            outcome.event_names,
            vec!["response.created", "response.delta"]
        );
    }

    // -----------------------------------------------------------------
    // Live tests against a real socket.
    //
    // Streaming is the requirement most easily broken and least visible in a
    // mock, so these stand up an actual OpenAI-compatible SSE server on
    // loopback and drive the real reqwest client against it. The buffered case
    // is the one that matters: it is indistinguishable from a working server
    // in any test that waits for the body before looking at it.
    // -----------------------------------------------------------------

    /// One scripted HTTP response, written as a sequence of separate socket
    /// writes. Several writes with a pause between them is a streaming server;
    /// one write is a buffering one.
    type Script = Vec<Vec<u8>>;

    /// Gap between scripted writes. Long enough that loopback delivers them as
    /// distinct reads, short enough not to slow the suite down.
    const WRITE_GAP: Duration = Duration::from_millis(40);

    async fn spawn_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str, &str) -> Script + Send + Sync + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        let handler = Arc::new(handler);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let _ = socket.set_nodelay(true);

                    // Read the head, then whatever body the request declared,
                    // so the client never sees a reset mid-write.
                    let mut raw = Vec::new();
                    let mut byte = [0u8; 1];
                    while !raw.ends_with(b"\r\n\r\n") {
                        match socket.read(&mut byte).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => raw.push(byte[0]),
                        }
                    }
                    let head = String::from_utf8_lossy(&raw).into_owned();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && socket.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let body = String::from_utf8_lossy(&body).into_owned();

                    for (index, write) in handler(&head, &body).into_iter().enumerate() {
                        if index > 0 {
                            tokio::time::sleep(WRITE_GAP).await;
                        }
                        if socket.write_all(&write).await.is_err() {
                            return;
                        }
                        let _ = socket.flush().await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{address}/v1"), handle)
    }

    /// Response head for an EOF-delimited body, as SSE endpoints send.
    const SSE_HEAD: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

    fn json_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// The frames of a normal chat completion stream.
    fn stream_frames() -> Vec<Vec<u8>> {
        vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"k\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec(),
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2,\"total_tokens\":11}}\n\n".to_vec(),
            b"data: [DONE]\n\n".to_vec(),
        ]
    }

    fn upstream_for(base_url: &str) -> Upstream {
        let config = EndpointConfig::from_launch(Vec::<String>::new(), Some(base_url.to_string()))
            .expect("valid configuration");
        Upstream::new(config, ProbeCache::default()).expect("client builds")
    }

    #[tokio::test]
    async fn a_streaming_backend_is_verified_as_incremental_over_a_real_socket() {
        let (base_url, server) = spawn_server(|head, _body| {
            assert!(head.starts_with("POST /v1/chat/completions "), "{head}");
            let mut script = vec![SSE_HEAD.to_vec()];
            script.extend(stream_frames());
            script
        })
        .await;

        let upstream = upstream_for(&base_url);
        let outcome = upstream
            .verify_stream("probe-model", 8, true)
            .await
            .expect("the stream completes");

        assert_eq!(outcome.verdict, StreamVerdict::Incremental);
        assert_eq!(outcome.events, 5);
        // The body arrived in more than one read: that is what "streaming"
        // means to a client, and what a buffering proxy would destroy.
        assert!(outcome.reads > 1, "reads = {}", outcome.reads);
        assert!(outcome.done_sentinel);
        assert_eq!(outcome.content_chars, 2);
        assert_eq!(outcome.content_type.as_deref(), Some("text/event-stream"));
        let finish = outcome.finish_reason.expect("a finish reason arrived");
        assert_eq!(finish.raw, "stop");
        assert!(finish.canonical);
        assert_eq!(outcome.usage.total_tokens, Some(11));
        assert!(!outcome.usage.total_derived);
        // First token well before the stream ended: no buffering anywhere.
        assert!(outcome.first_event_ms.expect("timed") < outcome.total_ms);

        server.abort();
    }

    #[tokio::test]
    async fn a_backend_that_buffers_the_whole_stream_is_caught() {
        let (base_url, server) = spawn_server(|_head, _body| {
            // Byte-for-byte the same SSE body as the streaming case — but
            // delivered in a single write, exactly as a reverse proxy with
            // response buffering would.
            let mut single = SSE_HEAD.to_vec();
            for frame in stream_frames() {
                single.extend_from_slice(&frame);
            }
            vec![single]
        })
        .await;

        let upstream = upstream_for(&base_url);
        let outcome = upstream
            .verify_stream("probe-model", 8, true)
            .await
            .expect("the request succeeds; the delivery is the problem");

        assert_eq!(outcome.verdict, StreamVerdict::Buffered);
        assert_eq!(outcome.reads, 1);
        assert_eq!(outcome.events, 5);
        // The content is all there — which is precisely why this failure mode
        // is invisible to any check that only looks at the finished body.
        assert_eq!(outcome.content_chars, 2);

        server.abort();
    }

    #[tokio::test]
    async fn stream_options_include_usage_is_sent_only_when_asked_for() {
        let (base_url, server) = spawn_server(|_head, body| {
            let sent: Value = serde_json::from_str(body).expect("probe sends JSON");
            let echoed = sent
                .get("stream_options")
                .map(|value| value.to_string())
                .unwrap_or_else(|| "absent".into());
            let mut script = vec![SSE_HEAD.to_vec()];
            script.push(
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{echoed:?}}}}}]}}\n\n")
                    .into_bytes(),
            );
            script.push(b"data: [DONE]\n\n".to_vec());
            script
        })
        .await;

        let upstream = upstream_for(&base_url);

        let with_usage = upstream
            .verify_stream("probe-model", 4, true)
            .await
            .expect("stream completes");
        assert!(with_usage.content_chars > "absent".len());

        let without_usage = upstream
            .verify_stream("probe-model", 4, false)
            .await
            .expect("stream completes");
        assert_eq!(without_usage.content_chars, "absent".len());

        server.abort();
    }

    #[tokio::test]
    async fn model_discovery_reads_the_ids_the_host_will_route_on() {
        let (base_url, server) = spawn_server(|head, _body| {
            assert!(head.starts_with("GET /v1/models "), "{head}");
            vec![json_response(
                "200 OK",
                r#"{"object":"list","data":[{"id":"qwen3-8b"},{"id":"llama3-70b"}]}"#,
            )]
        })
        .await;

        let outcome = upstream_for(&base_url)
            .discover_models()
            .await
            .expect("discovery succeeds");
        assert_eq!(outcome.status, 200);
        assert!(!outcome.authenticated);
        assert_eq!(outcome.report.ids, vec!["qwen3-8b", "llama3-70b"]);

        server.abort();
    }

    #[tokio::test]
    async fn an_unreachable_backend_errors_instead_of_returning_an_empty_success() {
        // Nothing listens on port 1.
        let failure = upstream_for("http://127.0.0.1:1/v1")
            .discover_models()
            .await
            .expect_err("an unreachable backend is an error");
        assert!(matches!(failure, ProbeFailure::Transport { .. }));

        let error = failure.into_plugin_error("model discovery");
        assert!(error.message.contains("model discovery failed"));
        assert!(error.data_json.contains("transport"));
    }

    #[tokio::test]
    async fn an_upstream_error_response_is_classified_rather_than_swallowed() {
        let (base_url, server) = spawn_server(|_head, _body| {
            vec![json_response(
                "503 Service Unavailable",
                r#"{"detail":"engine is still loading weights"}"#,
            )]
        })
        .await;

        let failure = upstream_for(&base_url)
            .discover_models()
            .await
            .expect_err("a 503 is an error");
        let ProbeFailure::Upstream(error) = &failure else {
            panic!("expected an upstream failure, got {failure:?}");
        };
        assert_eq!(error.status, 503);
        assert_eq!(error.shape, openai::ErrorShape::FastApiDetail);
        assert!(error.message.contains("loading weights"));
        // The host rewrites this into OpenAI's envelope before a client sees it.
        assert!(error.rewritten_by_host);

        server.abort();
    }

    #[tokio::test]
    async fn an_api_key_gated_endpoint_is_diagnosed_as_unroutable_not_merely_down() {
        // A unique name keeps this from racing other tests in the same process.
        const KEY_VAR: &str = "OPENAI_ENDPOINT_TEST_KEY_ROUTABILITY";
        // SAFETY: single-threaded within this test, and the variable name is
        // used by no other test in the crate.
        unsafe { std::env::set_var(KEY_VAR, "test-token") };

        let (base_url, server) = spawn_server(|head, _body| {
            let authorized = head
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));
            vec![if authorized {
                json_response("200 OK", r#"{"data":[{"id":"gated-model"}]}"#)
            } else {
                json_response("401 Unauthorized", r#"{"error":"missing api key"}"#)
            }]
        })
        .await;

        let config = EndpointConfig::from_launch(
            ["--api-key-env", KEY_VAR].map(String::from),
            Some(base_url),
        )
        .expect("valid configuration");
        let upstream = Upstream::new(config, ProbeCache::default()).expect("client builds");

        let health = upstream.check_health().await;
        // The host's probe carries no credentials, so it fails — and the
        // endpoint therefore never becomes routable, however healthy the
        // backend is from anywhere else.
        assert!(!health.host_equivalent.ok);
        assert_eq!(health.host_equivalent.status, Some(401));
        let authenticated = health.authenticated.expect("a key is configured");
        assert!(authenticated.ok);
        assert_eq!(authenticated.models, 1);

        unsafe { std::env::remove_var(KEY_VAR) };
        server.abort();
    }

    #[tokio::test]
    async fn a_healthy_endpoint_probe_matches_the_url_the_host_requests() {
        let (base_url, server) = spawn_server(|head, _body| {
            assert!(head.starts_with("GET /v1/models "), "{head}");
            vec![json_response("200 OK", r#"{"data":[{"id":"m"}]}"#)]
        })
        .await;

        let upstream = upstream_for(&base_url);
        let health = upstream.check_health().await;
        assert!(health.host_equivalent.ok);
        assert_eq!(health.host_equivalent.models, 1);
        assert!(health.authenticated.is_none());
        assert!(health.probe_url.ends_with("/v1/models"));
        // The health hook has something to report without making a request.
        assert!(upstream.cache().last().is_some());

        server.abort();
    }

    #[tokio::test]
    async fn a_non_streaming_completion_reports_usage_and_finish_reason() {
        let (base_url, server) = spawn_server(|_head, body| {
            let sent: Value = serde_json::from_str(body).expect("probe sends JSON");
            assert_eq!(sent["stream"], false);
            vec![json_response(
                "200 OK",
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"eos_token"}],
                    "usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
            )]
        })
        .await;

        let outcome = upstream_for(&base_url)
            .probe_completion("probe-model", 8)
            .await
            .expect("completion succeeds");
        assert_eq!(outcome.content_chars, 2);
        let finish = outcome.finish_reason.expect("a finish reason arrived");
        assert!(!finish.canonical);
        assert_eq!(finish.normalized, "stop");
        assert_eq!(outcome.usage.total_tokens, Some(11));
        assert!(outcome.usage.total_derived);

        server.abort();
    }

    #[test]
    fn a_failed_probe_becomes_an_mcp_error_carrying_the_classification() {
        let failure = ProbeFailure::Upstream(openai::normalize_error(
            503,
            r#"{"detail":"engine is loading"}"#,
        ));
        let error = failure.into_plugin_error("model discovery");
        assert!(error.message.contains("model discovery failed"));
        assert!(error.message.contains("503"));
        assert!(error.data_json.contains("fast_api_detail"));
    }
}
