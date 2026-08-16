//! The whole contribution surface of `openai-endpoint` in one declaration.
//!
//! The load-bearing line is the `inference` block: one
//! `openai_http(endpoint_id, address)` entry is what actually attaches the
//! backend to this node. Everything else is diagnostics.
//!
//! This plugin is **not a proxy**. It never sees a chat request. The host reads
//! the declared address, opens its own connection to the backend, and relays
//! bytes in both directions — so a token stream reaches the client exactly as
//! the backend produced it, and no buffering can be introduced here. The MCP
//! tools below exist to answer the questions that byte relay cannot: is the
//! address routable, what does the backend actually serve, and does it stream.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks. Omitting a field is fine, reordering is not.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tdcc_plugin::{
    PluginError, PluginMetadata, SimplePlugin, inference, mcp, plugin, plugin_server_info,
};

use crate::config::{EndpointConfig, validate_model_name};
use crate::openai;
use crate::upstream::{DEFAULT_PROBE_MAX_TOKENS, MAX_PROBE_MAX_TOKENS, ProbeCache, Upstream};

pub const PLUGIN_NAME: &str = "openai-endpoint";
pub const PLUGIN_VERSION: &str = "0.1.0";

// ---------------------------------------------------------------------------
// Tool arguments
//
// Doc comments on these fields become the schema descriptions the host
// advertises in `tools/list`, so they are written for the person (or model)
// choosing arguments, not for a maintainer reading the source.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StreamCheckArgs {
    /// Model to generate with. Defaults to `--model` if configured, otherwise
    /// the first model the endpoint advertises.
    #[serde(default)]
    model: Option<String>,
    /// Completion tokens to request. Kept small on purpose: this runs on a
    /// contributor's hardware. Clamped to 1-128.
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Ask for token accounting on the stream via
    /// `stream_options.include_usage`. Turn this off for a backend that
    /// rejects unknown request fields.
    #[serde(default)]
    include_usage: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompatArgs {
    /// Model to generate with. Defaults to `--model` if configured, otherwise
    /// the first model the endpoint advertises.
    #[serde(default)]
    model: Option<String>,
    /// Completion tokens to request. Clamped to 1-128.
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Also send one request naming a model that cannot exist, to capture the
    /// error envelope this backend uses. Never reaches a GPU.
    #[serde(default)]
    check_error_shape: Option<bool>,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

pub fn openai_endpoint_plugin(
    config: EndpointConfig,
    cache: ProbeCache,
) -> anyhow::Result<SimplePlugin> {
    let endpoint_id = config.endpoint_id().to_string();
    let address = config.endpoint_address();
    let upstream = Arc::new(Upstream::new(config, cache.clone())?);

    let for_status = Arc::clone(&upstream);
    let for_models = Arc::clone(&upstream);
    let for_health = Arc::clone(&upstream);
    let for_stream = Arc::clone(&upstream);
    let for_compat = Arc::clone(&upstream);
    let for_startup = Arc::clone(&upstream);
    let for_health_hook = cache;

    Ok(plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "OpenAI endpoint",
                "Attaches an already-running OpenAI-compatible server to this node",
                None::<String>,
            ),
        ),

        mcp: [
            // Cheap and offline: what is configured, and what the host will do
            // with it. Safe to call when the backend is down.
            mcp::tool("status")
                .description(
                    "Report the attached endpoint's effective configuration, the exact URL the \
                     host health-checks, how request paths are rewritten onto it, and the last \
                     observation this plugin made. Makes no network request."
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let upstream = Arc::clone(&for_status);
                    Box::pin(async move { Ok(status_report(&upstream)) })
                }),

            mcp::tool("models")
                .description(
                    "Ask the endpoint what it actually serves by reading /v1/models, rather than \
                     trusting configuration. Reports the ids exactly as the host reads them \
                     (data[].id) and errors if the endpoint cannot be reached."
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let upstream = Arc::clone(&for_models);
                    Box::pin(async move {
                        let outcome = upstream
                            .discover_models()
                            .await
                            .map_err(|failure| failure.into_plugin_error("model discovery"))?;
                        Ok(json!({
                            "endpoint": upstream.config().endpoint_address(),
                            "discovery": outcome,
                        }))
                    })
                }),

            mcp::tool("health")
                .description(
                    "Reproduce the host's own endpoint health probe and report whether this \
                     backend will be routable. Repeats the probe with the configured API key so \
                     an auth-gated endpoint is diagnosed rather than guessed at."
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let upstream = Arc::clone(&for_health);
                    Box::pin(async move {
                        let outcome = upstream.check_health().await;
                        Ok(json!({
                            "endpoint": upstream.config().endpoint_address(),
                            "routable": outcome.host_equivalent.ok,
                            "verdict": routability_verdict(&outcome),
                            "probe": outcome,
                        }))
                    })
                }),

            mcp::tool("verify_stream")
                .description(
                    "Send one small streaming chat completion and confirm tokens arrive \
                     progressively instead of in a single buffered blob. Reports the verdict, the \
                     number of network reads the body arrived in, timing to first token, and the \
                     finish reason and usage the backend emitted."
                )
                .input::<StreamCheckArgs>()
                .handle(move |args: StreamCheckArgs, _context| {
                    let upstream = Arc::clone(&for_stream);
                    Box::pin(async move {
                        let model = resolve_model(&upstream, args.model).await?;
                        let outcome = upstream
                            .verify_stream(
                                &model,
                                clamp_max_tokens(args.max_tokens),
                                args.include_usage.unwrap_or(true),
                            )
                            .await
                            .map_err(|failure| failure.into_plugin_error("streaming check"))?;
                        Ok(json!({
                            "endpoint": upstream.config().endpoint_address(),
                            "streaming_ok": matches!(
                                outcome.verdict,
                                openai::StreamVerdict::Incremental
                            ),
                            "explanation": stream_explanation(&outcome.verdict),
                            "observed": outcome,
                        }))
                    })
                }),

            mcp::tool("compat")
                .description(
                    "Send one non-streaming chat completion and report where this backend \
                     diverges from OpenAI: whether it accounts usage, whether its finish reason \
                     is an OpenAI value, and — optionally — the error envelope it returns and \
                     whether the host rewrites it."
                )
                .input::<CompatArgs>()
                .handle(move |args: CompatArgs, _context| {
                    let upstream = Arc::clone(&for_compat);
                    Box::pin(async move {
                        let model = resolve_model(&upstream, args.model).await?;
                        let completion = upstream
                            .probe_completion(&model, clamp_max_tokens(args.max_tokens))
                            .await
                            .map_err(|failure| failure.into_plugin_error("completion check"))?;

                        let error_shape = if args.check_error_shape.unwrap_or(false) {
                            Some(
                                upstream
                                    .probe_error_shape()
                                    .await
                                    .map_err(|failure| {
                                        failure.into_plugin_error("error-shape check")
                                    })?,
                            )
                        } else {
                            None
                        };

                        Ok(json!({
                            "endpoint": upstream.config().endpoint_address(),
                            "completion": completion,
                            "error_shape": error_shape,
                            "notes": compat_notes(&completion, error_shape.as_ref()),
                        }))
                    })
                }),
        ],

        // The one declaration that attaches the backend. `managed_by_plugin`
        // is false because this plugin did not start the server and must not
        // stop it: the machine's owner runs it, and TDCC only routes to it.
        inference: [
            inference::openai_http(endpoint_id, address)
                .managed_by_plugin(false)
                .supports_streaming(true),
        ],

        // Health must stay fast and independent of long-running work, so it
        // reads the cached observation instead of making a request. Endpoint
        // liveness is a separate concern the host probes on its own schedule —
        // this endpoint can go unhealthy and drop out of routing while the
        // plugin process stays perfectly healthy.
        health: move |_context| {
            let cache = for_health_hook.clone();
            Box::pin(async move { Ok(health_summary(&cache)) })
        },

        // One discovery probe at startup, spawned rather than awaited: the
        // initialize handshake has a timeout, and a slow or dead backend must
        // not stop the plugin from coming up.
        on_initialized: move |_context| {
            let upstream = Arc::clone(&for_startup);
            Box::pin(async move {
                tokio::spawn(async move {
                    let health = upstream.check_health().await;
                    if health.host_equivalent.ok {
                        eprintln!(
                            "[{PLUGIN_NAME}] endpoint ready: {} ({} model(s))",
                            health.probe_url, health.host_equivalent.models
                        );
                    } else {
                        eprintln!(
                            "[{PLUGIN_NAME}] endpoint not routable yet: {} ({})",
                            health.probe_url, health.host_equivalent.detail
                        );
                    }
                });
                Ok(())
            })
        },
    })
}

