//! Reading node state from the local `tdcc` HTTP API.
//!
//! Everything the exporter publishes comes from a single `GET /api/status` on
//! the operator console port. One request per scrape keeps the exporter cheap
//! and keeps the exposition internally consistent — every series in a scrape
//! describes the same instant, rather than being stitched together from
//! endpoints sampled seconds apart.
//!
//! ## What is deliberately not read
//!
//! - `/api/models` returns the **mesh catalogue**, not the loaded set. Turning
//!   it into series would put one label set per known model into Prometheus,
//!   which is exactly the cardinality mistake this plugin is supposed to avoid.
//! - `/api/runtime/llama` carries llama.cpp's own metrics (KV cache usage, slot
//!   occupancy). Useful, but it is a second request, it only exists for one
//!   backend, and its contents are whatever the upstream server happens to
//!   expose. Left out on purpose; see README > "What this cannot reach".
//! - `StatusPayload` contains a `token` field. It is **not** in the structs
//!   below, so it is dropped during deserialization and can never reach the
//!   exposition. Do not add it.

use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::settings::NodeEndpoint;

/// Path on the node API that carries everything the exporter needs.
pub const STATUS_PATH: &str = "/api/status";

/// Hard ceiling on a status response. A mesh large enough to exceed this has
/// bigger problems than metrics; failing loudly beats buffering without bound.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The subset of `GET /api/status` the exporter understands.
///
/// Unknown fields are ignored by serde, so a newer node adding payload fields
/// does not break an older exporter. Every field is `#[serde(default)]` so a
/// node that omits one (older build, different role) yields a zero rather than
/// a failed scrape.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeStatus {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub node_id: String,
    /// `client`, `standby`, `loading` or `serving`.
    #[serde(default)]
    pub node_state: String,
    #[serde(default)]
    pub is_host: bool,
    #[serde(default)]
    pub is_client: bool,
    /// Whether the local serving runtime reports itself ready.
    #[serde(default)]
    pub llama_ready: bool,
    #[serde(default)]
    pub my_hostname: Option<String>,
    #[serde(default)]
    pub mesh_id: Option<String>,
    /// Node VRAM in decimal gigabytes, as the node advertises it to the mesh.
    #[serde(default)]
    pub my_vram_gb: f64,
    /// On-disk size of the primary model, in decimal gigabytes.
    #[serde(default)]
    pub model_size_gb: f64,
    #[serde(default)]
    pub inflight_requests: u64,
    #[serde(default)]
    pub runtime: RuntimeStatus,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default)]
    pub routing_metrics: RoutingMetrics,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RuntimeStatus {
    /// One entry per local model process, including ones still starting.
    #[serde(default)]
    pub models: Vec<RuntimeModel>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RuntimeModel {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub backend: String,
    /// Free-form runtime status: `ready`, `starting`, `error`, …
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub context_length: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Gpu {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub vram_bytes: u64,
    /// VRAM the node holds back from model placement.
    #[serde(default)]
    pub reserved_bytes: Option<u64>,
    /// VRAM the node considers placeable.
    #[serde(default)]
    pub allocatable_vram_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Peer {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub vram_gb: f64,
    #[serde(default)]
    pub version: Option<String>,
    /// Direct round-trip time, when this node has measured one.
    #[serde(default)]
    pub rtt_ms: Option<u32>,
    /// Latency the router uses, which may be an estimate rather than a probe.
    #[serde(default)]
    pub latency_ms: Option<u32>,
    /// Age of that latency reading — a fresh 5 ms and an hour-old 5 ms are very
    /// different things to an operator.
    #[serde(default)]
    pub latency_age_ms: Option<u64>,
}

/// Local-only routing counters. The node states plainly that these describe
/// requests **this** node fronted, not mesh-wide totals; the HELP strings in
/// `render` repeat that so nobody builds a fleet dashboard on the wrong idea.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RoutingMetrics {
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub successful_requests: u64,
    #[serde(default)]
    pub retry_count: u64,
    #[serde(default)]
    pub failover_count: u64,
    #[serde(default)]
    pub attempt_timeout_count: u64,
    #[serde(default)]
    pub attempt_unavailable_count: u64,
    #[serde(default)]
    pub attempt_context_overflow_count: u64,
    #[serde(default)]
    pub attempt_reject_count: u64,
    /// Cumulative mean queue wait, in milliseconds, over all attempts.
    #[serde(default)]
    pub avg_queue_wait_ms: f64,
    /// Cumulative mean attempt duration, in milliseconds, over all attempts.
    #[serde(default)]
    pub avg_attempt_ms: f64,
    /// Cumulative mean generation throughput; absent until a sample exists.
    #[serde(default)]
    pub avg_tokens_per_second: Option<f64>,
    #[serde(default)]
    pub completion_tokens_observed: u64,
    #[serde(default)]
    pub throughput_samples: u64,
    #[serde(default)]
    pub local_node: LocalNodePressure,
    #[serde(default)]
    pub pressure: RoutingPressure,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct LocalNodePressure {
    // `current_inflight_requests` also lives here, but it repeats the
    // top-level `inflight_requests`, so it is left out rather than exported
    // twice under two names.
    #[serde(default)]
    pub peak_inflight_requests: u64,
    #[serde(default)]
    pub local_attempt_count: u64,
    #[serde(default)]
    pub remote_attempt_count: u64,
    #[serde(default)]
    pub endpoint_attempt_count: u64,
}

impl LocalNodePressure {
    /// Total routing attempts.
    ///
    /// The node exposes the three per-target counters but not their sum, and
    /// every attempt increments exactly one of them — which makes this the
    /// denominator behind `avg_attempt_ms` and `avg_queue_wait_ms`, and so the
    /// `_count` of the latency summaries.
    pub fn attempt_count(&self) -> u64 {
        self.local_attempt_count
            .saturating_add(self.remote_attempt_count)
            .saturating_add(self.endpoint_attempt_count)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RoutingPressure {
    #[serde(default)]
    pub locally_served_request_count: u64,
    #[serde(default)]
    pub remotely_served_request_count: u64,
    #[serde(default)]
    pub endpoint_request_count: u64,
}

/// Fetch and decode `GET /api/status`.
///
/// Errors are strings rather than a typed error because their only consumers
/// are a `# comment` line in the exposition and the `check` tool's message —
/// both of which want one readable sentence.
pub async fn fetch_status(
    endpoint: &NodeEndpoint,
    timeout: Duration,
) -> Result<NodeStatus, String> {
    let body = tokio::time::timeout(timeout, request_status(endpoint))
        .await
        .map_err(|_| {
            format!(
                "timed out after {} ms reading {}{STATUS_PATH}",
                timeout.as_millis(),
                endpoint.base_url()
            )
        })??;

    serde_json::from_str::<NodeStatus>(&body).map_err(|error| {
        format!(
            "{}{STATUS_PATH} returned unreadable JSON: {error}",
            endpoint.base_url()
        )
    })
}

async fn request_status(endpoint: &NodeEndpoint) -> Result<String, String> {
    let mut stream = TcpStream::connect((endpoint.connect_host(), endpoint.port()))
        .await
        .map_err(|error| format!("cannot reach {}: {error}", endpoint.base_url()))?;

    // Belt and braces with `settings::parse_node_url`: the URL was checked to be
    // a loopback host, and this checks that the address it actually resolved to
    // is loopback too. A hosts-file entry pointing "localhost" somewhere else
    // does not get to turn this plugin into an outbound network client.
    let peer = stream.peer_addr().map_err(|error| {
        format!(
            "cannot read peer address for {}: {error}",
            endpoint.base_url()
        )
    })?;
    if !peer.ip().is_loopback() {
        return Err(format!(
            "{} resolved to non-loopback address {}; refusing to send the request",
            endpoint.base_url(),
            peer.ip()
        ));
    }

    let request = format!(
        "GET {STATUS_PATH} HTTP/1.1\r\n\
         Host: {}\r\n\
         Accept: application/json\r\n\
         User-Agent: tdcc-prometheus-exporter/{}\r\n\
         Connection: close\r\n\r\n",
        endpoint.authority(),
        crate::EXPORTER_VERSION,
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("failed sending request to {}: {error}", endpoint.base_url()))?;

    read_response(&mut stream, endpoint).await
}

async fn read_response(stream: &mut TcpStream, endpoint: &NodeEndpoint) -> Result<String, String> {
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    let mut head: Option<ResponseHead> = None;

    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed reading from {}: {error}", endpoint.base_url()))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "{}{STATUS_PATH} returned more than {MAX_RESPONSE_BYTES} bytes",
                endpoint.base_url()
            ));
        }

        if head.is_none() {
            head = parse_response_head(&buffer)?;
        }
        // Stop as soon as the declared body has arrived. Reading to EOF instead
        // would stall for the full timeout whenever the node keeps the
        // connection alive.
        if let Some(head) = &head
            && let Some(length) = head.content_length
            && buffer.len() >= head.header_len + length
        {
            break;
        }
    }

    let head = head.ok_or_else(|| {
        format!(
            "{}{STATUS_PATH} closed the connection before sending a complete response header",
            endpoint.base_url()
        )
    })?;

    let body = &buffer[head.header_len.min(buffer.len())..];
    let body = match head.content_length {
        Some(length) if body.len() >= length => &body[..length],
        Some(length) => {
            return Err(format!(
                "{}{STATUS_PATH} promised {length} body bytes and delivered {}",
                endpoint.base_url(),
                body.len()
            ));
        }
        None => body,
    };

    if head.status != 200 {
        return Err(format!(
            "{}{STATUS_PATH} returned HTTP {}: {}",
            endpoint.base_url(),
            head.status,
            first_line(body)
        ));
    }

    String::from_utf8(body.to_vec()).map_err(|_| {
        format!(
            "{}{STATUS_PATH} returned a non-UTF-8 body",
            endpoint.base_url()
        )
    })
}

/// What the exporter needs out of a response header block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    /// Byte offset of the first body byte.
    pub header_len: usize,
    pub content_length: Option<usize>,
}

