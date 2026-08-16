//! The whole contribution surface of `contribution-ledger`, in one
//! declaration.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks. Omitting a field is fine; reordering is not.
//!
//! Two absences are deliberate:
//!
//! - **No `mesh` channels.** A local record must not gossip. Nothing this
//!   plugin records is sent to another node, so it declares no channel and the
//!   host's allowlist guarantees it could not receive one either.
//! - **No `inference`.** The ledger attaches no backend and serves no model; it
//!   only reads counters the host already keeps.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, config_boolean, config_integer, config_schema,
    config_setting, constraint_range, events, http, mcp, plugin, plugin_server_info, proto, web_ui,
    web_ui_bundle, web_ui_config_section, web_ui_page,
};

use crate::ledger::{DEFAULT_WINDOW_DAYS, Ledger, MeshEventKind};
use crate::{PLUGIN_NAME, PLUGIN_VERSION};

/// Arguments for the aggregating reads.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowArgs {
    /// How many days back to aggregate, counted from now. Values outside
    /// 1-3650 are clamped rather than rejected. Defaults to 7.
    #[serde(default)]
    pub days: Option<u32>,
}

/// Arguments for the row-returning reads.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RowsArgs {
    /// How many days back to look, counted from now. Clamped to 1-3650.
    /// Defaults to 7.
    #[serde(default)]
    pub days: Option<u32>,
    /// Maximum rows to return, newest first. Clamped to 1-1000. Defaults to 50.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

