//! Test-only HTTP servers.
//!
//! Compiled only under `cfg(test)`. They exist so the interesting failures —
//! an endpoint that answers with an error page, one that ignores
//! `stream: true`, a probe that is simply not there — are exercised over a real
//! socket rather than mocked away.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

pub const SSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

/// A short chat-completion stream: two content deltas, then a usage block.
pub const SSE_CHUNKS: &[&str] = &[
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"one \"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":260,\"completion_tokens\":9}}\n\n",
    "data: [DONE]\n\n",
];

pub fn url(address: SocketAddr, path: &str) -> Url {
    Url::parse(&format!("http://{address}{path}")).unwrap()
}

/// An address with nothing listening on it.
pub async fn dead_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// Serve exactly one request, then stop.
pub async fn serve_once(
    head: &'static str,
    body_chunks: Vec<&'static str>,
    gap: Duration,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            respond(&mut socket, head, &body_chunks, gap).await;
        }
    });
    address
}

/// Serve the same response to every request, for as long as the test runs.
pub async fn serve_forever(
    head: &'static str,
    body_chunks: &'static [&'static str],
    gap: Duration,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                respond(&mut socket, head, body_chunks, gap).await;
            });
        }
    });
    address
}

async fn respond(socket: &mut TcpStream, head: &str, chunks: &[&str], gap: Duration) {
    read_request(socket).await;
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for chunk in chunks {
        tokio::time::sleep(gap).await;
        if socket.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }
    let _ = socket.flush().await;
    let _ = socket.shutdown().await;
}

/// Consume the headers and the declared body, so the client never sees a reset
/// where it expected a response.
async fn read_request(socket: &mut TcpStream) {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while socket.read_exact(&mut byte).await.is_ok() {
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
    let content_length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = socket.read_exact(&mut body).await;
    }
}
