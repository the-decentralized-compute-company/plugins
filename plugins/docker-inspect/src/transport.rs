//! One connection, one `GET`, one bounded response.
//!
//! ## Why the HTTP is hand-written
//!
//! Access to the Docker socket is equivalent to root on the host: anything that
//! can create a container can bind-mount `/` into it. A Docker client library
//! would put `create`, `exec`, `start`, `stop`, and `remove` in the same binary
//! as the four read calls this plugin needs, and the only thing keeping them
//! from being called would be a code review.
//!
//! So the request line is built here, once, with the method as a literal:
//!
//! ```text
//! GET {path} HTTP/1.1
//! ```
//!
//! There is no parameter for the method and no other function that writes to
//! the socket. `POST`, `PUT`, `PATCH`, and `DELETE` are not reachable from this
//! binary because no code in it can emit them — see the test at the bottom of
//! this file, and [`crate::paths`] for the matching restriction on which paths
//! can be requested at all.
//!
//! ## Reading the response
//!
//! Docker answers with `Content-Length` on some endpoints and chunked transfer
//! encoding on others, and closes the connection at the end when asked to. All
//! three framings are handled, and every read is bounded: a body over the cap
//! stops the read and is reported as truncated rather than buffered without
//! limit.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::endpoint::Endpoint;
use crate::paths::ApiPath;

/// What a caller gets back from the daemon.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// The response hit the byte cap and is not complete.
    pub truncated: bool,
}

/// Why a request did not produce a response.
///
/// Split by cause rather than collapsed into one string because the remedies
/// are different, and a tool result that names the wrong setting wastes more of
/// an operator's time than one that names none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The endpoint could not be opened. Carries a sentence naming the likely
    /// cause and the setting that changes it.
    Unreachable(String),
    /// The daemon accepted the connection but did not answer in time.
    Timeout(String),
    /// Something answered, but not with an HTTP response this can read.
    Protocol(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(message) | Self::Timeout(message) | Self::Protocol(message) => {
                formatter.write_str(message)
            }
        }
    }
}

/// A configured connection to one Docker endpoint.
#[derive(Clone, Debug)]
pub struct Transport {
    endpoint: Endpoint,
    timeout: Duration,
    max_response_bytes: usize,
    user_agent: String,
}

impl Transport {
    pub fn new(
        endpoint: Endpoint,
        timeout: Duration,
        max_response_bytes: usize,
        user_agent: impl Into<String>,
    ) -> Self {
        Self {
            endpoint,
            timeout,
            max_response_bytes,
            user_agent: user_agent.into(),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Perform one `GET`.
    ///
    /// The timeout covers connecting and reading together, so a daemon that
    /// accepts a connection and then stops talking cannot hold a handler open.
    pub async fn get(&self, path: &ApiPath) -> Result<Response, TransportError> {
        let request = self.request_line(path);
        tokio::time::timeout(self.timeout, self.exchange(request))
            .await
            .map_err(|_| {
                TransportError::Timeout(format!(
                    "the Docker daemon at {} did not answer {path} within {} seconds. Raise \
                     `--timeout-secs` if this endpoint is genuinely slow.",
                    self.endpoint,
                    self.timeout.as_secs()
                ))
            })?
    }

    /// The complete request, with the method as a literal.
    fn request_line(&self, path: &ApiPath) -> String {
        format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Accept: application/json\r\n\
             User-Agent: {}\r\n\
             Connection: close\r\n\r\n",
            path.as_str(),
            self.endpoint.host_header(),
            self.user_agent
        )
    }

    async fn exchange(&self, request: String) -> Result<Response, TransportError> {
        match &self.endpoint {
            Endpoint::Tcp { host, port } => {
                let stream = tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(|error| {
                        TransportError::Unreachable(self.describe_connect_failure(&error))
                    })?;
                converse(stream, request, self.max_response_bytes).await
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|error| {
                        TransportError::Unreachable(self.describe_connect_failure(&error))
                    })?;
                converse(stream, request, self.max_response_bytes).await
            }
            #[cfg(windows)]
            Endpoint::NamedPipe(path) => {
                let stream = open_named_pipe(path).await.map_err(|error| {
                    TransportError::Unreachable(self.describe_connect_failure(&error))
                })?;
                converse(stream, request, self.max_response_bytes).await
            }
            // The startup check in `Endpoint::platform_support` refuses these
            // before any tool runs; this arm keeps the match total.
            #[cfg(not(unix))]
            Endpoint::Unix(_) => Err(TransportError::Unreachable(
                "this build cannot open a Unix socket endpoint.".to_string(),
            )),
            #[cfg(not(windows))]
            Endpoint::NamedPipe(_) => Err(TransportError::Unreachable(
                "this build cannot open a Windows named pipe endpoint.".to_string(),
            )),
        }
    }

