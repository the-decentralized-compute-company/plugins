//! `event-webhook` — mesh events out of the node and into wherever a team
//! already talks.
//!
//! The plugin declares the mesh event kinds it wants, and the host delivers
//! them over the control connection. Everything after that is this process's
//! own problem: normalize, filter, coalesce, queue, POST, retry, and account
//! for anything dropped.
//!
//! The single rule that shapes the design: **the node must never wait on a
//! webhook.** `on_mesh_event` does no I/O and no `await` on anything that can
//! block — it hands the event to a bounded queue with a non-blocking
//! `try_send` and returns. A dead endpoint costs the node a counter, not a
//! stalled control connection.

mod coalesce;
mod config;
mod delivery;
mod event;
mod format;
mod logging;
mod stats;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tdcc_plugin::{
    Plugin, PluginContext, PluginError, PluginMetadata, PluginRuntime, SimplePlugin, events, mcp,
    package_manifest_json, plugin, plugin_server_info, proto,
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::coalesce::{Admission, Coalescer};
use crate::config::{
    MAX_COALESCE_KEYS, MAX_LIST_ITEMS, MAX_TRACKED_PEERS, Settings, WEBHOOK_URL_ENV,
};
use crate::delivery::DeliveryConfig;
use crate::event::{Delivery, ModelTracker, NodeEvent, now_ms, translate};
use crate::stats::Stats;

const PLUGIN_NAME: &str = "event-webhook";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The node and mesh this process belongs to, learned from the events
/// themselves. The launch contract does not carry either, so before the first
/// mesh event arrives both are genuinely unknown and are reported as such.
#[derive(Clone, Debug, Default)]
struct Identity {
    node_id: Option<String>,
    mesh_id: Option<String>,
}

struct WebhookRuntime {
    settings: Settings,
    stats: Arc<Stats>,
    /// `None` when no destination is configured. The plugin still runs so the
    /// operator can see *why* nothing is arriving.
    delivery: Option<DeliveryConfig>,
    queue: Option<mpsc::Sender<Delivery>>,
    client: Client,
    tracker: Mutex<ModelTracker>,
    coalescer: Mutex<Coalescer>,
    identity: Mutex<Identity>,
}

/// A poisoned mutex must not take the plugin down: every critical section here
/// is a few field writes with no `await`, so the state behind a poisoned lock
/// is still coherent enough to keep using.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WebhookRuntime {
    fn new(
        settings: Settings,
        client: Client,
        delivery: Option<DeliveryConfig>,
        queue: Option<mpsc::Sender<Delivery>>,
    ) -> Self {
        Self {
            tracker: Mutex::new(ModelTracker::new(MAX_TRACKED_PEERS)),
            coalescer: Mutex::new(Coalescer::new(settings.coalesce_window, MAX_COALESCE_KEYS)),
            identity: Mutex::new(Identity::default()),
            stats: Arc::new(Stats::default()),
            settings,
            delivery,
            queue,
            client,
        }
    }

    /// The hot path. Synchronous by construction — see the module comment.
    fn ingest(&self, raw: proto::MeshEvent) {
        let now = now_ms();
        let events = {
            let mut tracker = lock(&self.tracker);
            translate(raw, now, &mut tracker)
        };

        for event in events {
            self.remember_identity(&event);
            self.stats.record_received();

            if !self.settings.filter.allows(event.kind) {
                self.stats.record_filtered();
                continue;
            }

            let admission = {
                let mut coalescer = lock(&self.coalescer);
                coalescer.admit(&event.coalesce_key(), now)
            };
            let suppressed = match admission {
                Admission::Suppress => {
                    self.stats.record_coalesced();
                    continue;
                }
                Admission::Deliver { suppressed } => suppressed,
            };

            let Some(queue) = &self.queue else {
                self.stats.record_dropped_no_target();
                continue;
            };

            match queue.try_send(Delivery { event, suppressed }) {
                Ok(()) => {
                    self.stats.record_queued();
                }
                // Backpressure policy: drop the newest event and say so. The
                // alternative — an unbounded queue — turns a webhook outage
                // into memory growth on somebody else's machine.
                Err(TrySendError::Full(job)) => {
                    let dropped = self.stats.record_dropped_queue_full();
                    if dropped == 1 || dropped.is_multiple_of(100) {
                        logging::warn(format!(
                            "queue full ({} deep); dropped {} ({dropped} dropped in total)",
                            self.settings.queue_capacity,
                            job.event.kind.as_str()
                        ));
                    }
                }
                Err(TrySendError::Closed(job)) => {
                    let dropped = self.stats.record_dropped_queue_full();
                    if dropped == 1 {
                        logging::error(format!(
                            "delivery worker is gone; dropping {} and everything after it",
                            job.event.kind.as_str()
                        ));
                    }
                }
            }
        }
    }

