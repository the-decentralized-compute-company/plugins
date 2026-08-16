//! The Docker Engine API, restricted to the eight read calls this plugin makes.
//!
//! Each method builds its path with [`crate::paths`], sends it through
//! [`crate::transport`] as a `GET`, and decodes a bounded body. Three things
//! are deliberate:
//!
//! * **A non-2xx answer is an error, never an empty result.** The daemon's own
//!   `{"message": …}` is unwrapped and returned, because "no such container" and
//!   "the socket is not there" are different problems and a caller that gets an
//!   empty list cannot tell them apart.
//! * **A truncated body is an error for JSON endpoints.** Half a container list
//!   parses as nothing useful, and silently returning fewer containers than
//!   exist is exactly the failure mode this plugin must not have. Logs are the
//!   one exception, because a partial log read is still a log read — and it says
//!   it was cut.
//! * **No method takes a path or a query fragment.** Ids arrive as strings that
//!   have already been matched against the daemon's own container list, and
//!   `paths` re-checks them.

use std::fmt;

use serde::de::DeserializeOwned;

#[cfg(test)]
use crate::endpoint::Endpoint;
use crate::model::{ContainerInspect, ContainerSummary, DaemonInfo, DaemonVersion, ImageSummary};
use crate::paths::{self, ApiPath};
use crate::settings::Settings;
use crate::stats::ContainerStats;
use crate::transport::{Transport, TransportError};

/// Why a Docker API call did not produce an answer.
#[derive(Clone, Debug)]
pub enum ApiError {
    /// The endpoint could not be reached, or did not answer in time.
    Transport(TransportError),
    /// The daemon answered with a non-2xx status.
    Status {
        status: u16,
        message: String,
        path: String,
    },
    /// The daemon answered with something this plugin could not read.
    Decode(String),
}

impl ApiError {
    /// Whether this is the caller's problem (a container that does not exist)
    /// rather than the node's (an unreachable daemon), so the tool layer can
    /// pick the right JSON-RPC error code.
    pub fn is_caller_error(&self) -> bool {
        matches!(self, Self::Status { status, .. } if (400..500).contains(status))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "{error}"),
            Self::Status {
                status,
                message,
                path,
            } => write!(
                formatter,
                "the Docker daemon answered HTTP {status} for {path}: {message}"
            ),
            Self::Decode(message) => formatter.write_str(message),
        }
    }
}

impl From<TransportError> for ApiError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

/// A read-only client for one Docker endpoint.
#[derive(Clone, Debug)]
pub struct Docker {
    transport: Transport,
    api_version: String,
    /// Log reads are capped separately and much lower than other responses.
    max_log_bytes: usize,
}

impl Docker {
    pub fn new(settings: &Settings) -> Self {
        Self {
            transport: Transport::new(
                settings.endpoint.clone(),
                settings.timeout,
                settings.max_response_bytes,
                settings.user_agent(),
            ),
            api_version: settings.api_version.clone(),
            max_log_bytes: settings.logs.max_bytes,
        }
    }

    /// Build a client for one endpoint directly. Used by the tests here.
    #[cfg(test)]
    pub fn for_endpoint(endpoint: Endpoint, api_version: &str) -> Self {
        Self {
            transport: Transport::new(
                endpoint,
                std::time::Duration::from_secs(5),
                1024 * 1024,
                "tdcc-docker-inspect/test",
            ),
            api_version: api_version.to_string(),
            max_log_bytes: 64 * 1024,
        }
    }

    /// `GET /_ping`. Answers `OK` in the body; anything 2xx counts.
    pub async fn ping(&self) -> Result<(), ApiError> {
        self.read_ok(&paths::ping(&self.api_version)).await?;
        Ok(())
    }

    pub async fn version(&self) -> Result<DaemonVersion, ApiError> {
        self.read_json(&paths::version(&self.api_version)).await
    }

    pub async fn info(&self) -> Result<DaemonInfo, ApiError> {
        self.read_json(&paths::info(&self.api_version)).await
    }

    pub async fn containers(&self, all: bool) -> Result<Vec<ContainerSummary>, ApiError> {
        self.read_json(&paths::containers(&self.api_version, all))
            .await
    }

    pub async fn images(&self) -> Result<Vec<ImageSummary>, ApiError> {
        self.read_json(&paths::images(&self.api_version)).await
    }

    pub async fn inspect(&self, id: &str) -> Result<ContainerInspect, ApiError> {
        self.read_json(&self.container_path(paths::container_inspect(&self.api_version, id))?)
            .await
    }

