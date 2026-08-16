//! Turning the host's `MeshEvent` into something a chat channel can read.
//!
//! The host delivers exactly six mesh event kinds over the control connection
//! (`proto::mesh_event::Kind`). Everything this plugin emits is either one of
//! those six, renamed to a stable dotted form, or *derived* from them — see
//! [`ModelTracker`]. Nothing here invents a signal the node does not actually
//! send.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tdcc_plugin::proto;

/// Milliseconds since the Unix epoch, saturating at 0 before 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// How an event should be coloured in Slack and Discord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Something joined, started, or became available.
    Good,
    /// Informational; neither an arrival nor a loss.
    Notice,
    /// Something left, stopped, or became unavailable.
    Warn,
}

/// Every event this plugin can emit.
///
/// The first six map one-to-one onto host mesh events. `ModelLoaded` and
/// `ModelUnloaded` are derived by diffing `serving_models` across peer updates.
/// `Test` is only ever produced by the `test` MCP tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    PeerUp,
    PeerDown,
    PeerUpdated,
    NodeAccepting,
    NodeStandby,
    MeshIdUpdated,
    ModelLoaded,
    ModelUnloaded,
    Test,
}

/// The kinds an operator may name in a filter. `Test` is deliberately absent:
/// it is not something the mesh produces, so filtering on it means nothing.
pub const SUBSCRIBABLE: [EventKind; 8] = [
    EventKind::PeerUp,
    EventKind::PeerDown,
    EventKind::PeerUpdated,
    EventKind::NodeAccepting,
    EventKind::NodeStandby,
    EventKind::MeshIdUpdated,
    EventKind::ModelLoaded,
    EventKind::ModelUnloaded,
];

impl EventKind {
    /// The canonical name used in payloads, filters, and the `status` tool.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PeerUp => "peer.up",
            EventKind::PeerDown => "peer.down",
            EventKind::PeerUpdated => "peer.updated",
            EventKind::NodeAccepting => "node.accepting",
            EventKind::NodeStandby => "node.standby",
            EventKind::MeshIdUpdated => "mesh.id_updated",
            EventKind::ModelLoaded => "model.loaded",
            EventKind::ModelUnloaded => "model.unloaded",
            EventKind::Test => "webhook.test",
        }
    }

    /// Accepts both the canonical dotted name and the host's own snake_case
    /// spelling, because operators read both in host logs and in this README.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "peer.up" | "peer_up" => Some(EventKind::PeerUp),
            "peer.down" | "peer_down" => Some(EventKind::PeerDown),
            "peer.updated" | "peer_updated" => Some(EventKind::PeerUpdated),
            "node.accepting" | "local_accepting" => Some(EventKind::NodeAccepting),
            "node.standby" | "local_standby" => Some(EventKind::NodeStandby),
            "mesh.id_updated" | "mesh_id_updated" => Some(EventKind::MeshIdUpdated),
            "model.loaded" | "model_loaded" => Some(EventKind::ModelLoaded),
            "model.unloaded" | "model_unloaded" => Some(EventKind::ModelUnloaded),
            _ => None,
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            EventKind::PeerUp | EventKind::NodeAccepting | EventKind::ModelLoaded => Severity::Good,
            EventKind::PeerDown | EventKind::NodeStandby | EventKind::ModelUnloaded => {
                Severity::Warn
            }
            EventKind::PeerUpdated | EventKind::MeshIdUpdated | EventKind::Test => Severity::Notice,
        }
    }
}

/// The parts of `proto::MeshPeer` that are actually populated by the host.
///
/// `capabilities` and `available_models` are always empty on the wire today, so
/// they are not carried here — advertising an always-empty field would read as
/// "this peer has no capabilities", which is not what it means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerSummary {
    pub peer_id: String,
    pub version: Option<String>,
    pub role: Option<String>,
    pub vram_bytes: Option<u64>,
    pub rtt_ms: Option<u32>,
    pub serving_models: Vec<String>,
    pub models: Vec<String>,
}