    fn remember_identity(&self, event: &NodeEvent) {
        let mut identity = lock(&self.identity);
        if !event.node_id.is_empty() {
            identity.node_id = Some(event.node_id.clone());
        }
        if event.mesh_id.is_some() {
            identity.mesh_id = event.mesh_id.clone();
        }
    }

    fn queue_depth(&self) -> usize {
        self.queue
            .as_ref()
            .map(|queue| queue.max_capacity().saturating_sub(queue.capacity()))
            .unwrap_or(0)
    }

    fn status_json(&self) -> Value {
        let identity = lock(&self.identity).clone();
        json!({
            "plugin": PLUGIN_NAME,
            "version": PLUGIN_VERSION,
            "settings": self.settings.to_json(),
            "node_id": identity.node_id,
            "mesh_id": identity.mesh_id,
            "queue": {
                "capacity": self.settings.queue_capacity,
                "waiting": self.queue_depth(),
            },
            "tracking": {
                "peers_with_known_models": lock(&self.tracker).tracked_peers(),
                "coalescing_keys": lock(&self.coalescer).tracked_keys(),
            },
            "counters": self.stats.to_json(),
        })
    }

    /// What the console shows next to the plugin. Never fails the health check
    /// for a missing destination: an unhealthy plugin invites a restart loop,
    /// and restarting will not conjure an environment variable.
    fn health_detail(&self) -> String {
        match (&self.delivery, self.stats.last_error()) {
            (None, _) => format!(
                "no webhook target configured; set {WEBHOOK_URL_ENV} in the environment of the \
                 tdcc process"
            ),
            (Some(_), Some(error)) => format!("last delivery failed: {error}"),
            (Some(delivery), None) => format!(
                "delivering {} events to {}",
                self.settings.format.as_str(),
                delivery.target.redacted()
            ),
        }
    }

    /// Sends one synthetic event through the real client, and reports the real
    /// result. Bypasses the filter and the coalescer on purpose: the operator
    /// asked for exactly one delivery, right now.
    async fn run_test(&self, note: Option<String>) -> Result<Value> {
        let Some(outbound) = &self.delivery else {
            bail!(
                "no webhook target configured; set {WEBHOOK_URL_ENV} in the environment of the \
                 tdcc process and restart the plugin"
            );
        };

        let identity = lock(&self.identity).clone();
        let job = Delivery {
            event: NodeEvent::test_event(
                identity.node_id.unwrap_or_else(|| "unknown".to_string()),
                identity.mesh_id,
                note,
            ),
            suppressed: 0,
        };
        let payload = format::render(outbound.format, &job, outbound.max_list_items);
        let outcome = delivery::deliver(&self.client, outbound, &payload).await;

        self.stats
            .record_retries(u64::from(outcome.attempts.saturating_sub(1)));
        if outcome.delivered {
            self.stats.record_delivered();
            self.stats.record_delivery_time(now_ms());
            self.stats.set_last_error(None);
            return Ok(json!({
                "delivered": true,
                "attempts": outcome.attempts,
                "status": outcome.status,
                "target": outbound.target.redacted(),
                "format": outbound.format.as_str(),
            }));
        }

        // A tool that cannot reach its backend returns an error, not an empty
        // success. The message is already scrubbed of the webhook URL.
        let reason = outcome
            .error
            .unwrap_or_else(|| "delivery failed with no further detail".to_string());
        self.stats.record_failed();
        self.stats.set_last_error(Some(reason.clone()));
        bail!(
            "webhook delivery to {} failed after {} attempt(s): {reason}",
            outbound.target.redacted(),
            outcome.attempts
        );
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StatusArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct TestArgs {
    /// Free-text note carried in the test payload, so you can tell one test
    /// delivery from another in a busy channel.
    #[serde(default)]
    note: Option<String>,
}

fn build_plugin(runtime: Arc<WebhookRuntime>) -> SimplePlugin {
    let for_status = Arc::clone(&runtime);
    let for_test = Arc::clone(&runtime);
    let for_health = Arc::clone(&runtime);
    let for_ready = Arc::clone(&runtime);
    let for_events = runtime;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Event webhook",
                "Delivers TDCC mesh events to a Slack, Discord, or generic JSON webhook",
                None::<String>,
            ),
        ),

