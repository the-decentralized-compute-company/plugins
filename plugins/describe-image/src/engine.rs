//! The tool implementations, and the only place in the plugin that touches the
//! network or the filesystem.
//!
//! Every call follows the same five steps, in this order, because each one is a
//! gate on the next:
//!
//! 1. **Resolve** each reference to bytes — inside a configured root, out of a
//!    `data:` URI, or from a URL that survived the address guard.
//! 2. **Render** each one: sniff, bound, decode, downscale, re-encode.
//! 3. **Choose a model**, from the node's own `/v1/models`.
//! 4. **Ask it**, with the images inlined as `data:` URIs.
//! 5. **Report** what was sent alongside the answer, so a caller can see the
//!    sizes rather than infer them.
//!
//! Failure is reported, never swallowed. No path here returns an empty success:
//! an unreachable endpoint, a mesh with no vision model, a file outside the
//! roots, an image over a cap, and an empty answer each come back as an error
//! naming the cause and, where there is one, the setting that fixes it.

use std::sync::Arc;

use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{Value, json};
use tdcc_plugin::{PluginError, PluginResult};

use crate::chat::{self, Completion};
use crate::config::{Config, PLUGIN_NAME, PLUGIN_VERSION, display_path};
use crate::models::{self, ModelEntry, Selection};
use crate::net;
use crate::render::{self, Rendered};
use crate::source::{self, Kind};

/// Cap on the model list body. Generous for hundreds of entries and still
/// bounded, so a misbehaving endpoint cannot stream the process to death.
const MAX_MODELS_BYTES: usize = 4 * 1_024 * 1_024;

/// Cap on how much of a failing response body is quoted back. Long enough for a
/// real error message, short enough that an HTML error page does not end up in
/// a tool result.
const MAX_ERROR_BODY: usize = 300;

/// Redirect budget when fetching a remote image.
const IMAGE_REDIRECT_LIMIT: u32 = 3;

/// Which of the three questions is being asked. The image handling is
/// identical for all of them; only the instruction and the temperature differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Task {
    Describe,
    Ask,
    ReadText,
}

impl Task {
    fn temperature(self) -> f64 {
        match self {
            Self::ReadText => chat::READ_TEXT_TEMPERATURE,
            _ => chat::DESCRIBE_TEMPERATURE,
        }
    }

    fn instruction(self, image_count: usize, detail: Option<&str>) -> String {
        match self {
            Self::Describe => chat::describe_instruction(image_count, detail),
            Self::Ask => chat::ask_instruction(image_count, detail.unwrap_or_default()),
            Self::ReadText => chat::read_text_instruction(image_count),
        }
    }
}

/// One image, resolved and rendered, with everything needed to report it.
struct PreparedImage {
    label: String,
    kind: &'static str,
    rendered: Rendered,
}

impl PreparedImage {
    fn report(&self) -> Value {
        json!({
            "source": self.kind,
            "label": self.label,
            "original": {
                "width": self.rendered.source_width,
                "height": self.rendered.source_height,
                "format": self.rendered.source_format,
                "bytes": self.rendered.source_bytes,
            },
            "sent": {
                "width": self.rendered.width,
                "height": self.rendered.height,
                "media_type": self.rendered.media_type,
                "bytes": self.rendered.bytes.len(),
            },
            "downscaled": self.rendered.downscaled,
        })
    }
}

pub struct Engine {
    client: Client,
    config: Config,
}

impl Engine {
    pub fn new(config: Config) -> Result<Arc<Self>, String> {
        let user_agent = format!(
            "tdcc-{PLUGIN_NAME}/{PLUGIN_VERSION} \
             (+https://github.com/the-decentralized-compute-company/tdcc-plugins)"
        );
        let client = net::build_client(&user_agent, config.request_timeout)
            .map_err(|error| format!("could not build the HTTP client: {error}"))?;
        Ok(Arc::new(Self { client, config }))
    }

    /// A one-line status for the host's health check.
    ///
    /// Deliberately local: health must stay fast and independent of long-running
    /// work, so this never touches the network.
    pub fn health(&self) -> String {
        format!(
            "ok; endpoint {} model {} roots {}",
            self.config.api_base,
            self.config.model.as_deref().unwrap_or("<discovered>"),
            self.config.roots.len()
        )
    }

    /// What this plugin is configured as, without making a request.
    ///
    /// The configured roots are named here on purpose. They are directories the
    /// operator deliberately opened, and an operator debugging "why can it not
    /// find my photo" needs to see them. Nothing else discloses a path: a
    /// failed lookup never says where the roots are or whether something exists
    /// outside them.
    pub fn status(&self) -> Value {
        let limits = &self.config.limits;
        json!({
            "plugin": PLUGIN_NAME,
            "version": PLUGIN_VERSION,
            "api_base": self.config.api_base.as_str(),
            "api_key": if self.config.api_key.is_some() { "set" } else { "unset" },
            "model": self.config.model,
            "model_selection": if self.config.model.is_some() {
                "pinned"
            } else {
                "discovered from /v1/models"
            },
            "roots": self.config.roots.iter().map(|root| display_path(root)).collect::<Vec<_>>(),
            "local_files": !self.config.roots.is_empty(),
            "remote_images": self.config.allow_remote_images,
            "private_network": self.config.allow_private_network,
            "limits": {
                "max_images_per_call": limits.max_images,
                "max_dimension": limits.max_dimension,
                "max_image_bytes": limits.max_image_bytes,
                "max_pixels": limits.max_pixels,
                "max_tokens": limits.max_tokens,
                "image_format": limits.image_format.label(),
                "jpeg_quality": limits.jpeg_quality,
                "timeout_seconds": self.config.request_timeout.as_secs(),
            },
            "advisories": self.config.advisories(),
        })
    }