/// Peer ids are 64 hex characters. Chat needs something a human can compare.
pub fn short_id(peer_id: &str) -> String {
    if peer_id.chars().count() <= 12 {
        peer_id.to_string()
    } else {
        let head: String = peer_id.chars().take(12).collect();
        format!("{head}…")
    }
}

impl PeerSummary {
    fn from_proto(peer: proto::MeshPeer) -> Self {
        Self {
            peer_id: peer.peer_id,
            version: non_empty(peer.version),
            role: non_empty(peer.role),
            vram_bytes: (peer.vram_bytes > 0).then_some(peer.vram_bytes),
            rtt_ms: peer.rtt_ms,
            serving_models: peer.serving_models,
            models: peer.models,
        }
    }

    pub fn short_id(&self) -> String {
        short_id(&self.peer_id)
    }
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One thing that happened, ready to be filtered, coalesced, and rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeEvent {
    pub kind: EventKind,
    /// Endpoint id of the node this plugin is running on.
    pub node_id: String,
    pub mesh_id: Option<String>,
    pub peer: Option<PeerSummary>,
    /// Set only on `model.loaded` / `model.unloaded`.
    pub model: Option<String>,
    /// `MeshEvent.detail_json`, parsed when the host sends one. The host sends
    /// an empty string for every kind today; this carries whatever it starts
    /// sending later rather than dropping it.
    pub detail: Option<Value>,
    pub timestamp_ms: u64,
}

impl NodeEvent {
    /// The identity used for coalescing. A flapping peer produces the same key
    /// every time, which is exactly what lets the coalescer collapse it.
    pub fn coalesce_key(&self) -> String {
        let peer = self
            .peer
            .as_ref()
            .map(|peer| peer.peer_id.as_str())
            .unwrap_or("-");
        let model = self.model.as_deref().unwrap_or("-");
        format!("{}|{peer}|{model}", self.kind.as_str())
    }

    /// Model lists are capped with the same `+N more` sentinel every format
    /// uses, so no payload can be blown past a receiver's size limit by a peer
    /// that happens to serve hundreds of models.
    pub fn peer_json(&self, max_list: usize) -> Option<Value> {
        self.peer.as_ref().map(|peer| {
            json!({
                "peer_id": peer.peer_id,
                "short_id": peer.short_id(),
                "version": peer.version,
                "role": peer.role,
                "vram_bytes": peer.vram_bytes,
                "rtt_ms": peer.rtt_ms,
                "serving_models": crate::format::truncate_list(&peer.serving_models, max_list),
                "models": crate::format::truncate_list(&peer.models, max_list),
            })
        })
    }

    /// The synthetic event behind the `test` MCP tool. It is labelled
    /// `webhook.test` so nobody mistakes it for real mesh activity.
    pub fn test_event(node_id: String, mesh_id: Option<String>, note: Option<String>) -> Self {
        Self {
            kind: EventKind::Test,
            node_id,
            mesh_id,
            peer: None,
            model: None,
            detail: note.map(|note| json!({ "note": note })),
            timestamp_ms: now_ms(),
        }
    }
}

/// One queued webhook POST: an event plus how many identical ones the
/// coalescer swallowed since this key was last delivered.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    pub event: NodeEvent,
    pub suppressed: u32,
}

/// Remembers which models each peer was last seen serving, so `peer.updated`
/// can be turned into `model.loaded` / `model.unloaded`.
///
/// The host never sends a model event to plugins; `serving_models` on a peer
/// update is the only place that information appears. Diffing it is therefore
/// the honest way to report model movement, and it is documented as derived.
#[derive(Debug)]
pub struct ModelTracker {
    serving: BTreeMap<String, BTreeSet<String>>,
    /// Hard cap so a large or hostile mesh cannot grow this map without bound.
    capacity: usize,
}

