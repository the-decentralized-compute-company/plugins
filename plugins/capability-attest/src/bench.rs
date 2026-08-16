//! Running the benchmark and turning a token stream into timings.
//!
//! The request is built entirely from the [`BenchmarkProfile`], so the same
//! profile on two machines sends the same bytes. Timing needs a *streamed*
//! response — time to first token is not observable from a buffered one — so a
//! server that ignores `stream: true` produces a clear error rather than a
//! silently wrong number.
//!
//! The parsing helpers are pure and carry the tests; the two `async` functions
//! are thin wrappers that add a clock and a socket.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::activity::{Contention, classify_busy_report, classify_guard_probe};
use crate::config::AttestConfig;
use crate::profile::BenchmarkProfile;

/// Cap on how much of an error body is quoted back. Endpoints have been known
/// to return an HTML page.
const ERROR_BODY_CHARS: usize = 400;

/// Where a run's output-token count came from.
///
/// A server-reported count is authoritative; counting streamed deltas is a
/// proxy that is wrong whenever a chunk carries more or less than one token.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenCountSource {
    ServerUsage,
    StreamDeltas,
}

/// One measured run.
///
/// Every number is an integer. See `profile.rs` for why: a claim that carries
/// an `f64` cannot be re-verified reliably after a JSON round trip.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RunSample {
    /// 1-based index among the measured runs.
    pub run: u32,
    pub time_to_first_token_us: u64,
    pub total_us: u64,
    pub output_tokens: u64,
    /// Generation rate in thousandths of a token per second: 63_000 is 63 tok/s.
    ///
    /// Tokens after the first, divided by the time after the first. Prefill is
    /// reported separately as `time_to_first_token_us` rather than being
    /// averaged into the generation rate.
    pub output_tokens_per_second_milli: u64,
    pub token_count_source: TokenCountSource,
    /// Prompt tokens as counted by the server, when it reports usage. This is
    /// the ground truth the profile's `context_tokens` only estimates.
    pub prompt_tokens: Option<u64>,
}

impl RunSample {
    /// Human-readable throughput. Derived for display; never signed.
    pub fn output_tokens_per_second(&self) -> f64 {
        self.output_tokens_per_second_milli as f64 / 1000.0
    }

    /// Human-readable time to first token. Derived for display; never signed.
    pub fn time_to_first_token_ms(&self) -> f64 {
        self.time_to_first_token_us as f64 / 1000.0
    }
}

/// Raw result of one streamed request, before it becomes a sample.
#[derive(Clone, Debug)]
pub struct StreamOutcome {
    pub time_to_first_token_us: u64,
    pub total_us: u64,
    pub delta_count: u64,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

impl StreamOutcome {
    pub fn into_sample(self, run: u32) -> Result<RunSample, String> {
        let (output_tokens, token_count_source) = match self.completion_tokens {
            Some(tokens) if tokens > 0 => (tokens, TokenCountSource::ServerUsage),
            _ => (self.delta_count, TokenCountSource::StreamDeltas),
        };
        let output_tokens_per_second_milli =
            throughput_milli(output_tokens, self.time_to_first_token_us, self.total_us)?;
        Ok(RunSample {
            run,
            time_to_first_token_us: self.time_to_first_token_us,
            total_us: self.total_us,
            output_tokens,
            output_tokens_per_second_milli,
            token_count_source,
            prompt_tokens: self.prompt_tokens,
        })
    }
}

/// Generation throughput in thousandths of a token per second, excluding
/// prefill.
///
/// The first token's cost is time to first token; the remaining `n - 1` tokens
/// are what the generation window produced. Reporting `n / total` instead would
/// quietly blend prefill into the rate and make long-context runs look slower
/// at generation than they are.
pub fn throughput_milli(output_tokens: u64, ttft_us: u64, total_us: u64) -> Result<u64, String> {
    if output_tokens < 2 {
        return Err(format!(
            "run produced {output_tokens} output token(s); at least 2 are needed to measure a \
             generation rate. Raise --max-output-tokens"
        ));
    }
    let Some(window_us) = total_us.checked_sub(ttft_us) else {
        return Err("run reports a first token after the run finished".to_string());
    };
    if window_us <= 1_000 {
        return Err(format!(
            "generation window was {window_us}us, too short to measure. \
             Raise --max-output-tokens"
        ));
    }
    // (tokens - 1) tokens per window_us microseconds, expressed in
    // milli-tokens per second: (tokens - 1) * 1e9 / window_us.
    let rate = u128::from(output_tokens - 1) * 1_000_000_000 / u128::from(window_us);
    u64::try_from(rate).map_err(|_| "computed throughput does not fit in a u64".to_string())
}

/// Median of a non-empty slice. Even lengths average the two middle values,
/// rounding down so the result stays an exact integer.
pub fn median_u64(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2
    })
}