    /// Which models this endpoint is serving, and which of them can see.
    ///
    /// Separate from `status` because it makes a request: an operator whose
    /// node is wedged needs one answer that cannot hang and one that says
    /// whether the endpoint is reachable, and merging them gives neither.
    pub async fn vision_models(&self) -> PluginResult<Value> {
        let entries = self.fetch_models().await?;
        let selection = models::select(&entries, self.config.model.as_deref());

        let listed: Vec<Value> = entries
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "display_name": entry.display_name,
                    "vision": entry.declared_vision().map(|confidence| confidence.label()),
                    "vision_status": entry.vision_status,
                    "capabilities": entry.capabilities,
                })
            })
            .collect();

        Ok(json!({
            "endpoint": self.config.api_base.as_str(),
            "models": listed,
            "count": entries.len(),
            "publishes_capabilities": entries.iter().any(ModelEntry::carries_metadata),
            "would_use": selection.as_ref().ok().map(|selection| selection.id.clone()),
            "selected_by": selection.as_ref().ok().map(|selection| selection.selected_by),
            "caveat": selection
                .as_ref()
                .ok()
                .and_then(|selection| selection.confidence)
                .and_then(|confidence| confidence.caveat()),
            // Not an error: listing models is a diagnostic, and "here is
            // everything you have and none of it can see" is the answer.
            "problem": selection.as_ref().err(),
        }))
    }

    // -- the three tools ---------------------------------------------------

    pub async fn run(
        &self,
        task: Task,
        images: &[String],
        detail: Option<&str>,
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> PluginResult<Value> {
        if task == Task::Ask
            && detail
                .map(|question| question.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(PluginError::invalid_params(
                "`question` is required and cannot be empty. Say what you want to know about the \
                 image; use `describe-image.describe` for an open-ended description."
                    .to_string(),
            ));
        }

        let prepared = self.prepare_images(images).await?;
        let selection = self.choose_model(model).await?;

        let instruction = task.instruction(prepared.len(), detail);
        let data_uris: Vec<String> = prepared
            .iter()
            .map(|image| image.rendered.as_data_uri())
            .collect();
        let budget = max_tokens
            .unwrap_or(self.config.limits.max_tokens)
            .clamp(16, self.config.limits.max_tokens);

        let request = chat::build_request(
            &selection.id,
            &instruction,
            &data_uris,
            budget,
            task.temperature(),
        );
        let completion = self.complete(&request, &selection).await?;

        let mut result = json!({
            "model": selection.id,
            "selected_by": selection.selected_by,
            "text": completion.text,
            "images": prepared.iter().map(PreparedImage::report).collect::<Vec<_>>(),
            "finish_reason": completion.finish_reason,
            "usage": {
                "prompt_tokens": completion.prompt_tokens,
                "completion_tokens": completion.completion_tokens,
            },
            "caveat": self.caveat_for(task, &selection),
        });
        if task == Task::ReadText {
            // A model asked to transcribe an image with no text answers with
            // the sentinel; surfacing that as a boolean saves every caller a
            // string comparison against a value they would have to guess.
            let empty = completion.text.trim() == chat::NO_TEXT_SENTINEL;
            result["no_text_found"] = json!(empty);
        }
        Ok(result)
    }

    /// The sentence that goes back with every answer.
    ///
    /// Always present, never conditional on how well things went: the output of
    /// a vision model is a guess about pixels, and a caller that treats it as a
    /// measurement will be wrong eventually regardless of which model produced
    /// it.
    fn caveat_for(&self, task: Task, selection: &Selection) -> String {
        let mut caveat = match task {
            Task::ReadText => String::from(
                "This transcription was produced by a vision language model, not by an OCR \
                 engine. It can misread characters, reorder lines, drop text, and occasionally \
                 invent plausible-looking words. Do not use it where the exact characters matter \
                 without checking them against the image.",
            ),
            _ => String::from(
                "This description was produced by a language model looking at the image. It can \
                 be wrong, confidently: objects, counts, colours, and any text it reports may not \
                 match what is actually there.",
            ),
        };
        if let Some(extra) = selection
            .confidence
            .and_then(|confidence| confidence.caveat())
        {
            caveat.push_str(" Model selection: ");
            caveat.push_str(extra);
            caveat.push('.');
        }
        if selection.selected_by == "configured" && selection.confidence.is_none() {
            caveat.push_str(&format!(
                " Model selection: `{}` was pinned by the operator, and the endpoint does not \
                 report it as vision-capable.",
                selection.id
            ));
        }
        caveat
    }

    // -- images ------------------------------------------------------------

    async fn prepare_images(&self, images: &[String]) -> PluginResult<Vec<PreparedImage>> {
        if images.is_empty() {
            return Err(PluginError::invalid_params(
                "no image was given. Pass at least one local path, \
                 `data:image/...;base64,...` URI, or http(s) URL in `images`."
                    .to_string(),
            ));
        }
        let max = self.config.limits.max_images as usize;
        if images.len() > max {
            return Err(PluginError::invalid_params(format!(
                "{} images were given but this plugin accepts at most {max} per call. Split the \
                 request, or raise --max-images.",
                images.len()
            )));
        }

        let mut prepared = Vec::with_capacity(images.len());
        for (index, raw) in images.iter().enumerate() {
            prepared.push(
                self.prepare_one(raw)
                    .await
                    // The caller gave an ordered list, so an error has to say
                    // which entry failed or a three-image call is a guessing
                    // game.
                    .map_err(|error| PluginError {
                        message: format!("image {} ({}): {}", index + 1, elide(raw), error.message),
                        ..error
                    })?,
            );
        }
        Ok(prepared)
    }

    async fn prepare_one(&self, raw: &str) -> PluginResult<PreparedImage> {
        let kind = source::classify(raw).map_err(PluginError::invalid_params)?;

        let (bytes, declared, label, kind_label) = match &kind {
            Kind::DataUri => {
                let parsed = source::parse_data_uri(raw, self.config.limits.max_image_bytes)
                    .map_err(PluginError::invalid_params)?;
                (
                    parsed.bytes,
                    Some(parsed.media_type.to_string()),
                    source::label_for(&kind, raw, None),
                    "data-uri",
                )
            }
            Kind::Path => {
                let resolved = source::resolve_in_roots(&self.config.roots, raw)
                    .map_err(|error| PluginError::invalid_params(error.to_string()))?;
                let bytes = source::read_capped(&resolved, self.config.limits.max_image_bytes)
                    .map_err(PluginError::invalid_request)?;
                (
                    bytes,
                    None,
                    source::label_for(&kind, raw, Some(&resolved)),
                    "file",
                )
            }
            Kind::Remote(url) => {
                if !self.config.allow_remote_images {
                    return Err(PluginError::invalid_request(
                        "fetching images from URLs is turned off. This node would be making the \
                         request, from its own address, on behalf of whoever called the tool, so \
                         it is opt-in: the operator enables it with `--allow-remote-images`. \
                         Until then, pass a local path or a `data:image/...;base64,...` URI."
                            .to_string(),
                    ));
                }
                let (bytes, declared) = self.fetch_image(url).await?;
                (bytes, declared, source::label_for(&kind, raw, None), "url")
            }
        };

        let rendered = render::render(&bytes, declared.as_deref(), &self.config.limits)
            .map_err(PluginError::invalid_request)?;
        Ok(PreparedImage {
            label,
            kind: kind_label,
            rendered,
        })
    }

    /// Fetch one remote image, re-running the address guard at every redirect.
    ///
    /// Redirects are followed by hand for exactly that reason: a permitted URL
    /// that 302s into `169.254.169.254` must not be followed just because the
    /// first hop passed.
    async fn fetch_image(&self, url: &Url) -> PluginResult<(Vec<u8>, Option<String>)> {
        let mut url = url.clone();
        let mut redirects = 0u32;

        let response = loop {
            net::check_url_destination(&url, self.config.allow_private_network)
                .await
                .map_err(PluginError::invalid_request)?;

            let response = self
                .client
                .get(url.clone())
                .header("Accept", "image/*")
                .send()
                .await
                .map_err(|error| {
                    PluginError::internal(format!("could not fetch {url}: {error}"))
                })?;

            if !response.status().is_redirection() {
                break response;
            }
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
            else {
                break response;
            };
            if redirects >= IMAGE_REDIRECT_LIMIT {
                return Err(PluginError::invalid_request(format!(
                    "that URL exceeded the {IMAGE_REDIRECT_LIMIT} redirect limit; the last hop \
                     was {url}."
                )));
            }
            redirects += 1;
            url = url.join(&location).map_err(|error| {
                PluginError::invalid_request(format!(
                    "{url} redirected to an unusable location `{location}`: {error}"
                ))
            })?;
        };

        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::invalid_request(format!(
                "{} answered {status}.",
                response.url()
            )));
        }

        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .filter(|value| !value.is_empty());

        // A declared non-image type is refused before the body is read: there
        // is no point spending the byte budget on an HTML error page that the
        // decoder will reject anyway. A missing header is allowed through,
        // because sniffing the bytes is a better answer than trusting a header.
        if let Some(media_type) = &media_type
            && !media_type.starts_with("image/")
        {
            return Err(PluginError::invalid_request(format!(
                "{} is `{media_type}`, not an image.",
                response.url()
            )));
        }

        let bytes = read_capped(response, self.config.limits.max_image_bytes as usize).await?;
        Ok((bytes, media_type))
    }

    // -- the endpoint ------------------------------------------------------

    async fn choose_model(&self, requested: Option<&str>) -> PluginResult<Selection> {
        let pinned = requested
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .or(self.config.model.as_deref());
        let entries = self.fetch_models().await?;
        models::select(&entries, pinned).map_err(PluginError::invalid_request)
    }

    async fn fetch_models(&self) -> PluginResult<Vec<ModelEntry>> {
        let url = self.config.models_url();
        let response = self
            .request(self.client.get(url.clone()))
            .send()
            .await
            .map_err(|error| {
                PluginError::internal(format!(
                    "the OpenAI-compatible endpoint at {url} is unreachable: {}. Check that the \
                     node is running and that --api-base points at it.",
                    self.redact(&error.to_string())
                ))
            })?;

        let status = response.status();
        let body = read_capped(response, MAX_MODELS_BYTES).await?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(PluginError::internal(
                self.status_message(status, &url, &body),
            ));
        }
        models::parse_models(&body).map_err(|error| {
            PluginError::internal(format!("{error}, reading the model list from {url}"))
        })
    }

    async fn complete(&self, request: &Value, selection: &Selection) -> PluginResult<Completion> {
        let url = self.config.chat_completions_url();
        let response = self
            .request(self.client.post(url.clone()).json(request))
            .send()
            .await
            .map_err(|error| {
                let error = self.redact(&error.to_string());
                if error.contains("timed out") || error.contains("timeout") {
                    PluginError::internal(format!(
                        "`{}` did not answer within {} seconds. Vision inference on a cold model \
                         is slow — the projector has to load too — so raise --timeout-secs if \
                         this is the first call after a restart.",
                        selection.id,
                        self.config.request_timeout.as_secs()
                    ))
                } else {
                    PluginError::internal(format!("could not reach {url}: {error}"))
                }
            })?;

        let status = response.status();
        let body = read_capped(response, MAX_MODELS_BYTES).await?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(PluginError::internal(
                self.status_message(status, &url, &body),
            ));
        }
        chat::parse_completion(&body).map_err(PluginError::internal)
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.api_key {
            Some(key) => builder.header(reqwest::header::AUTHORIZATION, key.as_header_value()),
            None => builder,
        }
    }

    /// Turn a failing HTTP status into a message that names the setting that
    /// fixes it, rather than a bare number.
    fn status_message(&self, status: StatusCode, url: &Url, body: &str) -> String {
        let quoted = truncate(&self.redact(body), MAX_ERROR_BODY);
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => format!(
                "{url} rejected the credentials ({status}). Set {} in the environment of the tdcc \
                 process, or unset it if that endpoint needs no key. Response: {quoted}",
                crate::config::ENV_API_KEY
            ),
            StatusCode::NOT_FOUND => format!(
                "{url} answered 404. That usually means --api-base points at the wrong prefix — \
                 it should be the base the API hangs off, such as `http://127.0.0.1:9337/v1`, not \
                 a complete endpoint. Response: {quoted}"
            ),
            StatusCode::PAYLOAD_TOO_LARGE => format!(
                "{url} refused the request as too large ({status}). Lower --max-dimension or \
                 --max-images so less image data is sent. Response: {quoted}"
            ),
            StatusCode::BAD_REQUEST => format!(
                "{url} rejected the request ({status}). If the message below mentions images or \
                 content parts, the selected model is not actually vision-capable on this node — \
                 call `describe-image.vision_models` to see what is. Response: {quoted}"
            ),
            other => format!("{url} answered {other}. Response: {quoted}"),
        }
    }

    /// Belt and braces: an error should never quote a request that carried the
    /// key, but if one ever does, it does not carry the key.
    fn redact(&self, message: &str) -> String {
        match &self.config.api_key {
            Some(key) if !key.expose_for_redaction().is_empty() => {
                message.replace(key.expose_for_redaction(), "<redacted>")
            }
            _ => message.to_string(),
        }
    }
}