        // Delivery is allowlist-based: an undeclared kind is never sent to this
        // process at all. All six are declared even when the operator's filter
        // is narrower, so that editing a filter does not silently change the
        // manifest that `tdcc plugins info` reports. Filtering happens here.
        events: [
            events::peer_up(),
            events::peer_down(),
            events::peer_updated(),
            events::local_accepting(),
            events::local_standby(),
            events::mesh_id_updated(),
        ],

        mcp: [
            // Projected as `event-webhook.status`.
            mcp::tool("status")
                .description(
                    "Report the webhook destination (redacted), the active filter, and delivery \
                     counters."
                )
                .input::<StatusArgs>()
                .handle(move |_args: StatusArgs, _context: &mut PluginContext<'_>| {
                    let runtime = Arc::clone(&for_status);
                    Box::pin(async move { Ok(runtime.status_json()) })
                }),

            // Projected as `event-webhook.test`.
            mcp::tool("test")
                .description(
                    "Send one synthetic event to the configured webhook and report the real HTTP \
                     result. Fails with an error if no destination is configured or delivery does \
                     not succeed."
                )
                .input::<TestArgs>()
                .handle(move |args: TestArgs, _context: &mut PluginContext<'_>| {
                    let runtime = Arc::clone(&for_test);
                    Box::pin(async move {
                        runtime.run_test(args.note).await.map_err(PluginError::from)
                    })
                }),
        ],

        health: move |_context: &mut PluginContext<'_>| {
            let runtime = Arc::clone(&for_health);
            // Health reads counters only; it never touches the network, so it
            // stays fast while a delivery is in flight.
            Box::pin(async move { Ok(runtime.health_detail()) })
        },

        on_initialized: move |_context: &mut PluginContext<'_>| {
            let runtime = Arc::clone(&for_ready);
            Box::pin(async move {
                logging::info(runtime.health_detail());
                Ok(())
            })
        },

        on_mesh_event: move |mesh_event: proto::MeshEvent, _context: &mut PluginContext<'_>| {
            let runtime = Arc::clone(&for_events);
            Box::pin(async move {
                runtime.ingest(mesh_event);
                Ok(())
            })
        },
    }
}