// ── Request bodies ──────────────────────────────────────────────────────────

/// The measured request. Every field comes from the profile.
pub fn chat_request_body(profile: &BenchmarkProfile, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": profile.model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": profile.max_output_tokens,
        "temperature": profile.temperature(),
        "top_p": profile.top_p(),
        "seed": profile.seed,
        "stream": true,
        // Servers that support it return a usage block on the final chunk,
        // which upgrades the token count from a delta tally to ground truth.
        "stream_options": { "include_usage": true },
    })
}

/// The contention guard request: as small as a request can be, because it runs
/// on a node that might be busy.
pub fn guard_request_body(profile: &BenchmarkProfile) -> serde_json::Value {
    serde_json::json!({
        "model": profile.model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
        "temperature": 0.0,
        "stream": true,
    })
}

// ── SSE parsing ─────────────────────────────────────────────────────────────

/// Append a network chunk to the parse buffer, dropping carriage returns.
///
/// SSE allows `\r\n` line endings, and a chunk boundary can fall between the
/// two bytes. Stripping `\r` on arrival means the framing search below only has
/// to look for `\n\n`. It is safe for this payload: a raw `0x0D` byte cannot
/// appear inside a JSON string (JSON requires `\r`), so nothing meaningful is
/// removed.
pub fn push_chunk(buffer: &mut Vec<u8>, chunk: &[u8]) {
    buffer.extend(chunk.iter().copied().filter(|byte| *byte != b'\r'));
}

/// Pull every complete SSE event out of the buffer, leaving any partial tail.
///
/// Returns the concatenated `data:` lines of each event. Comment lines (`:`)
/// and other fields are ignored, which is what the spec asks for.
pub fn drain_sse_payloads(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut payloads = Vec::new();
    while let Some(position) = buffer.windows(2).position(|window| window == b"\n\n") {
        let block: Vec<u8> = buffer.drain(..position + 2).collect();
        // A complete event block is whole UTF-8: it never ends mid-character,
        // because the terminator is ASCII.
        let text = String::from_utf8_lossy(&block[..position]);
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if !data.is_empty() {
            payloads.push(data);
        }
    }
    payloads
}

/// Flush a trailing event that arrived without its blank-line terminator.
pub fn drain_final_payloads(buffer: &mut Vec<u8>) -> Vec<String> {
    if buffer.is_empty() {
        return Vec::new();
    }
    buffer.extend_from_slice(b"\n\n");
    drain_sse_payloads(buffer)
}

/// The content of a chat completion delta, if this chunk carries any.
pub fn delta_text(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("choices")?
        .as_array()?
        .first()?
        .get("delta")?
        .get("content")?
        .as_str()
        .filter(|text| !text.is_empty())
}

/// `(prompt_tokens, completion_tokens)` from a chunk's usage block.
pub fn usage_tokens(payload: &serde_json::Value) -> Option<(Option<u64>, Option<u64>)> {
    let usage = payload.get("usage")?;
    if usage.is_null() {
        return None;
    }
    Some((
        usage
            .get("prompt_tokens")
            .and_then(serde_json::Value::as_u64),
        usage
            .get("completion_tokens")
            .and_then(serde_json::Value::as_u64),
    ))
}

