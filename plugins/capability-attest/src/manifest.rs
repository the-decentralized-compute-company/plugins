//! The whole contribution surface of `capability-attest` in one declaration.
//!
//! One `plugin!` invocation carries the capability, the mesh channel, the mesh
//! events, five MCP tools, four HTTP routes, and the lifecycle hooks. The host
//! reads this manifest during initialization and projects it; the plugin never
//! registers a route or an MCP method itself.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks. Omitting a field is fine, reordering is not.
//!
//! There is no `config` block. `[plugin.settings]` values never reach a plugin
//! process, so a schema here would render controls in the console that this
//! process could not read — the settings that matter are `[[plugin]].args` and
//! the `TDCC_ATTEST_*` variables, both documented in the README.

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, events, http, mcp, mesh, plugin, plugin_server_info,
    proto,
};

use crate::attestor::{Attestor, CHANNEL, MESSAGE_RECORD, MESSAGE_REQUEST};
use crate::record::{WHAT_A_SIGNATURE_DOES_NOT_PROVE, WHAT_A_SIGNATURE_PROVES};

/// The capability other components can depend on by name.
pub const CAPABILITY: &str = "capability-attest.v1";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordArgs {
    /// Also return this node's own verification of the record it is handing
    /// you. Useful for spotting an expired record without a second call.
    #[serde(default)]
    pub verify: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BenchmarkArgs {
    /// Skip the cooldown and the failure backoff. It does not skip an operator
    /// hold, and it does not skip the check for whether this node is currently
    /// serving traffic — both of those exist to protect somebody else's
    /// request, not to protect the schedule.
    #[serde(default)]
    pub ignore_cooldown: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyArgs {
    /// A signed capability record, exactly as `record` or a peer returned it.
    pub record: serde_json::Value,
    /// Reject a record older than this many seconds even if its own expiry has
    /// not passed. Omit to trust the record's declared lifetime.
    #[serde(default)]
    pub max_age_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HoldArgs {
    /// How long to pause attestation, in seconds. Zero clears an existing hold.
    pub seconds: u64,
    /// Why, so whoever reads `status` next knows what is going on.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Serialise a response, turning a serialisation failure into the `anyhow`
/// error the handler signature knows how to convert.
fn to_json<T: serde::Serialize>(value: &T) -> anyhow::Result<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|error| anyhow::anyhow!("could not serialise the response: {error}"))
}

/// Response for `record` when no benchmark has completed yet.
///
/// Not an error — a node that has just started legitimately has nothing to
/// publish — but unmistakably not a measurement either.
fn no_record_yet() -> serde_json::Value {
    serde_json::json!({
        "available": false,
        "reason": "no benchmark has completed on this node yet; call status for the schedule \
                   and any deferral or failure reason",
        "what_a_signature_proves": WHAT_A_SIGNATURE_PROVES,
        "what_a_signature_does_not_prove": WHAT_A_SIGNATURE_DOES_NOT_PROVE,
    })
}

pub fn capability_attest_plugin(attestor: Attestor) -> SimplePlugin {
    let for_status = attestor.clone();
    let for_record = attestor.clone();
    let for_verify = attestor.clone();
    let for_benchmark = attestor.clone();
    let for_hold = attestor.clone();
    let for_peers = attestor.clone();
    let for_http_status = attestor.clone();
    let for_http_record = attestor.clone();
    let for_http_peers = attestor.clone();
    let for_http_verify = attestor.clone();
    let for_health = attestor.clone();
    let for_initialized = attestor.clone();
    let for_channel = attestor.clone();
    let for_mesh_event = attestor;

    plugin! {
        metadata: PluginMetadata::new(
            "capability-attest",
            env!("CARGO_PKG_VERSION"),
            plugin_server_info(
                "capability-attest",
                env!("CARGO_PKG_VERSION"),
                "Capability attestation",
                "Benchmarks this node on a pinned profile and publishes a signed capability record",
                None::<String>,
            ),
        ),

        provides: [capability(CAPABILITY)],

        // The only channel this plugin is allowed to send or receive on.
        // Delivery is allowlist-based, so nothing else reaches it.
        mesh: [mesh::channel(CHANNEL)],

        // Peer arrival is when a record is worth exchanging; peer departure is
        // when a stored record stops being worth keeping.
        events: [events::peer_up(), events::peer_down()],

        mcp: [
            mcp::tool("status")
                .description(
                    "Report the pinned benchmark profile, the current record, the schedule, and \
                     why the last attempt did or did not run.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let attestor = for_status.clone();
                    Box::pin(async move { Ok(attestor.status()?) })
                }),

            mcp::tool("record")
                .description(
                    "Return this node's latest signed capability record. The signature proves the \
                     record came from this node's key and was not altered; it does not prove the \
                     benchmark was run honestly.",
                )
                .input::<RecordArgs>()
                .handle(move |args: RecordArgs, _context| {
                    let attestor = for_record.clone();
                    Box::pin(async move {
                        let Some(record) = attestor.latest_record() else {
                            return Ok(no_record_yet());
                        };
                        let mut response = serde_json::json!({
                            "available": true,
                            "record": record,
                            "what_a_signature_proves": WHAT_A_SIGNATURE_PROVES,
                            "what_a_signature_does_not_prove": WHAT_A_SIGNATURE_DOES_NOT_PROVE,
                        });
                        if args.verify.unwrap_or(true) {
                            let report = attestor.verify_record(to_json(&record)?, None)?;
                            response["verification"] = to_json(&report)?;
                        }
                        Ok(response)
                    })
                }),

            mcp::tool("verify")
                .description(
                    "Check a capability record from any node: signature, key binding, whether the \
                     pinned prompt still rebuilds, whether the headline numbers follow from the \
                     samples beside them, freshness, and owner attribution.",
                )
                .input::<VerifyArgs>()
                .handle(move |args: VerifyArgs, _context| {
                    let attestor = for_verify.clone();
                    Box::pin(async move {
                        let report =
                            attestor.verify_record(args.record, args.max_age_seconds)?;
                        Ok(to_json(&report)?)
                    })
                }),

            mcp::tool("benchmark")
                .description(
                    "Run the benchmark now and publish a new signed record. Defers if the node is \
                     serving traffic, if load cannot be determined, or if a hold is active. Takes \
                     as long as the benchmark takes; health stays responsive meanwhile.",
                )
                .input::<BenchmarkArgs>()
                .handle(move |args: BenchmarkArgs, _context| {
                    let attestor = for_benchmark.clone();
                    Box::pin(async move {
                        let outcome = attestor
                            .attempt(args.ignore_cooldown.unwrap_or(false))
                            .await?;
                        Ok(to_json(&outcome)?)
                    })
                }),

            mcp::tool("hold")
                .description(
                    "Pause attestation for a while — before a driver update, a rebuild, or any \
                     window where a benchmark would be wrong or unwelcome. Zero seconds clears it.",
                )
                .input::<HoldArgs>()
                .handle(move |args: HoldArgs, _context| {
                    let attestor = for_hold.clone();
                    Box::pin(async move { Ok(attestor.hold(args.seconds, args.reason)?) })
                }),

            mcp::tool("peers")
                .description(
                    "List capability records received from mesh peers, each re-verified now so \
                     freshness is current rather than whatever it was on arrival.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let attestor = for_peers.clone();
                    Box::pin(async move { Ok(attestor.peers()?) })
                }),
        ],

        http: [
            // GET /api/plugins/capability-attest/http/record
            http::get("/record")
                .description("This node's latest signed capability record.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let attestor = for_http_record.clone();
                    Box::pin(async move {
                        Ok(match attestor.latest_record() {
                            Some(record) => serde_json::json!({
                                "available": true,
                                "record": record,
                                "what_a_signature_proves": WHAT_A_SIGNATURE_PROVES,
                                "what_a_signature_does_not_prove":
                                    WHAT_A_SIGNATURE_DOES_NOT_PROVE,
                            }),
                            None => no_record_yet(),
                        })
                    })
                }),

            // GET /api/plugins/capability-attest/http/status
            http::get("/status")
                .description("Profile, current record, schedule, and last attempt.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let attestor = for_http_status.clone();
                    Box::pin(async move { Ok(attestor.status()?) })
                }),

            // GET /api/plugins/capability-attest/http/peers
            http::get("/peers")
                .description("Capability records received from mesh peers, re-verified now.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let attestor = for_http_peers.clone();
                    Box::pin(async move { Ok(attestor.peers()?) })
                }),

            // POST /api/plugins/capability-attest/http/verify
            http::post("/verify")
                .description("Verify a capability record supplied in the request body.")
                .input::<VerifyArgs>()
                .handle(move |args: VerifyArgs, _context| {
                    let attestor = for_http_verify.clone();
                    Box::pin(async move {
                        let report =
                            attestor.verify_record(args.record, args.max_age_seconds)?;
                        Ok(to_json(&report)?)
                    })
                }),
        ],

        // Health reads one field and returns. It never waits on a benchmark or
        // on the lock a benchmark holds.
        health: move |_context| {
            let attestor = for_health.clone();
            Box::pin(async move { attestor.health() })
        },

        // Start the periodic loop once the control session is up. This hook
        // runs while the host holds the plugin lock, so it must return
        // immediately — it spawns and gets out of the way.
        on_initialized: move |_context| {
            let attestor = for_initialized.clone();
            Box::pin(async move {
                attestor.spawn_background_loop();
                Ok(())
            })
        },

        on_channel_message: move |message: proto::ChannelMessage, context| {
            let attestor = for_channel.clone();
            Box::pin(async move {
                if message.channel != CHANNEL {
                    return Ok(());
                }
                match message.message_kind.as_str() {
                    MESSAGE_REQUEST => {
                        if let Some(record) = attestor.latest_record() {
                            context
                                .send_json_channel(
                                    CHANNEL,
                                    message.source_peer_id,
                                    MESSAGE_RECORD,
                                    &record,
                                )
                                .await?;
                        }
                    }
                    MESSAGE_RECORD => {
                        // A peer sending a record we cannot verify is that
                        // peer's problem. Logging and continuing keeps one bad
                        // peer from tearing down the control-session handler
                        // for every other peer.
                        if let Err(error) =
                            attestor.accept_peer_record(&message.source_peer_id, &message.body)
                        {
                            eprintln!("capability-attest: rejected a peer record: {error:#}");
                        }
                    }
                    _ => {}
                }
                Ok(())
            })
        },

        on_mesh_event: move |event: proto::MeshEvent, context| {
            let attestor = for_mesh_event.clone();
            Box::pin(async move {
                let peer_id = event
                    .peer
                    .as_ref()
                    .map(|peer| peer.peer_id.clone())
                    .unwrap_or_default();
                if peer_id.is_empty() {
                    return Ok(());
                }
                match proto::mesh_event::Kind::try_from(event.kind)
                    .unwrap_or(proto::mesh_event::Kind::Unspecified)
                {
                    proto::mesh_event::Kind::PeerUp => {
                        // Ask for theirs, offer ours. Both are small.
                        context
                            .send_json_channel(
                                CHANNEL,
                                peer_id.clone(),
                                MESSAGE_REQUEST,
                                &serde_json::json!({}),
                            )
                            .await?;
                        if let Some(record) = attestor.latest_record() {
                            context
                                .send_json_channel(CHANNEL, peer_id, MESSAGE_RECORD, &record)
                                .await?;
                        }
                    }
                    proto::mesh_event::Kind::PeerDown => attestor.forget_peer(&peer_id),
                    _ => {}
                }
                Ok(())
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tdcc_plugin::Plugin;

    fn manifest() -> proto::PluginManifest {
        let attestor = Attestor::new(&[], &BTreeMap::new(), "0.1.0");
        capability_attest_plugin(attestor)
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn the_manifest_declares_exactly_the_surfaces_this_plugin_needs() {
        let manifest = manifest();

        assert_eq!(manifest.capabilities, vec![CAPABILITY.to_string()]);

        let channels: Vec<&str> = manifest
            .mesh_channels
            .iter()
            .map(|channel| channel.name.as_str())
            .collect();
        assert_eq!(
            channels,
            vec![CHANNEL],
            "declaring extra channels would widen what the host delivers here"
        );

        let events: Vec<i32> = manifest
            .mesh_event_subscriptions
            .iter()
            .map(|subscription| subscription.kind)
            .collect();
        assert_eq!(
            events,
            vec![
                proto::mesh_event::Kind::PeerUp as i32,
                proto::mesh_event::Kind::PeerDown as i32,
            ]
        );

        assert!(
            manifest.config_schema.is_none(),
            "settings never reach the process, so declaring a schema would mislead the console"
        );
        assert!(manifest.web_ui.is_none());
        assert!(
            manifest.endpoints.is_empty(),
            "this plugin attaches no external endpoint"
        );
    }

    #[test]
    fn every_advertised_tool_is_present_with_a_description() {
        let manifest = manifest();
        let names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();

        for expected in ["status", "record", "verify", "benchmark", "hold", "peers"] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}: {names:?}"
            );
        }
        for operation in &manifest.operations {
            assert!(
                !operation.description.trim().is_empty(),
                "{} has no description; descriptions are shown to models and operators",
                operation.name
            );
        }
    }

    #[test]
    fn the_http_routes_are_the_four_documented_ones() {
        let manifest = manifest();
        let mut routes: Vec<String> = manifest
            .http_bindings
            .iter()
            .map(|binding| {
                let method = proto::HttpMethod::try_from(binding.method)
                    .unwrap_or(proto::HttpMethod::Unspecified);
                format!("{method:?} {}", binding.path)
            })
            .collect();
        routes.sort();

        assert_eq!(
            routes,
            vec![
                "Get /peers".to_string(),
                "Get /record".to_string(),
                "Get /status".to_string(),
                "Post /verify".to_string(),
            ]
        );
    }

    #[test]
    fn the_tool_schemas_describe_their_arguments() {
        let manifest = manifest();
        let verify = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "verify")
            .expect("verify tool");

        let schema: serde_json::Value =
            serde_json::from_str(&verify.input_schema_json).expect("schema is JSON");

        assert!(
            schema["properties"]["record"].is_object(),
            "the record argument must appear in the advertised schema: {schema}"
        );
        assert!(
            schema["properties"]["max_age_seconds"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("expiry"),
            "doc comments on tool arguments become the descriptions users see: {schema}"
        );
    }

    #[test]
    fn a_record_response_with_nothing_to_report_is_not_an_empty_success() {
        let response = no_record_yet();

        assert_eq!(response["available"], false);
        assert!(
            response["reason"].as_str().unwrap().contains("status"),
            "{response}"
        );
    }
}