pub fn contribution_ledger_plugin(ledger: Arc<Ledger>) -> SimplePlugin {
    // One clone per handler: the handlers are `Fn`, so each needs to own its
    // reference to the shared ledger rather than borrow one.
    let for_summary = Arc::clone(&ledger);
    let for_epochs = Arc::clone(&ledger);
    let for_peers = Arc::clone(&ledger);
    let for_status = Arc::clone(&ledger);
    let for_flush = Arc::clone(&ledger);
    let for_http_summary = Arc::clone(&ledger);
    let for_http_epochs = Arc::clone(&ledger);
    let for_http_peers = Arc::clone(&ledger);
    let for_http_status = Arc::clone(&ledger);
    let for_http_flush = Arc::clone(&ledger);
    let for_health = Arc::clone(&ledger);
    let for_init = Arc::clone(&ledger);
    let for_events = ledger;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Contribution ledger",
                "Local, self-reported record of what this node contributed to the mesh",
                None::<String>,
            ),
        ),

        // A stable name for "this node keeps a local contribution record",
        // so a console or another plugin can depend on the contract rather
        // than on this plugin's id.
        provides: [capability("contribution-ledger.v1")],

        // Operator-facing settings. These are stored by the host and rendered
        // by the console's own controls; they are *not* delivered to this
        // process, so both of them are read by the console bundle only. The
        // knobs the process itself needs live in `[[plugin]].args`.
        config: [config_schema(PLUGIN_NAME)
            .setting(
                config_setting("default_window_days", config_integer())
                    .default_value(&DEFAULT_WINDOW_DAYS)
                    .constraint(constraint_range(Some("1"), Some("3650")))
                    .apply_mode(proto::PluginConfigApplyMode::DynamicValidationOnly)
                    .restart_scope(proto::PluginConfigRestartScope::None)
                    .description("Window the console page opens with.")
                    .label("Default window (days)")
                    .help("Read by the console page only. Retention is set with --retain-days.")
                    .category("ledger-display", "Display", "Contribution page display", 10)
                    .order(10)
                    .unit("days")
                    .control_hint("number"),
            )
            .setting(
                config_setting("show_peer_ids", config_boolean())
                    .default_value(&true)
                    .apply_mode(proto::PluginConfigApplyMode::DynamicValidationOnly)
                    .restart_scope(proto::PluginConfigRestartScope::None)
                    .description("Show peer id prefixes on the console page.")
                    .label("Show peer ids")
                    .help(
                        "Display only. To stop peer ids being written to disk at all, pass \
                         --no-peer-ids in this plugin's args.",
                    )
                    .category("ledger-display", "Display", "Contribution page display", 10)
                    .order(20)
                    .control_hint("switch"),
            )],

        // v1 permits exactly one bundle root; every page and config section
        // references it by id and names an entry script inside it.
        web_ui: [web_ui()
            .bundle(web_ui_bundle("main", "bundle"))
            .page(
                web_ui_page(
                    "contribution",
                    "Contribution",
                    "contribution",
                    "register-mesh-plugin-ui.js",
                )
                .bundle_id("main"),
            )
            .config_section(
                web_ui_config_section(
                    "ledger-actions",
                    "Contribution ledger",
                    "register-mesh-plugin-ui.js",
                )
                .parent_tab("integrations")
                .bundle_id("main"),
            )],

        // Presence and serving-availability only. There is no per-request
        // event on this path, and the ledger declares nothing it does not use.
        events: [
            events::peer_up(),
            events::peer_down(),
            events::local_accepting(),
            events::local_standby(),
            events::mesh_id_updated(),
        ],

        mcp: [
            mcp::tool("summary")
                .description(
                    "Aggregate what this node contributed over a window: requests it fronted and \
                     answered on its own hardware, completion tokens observed, attempt time, and \
                     how much of the window the ledger could actually see. Self-reported local \
                     record, not a balance and not proof to anyone else. Errors instead of \
                     returning zeroes when nothing was measured.",
                )
                .input::<WindowArgs>()
                .handle(move |args: WindowArgs, _context| {
                    let ledger = Arc::clone(&for_summary);
                    Box::pin(async move { Ok(ledger.summary(args.days)?) })
                }),

            mcp::tool("epochs")
                .description(
                    "List the raw aggregated buckets behind the summary, newest first. Hourly for \
                     recent history, one row per UTC day beyond the compaction horizon. There is \
                     no per-request row to list.",
                )
                .input::<RowsArgs>()
                .handle(move |args: RowsArgs, _context| {
                    let ledger = Arc::clone(&for_epochs);
                    Box::pin(async move { Ok(ledger.epochs(args.days, args.limit)?) })
                }),

            mcp::tool("peers")
                .description(
                    "List peers this node shared a mesh with during the window, by id prefix, \
                     with how many buckets each appeared in. Presence only: the host does not \
                     attribute inbound requests to the peer that sent them, so this cannot say \
                     who was served.",
                )
                .input::<RowsArgs>()
                .handle(move |args: RowsArgs, _context| {
                    let ledger = Arc::clone(&for_peers);
                    Box::pin(async move { Ok(ledger.peers(args.days, args.limit)?) })
                }),

            mcp::tool("status")
                .description(
                    "Diagnostics for the ledger itself: where its files are, whether the last \
                     journal write and the last host status poll succeeded, the retention policy \
                     in force, and exactly what is and is not recorded. Always answers, including \
                     when `summary` refuses to.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let ledger = Arc::clone(&for_status);
                    Box::pin(async move { Ok(ledger.status()) })
                }),

            mcp::tool("flush")
                .description(
                    "Write the in-progress bucket to the journal now. Useful before reading the \
                     file directly or stopping the node; buckets otherwise reach disk when the \
                     UTC hour rolls over.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let ledger = Arc::clone(&for_flush);
                    Box::pin(async move { Ok(ledger.flush()?) })
                }),
        ],

        // The console page reads these; they mirror the MCP tools exactly so
        // there is one implementation and one set of caveats.
        http: [
            http::get("/summary")
                .description("Aggregate contribution over a window.")
                .input::<WindowArgs>()
                .handle(move |args: WindowArgs, _context| {
                    let ledger = Arc::clone(&for_http_summary);
                    Box::pin(async move { Ok(ledger.summary(args.days)?) })
                }),

            http::get("/epochs")
                .description("Aggregated buckets, newest first.")
                .input::<RowsArgs>()
                .handle(move |args: RowsArgs, _context| {
                    let ledger = Arc::clone(&for_http_epochs);
                    Box::pin(async move { Ok(ledger.epochs(args.days, args.limit)?) })
                }),

            http::get("/peers")
                .description("Peers seen in the mesh during the window.")
                .input::<RowsArgs>()
                .handle(move |args: RowsArgs, _context| {
                    let ledger = Arc::clone(&for_http_peers);
                    Box::pin(async move { Ok(ledger.peers(args.days, args.limit)?) })
                }),

            http::get("/status")
                .description("Ledger diagnostics.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let ledger = Arc::clone(&for_http_status);
                    Box::pin(async move { Ok(ledger.status()) })
                }),

            // POST, not GET: flushing writes to disk.
            http::post("/flush")
                .description("Write the in-progress bucket to the journal now.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let ledger = Arc::clone(&for_http_flush);
                    Box::pin(async move { Ok(ledger.flush()?) })
                }),
        ],

        // Reads two atomics and allocates one short string, so it stays
        // responsive no matter what the sampler or a compaction is doing.
        health: move |_context| {
            let line = for_health.health_line();
            Box::pin(async move { Ok(line) })
        },

        // The host may re-run this if the control session is re-established;
        // the sampler slot makes sure only one sampler ever exists.
        on_initialized: move |_context| {
            let ledger = Arc::clone(&for_init);
            Box::pin(async move {
                if ledger.claim_sampler_slot() {
                    crate::sampler::spawn(ledger);
                }
                Ok(())
            })
        },

        on_mesh_event: move |event: proto::MeshEvent, _context| {
            let ledger = Arc::clone(&for_events);
            Box::pin(async move {
                if let Some(kind) = mesh_event_kind(event.kind) {
                    ledger.note_mesh_event(
                        kind,
                        event.peer.as_ref().map(|peer| peer.peer_id.as_str()),
                        Some(event.local_peer_id.as_str()),
                        Some(event.mesh_id.as_str()),
                    );
                }
                Ok(())
            })
        },
    }
}

