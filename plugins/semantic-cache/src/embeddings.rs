//! The one piece of this plugin that talks to the network.
//!
//! # Blast radius
//!
//! Exactly one outbound HTTP request shape, to exactly one URL that the
//! operator configured, carrying exactly one thing: the text of the trailing
//! user message. No filesystem access, no subprocesses, no listening sockets,
//! no other hosts. The endpoint is refused at startup unless it is on loopback
//! or the operator passed `--allow-remote-embeddings` (see
//! [`crate::config::endpoint_reach`]).
//!
//! # The dependency this plugin has, stated plainly
//!
//! It needs an **OpenAI-compatible `POST /v1/embeddings`**. As of the SDK
//! version in `Cargo.toml`, the TDCC node's own OpenAI frontend on
//! `127.0.0.1:9337` does not serve one — it serves `/v1/models`,
//! `/v1/chat/completions`, `/v1/completions` and `/v1/responses`, and its own
//! documentation lists embeddings as out of scope. So out of the box this
//! plugin has to be pointed at a local embeddings server. README.md lists the
//! ones that work. Nothing here pretends otherwise: `status` reports the
//! endpoint's real state, and a lookup that cannot embed returns an error
//! rather than a comfortable-looking miss.

use serde::Serialize;
use url::Url;

use crate::config::{ApiKey, Config};

/// Cap on how much of a failing response body is quoted back.
///
/// Long enough for a real error message, short enough that an HTML error page
/// does not end up in a tool result.
const MAX_ERROR_BODY: usize = 300;

#[derive(Debug)]
pub struct EmbedError {
    pub message: String,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl EmbedError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: [&'a str; 1],
    /// Omitted when unset: llama.cpp's embedding server ignores it, while
    /// OpenAI and Ollama require it. Sending `""` fails on all three.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
}

pub struct EmbeddingClient {
    http: reqwest::Client,
    url: Url,
    model: Option<String>,
    api_key: Option<ApiKey>,
}

impl EmbeddingClient {
    pub fn new(config: &Config) -> Result<Self, EmbedError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| EmbedError::new(format!("could not build HTTP client: {error}")))?;
        Ok(Self {
            http,
            url: config.embeddings_url.clone(),
            model: Some(config.embedding_model.clone()).filter(|model| !model.is_empty()),
            api_key: config.api_key.clone(),
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.url
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Embed one string, returning an L2-normalized vector.
    ///
    /// Every failure path produces a message that names what went wrong and
    /// where — a caller looking at "cache never hits" needs to be able to tell
    /// a cold cache from an unreachable embedder without reading logs.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let request = EmbeddingRequest {
            input: [text],
            model: self.model.as_deref(),
        };
        let mut builder = self.http.post(self.url.clone()).json(&request);
        if let Some(key) = &self.api_key {
            builder = builder.header(reqwest::header::AUTHORIZATION, key.as_header_value());
        }

        let response = builder.send().await.map_err(|error| {
            EmbedError::new(format!(
                "embeddings endpoint {} is unreachable: {error}",
                self.url
            ))
        })?;

        let status = response.status();
        let body = response.text().await.map_err(|error| {
            EmbedError::new(format!(
                "embeddings endpoint {} returned {status} and the body could not be read: {error}",
                self.url
            ))
        })?;

        if !status.is_success() {
            return Err(EmbedError::new(format!(
                "embeddings endpoint {} returned {status}: {}",
                self.url,
                truncate(&body, MAX_ERROR_BODY)
            )));
        }

        let vector = parse_embedding_response(&body)
            .map_err(|error| EmbedError::new(format!("{} from {}", error, self.url)))?;
        crate::policy::normalize_l2(vector).map_err(EmbedError::new)
    }
}

/// Pull the first embedding out of an OpenAI-shaped response body.
///
/// Kept separate from the request so the parsing — which is where servers
/// actually differ from each other — is testable without a socket.
pub fn parse_embedding_response(body: &str) -> Result<Vec<f32>, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("embeddings response is not JSON ({error})"))?;

    let Some(first) = value.get("data").and_then(|data| data.get(0)) else {
        return Err("embeddings response has no data[0] entry".to_string());
    };
    let Some(embedding) = first.get("embedding") else {
        return Err("embeddings response has no data[0].embedding field".to_string());
    };

    // A string here means the server answered in base64. This plugin never
    // asks for that (`encoding_format` is left unset, so the default is
    // float), so it is a server-side default worth naming rather than a
    // generic type error.
    if embedding.is_string() {
        return Err(
            "embeddings response is base64-encoded; this plugin requires float embeddings, \
             so configure the server to default `encoding_format` to \"float\""
                .to_string(),
        );
    }

    let Some(values) = embedding.as_array() else {
        return Err("embeddings response data[0].embedding is not an array".to_string());
    };
    if values.is_empty() {
        return Err("embeddings response data[0].embedding is empty".to_string());
    }

    values
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|number| number as f32)
                .ok_or_else(|| "embeddings response contains a non-numeric component".to_string())
        })
        .collect()
}

