//! The only place the ledger gets numbers from: the host's own
//! `GET /api/status`, read over loopback.
//!
//! Why polling and not events. A plugin can subscribe to mesh *lifecycle*
//! events (`peer_up`, `peer_down`, `local_accepting`, `local_standby`,
//! `mesh_id_updated`) and to mesh channel messages, and that is the whole of
//! what the host pushes to a plugin. There is no per-request event on that
//! path. The host's internal `tdcc-events` taxonomy — `RequestRouted`,
//! `ModelLoaded`, and friends — is delivered through an in-process
//! `OutputSink` that terminal and TUI renderers install; it is not exposed
//! across the plugin control connection and a plugin cannot subscribe to it.
//!
//! What *is* reachable is the management API's `routing_metrics` block, which
//! carries monotonic, process-lifetime counters. Sampling it and differencing
//! consecutive readings is therefore the honest maximum a plugin can observe
//! today. The gaps that leaves are enumerated in the README, not papered over.
//!
//! The network surface is deliberately tiny: one hand-rolled HTTP/1.1 GET to a
//! validated loopback address, with a byte cap and a timeout. No TLS stack, no
//! redirects, no proxy support, no DNS.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::LoopbackEndpoint;
use crate::journal::Cumulative;

/// Path the ledger reads. The only path it ever requests.
pub const STATUS_PATH: &str = "/api/status";

/// A status payload on a busy mesh carries every peer and every model; 4 MiB is
/// far past that and still bounds a runaway response.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Long enough for a loaded node to answer, short enough that a wedged host
/// cannot stall the sampler past its own poll interval.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What one successful poll told the ledger.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSample {
    pub node_id: Option<String>,
    pub mesh_id: Option<String>,
    /// `serving`, `loading`, `standby`, or `client`.
    pub node_state: Option<String>,
    /// Peer ids exactly as the host reports them; shortening happens later, at
    /// the point they would be written down.
    pub peers: Vec<String>,
    pub cumulative: Cumulative,
    /// Counter fields the payload did not carry. Non-empty means this host
    /// build exposes less than the ledger knows how to read, and the affected
    /// totals will read as zero rather than as missing.
    pub missing: Vec<&'static str>,
}

impl StatusSample {
    /// Whether the node reported it was in a state where it serves work.
    ///
    /// Used only as a fallback: `local_accepting` / `local_standby` mesh events
    /// are the primary signal because the host pushes them.
    pub fn accepting(&self) -> Option<bool> {
        self.node_state
            .as_deref()
            .map(|state| matches!(state, "serving" | "loading"))
    }
}

/// Fetch and parse `/api/status`.
pub async fn poll(endpoint: &LoopbackEndpoint) -> Result<StatusSample> {
    let body = fetch(endpoint, STATUS_PATH).await?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("host status response was not JSON")?;
    Ok(parse_status(&value))
}

