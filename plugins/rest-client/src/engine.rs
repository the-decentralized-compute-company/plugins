//! The four tool implementations, and the only place in the plugin that
//! touches the network.
//!
//! Failure is reported, never swallowed. A missing credential, an endpoint the
//! operator did not declare, a parameter outside its declared range, a budget
//! that has run out, a base URL that resolves into private space, a `404`, a
//! `500` — each comes back as an error naming the cause and, where there is
//! one, the setting that would fix it. In particular a non-2xx response is an
//! **error with the status code intact**, both in the message and in the
//! structured `data` alongside it, so a caller can tell a `404` from a `500`
//! and from "the request never left this machine".
//!
//! Every message this module returns passes through the [`Redactor`] on its way
//! out.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Method};
use serde_json::{Value, json};
use tdcc_plugin::{PluginError, PluginResult};

use crate::auth::{AuthState, EnvMap, Redactor, ResolvedAuth, resolve};
use crate::catalog::Catalog;
use crate::ratelimit::RateLimiter;
use crate::request::{self, PreparedRequest};
use crate::{net, schema};

/// Response headers worth handing back to a caller. An allowlist, so a
/// `Set-Cookie` or an unexpected vendor header carrying session state never
/// ends up in a model's context.
const REPORTED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "date",
    "etag",
    "last-modified",
    "location",
    "link",
    "retry-after",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-request-id",
];

/// Longest header value reported back. A header is metadata; anything longer
/// than this is a payload wearing a header's clothes.
const MAX_HEADER_VALUE_CHARS: usize = 512;

/// How much of an error response body is quoted back. Enough for an API's
/// `{"message": "..."}`, not enough to be a way around the response cap.
const MAX_ERROR_EXCERPT_CHARS: usize = 1_000;

/// Where the declaration came from, so `status` can say why a node has no
/// endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogSource {
    /// The file existed and parsed.
    Loaded(PathBuf),
    /// No file at that path. The plugin starts inert rather than refusing to
    /// start: a node with the plugin installed and no declaration yet is a
    /// normal state, and `status` says so plainly.
    Missing(PathBuf),
}

impl CatalogSource {
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Loaded(path) | Self::Missing(path) => path,
        }
    }
}

pub struct Engine {
    catalog: Catalog,
    auth: BTreeMap<String, AuthState>,
    redactor: Redactor,
    client: Client,
    limiter: RateLimiter,
    source: CatalogSource,
    /// Built once at startup from the declaration; see `schema.rs`.
    call_description: String,
}

impl Engine {
    pub fn new(
        catalog: Catalog,
        source: CatalogSource,
        env: &EnvMap,
        user_agent: &str,
    ) -> Result<Arc<Self>, String> {
        let (auth, redactor) = resolve(&catalog, env);
        let client = net::build_client(user_agent)
            .map_err(|error| format!("could not build the HTTP client: {error}"))?;
        let limiter = RateLimiter::new(&catalog.names());
        let call_description = schema::render_catalog(&catalog);
        Ok(Arc::new(Self {
            catalog,
            auth,
            redactor,
            client,
            limiter,
            source,
            call_description,
        }))
    }

    /// The generated catalog listing that goes into the `call` tool's
    /// description. Read once, when the manifest is built.
    pub fn call_description(&self) -> &str {
        &self.call_description
    }

    /// A one-line status for the host's health check.
    ///
    /// Deliberately local: health must stay fast and independent of any call in
    /// flight, so this never touches the network.
    pub fn health(&self) -> String {
        let declared = self.catalog.endpoints.len();
        if declared == 0 {
            return format!(
                "ok; no endpoints declared in {}",
                self.source.path().display()
            );
        }
        let ready = self.auth.values().filter(|state| state.is_ready()).count();
        format!(
            "ok; {declared} endpoint{} declared, {ready} with credentials resolved",
            if declared == 1 { "" } else { "s" }
        )
    }

    // -- status ------------------------------------------------------------

    /// What this plugin is configured as, without touching the network.
    pub fn status(&self) -> Value {
        let now_ms = now_ms();
        let endpoints: Vec<Value> = self
            .catalog
            .endpoints
            .iter()
            .map(|endpoint| {
                let state = self.auth.get(&endpoint.name);
                let budget =
                    self.limiter
                        .peek(&endpoint.name, endpoint.limits.max_calls_per_minute, now_ms);
                json!({
                    "name": endpoint.name,
                    "base_url": endpoint.base_url,
                    "operations": endpoint.operations.len(),
                    "auth_kind": endpoint.auth.kind(),
                    "auth_env": endpoint.auth.env_name(),
                    "auth_ready": state.is_some_and(AuthState::is_ready),
                    "auth_problem": match state {
                        Some(AuthState::Missing(message)) => Value::String(message.clone()),
                        _ => Value::Null,
                    },
                    "allow_private_base": endpoint.allow_private_base,
                    "allow_insecure_auth": endpoint.allow_insecure_auth,
                    "calls_this_minute": budget.used,
                    "max_calls_per_minute": budget.limit,
                })
            })
            .collect();

        json!({
            "config_path": self.source.path().display().to_string(),
            "config_loaded": matches!(self.source, CatalogSource::Loaded(_)),
            "endpoint_count": self.catalog.endpoints.len(),
            "endpoints": endpoints,
            "note": match &self.source {
                CatalogSource::Loaded(_) if self.catalog.endpoints.is_empty() =>
                    "The declaration file loaded but declares no endpoints, so nothing can be \
                     called.",
                CatalogSource::Loaded(_) => "Endpoints are declared. `call` is usable for any \
                     endpoint whose auth_ready is true.",
                CatalogSource::Missing(_) =>
                    "No declaration file was found at config_path, so rest-client has nothing to \
                     call. Create that file, or point --config at one, and restart the node.",
            },
        })
    }

