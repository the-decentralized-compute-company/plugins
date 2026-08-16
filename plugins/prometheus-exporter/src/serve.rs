//! Answering the `/metrics` scrape.
//!
//! ## Why this route is different from every other route in the examples
//!
//! A buffered plugin HTTP binding is a JSON operation: the host calls the
//! handler, takes the JSON it returns, and writes it out as
//! `Content-Type: application/json`. Prometheus cannot scrape JSON — it needs
//! `text/plain; version=0.0.4`, and the plugin has to be the one to say so.
//!
//! Declaring `.stream_response()` on the binding changes the host's behaviour:
//! instead of invoking the operation, it negotiates a short-lived side stream,
//! forwards the raw HTTP request down it, and copies whatever bytes come back
//! straight through to the client. So this module writes a complete HTTP/1.1
//! response, status line and headers included, and that is what Prometheus
//! receives.
//!
//! The plugin still opens no listener of its own. The side stream is a local
//! socket (or named pipe) that the *host* connects to after this code accepts
//! the negotiation, and it lives for exactly one scrape.
//!
//! ## Why the request is read up to the header terminator and no further
//!
//! The host half-closes the side stream after forwarding the request, which is
//! a real EOF on a Unix socket and nothing at all on a Windows named pipe.
//! Reading to EOF would therefore work on one platform and hang on the other,
//! so this reads to `\r\n\r\n` and stops. `GET /metrics` carries no body.

use std::sync::Arc;
use std::time::Duration;

use tdcc_plugin::{LocalListener, PluginError, PluginResult, bind_side_stream, proto};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::collector::Collector;
use crate::render::CONTENT_TYPE;

/// Binding id declared for `GET /metrics`, and therefore also the MCP tool name
/// the host projects for it (`prometheus-exporter.metrics`).
pub const METRICS_BINDING_ID: &str = "metrics";

/// How long to wait for the host to connect to a side stream it asked for.
///
/// Without this, a host that negotiates a stream and then gives up would leave
/// a task and a bound endpoint behind for the life of the process.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the forwarded request headers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Refuse a request header block larger than this rather than buffering it.
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// Handle a host stream negotiation for the `/metrics` binding.
///
/// Returns quickly: it binds the side stream, hands the endpoint back to the
/// host, and does the actual work in a spawned task, so the control connection
/// is never held open across a collection.
pub async fn open_metrics_stream(
    collector: Arc<Collector>,
    request: proto::OpenStreamRequest,
) -> PluginResult<Option<proto::OpenStreamResponse>> {
    if let Some(binding_id) = binding_id_of(&request)
        && binding_id != METRICS_BINDING_ID
    {
        return Ok(Some(reject(
            &request,
            format!("prometheus-exporter has no streamed binding '{binding_id}'"),
        )));
    }

    let listener = bind_side_stream(crate::PLUGIN_NAME, &request.stream_id)
        .await
        .map_err(|error| {
            PluginError::internal(format!("failed to bind the /metrics side stream: {error}"))
        })?;
    let response = listener.open_stream_response(&request);

    tokio::spawn(async move {
        if let Err(error) = answer_scrape(collector, listener).await {
            // The host has already been told the stream was accepted, so the
            // only thing left is to make the failure visible in the plugin's
            // stderr, which the host captures.
            eprintln!("prometheus-exporter: /metrics scrape failed: {error}");
        }
    });

    Ok(Some(response))
}

async fn answer_scrape(collector: Arc<Collector>, listener: LocalListener) -> Result<(), String> {
    let stream = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "host never connected to the side stream".to_string())?
        .map_err(|error| format!("side stream accept failed: {error}"))?;

    let (mut read, mut write) = stream.into_split();
    tokio::time::timeout(REQUEST_TIMEOUT, read_request_head(&mut read))
        .await
        .map_err(|_| "host did not forward the request headers in time".to_string())??;

    let scrape = collector.scrape().await;
    let response = http_response(&scrape.body);
    write
        .write_all(&response)
        .await
        .map_err(|error| format!("failed writing the scrape response: {error}"))?;
    write
        .flush()
        .await
        .map_err(|error| format!("failed flushing the scrape response: {error}"))?;
    // Close the write half so the host's copy loop sees the end of the response
    // instead of waiting on an idle stream.
    let _ = write.shutdown().await;
    Ok(())
}