// ── Network ─────────────────────────────────────────────────────────────────

pub fn build_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| anyhow!("could not build an HTTP client: {error}"))
}

/// Issue one streamed chat completion and time it.
pub async fn stream_once(
    client: &reqwest::Client,
    url: &Url,
    api_key: Option<&str>,
    body: &serde_json::Value,
) -> Result<StreamOutcome> {
    let mut request = client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .body(serde_json::to_string(body)?);
    if let Some(key) = api_key {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
    }

    let started = Instant::now();
    let mut response = request
        .send()
        .await
        .map_err(|error| anyhow!("request to {url} failed: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "{url} returned {status}: {}",
            truncate(&body, ERROR_BODY_CHARS)
        );
    }

    let mut buffer = Vec::new();
    let mut first_token_at: Option<Duration> = None;
    let mut delta_count = 0u64;
    let mut prompt_tokens = None;
    let mut completion_tokens = None;
    let mut saw_any_event = false;

    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| anyhow!("stream from {url} failed: {error}"))?;
        let Some(chunk) = chunk else { break };
        push_chunk(&mut buffer, &chunk);
        for payload in drain_sse_payloads(&mut buffer) {
            saw_any_event = true;
            consume_payload(
                &payload,
                started,
                &mut first_token_at,
                &mut delta_count,
                &mut prompt_tokens,
                &mut completion_tokens,
            );
        }
    }
    for payload in drain_final_payloads(&mut buffer) {
        saw_any_event = true;
        consume_payload(
            &payload,
            started,
            &mut first_token_at,
            &mut delta_count,
            &mut prompt_tokens,
            &mut completion_tokens,
        );
    }

    let total_us = started.elapsed().as_micros() as u64;
    let Some(first_token_at) = first_token_at else {
        if saw_any_event {
            bail!(
                "{url} streamed {delta_count} content token(s); time to first token is not \
                 measurable from this response"
            );
        }
        bail!(
            "{url} did not return a server-sent event stream, so time to first token cannot be \
             measured. This plugin requires an endpoint that honours \"stream\": true"
        );
    };

    Ok(StreamOutcome {
        time_to_first_token_us: first_token_at.as_micros() as u64,
        total_us,
        delta_count,
        prompt_tokens,
        completion_tokens,
    })
}

