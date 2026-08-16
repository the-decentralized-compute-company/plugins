//! Mesh advertisement: telling peers what this node holds, and remembering
//! what they say they hold.
//!
//! Two message kinds on one declared channel, `model-mirror.v1`:
//!
//! | `message_kind`      | Direction | Body |
//! | ------------------- | --------- | ---- |
//! | `inventory_request` | ask       | `{}` |
//! | `inventory`         | answer    | [`InventoryPayload`] |
//!
//! An inventory is a **claim**, not evidence. Two peers advertising different
//! digests for the same canonical ref means at least one of them is wrong, and
//! [`PeerDirectory`] surfaces that as a [`DigestConflict`] rather than picking
//! a winner — the mirror is not a trust root and will not pretend to be one.
//! Whoever downloads still verifies against the digest *they* pinned.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactId;
use crate::cache::MirrorEntry;

/// The one mesh channel this plugin declares.
pub const CHANNEL: &str = "model-mirror.v1";

pub const KIND_INVENTORY_REQUEST: &str = "inventory_request";
pub const KIND_INVENTORY: &str = "inventory";

/// Cap on how many artifacts one announcement may carry.
///
/// A peer that claims to hold a hundred thousand artifacts is either broken or
/// hostile; either way this node is not going to allocate for it.
pub const MAX_ADVERTISED_ARTIFACTS: usize = 512;

/// One artifact as advertised to peers.
///
/// The field names deliberately match `model_artifact::ModelArtifactFile`
/// (`path`, `size_bytes`, `sha256`) plus the canonical ref, so a consumer that
/// already understands host artifact metadata understands this too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisedArtifact {
    pub canonical_ref: String,
    /// Repo-relative file path — the `path` of the matching `ModelArtifactFile`.
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
}

impl From<&MirrorEntry> for AdvertisedArtifact {
    fn from(entry: &MirrorEntry) -> Self {
        Self {
            canonical_ref: entry.canonical_ref.clone(),
            path: entry.file.clone(),
            size_bytes: entry.size_bytes,
            sha256: entry.sha256.to_string(),
            model_id: Some(entry.model_id.clone()),
            selector: entry.selector.clone(),
            distribution_id: entry.distribution_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryPayload {
    pub artifacts: Vec<AdvertisedArtifact>,
    /// False when the node is configured to hold nothing; peers should not ask
    /// it for bytes.
    #[serde(default)]
    pub serving: bool,
    /// Largest chunk this node will return from `read_chunk`.
    #[serde(default)]
    pub max_chunk_bytes: u64,
}

/// What the plugin should do with an inbound channel message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelAction {
    /// Answer the sender with this node's inventory.
    ReplyWithInventory,
    /// Record the sender's inventory.
    Record(InventoryPayload),
    /// Nothing addressed to this plugin; stay quiet.
    Ignore,
}

/// Decide what an inbound message means, without touching the network.
///
/// Malformed bodies are `Ignore` rather than errors: a peer running an older
/// or newer build should not be able to make this node log-spam or fail a
/// handler, and there is nothing useful to reply with anyway.
pub fn plan_channel_action(channel: &str, message_kind: &str, body: &[u8]) -> ChannelAction {
    if channel != CHANNEL {
        return ChannelAction::Ignore;
    }
    match message_kind {
        KIND_INVENTORY_REQUEST => ChannelAction::ReplyWithInventory,
        KIND_INVENTORY => match serde_json::from_slice::<InventoryPayload>(body) {
            Ok(payload) => ChannelAction::Record(sanitize_inventory(payload)),
            Err(_) => ChannelAction::Ignore,
        },
        _ => ChannelAction::Ignore,
    }
}

/// Drop anything in a peer's announcement this node will not act on.
///
/// Every advertised ref is re-parsed with the same validator used for local
/// requests, so a peer cannot inject a ref shape that would be refused
/// elsewhere, and the list is truncated to [`MAX_ADVERTISED_ARTIFACTS`].
pub fn sanitize_inventory(mut payload: InventoryPayload) -> InventoryPayload {
    payload.artifacts.retain(|artifact| {
        ArtifactId::parse(&artifact.canonical_ref).is_ok()
            && crate::digest::Sha256Hex::parse(&artifact.sha256).is_ok()
    });
    payload.artifacts.truncate(MAX_ADVERTISED_ARTIFACTS);
    payload
}

/// A peer advertising a digest that disagrees with what someone else says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DigestConflict {
    pub canonical_ref: String,
    pub peer_id: String,
    pub peer_sha256: String,
    /// Who we heard the other digest from — `"this node"` when it is local.
    pub conflicts_with: String,
    pub other_sha256: String,
}

/// Where each peer's advertised holdings are remembered.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PeerInventory {
    pub peer_id: String,
    pub serving: bool,
    pub max_chunk_bytes: u64,
    pub artifacts: Vec<AdvertisedArtifact>,
    pub updated_at: u64,
}

