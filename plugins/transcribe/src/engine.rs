//! The four tool implementations.
//!
//! Failure is reported, never swallowed. Every path that cannot produce a real
//! transcript — no backend, no root, an unreadable codec, a file over the size
//! cap, a recording needing more chunks than allowed, a server that is not
//! running — returns an error naming the cause and the setting that would fix
//! it. A transcript that comes back empty means the recording was silent.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use tdcc_plugin::{PluginError, PluginResult};

use crate::audio::{self, Format};
use crate::backend::{BackendClient, TranscriptionReply, TranscriptionRequest};
use crate::config::{
    BackendSetup, Config, ENV_API_KEY, PLUGIN_NAME, PLUGIN_VERSION, normalize_language,
};
use crate::listing;
use crate::plan::{self, Chunk};
use crate::roots::{Resolved, Roots, display_path};
use crate::segments::{self, ChunkResult, Segment, format_timestamp};

pub struct Engine {
    config: Config,
    roots: Roots,
    /// `None` when no backend is configured; `backend_problem` then holds the
    /// sentence naming what is missing.
    client: Option<BackendClient>,
    backend_problem: Option<String>,
}

/// A segment as a caller sees it: the machine-readable seconds a player seeks
/// to, and the clock strings a person reads in the same object.
#[derive(Debug, Clone, Serialize)]
pub struct SegmentView {
    pub id: u32,
    pub start: f64,
    pub end: f64,
    pub start_time: String,
    pub end_time: String,
    pub text: String,
}

impl From<&Segment> for SegmentView {
    fn from(segment: &Segment) -> Self {
        Self {
            id: segment.id,
            start: round_millis(segment.start),
            end: round_millis(segment.end),
            start_time: format_timestamp(segment.start),
            end_time: format_timestamp(segment.end),
            text: segment.text.clone(),
        }
    }
}

fn round_millis(seconds: f64) -> f64 {
    (seconds * 1_000.0).round() / 1_000.0
}

impl Engine {
    pub fn new(config: Config) -> Result<Arc<Self>, String> {
        let roots = Roots::open(&config.roots);
        let (client, backend_problem) = match &config.backend {
            BackendSetup::Configured(backend) => (
                Some(BackendClient::new(
                    (**backend).clone(),
                    &config.limits,
                    &config.user_agent,
                    config.send_granularity_field,
                )?),
                None,
            ),
            BackendSetup::Unconfigured(message) => (None, Some(message.clone())),
        };
        Ok(Arc::new(Self {
            config,
            roots,
            client,
            backend_problem,
        }))
    }

    /// A one-line status for the host's health check.
    ///
    /// Deliberately local: health must stay fast and independent of long-running
    /// work, so this never touches the network or the disk.
    pub fn health(&self) -> String {
        let backend = match &self.client {
            Some(client) => format!("backend {}", client.endpoint()),
            None => "no backend configured".to_string(),
        };
        let roots = match self.roots.entries().len() {
            0 => "no audio root configured".to_string(),
            count => format!("{count} audio root(s)"),
        };
        format!("ok; {backend}; {roots}")
    }

    fn client(&self) -> PluginResult<&BackendClient> {
        self.client.as_ref().ok_or_else(|| {
            PluginError::invalid_request(
                self.backend_problem
                    .clone()
                    .unwrap_or_else(|| "transcribe has no backend configured".to_string()),
            )
        })
    }

    // -- status --------------------------------------------------------------

    /// Everything this plugin is configured as, without touching anything.
    ///
    /// This is the tool an operator calls when the other three are failing, so
    /// it must always answer, and it must never need the network to do it.
    pub fn status(&self) -> Value {
        let roots: Vec<Value> = self
            .roots
            .entries()
            .iter()
            .map(|root| {
                json!({
                    "label": root.label,
                    "configured_path": display_path(&root.configured),
                    "available": root.is_available(),
                })
            })
            .collect();

        let backend = match &self.client {
            Some(client) => json!({
                "configured": true,
                "endpoint": client.endpoint(),
                "model": client.model(),
                // Whether, never what.
                "api_key_present": client.has_api_key(),
                "api_key_source": ENV_API_KEY,
            }),
            None => json!({
                "configured": false,
                "problem": self.backend_problem,
            }),
        };

        json!({
            "plugin": PLUGIN_NAME,
            "version": PLUGIN_VERSION,
            "backend": backend,
            "roots": roots,
            "audio_readable": !self.roots.is_empty(),
            "chunking": {
                "chunk_seconds": self.config.chunking.chunk.as_secs_f64(),
                "overlap_seconds": self.config.chunking.overlap.as_secs_f64(),
                "max_chunks": self.config.chunking.max_chunks,
                "sliceable_formats": ["WAV (linear PCM or IEEE float)"],
            },
            "limits": {
                "max_file_bytes": self.config.limits.max_file_bytes,
                "max_upload_bytes": self.config.limits.max_upload_bytes,
                "max_list_entries": self.config.limits.max_list_entries,
                "request_timeout_seconds": self.config.limits.request_timeout.as_secs(),
            },
            "default_language": self.config.default_language,
            "include_hidden": self.config.include_hidden,
            "sends_timestamp_granularities": self.config.send_granularity_field,
        })
    }

    // -- list_audio ----------------------------------------------------------

    pub async fn list_audio(&self, only: Option<String>) -> PluginResult<Value> {
        if self.roots.is_empty() {
            return Err(PluginError::invalid_request(Config::no_roots_message()));
        }
        if let Some(label) = &only
            && !self.roots.labels().iter().any(|known| known == label)
        {
            return Err(PluginError::invalid_params(format!(
                "`{label}` is not one of this plugin's audio roots. Configured roots: {}.",
                self.roots.labels().join(", ")
            )));
        }

        let roots = self.roots.clone();
        let include_hidden = self.config.include_hidden;
        let max_entries = self.config.limits.max_list_entries as usize;
        // A directory walk is blocking I/O; keeping it off the async runtime
        // means a large tree cannot stall the plugin's control connection.
        let listing = tokio::task::spawn_blocking(move || {
            listing::walk(&roots, include_hidden, max_entries, only.as_deref())
        })
        .await
        .map_err(|error| PluginError::internal(format!("listing task failed: {error}")))?;

        serde_json::to_value(&listing).map_err(|error| {
            PluginError::internal(format!("could not encode the listing: {error}"))
        })
    }

