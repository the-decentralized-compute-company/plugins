//! One request shape: a non-streaming chat completion against the endpoint the
//! operator configured.
//!
//! # Blast radius
//!
//! Exactly one outbound POST, to exactly one URL, carrying exactly what the
//! jobs file says: a model id, an optional system message, one user message,
//! and two sampling parameters. Nothing from a tool call reaches this request —
//! `run_now` chooses *which* job runs, never what it says.
//!
//! The default endpoint is the node's own OpenAI-compatible API on
//! `127.0.0.1:9337`, and a non-loopback endpoint is refused at startup unless
//! the operator passed `--allow-remote-endpoint`. See [`crate::config`].
//!
//! # Why `stream: false`
//!
//! There is nobody watching. A scheduled run has one consumer — a file or a
//! webhook — and both want the whole answer. Streaming would add an incremental
//! parser and a partial-output failure mode for no benefit.

use std::time::Duration;

use reqwest::{Client, Url};
use serde_json::{Value, json};

use crate::config::ApiKey;
use crate::jobs::Job;

/// Cap on the response body read from the endpoint.
///
/// A completion capped at 131,072 tokens is nowhere near this; the cap exists
/// so a confused or hostile endpoint cannot hand a plugin an unbounded body.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Longest snippet of a failing response body quoted back in an error.
pub const MAX_ERROR_BODY_CHARS: usize = 300;

/// A completion, reduced to what a run record and a sink need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    /// The model the endpoint says answered, which is not always the one asked
    /// for — a node may route to a peer holding a different quantization.
    pub model: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub finish_reason: Option<String>,
}

/// Build the request body for one job.
///
/// `max_tokens` rather than `max_completion_tokens`: it is the field every
/// OpenAI-compatible server in the wild still accepts, including llama.cpp,
/// Ollama, vLLM, and the node's own frontend.
pub fn build_request(job: &Job) -> Value {
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = &job.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": job.prompt }));

    let mut body = json!({
        "model": job.model,
        "messages": messages,
        "stream": false,
    });
    let object = body.as_object_mut().expect("a JSON object was just built");
    if let Some(max_tokens) = job.max_output_tokens {
        object.insert("max_tokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = job.temperature {
        object.insert("temperature".into(), json!(temperature));
    }
    body
}

/// Pull the completion out of an OpenAI-shaped response body.
///
/// Kept separate from the request so the parsing — which is where servers
/// actually differ from one another — is testable without a socket.
pub fn parse_completion(body: &str) -> Result<Completion, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("response is not JSON ({error}): {}", truncate(body)))?;

    // An OpenAI-shaped error can arrive with a 200 from some proxies, so it is
    // checked before the happy path rather than after.
    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message").or(Some(error)))
        .and_then(|message| message.as_str().map(str::to_string))
    {
        return Err(format!("endpoint returned an error: {message}"));
    }

    let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
        return Err(format!(
            "response has no choices[0] entry: {}",
            truncate(body)
        ));
    };
    let Some(content) = choice
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return Err("response has no choices[0].message.content field".to_string());
    };

    let text = match content {
        Value::String(text) => text.clone(),
        // Some servers answer with the content-part array form.
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => {
            return Err(
                "response carried a null completion, which usually means the model produced only \
                 a tool call. A scheduled prompt has nobody to answer a tool call."
                    .to_string(),
            );
        }
        other => {
            return Err(format!(
                "response choices[0].message.content is a {}, not text",
                kind_of(other)
            ));
        }
    };

    if text.trim().is_empty() {
        // An empty completion is a failure, not a run that produced nothing:
        // writing an empty file or posting an empty webhook would look exactly
        // like a successful run to whoever reads the destination.
        return Err(
            "endpoint returned an empty completion. Nothing was delivered, because an empty \
             delivery is indistinguishable from a successful one at the destination."
                .to_string(),
        );
    }

    let usage = value.get("usage");
    Ok(Completion {
        text,
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        prompt_tokens: usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(Value::as_u64),
        completion_tokens: usage
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64),
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Trim a body for inclusion in an error message, on a character boundary.
pub fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_ERROR_BODY_CHARS {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(MAX_ERROR_BODY_CHARS).collect();
    format!("{kept}… ({} bytes total)", trimmed.len())
}

/// The one HTTP client this plugin uses for completions.
///
/// No redirects: an OpenAI-compatible endpoint that answers 302 is a
/// misconfiguration, and following it would send the operator's prompts
/// somewhere they never named.
pub fn build_client() -> reqwest::Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!(
            "tdcc-scheduled-prompts/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
}

/// Run one completion, with the job's own timeout.
///
/// Every failure names the endpoint, because "the job stopped working" and "the
/// node stopped serving that model" look identical from the outside otherwise.
pub async fn complete(
    client: &Client,
    url: &Url,
    api_key: Option<&ApiKey>,
    job: &Job,
) -> Result<Completion, String> {
    let mut request = client
        .post(url.clone())
        .timeout(Duration::from_secs(job.timeout_secs))
        .json(&build_request(job));
    if let Some(key) = api_key {
        request = request.header(reqwest::header::AUTHORIZATION, key.header_value());
    }

    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            format!(
                "{url} did not answer within timeout_secs = {} for model `{}`",
                job.timeout_secs, job.model
            )
        } else {
            format!("{url} is unreachable: {}", error.without_url())
        }
    })?;

    let status = response.status();
    let body = read_bounded(response).await?;
    if !status.is_success() {
        return Err(format!(
            "{url} answered HTTP {} for model `{}`: {}",
            status.as_u16(),
            job.model,
            truncate(&body)
        ));
    }

    parse_completion(&body).map_err(|error| format!("{error} (from {url})"))
}

