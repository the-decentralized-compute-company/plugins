//! The one place in this plugin that touches the network.
//!
//! One request shape — `multipart/form-data` to a Whisper-compatible
//! transcription endpoint — and a reply parser that is deliberately forgiving
//! about the envelope and deliberately strict about failure. A backend that
//! cannot be reached, refuses the key, or answers with something that is not a
//! transcript produces an error naming the cause and the setting that fixes it.
//! It never produces an empty transcript, because an outage and a silent
//! recording would then look identical to the caller.

use std::fmt;

use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;

use crate::config::{Backend, ENV_API_KEY, Limits};
use crate::segments::{self, Segment};

/// Cap on a reply body. A `verbose_json` transcript of an hour of speech is a
/// few hundred kilobytes; this is generous and still bounded, so a misbehaving
/// endpoint cannot stream the process to death.
const MAX_REPLY_BYTES: usize = 8 * 1_024 * 1_024;

/// How much of a failing backend's body is quoted back. Enough to carry the
/// real reason, short enough not to paste somebody's HTML error page into a
/// model's context.
const BODY_EXCERPT_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub enum BackendError {
    /// Nothing answered: wrong port, server not running, DNS, TLS.
    Unreachable { detail: String },
    /// The request was accepted but took longer than the configured timeout.
    TimedOut { seconds: u64 },
    /// A non-2xx status, already turned into an operator-readable sentence
    /// that names the status, the endpoint, and the setting to change.
    Refused { message: String },
    /// A 2xx whose body is not a transcript.
    Unusable { message: String },
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { detail } => formatter.write_str(detail),
            Self::TimedOut { seconds } => write!(
                formatter,
                "the transcription backend did not answer within {seconds}s. Transcribing a long \
                 chunk on a busy GPU can legitimately take longer — raise `--timeout-secs`, or \
                 lower `--chunk-seconds` so each request is smaller."
            ),
            Self::Refused { message } => formatter.write_str(message),
            Self::Unusable { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for BackendError {}

/// What one request asks for.
pub struct TranscriptionRequest {
    pub audio: Vec<u8>,
    /// Sent as the multipart filename. Backends pick a decoder from its
    /// extension, so it is derived from the sniffed bytes, not the real name —
    /// and never from a caller-supplied string, which keeps a path out of the
    /// request entirely.
    pub filename: String,
    pub mime_type: &'static str,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub want_segments: bool,
}

/// What one reply contained.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptionReply {
    pub text: Option<String>,
    pub segments: Vec<Segment>,
    /// The language the backend reports it detected, when it reports one.
    pub language: Option<String>,
    /// The duration the backend measured, when it reports one. Used only as a
    /// cross-check; this plugin's own timeline comes from the chunk plan.
    pub duration: Option<f64>,
}

pub struct BackendClient {
    client: Client,
    backend: Backend,
    send_granularity_field: bool,
    timeout_seconds: u64,
}

impl BackendClient {
    pub fn new(
        backend: Backend,
        limits: &Limits,
        user_agent: &str,
        send_granularity_field: bool,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .user_agent(user_agent.to_string())
            .timeout(limits.request_timeout)
            .connect_timeout(std::time::Duration::from_secs(10).min(limits.request_timeout))
            // Redirects are not followed: a transcription endpoint that
            // redirects is a misconfiguration, and replaying a multipart body
            // with an Authorization header to a new host is not something to do
            // silently.
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .build()
            .map_err(|error| format!("could not build the HTTP client: {error}"))?;
        Ok(Self {
            client,
            backend,
            send_granularity_field,
            timeout_seconds: limits.request_timeout.as_secs(),
        })
    }

    pub fn endpoint(&self) -> &str {
        self.backend.endpoint.as_str()
    }

    pub fn model(&self) -> &str {
        &self.backend.model
    }

    pub fn has_api_key(&self) -> bool {
        self.backend.api_key.is_some()
    }

    pub async fn transcribe(
        &self,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionReply, BackendError> {
        let form = self.build_form(request)?;
        let mut outgoing = self
            .client
            .post(self.backend.endpoint.clone())
            .header("Accept", "application/json")
            .multipart(form);
        if let Some(key) = &self.backend.api_key {
            outgoing = outgoing.bearer_auth(key);
        }

        let response = outgoing.send().await.map_err(|error| {
            if error.is_timeout() {
                BackendError::TimedOut {
                    seconds: self.timeout_seconds,
                }
            } else {
                BackendError::Unreachable {
                    detail: format!(
                        "could not reach the transcription backend at {}: {}. Check that the \
                         server is running and that `--backend-url` points at its transcriptions \
                         endpoint.",
                        self.backend.endpoint,
                        self.redact(&error.to_string())
                    ),
                }
            }
        })?;

        let status = response.status();
        let body = self.read_capped(response).await?;
        if !status.is_success() {
            return Err(BackendError::Refused {
                message: self.redact(&status_message(
                    status,
                    &body,
                    self.backend.endpoint.as_str(),
                    self.backend.api_key.is_some(),
                    &self.backend.model,
                )),
            });
        }

        parse_reply(&body).map_err(|message| BackendError::Unusable {
            message: self.redact(&message),
        })
    }

    fn build_form(&self, request: TranscriptionRequest) -> Result<Form, BackendError> {
        let part = Part::bytes(request.audio)
            .file_name(request.filename)
            .mime_str(request.mime_type)
            .map_err(|error| BackendError::Unusable {
                message: format!("could not build the upload part: {error}"),
            })?;

        let mut form = Form::new()
            .part("file", part)
            .text("model", self.backend.model.clone())
            .text(
                "response_format",
                if request.want_segments {
                    "verbose_json"
                } else {
                    "json"
                },
            );
        // OpenAI needs this to return segments at all; whisper.cpp ignores it.
        // `--no-granularity-field` exists for a backend strict enough to reject
        // a multipart field it does not know.
        if request.want_segments && self.send_granularity_field {
            form = form.text("timestamp_granularities[]", "segment");
        }
        if let Some(language) = request.language {
            // `auto` is this plugin's spelling of "do not send a hint".
            if language != "auto" {
                form = form.text("language", language);
            }
        }
        if let Some(prompt) = request.prompt {
            form = form.text("prompt", prompt);
        }
        Ok(form)
    }

    /// Read a reply body with a ceiling, so a backend answering with a gigabyte
    /// of HTML cannot take the node down with it.
    async fn read_capped(&self, mut response: Response) -> Result<Vec<u8>, BackendError> {
        let mut body = Vec::new();
        loop {
            let next = response.chunk().await.map_err(|error| {
                if error.is_timeout() {
                    BackendError::TimedOut {
                        seconds: self.timeout_seconds,
                    }
                } else {
                    BackendError::Unreachable {
                        detail: format!(
                            "the transcription backend at {} closed the connection while sending \
                             its reply: {}",
                            self.backend.endpoint,
                            self.redact(&error.to_string())
                        ),
                    }
                }
            })?;
            let Some(chunk) = next else { break };
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err(BackendError::Unusable {
                    message: format!(
                        "the transcription backend at {} sent more than {MAX_REPLY_BYTES} bytes in \
                         reply, which is not a transcript. Check that `--backend-url` points at a \
                         transcriptions endpoint and not at a download.",
                        self.backend.endpoint
                    ),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn redact(&self, text: &str) -> String {
        match &self.backend.api_key {
            Some(key) => redact(text, key),
            None => text.to_string(),
        }
    }
}

/// Remove an API key from text on its way to a caller or a log.
///
/// Transport and TLS errors quote the request they failed on, and a
/// misconfigured backend cheerfully echoes headers back in its error body. The
/// repository is public and so is a tool result; neither is a place for a key.
pub fn redact(text: &str, key: &str) -> String {
    if key.len() < 4 {
        return text.to_string();
    }
    text.replace(key, "<redacted>")
}

/// Turn a non-2xx into a sentence naming the likely cause and the setting.
///
/// Split out as a pure function because these are the messages an operator will
/// actually read at three in the morning, and they are worth testing.
pub fn status_message(
    status: StatusCode,
    body: &[u8],
    endpoint: &str,
    has_key: bool,
    model: &str,
) -> String {
    let detail = body_excerpt(body);
    let quoted = if detail.is_empty() {
        String::new()
    } else {
        format!(" It said: {detail}")
    };

    match status.as_u16() {
        401 | 403 if has_key => format!(
            "the transcription backend at {endpoint} rejected the API key ({status}). Check the \
             value of {ENV_API_KEY} in the environment of the tdcc process.{quoted}"
        ),
        401 | 403 => format!(
            "the transcription backend at {endpoint} requires authentication ({status}) and no key \
             is configured. Export {ENV_API_KEY} in the environment of the tdcc process.{quoted}"
        ),
        404 => format!(
            "the transcription backend at {endpoint} has no such endpoint ({status}). A TDCC node \
             does not serve /v1/audio/transcriptions itself. whisper.cpp's server usually serves \
             `/inference`; an OpenAI-compatible server usually serves \
             `/v1/audio/transcriptions`. Correct `--backend-url`.{quoted}"
        ),
        405 => format!(
            "the transcription backend at {endpoint} does not accept POST ({status}), so that URL \
             is not a transcriptions endpoint. Correct `--backend-url`.{quoted}"
        ),
        413 => format!(
            "the transcription backend at {endpoint} refused the upload as too large ({status}). \
             Lower `--max-upload-bytes`, and lower `--chunk-seconds` so each chunk of a long WAV is \
             smaller.{quoted}"
        ),
        415 => format!(
            "the transcription backend at {endpoint} does not accept this audio format \
             ({status}). Convert the recording to WAV or MP3, or point `--backend-url` at a \
             backend that decodes it.{quoted}"
        ),
        400 | 422 => format!(
            "the transcription backend at {endpoint} rejected the request ({status}). The usual \
             cause is the `model` field: this plugin sent `{model}`, which you can change with \
             `--model`.{quoted}"
        ),
        429 => format!(
            "the transcription backend at {endpoint} is rate limiting this node ({status}). Retry \
             later, or lower how many chunks one call sends by raising \
             `--chunk-seconds`.{quoted}"
        ),
        500..=599 => format!(
            "the transcription backend at {endpoint} failed while transcribing ({status}). This is \
             the backend's own error, not a configuration problem on this node.{quoted}"
        ),
        _ => format!("the transcription backend at {endpoint} answered {status}.{quoted}"),
    }
}

/// Read a 2xx body into a reply.
///
/// JSON is the expected shape. A backend that ignored `response_format` and
/// answered with plain text is still useful — the transcript is right there —
/// so it is accepted, with no segments, rather than failed.
pub fn parse_reply(body: &[u8]) -> Result<TranscriptionReply, String> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(
            "the transcription backend answered successfully with an empty body, so there is no \
             transcript to return. Check the backend's own logs."
                .to_string(),
        );
    }

    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        // Not JSON. `response_format=text` looks exactly like this, and the
        // content is a perfectly good transcript.
        return Ok(TranscriptionReply {
            text: Some(trimmed.to_string()),
            ..TranscriptionReply::default()
        });
    };

    // A 200 carrying an error envelope is a real pattern; it must not become an
    // empty transcript.
    if let Some(message) = error_envelope(&value) {
        return Err(format!(
            "the transcription backend answered with an error rather than a transcript: {message}"
        ));
    }

    // Presence of the fields, not their contents, is what makes this a
    // transcript. `{"text": ""}` is the correct and complete answer for a
    // silent recording — `probe_backend` uploads exactly that — so it must not
    // be mistaken for a backend answering with something else entirely.
    let carries_a_transcript = value.get("text").is_some_and(Value::is_string)
        || value.get("segments").is_some_and(Value::is_array);
    if !carries_a_transcript {
        return Err(format!(
            "the transcription backend answered with JSON that carries no `text` and no \
             `segments`, so it is not a transcript: {}",
            body_excerpt(body)
        ));
    }

    Ok(TranscriptionReply {
        text: segments::parse_text(&value),
        language: value
            .get("language")
            .and_then(Value::as_str)
            .map(|language| language.trim().to_string())
            .filter(|language| !language.is_empty()),
        duration: value.get("duration").and_then(segments::parse_time),
        segments: segments::parse_segments(&value),
    })
}

/// The message out of the error envelopes these backends use.
fn error_envelope(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.trim().to_string());
    }
    if let Some(message) = error.as_str() {
        return Some(message.trim().to_string());
    }
    Some(error.to_string())
}

/// A short, single-line quotation of a backend's body.
fn body_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return String::new();
    }
    if collapsed.chars().count() <= BODY_EXCERPT_CHARS {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(BODY_EXCERPT_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(status: u16, body: &str, has_key: bool) -> String {
        status_message(
            StatusCode::from_u16(status).expect("status"),
            body.as_bytes(),
            "http://127.0.0.1:8080/v1/audio/transcriptions",
            has_key,
            "whisper-1",
        )
    }

    #[test]
    fn a_rejected_key_names_the_environment_variable_rather_than_the_key() {
        let with_key = message(401, r#"{"error":{"message":"Incorrect API key"}}"#, true);
        assert!(with_key.contains(ENV_API_KEY), "{with_key}");
        assert!(with_key.contains("rejected the API key"), "{with_key}");

        let without_key = message(401, "", false);
        assert!(
            without_key.contains("no key is configured"),
            "{without_key}"
        );
        assert!(without_key.contains(ENV_API_KEY), "{without_key}");
    }

    #[test]
    fn a_404_says_the_node_does_not_serve_this_itself_and_names_both_conventions() {
        let message = message(404, "Not Found", false);

        assert!(
            message.contains("does not serve /v1/audio/transcriptions"),
            "{message}"
        );
        assert!(message.contains("/inference"), "{message}");
        assert!(message.contains("--backend-url"), "{message}");
    }

    #[test]
    fn each_failure_that_matters_gets_its_own_sentence_and_its_own_setting() {
        for (status, expected_setting) in [
            (413u16, "--max-upload-bytes"),
            (415, "Convert the recording"),
            (400, "--model"),
            (422, "--model"),
            (429, "rate limiting"),
            (405, "does not accept POST"),
            (503, "backend's own error"),
        ] {
            let rendered = message(status, "", false);
            assert!(
                rendered.contains(expected_setting),
                "status {status} should mention {expected_setting}: {rendered}"
            );
        }
    }

    #[test]
    fn an_unrecognised_status_still_answers_with_the_status_and_the_endpoint() {
        let rendered = message(418, "teapot", false);
        assert!(rendered.contains("418"), "{rendered}");
        assert!(rendered.contains("127.0.0.1:8080"), "{rendered}");
        assert!(rendered.contains("teapot"), "{rendered}");
    }

    #[test]
    fn a_long_error_body_is_quoted_briefly_and_on_one_line() {
        let html = format!("<html>\n  <body>{}</body>\n</html>", "x".repeat(5_000));
        let rendered = message(500, &html, false);

        assert!(rendered.contains('…'), "{rendered}");
        assert!(
            !rendered.contains('\n'),
            "the excerpt is collapsed to one line"
        );
        assert!(rendered.len() < 1_000, "length {}", rendered.len());
    }

    #[test]
    fn a_key_is_removed_from_anything_on_its_way_back_to_a_caller() {
        let leaked = "connection to https://api.example/v1 failed: Bearer sk-live-abc123def";
        assert_eq!(
            redact(leaked, "sk-live-abc123def"),
            "connection to https://api.example/v1 failed: Bearer <redacted>"
        );
        // A short "key" is not treated as one: replacing every "ab" in an error
        // message would mangle it for no security benefit.
        assert_eq!(redact("a backend", "ab"), "a backend");
    }

    #[test]
    fn a_verbose_json_reply_parses_into_text_and_segments() {
        let body = json!({
            "text": "Hello there.",
            "language": "english",
            "duration": 4.5,
            "segments": [{"start": 0.0, "end": 4.5, "text": "Hello there."}]
        })
        .to_string();

        let reply = parse_reply(body.as_bytes()).expect("a transcript");
        assert_eq!(reply.text.as_deref(), Some("Hello there."));
        assert_eq!(reply.language.as_deref(), Some("english"));
        assert_eq!(reply.duration, Some(4.5));
        assert_eq!(reply.segments.len(), 1);
    }

    #[test]
    fn a_json_reply_with_only_text_is_a_transcript_without_segments() {
        let reply = parse_reply(br#"{"text":"just the words"}"#).expect("a transcript");

        assert_eq!(reply.text.as_deref(), Some("just the words"));
        assert!(reply.segments.is_empty());
    }

    #[test]
    fn a_plain_text_reply_is_accepted_rather_than_failed() {
        let reply = parse_reply(b"  the backend ignored response_format  ").expect("a transcript");

        assert_eq!(
            reply.text.as_deref(),
            Some("the backend ignored response_format")
        );
        assert!(reply.segments.is_empty());
    }

    #[test]
    fn an_error_envelope_returned_with_a_2xx_is_not_an_empty_success() {
        let error = parse_reply(br#"{"error":{"message":"model not loaded"}}"#)
            .expect_err("an error envelope is not a transcript");
        assert!(error.contains("model not loaded"), "{error}");

        let bare = parse_reply(br#"{"error":"no model"}"#).expect_err("also an error");
        assert!(bare.contains("no model"), "{bare}");
    }

    #[test]
    fn an_empty_body_is_an_error_naming_what_to_look_at() {
        let error = parse_reply(b"   ").expect_err("empty is not a transcript");
        assert!(error.contains("empty body"), "{error}");
        assert!(error.contains("backend's own logs"), "{error}");
    }

    #[test]
    fn json_that_is_not_a_transcript_is_refused_and_quoted() {
        let error = parse_reply(br#"{"status":"queued","id":"job-17"}"#)
            .expect_err("no text and no segments");

        assert!(error.contains("no `text` and no `segments`"), "{error}");
        assert!(error.contains("job-17"), "{error}");
    }

    #[test]
    fn a_reply_whose_segments_are_empty_but_whose_text_is_present_is_still_a_transcript() {
        let reply = parse_reply(br#"{"text":"words","segments":[]}"#).expect("a transcript");
        assert_eq!(reply.text.as_deref(), Some("words"));
        assert!(reply.segments.is_empty());
    }

    /// Silence transcribes to nothing, and nothing is the right answer. It must
    /// not be confused with a backend that answered with something that is not
    /// a transcript at all — which is why presence of the field decides, not
    /// its contents.
    #[test]
    fn an_empty_transcript_of_a_silent_recording_is_a_success() {
        let reply = parse_reply(br#"{"text":"","segments":[]}"#).expect("silence is a transcript");
        assert_eq!(reply.text, None, "no words were spoken");
        assert!(reply.segments.is_empty());

        let bare = parse_reply(br#"{"text":""}"#).expect("also a transcript");
        assert_eq!(bare.text, None);
    }
}