/// Map the wire enum onto the ledger's own event kind.
///
/// Unknown or unspecified kinds return `None`: the host only delivers what the
/// manifest declared, but a newer host with a wider enum must not make an older
/// plugin misfile an event.
fn mesh_event_kind(kind: i32) -> Option<MeshEventKind> {
    match proto::mesh_event::Kind::try_from(kind).ok()? {
        proto::mesh_event::Kind::PeerUp => Some(MeshEventKind::PeerUp),
        proto::mesh_event::Kind::PeerDown => Some(MeshEventKind::PeerDown),
        proto::mesh_event::Kind::LocalAccepting => Some(MeshEventKind::LocalAccepting),
        proto::mesh_event::Kind::LocalStandby => Some(MeshEventKind::LocalStandby),
        proto::mesh_event::Kind::MeshIdUpdated => Some(MeshEventKind::MeshIdUpdated),
        proto::mesh_event::Kind::PeerUpdated | proto::mesh_event::Kind::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LedgerConfig, StatusSource};
    use tdcc_plugin::Plugin;

    fn test_ledger(tag: &str) -> Arc<Ledger> {
        let dir =
            std::env::temp_dir().join(format!("tdcc-ledger-manifest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(
            Ledger::open(LedgerConfig {
                state_dir: dir,
                source: StatusSource::OptedOut,
                poll_secs: 30,
                record_peers: true,
                compact_after_days: 14,
                retain_days: 400,
            })
            .expect("a fresh directory is a valid ledger"),
        )
    }

    #[test]
    fn the_manifest_declares_one_bundle_root_with_matching_bundle_ids() {
        let ledger = test_ledger("bundle");
        let manifest = contribution_ledger_plugin(Arc::clone(&ledger))
            .manifest()
            .expect("declarative plugins have a manifest");
        let web_ui = manifest.web_ui.expect("web_ui is declared");

        let [bundle] = web_ui.bundles.as_slice() else {
            panic!("v1 permits exactly one bundle root");
        };
        assert_eq!(bundle.root_path, "bundle");
        assert!(web_ui.pages.iter().all(|page| page.bundle_id == bundle.id));
        assert!(
            web_ui
                .config_sections
                .iter()
                .all(|section| section.bundle_id == bundle.id)
        );

        std::fs::remove_dir_all(ledger.config().state_dir.clone()).unwrap();
    }

    #[test]
    fn the_manifest_subscribes_to_presence_events_and_no_mesh_channels() {
        let ledger = test_ledger("events");
        let manifest = contribution_ledger_plugin(Arc::clone(&ledger))
            .manifest()
            .expect("declarative plugins have a manifest");

        assert_eq!(
            manifest.mesh_event_subscriptions.len(),
            5,
            "peer up/down, local accepting/standby, mesh id"
        );
        assert!(
            manifest.mesh_channels.is_empty(),
            "a local record must not gossip"
        );
        assert!(
            manifest.endpoints.is_empty(),
            "the ledger attaches no external endpoint"
        );

        std::fs::remove_dir_all(ledger.config().state_dir.clone()).unwrap();
    }

    #[test]
    fn every_wire_event_kind_maps_or_is_deliberately_ignored() {
        assert_eq!(
            mesh_event_kind(proto::mesh_event::Kind::PeerUp as i32),
            Some(MeshEventKind::PeerUp)
        );
        assert_eq!(
            mesh_event_kind(proto::mesh_event::Kind::LocalStandby as i32),
            Some(MeshEventKind::LocalStandby)
        );
        assert_eq!(
            mesh_event_kind(proto::mesh_event::Kind::PeerUpdated as i32),
            None
        );
        assert_eq!(mesh_event_kind(0), None, "unspecified is not an event");
        assert_eq!(
            mesh_event_kind(9_999),
            None,
            "a newer host must not misfile"
        );
    }

    #[test]
    fn the_declared_argument_bounds_match_what_the_handlers_enforce() {
        // The tool descriptions promise these ranges to models and operators;
        // the clamps the handlers apply have to agree with them.
        assert_eq!(crate::ledger::clamp_window(None), 7);
        assert_eq!(crate::ledger::clamp_window(Some(u32::MAX)), 3_650);
        assert_eq!(crate::ledger::clamp_limit(None), 50);
        assert_eq!(crate::ledger::clamp_limit(Some(u32::MAX)), 1_000);
    }
}