    // -- transcribe ----------------------------------------------------------

    pub async fn transcribe(
        &self,
        path: &str,
        language: Option<&str>,
        want_segments: bool,
        prompt: Option<&str>,
    ) -> PluginResult<Value> {
        let client = self.client()?;
        let resolved = self
            .roots
            .resolve(path)
            .map_err(|error| PluginError::invalid_params(error.to_string()))?;

        let language = match language {
            Some(raw) => Some(
                normalize_language(raw, "the `language` argument")
                    .map_err(PluginError::invalid_params)?,
            ),
            None => self.config.default_language.clone(),
        };

        let bytes = self.read_audio(&resolved).await?;
        let format = audio::sniff(&bytes).ok_or_else(|| {
            PluginError::invalid_request(format!(
                "`{}` is not a recognised audio file: its first bytes match no audio container \
                 this plugin knows (WAV, MP3, FLAC, Ogg, MP4/M4A, WebM, AIFF). Nothing was \
                 uploaded.",
                resolved.addressed()
            ))
        })?;
        if !format.is_audio() {
            return Err(PluginError::invalid_request(format!(
                "`{}` is {}, not audio. Nothing was uploaded.",
                resolved.addressed(),
                format.label()
            )));
        }

        let started = Instant::now();
        let mut warnings: Vec<String> = Vec::new();
        let outcome = self
            .run(
                client,
                &resolved,
                &bytes,
                format,
                &language,
                want_segments,
                prompt,
                &mut warnings,
            )
            .await?;

        let views: Vec<SegmentView> = outcome.segments.iter().map(SegmentView::from).collect();
        let text = if outcome.segments.is_empty() {
            outcome.fallback_text
        } else {
            segments::join_text(&outcome.segments)
        };

        Ok(json!({
            "path": resolved.addressed(),
            "format": format.label(),
            "bytes": bytes.len(),
            "duration_seconds": outcome.duration.map(round_millis),
            "backend": client.endpoint(),
            "model": client.model(),
            "language_requested": language,
            "language_detected": outcome.detected_language,
            "chunks": outcome.chunks,
            "chunk_seconds": if outcome.chunks > 1 {
                Some(self.config.chunking.chunk.as_secs_f64())
            } else {
                None
            },
            "overlap_seconds": if outcome.chunks > 1 {
                Some(self.config.chunking.overlap.as_secs_f64())
            } else {
                None
            },
            "segments_available": !views.is_empty(),
            "segments": views,
            "text": text,
            "warnings": warnings,
            "elapsed_seconds": round_millis(started.elapsed().as_secs_f64()),
        }))
    }

    /// Read the file, refusing anything over the configured ceiling before a
    /// byte of it is in memory.
    async fn read_audio(&self, resolved: &Resolved) -> PluginResult<Vec<u8>> {
        let metadata = std::fs::metadata(&resolved.absolute).map_err(|error| {
            PluginError::internal(format!(
                "`{}` could not be read: {}",
                resolved.addressed(),
                error.kind()
            ))
        })?;
        let size = metadata.len();
        if size == 0 {
            return Err(PluginError::invalid_request(format!(
                "`{}` is empty, so there is no audio to transcribe.",
                resolved.addressed()
            )));
        }
        if size > self.config.limits.max_file_bytes {
            return Err(PluginError::invalid_request(format!(
                "`{}` is {size} bytes, over this plugin's `--max-file-bytes` limit of {}. Raise \
                 that limit if reading a file this large is intended.",
                resolved.addressed(),
                self.config.limits.max_file_bytes
            )));
        }

        let path = resolved.absolute.clone();
        let addressed = resolved.addressed();
        tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .map_err(|error| PluginError::internal(format!("read task failed: {error}")))?
            .map_err(|error| {
                PluginError::internal(format!("`{addressed}` could not be read: {}", error.kind()))
            })
    }

    /// Decide between one request and a chunked run, then do it.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        client: &BackendClient,
        resolved: &Resolved,
        bytes: &[u8],
        format: Format,
        language: &Option<String>,
        want_segments: bool,
        prompt: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> PluginResult<Outcome> {
        let shape = match format {
            Format::Wav => match audio::parse_wav(bytes) {
                Ok(shape) => Some(shape),
                Err(error) => {
                    return Err(PluginError::invalid_request(format!(
                        "`{}` has a WAV extension and a RIFF header, but {error}.",
                        resolved.addressed()
                    )));
                }
            },
            _ => None,
        };
        let duration = shape.as_ref().map(audio::WavShape::duration_seconds);
        // Both halves matter: the container has to be one this plugin can cut,
        // and the stream inside it has to be uncompressed frames.
        let sliceable =
            format.is_sliceable() && shape.as_ref().is_some_and(audio::WavShape::is_linear_pcm);

        let over_upload_cap = bytes.len() as u64 > self.config.limits.max_upload_bytes;
        let over_chunk_length =
            duration.is_some_and(|seconds| seconds > self.config.chunking.chunk.as_secs_f64());

        if !sliceable {
            if over_upload_cap {
                return Err(self.too_large_to_send(resolved, bytes.len(), format, shape.as_ref()));
            }
            return self
                .single_request(
                    client,
                    bytes,
                    format,
                    language,
                    want_segments,
                    prompt,
                    duration,
                )
                .await;
        }
        if !over_upload_cap && !over_chunk_length {
            return self
                .single_request(
                    client,
                    bytes,
                    format,
                    language,
                    want_segments,
                    prompt,
                    duration,
                )
                .await;
        }

        let shape = shape.expect("a sliceable file has a parsed shape");
        let duration_seconds = duration.unwrap_or_default();
        let chunks = plan::plan(
            duration_seconds,
            self.config.chunking.chunk.as_secs_f64(),
            self.config.chunking.overlap.as_secs_f64(),
            self.config.chunking.max_chunks,
        )
        .map_err(|error| PluginError::invalid_request(error.to_string()))?;

        // A single chunk that is still over the request ceiling is a settings
        // problem, and saying so beats letting the backend answer 413.
        let bytes_per_second = f64::from(shape.block_align) * f64::from(shape.sample_rate);
        let chunk_bytes = bytes_per_second * self.config.chunking.chunk.as_secs_f64();
        if chunk_bytes > self.config.limits.max_upload_bytes as f64 {
            let fits = (self.config.limits.max_upload_bytes as f64 / bytes_per_second).floor();
            return Err(PluginError::invalid_request(format!(
                "one {:.0}s chunk of `{}` is about {chunk_bytes:.0} bytes, over the \
                 `--max-upload-bytes` ceiling of {}. Lower `--chunk-seconds` to {fits:.0} or less, \
                 or raise `--max-upload-bytes`.",
                self.config.chunking.chunk.as_secs_f64(),
                resolved.addressed(),
                self.config.limits.max_upload_bytes
            )));
        }

        if !want_segments && chunks.len() > 1 {
            warnings.push(
                "This recording was transcribed in overlapping chunks. With `segments` off there \
                 is no timeline to stitch on, so a few words near each chunk boundary may appear \
                 twice in `text`. Ask for segments to get a clean join."
                    .to_string(),
            );
        }

        self.chunked_request(
            client,
            bytes,
            &shape,
            &chunks,
            language,
            want_segments,
            prompt,
            duration_seconds,
            warnings,
        )
        .await
    }