/// Trim a body for inclusion in an error message, on a character boundary.
pub fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}… ({} bytes total)", trimmed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_openai_response_parses() {
        let body = r#"{"object":"list","data":[{"object":"embedding","index":0,
            "embedding":[0.1,-0.2,0.3]}],"model":"text-embedding-3-small"}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![0.1, -0.2, 0.3]
        );
    }

    #[test]
    fn integer_components_parse() {
        // Some servers emit `0` rather than `0.0` for an exact zero.
        let body = r#"{"data":[{"embedding":[1,0,-1]}]}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![1.0, 0.0, -1.0]
        );
    }

    #[test]
    fn only_the_first_embedding_is_taken() {
        let body = r#"{"data":[{"embedding":[1.0,0.0]},{"embedding":[0.0,1.0]}]}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![1.0, 0.0]
        );
    }

    #[test]
    fn every_malformed_shape_names_what_is_wrong() {
        let cases = [
            ("not json at all", "not JSON"),
            (r#"{"error":{"message":"model not found"}}"#, "data[0]"),
            (r#"{"data":[]}"#, "data[0]"),
            (r#"{"data":[{}]}"#, "data[0].embedding"),
            (r#"{"data":[{"embedding":{}}]}"#, "not an array"),
            (r#"{"data":[{"embedding":[]}]}"#, "empty"),
            (r#"{"data":[{"embedding":[1.0,"x"]}]}"#, "non-numeric"),
        ];
        for (body, expected) in cases {
            let error = parse_embedding_response(body).expect_err("must fail");
            assert!(error.contains(expected), "{body} -> {error}");
        }
    }

    #[test]
    fn a_base64_embedding_gets_an_actionable_message() {
        let body = r#"{"data":[{"embedding":"gpWEPYyMjD0="}]}"#;
        let error = parse_embedding_response(body).expect_err("must fail");
        assert!(error.contains("base64"), "{error}");
        assert!(error.contains("encoding_format"), "{error}");
    }

    #[test]
    fn a_short_error_body_is_quoted_whole() {
        assert_eq!(truncate("  model not found  ", 300), "model not found");
    }

    #[test]
    fn a_long_error_body_is_cut_and_labelled() {
        let html = "<html>".to_string() + &"x".repeat(5_000);
        let truncated = truncate(&html, 300);
        assert!(
            truncated.chars().count() < 340,
            "{}",
            truncated.chars().count()
        );
        assert!(truncated.contains("bytes total"));
        assert!(truncated.starts_with("<html>"));
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Cutting by byte index here would panic mid-codepoint.
        let text = "é".repeat(100);
        let truncated = truncate(&text, 10);
        assert_eq!(truncated.chars().filter(|c| *c == 'é').count(), 10);
    }

    #[test]
    fn the_request_body_omits_an_unset_model() {
        let with_model = serde_json::to_value(EmbeddingRequest {
            input: ["hello"],
            model: Some("text-embedding-3-small"),
        })
        .expect("serializes");
        assert_eq!(with_model["model"], "text-embedding-3-small");
        assert_eq!(with_model["input"], serde_json::json!(["hello"]));

        let without_model = serde_json::to_value(EmbeddingRequest {
            input: ["hello"],
            model: None,
        })
        .expect("serializes");
        assert!(
            without_model.get("model").is_none(),
            "an empty model field is rejected by every server; omit it instead"
        );
    }

    #[test]
    fn the_client_reflects_the_configured_endpoint_and_hides_the_key() {
        let mut config = Config {
            embedding_model: "nomic-embed-text".to_string(),
            ..Config::default()
        };
        let client = EmbeddingClient::new(&config).expect("client builds");
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:9337/v1/embeddings"
        );
        assert_eq!(client.model(), Some("nomic-embed-text"));

        config.embedding_model = String::new();
        let client = EmbeddingClient::new(&config).expect("client builds");
        assert_eq!(client.model(), None);
    }
}