impl ModelTracker {
    pub fn new(capacity: usize) -> Self {
        Self {
            serving: BTreeMap::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn tracked_peers(&self) -> usize {
        self.serving.len()
    }

    /// Records the current serving set and reports the transition.
    ///
    /// The first sighting of a peer establishes a baseline and reports nothing:
    /// on startup the host replays a `peer_up` for every peer it already knows,
    /// and announcing forty models as "just loaded" would be a lie.
    fn observe(&mut self, peer_id: &str, models: &[String]) -> (Vec<String>, Vec<String>) {
        let current: BTreeSet<String> = models.iter().cloned().collect();
        match self.serving.get_mut(peer_id) {
            Some(previous) => {
                let added = current.difference(previous).cloned().collect();
                let removed = previous.difference(&current).cloned().collect();
                *previous = current;
                (added, removed)
            }
            None => {
                if self.serving.len() >= self.capacity {
                    // Refuse to grow rather than evict someone else's state:
                    // dropping derivation for the newcomer is a smaller lie
                    // than fabricating load/unload churn for an evicted peer.
                    return (Vec::new(), Vec::new());
                }
                self.serving.insert(peer_id.to_string(), current);
                (Vec::new(), Vec::new())
            }
        }
    }

    /// Forgets a peer and reports what it was serving when it disappeared.
    fn forget(&mut self, peer_id: &str) -> Vec<String> {
        self.serving
            .remove(peer_id)
            .map(|models| models.into_iter().collect())
            .unwrap_or_default()
    }
}

/// Normalizes one host mesh event into zero or more outgoing events.
///
/// Zero happens for an unrecognized or unspecified kind: a newer host that adds
/// a seventh kind must not make this plugin emit something it cannot describe.
pub fn translate(
    event: proto::MeshEvent,
    now_ms: u64,
    tracker: &mut ModelTracker,
) -> Vec<NodeEvent> {
    let Ok(kind) = proto::mesh_event::Kind::try_from(event.kind) else {
        return Vec::new();
    };

    let base_kind = match kind {
        proto::mesh_event::Kind::PeerUp => EventKind::PeerUp,
        proto::mesh_event::Kind::PeerDown => EventKind::PeerDown,
        proto::mesh_event::Kind::PeerUpdated => EventKind::PeerUpdated,
        proto::mesh_event::Kind::LocalAccepting => EventKind::NodeAccepting,
        proto::mesh_event::Kind::LocalStandby => EventKind::NodeStandby,
        proto::mesh_event::Kind::MeshIdUpdated => EventKind::MeshIdUpdated,
        proto::mesh_event::Kind::Unspecified => return Vec::new(),
    };

    let peer = event.peer.map(PeerSummary::from_proto);
    let mesh_id = non_empty(event.mesh_id);
    let node_id = event.local_peer_id;
    let detail = parse_detail(&event.detail_json);

    let mut out = vec![NodeEvent {
        kind: base_kind,
        node_id: node_id.clone(),
        mesh_id: mesh_id.clone(),
        peer: peer.clone(),
        model: None,
        detail,
        timestamp_ms: now_ms,
    }];

    let Some(peer) = peer else {
        return out;
    };

    let (loaded, unloaded) = match base_kind {
        EventKind::PeerUp | EventKind::PeerUpdated => {
            tracker.observe(&peer.peer_id, &peer.serving_models)
        }
        // A peer that left is no longer serving anything it was serving. This
        // is reported explicitly so a `model.unloaded`-only filter still sees
        // capacity leaving the mesh.
        EventKind::PeerDown => (Vec::new(), tracker.forget(&peer.peer_id)),
        _ => (Vec::new(), Vec::new()),
    };

    for model in loaded {
        out.push(NodeEvent {
            kind: EventKind::ModelLoaded,
            node_id: node_id.clone(),
            mesh_id: mesh_id.clone(),
            peer: Some(peer.clone()),
            model: Some(model),
            detail: None,
            timestamp_ms: now_ms,
        });
    }
    for model in unloaded {
        out.push(NodeEvent {
            kind: EventKind::ModelUnloaded,
            node_id: node_id.clone(),
            mesh_id: mesh_id.clone(),
            peer: Some(peer.clone()),
            model: Some(model),
            detail: None,
            timestamp_ms: now_ms,
        });
    }
    out
}

/// `detail_json` is a host-controlled free-form string. Keep valid JSON as
/// JSON, keep anything else as a string, and drop the empty case entirely.
fn parse_detail(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_peer(peer_id: &str, serving: &[&str]) -> proto::MeshPeer {
        proto::MeshPeer {
            peer_id: peer_id.to_string(),
            version: "0.72.1".to_string(),
            capabilities: Vec::new(),
            role: "host".to_string(),
            vram_bytes: 24 * 1024 * 1024 * 1024,
            models: serving.iter().map(|model| model.to_string()).collect(),
            serving_models: serving.iter().map(|model| model.to_string()).collect(),
            available_models: Vec::new(),
            requested_models: Vec::new(),
            rtt_ms: Some(12),
            model_source: String::new(),
            hosted_models: Vec::new(),
            hosted_models_known: Some(true),
        }
    }

    fn mesh_event(
        kind: proto::mesh_event::Kind,
        peer: Option<proto::MeshPeer>,
    ) -> proto::MeshEvent {
        proto::MeshEvent {
            kind: kind as i32,
            peer,
            local_peer_id: "local-node".to_string(),
            mesh_id: "mesh-7".to_string(),
            detail_json: String::new(),
        }
    }

    #[test]
    fn every_subscribable_kind_round_trips_through_its_name() {
        for kind in SUBSCRIBABLE {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(EventKind::parse("PEER.UP"), Some(EventKind::PeerUp));
        assert_eq!(EventKind::parse(" peer_down "), Some(EventKind::PeerDown));
        assert_eq!(EventKind::parse("webhook.test"), None);
        assert_eq!(EventKind::parse("nonsense"), None);
    }

    #[test]
    fn unknown_and_unspecified_kinds_produce_nothing() {
        let mut tracker = ModelTracker::new(16);
        let unspecified = mesh_event(proto::mesh_event::Kind::Unspecified, None);
        assert!(translate(unspecified, 0, &mut tracker).is_empty());

        let future_kind = proto::MeshEvent {
            kind: 99,
            ..mesh_event(proto::mesh_event::Kind::PeerUp, None)
        };
        assert!(translate(future_kind, 0, &mut tracker).is_empty());
    }

    #[test]
    fn first_sighting_of_a_peer_is_a_baseline_not_a_load_burst() {
        let mut tracker = ModelTracker::new(16);
        let event = mesh_event(
            proto::mesh_event::Kind::PeerUp,
            Some(mesh_peer("peer-a", &["qwen3-8b", "llama3-70b"])),
        );

        let out = translate(event, 1_000, &mut tracker);

        assert_eq!(out.len(), 1, "only the peer.up itself: {out:?}");
        assert_eq!(out[0].kind, EventKind::PeerUp);
        assert_eq!(out[0].mesh_id.as_deref(), Some("mesh-7"));
        assert_eq!(tracker.tracked_peers(), 1);
    }

    #[test]
    fn later_updates_derive_model_load_and_unload() {
        let mut tracker = ModelTracker::new(16);
        translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUp,
                Some(mesh_peer("peer-a", &["qwen3-8b"])),
            ),
            1_000,
            &mut tracker,
        );

        let out = translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUpdated,
                Some(mesh_peer("peer-a", &["llama3-70b"])),
            ),
            2_000,
            &mut tracker,
        );

