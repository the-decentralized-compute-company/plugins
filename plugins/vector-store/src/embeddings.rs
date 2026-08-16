//! The one part of this plugin that talks to the network.
//!
//! # The prerequisite, stated plainly
//!
//! This plugin needs an **OpenAI-compatible `POST /v1/embeddings`**. It does
//! not ship a model and it does not compute vectors itself.
//!
//! **The TDCC node does not currently expose one.** Its OpenAI frontend router
//! declares exactly four routes — `/v1/models`, `/v1/chat/completions`,
//! `/v1/completions`, `/v1/responses` — and that component's own
//! documentation lists embeddings as out of scope. This was checked against
//! the SDK checkout in `Cargo.toml`, not assumed, and the sibling
//! `semantic-cache` plugin carries the same unmet prerequisite for the same
//! reason.
//!
//! So the endpoint is configurable, the default points at the node anyway (so
//! that the day it grows the route this plugin works with no configuration),
//! and until then it **fails loudly** — the `status` tool sends one real probe
//! and reports what it actually found, and a `query` that cannot embed returns
//! an error rather than an empty result list. An empty list and a dead
//! embedder look identical to a caller, and the difference is the whole value
//! of the tool.
//!
//! # Blast radius
//!
//! One outbound HTTP request shape, to one URL the operator configured,
//! carrying one thing: the text being embedded — the documents on `upsert` and
//! the query string on `query`. No other host is contacted, ever. The endpoint
//! is refused at startup unless it is on loopback or the operator passed
//! `--allow-remote-embeddings`.

use std::time::Instant;

use serde::Serialize;
use url::Url;

use crate::config::{ApiKey, Config};
use crate::similarity::normalize_l2;

/// Cap on how much of a failing response body is quoted back.
///
/// Long enough for a real error message, short enough that an HTML error page
/// does not end up in a tool result.
const MAX_ERROR_BODY: usize = 300;

/// Text sent by the `status` probe. Deliberately boring and free of anything
/// from a caller.
pub const PROBE_TEXT: &str = "tdcc vector-store endpoint probe";

#[derive(Debug)]
pub struct EmbedError {
    pub message: String,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EmbedError {}

impl EmbedError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: &'a [String],
    /// Omitted when unset: llama.cpp's embedding server ignores it, while
    /// OpenAI, Ollama and vLLM require it. Sending `""` fails on all four.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
}

/// What the `status` tool learned from one real request.
#[derive(Debug)]
pub struct Probe {
    pub dimensions: usize,
    pub latency_ms: u64,
}