/// Read a response body, refusing to buffer more than [`MAX_RESPONSE_BYTES`].
async fn read_bounded(response: reqwest::Response) -> Result<String, String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buffer.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(format!(
                        "response exceeded {MAX_RESPONSE_BYTES} bytes and was abandoned"
                    ));
                }
                buffer.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return Err(format!(
                    "reading the response failed: {}",
                    error.without_url()
                ));
            }
        }
    }
    String::from_utf8(buffer).map_err(|_| "response body was not valid UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvMap;
    use crate::jobs::parse_jobs;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const NOW: i64 = 1_772_323_200_000;

    fn job(extra: &str) -> Job {
        let text = format!(
            "version = 1\n\
             timezone = \"utc\"\n\
             \n\
             [[job]]\n\
             id = \"digest\"\n\
             schedule = \"0 3 * * *\"\n\
             model = \"qwen3:8b\"\n\
             prompt = \"Summarise the day.\"\n\
             sink = {{ kind = \"file\", path = \"a.md\" }}\n\
             {extra}"
        );
        parse_jobs(&text, &EnvMap::new(), NOW)
            .expect("fixture loads")
            .jobs
            .pop()
            .expect("one job")
    }

    #[test]
    fn the_request_carries_exactly_what_the_file_declared() {
        let body = build_request(&job(
            "system = \"You are terse.\"\nmax_output_tokens = 256\ntemperature = 0.2\n",
        ));

        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["temperature"], 0.2);
        let messages = body["messages"].as_array().expect("messages is an array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are terse.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Summarise the day.");
    }

    #[test]
    fn unset_sampling_parameters_are_omitted_rather_than_sent_as_defaults() {
        let body = build_request(&job(""));

        assert!(
            body.get("max_tokens").is_none(),
            "a server's own default must win when the file is silent"
        );
        assert!(body.get("temperature").is_none());
        assert_eq!(
            body["messages"].as_array().expect("array").len(),
            1,
            "no system message means no system message"
        );
    }

    #[test]
    fn an_ordinary_response_parses_with_its_usage() {
        let body = r#"{
            "model": "qwen3:8b",
            "choices": [{"index":0,"message":{"role":"assistant","content":"Two things happened."},
                         "finish_reason":"stop"}],
            "usage": {"prompt_tokens": 31, "completion_tokens": 12}
        }"#;

        let completion = parse_completion(body).expect("parses");

        assert_eq!(completion.text, "Two things happened.");
        assert_eq!(completion.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(completion.prompt_tokens, Some(31));
        assert_eq!(completion.completion_tokens, Some(12));
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn the_content_part_array_form_is_understood() {
        let body = r#"{"choices":[{"message":{"content":[
            {"type":"text","text":"first "},{"type":"text","text":"second"}]}}]}"#;

        assert_eq!(parse_completion(body).expect("parses").text, "first second");
    }

    #[test]
    fn an_empty_completion_is_a_failure_rather_than_an_empty_delivery() {
        for body in [
            r#"{"choices":[{"message":{"content":""}}]}"#,
            r#"{"choices":[{"message":{"content":"   \n "}}]}"#,
            r#"{"choices":[{"message":{"content":[]}}]}"#,
        ] {
            let error = parse_completion(body).expect_err("must fail");
            assert!(error.contains("empty"), "{body} -> {error}");
        }
    }

    #[test]
    fn every_malformed_shape_names_what_is_wrong() {
        let cases = [
            ("not json at all", "not JSON"),
            (
                r#"{"error":{"message":"model not found"}}"#,
                "model not found",
            ),
            (r#"{"choices":[]}"#, "choices[0]"),
            (r#"{"choices":[{"message":{}}]}"#, "content"),
            (r#"{"choices":[{"message":{"content":42}}]}"#, "not text"),
            (r#"{"choices":[{"message":{"content":null}}]}"#, "tool call"),
        ];
        for (body, expected) in cases {
            let error = parse_completion(body).expect_err("must fail");
            assert!(error.contains(expected), "{body} -> {error}");
        }
    }

    #[test]
    fn a_long_error_body_is_cut_on_a_character_boundary_and_labelled() {
        let html = "é".repeat(5_000);

        let truncated = truncate(&html);

        assert!(truncated.chars().count() < MAX_ERROR_BODY_CHARS + 40);
        assert!(truncated.contains("bytes total"));
    }

    // -----------------------------------------------------------------------
    // The request path against a real socket. A scripted listener answers one
    // canned response, so the client, the bounded read, and the error messages
    // are exercised rather than described.
    // -----------------------------------------------------------------------

    /// Serve one canned HTTP response on loopback and hand back its URL.
    async fn serve_once(response: String) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        Url::parse(&format!("http://127.0.0.1:{port}/v1/chat/completions")).expect("valid url")
    }

    fn http(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn a_real_round_trip_returns_the_completion() {
        let body = r#"{"model":"qwen3:8b","choices":[{"message":{"content":"done"}}],
                       "usage":{"completion_tokens":4}}"#;
        let url = serve_once(http(200, body)).await;

        let completion = complete(&build_client().expect("client"), &url, None, &job(""))
            .await
            .expect("a 200 with a completion is readable");

        assert_eq!(completion.text, "done");
        assert_eq!(completion.completion_tokens, Some(4));
    }

    #[tokio::test]
    async fn a_non_200_names_the_endpoint_the_status_and_the_model() {
        let url = serve_once(http(404, r#"{"error":{"message":"no such model"}}"#)).await;

        let error = complete(&build_client().expect("client"), &url, None, &job(""))
            .await
            .expect_err("404 is a failure");

        assert!(error.contains("404"), "{error}");
        assert!(error.contains("qwen3:8b"), "{error}");
        assert!(error.contains("127.0.0.1"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_port_is_an_error_naming_the_endpoint() {
        // Bind, take the port, then drop the listener so nothing answers.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let url = Url::parse(&format!("http://127.0.0.1:{port}/v1/chat/completions")).expect("url");

        let error = complete(&build_client().expect("client"), &url, None, &job(""))
            .await
            .expect_err("nothing is listening");

        assert!(error.contains("unreachable"), "{error}");
        assert!(error.contains(&port.to_string()), "{error}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_completion_fails_rather_than_delivering_html() {
        let url = serve_once(http(200, "<html>proxy error</html>")).await;

        let error = complete(&build_client().expect("client"), &url, None, &job(""))
            .await
            .expect_err("HTML is not a completion");

        assert!(error.contains("not JSON"), "{error}");
    }
}