// ---------------------------------------------------------------------------
// Report builders — pure enough to test
// ---------------------------------------------------------------------------

fn status_report(upstream: &Upstream) -> serde_json::Value {
    let config = upstream.config();
    let base_path = config.base_path();
    let last = upstream.cache().last();

    json!({
        "endpoint_id": config.endpoint_id(),
        "address": config.endpoint_address(),
        "url_source": config.url_source(),
        "timeout_secs": config.timeout().as_secs(),
        "default_model": config.default_model(),
        // The name only. The value is never returned, logged, or packaged.
        "api_key_env": config.api_key_env(),
        "api_key_resolved": config.api_key().is_some(),
        "host_health_probe_url": openai::models_probe_url(config.base_url()).to_string(),
        "request_path_mapping": {
            "/v1/chat/completions": openai::forward_path(&base_path, "/v1/chat/completions"),
            "/v1/models": openai::forward_path(&base_path, "/v1/models"),
        },
        "data_plane": "host-proxied; this plugin is not on the request path",
        "last_observation": last.map(|(summary, age)| json!({
            "summary": summary,
            "age_secs": age.as_secs(),
        })),
    })
}

fn health_summary(cache: &ProbeCache) -> String {
    match cache.last() {
        Some((summary, age)) => format!("ok; {summary} ({}s ago)", age.as_secs()),
        None => "ok; no endpoint observation yet".to_string(),
    }
}

