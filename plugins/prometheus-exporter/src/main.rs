//! `prometheus-exporter` — publishes this node's state in Prometheus text
//! exposition format so an existing monitoring stack can scrape it.
//!
//! The whole plugin is one read and one render. Every scrape does a single
//! `GET /api/status` against the local node API and turns the result into
//! metrics; nothing is cached, nothing is pushed, and no state is kept beyond
//! the exporter's own collection counters.
//!
//! ## Surfaces
//!
//! | Surface | Where | What it is for |
//! | --- | --- | --- |
//! | `GET /api/plugins/prometheus-exporter/http/metrics` | host HTTP | the scrape target |
//! | `prometheus-exporter.metrics` | host MCP | the same exposition, as JSON |
//! | `prometheus-exporter.check` | host MCP | "did I wire this up right?" |
//!
//! The scrape target is a **streamed** binding, because a buffered one would be
//! served as `application/json` and Prometheus cannot read that. See `serve`
//! for why that changes how the host treats the route.
//!
//! ## No config schema, on purpose
//!
//! `[plugin.settings]` never reaches a plugin process — the host stores those
//! values and the console renders them, but nothing delivers them here.
//! Declaring a settings schema would therefore put knobs in the console that
//! silently do nothing, so this plugin takes its configuration from
//! `[[plugin]].url` and `[[plugin]].args` instead, and ships no schema. That is
//! also why `plugin-manifest.json` is `{}` and can be left out of the archive.

mod collector;
mod node;
mod render;
mod serve;
mod settings;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    Plugin, PluginError, PluginMetadata, PluginRuntime, SimplePlugin, capability, http, mcp,
    package_manifest_json, plugin, plugin_server_info,
};

use crate::collector::Collector;
use crate::serve::METRICS_BINDING_ID;

/// Must match `plugin.toml`, the crate name, the archive directory, and
/// `[[plugin]].name` in `config.toml`. The host rejects the handshake if the
/// manifest id and the configured name disagree.
pub const PLUGIN_NAME: &str = "prometheus-exporter";
pub const EXPORTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Neither surface takes arguments. The host still synthesises a schema from
/// this type and validates against it, which is what stops a caller smuggling
/// unexpected fields into a handler.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