    /// Turn a connect failure into a sentence naming the cause and the fix.
    ///
    /// A raw `No such file or directory (os error 2)` in a tool result tells an
    /// operator nothing about which of four settings to change.
    fn describe_connect_failure(&self, error: &std::io::Error) -> String {
        describe_connect_failure(&self.endpoint, error.kind(), &error.to_string())
    }
}

/// The operator-facing text for a failed connect. Separated from the IO so it
/// can be tested against every error kind that matters.
pub fn describe_connect_failure(
    endpoint: &Endpoint,
    kind: std::io::ErrorKind,
    detail: &str,
) -> String {
    let where_it_looked = format!("docker-inspect could not reach the Docker daemon at {endpoint}");
    let how_to_point_it_elsewhere = "Point it somewhere else with `--endpoint <value>` in [[plugin]].args, \
         TDCC_DOCKER_INSPECT_ENDPOINT, [[plugin]].url, or DOCKER_HOST.";

    match kind {
        std::io::ErrorKind::NotFound => format!(
            "{where_it_looked}: nothing exists at that path. Either the daemon is not running, or \
             it listens somewhere else — rootless Docker uses \
             `unix:///run/user/<uid>/docker.sock`, and Colima and Rancher Desktop each use their \
             own socket. {how_to_point_it_elsewhere}"
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "{where_it_looked}: permission denied. The user running tdcc has no access to that \
             socket. On Linux that normally means adding the user to the `docker` group and \
             restarting tdcc — understand first that membership of that group is equivalent to \
             root on this machine, because anyone who can create a container can mount the host \
             filesystem into it. {how_to_point_it_elsewhere}"
        ),
        std::io::ErrorKind::ConnectionRefused => format!(
            "{where_it_looked}: the connection was refused, so nothing is listening there. \
             {how_to_point_it_elsewhere}"
        ),
        std::io::ErrorKind::TimedOut => {
            format!("{where_it_looked}: the connection timed out. {how_to_point_it_elsewhere}")
        }
        _ => format!("{where_it_looked}: {detail}. {how_to_point_it_elsewhere}"),
    }
}