/// Plain-language reading of the host-equivalent probe, including the trap that
/// an authenticated endpoint falls into.
pub fn routability_verdict(outcome: &crate::upstream::HealthOutcome) -> String {
    if outcome.host_equivalent.ok {
        return format!(
            "routable: the host's unauthenticated probe succeeded and found {} model(s)",
            outcome.host_equivalent.models
        );
    }
    match &outcome.authenticated {
        Some(authenticated) if authenticated.ok => format!(
            "NOT routable: the endpoint answers only with an API key, but the host's endpoint \
             health probe is unauthenticated and cannot send one. It will keep failing, so this \
             endpoint never becomes routable. Put an unauthenticated local listener in front of \
             it, or disable auth on the models route. (unauthenticated: {}; authenticated: {})",
            outcome.host_equivalent.detail, authenticated.detail
        ),
        _ => format!(
            "NOT routable: {}. The host retries every 15 seconds and the endpoint becomes \
             routable again on its own once this succeeds.",
            outcome.host_equivalent.detail
        ),
    }
}

fn stream_explanation(verdict: &openai::StreamVerdict) -> &'static str {
    match verdict {
        openai::StreamVerdict::Incremental => {
            "tokens arrived progressively across several network reads; the chat surface will \
             stream normally"
        }
        openai::StreamVerdict::Buffered => {
            "every event arrived in one read: the backend, or something in front of it, buffered \
             the whole response. Clients will see a long pause and then the entire answer at \
             once. Check for a reverse proxy with response buffering enabled between here and the \
             backend"
        }
        openai::StreamVerdict::SingleEvent => {
            "only one event arrived, which is too short to tell streaming from buffering; run \
             again with a larger max_tokens"
        }
        openai::StreamVerdict::NoEvents => {
            "no server-sent events arrived; the backend answered but did not produce a stream"
        }
    }
}