fn build_plugin(collector: Arc<Collector>) -> SimplePlugin {
    let for_metrics = Arc::clone(&collector);
    let for_check = Arc::clone(&collector);
    let for_health = Arc::clone(&collector);
    let for_streams = Arc::clone(&collector);

    let plugin = plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            EXPORTER_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                EXPORTER_VERSION,
                "Prometheus exporter",
                "Publishes node, model, request and peer metrics in Prometheus text exposition format",
                None::<String>,
            ),
        ),

        // A name another component can depend on instead of on this plugin's
        // id, in case a second exporter implementation ever replaces it.
        provides: [capability("metrics.prometheus.v1")],

        mcp: [
            mcp::tool("check")
                .description(
                    "Check that the exporter can read this node's state, and report the scrape \
                     URL, the collection latency, and how many series the last scrape produced. \
                     Fails loudly when the node API cannot be reached.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let collector = Arc::clone(&for_check);
                    Box::pin(async move {
                        let scrape = collector.scrape().await;
                        let settings = collector.settings();
                        // A tool that cannot reach its backend returns an
                        // error. The scrape endpoint deliberately does the
                        // opposite and reports tdcc_up 0, because Prometheus
                        // needs a parseable body to alert on.
                        if let Some(error) = scrape.error {
                            return Err(PluginError::internal(format!(
                                "cannot read node state: {error}"
                            )));
                        }
                        Ok(serde_json::json!({
                            "up": scrape.up,
                            "node_api": settings.node.base_url(),
                            "scrape_url": format!(
                                "{}/api/plugins/{PLUGIN_NAME}/http/metrics",
                                settings.node.base_url()
                            ),
                            "collect_seconds": scrape.duration_seconds,
                            "node_series": scrape.series,
                            "max_series": settings.max_series(),
                            "limits": {
                                "max_peer_series": settings.max_peer_series,
                                "max_model_series": settings.max_model_series,
                                "max_gpu_series": settings.max_gpu_series,
                                "collect_timeout_ms": settings.collect_timeout.as_millis() as u64,
                            },
                        }))
                    })
                }),
        ],

        http: [
            // Streamed, so the plugin writes the response itself and can set
            // `Content-Type: text/plain; version=0.0.4`. The handler below is
            // what the host projects as the `prometheus-exporter.metrics` MCP
            // tool; the HTTP route never reaches it, because a streamed binding
            // is proxied over a side stream instead. See `serve`.
            http::get("/metrics")
                .binding_id(METRICS_BINDING_ID)
                .description(
                    "Return this node's metrics in Prometheus text exposition format. Over HTTP \
                     this is the scrape target; over MCP the same text is returned as a JSON \
                     string.",
                )
                .input::<NoArgs>()
                .stream_response()
                .handle(move |_args: NoArgs, _context| {
                    let collector = Arc::clone(&for_metrics);
                    Box::pin(async move {
                        let scrape = collector.scrape().await;
                        Ok(serde_json::json!({
                            "content_type": render::CONTENT_TYPE,
                            "up": scrape.up,
                            "error": scrape.error,
                            "node_series": scrape.series,
                            "exposition": scrape.body,
                        }))
                    })
                }),
        ],

        // Health must stay fast and must not depend on the node API being up:
        // a node that is down is the thing this plugin exists to report, not a
        // reason for the host to restart the reporter.
        health: move |_context| {
            let collector = Arc::clone(&for_health);
            Box::pin(async move { Ok(collector.health_detail()) })
        },
    };

    plugin.on_open_stream(move |request, _context| {
        let collector = Arc::clone(&for_streams);
        Box::pin(async move { serve::open_metrics_stream(collector, request).await })
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // Packaging path first, so `--print-package-manifest` works without the
    // launch environment. This plugin declares neither a config schema nor a
    // web UI, so it prints `{}`; the option exists so the packaging step is the
    // same for every plugin in the catalog.
    if arguments.first().map(String::as_str) == Some("--print-package-manifest") {
        let plugin = build_plugin(Arc::new(Collector::new(
            settings::settings_from(&[], None).map_err(anyhow::Error::msg)?,
        )));
        let manifest = plugin.manifest().context("prometheus-exporter manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }

    // `[[plugin]].url` arrives as TDCC_PLUGIN_URL; `[[plugin]].args` arrives as
    // process arguments. Bad configuration fails here, at startup, with a
    // message naming the offending value — not later, on a scrape nobody is
    // watching.
    let plugin_url = std::env::var("TDCC_PLUGIN_URL").ok();
    let settings = settings::settings_from(&arguments, plugin_url.as_deref())
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    if std::env::var("TDCC_PLUGIN_ENDPOINT").is_err()
        && std::env::var("MESH_LLM_PLUGIN_ENDPOINT").is_err()
    {
        // `PluginRuntime::run` says this too, but adding the exporter's own
        // configuration to the message turns "why did it exit?" into "ah, it
        // wanted a host" in one read.
        bail!(
            "TDCC_PLUGIN_ENDPOINT is not set: this binary is launched by tdcc, not run directly. \
             Configured node API would have been {}.",
            settings.node.base_url()
        );
    }

    PluginRuntime::run(build_plugin(Arc::new(Collector::new(settings)))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::settings_from;

    fn plugin() -> SimplePlugin {
        build_plugin(Arc::new(Collector::new(
            settings_from(&[], None).expect("defaults parse"),
        )))
    }

    #[test]
    fn the_manifest_declares_the_scrape_route_as_a_streamed_response() {
        let manifest = plugin().manifest().expect("declarative plugins have one");
        let binding = manifest
            .http_bindings
            .iter()
            .find(|binding| binding.binding_id == METRICS_BINDING_ID)
            .expect("the metrics binding is declared");

        assert_eq!(binding.path, "/metrics");
        assert_eq!(
            binding.method,
            tdcc_plugin::proto::HttpMethod::Get as i32,
            "Prometheus scrapes with GET"
        );
        assert_eq!(
            binding.response_body_mode,
            tdcc_plugin::proto::HttpBodyMode::Streamed as i32,
            "a buffered binding would be served as application/json"
        );
        assert_eq!(
            binding.request_body_mode,
            tdcc_plugin::proto::HttpBodyMode::Buffered as i32,
            "the scrape request has no body to stream"
        );
    }

    #[test]
    fn the_manifest_matches_the_names_used_everywhere_else() {
        let manifest = plugin().manifest().expect("declarative plugins have one");
        assert!(
            manifest
                .capabilities
                .contains(&"metrics.prometheus.v1".to_string())
        );
        let operations: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        assert!(operations.contains(&"check"), "{operations:?}");
        assert!(operations.contains(&METRICS_BINDING_ID), "{operations:?}");
        // The plugin id has to equal `plugin.toml` and `[[plugin]].name`, or
        // the host refuses the initialize handshake.
        assert_eq!(plugin().plugin_id(), PLUGIN_NAME);
    }

    #[test]
    fn the_plugin_declares_no_config_schema_or_web_ui() {
        // Both would be misleading: settings never reach the process, and there
        // is no page to render. `plugin-manifest.json` is `{}` as a result.
        let manifest = plugin().manifest().expect("declarative plugins have one");
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert_eq!(
            package_manifest_json(&manifest).expect("serializes"),
            "{}",
            "an empty package manifest may be omitted from the archive"
        );
    }
}