/// Parse a response header block, returning `None` while it is still partial.
///
/// The node API answers with `Content-Length`, so chunked transfer is treated as
/// an explicit "this exporter does not implement that" rather than being decoded
/// incorrectly and silently.
pub fn parse_response_head(buffer: &[u8]) -> Result<Option<ResponseHead>, String> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = response
        .parse(buffer)
        .map_err(|error| format!("node API sent a malformed HTTP response: {error}"))?;

    let header_len = match parsed {
        httparse::Status::Complete(length) => length,
        httparse::Status::Partial => return Ok(None),
    };
    let status = response
        .code
        .ok_or_else(|| "node API sent a response without a status code".to_string())?;

    let mut content_length = None;
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(header.value)
                .to_ascii_lowercase()
                .contains("chunked")
        {
            return Err(
                "node API used chunked transfer encoding, which this exporter does not decode"
                    .to_string(),
            );
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let raw = std::str::from_utf8(header.value)
                .map_err(|_| "node API sent a non-UTF-8 Content-Length".to_string())?;
            content_length = Some(
                raw.trim()
                    .parse::<usize>()
                    .map_err(|_| format!("node API sent an invalid Content-Length '{raw}'"))?,
            );
        }
    }

    Ok(Some(ResponseHead {
        status,
        header_len,
        content_length,
    }))
}