fn compat_notes(
    completion: &crate::upstream::CompletionOutcome,
    error_shape: Option<&openai::NormalizedError>,
) -> Vec<String> {
    let mut notes = Vec::new();

    if !completion.usage.present {
        notes.push(
            "no usage object: this backend does not account tokens, and TDCC relays responses \
             byte for byte, so clients will see none either"
                .to_string(),
        );
    } else if !completion.usage.complete() {
        notes.push("usage is present but incomplete; some token counters are missing".to_string());
    }
    if completion.usage.total_derived {
        notes.push(
            "total_tokens was absent and has been added up here; the client still receives the \
             response without it"
                .to_string(),
        );
    }
    if !completion.usage.alternate_keys.is_empty() {
        notes.push(format!(
            "non-OpenAI usage keys in use: {}",
            completion.usage.alternate_keys.join(", ")
        ));
    }

    match &completion.finish_reason {
        None => notes.push("no finish_reason on the completion".to_string()),
        Some(reason) if !reason.canonical => notes.push(format!(
            "finish_reason '{}' is not an OpenAI value (closest: '{}'); it is relayed unchanged, \
             so a strict client may not recognise it",
            reason.raw, reason.normalized
        )),
        Some(_) => {}
    }

    if let Some(error) = error_shape {
        notes.push(if error.rewritten_by_host {
            format!(
                "error bodies use the '{}' shape; the host rewrites non-2xx bodies into OpenAI's \
                 error envelope, so clients see a normalised error",
                error.shape.label()
            )
        } else {
            format!(
                "error bodies already use OpenAI's '{}' shape and pass through unchanged",
                error.shape.label()
            )
        });
    }

    if notes.is_empty() {
        notes.push("no divergence from OpenAI's response shape observed".to_string());
    }
    notes
}

// ---------------------------------------------------------------------------
// Argument handling
// ---------------------------------------------------------------------------

fn clamp_max_tokens(requested: Option<u32>) -> u32 {
    requested
        .unwrap_or(DEFAULT_PROBE_MAX_TOKENS)
        .clamp(1, MAX_PROBE_MAX_TOKENS)
}