    pub async fn stats(&self, id: &str) -> Result<ContainerStats, ApiError> {
        self.read_json(&self.container_path(paths::container_stats(&self.api_version, id))?)
            .await
    }

    /// Read a bounded slice of a container's logs.
    ///
    /// Returns the raw body and whether the byte cap cut it short. Unlike the
    /// JSON endpoints a truncated read is still returned, because a partial log
    /// is useful as long as the caller is told it is partial.
    pub async fn logs(
        &self,
        id: &str,
        tail: usize,
        timestamps: bool,
        since_unix: Option<u64>,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        let path = self.container_path(paths::container_logs(
            &self.api_version,
            id,
            tail,
            timestamps,
            since_unix,
        ))?;

        // A log body is capped far below `--max-response-bytes`: it is the one
        // response whose size a caller can influence, so it gets its own,
        // tighter budget. Everything else about the request is unchanged.
        let bounded = Transport::new(
            self.transport.endpoint().clone(),
            self.transport.timeout(),
            self.max_log_bytes,
            self.transport.user_agent(),
        );
        let response = bounded.get(&path).await?;
        let body = self.check_status(response.status, &response.body, &path)?;
        Ok((body, response.truncated))
    }

    /// `paths` returns `None` for an id that is not hexadecimal. That should be
    /// unreachable — ids come from the daemon's own listing — so it is reported
    /// as an internal inconsistency rather than silently skipped.
    fn container_path(&self, path: Option<ApiPath>) -> Result<ApiPath, ApiError> {
        path.ok_or_else(|| {
            ApiError::Decode(
                "the Docker daemon reported a container id that is not hexadecimal, so no request \
                 was made for it."
                    .to_string(),
            )
        })
    }

    async fn read_ok(&self, path: &ApiPath) -> Result<Vec<u8>, ApiError> {
        let response = self.transport.get(path).await?;
        let body = self.check_status(response.status, &response.body, path)?;
        if response.truncated {
            return Err(self.truncated(path));
        }
        Ok(body)
    }

    async fn read_json<T: DeserializeOwned>(&self, path: &ApiPath) -> Result<T, ApiError> {
        let body = self.read_ok(path).await?;
        serde_json::from_slice(&body).map_err(|error| {
            ApiError::Decode(format!(
                "the Docker daemon's answer to {path} could not be read as the expected JSON \
                 ({error}). This usually means the endpoint is not a Docker daemon, or speaks a \
                 different API version — try `--api-version`."
            ))
        })
    }

    fn truncated(&self, path: &ApiPath) -> ApiError {
        ApiError::Decode(format!(
            "the Docker daemon's answer to {path} was larger than the {} byte cap, so it was not \
             read completely. Raise `--max-response-bytes` if this machine genuinely runs that \
             many containers or images.",
            self.transport.max_response_bytes()
        ))
    }

    fn check_status(&self, status: u16, body: &[u8], path: &ApiPath) -> Result<Vec<u8>, ApiError> {
        if (200..300).contains(&status) {
            return Ok(body.to_vec());
        }
        Err(ApiError::Status {
            status,
            message: daemon_message(body, status, &self.api_version),
            path: path.to_string(),
        })
    }
}