    /// The refusal for a file that is too big to send and impossible to cut.
    ///
    /// Deliberately its own message: "too large" and "too large *and I cannot
    /// do anything about it*" call for different actions from the operator.
    fn too_large_to_send(
        &self,
        resolved: &Resolved,
        size: usize,
        format: Format,
        shape: Option<&audio::WavShape>,
    ) -> PluginError {
        let why = match shape {
            Some(shape) if !shape.is_linear_pcm() => format!(
                "it is a WAV wrapping a compressed payload (format tag {}), which cannot be cut \
                 without decoding it",
                shape.format_tag
            ),
            _ => format!(
                "cutting {format} at an exact second needs an audio decoder, which this plugin \
                 deliberately does not carry"
            ),
        };
        PluginError::invalid_request(format!(
            "`{}` is {size} bytes, over the `--max-upload-bytes` ceiling of {}, and it cannot be \
             split into chunks: {why}. Convert it to a PCM WAV (for example \
             `ffmpeg -i input -ar 16000 -ac 1 output.wav`), which this plugin chunks natively, or \
             raise `--max-upload-bytes` if the backend accepts a body that large.",
            resolved.addressed(),
            self.config.limits.max_upload_bytes
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn single_request(
        &self,
        client: &BackendClient,
        bytes: &[u8],
        format: Format,
        language: &Option<String>,
        want_segments: bool,
        prompt: Option<&str>,
        duration: Option<f64>,
    ) -> PluginResult<Outcome> {
        let reply = client
            .transcribe(TranscriptionRequest {
                audio: bytes.to_vec(),
                filename: upload_name(format),
                mime_type: format.mime_type(),
                language: language.clone(),
                prompt: prompt.map(str::to_string),
                want_segments,
            })
            .await
            .map_err(|error| PluginError::internal(error.to_string()))?;

        let ceiling = duration
            .or(reply.duration)
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(f64::INFINITY);
        let stitched = segments::stitch(
            &[ChunkResult {
                chunk: Chunk::whole(ceiling),
                segments: reply.segments.clone(),
            }],
            ceiling,
        );

        Ok(Outcome {
            chunks: 1,
            duration: duration.or(reply.duration),
            detected_language: reply.language.clone(),
            fallback_text: reply.text.clone().unwrap_or_default(),
            segments: stitched,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn chunked_request(
        &self,
        client: &BackendClient,
        bytes: &[u8],
        shape: &audio::WavShape,
        chunks: &[Chunk],
        language: &Option<String>,
        want_segments: bool,
        prompt: Option<&str>,
        duration: f64,
        warnings: &mut Vec<String>,
    ) -> PluginResult<Outcome> {
        let mut results: Vec<ChunkResult> = Vec::with_capacity(chunks.len());
        let mut texts: Vec<String> = Vec::with_capacity(chunks.len());
        let mut detected_language: Option<String> = None;
        let mut without_segments = 0usize;

        for chunk in chunks {
            let slice =
                audio::slice_wav(bytes, shape, chunk.start, chunk.end).map_err(|error| {
                    PluginError::internal(format!(
                        "chunk {} of this recording could not be cut: {error}",
                        chunk.index + 1
                    ))
                })?;

            let reply: TranscriptionReply = client
                .transcribe(TranscriptionRequest {
                    audio: slice,
                    filename: upload_name(Format::Wav),
                    mime_type: Format::Wav.mime_type(),
                    language: language.clone(),
                    prompt: prompt.map(str::to_string),
                    want_segments,
                })
                .await
                .map_err(|error| {
                    // Which chunk failed matters: a backend that dies on chunk
                    // 9 of 12 is a different problem from one that never
                    // answered at all.
                    PluginError::internal(format!(
                        "chunk {} of {} (from {} to {}) failed: {error}",
                        chunk.index + 1,
                        chunks.len(),
                        format_timestamp(chunk.start),
                        format_timestamp(chunk.end),
                    ))
                })?;

            if detected_language.is_none() {
                detected_language = reply.language.clone();
            }
            if want_segments && reply.segments.is_empty() {
                without_segments += 1;
            }
            if let Some(text) = &reply.text {
                texts.push(text.clone());
            }
            results.push(ChunkResult {
                chunk: chunk.clone(),
                segments: reply.segments,
            });
        }

        if without_segments > 0 {
            warnings.push(format!(
                "{without_segments} of {} chunks came back without timestamped segments, so those \
                 parts of the recording contribute text without a timeline. The backend at {} may \
                 not implement `response_format=verbose_json`.",
                chunks.len(),
                client.endpoint()
            ));
        }

        Ok(Outcome {
            chunks: chunks.len(),
            duration: Some(duration),
            detected_language,
            fallback_text: texts.join(" ").trim().to_string(),
            segments: segments::stitch(&results, duration),
        })
    }

    // -- probe_backend -------------------------------------------------------

    /// Send a short generated recording through the real request path.
    ///
    /// Not a health check on a `/health` route — a transcription endpoint may
    /// not have one, and one answering does not prove a model is loaded. This
    /// uploads 300 ms of silence exactly the way a real chunk is uploaded, so a
    /// success means the whole path works: URL, auth, model name, multipart
    /// shape, and reply parsing.
    pub async fn probe_backend(&self) -> PluginResult<Value> {
        let client = self.client()?;
        let audio = audio::silence_wav(0.3);
        let started = Instant::now();

        let reply = client
            .transcribe(TranscriptionRequest {
                audio: audio.clone(),
                filename: upload_name(Format::Wav),
                mime_type: Format::Wav.mime_type(),
                language: None,
                prompt: None,
                want_segments: true,
            })
            .await
            .map_err(|error| PluginError::internal(error.to_string()))?;

        Ok(json!({
            "reachable": true,
            "endpoint": client.endpoint(),
            "model": client.model(),
            "api_key_present": client.has_api_key(),
            "elapsed_seconds": round_millis(started.elapsed().as_secs_f64()),
            "probe_bytes": audio.len(),
            "probe_seconds": 0.3,
            // A silent probe legitimately transcribes to nothing, so this says
            // whether the backend *can* return segments, not whether it did.
            "returned_segments": !reply.segments.is_empty(),
            "returned_text": reply.text.unwrap_or_default(),
            "detected_language": reply.language,
        }))
    }
}

struct Outcome {
    chunks: usize,
    duration: Option<f64>,
    detected_language: Option<String>,
    /// Used only when no segments came back at all.
    fallback_text: String,
    segments: Vec<Segment>,
}

/// The filename sent in the multipart part.
///
/// Fixed rather than derived from the real path: backends only read the
/// extension, and a caller-supplied name is one more piece of the operator's
/// filesystem than a remote service needs to know.
fn upload_name(format: Format) -> String {
    format!("audio.{}", format.upload_extension())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EnvMap};
    use crate::testutil::{TempTree, wav_fixture};

    fn config_for(tree: &TempTree, backend: Option<&str>, extra: &[&str]) -> Config {
        let mut args: Vec<String> = vec![
            "--root".to_string(),
            tree.path().join("audio").to_string_lossy().into_owned(),
        ];
        if let Some(url) = backend {
            args.push("--backend-url".to_string());
            args.push(url.to_string());
        }
        args.extend(extra.iter().map(|value| (*value).to_string()));
        Config::parse(&args, &EnvMap::new()).expect("test config parses")
    }

    fn engine_for(tree: &TempTree, backend: Option<&str>, extra: &[&str]) -> Arc<Engine> {
        Engine::new(config_for(tree, backend, extra)).expect("client builds")
    }

    fn verbose_json(segments: &[(f64, f64, &str)]) -> String {
        let items: Vec<Value> = segments
            .iter()
            .map(|(start, end, text)| json!({"start": start, "end": end, "text": text}))
            .collect();
        json!({
            "task": "transcribe",
            "language": "english",
            "text": segments.iter().map(|(_, _, text)| *text).collect::<Vec<_>>().join(" "),
            "segments": items,
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_short_recording_is_sent_once_and_comes_back_with_segments() {
        let tree = TempTree::new("engine-single");
        tree.write("audio/note.wav", &wav_fixture(16_000, 1, 3.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.5, "Hello there."), (1.5, 3.0, "General Kenobi.")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let result = engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect("the stub answers with a transcript");

        assert_eq!(result["path"], "audio/note.wav");
        assert_eq!(result["format"], "WAV (RIFF)");
        assert_eq!(result["chunks"], 1);
        assert_eq!(result["segments_available"], true);
        assert_eq!(result["text"], "Hello there. General Kenobi.");
        assert_eq!(result["segments"][0]["start_time"], "00:00:00.000");
        assert_eq!(result["segments"][1]["start"], 1.5);
        assert_eq!(result["segments"][1]["start_time"], "00:00:01.500");
        assert_eq!(result["language_detected"], "english");
        assert!((result["duration_seconds"].as_f64().unwrap() - 3.0).abs() < 1e-6);

        assert_eq!(stub.calls().len(), 1);
    }

    #[tokio::test]
    async fn the_request_carries_the_model_the_format_and_the_language_hint() {
        let tree = TempTree::new("engine-fields");
        tree.write("audio/note.wav", &wav_fixture(16_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "ja")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &["--model", "ggml-base.en"]);

        engine
            .transcribe("audio/note.wav", Some("DE"), true, Some("Kubernetes, etcd"))
            .await
            .expect("transcribes");

        let calls = stub.calls();
        let call = &calls[0];
        assert_eq!(
            stub::field(&call.body, "model").as_deref(),
            Some("ggml-base.en")
        );
        assert_eq!(
            stub::field(&call.body, "response_format").as_deref(),
            Some("verbose_json")
        );
        assert_eq!(stub::field(&call.body, "language").as_deref(), Some("de"));
        assert_eq!(
            stub::field(&call.body, "prompt").as_deref(),
            Some("Kubernetes, etcd")
        );
        assert_eq!(
            stub::field(&call.body, "timestamp_granularities[]").as_deref(),
            Some("segment")
        );
        // The upload is named from the sniffed format, not the real path.
        assert!(
            call.body_text().contains("filename=\"audio.wav\""),
            "{}",
            call.body_text()
        );
        assert!(
            !call.body_text().contains("note.wav"),
            "the real filename is not sent"
        );
    }

    #[tokio::test]
    async fn asking_for_no_segments_asks_the_backend_for_plain_json() {
        let tree = TempTree::new("engine-no-segments");
        tree.write("audio/note.wav", &wav_fixture(16_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            r#"{"text":"words"}"#.to_string(),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let result = engine
            .transcribe("audio/note.wav", None, false, None)
            .await
            .expect("transcribes");

        let calls = stub.calls();
        assert_eq!(
            stub::field(&calls[0].body, "response_format").as_deref(),
            Some("json")
        );
        assert_eq!(
            stub::field(&calls[0].body, "timestamp_granularities[]"),
            None
        );
        assert_eq!(result["segments_available"], false);
        assert_eq!(result["text"], "words");
    }

    #[tokio::test]
    async fn the_granularity_field_can_be_suppressed_for_a_strict_backend() {
        let tree = TempTree::new("engine-no-granularity");
        tree.write("audio/note.wav", &wav_fixture(16_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &["--no-granularity-field"]);

        engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect("transcribes");

        let calls = stub.calls();
        assert_eq!(
            stub::field(&calls[0].body, "response_format").as_deref(),
            Some("verbose_json")
        );
        assert_eq!(
            stub::field(&calls[0].body, "timestamp_granularities[]"),
            None
        );
    }

    /// The reason this plugin cuts audio itself rather than handing the file
    /// over: each chunk is a complete WAV, and the timestamps come back
    /// corrected to absolute time.
    #[tokio::test]
    async fn a_long_recording_is_chunked_with_overlap_and_stitched_back_to_absolute_time() {
        let tree = TempTree::new("engine-chunked");
        // 25 seconds at 8 kHz mono.
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 25.0));
        let stub = stub::start(vec![
            (
                200,
                "application/json",
                verbose_json(&[(0.0, 4.0, "first part"), (7.5, 9.5, "spanning the cut")]),
            ),
            (
                200,
                "application/json",
                verbose_json(&[(0.0, 2.0, "spanning the cut"), (3.0, 5.0, "second part")]),
            ),
            (
                200,
                "application/json",
                verbose_json(&[(1.0, 3.0, "third part")]),
            ),
        ])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &["--chunk-seconds", "10", "--overlap-seconds", "2"],
        );

        let result = engine
            .transcribe("audio/long.wav", None, true, None)
            .await
            .expect("transcribes");

        assert_eq!(result["chunks"], 3, "stride 8s over 25s of audio");
        assert_eq!(result["chunk_seconds"], 10.0);
        assert_eq!(result["overlap_seconds"], 2.0);
        assert_eq!(stub.calls().len(), 3);

        // Chunk 2 starts at 8s and chunk 3 at 16s, so their local times are
        // shifted by exactly that much.
        let texts: Vec<&str> = result["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| segment["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "first part",
                "spanning the cut",
                "second part",
                "third part"
            ],
            "the overlap is heard twice and reported once"
        );
        let starts: Vec<f64> = result["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| segment["start"].as_f64().unwrap())
            .collect();
        assert_eq!(starts, [0.0, 7.5, 11.0, 17.0]);
        assert_eq!(result["segments"][3]["start_time"], "00:00:17.000");
    }

    #[tokio::test]
    async fn every_chunk_sent_is_itself_a_valid_wav_of_the_expected_length() {
        let tree = TempTree::new("engine-chunk-bytes");
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 25.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &["--chunk-seconds", "10", "--overlap-seconds", "2"],
        );

        engine
            .transcribe("audio/long.wav", None, true, None)
            .await
            .expect("transcribes");

        let calls = stub.calls();
        assert_eq!(calls.len(), 3);
        let expected = [10.0, 10.0, 9.0];
        for (index, call) in calls.iter().enumerate() {
            let uploaded = stub::file_part(&call.body).expect("a file part");
            let shape = audio::parse_wav(&uploaded).expect("each chunk is a valid WAV");
            assert_eq!(shape.sample_rate, 8_000);
            assert!(
                (shape.duration_seconds() - expected[index]).abs() < 1e-6,
                "chunk {index} is {}s, expected {}s",
                shape.duration_seconds(),
                expected[index]
            );
        }
    }

    #[tokio::test]
    async fn a_failure_partway_through_a_chunked_run_names_the_chunk_and_the_time() {
        let tree = TempTree::new("engine-chunk-failure");
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 25.0));
        let stub = stub::start(vec![
            (200, "application/json", verbose_json(&[(0.0, 1.0, "fine")])),
            (500, "text/plain", "model crashed".to_string()),
        ])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &["--chunk-seconds", "10", "--overlap-seconds", "2"],
        );

        let error = engine
            .transcribe("audio/long.wav", None, true, None)
            .await
            .expect_err("the second chunk failed");

        assert!(error.message.contains("chunk 2 of 3"), "{}", error.message);
        assert!(error.message.contains("00:00:08.000"), "{}", error.message);
        assert!(error.message.contains("model crashed"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_backend_that_returns_no_segments_for_a_chunked_run_says_so_in_a_warning() {
        let tree = TempTree::new("engine-no-segment-warning");
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 25.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            r#"{"text":"part"}"#.to_string(),
        )])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &["--chunk-seconds", "10", "--overlap-seconds", "2"],
        );

        let result = engine
            .transcribe("audio/long.wav", None, true, None)
            .await
            .expect("still a transcript");

        assert_eq!(result["segments_available"], false);
        assert_eq!(result["text"], "part part part");
        let warnings = result["warnings"].as_array().expect("warnings");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].as_str().unwrap().contains("verbose_json"),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn turning_segments_off_on_a_chunked_run_warns_that_the_seams_may_repeat() {
        let tree = TempTree::new("engine-seam-warning");
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 25.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            r#"{"text":"part"}"#.to_string(),
        )])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &["--chunk-seconds", "10", "--overlap-seconds", "2"],
        );

        let result = engine
            .transcribe("audio/long.wav", None, false, None)
            .await
            .expect("transcribes");

        let warnings = result["warnings"].as_array().expect("warnings");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("twice")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn a_recording_needing_more_chunks_than_allowed_transcribes_nothing() {
        let tree = TempTree::new("engine-too-many-chunks");
        tree.write("audio/long.wav", &wav_fixture(8_000, 1, 60.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &[
                "--chunk-seconds",
                "10",
                "--overlap-seconds",
                "2",
                "--max-chunks",
                "3",
            ],
        );

        let error = engine
            .transcribe("audio/long.wav", None, true, None)
            .await
            .expect_err("over the chunk limit");

        assert!(error.message.contains("--max-chunks"), "{}", error.message);
        assert!(
            stub.calls().is_empty(),
            "nothing is uploaded when the plan is refused"
        );
    }

    #[tokio::test]
    async fn a_compressed_file_over_the_upload_ceiling_is_refused_with_its_own_message() {
        let tree = TempTree::new("engine-too-large-mp3");
        let mut mp3 = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        mp3.resize(200_000, 0);
        tree.write("audio/big.mp3", &mp3);
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &["--max-upload-bytes", "16384"]);

        let error = engine
            .transcribe("audio/big.mp3", None, true, None)
            .await
            .expect_err("too large and not cuttable");

        assert!(
            error.message.contains("--max-upload-bytes"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("cannot be split"),
            "{}",
            error.message
        );
        assert!(error.message.contains("ffmpeg"), "{}", error.message);
        assert!(stub.calls().is_empty(), "nothing is uploaded");
    }

    #[tokio::test]
    async fn a_wav_over_the_upload_ceiling_is_chunked_rather_than_refused() {
        let tree = TempTree::new("engine-large-wav");
        // 20 s at 8 kHz mono = 320 KB, over a 64 KB ceiling but well under the
        // chunk length, so only the byte cap forces the split.
        tree.write("audio/big.wav", &wav_fixture(8_000, 1, 20.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(
            &tree,
            Some(&stub.base),
            &[
                "--max-upload-bytes",
                "65536",
                "--chunk-seconds",
                "10",
                "--overlap-seconds",
                "1",
            ],
        );

        let error = engine
            .transcribe("audio/big.wav", None, true, None)
            .await
            .expect_err("a 10s chunk is still 160 KB");

        // The message does the arithmetic for the operator.
        assert!(
            error.message.contains("--chunk-seconds"),
            "{}",
            error.message
        );
        assert!(error.message.contains("4 or less"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_file_that_is_not_audio_is_named_rather_than_uploaded() {
        let tree = TempTree::new("engine-not-audio");
        tree.write("audio/report.wav", b"%PDF-1.7 this is not a recording");
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/report.wav", None, true, None)
            .await
            .expect_err("a PDF is not audio");

        assert!(
            error.message.contains("a PDF document"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("Nothing was uploaded"),
            "{}",
            error.message
        );
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn unrecognisable_bytes_are_refused_before_any_upload() {
        let tree = TempTree::new("engine-unknown");
        tree.write("audio/mystery.wav", b"\x01\x02\x03\x04 whatever this is");
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/mystery.wav", None, true, None)
            .await
            .expect_err("unknown container");

        assert!(
            error.message.contains("not a recognised audio file"),
            "{}",
            error.message
        );
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn a_corrupt_wav_says_what_is_wrong_with_it() {
        let tree = TempTree::new("engine-corrupt-wav");
        // A RIFF/WAVE header with no fmt chunk at all.
        let mut bytes = b"RIFF\x10\x00\x00\x00WAVE".to_vec();
        bytes.extend_from_slice(b"data\x04\x00\x00\x00\x01\x02\x03\x04");
        tree.write("audio/broken.wav", &bytes);
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/broken.wav", None, true, None)
            .await
            .expect_err("no fmt chunk");

        assert!(error.message.contains("`fmt ` chunk"), "{}", error.message);
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn an_empty_file_is_refused_rather_than_uploaded() {
        let tree = TempTree::new("engine-empty");
        tree.write("audio/nothing.wav", b"");
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/nothing.wav", None, true, None)
            .await
            .expect_err("empty file");
        assert!(error.message.contains("is empty"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_file_over_the_read_ceiling_is_refused_before_it_is_read() {
        let tree = TempTree::new("engine-max-file");
        tree.write("audio/big.wav", &wav_fixture(8_000, 1, 5.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &["--max-file-bytes", "2048"]);

        let error = engine
            .transcribe("audio/big.wav", None, true, None)
            .await
            .expect_err("over the file ceiling");

        assert!(
            error.message.contains("--max-file-bytes"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_path_outside_the_root_is_refused_and_the_root_is_not_disclosed() {
        let tree = TempTree::new("engine-escape");
        tree.write("audio/inside.wav", &wav_fixture(8_000, 1, 1.0));
        tree.write("private/secret.wav", &wav_fixture(8_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let secret = crate::roots::display_path(&tree.canonical_root());
        for path in [
            "../private/secret.wav",
            "/etc/passwd",
            "audio/../../private/secret.wav",
        ] {
            let error = match engine.transcribe(path, None, true, None).await {
                Ok(value) => panic!("{path} must be refused, got {value}"),
                Err(error) => error,
            };
            assert!(
                !error.message.contains(&secret),
                "{path} leaked where the root lives: {}",
                error.message
            );
        }
        // And the file inside the root is unaffected.
        assert!(
            engine
                .transcribe("audio/inside.wav", None, true, None)
                .await
                .is_ok()
        );
        assert_eq!(
            stub.calls().len(),
            1,
            "only the legitimate file was uploaded"
        );
    }

    #[tokio::test]
    async fn with_no_backend_configured_transcribe_names_the_missing_setting() {
        let tree = TempTree::new("engine-no-backend");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        let engine = engine_for(&tree, None, &[]);

        let error = engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect_err("no backend");

        assert!(
            error.message.contains("TDCC_TRANSCRIBE_BACKEND_URL"),
            "{}",
            error.message
        );
        // And the two tools that do not need a backend still work.
        assert!(engine.list_audio(None).await.is_ok());
        assert_eq!(engine.status()["backend"]["configured"], false);
    }

    #[tokio::test]
    async fn with_no_root_configured_transcribe_and_list_both_name_the_missing_setting() {
        let config = Config::parse(
            &[
                "--backend-url".to_string(),
                "http://127.0.0.1:1/x".to_string(),
            ],
            &EnvMap::new(),
        )
        .expect("parses");
        let engine = Engine::new(config).expect("builds");

        let listing = engine.list_audio(None).await.expect_err("no roots");
        assert!(listing.message.contains("--root"), "{}", listing.message);

        let transcribe = engine
            .transcribe("anything.wav", None, true, None)
            .await
            .expect_err("no roots");
        assert!(
            transcribe.message.contains("--root"),
            "{}",
            transcribe.message
        );
    }

    #[tokio::test]
    async fn a_bad_language_hint_is_refused_before_the_file_is_even_opened() {
        let tree = TempTree::new("engine-bad-language");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/note.wav", Some("English"), true, None)
            .await
            .expect_err("not an ISO-639-1 code");

        assert!(error.message.contains("ISO-639-1"), "{}", error.message);
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn auto_means_send_no_language_hint_at_all() {
        let tree = TempTree::new("engine-auto-language");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        let stub = stub::start(vec![(
            200,
            "application/json",
            verbose_json(&[(0.0, 1.0, "x")]),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        engine
            .transcribe("audio/note.wav", Some("auto"), true, None)
            .await
            .expect("transcribes");

        assert_eq!(stub::field(&stub.calls()[0].body, "language"), None);
    }

    #[tokio::test]
    async fn an_api_key_is_sent_as_a_bearer_token_and_never_appears_in_a_result() {
        let tree = TempTree::new("engine-api-key");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        let stub = stub::start(vec![(
            401,
            "application/json",
            r#"{"error":{"message":"bad key"}}"#.to_string(),
        )])
        .await;

        let config = Config::parse(
            &[
                "--root".to_string(),
                tree.path().join("audio").to_string_lossy().into_owned(),
                "--backend-url".to_string(),
                stub.base.clone(),
            ],
            &EnvMap::from([(ENV_API_KEY.to_string(), "sk-live-topsecret".to_string())]),
        )
        .expect("parses");
        let engine = Engine::new(config).expect("builds");

        let error = engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect_err("the stub rejects the key");

        assert_eq!(
            stub.calls()[0].authorization.as_deref(),
            Some("Bearer sk-live-topsecret")
        );
        assert!(error.message.contains(ENV_API_KEY), "{}", error.message);
        assert!(
            !error.message.contains("sk-live-topsecret"),
            "{}",
            error.message
        );
        // Nor in the status payload.
        let status = engine.status();
        assert_eq!(status["backend"]["api_key_present"], true);
        assert!(
            !status.to_string().contains("sk-live-topsecret"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn a_backend_that_is_not_running_is_reported_as_unreachable() {
        let tree = TempTree::new("engine-unreachable");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        // Port 1 on loopback: nothing listens there.
        let engine = engine_for(
            &tree,
            Some("http://127.0.0.1:1/v1/audio/transcriptions"),
            &[],
        );

        let error = engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect_err("nothing is listening");

        assert!(
            error.message.contains("could not reach"),
            "{}",
            error.message
        );
        assert!(error.message.contains("--backend-url"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_404_from_the_backend_explains_that_a_node_does_not_serve_this_itself() {
        let tree = TempTree::new("engine-404");
        tree.write("audio/note.wav", &wav_fixture(8_000, 1, 1.0));
        let stub = stub::start(vec![(404, "text/plain", "not found".to_string())]).await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let error = engine
            .transcribe("audio/note.wav", None, true, None)
            .await
            .expect_err("no such route");

        assert!(error.message.contains("/inference"), "{}", error.message);
        assert!(error.message.contains("--backend-url"), "{}", error.message);
    }

    #[tokio::test]
    async fn probing_the_backend_uploads_a_short_generated_wav_through_the_real_path() {
        let tree = TempTree::new("engine-probe");
        let stub = stub::start(vec![(
            200,
            "application/json",
            r#"{"text":"","segments":[]}"#.to_string(),
        )])
        .await;
        let engine = engine_for(&tree, Some(&stub.base), &[]);

        let result = engine.probe_backend().await.expect("the stub answers");

        assert_eq!(result["reachable"], true);
        assert_eq!(result["endpoint"], stub.base.as_str());
        assert_eq!(result["returned_segments"], false);

        let uploaded = stub::file_part(&stub.calls()[0].body).expect("a file part");
        let shape = audio::parse_wav(&uploaded).expect("the probe is a valid WAV");
        assert_eq!(shape.sample_rate, 16_000);
        assert!((shape.duration_seconds() - 0.3).abs() < 1e-6);
    }

    #[tokio::test]
    async fn probing_a_backend_that_is_not_configured_names_the_setting() {
        let tree = TempTree::new("engine-probe-unconfigured");
        let engine = engine_for(&tree, None, &[]);

        let error = engine.probe_backend().await.expect_err("no backend");
        assert!(
            error.message.contains("TDCC_TRANSCRIBE_BACKEND_URL"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn status_answers_without_a_backend_a_root_or_a_network() {
        let tree = TempTree::new("engine-status");
        tree.write("audio/one.wav", &wav_fixture(8_000, 1, 1.0));
        let engine = engine_for(
            &tree,
            Some("http://127.0.0.1:9/v1/audio/transcriptions"),
            &[],
        );

        let status = engine.status();

        assert_eq!(status["plugin"], PLUGIN_NAME);
        assert_eq!(status["backend"]["configured"], true);
        assert_eq!(status["backend"]["model"], "whisper-1");
        assert_eq!(status["backend"]["api_key_present"], false);
        assert_eq!(status["audio_readable"], true);
        assert_eq!(status["roots"][0]["label"], "audio");
        assert_eq!(status["roots"][0]["available"], true);
        assert_eq!(status["chunking"]["chunk_seconds"], 300.0);
        assert_eq!(status["limits"]["max_upload_bytes"], 24_000_000);
    }

    #[tokio::test]
    async fn listing_returns_the_paths_transcribe_accepts() {
        let tree = TempTree::new("engine-list");
        tree.write("audio/takes/one.wav", &wav_fixture(8_000, 1, 2.0));
        let engine = engine_for(&tree, None, &[]);

        let listing = engine.list_audio(None).await.expect("lists");

        assert_eq!(listing["entries"][0]["path"], "audio/takes/one.wav");
        assert_eq!(listing["truncated"], false);
        assert!((listing["entries"][0]["duration_seconds"].as_f64().unwrap() - 2.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn listing_an_unknown_root_names_the_ones_that_exist() {
        let tree = TempTree::new("engine-list-unknown");
        tree.write("audio/one.wav", &wav_fixture(8_000, 1, 1.0));
        let engine = engine_for(&tree, None, &[]);

        let error = engine
            .list_audio(Some("podcasts".to_string()))
            .await
            .expect_err("no such root");
        assert!(error.message.contains("audio"), "{}", error.message);
    }

    #[test]
    fn health_is_a_local_summary_and_never_a_request() {
        let tree = TempTree::new("engine-health");
        let configured = engine_for(&tree, Some("http://127.0.0.1:9/x"), &[]);
        assert!(configured.health().contains("127.0.0.1:9"));
        assert!(configured.health().contains("1 audio root"));

        let bare =
            Engine::new(Config::parse(&[], &EnvMap::new()).expect("parses")).expect("builds");
        assert!(bare.health().contains("no backend configured"));
        assert!(bare.health().contains("no audio root configured"));
    }

    /// A scripted HTTP server on loopback.
    ///
    /// A stub rather than a mock because the questions worth asking are about
    /// bytes on a socket: how many requests were made, what multipart fields
    /// they carried, and whether each uploaded chunk is a WAV a decoder would
    /// accept. A mocked client cannot answer any of those.
    mod stub {
        use std::sync::{Arc, Mutex};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        pub struct Recorded {
            pub authorization: Option<String>,
            pub body: Vec<u8>,
        }

        impl Recorded {
            pub fn body_text(&self) -> String {
                String::from_utf8_lossy(&self.body).into_owned()
            }
        }

        pub struct Stub {
            pub base: String,
            calls: Arc<Mutex<Vec<Recorded>>>,
        }

        impl Stub {
            pub fn calls(&self) -> std::sync::MutexGuard<'_, Vec<Recorded>> {
                self.calls.lock().expect("stub call log")
            }
        }

        /// `(status, content type, body)` per call; the last entry repeats.
        pub type Reply = (u16, &'static str, String);

        pub async fn start(replies: Vec<Reply>) -> Stub {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("loopback bind");
            let port = listener.local_addr().expect("local address").port();
            let calls: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&calls);
            let replies = Arc::new(replies);

            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let log = Arc::clone(&log);
                    let replies = Arc::clone(&replies);
                    tokio::spawn(async move {
                        let mut raw = Vec::new();
                        let mut buffer = [0u8; 16 * 1_024];
                        let mut header_end = None;
                        let mut content_length = 0usize;

                        loop {
                            let read = match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => read,
                            };
                            raw.extend_from_slice(&buffer[..read]);
                            if header_end.is_none()
                                && let Some(at) = find(&raw, b"\r\n\r\n")
                            {
                                header_end = Some(at + 4);
                                let headers = String::from_utf8_lossy(&raw[..at]).to_lowercase();
                                content_length = headers
                                    .lines()
                                    .find_map(|line| line.strip_prefix("content-length:"))
                                    .and_then(|value| value.trim().parse().ok())
                                    .unwrap_or(0);
                            }
                            if let Some(start) = header_end
                                && raw.len() >= start + content_length
                            {
                                break;
                            }
                        }

                        let start = header_end.unwrap_or(raw.len());
                        let headers =
                            String::from_utf8_lossy(&raw[..start.saturating_sub(4)]).into_owned();
                        let index = {
                            let mut log = log.lock().expect("stub call log");
                            log.push(Recorded {
                                authorization: headers
                                    .lines()
                                    .find(|line| {
                                        line.to_ascii_lowercase().starts_with("authorization:")
                                    })
                                    .and_then(|line| line.split_once(':'))
                                    .map(|(_, value)| value.trim().to_string()),
                                body: raw[start..].to_vec(),
                            });
                            log.len() - 1
                        };

                        let (status, content_type, body) = replies
                            .get(index)
                            .or_else(|| replies.last())
                            .cloned()
                            .unwrap_or((200, "application/json", "{}".to_string()));
                        let response = format!(
                            "HTTP/1.1 {status} Stub\r\nContent-Type: {content_type}\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            Stub {
                base: format!("http://127.0.0.1:{port}/v1/audio/transcriptions"),
                calls,
            }
        }

        fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        }

        /// The part separator, read from the body's own first line rather than
        /// assumed: the client picks a random boundary for every request.
        fn separator(body: &[u8]) -> Option<Vec<u8>> {
            let end = find(body, b"\r\n")?;
            let mut separator = b"\r\n".to_vec();
            separator.extend_from_slice(&body[..end]);
            Some(separator)
        }

        /// The raw bytes of one named part.
        fn part(body: &[u8], name: &str) -> Option<Vec<u8>> {
            let separator = separator(body)?;
            let marker = format!("name=\"{name}\"");
            let at = find(body, marker.as_bytes())?;
            let rest = &body[at + marker.len()..];
            let value_start = find(rest, b"\r\n\r\n")? + 4;
            let value = &rest[value_start..];
            let value_end = find(value, &separator)?;
            Some(value[..value_end].to_vec())
        }

        /// The value of a text field in a multipart body.
        pub fn field(body: &[u8], name: &str) -> Option<String> {
            part(body, name).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        }

        /// The raw bytes of the `file` part.
        pub fn file_part(body: &[u8]) -> Option<Vec<u8>> {
            part(body, "file")
        }
    }
}