/// Open a Windows named pipe, waiting briefly if every instance is busy.
///
/// `ERROR_PIPE_BUSY` means the server has no free instance right now, which is
/// ordinary under concurrent requests and not a failure.
#[cfg(windows)]
async fn open_named_pipe(
    path: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    /// `ERROR_PIPE_BUSY`, from the Windows SDK. Named here rather than pulling
    /// in a bindings crate for one constant.
    const ERROR_PIPE_BUSY: i32 = 231;

    for _ in 0..10 {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
    tokio::net::windows::named_pipe::ClientOptions::new().open(path)
}

/// Write the request and read the response, whatever the stream is underneath.
async fn converse<S>(
    mut stream: S,
    request: String,
    max_response_bytes: usize,
) -> Result<Response, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| TransportError::Unreachable(format!("failed sending request: {error}")))?;
    stream
        .flush()
        .await
        .map_err(|error| TransportError::Unreachable(format!("failed sending request: {error}")))?;

    let mut buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    let mut head: Option<Head> = None;
    let mut truncated = false;

    loop {
        let read = stream.read(&mut chunk).await.map_err(|error| {
            TransportError::Protocol(format!("failed reading response: {error}"))
        })?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if head.is_none() {
            head = parse_head(&buffer)?;
        }
        // Cap the *body*, so a large header block cannot be used to smuggle
        // past the limit and so the limit means what the setting says.
        if let Some(head) = &head
            && buffer.len().saturating_sub(head.header_len) > max_response_bytes
        {
            truncated = true;
            break;
        }
        if buffer.len() > max_response_bytes.saturating_add(64 * 1024) {
            truncated = true;
            break;
        }

        if let Some(head) = &head {
            match head.framing {
                Framing::ContentLength(length) => {
                    if buffer.len() >= head.header_len + length {
                        break;
                    }
                }
                Framing::Chunked => {
                    let (_, complete) = decode_chunked(&buffer[head.header_len..]);
                    if complete {
                        break;
                    }
                }
                // Nothing to detect: the daemon signals the end by closing.
                Framing::UntilClose => {}
            }
        }
    }

    let head = head.ok_or_else(|| {
        TransportError::Protocol(
            "the Docker endpoint closed the connection before sending a complete HTTP response \
             header. If this endpoint is a TLS-protected Docker daemon, docker-inspect cannot \
             speak to it: it links no TLS stack."
                .to_string(),
        )
    })?;

    let raw_body = &buffer[head.header_len.min(buffer.len())..];
    let body = match head.framing {
        Framing::ContentLength(length) => raw_body[..length.min(raw_body.len())].to_vec(),
        Framing::Chunked => {
            let (decoded, complete) = decode_chunked(raw_body);
            truncated |= !complete;
            decoded
        }
        Framing::UntilClose => raw_body.to_vec(),
    };

    Ok(Response {
        status: head.status,
        body,
        truncated,
    })
}

/// How the body of a response is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    ContentLength(usize),
    Chunked,
    UntilClose,
}

/// What this plugin needs out of a response header block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    /// Byte offset of the first body byte.
    pub header_len: usize,
    pub framing: Framing,
}

/// Parse a response header block, returning `None` while it is still partial.
pub fn parse_head(buffer: &[u8]) -> Result<Option<Head>, TransportError> {
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = response.parse(buffer).map_err(|error| {
        TransportError::Protocol(format!(
            "the Docker endpoint sent a malformed HTTP response: {error}"
        ))
    })?;

    let header_len = match parsed {
        httparse::Status::Complete(length) => length,
        httparse::Status::Partial => return Ok(None),
    };
    let status = response.code.ok_or_else(|| {
        TransportError::Protocol("the Docker endpoint sent no HTTP status code".to_string())
    })?;

    let mut framing = Framing::UntilClose;
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(header.value)
                .to_ascii_lowercase()
                .contains("chunked")
        {
            framing = Framing::Chunked;
            break;
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let raw = String::from_utf8_lossy(header.value);
            let length = raw.trim().parse::<usize>().map_err(|_| {
                TransportError::Protocol(format!(
                    "the Docker endpoint sent an invalid Content-Length `{raw}`"
                ))
            })?;
            framing = Framing::ContentLength(length);
        }
    }

    Ok(Some(Head {
        status,
        header_len,
        framing,
    }))
}