    // -- endpoints ---------------------------------------------------------

    /// The declared catalog, with each endpoint's operations and parameters.
    pub fn endpoints(&self) -> Value {
        let summaries: Vec<Value> = self
            .catalog
            .endpoints
            .iter()
            .map(|endpoint| {
                let ready = self
                    .auth
                    .get(&endpoint.name)
                    .is_some_and(AuthState::is_ready);
                schema::endpoint_summary(endpoint, ready)
            })
            .collect();

        json!({
            "count": summaries.len(),
            "endpoints": summaries,
            "config_path": self.source.path().display().to_string(),
            "note": if summaries.is_empty() {
                "No endpoints are declared. A model can only call APIs this node's operator wrote \
                 into the declaration file at config_path; there is no way to pass a URL."
            } else {
                "Call one of these with `rest-client.call`, naming the endpoint and the operation. \
                 There is no way to pass a URL."
            },
        })
    }

    // -- describe ----------------------------------------------------------

    /// One endpoint, or one operation, in full — including a JSON Schema for
    /// the operation's parameters.
    pub fn describe(
        &self,
        endpoint_name: &str,
        operation_name: Option<&str>,
    ) -> PluginResult<Value> {
        let endpoint = self.endpoint(endpoint_name)?;

        let Some(operation_name) = operation_name else {
            return Ok(json!({
                "endpoint": endpoint.name,
                "description": endpoint.description,
                "base_url": endpoint.base_url,
                "methods": endpoint.methods,
                "allowed_paths": endpoint.paths,
                "auth": {
                    "kind": endpoint.auth.kind(),
                    "env": endpoint.auth.env_name(),
                    "ready": self.auth.get(&endpoint.name).is_some_and(AuthState::is_ready),
                },
                "operations": endpoint
                    .operations
                    .iter()
                    .map(|operation| schema::operation_detail(endpoint, operation))
                    .collect::<Vec<_>>(),
            }));
        };

        let operation = endpoint.operation(operation_name).ok_or_else(|| {
            PluginError::invalid_params(format!(
                "endpoint `{}` has no operation `{operation_name}`. It declares: {}.",
                endpoint.name,
                endpoint
                    .operations
                    .iter()
                    .map(|operation| operation.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(schema::operation_detail(endpoint, operation))
    }

    // -- call --------------------------------------------------------------

    /// Invoke one declared operation.
    pub async fn call(
        &self,
        endpoint_name: &str,
        operation_name: &str,
        params: &BTreeMap<String, Value>,
        body: Option<&Value>,
    ) -> PluginResult<Value> {
        let endpoint = self.endpoint(endpoint_name)?;
        let operation = endpoint.operation(operation_name).ok_or_else(|| {
            PluginError::invalid_params(format!(
                "endpoint `{}` has no operation `{operation_name}`. It declares: {}. Call \
                 `rest-client.endpoints` for the catalog.",
                endpoint.name,
                endpoint
                    .operations
                    .iter()
                    .map(|operation| operation.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        let auth = match self.auth.get(&endpoint.name) {
            Some(AuthState::Ready(auth)) => auth,
            // Not an empty success and not a silent unauthenticated request:
            // the operator declared a credential, so a call without one is a
            // configuration failure with a name attached.
            Some(AuthState::Missing(message)) => {
                return Err(PluginError::invalid_request(message.clone()));
            }
            None => &ResolvedAuth::None,
        };

        let prepared = request::build(endpoint, operation, params, body, auth)
            .map_err(|error| self.invalid_params(error))?;

        // Charged once, here: after the caller's arguments have been found
        // valid and before anything leaves the machine, so a malformed call
        // does not spend somebody's API quota and a valid one always does.
        let budget = self
            .limiter
            .admit(
                &endpoint.name,
                endpoint.limits.max_calls_per_minute,
                now_ms(),
            )
            .map_err(|error| self.invalid_request(error))?;

        net::check_destination(&prepared.url, endpoint.allow_private_base, &endpoint.name)
            .await
            .map_err(|error| self.invalid_request(error))?;

        let started = Instant::now();
        let response = self
            .send(&prepared, Duration::from_secs(endpoint.limits.timeout_secs))
            .await?;
        let status = response.status();

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let media_type = net::media_type(&content_type);
        let headers = self.reported_headers(response.headers());

        let (bytes, truncated) = net::read_capped(response, endpoint.limits.max_response_bytes)
            .await
            .map_err(|error| self.internal(error))?;
        let duration_ms = started.elapsed().as_millis() as u64;

        if !status.is_success() {
            return Err(self.status_error(
                endpoint_name,
                operation_name,
                &prepared,
                status,
                &bytes,
                &media_type,
                duration_ms,
            ));
        }

        let mut result = json!({
            "endpoint": endpoint.name,
            "operation": operation.name,
            "method": prepared.method,
            "url": self.redactor.redact(prepared.display_url.clone()),
            "status": status.as_u16(),
            "content_type": media_type,
            "bytes": bytes.len(),
            "truncated": truncated,
            "duration_ms": duration_ms,
            "response_headers": headers,
            "budget": {
                "used_this_minute": budget.used,
                "max_calls_per_minute": budget.limit,
            },
        });
        self.attach_body(
            &mut result,
            &bytes,
            &media_type,
            truncated,
            endpoint.limits.max_response_bytes,
        );
        Ok(result)
    }

    async fn send(
        &self,
        prepared: &PreparedRequest,
        timeout: Duration,
    ) -> PluginResult<reqwest::Response> {
        let method = Method::from_bytes(prepared.method.as_bytes())
            .map_err(|error| self.internal(format!("unusable method: {error}")))?;
        let mut builder = self
            .client
            .request(method, prepared.url.clone())
            .timeout(timeout);
        for (name, value) in &prepared.headers {
            builder = builder.header(name, value);
        }
        if let Some((content_type, bytes)) = &prepared.body {
            builder = builder
                .header("Content-Type", content_type)
                .body(bytes.clone());
        }

        builder.send().await.map_err(|error| {
            let reason = if error.is_timeout() {
                format!("timed out after {} seconds", timeout.as_secs())
            } else if error.is_connect() {
                "could not connect".to_string()
            } else {
                error.to_string()
            };
            self.internal(format!(
                "{} {} failed: {reason}",
                prepared.method, prepared.display_url
            ))
        })
    }

    /// Build the error for a non-2xx response.
    ///
    /// The status code is in the message *and* in the structured data, because
    /// the two travel differently: a model reading a tool error sees the
    /// message, and a caller inspecting the error programmatically reads the
    /// data.
    #[allow(clippy::too_many_arguments)]
    fn status_error(
        &self,
        endpoint: &str,
        operation: &str,
        prepared: &PreparedRequest,
        status: reqwest::StatusCode,
        bytes: &[u8],
        media_type: &str,
        duration_ms: u64,
    ) -> PluginError {
        let excerpt = excerpt(bytes, media_type);
        let url = self.redactor.redact(prepared.display_url.clone());
        let retryable = is_retryable(status.as_u16());

        let hint = match status.as_u16() {
            401 | 403 => format!(
                " Check the credential for endpoint `{endpoint}` — rest-client reads it from the \
                 environment of the tdcc process, and `rest-client.status` reports which variable."
            ),
            404 => {
                " The endpoint answered, so the request was permitted; the resource itself does \
                    not exist."
                    .to_string()
            }
            429 => " The remote API is rate-limiting this node. `max_calls_per_minute` on this \
                    endpoint limits how fast rest-client will try."
                .to_string(),
            _ if retryable => " This status is usually transient.".to_string(),
            _ => String::new(),
        };

        let mut error = PluginError::internal(self.redactor.redact(format!(
            "{endpoint}.{operation} answered {} {}.{hint}{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown"),
            if excerpt.is_empty() {
                String::new()
            } else {
                format!(" Response body: {excerpt}")
            }
        )));
        error.data_json = self.redactor.redact(
            json!({
                "endpoint": endpoint,
                "operation": operation,
                "method": prepared.method,
                "url": url,
                "status": status.as_u16(),
                "reason": status.canonical_reason(),
                "retryable": retryable,
                "body_excerpt": excerpt,
                "duration_ms": duration_ms,
            })
            .to_string(),
        );
        error
    }

    /// Attach the response body in whichever form is honest for its media type.
    fn attach_body(
        &self,
        result: &mut Value,
        bytes: &[u8],
        media_type: &str,
        truncated: bool,
        limit: usize,
    ) {
        let Some(object) = result.as_object_mut() else {
            return;
        };

        if !net::is_textual_media_type(media_type) {
            object.insert(
                "note".into(),
                json!(format!(
                    "The response is `{media_type}`, which this tool does not turn into text. \
                     Only its status, size, and headers are reported."
                )),
            );
            return;
        }

        let text = String::from_utf8_lossy(bytes).into_owned();
        if net::is_json_media_type(media_type) {
            match serde_json::from_str::<Value>(&text) {
                Ok(parsed) => {
                    object.insert("json".into(), parsed);
                    return;
                }
                Err(error) => {
                    object.insert(
                        "note".into(),
                        json!(if truncated {
                            format!(
                                "The response was cut off at the {limit}-byte cap for this \
                                 endpoint, so it could not be parsed as JSON. The truncated text \
                                 is in `text`. Raise `max_response_bytes` on this endpoint, or \
                                 narrow the request."
                            )
                        } else {
                            format!(
                                "The response claims to be `{media_type}` but did not parse as \
                                 JSON ({error}). The raw text is in `text`."
                            )
                        }),
                    );
                }
            }
        }
        object.insert("text".into(), json!(text));
    }

    fn reported_headers(&self, headers: &reqwest::header::HeaderMap) -> Value {
        let mut reported = serde_json::Map::new();
        for name in REPORTED_HEADERS {
            if let Some(value) = headers.get(*name).and_then(|value| value.to_str().ok()) {
                let value = if value.chars().count() > MAX_HEADER_VALUE_CHARS {
                    value.chars().take(MAX_HEADER_VALUE_CHARS).collect()
                } else {
                    value.to_string()
                };
                reported.insert((*name).to_string(), json!(self.redactor.redact(value)));
            }
        }
        Value::Object(reported)
    }

    fn endpoint(&self, name: &str) -> PluginResult<&crate::catalog::Endpoint> {
        self.catalog.endpoint(name).ok_or_else(|| {
            let declared = self.catalog.names();
            PluginError::invalid_params(if declared.is_empty() {
                format!(
                    "no endpoint named `{name}` — this node declares none at all. Endpoints are \
                     declared by the operator in {}; `rest-client.status` says whether that file \
                     was found.",
                    self.source.path().display()
                )
            } else {
                format!(
                    "no endpoint named `{name}`. This node declares: {}. Call \
                     `rest-client.endpoints` for what each one offers.",
                    declared.join(", ")
                )
            })
        })
    }

    fn invalid_params(&self, message: impl Into<String>) -> PluginError {
        PluginError::invalid_params(self.redactor.redact(message.into()))
    }

    fn invalid_request(&self, message: impl Into<String>) -> PluginError {
        PluginError::invalid_request(self.redactor.redact(message.into()))
    }

    fn internal(&self, message: impl Into<String>) -> PluginError {
        PluginError::internal(self.redactor.redact(message.into()))
    }
}

/// Statuses worth trying again. A model deciding whether to retry should not
/// have to know the HTTP registry by heart.
fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// A bounded, single-line excerpt of an error body.
fn excerpt(bytes: &[u8], media_type: &str) -> String {
    if bytes.is_empty() || !net::is_textual_media_type(media_type) {
        return String::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let flattened: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = flattened.trim();
    if trimmed.chars().count() > MAX_ERROR_EXCERPT_CHARS {
        let head: String = trimmed.chars().take(MAX_ERROR_EXCERPT_CHARS).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// A one-connection-per-request HTTP server on loopback that records what it
/// was asked for, so the call path is exercised against a real socket rather
/// than a mocked client.
#[cfg(test)]
pub(crate) mod stub {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// `(request target, status, content type, body)`.
    pub type Route = (&'static str, u16, &'static str, String);

    #[derive(Clone, Debug)]
    pub struct Recorded {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Recorded {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    pub struct Stub {
        pub base: String,
        requests: Arc<Mutex<Vec<Recorded>>>,
    }

    impl Stub {
        pub fn requests(&self) -> Vec<Recorded> {
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        pub fn last(&self) -> Recorded {
            self.requests().pop().expect("a request was recorded")
        }
    }

    /// Start the server and return its base URL. It stops when the test process
    /// ends; there is nothing to tear down.
    pub async fn start(routes: Vec<Route>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let port = listener.local_addr().expect("local address").port();
        let routes = Arc::new(routes);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buffer = vec![0u8; 4 * 1_024];
                    // Read until the headers are complete, then until the
                    // declared body length has arrived.
                    loop {
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        let Some(head_end) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let length = content_length(&text[..head_end]);
                        if raw.len() >= head_end + 4 + length {
                            break;
                        }
                    }

                    let text = String::from_utf8_lossy(&raw).to_string();
                    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or_default();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET").to_string();
                    let target = parts.next().unwrap_or("/").to_string();
                    let headers: Vec<(String, String)> = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
                        .collect();

                    recorder
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(Recorded {
                            method,
                            target: target.clone(),
                            headers,
                            body: body.to_string(),
                        });

                    let (status, content_type, payload) = routes
                        .iter()
                        .find(|(path, ..)| *path == target)
                        .map(|(_, status, content_type, body)| {
                            (*status, *content_type, body.clone())
                        })
                        .unwrap_or((
                            404,
                            "application/json",
                            r#"{"message":"no such route"}"#.to_string(),
                        ));

                    let response = format!(
                        "HTTP/1.1 {status} Stub\r\nContent-Type: {content_type}\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Stub {
            base: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    /// A declaration pointed at a stub server. `allow_private_base` is set
    /// because the stub is on loopback, which the address guard refuses by
    /// default — the same opt-in an operator makes for a LAN service.
    fn document(base: &str, extra_endpoint: &str) -> String {
        format!(
            r#"
version = 1

[[endpoint]]
name = "demo"
description = "A stub API used by the test suite."
base_url = "{base}"
methods = ["GET", "POST"]
paths = ["/things", "/things/*", "/search", "/binary", "/slow", "/big"]
allow_private_base = true
allow_insecure_auth = true
{extra_endpoint}

[[endpoint.operation]]
name = "get_thing"
description = "Fetch one thing by identifier."
method = "GET"
path = "/things/{{id}}"

[[endpoint.operation.parameter]]
name = "id"
in = "path"
type = "string"
required = true
description = "Identifier of the thing."

[[endpoint.operation]]
name = "list_things"
description = "List things."
method = "GET"
path = "/things"

[[endpoint.operation.parameter]]
name = "limit"
in = "query"
type = "integer"
description = "How many to return."
minimum = 1
maximum = 100
default = 10

[[endpoint.operation]]
name = "search"
description = "Search things."
method = "POST"
path = "/search"

[endpoint.operation.body]
required = true
description = "A JSON object with a `query` string."

[[endpoint.operation]]
name = "binary"
description = "Return a non-textual body."
method = "GET"
path = "/binary"

[[endpoint.operation]]
name = "big"
description = "Return a body larger than the cap."
method = "GET"
path = "/big"
"#
        )
    }

    fn engine(document: &str, env_pairs: &[(&str, &str)]) -> Arc<Engine> {
        let catalog = catalog::parse(document)
            .unwrap_or_else(|error| panic!("the test document must parse:\n{error}"));
        Engine::new(
            catalog,
            CatalogSource::Loaded(PathBuf::from("/test/rest-client.toml")),
            &env(env_pairs),
            "tdcc-rest-client/test",
        )
        .expect("the client builds")
    }

    async fn stub_engine(
        extra_endpoint: &str,
        env_pairs: &[(&str, &str)],
    ) -> (Arc<Engine>, stub::Stub) {
        let server = stub::start(vec![
            (
                "/things/abc",
                200,
                "application/json",
                r#"{"id":"abc","name":"A thing"}"#.to_string(),
            ),
            (
                "/things?limit=10",
                200,
                "application/json",
                r#"{"things":[]}"#.to_string(),
            ),
            (
                "/things?limit=3",
                200,
                "application/json",
                r#"{"things":[1,2,3]}"#.to_string(),
            ),
            (
                "/things?limit=10&api_key=key-value",
                200,
                "application/json",
                r#"{"things":["with a key"]}"#.to_string(),
            ),
            (
                "/things/gone",
                404,
                "application/json",
                r#"{"message":"not found"}"#.to_string(),
            ),
            (
                "/things/broken",
                500,
                "application/json",
                r#"{"message":"internal"}"#.to_string(),
            ),
            (
                "/things/denied",
                401,
                "application/json",
                r#"{"message":"bad credentials"}"#.to_string(),
            ),
            (
                "/things/notjson",
                200,
                "application/json",
                "this is not json".to_string(),
            ),
            (
                // Real APIs do sometimes echo the credential they rejected.
                "/things/leaky",
                500,
                "application/json",
                r#"{"message":"upstream rejected token s3cret-token-value"}"#.to_string(),
            ),
            (
                "/search",
                200,
                "application/json",
                r#"{"results":[{"id":"abc"}]}"#.to_string(),
            ),
            ("/binary", 200, "image/png", "\u{0089}PNG-ish".to_string()),
            (
                "/big",
                200,
                "application/json",
                format!("[\"{}\"]", "x".repeat(5_000)),
            ),
        ])
        .await;
        let engine = engine(&document(&server.base, extra_endpoint), env_pairs);
        (engine, server)
    }

    // -- status and catalog -------------------------------------------------

    #[test]
    fn status_reports_the_config_path_and_every_endpoint_without_touching_the_network() {
        let engine = engine(&document("https://api.example.com", ""), &[]);

        let status = engine.status();

        assert_eq!(status["config_loaded"], true);
        assert_eq!(status["endpoint_count"], 1);
        assert_eq!(status["endpoints"][0]["name"], "demo");
        assert_eq!(status["endpoints"][0]["auth_kind"], "none");
        assert_eq!(status["endpoints"][0]["auth_ready"], true);
        assert_eq!(status["endpoints"][0]["calls_this_minute"], 0);
        assert!(
            status["config_path"]
                .as_str()
                .unwrap()
                .contains("rest-client.toml")
        );
    }

    #[test]
    fn status_on_a_node_with_no_declaration_says_where_the_file_should_be() {
        let engine = Engine::new(
            Catalog::default(),
            CatalogSource::Missing(PathBuf::from("/home/op/.tdcc/rest-client.toml")),
            &env(&[]),
            "tdcc-rest-client/test",
        )
        .expect("the client builds");

        let status = engine.status();

        assert_eq!(status["config_loaded"], false);
        assert_eq!(status["endpoint_count"], 0);
        assert!(
            status["note"]
                .as_str()
                .unwrap()
                .contains("No declaration file"),
            "{status:#}"
        );
        assert!(engine.health().contains("no endpoints declared"));
    }

    #[test]
    fn status_and_endpoints_report_a_missing_credential_by_variable_name_only() {
        let engine = engine(
            &document(
                "https://api.example.com",
                "\n[endpoint.auth]\nkind = \"bearer\"\ntoken_env = \"DEMO_TOKEN\"\n",
            ),
            &[],
        );

        let status = engine.status();
        assert_eq!(status["endpoints"][0]["auth_ready"], false);
        assert_eq!(status["endpoints"][0]["auth_env"], "DEMO_TOKEN");
        assert!(
            status["endpoints"][0]["auth_problem"]
                .as_str()
                .unwrap()
                .contains("DEMO_TOKEN")
        );

        let endpoints = engine.endpoints();
        assert_eq!(endpoints["endpoints"][0]["auth"]["ready"], false);
        assert_eq!(endpoints["endpoints"][0]["auth"]["env"], "DEMO_TOKEN");
    }

    #[test]
    fn endpoints_on_an_empty_catalog_explains_that_a_url_can_never_be_passed() {
        let engine = Engine::new(
            Catalog::default(),
            CatalogSource::Missing(PathBuf::from("/tmp/none.toml")),
            &env(&[]),
            "tdcc-rest-client/test",
        )
        .expect("the client builds");

        let endpoints = engine.endpoints();

        assert_eq!(endpoints["count"], 0);
        assert!(
            endpoints["note"]
                .as_str()
                .unwrap()
                .contains("no way to pass a URL"),
            "{endpoints:#}"
        );
    }

    #[test]
    fn describe_returns_a_json_schema_for_one_operation() {
        let engine = engine(&document("https://api.example.com", ""), &[]);

        let detail = engine
            .describe("demo", Some("list_things"))
            .expect("declared");

        assert_eq!(detail["params_schema"]["type"], "object");
        assert_eq!(
            detail["params_schema"]["properties"]["limit"]["maximum"],
            100.0
        );
        assert_eq!(detail["params_schema"]["additionalProperties"], false);
        assert_eq!(detail["method"], "GET");
    }

    #[test]
    fn describe_names_what_is_declared_when_asked_for_something_that_is_not() {
        let engine = engine(&document("https://api.example.com", ""), &[]);

        let error = engine.describe("nope", None).expect_err("no such endpoint");
        assert!(error.message.contains("demo"), "{}", error.message);

        let error = engine
            .describe("demo", Some("delete_everything"))
            .expect_err("no such operation");
        assert!(error.message.contains("get_thing"), "{}", error.message);
    }

    #[test]
    fn the_generated_call_description_lists_the_declared_operations() {
        let engine = engine(&document("https://api.example.com", ""), &[]);

        let description = engine.call_description();

        assert!(
            description.contains("demo.get_thing — GET /things/{id}"),
            "{description}"
        );
        assert!(description.contains("How many to return."), "{description}");
    }

    // -- the call path ------------------------------------------------------

    #[tokio::test]
    async fn a_declared_operation_reaches_the_stub_and_returns_parsed_json() {
        let (engine, server) = stub_engine("", &[]).await;

        let result = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect("the stub answers");

        assert_eq!(result["status"], 200);
        assert_eq!(result["json"]["name"], "A thing");
        assert_eq!(result["content_type"], "application/json");
        assert_eq!(result["truncated"], false);
        assert_eq!(result["budget"]["used_this_minute"], 1);
        assert!(result["response_headers"]["content-type"].is_string());

        let recorded = server.last();
        assert_eq!(recorded.method, "GET");
        assert_eq!(recorded.target, "/things/abc");
    }

    #[tokio::test]
    async fn a_default_is_applied_and_an_explicit_value_overrides_it() {
        let (engine, server) = stub_engine("", &[]).await;

        engine
            .call("demo", "list_things", &params(&[]), None)
            .await
            .expect("the default applies");
        assert_eq!(server.last().target, "/things?limit=10");

        engine
            .call(
                "demo",
                "list_things",
                &params(&[("limit", Value::from(3))]),
                None,
            )
            .await
            .expect("the explicit value applies");
        assert_eq!(server.last().target, "/things?limit=3");
    }

    #[tokio::test]
    async fn a_bearer_credential_is_sent_as_a_header_and_never_returned() {
        let (engine, server) = stub_engine(
            "\n[endpoint.auth]\nkind = \"bearer\"\ntoken_env = \"DEMO_TOKEN\"\n",
            &[("DEMO_TOKEN", "s3cret-token")],
        )
        .await;

        let result = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect("the stub answers");

        assert_eq!(
            server.last().header("authorization"),
            Some("Bearer s3cret-token")
        );
        let rendered = result.to_string();
        assert!(!rendered.contains("s3cret-token"), "{rendered}");
    }

    #[tokio::test]
    async fn a_query_credential_is_sent_but_redacted_in_the_url_that_comes_back() {
        let (engine, server) = stub_engine(
            "\n[endpoint.auth]\nkind = \"query\"\nparam = \"api_key\"\nvalue_env = \"DEMO_KEY\"\n",
            &[("DEMO_KEY", "key-value")],
        )
        .await;

        let result = engine
            .call("demo", "list_things", &params(&[]), None)
            .await
            .expect("the stub answers");

        assert_eq!(server.last().target, "/things?limit=10&api_key=key-value");
        let rendered = result.to_string();
        assert!(!rendered.contains("key-value"), "{rendered}");
        assert!(
            result["url"].as_str().unwrap().contains("redacted"),
            "{result:#}"
        );
    }

    #[tokio::test]
    async fn a_missing_credential_fails_the_call_and_names_the_variable() {
        let (engine, server) = stub_engine(
            "\n[endpoint.auth]\nkind = \"bearer\"\ntoken_env = \"DEMO_TOKEN\"\n",
            &[],
        )
        .await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect_err("no credential, no call");

        assert!(error.message.contains("DEMO_TOKEN"), "{}", error.message);
        assert!(
            server.requests().is_empty(),
            "nothing should have been sent"
        );
    }

    #[tokio::test]
    async fn a_declared_body_is_posted_with_its_content_type() {
        let (engine, server) = stub_engine("", &[]).await;

        let result = engine
            .call(
                "demo",
                "search",
                &params(&[]),
                Some(&json!({"query": "rust"})),
            )
            .await
            .expect("the stub answers");

        assert_eq!(result["json"]["results"][0]["id"], "abc");
        let recorded = server.last();
        assert_eq!(recorded.method, "POST");
        assert_eq!(recorded.body, r#"{"query":"rust"}"#);
        assert_eq!(recorded.header("content-type"), Some("application/json"));
    }

    // -- failure is never an empty success ----------------------------------

    #[tokio::test]
    async fn a_404_is_an_error_carrying_the_status_and_the_response_body() {
        let (engine, _) = stub_engine("", &[]).await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("gone"))]),
                None,
            )
            .await
            .expect_err("a 404 is a failed call");

        assert!(error.message.contains("404"), "{}", error.message);
        assert!(error.message.contains("Not Found"), "{}", error.message);
        assert!(error.message.contains("not found"), "{}", error.message);

        let data: Value = serde_json::from_str(&error.data_json).expect("structured data");
        assert_eq!(data["status"], 404);
        assert_eq!(data["retryable"], false);
        assert_eq!(data["endpoint"], "demo");
        assert_eq!(data["operation"], "get_thing");
    }

    #[tokio::test]
    async fn a_500_is_distinguishable_from_a_404_and_marked_retryable() {
        let (engine, _) = stub_engine("", &[]).await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("broken"))]),
                None,
            )
            .await
            .expect_err("a 500 is a failed call");

        let data: Value = serde_json::from_str(&error.data_json).expect("structured data");
        assert_eq!(data["status"], 500);
        assert_eq!(data["retryable"], true);
        assert!(error.message.contains("transient"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_401_points_at_the_credential_rather_than_at_the_request() {
        let (engine, _) = stub_engine("", &[]).await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("denied"))]),
                None,
            )
            .await
            .expect_err("a 401 is a failed call");

        assert!(error.message.contains("credential"), "{}", error.message);
        assert!(error.message.contains("environment"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_credential_echoed_back_by_the_remote_api_is_stripped_from_the_error() {
        let (engine, _) = stub_engine(
            "\n[endpoint.auth]\nkind = \"bearer\"\ntoken_env = \"DEMO_TOKEN\"\n",
            &[("DEMO_TOKEN", "s3cret-token-value")],
        )
        .await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("leaky"))]),
                None,
            )
            .await
            .expect_err("a 500 whose body quotes the token");

        assert!(
            !error.message.contains("s3cret-token-value"),
            "{}",
            error.message
        );
        assert!(error.message.contains("<redacted>"), "{}", error.message);
        assert!(
            !error.data_json.contains("s3cret-token-value"),
            "{}",
            error.data_json
        );
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_reports_the_failure_rather_than_an_empty_result() {
        let engine = engine(&document("http://127.0.0.1:1/", ""), &[]);

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect_err("nothing is listening");

        assert!(
            error.message.contains("connect") || error.message.contains("failed"),
            "{}",
            error.message
        );
    }

    // -- bounds -------------------------------------------------------------

    #[tokio::test]
    async fn a_response_over_the_cap_is_truncated_and_says_so() {
        let (engine, _) = stub_engine("\nmax_response_bytes = 1024\n", &[]).await;

        let result = engine
            .call("demo", "big", &params(&[]), None)
            .await
            .expect("the stub answers");

        assert_eq!(result["truncated"], true);
        assert_eq!(result["bytes"], 1024);
        assert!(result["json"].is_null(), "{result:#}");
        assert!(
            result["note"]
                .as_str()
                .unwrap()
                .contains("max_response_bytes"),
            "{result:#}"
        );
    }

    #[tokio::test]
    async fn a_non_textual_body_is_reported_by_type_rather_than_decoded() {
        let (engine, _) = stub_engine("", &[]).await;

        let result = engine
            .call("demo", "binary", &params(&[]), None)
            .await
            .expect("the stub answers");

        assert_eq!(result["content_type"], "image/png");
        assert!(result["text"].is_null(), "{result:#}");
        assert!(result["json"].is_null(), "{result:#}");
        assert!(
            result["note"].as_str().unwrap().contains("image/png"),
            "{result:#}"
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_json_it_claims_to_be_comes_back_as_text_with_a_note() {
        let (engine, _) = stub_engine("", &[]).await;

        let result = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("notjson"))]),
                None,
            )
            .await
            .expect("the stub answers 200");

        assert_eq!(result["text"], "this is not json");
        assert!(
            result["note"].as_str().unwrap().contains("did not parse"),
            "{result:#}"
        );
    }

    #[tokio::test]
    async fn the_per_endpoint_call_budget_is_enforced() {
        let (engine, server) = stub_engine("\nmax_calls_per_minute = 2\n", &[]).await;

        for _ in 0..2 {
            engine
                .call(
                    "demo",
                    "get_thing",
                    &params(&[("id", Value::from("abc"))]),
                    None,
                )
                .await
                .expect("within budget");
        }
        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect_err("over budget");

        assert!(
            error.message.contains("max_calls_per_minute"),
            "{}",
            error.message
        );
        assert_eq!(
            server.requests().len(),
            2,
            "the third call must not be sent"
        );
    }

    #[tokio::test]
    async fn an_invalid_call_does_not_spend_the_budget() {
        let (engine, _) = stub_engine("\nmax_calls_per_minute = 1\n", &[]).await;

        engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from(7))]),
                None,
            )
            .await
            .expect_err("id must be a string");

        engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect("the budget was not spent on a malformed call");
    }

    // -- confinement --------------------------------------------------------

    #[tokio::test]
    async fn a_loopback_base_without_the_opt_in_is_refused_before_any_request() {
        let document =
            document("http://127.0.0.1:9337", "").replace("allow_private_base = true\n", "");
        let engine = engine(&document, &[]);

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("abc"))]),
                None,
            )
            .await
            .expect_err("loopback is refused");