/// Read exactly as far as the end of the request header block.
async fn read_request_head<R>(read: &mut R) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let count = read
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed reading the forwarded request: {error}"))?;
        if count == 0 {
            // A Unix host half-closes after the request; that is a complete
            // read, not a truncated one, provided the terminator arrived.
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(format!(
                "forwarded request headers exceeded {MAX_REQUEST_HEAD_BYTES} bytes"
            ));
        }
        if find_head_end(&buffer).is_some() {
            break;
        }
    }
    if find_head_end(&buffer).is_none() {
        return Err("forwarded request ended before its header terminator".to_string());
    }
    Ok(buffer)
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Build the complete HTTP/1.1 response Prometheus will receive verbatim.
pub fn http_response(body: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {CONTENT_TYPE}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body.as_bytes());
    response
}

fn binding_id_of(request: &proto::OpenStreamRequest) -> Option<String> {
    let metadata = request.metadata_json.as_deref()?;
    serde_json::from_str::<serde_json::Value>(metadata)
        .ok()?
        .get("binding_id")?
        .as_str()
        .map(str::to_string)
}

fn reject(request: &proto::OpenStreamRequest, message: String) -> proto::OpenStreamResponse {
    proto::OpenStreamResponse {
        stream_id: request.stream_id.clone(),
        accepted: false,
        transport_kind: proto::StreamTransportKind::Unspecified as i32,
        endpoint: None,
        token: None,
        expires_at_unix_ms: None,
        message: Some(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_response_declares_the_prometheus_content_type_and_length() {
        let body = "# HELP tdcc_up ok\n# TYPE tdcc_up gauge\ntdcc_up 1\n";
        let response = String::from_utf8(http_response(body)).expect("ascii response");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n"));
        assert!(response.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(response.ends_with(body));
    }

    #[test]
    fn the_header_terminator_is_found_and_nothing_beyond_it_is_required() {
        assert_eq!(find_head_end(b"GET /metrics HTTP/1.1\r\n\r\n"), Some(25));
        assert_eq!(find_head_end(b"GET /metrics HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn reading_stops_at_the_header_terminator_without_waiting_for_eof() {
        // A named pipe never delivers EOF after the request, so the reader must
        // return on the terminator alone. `&[u8]` yields Ok(0) at the end,
        // which would mask the bug, so the terminator is mid-buffer here.
        let raw = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\ntrailing".to_vec();
        let mut cursor = raw.as_slice();
        let head = read_request_head(&mut cursor).await.expect("head parses");
        assert!(head.starts_with(b"GET /metrics"));
        assert!(find_head_end(&head).is_some());
    }

    #[tokio::test]
    async fn a_truncated_request_is_an_error_not_an_empty_scrape() {
        let raw = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n".to_vec();
        let mut cursor = raw.as_slice();
        let error = read_request_head(&mut cursor)
            .await
            .expect_err("truncated request");
        assert!(error.contains("header terminator"), "{error}");
    }

    #[test]
    fn a_stream_for_another_binding_is_rejected_by_name() {
        let request = proto::OpenStreamRequest {
            stream_id: "s1".into(),
            metadata_json: Some(r#"{"binding_id":"something-else"}"#.into()),
            ..proto::OpenStreamRequest::default()
        };
        assert_eq!(binding_id_of(&request).as_deref(), Some("something-else"));
        let rejection = reject(&request, "no".into());
        assert!(!rejection.accepted);
        assert_eq!(rejection.stream_id, "s1");
    }

    #[test]
    fn missing_or_unreadable_metadata_does_not_block_the_scrape() {
        let bare = proto::OpenStreamRequest::default();
        assert_eq!(binding_id_of(&bare), None);
        let broken = proto::OpenStreamRequest {
            metadata_json: Some("not json".into()),
            ..proto::OpenStreamRequest::default()
        };
        assert_eq!(binding_id_of(&broken), None);
    }
}