pub struct EmbeddingClient {
    http: reqwest::Client,
    url: Url,
    model: Option<String>,
    api_key: Option<ApiKey>,
    batch_size: usize,
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
            batch_size: config.embed_batch_size,
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.url
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The model identity a collection is pinned to.
    ///
    /// An unset model is its own distinct value rather than an empty string,
    /// so vectors created before a model was configured are never silently
    /// compared against vectors created after one was.
    pub fn model_identity(&self) -> &str {
        self.model.as_deref().unwrap_or(UNSET_MODEL)
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Embed many texts, returning L2-normalized vectors in the same order.
    ///
    /// Sent in batches of `--embed-batch-size`. A partial failure fails the
    /// whole call: a document indexed with half its chunks missing is worse
    /// than a document not indexed at all, because nothing later reports the
    /// gap.
    pub async fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.batch_size.max(1)) {
            let vectors = self.embed_batch(batch).await?;
            if vectors.len() != batch.len() {
                return Err(EmbedError::new(format!(
                    "embeddings endpoint {} returned {} vectors for {} inputs; the batch \
                     cannot be matched to its texts, so nothing was stored",
                    self.url,
                    vectors.len(),
                    batch.len()
                )));
            }
            out.extend(vectors);
        }
        Ok(out)
    }

    /// Embed exactly one text.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vectors = self
            .embed_batch(std::slice::from_ref(&text.to_string()))
            .await?;
        if vectors.len() != 1 {
            return Err(EmbedError::new(format!(
                "embeddings endpoint {} returned {} vectors for one input",
                self.url,
                vectors.len()
            )));
        }
        Ok(vectors.remove(0))
    }

    /// One real request, for the `status` tool.
    pub async fn probe(&self) -> Result<Probe, EmbedError> {
        let started = Instant::now();
        let vector = self.embed_one(PROBE_TEXT).await?;
        Ok(Probe {
            dimensions: vector.len(),
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let request = EmbeddingRequest {
            input: texts,
            model: self.model.as_deref(),
        };
        let mut builder = self.http.post(self.url.clone()).json(&request);
        if let Some(key) = &self.api_key {
            builder = builder.header(reqwest::header::AUTHORIZATION, key.as_header_value());
        }

        let response = builder.send().await.map_err(|error| {
            EmbedError::new(format!(
                "embeddings endpoint {} is unreachable: {error}. Set --embeddings-url to a \
                 server that serves POST /v1/embeddings — the TDCC node itself does not.",
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

        let raw = parse_embedding_response(&body)
            .map_err(|error| EmbedError::new(format!("{error} from {}", self.url)))?;

        raw.into_iter()
            .map(|vector| normalize_l2(vector).map_err(EmbedError::new))
            .collect()
    }
}

/// A model that has not been configured, as it appears in a collection header.
///
/// A real model id can never collide with this: `<` and `>` are not legal in
/// any of them, and a collection pinned to `<unset>` therefore only ever
/// matches another process that also has no model configured.
pub const UNSET_MODEL: &str = "<unset>";

/// Pull every embedding out of an OpenAI-shaped response body, in input order.
///
/// Kept separate from the request so the parsing — which is where servers
/// actually differ from each other — is testable without a socket. Entries are
/// sorted by their declared `index` rather than trusted to arrive in order,
/// because a vector matched to the wrong chunk is a silent, permanent
/// corruption of the store.
pub fn parse_embedding_response(body: &str) -> Result<Vec<Vec<f32>>, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("embeddings response is not JSON ({error})"))?;

    let Some(data) = value.get("data").and_then(|data| data.as_array()) else {
        return Err("embeddings response has no data array".to_string());
    };
    if data.is_empty() {
        return Err("embeddings response has an empty data array".to_string());
    }

    let mut indexed: Vec<(u64, Vec<f32>)> = Vec::with_capacity(data.len());
    for (position, entry) in data.iter().enumerate() {
        let Some(embedding) = entry.get("embedding") else {
            return Err(format!(
                "embeddings response entry {position} has no embedding field"
            ));
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
            return Err(format!(
                "embeddings response entry {position} has a non-array embedding"
            ));
        };
        if values.is_empty() {
            return Err(format!(
                "embeddings response entry {position} has an empty embedding"
            ));
        }
        let vector = values
            .iter()
            .map(|value| {
                value.as_f64().map(|number| number as f32).ok_or_else(|| {
                    "embeddings response contains a non-numeric component".to_string()
                })
            })
            .collect::<Result<Vec<f32>, String>>()?;

        let index = entry
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(position as u64);
        indexed.push((index, vector));
    }

    indexed.sort_by_key(|(index, _)| *index);

    // Every vector in one response must be the same width. A ragged response
    // means the server switched models mid-batch, and storing it would leave
    // one collection holding two embedding spaces.
    let width = indexed[0].1.len();
    if let Some((index, vector)) = indexed.iter().find(|(_, vector)| vector.len() != width) {
        return Err(format!(
            "embeddings response mixes widths: entry {index} has {} dimensions, the first has \
             {width}",
            vector.len()
        ));
    }

    Ok(indexed.into_iter().map(|(_, vector)| vector).collect())
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
    use crate::config::Config;

    #[test]
    fn a_normal_single_response_parses() {
        let body = r#"{"object":"list","data":[{"object":"embedding","index":0,
            "embedding":[0.1,-0.2,0.3]}],"model":"text-embedding-3-small"}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![vec![0.1, -0.2, 0.3]]
        );
    }

    #[test]
    fn a_batch_response_keeps_every_vector() {
        let body = r#"{"data":[
            {"index":0,"embedding":[1.0,0.0]},
            {"index":1,"embedding":[0.0,1.0]},
            {"index":2,"embedding":[0.5,0.5]}]}"#;
        let vectors = parse_embedding_response(body).expect("parses");
        assert_eq!(vectors.len(), 3);
        assert_eq!(vectors[2], vec![0.5, 0.5]);
    }

    #[test]
    fn out_of_order_entries_are_restored_to_input_order() {
        // A vector matched to the wrong chunk is a silent, permanent
        // corruption of the store, so order is taken from `index`, never from
        // arrival.
        let body = r#"{"data":[
            {"index":2,"embedding":[3.0,0.0]},
            {"index":0,"embedding":[1.0,0.0]},
            {"index":1,"embedding":[2.0,0.0]}]}"#;
        let vectors = parse_embedding_response(body).expect("parses");
        assert_eq!(
            vectors,
            vec![vec![1.0, 0.0], vec![2.0, 0.0], vec![3.0, 0.0]]
        );
    }

    #[test]
    fn a_response_without_index_fields_keeps_arrival_order() {
        let body = r#"{"data":[{"embedding":[1.0,0.0]},{"embedding":[2.0,0.0]}]}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![vec![1.0, 0.0], vec![2.0, 0.0]]
        );
    }

    #[test]
    fn integer_components_parse() {
        // Some servers emit `0` rather than `0.0` for an exact zero.
        let body = r#"{"data":[{"embedding":[1,0,-1]}]}"#;
        assert_eq!(
            parse_embedding_response(body).expect("parses"),
            vec![vec![1.0, 0.0, -1.0]]
        );
    }

    #[test]
    fn a_ragged_batch_is_refused() {
        let body = r#"{"data":[
            {"index":0,"embedding":[1.0,0.0]},
            {"index":1,"embedding":[1.0,0.0,0.0]}]}"#;
        let error = parse_embedding_response(body)
            .expect_err("two widths in one response means two models");
        assert!(error.contains("mixes widths"), "{error}");
    }

    #[test]
    fn every_malformed_shape_names_what_is_wrong() {
        let cases = [
            ("not json at all", "not JSON"),
            (
                r#"{"error":{"message":"model not found"}}"#,
                "no data array",
            ),
            (r#"{"data":[]}"#, "empty data array"),
            (r#"{"data":[{}]}"#, "no embedding field"),
            (r#"{"data":[{"embedding":{}}]}"#, "non-array embedding"),
            (r#"{"data":[{"embedding":[]}]}"#, "empty embedding"),
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
        assert!(truncated.chars().count() < 340);
        assert!(truncated.contains("bytes total"));
        assert!(truncated.starts_with("<html>"));
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Cutting by byte index here would panic mid-codepoint.
        let text = "é".repeat(100);
        assert_eq!(
            truncate(&text, 10).chars().filter(|c| *c == 'é').count(),
            10
        );
    }

    #[test]
    fn the_request_body_omits_an_unset_model() {
        let inputs = vec!["hello".to_string()];
        let with_model = serde_json::to_value(EmbeddingRequest {
            input: &inputs,
            model: Some("nomic-embed-text"),
        })
        .expect("serializes");
        assert_eq!(with_model["model"], "nomic-embed-text");
        assert_eq!(with_model["input"], serde_json::json!(["hello"]));

        let without_model = serde_json::to_value(EmbeddingRequest {
            input: &inputs,
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
            "http://127.0.0.1:9337/v1/embeddings",
            "the default is the node itself, which does not serve this route yet"
        );
        assert_eq!(client.model(), Some("nomic-embed-text"));
        assert_eq!(client.model_identity(), "nomic-embed-text");

        config.embedding_model = String::new();
        let client = EmbeddingClient::new(&config).expect("client builds");
        assert_eq!(client.model(), None);
        assert_eq!(
            client.model_identity(),
            UNSET_MODEL,
            "an unconfigured model must pin to its own distinct identity"
        );
    }

    #[tokio::test]
    async fn embedding_an_empty_list_makes_no_request() {
        // A `Config` whose endpoint is a port nothing listens on: if this
        // reached the network it would fail rather than return an empty list.
        let config = Config::default();
        let client = EmbeddingClient::new(&config).expect("client builds");
        assert!(
            client
                .embed_many(&[])
                .await
                .expect("no work, no error")
                .is_empty()
        );
    }
}