/// Builds the client and, when a destination is configured, the bounded queue
/// and the worker draining it.
fn bootstrap(settings: Settings) -> Result<Arc<WebhookRuntime>> {
    let client = delivery::build_client(settings.request_timeout)
        .context("building the webhook HTTP client")?;

    let Some(target) = settings.target.clone() else {
        return Ok(Arc::new(WebhookRuntime::new(settings, client, None, None)));
    };

    let config = DeliveryConfig {
        target,
        format: settings.format,
        max_attempts: settings.max_attempts,
        max_list_items: MAX_LIST_ITEMS,
    };
    let (sender, receiver) = mpsc::channel(settings.queue_capacity);
    let runtime = Arc::new(WebhookRuntime::new(
        settings,
        client.clone(),
        Some(config.clone()),
        Some(sender),
    ));
    tokio::spawn(delivery::run_worker(
        receiver,
        client,
        config,
        Arc::clone(&runtime.stats),
    ));
    Ok(runtime)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Packaging path: the manifest is the same declaration the runtime serves,
    // and it does not depend on operator configuration.
    if args.first().map(String::as_str) == Some("--print-package-manifest") {
        if args.len() > 1 {
            bail!("--print-package-manifest takes no other arguments");
        }
        let runtime = bootstrap(config::parse(&[], &BTreeMap::new())?)?;
        let manifest = build_plugin(runtime)
            .manifest()
            .context("event-webhook manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }

    let settings = config::parse(&args, &config::read_env())?;
    let runtime = bootstrap(settings)?;
    logging::info(format!(
        "starting; {} (queue {} deep, coalescing {}s)",
        runtime.health_detail(),
        runtime.settings.queue_capacity,
        runtime.settings.coalesce_window.as_secs(),
    ));

    // Runtime path: connect to the control endpoint the host passed in
    // TDCC_PLUGIN_ENDPOINT / TDCC_PLUGIN_TRANSPORT and serve the manifest.
    PluginRuntime::run(build_plugin(runtime)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PayloadFormat;
    use crate::event::EventKind;
    use std::time::Duration;

    fn settings(filter: &str, queue_capacity: usize, coalesce_secs: u64) -> Settings {
        let mut settings = config::parse(&[], &BTreeMap::new()).expect("defaults parse");
        settings.filter = config::EventFilter::parse(filter).expect("valid filter");
        settings.queue_capacity = queue_capacity;
        settings.coalesce_window = Duration::from_secs(coalesce_secs);
        settings
    }

    fn runtime_with_queue(
        mut settings: Settings,
    ) -> (Arc<WebhookRuntime>, mpsc::Receiver<Delivery>) {
        let target = config::parse_target(
            "https://hooks.example.com/services/XXXXsecret",
            WEBHOOK_URL_ENV,
            false,
        )
        .expect("valid target");
        settings.target = Some(target.clone());
        let config = DeliveryConfig {
            target,
            format: PayloadFormat::Json,
            max_attempts: 1,
            max_list_items: MAX_LIST_ITEMS,
        };
        let (sender, receiver) = mpsc::channel(settings.queue_capacity);
        let client = delivery::build_client(settings.request_timeout).expect("client");
        (
            Arc::new(WebhookRuntime::new(
                settings,
                client,
                Some(config),
                Some(sender),
            )),
            receiver,
        )
    }

    fn peer_event(kind: proto::mesh_event::Kind, peer_id: &str) -> proto::MeshEvent {
        proto::MeshEvent {
            kind: kind as i32,
            peer: Some(proto::MeshPeer {
                peer_id: peer_id.to_string(),
                version: "0.72.1".to_string(),
                capabilities: Vec::new(),
                role: "host".to_string(),
                vram_bytes: 0,
                models: Vec::new(),
                serving_models: Vec::new(),
                available_models: Vec::new(),
                requested_models: Vec::new(),
                rtt_ms: None,
                model_source: String::new(),
                hosted_models: Vec::new(),
                hosted_models_known: None,
            }),
            local_peer_id: "local-node".to_string(),
            mesh_id: "mesh-7".to_string(),
            detail_json: String::new(),
        }
    }

    #[tokio::test]
    async fn the_manifest_declares_every_mesh_event_kind_and_both_tools() {
        let runtime =
            bootstrap(config::parse(&[], &BTreeMap::new()).expect("defaults")).expect("bootstrap");
        let manifest = build_plugin(runtime)
            .manifest()
            .expect("declarative manifest");

        assert_eq!(
            manifest.mesh_event_subscriptions.len(),
            6,
            "all six host mesh event kinds must be declared or they are never delivered"
        );
        let tools: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        assert_eq!(tools, vec!["status", "test"]);

        // No web UI and no config schema: `[plugin.settings]` never reaches the
        // plugin process, so declaring settings this process cannot read would
        // put dead controls in the console.
        assert!(manifest.web_ui.is_none());
        assert!(manifest.config_schema.is_none());
        assert!(manifest.http_bindings.is_empty());
    }

    #[tokio::test]
    async fn events_outside_the_filter_are_counted_and_dropped() {
        let (runtime, mut receiver) = runtime_with_queue(settings("peer.down", 8, 0));

        runtime.ingest(peer_event(proto::mesh_event::Kind::PeerUp, "peer-a"));

        assert_eq!(runtime.stats.received(), 1);
        assert_eq!(runtime.stats.filtered(), 1);
        assert_eq!(runtime.stats.queued(), 0);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_matching_event_reaches_the_queue_intact() {
        let (runtime, mut receiver) = runtime_with_queue(settings("all", 8, 0));

        runtime.ingest(peer_event(proto::mesh_event::Kind::PeerUp, "peer-a"));

        let job = receiver.try_recv().expect("queued");
        assert_eq!(job.event.kind, EventKind::PeerUp);
        assert_eq!(job.event.mesh_id.as_deref(), Some("mesh-7"));
        assert_eq!(runtime.stats.queued(), 1);
    }

    #[tokio::test]
    async fn a_flood_is_coalesced_before_it_ever_reaches_the_queue() {
        let (runtime, mut receiver) = runtime_with_queue(settings("all", 64, 3_600));

        for _ in 0..200 {
            runtime.ingest(peer_event(proto::mesh_event::Kind::PeerDown, "peer-a"));
        }

        let mut queued = 0;
        while receiver.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(
            queued, 1,
            "200 identical events must not become 200 messages"
        );
        assert_eq!(runtime.stats.coalesced(), 199);
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_accounts_instead_of_growing() {
        // Coalescing off and a queue of two, with nothing draining it.
        let (runtime, _receiver) = runtime_with_queue(settings("all", 2, 0));

        for index in 0..50 {
            runtime.ingest(peer_event(
                proto::mesh_event::Kind::PeerUp,
                &format!("peer-{index}"),
            ));
        }

        assert_eq!(runtime.stats.queued(), 2, "the queue cap must hold");
        assert_eq!(runtime.stats.dropped_queue_full(), 48);
        assert_eq!(runtime.queue_depth(), 2);
    }

    #[tokio::test]
    async fn with_no_destination_events_are_counted_as_dropped_not_silently_lost() {
        let runtime = bootstrap(settings("all", 8, 0)).expect("bootstrap without a target");

        runtime.ingest(peer_event(proto::mesh_event::Kind::PeerUp, "peer-a"));

        assert_eq!(runtime.stats.received(), 1);
        assert_eq!(runtime.stats.dropped_no_target(), 1);
        assert!(runtime.health_detail().contains(WEBHOOK_URL_ENV));
    }

    #[tokio::test]
    async fn the_test_tool_errors_when_nothing_is_configured() {
        let runtime = bootstrap(settings("all", 8, 0)).expect("bootstrap without a target");

        let error = runtime.run_test(None).await.expect_err("must fail loudly");

        assert!(error.to_string().contains(WEBHOOK_URL_ENV), "{error}");
    }

    #[tokio::test]
    async fn status_reports_the_destination_without_the_secret() {
        let (runtime, _receiver) = runtime_with_queue(settings("all", 8, 0));

        let rendered = runtime.status_json().to_string();

        assert!(rendered.contains("hooks.example.com"));
        assert!(!rendered.contains("XXXXsecret"), "{rendered}");
        assert_eq!(runtime.status_json()["queue"]["capacity"], json!(8));
    }

    #[tokio::test]
    async fn identity_is_learned_from_events_rather_than_invented() {
        let (runtime, _receiver) = runtime_with_queue(settings("all", 8, 0));
        assert_eq!(runtime.status_json()["node_id"], Value::Null);

        runtime.ingest(peer_event(proto::mesh_event::Kind::PeerUp, "peer-a"));

        assert_eq!(runtime.status_json()["node_id"], json!("local-node"));
        assert_eq!(runtime.status_json()["mesh_id"], json!("mesh-7"));
    }
}