/// Pull the fields the ledger understands out of a status payload.
///
/// Tolerant by design: a host that renames or drops a field must degrade to a
/// recorded gap, not to a crashed sampler. Every field it could not find is
/// named in [`StatusSample::missing`] so the `status` tool can show it.
pub fn parse_status(value: &serde_json::Value) -> StatusSample {
    let mut missing = Vec::new();
    let routing = value.get("routing_metrics");
    let pressure = routing.and_then(|routing| routing.get("pressure"));
    let local = routing.and_then(|routing| routing.get("local_node"));

    let mut read = |parent: Option<&serde_json::Value>, key: &'static str| -> u64 {
        match parent
            .and_then(|parent| parent.get(key))
            .and_then(serde_json::Value::as_u64)
        {
            Some(found) => found,
            None => {
                missing.push(key);
                0
            }
        }
    };

    let requests_fronted = read(routing, "request_count");
    let requests_succeeded = read(routing, "successful_requests");
    let served_locally = read(pressure, "locally_served_request_count");
    let served_remotely = read(pressure, "remotely_served_request_count");
    let served_by_endpoint = read(pressure, "endpoint_request_count");
    let local_attempts = read(local, "local_attempt_count");
    let remote_attempts = read(local, "remote_attempt_count");
    let endpoint_attempts = read(local, "endpoint_attempt_count");
    let completion_tokens = read(routing, "completion_tokens_observed");

    // The host publishes a running mean, not a total. Multiplying it back out
    // by the attempt count reconstructs the sum it was derived from, which is
    // the only form that differences correctly across polls.
    let attempts = local_attempts
        .saturating_add(remote_attempts)
        .saturating_add(endpoint_attempts);
    let avg_attempt_ms = routing
        .and_then(|routing| routing.get("avg_attempt_ms"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
        .max(0.0);
    let attempt_ms = (avg_attempt_ms * attempts as f64).round();
    let attempt_ms = if attempt_ms.is_finite() && attempt_ms >= 0.0 {
        attempt_ms.min(u64::MAX as f64) as u64
    } else {
        0
    };

    StatusSample {
        node_id: string_field(value, "node_id"),
        mesh_id: string_field(value, "mesh_id"),
        node_state: string_field(value, "node_state"),
        peers: value
            .get("peers")
            .and_then(serde_json::Value::as_array)
            .map(|peers| {
                peers
                    .iter()
                    .filter_map(|peer| peer.get("id").and_then(serde_json::Value::as_str))
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        cumulative: Cumulative {
            requests_fronted,
            requests_succeeded,
            served_locally,
            served_remotely,
            served_by_endpoint,
            local_attempts,
            remote_attempts,
            endpoint_attempts,
            completion_tokens,
            attempt_ms,
        },
        missing,
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|found| !found.is_empty())
        .map(str::to_string)
}

/// One HTTP/1.1 GET against a validated loopback address.
async fn fetch(endpoint: &LoopbackEndpoint, path: &str) -> Result<Vec<u8>> {
    let (ip, port) = endpoint.socket_addr();
    let address = SocketAddr::new(ip, port);
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Accept: application/json\r\n\
         User-Agent: tdcc-contribution-ledger/{version}\r\n\
         Connection: close\r\n\
         \r\n",
        authority = endpoint.authority,
        version = crate::PLUGIN_VERSION,
    );

    let bytes = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut stream = TcpStream::connect(address)
            .await
            .with_context(|| format!("connecting to {address}"))?;
        stream
            .write_all(request.as_bytes())
            .await
            .with_context(|| format!("sending {path} to {address}"))?;
        stream.flush().await.ok();

        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = stream
                .read(&mut chunk)
                .await
                .with_context(|| format!("reading {path} from {address}"))?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
            if raw.len() > MAX_RESPONSE_BYTES {
                bail!("host status response exceeded {MAX_RESPONSE_BYTES} bytes");
            }
            // Stop as soon as the declared body has arrived, rather than
            // waiting for the peer to close.
            if let Some((head_len, head)) = split_head(&raw)
                && let Some(length) = content_length(head)
                && raw.len() >= head_len + length
            {
                break;
            }
        }
        anyhow::Ok(raw)
    })
    .await
    .with_context(|| {
        format!("timed out after {REQUEST_TIMEOUT:?} reading {path} from {address}")
    })??;

    let (head_len, head) =
        split_head(&bytes).context("host status response had no complete header block")?;
    let status = status_code(head).context("host status response had no status line")?;
    if status != 200 {
        bail!("host answered {path} with HTTP {status}");
    }
    Ok(bytes[head_len..].to_vec())
}

/// Split a raw response into (offset of the body, header block).
fn split_head(raw: &[u8]) -> Option<(usize, &str)> {
    let end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    std::str::from_utf8(&raw[..end])
        .ok()
        .map(|head| (end + 4, head))
}