/// First line of an error body, trimmed, for use inside a one-line message.
fn first_line(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_header_block_is_not_an_error() {
        let partial = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n";
        assert_eq!(parse_response_head(partial), Ok(None));
    }

    #[test]
    fn content_length_and_status_are_extracted() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let head = parse_response_head(raw)
            .expect("well formed")
            .expect("complete");
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(2));
        assert_eq!(&raw[head.header_len..], b"{}");
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let raw = b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\n\r\nnope";
        let head = parse_response_head(raw)
            .expect("well formed")
            .expect("complete");
        assert_eq!(head.status, 503);
        assert_eq!(head.content_length, Some(4));
    }

    #[test]
    fn chunked_responses_are_refused_rather_than_mis_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        let error = parse_response_head(raw).expect_err("chunked is refused");
        assert!(error.contains("chunked"), "{error}");
    }

    #[test]
    fn an_invalid_content_length_is_reported_not_ignored() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: twelve\r\n\r\n";
        assert!(parse_response_head(raw).is_err());
    }

    #[test]
    fn attempt_count_sums_the_three_target_kinds() {
        let pressure = LocalNodePressure {
            local_attempt_count: 7,
            remote_attempt_count: 3,
            endpoint_attempt_count: 1,
            ..LocalNodePressure::default()
        };
        assert_eq!(pressure.attempt_count(), 11);
    }

    #[test]
    fn status_decoding_tolerates_a_minimal_payload_and_drops_the_token() {
        // A node that reports almost nothing must still yield a usable status
        // rather than failing the scrape.
        let status: NodeStatus = serde_json::from_str(r#"{"node_id":"abc","token":"s3cret"}"#)
            .expect("sparse payload decodes");
        assert_eq!(status.node_id, "abc");
        assert_eq!(status.peers.len(), 0);
        assert_eq!(status.routing_metrics.request_count, 0);
        // `token` has no field to land in; the compiler enforces that better
        // than any assertion, but the fixture keeps it in front of a reader.
        let json = serde_json::to_string(&serde_json::json!({ "kept": status.node_id }))
            .expect("serializes");
        assert!(!json.contains("s3cret"));
    }

    #[test]
    fn status_decoding_reads_the_fields_the_exporter_publishes() {
        let raw = r#"{
            "version": "0.72.1",
            "node_id": "node-1",
            "node_state": "serving",
            "is_host": true,
            "is_client": false,
            "llama_ready": true,
            "my_hostname": "workstation",
            "mesh_id": "mesh-7",
            "my_vram_gb": 24.0,
            "model_size_gb": 4.5,
            "inflight_requests": 2,
            "runtime": { "models": [
                { "name": "qwen3", "backend": "cuda", "status": "ready", "context_length": 8192 }
            ] },
            "gpus": [
                { "name": "RTX 4090", "vram_bytes": 25769803776, "reserved_bytes": 1073741824,
                  "allocatable_vram_bytes": 24696061952 }
            ],
            "peers": [
                { "id": "peer-a", "role": "host", "state": "serving", "vram_gb": 48.0,
                  "version": "0.72.1", "rtt_ms": 12, "latency_ms": 14, "latency_age_ms": 3000 }
            ],
            "routing_metrics": {
                "request_count": 10, "successful_requests": 9, "retry_count": 2,
                "failover_count": 1, "attempt_timeout_count": 1,
                "attempt_unavailable_count": 0, "attempt_context_overflow_count": 0,
                "attempt_reject_count": 1, "avg_queue_wait_ms": 5.0, "avg_attempt_ms": 250.0,
                "avg_tokens_per_second": 42.5, "completion_tokens_observed": 1234,
                "throughput_samples": 9,
                "local_node": { "current_inflight_requests": 2, "peak_inflight_requests": 6,
                                "local_attempt_count": 8, "remote_attempt_count": 3,
                                "endpoint_attempt_count": 1 },
                "pressure": { "locally_served_request_count": 7,
                              "remotely_served_request_count": 2,
                              "endpoint_request_count": 1 }
            }
        }"#;
        let status: NodeStatus = serde_json::from_str(raw).expect("full payload decodes");
        assert_eq!(status.runtime.models[0].context_length, Some(8192));
        assert_eq!(status.gpus[0].vram_bytes, 25_769_803_776);
        assert_eq!(status.peers[0].rtt_ms, Some(12));
        assert_eq!(status.routing_metrics.local_node.attempt_count(), 12);
        assert_eq!(status.routing_metrics.avg_tokens_per_second, Some(42.5));
    }
}
