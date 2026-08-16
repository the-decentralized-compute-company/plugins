//! The whole contribution surface of `describe-image` in one declaration.
//!
//! Five MCP tools, the same five operations mounted over HTTP, one capability,
//! and a health hook. The host synthesizes `tools/list`, `tools/call`, the JSON
//! Schema for every argument, and the request validation that runs before a
//! handler is entered — this plugin opens no socket and speaks no MCP.
//!
//! There is deliberately **no** `config_schema` and no `web_ui`.
//! `[plugin.settings]` never reaches a plugin process, so a schema here would
//! render console controls whose values could not affect a single request. See
//! `config.rs` for where settings actually come from.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, http, mcp, plugin, plugin_server_info,
};

use crate::config::{PLUGIN_NAME, PLUGIN_VERSION};
use crate::engine::{Engine, Task};

/// Arguments for the `describe` tool.
///
/// Every doc comment in this struct — and in the three below it — becomes a
/// `description` in the JSON Schema the host advertises, so a model reads these
/// words when it decides how to call the tool. They are written for that
/// audience: what to pass, what happens to it, and what will be refused.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeArgs {
    /// The images to look at, in order. Each entry is one of: a path to a file
    /// inside a directory the operator configured (relative to it, or the full
    /// path); a `data:image/png;base64,...` URI with the bytes inline; or an
    /// http/https URL, if the operator turned remote fetching on. PNG, JPEG,
    /// GIF, WebP, BMP, and TIFF are accepted. Large images are downscaled
    /// before they are sent.
    pub images: Vec<String>,

    /// What to pay particular attention to, in plain words — "the text on the
    /// sign", "whether the door is open", "the wiring in the top left". Omit it
    /// for a general description.
    #[serde(default)]
    pub focus: Option<String>,

    /// Use this exact model id instead of the vision-capable one this plugin
    /// would pick. Must be a model the endpoint is currently serving; call
    /// `describe-image.vision_models` to see the list.
    #[serde(default)]
    pub model: Option<String>,

    /// Cap the length of the answer, in tokens. Clamped to the operator's
    /// configured ceiling; omit it to use that ceiling.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments for the `ask` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    /// The images to look at, in order. Same forms as `describe`: a path inside
    /// a configured directory, a `data:image/...;base64,...` URI, or an
    /// http/https URL when the operator allowed it.
    pub images: Vec<String>,

    /// What you want to know about the image, as a plain question. The model is
    /// told to answer only from what is visible and to say so when the image
    /// does not settle the question.
    pub question: String,

    /// Use this exact model id instead of the one this plugin would pick. Must
    /// be a model the endpoint is currently serving.
    #[serde(default)]
    pub model: Option<String>,

    /// Cap the length of the answer, in tokens. Clamped to the operator's
    /// configured ceiling.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments for the `read_text` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadTextArgs {
    /// The images to transcribe, in order. Same forms as `describe`. Screenshots
    /// and scans work best; a photograph of small print often does not.
    pub images: Vec<String>,

    /// Use this exact model id instead of the one this plugin would pick. Must
    /// be a model the endpoint is currently serving.
    #[serde(default)]
    pub model: Option<String>,

    /// Cap the length of the transcription, in tokens. A dense page of text
    /// needs more than the default; raise it if the result stops mid-sentence
    /// with `finish_reason` `length`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments for `status` and `vision_models` — neither takes any.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