/// Decode chunked transfer encoding.
///
/// Returns everything decoded so far and whether the terminal zero-length chunk
/// was seen, so a stream cut short by the byte cap still yields the chunks that
/// did arrive instead of nothing at all.
pub fn decode_chunked(body: &[u8]) -> (Vec<u8>, bool) {
    let mut decoded = Vec::with_capacity(body.len());
    let mut offset = 0usize;

    loop {
        let Some(line_end) = find_crlf(&body[offset..]) else {
            return (decoded, false);
        };
        let header = &body[offset..offset + line_end];
        // A chunk extension (`1a;name=value`) is legal and ignorable.
        let size_text = header
            .split(|byte| *byte == b';')
            .next()
            .unwrap_or_default();
        let Ok(size_text) = std::str::from_utf8(size_text) else {
            return (decoded, false);
        };
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
            return (decoded, false);
        };

        let data_start = offset + line_end + 2;
        if size == 0 {
            return (decoded, true);
        }
        if data_start + size > body.len() {
            // Partial final chunk: keep what is there rather than dropping it.
            if data_start < body.len() {
                decoded.extend_from_slice(&body[data_start..]);
            }
            return (decoded, false);
        }
        decoded.extend_from_slice(&body[data_start..data_start + size]);
        offset = data_start + size + 2;
        if offset > body.len() {
            return (decoded, false);
        }
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths;
    use std::io::ErrorKind;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::TcpListener;

    const API_VERSION: &str = "v1.41";

    fn transport(port: u16) -> Transport {
        Transport::new(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
            Duration::from_secs(5),
            1024 * 1024,
            "tdcc-docker-inspect/test",
        )
    }

    /// Serve one canned response and hand back the request line that asked for
    /// it, so a test can assert on both halves of the exchange.
    async fn serve_once(response: Vec<u8>) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let port = listener.local_addr().expect("has an address").port();

        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accepts");
            let (reader, mut writer) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let read = reader.read_line(&mut line).await.expect("reads");
                if read == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            writer.write_all(&response).await.expect("writes");
            writer.flush().await.expect("flushes");
            drop(writer);
            request
        });

        (port, handle)
    }

    #[tokio::test]
    async fn the_only_request_this_plugin_can_send_is_a_get() {
        let (port, server) =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec()).await;

        let response = transport(port)
            .get(&paths::containers(API_VERSION, true))
            .await
            .expect("the stub answers");
        let request = server.await.expect("the stub finishes");

        assert!(
            request.starts_with("GET /v1.41/containers/json?all=1 HTTP/1.1"),
            "{request}"
        );
        assert!(request.contains("Connection: close"), "{request}");
        assert!(
            request.contains("User-Agent: tdcc-docker-inspect/test"),
            "{request}"
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}");
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn a_chunked_response_is_reassembled() {
        let body = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (port, server) = serve_once(body.as_bytes().to_vec()).await;

        let response = transport(port)
            .get(&paths::info(API_VERSION))
            .await
            .expect("the stub answers");
        server.await.expect("the stub finishes");

        assert_eq!(String::from_utf8_lossy(&response.body), "hello world");
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn a_close_delimited_response_is_read_to_the_end() {
        let (port, server) =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nOK".to_vec()).await;

        let response = transport(port)
            .get(&paths::ping(API_VERSION))
            .await
            .expect("the stub answers");
        server.await.expect("the stub finishes");

        assert_eq!(response.body, b"OK");
    }

    #[tokio::test]
    async fn an_error_status_is_returned_rather_than_thrown_away() {
        let body = "HTTP/1.1 404 Not Found\r\nContent-Length: 34\r\n\r\n\
                    {\"message\":\"No such container\"}\r\n\r\n";
        let (port, server) = serve_once(body.as_bytes().to_vec()).await;

        let response = transport(port)
            .get(&paths::info(API_VERSION))
            .await
            .expect("the stub answers");
        server.await.expect("the stub finishes");

        assert_eq!(response.status, 404);
        assert!(String::from_utf8_lossy(&response.body).contains("No such container"));
    }

    #[tokio::test]
    async fn a_body_past_the_cap_stops_the_read_and_says_it_was_truncated() {
        let payload = "x".repeat(200_000);
        let response_bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .into_bytes();
        let (port, server) = serve_once(response_bytes).await;

        let capped = Transport::new(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
            Duration::from_secs(5),
            50_000,
            "tdcc-docker-inspect/test",
        );
        let response = capped
            .get(&paths::images(API_VERSION))
            .await
            .expect("the stub answers");
        server.abort();

        assert!(response.truncated);
        assert!(response.body.len() < payload.len());
    }

    #[tokio::test]
    async fn a_refused_connection_names_the_endpoint_and_the_settings() {
        // Bind and drop, so the port is almost certainly closed.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        drop(listener);

        let error = transport(port)
            .get(&paths::ping(API_VERSION))
            .await
            .expect_err("nothing is listening");

        let TransportError::Unreachable(message) = error else {
            panic!("a closed port is an unreachable endpoint");
        };
        assert!(
            message.contains(&format!("tcp://127.0.0.1:{port}")),
            "{message}"
        );
        assert!(message.contains("DOCKER_HOST"), "{message}");
    }

    #[tokio::test]
    async fn a_silent_endpoint_times_out_and_names_the_timeout_setting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accepts");
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });

        let slow = Transport::new(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
            Duration::from_millis(150),
            1024,
            "tdcc-docker-inspect/test",
        );
        let error = slow
            .get(&paths::ping(API_VERSION))
            .await
            .expect_err("the stub never answers");
        server.abort();

        let TransportError::Timeout(message) = error else {
            panic!("a silent endpoint is a timeout");
        };
        assert!(message.contains("--timeout-secs"), "{message}");
    }

    #[test]
    fn a_missing_socket_explains_that_docker_may_not_be_running() {
        let message = describe_connect_failure(
            &Endpoint::Unix("/var/run/docker.sock".into()),
            ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        );

        assert!(message.contains("unix:///var/run/docker.sock"), "{message}");
        assert!(message.contains("not running"), "{message}");
        assert!(message.contains("rootless"), "{message}");
        assert!(message.contains("--endpoint"), "{message}");
    }

    #[test]
    fn a_permission_error_names_the_group_and_what_granting_it_means() {
        let message = describe_connect_failure(
            &Endpoint::Unix("/var/run/docker.sock".into()),
            ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        );

        assert!(message.contains("docker` group"), "{message}");
        assert!(message.contains("equivalent to root"), "{message}");
    }

    #[test]
    fn header_parsing_reports_each_framing() {
        let with_length = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .expect("parses")
            .expect("complete");
        assert_eq!(with_length.framing, Framing::ContentLength(2));
        assert_eq!(with_length.status, 200);

        let chunked = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .expect("parses")
            .expect("complete");
        assert_eq!(chunked.framing, Framing::Chunked);

        let until_close = parse_head(b"HTTP/1.1 500 Internal Server Error\r\n\r\n")
            .expect("parses")
            .expect("complete");
        assert_eq!(until_close.framing, Framing::UntilClose);
        assert_eq!(until_close.status, 500);
    }

    #[test]
    fn a_partial_header_block_is_not_an_error() {
        assert_eq!(parse_head(b"HTTP/1.1 200 OK\r\nContent-Ty"), Ok(None));
    }

    #[test]
    fn an_invalid_content_length_is_reported_rather_than_ignored() {
        assert!(matches!(
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: two\r\n\r\n"),
            Err(TransportError::Protocol(_))
        ));
    }

    #[test]
    fn chunked_decoding_handles_extensions_and_reports_completeness() {
        let (decoded, complete) = decode_chunked(b"4;name=value\r\nabcd\r\n0\r\n\r\n");
        assert_eq!(decoded, b"abcd");
        assert!(complete);

        let (partial, complete) = decode_chunked(b"4\r\nabcd\r\n8\r\nefgh");
        assert_eq!(partial, b"abcdefgh");
        assert!(!complete, "a cut-off stream is not complete");

        let (empty, complete) = decode_chunked(b"0\r\n\r\n");
        assert!(empty.is_empty());
        assert!(complete);
    }

    #[test]
    fn chunked_decoding_of_nonsense_stops_rather_than_looping() {
        let (decoded, complete) = decode_chunked(b"not-a-chunk-size\r\nbody");
        assert!(decoded.is_empty());
        assert!(!complete);
    }
}
