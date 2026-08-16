//! The whole contribution surface of `transcribe` in one declaration.
//!
//! Four MCP tools, one capability, and a health hook. The host synthesizes
//! `tools/list`, `tools/call`, the JSON Schema for every argument, and the
//! request validation that runs before a handler is entered — this plugin opens
//! no socket and speaks no MCP.
//!
//! There is deliberately **no** `config_schema`, no `web_ui`, no HTTP route,
//! and no mesh surface. `[plugin.settings]` never reaches a plugin process, so
//! a schema here would render console controls that could not change which
//! directory this plugin reads. Delivery of mesh traffic is allowlist-based, so
//! declaring nothing means receiving nothing, which is the right posture for
//! something that reads people's recordings.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{PluginMetadata, SimplePlugin, capability, mcp, plugin, plugin_server_info};

use crate::config::{PLUGIN_NAME, PLUGIN_VERSION};
use crate::engine::Engine;

/// Arguments for the `transcribe` tool.
///
/// Every doc comment in this struct becomes a `description` in the JSON Schema
/// the host advertises, so a model reads these words when it decides how to
/// call the tool. They are written for that audience.
///
/// `deny_unknown_fields` because a misspelled argument that was silently
/// ignored — `langauge`, say — would transcribe the wrong language and give no
/// sign that anything was wrong.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscribeArgs {
    /// The audio file to transcribe, written as `<root>/<path>` — exactly one
    /// of the `path` values that `transcribe.list_audio` returns, for example
    /// `podcasts/2024/episode-12.wav`. Paths are relative to the directories
    /// the operator configured; absolute paths and `..` are refused.
    pub path: String,

    /// Two-letter ISO-639-1 code for the language being spoken, such as `en`,
    /// `de`, or `ja`. A correct hint makes transcription faster and more
    /// accurate. Omit it, or pass `auto`, to let the backend detect it.
    #[serde(default)]
    pub language: Option<String>,

    /// Return timestamped segments as well as the plain text. Default true.
    /// Keep it on unless you only want the words: segments are what let you
    /// quote a moment ("at 00:14:32 she said…") instead of a whole recording,
    /// and they are what makes a long file navigable.
    #[serde(default)]
    pub segments: Option<bool>,

    /// Optional context to bias the transcription — names, jargon, or acronyms
    /// that appear in the audio and are easy to mishear, given as a short
    /// phrase list such as `Kubernetes, etcd, Anthropic`. It is passed to the
    /// backend as a prompt, not transcribed.
    #[serde(default)]
    pub prompt: Option<String>,
}

impl TranscribeArgs {
    pub fn want_segments(&self) -> bool {
        self.segments.unwrap_or(true)
    }
}

/// Arguments for the `list_audio` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAudioArgs {
    /// Restrict the listing to one configured root, named by its label — the
    /// first segment of any `path` this tool returns. Omit it to list them all.
    #[serde(default)]
    pub root: Option<String>,
}

/// Arguments for the tools that take none.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