pub fn describe_image_plugin(engine: Arc<Engine>) -> SimplePlugin {
    // One clone per handler closure; they all share the single HTTP client
    // inside the engine.
    let for_describe_tool = Arc::clone(&engine);
    let for_ask_tool = Arc::clone(&engine);
    let for_read_text_tool = Arc::clone(&engine);
    let for_status_tool = Arc::clone(&engine);
    let for_models_tool = Arc::clone(&engine);
    let for_describe_route = Arc::clone(&engine);
    let for_ask_route = Arc::clone(&engine);
    let for_read_text_route = Arc::clone(&engine);
    let for_status_route = Arc::clone(&engine);
    let for_models_route = Arc::clone(&engine);
    let for_health = Arc::clone(&engine);

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Describe image",
                "Describe, question, and transcribe images using a vision model on the mesh",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can look at a picture", so
        // a caller can depend on the capability rather than on this plugin's id.
        provides: [capability("describe-image.v1")],

        mcp: [
            // Projected as `describe-image.describe` on the host MCP endpoint.
            mcp::tool("describe")
                .title("Describe an image")
                .description(
                    "Look at one or more images and describe what is in them, using a \
                     vision-capable model served on this mesh. Accepts a local file path inside a \
                     directory the operator configured, an inline `data:image/...;base64,...` \
                     URI, or an http/https URL when the operator allowed remote fetching. Images \
                     are downscaled before they are sent. The description comes from a language \
                     model and can be wrong — counts, colours, and any text it reports are not \
                     measurements.",
                )
                .input::<DescribeArgs>()
                .handle(move |args: DescribeArgs, _context| {
                    let engine = Arc::clone(&for_describe_tool);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::Describe,
                                &args.images,
                                args.focus.as_deref(),
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // Projected as `describe-image.ask`.
            mcp::tool("ask")
                .title("Ask a question about an image")
                .description(
                    "Answer one question about one or more images, using a vision-capable model \
                     served on this mesh. Use this instead of `describe-image.describe` when you \
                     already know what you need — \"what is the error code on this screen\", \
                     \"how many people are at the table\", \"is the light on\". The model is \
                     instructed to answer only from what is visible and to say when the image \
                     does not settle the question, but it can still be confidently wrong.",
                )
                .input::<AskArgs>()
                .handle(move |args: AskArgs, _context| {
                    let engine = Arc::clone(&for_ask_tool);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::Ask,
                                &args.images,
                                Some(args.question.as_str()),
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // Projected as `describe-image.read_text`.
            mcp::tool("read_text")
                .title("Read the text in an image")
                .description(
                    "Transcribe the text visible in one or more images, preserving reading order \
                     and line breaks. This is a vision language model reading the picture, not an \
                     OCR engine: it can misread characters, drop lines, and occasionally invent \
                     plausible-looking words, so do not rely on it where the exact characters \
                     matter. `no_text_found` is true when the model reported no legible text at \
                     all, which is different from it failing to answer.",
                )
                .input::<ReadTextArgs>()
                .handle(move |args: ReadTextArgs, _context| {
                    let engine = Arc::clone(&for_read_text_tool);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::ReadText,
                                &args.images,
                                None,
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // Projected as `describe-image.status`. Cheap, local, and the first
            // thing to call when the other tools are failing.
            mcp::tool("status")
                .title("Show how this plugin is configured")
                .description(
                    "Report how describe-image is configured — which endpoint it sends images to, \
                     whether a model is pinned, which directories local paths may come from, and \
                     every size limit. Makes no network request, so it answers even when the \
                     inference endpoint is down.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_status_tool);
                    Box::pin(async move { Ok(engine.status()) })
                }),

            // Projected as `describe-image.vision_models`.
            mcp::tool("vision_models")
                .title("List the models that can see")
                .description(
                    "Ask the configured endpoint which models it is serving and report which of \
                     them can accept an image, which one this plugin would use, and how sure it \
                     is. Call this when a describe or ask call reports that no vision model is \
                     available. Unlike `describe-image.status` this makes a network request, so \
                     it also tells you whether the endpoint is reachable at all.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_models_tool);
                    Box::pin(async move { engine.vision_models().await })
                }),
        ],

        http: [
            // POST /api/plugins/describe-image/http/describe
            // The image-carrying operations are POST because their input is a
            // list of references, one of which may be a megabyte-long data URI.
            http::post("/describe")
                .description("Describe one or more images.")
                .input::<DescribeArgs>()
                .handle(move |args: DescribeArgs, _context| {
                    let engine = Arc::clone(&for_describe_route);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::Describe,
                                &args.images,
                                args.focus.as_deref(),
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // POST /api/plugins/describe-image/http/ask
            http::post("/ask")
                .description("Answer a question about one or more images.")
                .input::<AskArgs>()
                .handle(move |args: AskArgs, _context| {
                    let engine = Arc::clone(&for_ask_route);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::Ask,
                                &args.images,
                                Some(args.question.as_str()),
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // POST /api/plugins/describe-image/http/read_text
            http::post("/read_text")
                .description("Transcribe the text visible in one or more images.")
                .input::<ReadTextArgs>()
                .handle(move |args: ReadTextArgs, _context| {
                    let engine = Arc::clone(&for_read_text_route);
                    Box::pin(async move {
                        engine
                            .run(
                                Task::ReadText,
                                &args.images,
                                None,
                                args.model.as_deref(),
                                args.max_tokens,
                            )
                            .await
                    })
                }),

            // GET /api/plugins/describe-image/http/status
            http::get("/status")
                .description("Report how this plugin is configured.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_status_route);
                    Box::pin(async move { Ok(engine.status()) })
                }),

            // GET /api/plugins/describe-image/http/vision_models
            http::get("/vision_models")
                .description("List the endpoint's models and which of them can see.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_models_route);
                    Box::pin(async move { engine.vision_models().await })
                }),
        ],

        // Health must stay fast and independent of long-running work, so it
        // reports configuration state and never makes a request.
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

    use crate::config::Config;

    const TOOLS: &[&str] = &["describe", "ask", "read_text", "status", "vision_models"];

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        let config = Config::parse(&[], &Default::default()).expect("defaults parse");
        let engine = Engine::new(config).expect("client builds");
        describe_image_plugin(engine)
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    fn operation(
        manifest: &tdcc_plugin::proto::PluginManifest,
        name: &str,
    ) -> tdcc_plugin::proto::OperationManifest {
        manifest
            .operations
            .iter()
            .find(|operation| operation.name == name)
            .unwrap_or_else(|| panic!("`{name}` is declared"))
            .clone()
    }

    #[test]
    fn every_tool_is_declared_with_a_description_and_a_schema() {
        let manifest = manifest();

        for name in TOOLS {
            let operation = operation(&manifest, name);
            assert!(
                operation.description.len() > 60,
                "`{name}` needs a description a model can act on"
            );
            let schema: serde_json::Value = serde_json::from_str(&operation.input_schema_json)
                .unwrap_or_else(|error| panic!("`{name}` has a JSON schema: {error}"));
            assert_eq!(schema["type"], "object", "`{name}`: {schema}");
            // The two argument-free tools have no `properties` at all, which is
            // the correct schema for "takes nothing" rather than an omission.
            if ["describe", "ask", "read_text"].contains(name) {
                assert!(
                    schema["properties"]["images"].is_object(),
                    "`{name}`: {schema}"
                );
            } else {
                assert!(schema.get("properties").is_none(), "`{name}`: {schema}");
            }
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = manifest();
        let schema = operation(&manifest, "describe").input_schema_json;

        assert!(schema.contains("data:image/png;base64"), "{schema}");
        assert!(schema.contains("downscaled"), "{schema}");
        assert!(schema.contains("\"required\""), "{schema}");

        let schema = operation(&manifest, "ask").input_schema_json;
        assert!(schema.contains("only from what is visible"), "{schema}");
    }

    #[test]
    fn the_descriptions_say_the_answer_can_be_wrong() {
        let manifest = manifest();

        assert!(
            operation(&manifest, "describe")
                .description
                .contains("can be wrong"),
            "a model reading the tool list has to know this before it calls"
        );
        assert!(
            operation(&manifest, "read_text")
                .description
                .contains("not an OCR engine")
        );
    }

    #[test]
    fn the_image_carrying_routes_are_post_and_the_diagnostics_are_get() {
        let manifest = manifest();

        let method_for = |path: &str| {
            manifest
                .http_bindings
                .iter()
                .find(|binding| binding.path == path)
                .unwrap_or_else(|| panic!("`{path}` is bound"))
                .method
        };

        let post = tdcc_plugin::proto::HttpMethod::Post as i32;
        let get = tdcc_plugin::proto::HttpMethod::Get as i32;
        for path in ["/describe", "/ask", "/read_text"] {
            assert_eq!(method_for(path), post, "{path} must be POST");
        }
        for path in ["/status", "/vision_models"] {
            assert_eq!(method_for(path), get, "{path} must be GET");
        }
    }

    #[test]
    fn the_http_routes_mirror_the_tools_one_for_one() {
        let manifest = manifest();

        let mut paths: Vec<String> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.trim_start_matches('/').to_string())
            .collect();
        paths.sort();
        let mut expected: Vec<String> = TOOLS.iter().map(|name| (*name).to_string()).collect();
        expected.sort();

        assert_eq!(paths, expected);
    }

    #[test]
    fn no_config_schema_or_web_ui_is_declared_and_the_capability_is_versioned() {
        let manifest = manifest();

        // Both would be dishonest surfaces for this plugin: settings never
        // reach the process, and there is no bundle to serve.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert_eq!(manifest.capabilities, vec!["describe-image.v1".to_string()]);
    }

    #[test]
    fn no_mesh_channel_or_event_subscription_is_declared() {
        let manifest = manifest();

        // Delivery is allowlist-based, so declaring nothing means receiving
        // nothing. This plugin has no reason to hear from peers.
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_ignored() {
        // `deny_unknown_fields` is what makes a mistyped `image` (singular)
        // fail loudly instead of running against an empty list.
        let error = serde_json::from_value::<DescribeArgs>(serde_json::json!({
            "images": ["data:image/png;base64,AA"],
            "image": "extra.png"
        }))
        .expect_err("an unknown field is an error");
        assert!(error.to_string().contains("image"), "{error}");
    }

    #[test]
    fn the_minimal_arguments_deserialize() {
        let args: DescribeArgs = serde_json::from_value(serde_json::json!({
            "images": ["photo.png"]
        }))
        .expect("images alone is enough");
        assert_eq!(args.images, vec!["photo.png".to_string()]);
        assert!(args.focus.is_none());
        assert!(args.model.is_none());
        assert!(args.max_tokens.is_none());

        let args: AskArgs = serde_json::from_value(serde_json::json!({
            "images": ["photo.png"],
            "question": "what is this?"
        }))
        .expect("images and a question");
        assert_eq!(args.question, "what is this?");

        serde_json::from_value::<NoArgs>(serde_json::json!({})).expect("no arguments");
    }

    #[test]
    fn ask_requires_a_question_at_the_schema_level() {
        let manifest = manifest();
        let schema = operation(&manifest, "ask").input_schema_json;

        let parsed: serde_json::Value = serde_json::from_str(&schema).expect("the schema is JSON");
        let required: Vec<&str> = parsed["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert!(required.contains(&"images"), "{schema}");
        assert!(required.contains(&"question"), "{schema}");
    }
}