/// Peer holdings, plus the digest disagreements they imply.
#[derive(Debug, Default)]
pub struct PeerDirectory {
    peers: BTreeMap<String, PeerInventory>,
}

impl PeerDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one peer's inventory and report every digest disagreement it
    /// creates, against local entries and against other peers.
    pub fn record(
        &mut self,
        peer_id: &str,
        payload: InventoryPayload,
        local: &[AdvertisedArtifact],
        now: u64,
    ) -> Vec<DigestConflict> {
        let mut conflicts = Vec::new();
        for artifact in &payload.artifacts {
            if let Some(local_match) = local
                .iter()
                .find(|entry| entry.canonical_ref == artifact.canonical_ref)
                && local_match.sha256 != artifact.sha256
            {
                conflicts.push(DigestConflict {
                    canonical_ref: artifact.canonical_ref.clone(),
                    peer_id: peer_id.to_string(),
                    peer_sha256: artifact.sha256.clone(),
                    conflicts_with: "this node".to_string(),
                    other_sha256: local_match.sha256.clone(),
                });
            }
            for (other_id, other) in &self.peers {
                if other_id == peer_id {
                    continue;
                }
                if let Some(other_match) = other
                    .artifacts
                    .iter()
                    .find(|entry| entry.canonical_ref == artifact.canonical_ref)
                    && other_match.sha256 != artifact.sha256
                {
                    conflicts.push(DigestConflict {
                        canonical_ref: artifact.canonical_ref.clone(),
                        peer_id: peer_id.to_string(),
                        peer_sha256: artifact.sha256.clone(),
                        conflicts_with: other_id.clone(),
                        other_sha256: other_match.sha256.clone(),
                    });
                }
            }
        }

        self.peers.insert(
            peer_id.to_string(),
            PeerInventory {
                peer_id: peer_id.to_string(),
                serving: payload.serving,
                max_chunk_bytes: payload.max_chunk_bytes,
                artifacts: payload.artifacts,
                updated_at: now,
            },
        );
        conflicts
    }

    pub fn forget(&mut self, peer_id: &str) {
        self.peers.remove(peer_id);
    }

    pub fn peers(&self) -> Vec<PeerInventory> {
        self.peers.values().cloned().collect()
    }

    /// Peers advertising a given artifact, most useful first is not implied —
    /// the caller picks, and always verifies against its own pinned digest.
    pub fn holders(&self, canonical_ref: &str) -> Vec<(String, AdvertisedArtifact)> {
        self.peers
            .values()
            .filter(|peer| peer.serving)
            .filter_map(|peer| {
                peer.artifacts
                    .iter()
                    .find(|artifact| artifact.canonical_ref == canonical_ref)
                    .map(|artifact| (peer.peer_id.clone(), artifact.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(canonical_ref: &str, digest_seed: &[u8]) -> AdvertisedArtifact {
        AdvertisedArtifact {
            canonical_ref: canonical_ref.to_string(),
            path: "Model-Q4_K_M.gguf".to_string(),
            size_bytes: 10,
            sha256: crate::digest::Sha256Hex::of_bytes(digest_seed).to_string(),
            model_id: None,
            selector: None,
            distribution_id: None,
        }
    }

    const REF: &str = "org/repo@abc123/Model-Q4_K_M.gguf";

    #[test]
    fn a_request_asks_for_a_reply_and_an_inventory_is_recorded() {
        assert_eq!(
            plan_channel_action(CHANNEL, KIND_INVENTORY_REQUEST, b""),
            ChannelAction::ReplyWithInventory
        );

        let payload = InventoryPayload {
            artifacts: vec![artifact(REF, b"genuine")],
            serving: true,
            max_chunk_bytes: 1024,
        };
        let body = serde_json::to_vec(&payload).expect("payload serializes");

        assert_eq!(
            plan_channel_action(CHANNEL, KIND_INVENTORY, &body),
            ChannelAction::Record(payload)
        );
    }

    #[test]
    fn traffic_on_another_channel_or_of_another_kind_is_ignored() {
        assert_eq!(
            plan_channel_action("something.else", KIND_INVENTORY_REQUEST, b""),
            ChannelAction::Ignore
        );
        assert_eq!(
            plan_channel_action(CHANNEL, "gossip", b"{}"),
            ChannelAction::Ignore
        );
    }

    #[test]
    fn a_malformed_inventory_is_ignored_rather_than_raised() {
        assert_eq!(
            plan_channel_action(CHANNEL, KIND_INVENTORY, b"not json at all"),
            ChannelAction::Ignore
        );
    }

    #[test]
    fn sanitizing_drops_refs_and_digests_this_node_would_not_accept() {
        let payload = InventoryPayload {
            artifacts: vec![
                artifact(REF, b"genuine"),
                artifact("org/repo@abc123/../../etc/passwd", b"hostile"),
                AdvertisedArtifact {
                    sha256: "not-a-digest".to_string(),
                    ..artifact("org/other@def456/Model.gguf", b"x")
                },
            ],
            serving: true,
            max_chunk_bytes: 1024,
        };

        let sanitized = sanitize_inventory(payload);

        assert_eq!(sanitized.artifacts.len(), 1);
        assert_eq!(sanitized.artifacts[0].canonical_ref, REF);
    }

    #[test]
    fn sanitizing_truncates_an_implausibly_large_announcement() {
        let payload = InventoryPayload {
            artifacts: (0..MAX_ADVERTISED_ARTIFACTS + 50)
                .map(|index| artifact(&format!("org/repo@abc123/model-{index}.gguf"), b"x"))
                .collect(),
            serving: true,
            max_chunk_bytes: 1024,
        };

        assert_eq!(
            sanitize_inventory(payload).artifacts.len(),
            MAX_ADVERTISED_ARTIFACTS
        );
    }

    #[test]
    fn a_peer_disagreeing_with_this_node_is_reported_not_resolved() {
        let mut directory = PeerDirectory::new();
        let local = vec![artifact(REF, b"genuine")];

        let conflicts = directory.record(
            "peer-1",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"substituted")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &local,
            42,
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].peer_id, "peer-1");
        assert_eq!(conflicts[0].conflicts_with, "this node");
        assert_eq!(conflicts[0].canonical_ref, REF);
        // The peer's claim is still recorded: the operator gets to see both
        // sides rather than having one silently discarded.
        assert_eq!(directory.peers().len(), 1);
    }

    #[test]
    fn two_peers_disagreeing_with_each_other_are_reported() {
        let mut directory = PeerDirectory::new();
        directory.record(
            "peer-1",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"one")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &[],
            1,
        );

        let conflicts = directory.record(
            "peer-2",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"two")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &[],
            2,
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].peer_id, "peer-2");
        assert_eq!(conflicts[0].conflicts_with, "peer-1");
    }

    #[test]
    fn agreement_produces_no_conflicts() {
        let mut directory = PeerDirectory::new();
        let local = vec![artifact(REF, b"genuine")];
        directory.record(
            "peer-1",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"genuine")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &local,
            1,
        );

        let conflicts = directory.record(
            "peer-2",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"genuine")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &local,
            2,
        );

        assert!(conflicts.is_empty());
    }

    #[test]
    fn holders_lists_only_peers_that_are_actually_serving() {
        let mut directory = PeerDirectory::new();
        directory.record(
            "serving-peer",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"genuine")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &[],
            1,
        );
        directory.record(
            "idle-peer",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"genuine")],
                serving: false,
                max_chunk_bytes: 0,
            },
            &[],
            1,
        );

        let holders = directory.holders(REF);

        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].0, "serving-peer");
        assert!(directory.holders("org/repo@abc123/absent.gguf").is_empty());
    }

    #[test]
    fn a_peer_that_leaves_is_forgotten() {
        let mut directory = PeerDirectory::new();
        directory.record(
            "peer-1",
            InventoryPayload {
                artifacts: vec![artifact(REF, b"genuine")],
                serving: true,
                max_chunk_bytes: 1024,
            },
            &[],
            1,
        );

        directory.forget("peer-1");

        assert!(directory.peers().is_empty());
        assert!(directory.holders(REF).is_empty());
    }
}