        let derived: Vec<(EventKind, Option<&str>)> = out
            .iter()
            .map(|event| (event.kind, event.model.as_deref()))
            .collect();
        assert_eq!(
            derived,
            vec![
                (EventKind::PeerUpdated, None),
                (EventKind::ModelLoaded, Some("llama3-70b")),
                (EventKind::ModelUnloaded, Some("qwen3-8b")),
            ]
        );
    }

    #[test]
    fn an_unchanged_peer_update_derives_no_model_events() {
        let mut tracker = ModelTracker::new(16);
        translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUp,
                Some(mesh_peer("peer-a", &["qwen3-8b"])),
            ),
            1_000,
            &mut tracker,
        );

        let out = translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUpdated,
                Some(mesh_peer("peer-a", &["qwen3-8b"])),
            ),
            2_000,
            &mut tracker,
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::PeerUpdated);
    }

    #[test]
    fn peer_down_unloads_everything_it_was_serving_and_forgets_it() {
        let mut tracker = ModelTracker::new(16);
        translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUp,
                Some(mesh_peer("peer-a", &["a", "b"])),
            ),
            1_000,
            &mut tracker,
        );

        let out = translate(
            mesh_event(
                proto::mesh_event::Kind::PeerDown,
                Some(mesh_peer("peer-a", &["a", "b"])),
            ),
            2_000,
            &mut tracker,
        );

        assert_eq!(out[0].kind, EventKind::PeerDown);
        let unloaded: Vec<&str> = out[1..].iter().filter_map(|e| e.model.as_deref()).collect();
        assert_eq!(unloaded, vec!["a", "b"]);
        assert_eq!(tracker.tracked_peers(), 0);
    }

    #[test]
    fn the_tracker_refuses_to_grow_past_its_capacity() {
        let mut tracker = ModelTracker::new(2);
        for index in 0..10 {
            translate(
                mesh_event(
                    proto::mesh_event::Kind::PeerUp,
                    Some(mesh_peer(&format!("peer-{index}"), &["m"])),
                ),
                1_000,
                &mut tracker,
            );
        }
        assert_eq!(tracker.tracked_peers(), 2);
    }

    #[test]
    fn coalesce_keys_separate_peers_kinds_and_models() {
        let mut tracker = ModelTracker::new(16);
        let up_a = translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUp,
                Some(mesh_peer("peer-a", &[])),
            ),
            0,
            &mut tracker,
        );
        let up_b = translate(
            mesh_event(
                proto::mesh_event::Kind::PeerUp,
                Some(mesh_peer("peer-b", &[])),
            ),
            0,
            &mut tracker,
        );
        assert_ne!(up_a[0].coalesce_key(), up_b[0].coalesce_key());
        assert_eq!(up_a[0].coalesce_key(), "peer.up|peer-a|-");
    }

    #[test]
    fn detail_json_keeps_json_as_json_and_junk_as_text() {
        assert_eq!(parse_detail(""), None);
        assert_eq!(parse_detail("   "), None);
        assert_eq!(parse_detail(r#"{"a":1}"#), Some(json!({"a": 1})));
        assert_eq!(parse_detail("not json"), Some(json!("not json")));
    }

    #[test]
    fn short_id_truncates_only_when_it_helps() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("0123456789ab"), "0123456789ab");
        assert_eq!(short_id("0123456789abcdef"), "0123456789ab…");
    }

    #[test]
    fn empty_host_strings_become_none_rather_than_empty_fields() {
        let mut tracker = ModelTracker::new(4);
        let mut raw = mesh_event(
            proto::mesh_event::Kind::PeerUp,
            Some(mesh_peer("peer-a", &[])),
        );
        raw.mesh_id = String::new();
        if let Some(peer) = raw.peer.as_mut() {
            peer.version = String::new();
            peer.role = String::new();
            peer.vram_bytes = 0;
        }

        let out = translate(raw, 0, &mut tracker);
        let peer = out[0].peer.as_ref().expect("peer present");

        assert_eq!(out[0].mesh_id, None);
        assert_eq!(peer.version, None);
        assert_eq!(peer.role, None);
        assert_eq!(peer.vram_bytes, None);
    }
}