fn consume_payload(
    payload: &str,
    started: Instant,
    first_token_at: &mut Option<Duration>,
    delta_count: &mut u64,
    prompt_tokens: &mut Option<u64>,
    completion_tokens: &mut Option<u64>,
) {
    if payload.trim() == "[DONE]" {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    if let Some((prompt, completion)) = usage_tokens(&value) {
        if prompt.is_some() {
            *prompt_tokens = prompt;
        }
        if completion.is_some() {
            *completion_tokens = completion;
        }
    }
    if delta_text(&value).is_some() {
        if first_token_at.is_none() {
            *first_token_at = Some(started.elapsed());
        }
        *delta_count += 1;
    }
}

/// Ask the node whether it is busy.
///
/// With `--busy-url` configured, that answer is authoritative and an
/// unreachable probe means [`Contention::Unknown`] — never "probably idle".
/// Without one, a one-token request stands in as a latency proxy, which is
/// weaker and is labelled as such in the detail string.
pub async fn measure_contention(client: &reqwest::Client, config: &AttestConfig) -> Contention {
    if let Some(busy_url) = &config.busy_url {
        return match fetch_json(client, busy_url).await {
            Ok(body) => classify_busy_report(&body, &config.busy_pointer, config.busy_threshold),
            Err(error) => Contention::Unknown {
                detail: format!("busy probe {busy_url} failed: {error}"),
            },
        };
    }

    let url = match config.chat_completions_url() {
        Ok(url) => url,
        Err(error) => {
            return Contention::Unknown {
                detail: error.to_string(),
            };
        }
    };
    match stream_once(
        client,
        &url,
        config.api_key.as_deref(),
        &guard_request_body(&config.profile),
    )
    .await
    {
        Ok(outcome) => {
            classify_guard_probe(outcome.time_to_first_token_us, config.max_guard_ttft_ms)
        }
        Err(error) => Contention::Unknown {
            detail: format!("guard probe failed: {error}"),
        },
    }
}

async fn fetch_json(client: &reqwest::Client, url: &Url) -> Result<serde_json::Value> {
    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| anyhow!("{error}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| anyhow!("{error}"))?;
    if !status.is_success() {
        bail!("{status}: {}", truncate(&body, ERROR_BODY_CHARS));
    }
    serde_json::from_str(&body).map_err(|error| anyhow!("response is not JSON: {error}"))
}

fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::DEFAULT_FILLER_SENTENCE;

    fn profile() -> BenchmarkProfile {
        BenchmarkProfile::build(
            "demo-model".into(),
            64,
            128,
            0.2,
            0.9,
            7,
            1,
            3,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap()
    }

    fn feed(buffer: &mut Vec<u8>, text: &str) -> Vec<String> {
        push_chunk(buffer, text.as_bytes());
        drain_sse_payloads(buffer)
    }

    #[test]
    fn events_split_across_chunk_boundaries_are_reassembled() {
        let mut buffer = Vec::new();

        assert!(feed(&mut buffer, "data: {\"a\":").is_empty());
        assert!(feed(&mut buffer, "1}\n").is_empty());

        let payloads = feed(&mut buffer, "\ndata: {\"b\":2}\n\n");

        assert_eq!(payloads, vec!["{\"a\":1}", "{\"b\":2}"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn crlf_framing_and_comment_lines_are_handled() {
        let mut buffer = Vec::new();

        let payloads = feed(
            &mut buffer,
            ": keep-alive\r\n\r\ndata: {\"x\":1}\r\n\r\ndata: [DONE]\r\n\r\n",
        );

        assert_eq!(payloads, vec!["{\"x\":1}", "[DONE]"]);
    }

    #[test]
    fn a_multi_byte_payload_split_mid_character_still_parses() {
        // The chunk boundary falls inside the UTF-8 encoding of "é".
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"é\"}}]}\n\n";
        let bytes = text.as_bytes();
        let split = text.find('é').unwrap() + 1;

        let mut buffer = Vec::new();
        push_chunk(&mut buffer, &bytes[..split]);
        assert!(drain_sse_payloads(&mut buffer).is_empty());
        push_chunk(&mut buffer, &bytes[split..]);
        let payloads = drain_sse_payloads(&mut buffer);

        let value: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(delta_text(&value), Some("é"));
    }

    #[test]
    fn a_trailing_event_without_a_blank_line_is_still_delivered() {
        let mut buffer = Vec::new();
        push_chunk(&mut buffer, b"data: {\"z\":1}");

        assert!(drain_sse_payloads(&mut buffer).is_empty());
        assert_eq!(drain_final_payloads(&mut buffer), vec!["{\"z\":1}"]);
    }

    #[test]
    fn only_non_empty_content_deltas_count_as_tokens() {
        let role_only = serde_json::json!({ "choices": [{ "delta": { "role": "assistant" } }] });
        let empty = serde_json::json!({ "choices": [{ "delta": { "content": "" } }] });
        let content = serde_json::json!({ "choices": [{ "delta": { "content": "hi" } }] });
        let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });

        assert_eq!(delta_text(&role_only), None);
        assert_eq!(delta_text(&empty), None);
        assert_eq!(delta_text(&content), Some("hi"));
        assert_eq!(delta_text(&finish), None);
    }

    #[test]
    fn usage_is_read_when_present_and_ignored_when_null() {
        let with_usage = serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 260, "completion_tokens": 64 }
        });
        assert_eq!(usage_tokens(&with_usage), Some((Some(260), Some(64))));

        let null_usage = serde_json::json!({ "choices": [], "usage": null });
        assert_eq!(usage_tokens(&null_usage), None);

        let no_usage = serde_json::json!({ "choices": [] });
        assert_eq!(usage_tokens(&no_usage), None);
    }

    #[test]
    fn server_usage_beats_a_delta_tally_and_is_labelled() {
        let outcome = StreamOutcome {
            time_to_first_token_us: 100_000,
            total_us: 1_100_000,
            delta_count: 60,
            prompt_tokens: Some(260),
            completion_tokens: Some(64),
        };

        let sample = outcome.into_sample(1).unwrap();

        assert_eq!(sample.output_tokens, 64);
        assert_eq!(sample.token_count_source, TokenCountSource::ServerUsage);
        assert_eq!(sample.prompt_tokens, Some(260));
        // 63 tokens across a 1000ms generation window.
        assert_eq!(sample.output_tokens_per_second_milli, 63_000);
        assert!((sample.output_tokens_per_second() - 63.0).abs() < 1e-9);
    }

    #[test]
    fn without_server_usage_the_delta_tally_is_used_and_labelled() {
        let outcome = StreamOutcome {
            time_to_first_token_us: 50_000,
            total_us: 1_050_000,
            delta_count: 41,
            prompt_tokens: None,
            completion_tokens: None,
        };

        let sample = outcome.into_sample(2).unwrap();

        assert_eq!(sample.output_tokens, 41);
        assert_eq!(sample.token_count_source, TokenCountSource::StreamDeltas);
    }

    #[test]
    fn a_run_too_short_to_measure_is_an_error_rather_than_a_huge_number() {
        assert!(throughput_milli(1, 10_000, 20_000).is_err());
        assert!(throughput_milli(64, 10_000, 10_500).is_err());
        assert!(
            throughput_milli(64, 100_000, 50_000).is_err(),
            "a first token after the end of the run is not a measurement"
        );

        let error = throughput_milli(1, 10_000, 2_000_000).unwrap_err();
        assert!(error.contains("--max-output-tokens"), "{error}");
    }

    #[test]
    fn throughput_excludes_prefill() {
        // 100ms prefill, then 10 further tokens in 1000ms.
        assert_eq!(throughput_milli(11, 100_000, 1_100_000).unwrap(), 10_000);
    }

    #[test]
    fn median_handles_odd_and_even_run_counts() {
        assert_eq!(median_u64(&[]), None);
        assert_eq!(median_u64(&[5]), Some(5));
        assert_eq!(median_u64(&[9, 1, 5]), Some(5));
        assert_eq!(median_u64(&[4, 1, 3, 2]), Some(2));
    }

    #[test]
    fn the_request_body_is_built_only_from_pinned_profile_fields() {
        let profile = profile();
        let body = chat_request_body(&profile, "PROMPT");

        assert_eq!(body["model"], "demo-model");
        assert_eq!(body["messages"][0]["content"], "PROMPT");
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["seed"], 7);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn the_guard_request_is_as_small_as_possible() {
        let body = guard_request_body(&profile());

        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "ping");
    }

    #[test]
    fn long_error_bodies_are_quoted_but_not_dumped() {
        let html = "x".repeat(5_000);
        let quoted = truncate(&html, ERROR_BODY_CHARS);

        assert_eq!(quoted.chars().count(), ERROR_BODY_CHARS + 1);
        assert!(quoted.ends_with('…'));
    }

    // ── End to end, over a real socket ──────────────────────────────────────
    //
    // The parsing above is pure and cheap to test. These stand up an actual
    // listener and speak HTTP to it, because the interesting failures — a
    // backend that answers with an error page, a backend that ignores
    // `stream: true`, a probe that is simply not there — only happen at that
    // boundary.

    use crate::testutil::{SSE_CHUNKS, SSE_HEAD, dead_address, serve_once, url};

    #[tokio::test]
    async fn a_streamed_response_is_timed_and_its_usage_block_is_believed() {
        let address = serve_once(SSE_HEAD, SSE_CHUNKS.to_vec(), Duration::from_millis(40)).await;
        let client = build_client(Duration::from_secs(10)).unwrap();

        let outcome = stream_once(
            &client,
            &url(address, "/v1/chat/completions"),
            None,
            &chat_request_body(&profile(), "prompt"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.delta_count, 2);
        assert_eq!(outcome.completion_tokens, Some(9));
        assert_eq!(outcome.prompt_tokens, Some(260));
        // The first content delta lands after two 40ms gaps, and the stream
        // runs for five, so the timings have to be ordered and plausible.
        assert!(
            outcome.time_to_first_token_us >= 60_000,
            "ttft {}us",
            outcome.time_to_first_token_us
        );
        assert!(outcome.total_us > outcome.time_to_first_token_us);

        let sample = outcome.into_sample(1).unwrap();
        assert_eq!(sample.output_tokens, 9);
        assert_eq!(sample.token_count_source, TokenCountSource::ServerUsage);
    }

    #[tokio::test]
    async fn a_backend_that_ignores_stream_true_is_an_error_not_a_zero() {
        // A plain buffered completion. Nothing here says when the first token
        // appeared, so there is no honest number to report.
        let address = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            vec!["{\"choices\":[{\"message\":{\"content\":\"hello\"}}]}"],
            Duration::from_millis(1),
        )
        .await;
        let client = build_client(Duration::from_secs(10)).unwrap();

        let error = stream_once(
            &client,
            &url(address, "/v1/chat/completions"),
            None,
            &chat_request_body(&profile(), "prompt"),
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("stream"),
            "the error must name the reason: {error}"
        );
    }

    #[tokio::test]
    async fn an_error_status_is_reported_with_the_body_the_backend_sent() {
        let address = serve_once(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 27\r\nConnection: close\r\n\r\n",
            vec!["{\"error\":\"model loading\"}\r\n"],
            Duration::from_millis(1),
        )
        .await;
        let client = build_client(Duration::from_secs(10)).unwrap();

        let error = stream_once(
            &client,
            &url(address, "/v1/chat/completions"),
            None,
            &chat_request_body(&profile(), "prompt"),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("503"), "{error}");
        assert!(error.contains("model loading"), "{error}");
    }

    #[tokio::test]
    async fn a_configured_busy_probe_that_reports_work_stops_the_benchmark() {
        let address = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 24\r\nConnection: close\r\n\r\n",
            vec!["{\"active_requests\": 4}\r\n"],
            Duration::from_millis(1),
        )
        .await;
        let config = crate::config::resolve(
            &[
                "--endpoint".into(),
                "http://127.0.0.1:9/v1".into(),
                "--model".into(),
                "demo".into(),
                "--busy-url".into(),
                format!("http://{address}/stats"),
            ],
            &crate::config::EnvMap::new(),
        )
        .unwrap();
        let client = build_client(Duration::from_secs(10)).unwrap();

        let contention = measure_contention(&client, &config).await;

        assert!(
            matches!(contention, Contention::Busy { .. }),
            "{contention:?}"
        );
    }

    #[tokio::test]
    async fn a_busy_probe_that_cannot_be_reached_defers_instead_of_assuming_idle() {
        let address = dead_address().await;
        let config = crate::config::resolve(
            &[
                "--endpoint".into(),
                "http://127.0.0.1:9/v1".into(),
                "--model".into(),
                "demo".into(),
                "--busy-url".into(),
                format!("http://{address}/stats"),
            ],
            &crate::config::EnvMap::new(),
        )
        .unwrap();
        let client = build_client(Duration::from_secs(2)).unwrap();

        let contention = measure_contention(&client, &config).await;

        assert!(
            matches!(contention, Contention::Unknown { .. }),
            "an unreachable probe must never be read as an idle node: {contention:?}"
        );
        assert!(crate::activity::contention_deferral(&contention).is_some());
    }
}