/// Unwrap the daemon's own error text.
///
/// Docker answers errors with `{"message": "No such container: x"}`. That
/// sentence is more useful than anything this plugin could invent, so it is
/// passed through — with one addition, for the single error whose remedy is a
/// setting of this plugin rather than something on the machine.
pub fn daemon_message(body: &[u8], status: u16, api_version: &str) -> String {
    #[derive(serde::Deserialize)]
    struct DaemonError {
        message: String,
    }

    let text = serde_json::from_slice::<DaemonError>(body)
        .map(|error| error.message)
        .unwrap_or_else(|_| {
            String::from_utf8_lossy(body)
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect()
        });

    let text = if text.trim().is_empty() {
        format!("no detail was given (HTTP {status})")
    } else {
        text
    };

    if text.contains("too new") || text.contains("client version") {
        return format!(
            "{text} docker-inspect asks for API {api_version}; set `--api-version` to the highest \
             version this daemon supports."
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{StubDaemon, ok};
    use tokio::net::TcpListener;

    const API_VERSION: &str = "v1.41";

    fn client(daemon: &StubDaemon) -> Docker {
        Docker::for_endpoint(daemon.endpoint(), API_VERSION)
    }

    #[tokio::test]
    async fn the_container_list_decodes_and_is_requested_with_get() {
        let daemon = StubDaemon::spawn(vec![ok(
            r#"[{"Id":"aa11","Names":["/web"],"State":"running","Labels":null}]"#,
        )]);

        let containers = client(&daemon)
            .containers(true)
            .await
            .expect("the stub answers");

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].primary_name(), "web");
        assert_eq!(
            daemon.requests(),
            vec!["GET /v1.41/containers/json?all=1 HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn a_404_becomes_a_caller_error_carrying_the_daemons_own_sentence() {
        let daemon = StubDaemon::spawn(vec![(
            404,
            r#"{"message":"No such container: deadbeef"}"#.to_string(),
        )]);

        let error = client(&daemon)
            .inspect(&"a".repeat(64))
            .await
            .expect_err("the stub refuses");

        assert!(error.is_caller_error());
        assert!(error.to_string().contains("No such container"), "{error}");
        assert!(error.to_string().contains("404"), "{error}");
    }

    #[tokio::test]
    async fn a_500_is_not_a_caller_error() {
        let daemon = StubDaemon::spawn(vec![(500, r#"{"message":"driver failed"}"#.to_string())]);

        let error = client(&daemon).info().await.expect_err("the stub fails");

        assert!(!error.is_caller_error());
        assert!(error.to_string().contains("driver failed"), "{error}");
    }

    #[tokio::test]
    async fn an_unreadable_body_names_the_api_version_setting() {
        let daemon = StubDaemon::spawn(vec![ok("<html>not docker</html>")]);

        let error = client(&daemon)
            .version()
            .await
            .expect_err("html is not the expected JSON");

        assert!(error.to_string().contains("--api-version"), "{error}");
        assert!(!error.is_caller_error());
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_a_transport_error_not_an_empty_list() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        drop(listener);

        let error = Docker::for_endpoint(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
            API_VERSION,
        )
        .containers(false)
        .await
        .expect_err("nothing is listening");

        assert!(matches!(error, ApiError::Transport(_)));
        assert!(error.to_string().contains("could not reach"), "{error}");
    }

    #[tokio::test]
    async fn a_log_read_returns_bytes_and_asks_for_the_right_bounds() {
        let daemon = StubDaemon::spawn(vec![ok("frameless log body")]);

        let (body, truncated) = client(&daemon)
            .logs(&"a".repeat(64), 25, true, Some(1_700_000_000))
            .await
            .expect("the stub answers");

        assert_eq!(String::from_utf8_lossy(&body), "frameless log body");
        assert!(!truncated);
        let request = &daemon.requests()[0];
        assert!(request.contains("tail=25"), "{request}");
        assert!(request.contains("timestamps=1"), "{request}");
        assert!(request.contains("since=1700000000"), "{request}");
        assert!(request.contains("follow=0"), "{request}");
    }

    #[tokio::test]
    async fn stats_and_version_decode_the_fields_the_tools_report() {
        let daemon = StubDaemon::spawn(vec![
            ok(r#"{"Version":"25.0.3","ApiVersion":"1.44","Os":"linux","Arch":"amd64"}"#),
            ok(r#"{"read":"2024-05-01T10:00:00Z","cpu_stats":{"cpu_usage":{"total_usage":10}}}"#),
        ]);
        let client = client(&daemon);

        let version = client.version().await.expect("the stub answers");
        let stats = client
            .stats(&"a".repeat(64))
            .await
            .expect("the stub answers");

        assert_eq!(version.version, "25.0.3");
        assert_eq!(version.api_version, "1.44");
        assert_eq!(stats.read, "2024-05-01T10:00:00Z");
    }

    #[test]
    fn a_daemon_message_is_preferred_over_the_raw_body() {
        assert_eq!(
            daemon_message(br#"{"message":"No such container: x"}"#, 404, "v1.41"),
            "No such container: x"
        );
        assert_eq!(
            daemon_message(b"page not found\nsecond line", 404, "v1.41"),
            "page not found"
        );
        assert!(daemon_message(b"", 500, "v1.41").contains("HTTP 500"));
    }

    #[test]
    fn a_version_mismatch_points_at_the_setting_that_fixes_it() {
        let message = daemon_message(
            br#"{"message":"client version 1.41 is too new. Maximum supported API version is 1.24"}"#,
            400,
            "v1.41",
        );

        assert!(message.contains("--api-version"), "{message}");
        assert!(message.contains("Maximum supported"), "{message}");
    }
}