/// Pick the model to generate with: caller's choice, then `--model`, then
/// whatever the endpoint advertises first.
///
/// Falling back to discovery rather than to a hard-coded name is deliberate —
/// a guessed model produces a confusing 404 from the backend instead of a clear
/// answer here.
async fn resolve_model(
    upstream: &Upstream,
    requested: Option<String>,
) -> Result<String, PluginError> {
    if let Some(requested) = requested {
        return validate_model_name(&requested)
            .map_err(|error| PluginError::invalid_params(error.to_string()));
    }
    if let Some(configured) = upstream.config().default_model() {
        return Ok(configured.to_string());
    }

    let discovery = upstream
        .discover_models()
        .await
        .map_err(|failure| failure.into_plugin_error("model discovery"))?;
    discovery.report.ids.first().cloned().ok_or_else(|| {
        PluginError::invalid_params(format!(
            "no model to probe with: {} advertises no models under data[].id. Pass `model`, or \
             configure `--model <name>`.",
            discovery.url
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{CompletionOutcome, HealthOutcome, ProbeAttempt};
    use tdcc_plugin::Plugin;

    fn config() -> EndpointConfig {
        EndpointConfig::from_launch(
            ["--endpoint-id", "vllm"].map(String::from),
            Some("http://127.0.0.1:8000/v1".to_string()),
        )
        .expect("valid configuration")
    }

    fn attempt(ok: bool, models: usize, detail: &str) -> ProbeAttempt {
        ProbeAttempt {
            ok,
            status: Some(if ok { 200 } else { 401 }),
            elapsed_ms: 1,
            models,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn the_manifest_declares_exactly_one_streaming_inference_endpoint() {
        let plugin = openai_endpoint_plugin(config(), ProbeCache::default()).expect("builds");
        let manifest = plugin
            .manifest()
            .expect("declarative plugins have a manifest");

        let [endpoint] = manifest.endpoints.as_slice() else {
            panic!("exactly one endpoint should be declared");
        };
        assert_eq!(endpoint.endpoint_id, "vllm");
        assert_eq!(
            endpoint.address.as_deref(),
            Some("http://127.0.0.1:8000/v1")
        );
        assert_eq!(endpoint.protocol.as_deref(), Some("openai_compatible"));
        assert!(endpoint.supports_streaming);
        // The machine's owner runs the server; TDCC must never stop it.
        assert!(!endpoint.managed_by_plugin);
    }

    #[test]
    fn the_manifest_declares_no_settings_or_web_ui_so_the_package_manifest_is_empty() {
        let plugin = openai_endpoint_plugin(config(), ProbeCache::default()).expect("builds");
        let manifest = plugin.manifest().expect("manifest");
        // Settings are host-owned and never delivered to the process, so a
        // config schema here would render controls this plugin cannot read.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
    }

    #[test]
    fn every_declared_tool_is_present_and_described() {
        let plugin = openai_endpoint_plugin(config(), ProbeCache::default()).expect("builds");
        let manifest = plugin.manifest().expect("manifest");
        let names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        for expected in ["status", "models", "health", "verify_stream", "compat"] {
            assert!(
                names.contains(&expected),
                "missing tool '{expected}' in {names:?}"
            );
        }
        assert!(
            manifest
                .operations
                .iter()
                .all(|operation| !operation.description.is_empty()),
            "every tool description is shown to models and users"
        );
    }

    #[test]
    fn status_reports_the_host_probe_url_and_path_mapping_without_the_key() {
        let config = EndpointConfig::from_launch(
            ["--api-key-env", "OPENAI_ENDPOINT_TEST_KEY"].map(String::from),
            Some("http://127.0.0.1:8000/api/v1".to_string()),
        )
        .expect("valid");
        let upstream = Upstream::new(config, ProbeCache::default()).expect("client builds");

        let report = status_report(&upstream);
        assert_eq!(
            report["host_health_probe_url"],
            "http://127.0.0.1:8000/api/v1/models"
        );
        assert_eq!(
            report["request_path_mapping"]["/v1/chat/completions"],
            "/api/v1/chat/completions"
        );
        // The variable name is reported; the value never is.
        assert_eq!(report["api_key_env"], "OPENAI_ENDPOINT_TEST_KEY");
        assert!(report["last_observation"].is_null());
    }

    #[test]
    fn health_summary_survives_having_no_observation_yet() {
        let cache = ProbeCache::default();
        assert!(health_summary(&cache).starts_with("ok;"));
        cache.record("probe ok");
        assert!(health_summary(&cache).contains("probe ok"));
    }

    #[test]
    fn an_auth_gated_endpoint_is_diagnosed_rather_than_called_unhealthy() {
        let outcome = HealthOutcome {
            probe_url: "http://127.0.0.1:8000/v1/models".into(),
            host_equivalent: attempt(false, 0, "GET /v1/models -> 401"),
            authenticated: Some(attempt(true, 3, "GET /v1/models -> 200")),
        };
        let verdict = routability_verdict(&outcome);
        assert!(verdict.starts_with("NOT routable"));
        assert!(verdict.contains("unauthenticated"), "{verdict}");
        assert!(verdict.contains("never becomes routable"), "{verdict}");
    }

    #[test]
    fn a_reachable_endpoint_is_reported_as_routable() {
        let outcome = HealthOutcome {
            probe_url: "http://127.0.0.1:8000/v1/models".into(),
            host_equivalent: attempt(true, 2, "GET /v1/models -> 200"),
            authenticated: None,
        };
        assert!(routability_verdict(&outcome).starts_with("routable"));
    }

    #[test]
    fn a_plainly_down_endpoint_says_it_recovers_on_its_own() {
        let outcome = HealthOutcome {
            probe_url: "http://127.0.0.1:8000/v1/models".into(),
            host_equivalent: attempt(false, 0, "connection refused"),
            authenticated: None,
        };
        let verdict = routability_verdict(&outcome);
        assert!(verdict.contains("connection refused"));
        assert!(verdict.contains("routable again on its own"));
    }

    #[test]
    fn the_buffered_verdict_explains_what_a_user_will_see() {
        let explanation = stream_explanation(&openai::StreamVerdict::Buffered);
        assert!(explanation.contains("buffered"));
        assert!(explanation.contains("entire answer at once"));
        assert!(stream_explanation(&openai::StreamVerdict::Incremental).contains("progressively"));
    }

    #[test]
    fn compat_notes_call_out_missing_usage_and_dialect_finish_reasons() {
        let completion = CompletionOutcome {
            model: "m".into(),
            status: 200,
            elapsed_ms: 5,
            content_chars: 2,
            finish_reason: Some(openai::normalize_finish_reason("eos_token")),
            usage: openai::normalize_usage(None),
        };
        let notes = compat_notes(&completion, None);
        assert!(notes.iter().any(|note| note.contains("no usage object")));
        assert!(notes.iter().any(|note| note.contains("eos_token")));
    }

    #[test]
    fn compat_notes_report_when_the_host_rewrites_the_error_envelope() {
        let completion = CompletionOutcome {
            model: "m".into(),
            status: 200,
            elapsed_ms: 5,
            content_chars: 2,
            finish_reason: Some(openai::normalize_finish_reason("stop")),
            usage: openai::normalize_usage(Some(&json!({
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }))),
        };

        let rewritten = openai::normalize_error(404, r#"{"detail":"not found"}"#);
        let notes = compat_notes(&completion, Some(&rewritten));
        assert!(notes.iter().any(|note| note.contains("host rewrites")));

        let passthrough = openai::normalize_error(
            404,
            r#"{"error":{"message":"no","type":"invalid_request_error"}}"#,
        );
        let notes = compat_notes(&completion, Some(&passthrough));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("pass through unchanged"))
        );
    }

    #[test]
    fn compat_notes_are_never_empty() {
        let completion = CompletionOutcome {
            model: "m".into(),
            status: 200,
            elapsed_ms: 5,
            content_chars: 2,
            finish_reason: Some(openai::normalize_finish_reason("stop")),
            usage: openai::normalize_usage(Some(&json!({
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }))),
        };
        assert_eq!(
            compat_notes(&completion, None),
            vec!["no divergence from OpenAI's response shape observed".to_string()]
        );
    }

    #[test]
    fn probe_token_budgets_are_clamped_in_both_directions() {
        assert_eq!(clamp_max_tokens(None), DEFAULT_PROBE_MAX_TOKENS);
        assert_eq!(clamp_max_tokens(Some(0)), 1);
        assert_eq!(clamp_max_tokens(Some(u32::MAX)), MAX_PROBE_MAX_TOKENS);
        assert_eq!(clamp_max_tokens(Some(8)), 8);
    }

    #[tokio::test]
    async fn a_caller_supplied_model_is_validated_before_any_request() {
        // Port 1 has nothing listening: if validation did not happen first,
        // this would fail as a transport error instead of invalid params.
        let config = EndpointConfig::from_launch(
            Vec::<String>::new(),
            Some("http://127.0.0.1:1/v1".to_string()),
        )
        .expect("valid");
        let upstream = Upstream::new(config, ProbeCache::default()).expect("client builds");

        let error = resolve_model(&upstream, Some("bad\nmodel".into()))
            .await
            .expect_err("control characters are rejected");
        assert!(
            error.message.contains("control characters"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_configured_default_model_avoids_a_discovery_request() {
        let config = EndpointConfig::from_launch(
            ["--model", "qwen3-8b"].map(String::from),
            Some("http://127.0.0.1:1/v1".to_string()),
        )
        .expect("valid");
        let upstream = Upstream::new(config, ProbeCache::default()).expect("client builds");

        assert_eq!(
            resolve_model(&upstream, None)
                .await
                .expect("configured default is used"),
            "qwen3-8b"
        );
    }
}