        assert!(
            error.message.contains("allow_private_base"),
            "{}",
            error.message
        );
        assert!(error.message.contains("127.0.0.1"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_caller_cannot_name_an_endpoint_or_operation_the_operator_did_not_declare() {
        let (engine, server) = stub_engine("", &[]).await;

        let error = engine
            .call("internal", "get", &params(&[]), None)
            .await
            .expect_err("no such endpoint");
        assert!(error.message.contains("demo"), "{}", error.message);

        let error = engine
            .call("demo", "delete_everything", &params(&[]), None)
            .await
            .expect_err("no such operation");
        assert!(error.message.contains("get_thing"), "{}", error.message);

        assert!(
            server.requests().is_empty(),
            "nothing should have been sent"
        );
    }

    #[tokio::test]
    async fn a_path_parameter_cannot_reach_a_path_the_operator_did_not_allow() {
        let (engine, server) = stub_engine("", &[]).await;

        let error = engine
            .call(
                "demo",
                "get_thing",
                &params(&[("id", Value::from("../admin/keys"))]),
                None,
            )
            .await
            .expect_err("traversal is refused");

        assert!(
            error.message.contains("path separator"),
            "{}",
            error.message
        );
        assert!(
            server.requests().is_empty(),
            "nothing should have been sent"
        );
    }

    #[test]
    fn retryable_statuses_are_the_transient_ones() {
        for status in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable(status), "{status}");
        }
        for status in [400, 401, 403, 404, 409, 422, 501] {
            assert!(!is_retryable(status), "{status}");
        }
    }

    #[test]
    fn an_excerpt_is_flattened_and_bounded() {
        assert_eq!(excerpt(b"  a\nb  ", "application/json"), "a b");
        assert_eq!(excerpt(b"", "application/json"), "");
        assert_eq!(excerpt(b"\x89PNG", "image/png"), "");

        let long = vec![b'x'; MAX_ERROR_EXCERPT_CHARS * 2];
        let excerpt = excerpt(&long, "text/plain");
        assert_eq!(excerpt.chars().count(), MAX_ERROR_EXCERPT_CHARS + 1);
        assert!(excerpt.ends_with('…'));
    }
}