pub fn transcribe_plugin(engine: Arc<Engine>) -> SimplePlugin {
    // One clone per handler closure; the closures are `Fn`, so each needs its
    // own handle rather than borrowing a shared one.
    let for_transcribe = Arc::clone(&engine);
    let for_list = Arc::clone(&engine);
    let for_status = Arc::clone(&engine);
    let for_probe = Arc::clone(&engine);
    let for_health = engine;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Transcribe audio",
                "Audio to text with timestamped segments, via a Whisper-compatible backend",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can turn audio into text",
        // so a caller can depend on the capability rather than on this plugin.
        provides: [capability("transcribe.v1")],

        mcp: [
            // Projected as `transcribe.transcribe` on the host MCP endpoint.
            mcp::tool("transcribe")
                .title("Transcribe an audio file")
                .description(
                    "Turn one audio file into text, with timestamped segments so a specific \
                     moment can be quoted and seeked to. Reads only from the directories the \
                     operator configured — call `transcribe.list_audio` first to see which files \
                     exist and how to name them. Long WAV recordings are cut into overlapping \
                     chunks automatically and stitched back together with their timestamps \
                     corrected to absolute time. Requires the operator to have configured a \
                     Whisper-compatible backend, and returns a message naming the missing setting \
                     when they have not.",
                )
                .input::<TranscribeArgs>()
                .handle(move |args: TranscribeArgs, _context| {
                    let engine = Arc::clone(&for_transcribe);
                    Box::pin(async move {
                        let want_segments = args.want_segments();
                        engine
                            .transcribe(
                                &args.path,
                                args.language.as_deref(),
                                want_segments,
                                args.prompt.as_deref(),
                            )
                            .await
                    })
                }),

            mcp::tool("list_audio")
                .title("List available audio")
                .description(
                    "List the audio files this plugin is allowed to read, with their size and — \
                     for WAV — their duration. Start here: the `path` of each entry is exactly \
                     what `transcribe.transcribe` accepts, and no other path will resolve.",
                )
                .input::<ListAudioArgs>()
                .handle(move |args: ListAudioArgs, _context| {
                    let engine = Arc::clone(&for_list);
                    Box::pin(async move { engine.list_audio(args.root).await })
                }),

            // Cheap, local, and always answering: the tool to call when the
            // other three are failing.
            mcp::tool("status")
                .title("Show configuration")
                .description(
                    "Report how this plugin is configured — the backend URL and model, whether an \
                     API key is present, which audio roots exist and whether each is readable, and \
                     the chunking and size limits. Touches no network and no audio; use \
                     `transcribe.probe_backend` to find out whether the backend actually answers.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_status);
                    Box::pin(async move { Ok(engine.status()) })
                }),

            mcp::tool("probe_backend")
                .title("Check the backend answers")
                .description(
                    "Send a third of a second of generated silence to the configured \
                     transcription backend and report what came back. This exercises the whole \
                     request path — URL, authentication, model name, upload format, and reply \
                     parsing — so a success means a real transcription would work. Fails with the \
                     specific reason when it would not.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_probe);
                    Box::pin(async move { engine.probe_backend().await })
                }),
        ],

        // Health must stay fast and independent of long-running work, so it
        // reports configuration state and never makes a request. Transcribing
        // can take minutes; a health check must not.
        health: move |_context| {
            let engine = Arc::clone(&for_health);
            Box::pin(async move { Ok(engine.health()) })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::config::{Config, EnvMap};

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        let config = Config::parse(&[], &EnvMap::new()).expect("defaults parse");
        let engine = Engine::new(config).expect("engine builds");
        transcribe_plugin(engine)
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn the_manifest_declares_exactly_the_four_tools() {
        let manifest = manifest();

        let mut operations: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        operations.sort_unstable();
        assert_eq!(
            operations,
            ["list_audio", "probe_backend", "status", "transcribe"]
        );
    }

    #[test]
    fn every_tool_carries_a_description_a_model_can_act_on() {
        for operation in &manifest().operations {
            assert!(
                operation.description.len() > 40,
                "{} needs a real description",
                operation.name
            );

            let schema: serde_json::Value = serde_json::from_str(&operation.input_schema_json)
                .unwrap_or_else(|error| {
                    panic!("{}'s input schema must be JSON: {error}", operation.name)
                });
            assert_eq!(schema["type"], "object", "{}: {schema}", operation.name);
            // Every argument a caller may pass is described. `status` and
            // `probe_backend` take none, so they have no properties to describe
            // — and their schemas still refuse an argument that was passed by
            // mistake.
            assert_eq!(
                schema["additionalProperties"], false,
                "{} must refuse unknown arguments: {schema}",
                operation.name
            );
        }
    }

    #[test]
    fn every_argument_of_every_tool_is_described_for_the_model_that_reads_it() {
        for operation in &manifest().operations {
            let schema: serde_json::Value =
                serde_json::from_str(&operation.input_schema_json).expect("valid schema");
            let Some(properties) = schema["properties"].as_object() else {
                continue;
            };
            for (name, property) in properties {
                let description = property["description"].as_str().unwrap_or_default();
                assert!(
                    description.len() > 20,
                    "{}.{name} needs a description written for a stranger, got {description:?}",
                    operation.name
                );
            }
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = manifest();
        let transcribe = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "transcribe")
            .expect("transcribe is declared");

        let schema = &transcribe.input_schema_json;
        assert!(schema.contains("ISO-639-1"), "{schema}");
        assert!(schema.contains("list_audio"), "{schema}");
        assert!(schema.contains("\"required\""), "{schema}");
        // A misspelled argument must not be silently accepted.
        assert!(schema.contains("additionalProperties"), "{schema}");
    }

    #[test]
    fn no_other_surface_is_declared() {
        let manifest = manifest();

        assert!(
            manifest.http_bindings.is_empty(),
            "no HTTP routes are declared"
        );
        assert!(
            manifest.mesh_channels.is_empty(),
            "no mesh channels are declared"
        );
        assert!(
            manifest.mesh_event_subscriptions.is_empty(),
            "no mesh events are declared"
        );
        assert!(
            manifest.endpoints.is_empty(),
            "no external endpoints are declared"
        );
        assert!(manifest.web_ui.is_none(), "no web UI is declared");
        assert!(
            manifest.config_schema.is_none(),
            "settings cannot reach this process, so none are declared"
        );
        assert_eq!(manifest.capabilities, vec!["transcribe.v1".to_string()]);
    }

    #[test]
    fn segments_default_to_on_because_they_are_the_point_of_the_tool() {
        let args: TranscribeArgs =
            serde_json::from_value(serde_json::json!({"path": "audio/one.wav"}))
                .expect("minimal arguments");
        assert!(args.want_segments());

        let off: TranscribeArgs =
            serde_json::from_value(serde_json::json!({"path": "a.wav", "segments": false}))
                .expect("explicit off");
        assert!(!off.want_segments());
    }

    #[test]
    fn a_misspelled_argument_is_rejected_rather_than_ignored() {
        let error = serde_json::from_value::<TranscribeArgs>(
            serde_json::json!({"path": "a.wav", "langauge": "en"}),
        )
        .expect_err("a typo must not silently transcribe the wrong language");
        assert!(error.to_string().contains("langauge"), "{error}");
    }
}