/// Read a body, stopping at `limit` bytes.
///
/// `Content-Length` is checked first so an obviously oversized response is
/// rejected before it is transferred, but the chunk loop is what actually
/// enforces the cap: a chunked response has no length to check.
async fn read_capped(response: Response, limit: usize) -> PluginResult<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(PluginError::invalid_request(format!(
            "{} is {length} bytes, over the {limit}-byte limit.",
            response.url()
        )));
    }

    let url = response.url().clone();
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| PluginError::internal(format!("reading {url} failed: {error}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            return Err(PluginError::invalid_request(format!(
                "{url} exceeded the {limit}-byte limit; nothing was returned rather than a \
                 truncated body."
            )));
        }
    }
    Ok(body)
}

/// Trim text for an error message, on a character boundary.
fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}… ({} bytes total)", trimmed.len())
}

/// Shorten a caller's reference for an error message.
///
/// A data URI is megabytes long; quoting one back would put the whole image in
/// the error, in the log, and in the model's context.
fn elide(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("data:") {
        let header: String = trimmed.chars().take(32).collect();
        return format!("{header}…");
    }
    truncate(trimmed, 120)
}

#[cfg(test)]
mod stub {
    //! A one-connection-per-request HTTP server on loopback, used to exercise
    //! the real request path without depending on anybody else's uptime.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// `(request target, status, content type, body)`. For a 3xx the body slot
    /// carries the `Location`.
    pub type Route = (&'static str, u16, &'static str, Vec<u8>);