fn status_code(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_payload() -> serde_json::Value {
        serde_json::json!({
            "node_id": "node-abc",
            "mesh_id": "mesh-xyz",
            "node_state": "serving",
            "peers": [
                { "id": "peer-one", "role": "host" },
                { "id": "peer-two", "role": "client" },
                { "id": "" }
            ],
            "routing_metrics": {
                "request_count": 120,
                "successful_requests": 118,
                "completion_tokens_observed": 45_000,
                "avg_attempt_ms": 250.0,
                "local_node": {
                    "local_attempt_count": 100,
                    "remote_attempt_count": 20,
                    "endpoint_attempt_count": 5
                },
                "pressure": {
                    "locally_served_request_count": 95,
                    "remotely_served_request_count": 20,
                    "endpoint_request_count": 3
                }
            }
        })
    }

    #[test]
    fn a_full_payload_maps_onto_the_cumulative_counters() {
        let sample = parse_status(&status_payload());

        assert_eq!(sample.node_id.as_deref(), Some("node-abc"));
        assert_eq!(sample.mesh_id.as_deref(), Some("mesh-xyz"));
        assert_eq!(sample.accepting(), Some(true));
        assert_eq!(sample.peers, vec!["peer-one", "peer-two"]);
        assert!(sample.missing.is_empty(), "{:?}", sample.missing);

        assert_eq!(sample.cumulative.requests_fronted, 120);
        assert_eq!(sample.cumulative.served_locally, 95);
        assert_eq!(sample.cumulative.completion_tokens, 45_000);
        // 125 attempts * 250 ms mean = 31_250 ms of attempt time.
        assert_eq!(sample.cumulative.attempt_ms, 31_250);
    }

    #[test]
    fn a_standby_node_is_not_accepting() {
        let mut payload = status_payload();
        payload["node_state"] = serde_json::json!("standby");

        assert_eq!(parse_status(&payload).accepting(), Some(false));
    }

    #[test]
    fn missing_counters_are_named_rather_than_silently_zeroed() {
        let sample = parse_status(&serde_json::json!({ "node_id": "node-abc" }));

        assert_eq!(sample.cumulative, Cumulative::default());
        assert!(sample.missing.contains(&"request_count"));
        assert!(sample.missing.contains(&"locally_served_request_count"));
        assert!(sample.missing.contains(&"local_attempt_count"));
        assert_eq!(sample.node_state, None);
        assert_eq!(sample.accepting(), None);
    }

    #[test]
    fn a_nonsense_payload_does_not_panic() {
        for payload in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({ "routing_metrics": 7 }),
            serde_json::json!({ "peers": "not an array" }),
            serde_json::json!({ "routing_metrics": { "avg_attempt_ms": f64::MAX } }),
        ] {
            let sample = parse_status(&payload);
            assert!(sample.peers.is_empty());
        }
    }

    #[test]
    fn header_parsing_finds_the_body_and_the_status() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";

        let (offset, head) = split_head(raw).expect("headers are complete");

        assert_eq!(status_code(head), Some(200));
        assert_eq!(content_length(head), Some(2));
        assert_eq!(&raw[offset..], b"{}");
    }

    #[test]
    fn header_parsing_reports_an_incomplete_block() {
        assert!(split_head(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n").is_none());
        assert_eq!(status_code("garbage"), None);
        assert_eq!(content_length("HTTP/1.1 200 OK\r\n"), None);
    }

    /// Serve one canned response on a loopback port and hand back its endpoint.
    ///
    /// This exercises the hand-rolled client end to end — connect, request,
    /// header split, `Content-Length` short-circuit — rather than only the
    /// pure parsers.
    async fn serve_once(response: String) -> LoopbackEndpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Drain the request line and headers before answering.
            let mut scratch = [0u8; 1024];
            let _ = stream.read(&mut scratch).await;
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });
        crate::config::parse_loopback_base(&format!("http://127.0.0.1:{port}")).unwrap()
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn a_real_loopback_round_trip_parses_the_payload() {
        let endpoint = serve_once(http_ok(&status_payload().to_string())).await;

        let sample = poll(&endpoint).await.expect("a 200 with JSON is readable");

        assert_eq!(sample.node_id.as_deref(), Some("node-abc"));
        assert_eq!(sample.cumulative.requests_fronted, 120);
    }

    #[tokio::test]
    async fn a_non_200_is_an_error_not_an_empty_sample() {
        let endpoint =
            serve_once("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_string())
                .await;

        let error = poll(&endpoint).await.expect_err("503 is a failure");

        assert!(error.to_string().contains("503"), "{error}");
    }

    #[tokio::test]
    async fn a_non_json_body_is_an_error_not_an_empty_sample() {
        let endpoint = serve_once(http_ok("<html>not json</html>")).await;

        let error = poll(&endpoint)
            .await
            .expect_err("HTML is not a status payload");

        assert!(error.to_string().contains("not JSON"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_port_is_a_connection_error() {
        // Bind, read the port, then drop the listener so nothing is listening.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let endpoint =
            crate::config::parse_loopback_base(&format!("http://127.0.0.1:{port}")).unwrap();

        assert!(poll(&endpoint).await.is_err(), "nothing is listening");
    }
}