    pub struct Stub {
        pub base: String,
        pub hits: Arc<AtomicUsize>,
    }

    /// Start the server and return its base URL. It stops when the test
    /// process ends; there is nothing to tear down.
    pub async fn start(routes: Vec<Route>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let port = listener.local_addr().expect("local address").port();
        let routes = Arc::new(routes);
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    // Read until the headers end, then drain the declared body
                    // so the client is not writing into a closed socket.
                    let mut buffer = Vec::new();
                    let mut chunk = vec![0u8; 16 * 1_024];
                    loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buffer);
                        if let Some(headers_end) = text.find("\r\n\r\n") {
                            let declared = text
                                .to_ascii_lowercase()
                                .split("content-length:")
                                .nth(1)
                                .and_then(|rest| {
                                    rest.split("\r\n").next()?.trim().parse::<usize>().ok()
                                })
                                .unwrap_or(0);
                            if buffer.len() >= headers_end + 4 + declared {
                                break;
                            }
                        }
                    }

                    let request = String::from_utf8_lossy(&buffer).to_string();
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    counter.fetch_add(1, Ordering::SeqCst);

                    let (status, content_type, body) = routes
                        .iter()
                        .find(|(path, ..)| *path == target)
                        .map(|(_, status, content_type, body)| {
                            (*status, *content_type, body.clone())
                        })
                        .unwrap_or((404, "text/plain", b"not found".to_vec()));

                    let location = if (300..400).contains(&status) {
                        format!("Location: {}\r\n", String::from_utf8_lossy(&body))
                    } else {
                        String::new()
                    };
                    let payload = if location.is_empty() {
                        body
                    } else {
                        Vec::new()
                    };
                    let head = format!(
                        "HTTP/1.1 {status} Stub\r\nContent-Type: {content_type}\r\n{location}\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Stub {
            base: format!("http://127.0.0.1:{port}"),
            hits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    /// The model list a TDCC node with one vision model answers with.
    const MODELS_BODY: &str = r#"{"object":"list","data":[
        {"id":"Llama-3.1-8B-Instruct","capabilities":["text"],"vision_status":"none"},
        {"id":"Qwen3-VL-4B-Instruct","capabilities":["text","vision"],"vision_status":"supported"}
    ]}"#;

    const TEXT_ONLY_MODELS_BODY: &str = r#"{"object":"list","data":[
        {"id":"Llama-3.1-8B-Instruct","capabilities":["text"],"vision_status":"none"}
    ]}"#;

    fn completion_body(text: &str) -> Vec<u8> {
        json!({
            "id": "chatcmpl-stub",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": text }
            }],
            "usage": { "prompt_tokens": 812, "completion_tokens": 12 }
        })
        .to_string()
        .into_bytes()
    }

    /// A small PNG with a black bar, encoded the way a caller would send it.
    fn sample_png(width: u32, height: u32) -> Vec<u8> {
        let mut buffer = image::RgbImage::new(width, height);
        for (x, y, pixel) in buffer.enumerate_pixels_mut() {
            *pixel = if y * 4 / height.max(1) == 1 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 200])
            };
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("the sample encodes");
        bytes
    }

    fn sample_data_uri(width: u32, height: u32) -> String {
        format!(
            "data:image/png;base64,{}",
            BASE64.encode(sample_png(width, height))
        )
    }

    fn engine_for(base: &str, extra: &[&str]) -> Arc<Engine> {
        let mut args = vec!["--api-base".to_string(), base.to_string()];
        args.extend(extra.iter().map(|value| (*value).to_string()));
        let config = Config::parse(&args, &Default::default()).expect("config parses");
        Engine::new(config).expect("client builds")
    }

    async fn node_stub(models: &'static str, completion: Vec<u8>) -> stub::Stub {
        stub::start(vec![
            (
                "/v1/models",
                200,
                "application/json",
                models.as_bytes().to_vec(),
            ),
            ("/v1/chat/completions", 200, "application/json", completion),
        ])
        .await
    }

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "describe-image-engine-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock is after 1970")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("temp tree is creatable");
            Self(path)
        }

        fn write(&self, relative: &str, bytes: &[u8]) {
            let target = self.0.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("parent is creatable");
            }
            std::fs::write(target, bytes).expect("file is writable");
        }

        fn root(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn a_data_uri_is_described_end_to_end_and_the_result_reports_what_was_sent() {
        let stub = node_stub(
            MODELS_BODY,
            completion_body("A colour gradient with a black bar."),
        )
        .await;
        let engine = engine_for(&stub.base, &[]);

        let result = engine
            .run(
                Task::Describe,
                &[sample_data_uri(2_000, 1_500)],
                None,
                None,
                None,
            )
            .await
            .expect("the stub node answers");

        assert_eq!(result["model"], "Qwen3-VL-4B-Instruct");
        assert_eq!(result["selected_by"], "declared");
        assert_eq!(result["text"], "A colour gradient with a black bar.");
        assert_eq!(result["usage"]["prompt_tokens"], 812);

        let image = &result["images"][0];
        assert_eq!(image["source"], "data-uri");
        assert_eq!(image["label"], "data: URI");
        assert_eq!(image["original"]["width"], 2_000);
        assert_eq!(image["sent"]["width"], 1_024);
        assert_eq!(image["sent"]["height"], 768);
        assert_eq!(image["downscaled"], true);
        assert!(
            image["sent"]["bytes"].as_u64().expect("a number")
                < image["original"]["bytes"].as_u64().expect("a number"),
            "{image:#}"
        );
    }

    #[tokio::test]
    async fn every_answer_carries_the_caveat_that_a_model_can_be_wrong() {
        let stub = node_stub(MODELS_BODY, completion_body("A cat.")).await;
        let engine = engine_for(&stub.base, &[]);

        let described = engine
            .run(Task::Describe, &[sample_data_uri(64, 64)], None, None, None)
            .await
            .expect("answers");
        assert!(
            described["caveat"]
                .as_str()
                .expect("a caveat")
                .contains("can be wrong"),
            "{described:#}"
        );

        let transcribed = engine
            .run(Task::ReadText, &[sample_data_uri(64, 64)], None, None, None)
            .await
            .expect("answers");
        let caveat = transcribed["caveat"].as_str().expect("a caveat");
        assert!(caveat.contains("not by an OCR engine"), "{caveat}");
    }

    #[tokio::test]
    async fn read_text_flags_the_sentinel_so_no_text_is_distinguishable_from_a_refusal() {
        let stub = node_stub(MODELS_BODY, completion_body(chat::NO_TEXT_SENTINEL)).await;
        let engine = engine_for(&stub.base, &[]);

        let result = engine
            .run(Task::ReadText, &[sample_data_uri(64, 64)], None, None, None)
            .await
            .expect("answers");

        assert_eq!(result["no_text_found"], true);
    }

    #[tokio::test]
    async fn ask_requires_a_question() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        for question in [None, Some(""), Some("   ")] {
            let error = engine
                .run(Task::Ask, &[sample_data_uri(32, 32)], question, None, None)
                .await
                .expect_err("a question is required");
            assert!(error.message.contains("`question` is required"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_file_inside_a_configured_root_is_read_and_labelled_by_its_name() {
        let tree = TempTree::new("root-read");
        tree.write("album/holiday.png", &sample_png(300, 200));
        let stub = node_stub(MODELS_BODY, completion_body("A beach.")).await;
        let engine = engine_for(&stub.base, &["--root", &tree.root()]);

        let result = engine
            .run(
                Task::Describe,
                &["album/holiday.png".to_string()],
                None,
                None,
                None,
            )
            .await
            .expect("the file is inside the root");

        assert_eq!(result["images"][0]["source"], "file");
        assert_eq!(result["images"][0]["label"], "holiday.png");
        assert_eq!(result["images"][0]["downscaled"], false);
    }

    #[tokio::test]
    async fn a_local_path_is_refused_outright_when_no_root_is_configured() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(Task::Describe, &["photo.png".to_string()], None, None, None)
            .await
            .expect_err("the default is no filesystem access");

        assert!(error.message.contains("--root"), "{error}");
        assert!(error.message.contains("image 1"), "{error}");
    }

    #[tokio::test]
    async fn a_path_outside_the_root_is_refused_without_disclosing_the_root() {
        let tree = TempTree::new("outside");
        tree.write("root/a.png", &sample_png(32, 32));
        tree.write("secret/passwords.png", &sample_png(32, 32));
        let root = format!("{}/root", tree.root());
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &["--root", &root]);

        let error = engine
            .run(
                Task::Describe,
                &["../secret/passwords.png".to_string()],
                None,
                None,
                None,
            )
            .await
            .expect_err("traversal is refused");

        assert!(error.message.contains("'..'"), "{error}");
        assert!(!error.message.contains("passwords.png\n"), "{error}");
    }

    #[tokio::test]
    async fn remote_urls_are_refused_until_the_operator_opts_in() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(
                Task::Describe,
                &["https://example.com/cat.jpg".to_string()],
                None,
                None,
                None,
            )
            .await
            .expect_err("remote fetching is off by default");

        assert!(error.message.contains("--allow-remote-images"), "{error}");
    }

    #[tokio::test]
    async fn an_opted_in_remote_url_still_cannot_reach_the_operators_own_network() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &["--allow-remote-images"]);

        let error = engine
            .run(
                Task::Describe,
                &["http://169.254.169.254/latest/meta-data/".to_string()],
                None,
                None,
                None,
            )
            .await
            .expect_err("link-local is refused");

        assert!(error.message.contains("--allow-private-network"), "{error}");
    }

    #[tokio::test]
    async fn a_remote_image_is_fetched_when_both_guards_are_opened() {
        let images = stub::start(vec![(
            "/cat.png",
            200,
            "image/png",
            sample_png(1_600, 1_200),
        )])
        .await;
        let node = node_stub(MODELS_BODY, completion_body("A cat.")).await;
        let engine = engine_for(
            &node.base,
            &["--allow-remote-images", "--allow-private-network"],
        );

        let result = engine
            .run(
                Task::Describe,
                &[format!("{}/cat.png", images.base)],
                None,
                None,
                None,
            )
            .await
            .expect("the stub serves an image");

        assert_eq!(result["images"][0]["source"], "url");
        assert_eq!(result["images"][0]["original"]["width"], 1_600);
        assert_eq!(result["images"][0]["sent"]["width"], 1_024);
    }

    #[tokio::test]
    async fn a_remote_url_that_serves_a_web_page_is_refused_by_type() {
        let pages = stub::start(vec![(
            "/cat",
            200,
            "text/html",
            b"<html>not an image</html>".to_vec(),
        )])
        .await;
        let node = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(
            &node.base,
            &["--allow-remote-images", "--allow-private-network"],
        );

        let error = engine
            .run(
                Task::Describe,
                &[format!("{}/cat", pages.base)],
                None,
                None,
                None,
            )
            .await
            .expect_err("HTML is not an image");

        assert!(error.message.contains("text/html"), "{error}");
    }

    #[tokio::test]
    async fn a_redirect_into_private_space_is_not_followed() {
        let hop = stub::start(vec![(
            "/cat.png",
            302,
            "text/plain",
            b"http://127.0.0.1:9337/v1/models".to_vec(),
        )])
        .await;
        let node = node_stub(MODELS_BODY, completion_body("x")).await;
        // Remote images are allowed and the *first* hop is loopback-permitted,
        // but the guard runs again after the redirect.
        let engine = engine_for(&node.base, &["--allow-remote-images"]);

        let error = engine
            .run(
                Task::Describe,
                &[format!("{}/cat.png", hop.base)],
                None,
                None,
                None,
            )
            .await
            .expect_err("the first hop is already private");

        assert!(error.message.contains("--allow-private-network"), "{error}");
    }

    #[tokio::test]
    async fn a_redirect_loop_terminates_at_the_budget_rather_than_spinning() {
        let hop = stub::start(vec![("/cat.png", 302, "text/plain", b"/cat.png".to_vec())]).await;
        let node = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(
            &node.base,
            &["--allow-remote-images", "--allow-private-network"],
        );

        let error = engine
            .run(
                Task::Describe,
                &[format!("{}/cat.png", hop.base)],
                None,
                None,
                None,
            )
            .await
            .expect_err("a redirect loop must terminate");

        assert!(error.message.contains("redirect limit"), "{error}");
        assert_eq!(
            hop.hits.load(std::sync::atomic::Ordering::SeqCst),
            IMAGE_REDIRECT_LIMIT as usize + 1,
            "the budget has to bound the number of requests actually made"
        );
    }

    #[tokio::test]
    async fn more_images_than_the_cap_are_refused_before_anything_is_decoded() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &["--max-images", "2"]);

        let error = engine
            .run(
                Task::Describe,
                &[
                    sample_data_uri(32, 32),
                    sample_data_uri(32, 32),
                    sample_data_uri(32, 32),
                ],
                None,
                None,
                None,
            )
            .await
            .expect_err("over the per-call cap");

        assert!(error.message.contains("--max-images"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_image_list_is_refused_with_the_accepted_forms_named() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(Task::Describe, &[], None, None, None)
            .await
            .expect_err("nothing to look at");

        assert!(error.message.contains("data:image"), "{error}");
    }

    #[tokio::test]
    async fn an_error_names_which_image_in_the_list_failed_without_quoting_a_whole_data_uri() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);
        let huge = format!("data:image/png;base64,{}", "A".repeat(4_000));

        let error = engine
            .run(
                Task::Describe,
                &[sample_data_uri(32, 32), huge],
                None,
                None,
                None,
            )
            .await
            .expect_err("the second one is not a real image");

        assert!(error.message.contains("image 2"), "{error}");
        assert!(
            error.message.len() < 600,
            "the data URI must not be quoted back whole: {} chars",
            error.message.len()
        );
    }

    #[tokio::test]
    async fn a_mesh_with_no_vision_model_fails_with_what_it_does_serve() {
        let stub = node_stub(TEXT_ONLY_MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("nothing can see");

        assert!(error.message.contains("Llama-3.1-8B-Instruct"), "{error}");
        assert!(error.message.contains("--model"), "{error}");
    }

    #[tokio::test]
    async fn a_pinned_model_that_is_not_served_fails_before_any_image_is_sent() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &["--model", "Qwen3-VL-4B-Instrukt"]);

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("a typo must surface as a typo");

        assert!(error.message.contains("is not being served"), "{error}");
    }

    #[tokio::test]
    async fn a_per_call_model_argument_wins_over_the_configured_one() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &["--model", "Qwen3-VL-4B-Instruct"]);

        let result = engine
            .run(
                Task::Describe,
                &[sample_data_uri(32, 32)],
                None,
                Some("Llama-3.1-8B-Instruct"),
                None,
            )
            .await
            .expect("the caller's choice is honoured");

        assert_eq!(result["model"], "Llama-3.1-8B-Instruct");
        assert_eq!(result["selected_by"], "configured");
        assert!(
            result["caveat"]
                .as_str()
                .expect("a caveat")
                .contains("does not report it as vision-capable"),
            "pinning a text model has to be said out loud: {result:#}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_names_the_setting_rather_than_returning_nothing() {
        // Port 1 on loopback: nothing listens there.
        let engine = engine_for("http://127.0.0.1:1/v1", &[]);

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("nothing is listening");

        assert!(error.message.contains("unreachable"), "{error}");
        assert!(error.message.contains("--api-base"), "{error}");
    }

    #[tokio::test]
    async fn an_unauthorised_endpoint_names_the_environment_variable() {
        let stub = stub::start(vec![(
            "/v1/models",
            401,
            "application/json",
            br#"{"error":{"message":"invalid key"}}"#.to_vec(),
        )])
        .await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("401 is a failure");

        assert!(
            error.message.contains(crate::config::ENV_API_KEY),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_400_from_the_completion_points_at_the_vision_models_tool() {
        let stub = stub::start(vec![
            (
                "/v1/models",
                200,
                "application/json",
                MODELS_BODY.as_bytes().to_vec(),
            ),
            (
                "/v1/chat/completions",
                400,
                "application/json",
                br#"{"error":{"message":"this model does not support image input"}}"#.to_vec(),
            ),
        ])
        .await;
        let engine = engine_for(&stub.base, &[]);

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("400 is a failure");

        assert!(error.message.contains("vision_models"), "{error}");
        assert!(
            error.message.contains("does not support image input"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_api_key_never_appears_in_an_error() {
        let stub = stub::start(vec![(
            "/v1/models",
            500,
            "text/plain",
            b"upstream failed while authenticating sk-super-secret".to_vec(),
        )])
        .await;
        let config = Config::parse(
            &["--api-base".to_string(), stub.base.clone()],
            &[(
                crate::config::ENV_API_KEY.to_string(),
                "sk-super-secret".to_string(),
            )]
            .into_iter()
            .collect(),
        )
        .expect("config parses");
        let engine = Engine::new(config).expect("client builds");

        let error = engine
            .run(Task::Describe, &[sample_data_uri(32, 32)], None, None, None)
            .await
            .expect_err("500 is a failure");

        assert!(!error.message.contains("sk-super-secret"), "{error}");
        assert!(error.message.contains("<redacted>"), "{error}");
    }

    #[tokio::test]
    async fn the_request_actually_carries_the_image_as_an_image_url_content_part() {
        // The stub echoes nothing back, so this asserts on the builder that
        // produced the body plus the fact that the round trip succeeded.
        let stub = node_stub(MODELS_BODY, completion_body("ok")).await;
        let engine = engine_for(&stub.base, &[]);
        let prepared = engine
            .prepare_images(&[sample_data_uri(120, 90)])
            .await
            .expect("prepares");

        let uris: Vec<String> = prepared
            .iter()
            .map(|image| image.rendered.as_data_uri())
            .collect();
        let request = chat::build_request("m", "instruction", &uris, 64, 0.0);
        let part = &request["messages"][0]["content"][1];

        assert_eq!(part["type"], "image_url");
        assert!(
            part["image_url"]["url"]
                .as_str()
                .expect("a url")
                .starts_with("data:image/png;base64,"),
            "{part:#}"
        );
    }

    #[tokio::test]
    async fn status_answers_without_touching_the_network() {
        // Deliberately an endpoint nothing listens on: `status` must not care.
        let engine = engine_for("http://127.0.0.1:1/v1", &["--max-dimension", "800"]);

        let status = engine.status();

        assert_eq!(status["plugin"], PLUGIN_NAME);
        assert_eq!(status["api_base"], "http://127.0.0.1:1/v1/");
        assert_eq!(status["api_key"], "unset");
        assert_eq!(status["local_files"], false);
        assert_eq!(status["remote_images"], false);
        assert_eq!(status["limits"]["max_dimension"], 800);
        assert!(
            status["advisories"]
                .as_array()
                .expect("advisories")
                .iter()
                .any(|line| line.as_str().unwrap_or_default().contains("--root")),
            "{status:#}"
        );
    }

    #[tokio::test]
    async fn vision_models_lists_everything_and_says_which_would_be_used() {
        let stub = node_stub(MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let listed = engine.vision_models().await.expect("the stub answers");

        assert_eq!(listed["count"], 2);
        assert_eq!(listed["publishes_capabilities"], true);
        assert_eq!(listed["would_use"], "Qwen3-VL-4B-Instruct");
        assert_eq!(listed["selected_by"], "declared");
        assert_eq!(listed["models"][0]["vision"], Value::Null);
        assert_eq!(listed["models"][1]["vision"], "declared");
        assert_eq!(listed["problem"], Value::Null);
    }

    #[tokio::test]
    async fn vision_models_reports_the_problem_rather_than_erroring_when_nothing_can_see() {
        let stub = node_stub(TEXT_ONLY_MODELS_BODY, completion_body("x")).await;
        let engine = engine_for(&stub.base, &[]);

        let listed = engine
            .vision_models()
            .await
            .expect("listing still succeeds");

        assert_eq!(listed["would_use"], Value::Null);
        assert!(
            listed["problem"]
                .as_str()
                .expect("a problem")
                .contains("none of the models"),
            "{listed:#}"
        );
    }

    #[tokio::test]
    async fn vision_models_still_fails_when_the_endpoint_is_unreachable() {
        let engine = engine_for("http://127.0.0.1:1/v1", &[]);
        let error = engine
            .vision_models()
            .await
            .expect_err("an unreachable endpoint is not an empty list");
        assert!(error.message.contains("unreachable"), "{error}");
    }

    #[tokio::test]
    async fn health_is_local_and_names_the_endpoint() {
        let engine = engine_for("http://127.0.0.1:1/v1", &[]);
        assert!(engine.health().starts_with("ok;"), "{}", engine.health());
        assert!(
            engine.health().contains("127.0.0.1:1"),
            "{}",
            engine.health()
        );
    }

    #[test]
    fn a_data_uri_is_elided_rather_than_quoted_into_an_error() {
        let raw = format!("data:image/png;base64,{}", "A".repeat(100_000));
        let elided = elide(&raw);
        assert!(elided.len() < 64, "{elided}");
        assert!(elided.starts_with("data:image/png"), "{elided}");
    }

    #[test]
    fn a_path_is_shortened_but_readable_in_an_error() {
        assert_eq!(elide("  album/holiday.png "), "album/holiday.png");
        assert!(elide(&"a".repeat(500)).contains("bytes total"));
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
}
