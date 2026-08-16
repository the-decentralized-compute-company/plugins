//! The whole contribution surface of `model-mirror` in one declaration.
//!
//! Everything mutating is an MCP tool. The three HTTP routes are read-only, so
//! a stray `GET` from a console page or a curl loop can never change what this
//! node holds.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks. Omitting a field is fine; reordering is not.
//!
//! There is deliberately no `config` block — see [`crate::options`] for why
//! operator limits live in `[[plugin]].args` instead of `[plugin.settings]`.

use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{
    PluginContext, PluginMetadata, SimplePlugin, capability, events, http, mcp, mesh, plugin,
    plugin_server_info, proto,
};

use crate::announce::{
    self, AdvertisedArtifact, ChannelAction, InventoryPayload, PeerDirectory, PeerInventory,
};
use crate::cache::{
    ChunkResponse, EvictReport, ImportReport, MirrorCache, MirrorEntry, ReceiveProgress,
    StatusReport, VerifyReport, epoch_secs,
};

pub const PLUGIN_NAME: &str = "model-mirror";
pub const PLUGIN_VERSION: &str = "0.1.0";

/// Default and maximum page size for `list`.
const DEFAULT_LIST_LIMIT: u32 = 100;
const MAX_LIST_LIMIT: u32 = 500;

/// Shared peer directory. A plain `Mutex` because it is only ever held for the
/// length of a map update, never across an await.
type Peers = Arc<Mutex<PeerDirectory>>;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Include artifacts that failed an integrity check and were quarantined.
    #[serde(default)]
    include_quarantined: bool,
    /// Maximum number of artifacts to return. Defaults to 100, capped at 500.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportArgs {
    /// Absolute path of the file to take into the mirror. It must resolve
    /// inside one of this node's configured import roots; symlinks are followed
    /// before the check, so a link out of the root is refused.
    path: String,
    /// Canonical artifact ref, `org/repo@revision/file`. Required unless `path`
    /// sits in a Hugging Face snapshot layout the mirror can read the identity
    /// out of.
    #[serde(default)]
    canonical_ref: Option<String>,
    /// SHA-256 the file must hash to, as 64 hex characters. Supply it whenever
    /// you know it: without it the mirror can only certify that the bytes have
    /// not changed since import, not that they were ever the right bytes.
    #[serde(default)]
    expected_sha256: Option<String>,
    /// Keep this artifact through eviction.
    #[serde(default)]
    pin: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadChunkArgs {
    /// Canonical artifact ref, `org/repo@revision/file`.
    canonical_ref: String,
    /// Byte offset to read from. To resume an interrupted transfer, pass how
    /// many bytes you already hold.
    #[serde(default)]
    offset: u64,
    /// Bytes to read. Clamped to this node's chunk limit; omit to let the
    /// mirror choose. The response may be shorter when the bandwidth budget
    /// trims it — ask again from the new offset.
    #[serde(default)]
    length: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BeginReceiveArgs {
    /// Canonical artifact ref, `org/repo@revision/file`.
    canonical_ref: String,
    /// SHA-256 the completed artifact must hash to, as 64 hex characters. This
    /// is the digest you are pinning: the transfer is only published if the
    /// assembled file matches it exactly.
    expected_sha256: String,
    /// Total size of the artifact in bytes, so the mirror can reserve space
    /// before any bytes move.
    total_bytes: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReceiveChunkArgs {
    /// Canonical artifact ref, `org/repo@revision/file`.
    canonical_ref: String,
    /// Byte offset this chunk starts at. Transfers are append-only, so this
    /// must equal the bytes already received.
    offset: u64,
    /// Chunk payload, base64 encoded.
    data_base64: String,
    /// SHA-256 of this chunk's decoded bytes. Optional, and worth sending: it
    /// catches corruption at the chunk instead of after the whole artifact.
    #[serde(default)]
    chunk_sha256: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefArgs {
    /// Canonical artifact ref, `org/repo@revision/file`.
    canonical_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PinArgs {
    /// Canonical artifact ref, `org/repo@revision/file`.
    canonical_ref: String,
    /// True to keep the artifact through eviction, false to release it.
    pinned: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvictArgs {
    /// Drop exactly this artifact.
    #[serde(default)]
    canonical_ref: Option<String>,
    /// Drop least-recently-served artifacts until at least this many bytes are
    /// free. Ignored when `canonical_ref` is given.
    #[serde(default)]
    reclaim_bytes: Option<u64>,
    /// Also drop pinned artifacts. Off by default, because pinning is how an
    /// operator says "not this one".
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub total: usize,
    pub returned: usize,
    pub artifacts: Vec<MirrorEntry>,
}

#[derive(Debug, Serialize)]
pub struct PeersResponse {
    pub peers: Vec<PeerInventory>,
    /// Peers whose advertised digests disagree with this node's or with each
    /// other's. A non-empty list here means somebody is serving the wrong
    /// bytes; it is not resolved automatically.
    pub digest_conflicts: Vec<announce::DigestConflict>,
}

#[derive(Debug, Serialize)]
pub struct PinResponse {
    pub artifact: MirrorEntry,
}

#[derive(Debug, Serialize)]
pub struct FindResponse {
    pub canonical_ref: String,
    /// True when this node already holds a verified copy.
    pub held_locally: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_sha256: Option<String>,
    /// Peers advertising the artifact. Their digests are claims: verify against
    /// the digest you pinned, not against theirs.
    pub peers: Vec<FoundOnPeer>,
    /// True when the peers do not agree on the digest. Pick a source only after
    /// deciding which digest is correct.
    pub peers_disagree: bool,
}

#[derive(Debug, Serialize)]
pub struct FoundOnPeer {
    pub peer_id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT) as usize
}

/// This node's holdings, in the shape peers and the console both read.
pub fn inventory_payload(cache: &MirrorCache) -> InventoryPayload {
    InventoryPayload {
        artifacts: cache
            .ready_entries()
            .iter()
            .map(AdvertisedArtifact::from)
            .collect(),
        serving: cache.options().holds_artifacts(),
        max_chunk_bytes: cache.options().max_chunk_bytes,
    }
}

fn lock_peers(peers: &Peers) -> std::sync::MutexGuard<'_, PeerDirectory> {
    // A poisoned lock means a handler panicked mid-update. The directory is
    // advisory, rebuilt from the next announcement, so recovering it keeps the
    // plugin usable rather than failing every later message.
    peers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Announce this node's inventory to one peer, or to every peer when
/// `target_peer_id` is empty.
async fn announce_inventory(
    cache: &MirrorCache,
    context: &mut PluginContext<'_>,
    target_peer_id: &str,
) -> anyhow::Result<()> {
    if !cache.options().advertise {
        return Ok(());
    }
    let payload = inventory_payload(cache);
    context
        .send_json_channel(
            announce::CHANNEL,
            target_peer_id.to_string(),
            announce::KIND_INVENTORY,
            &payload,
        )
        .await
}

async fn handle_channel_message(
    cache: &MirrorCache,
    peers: &Peers,
    message: proto::ChannelMessage,
    context: &mut PluginContext<'_>,
) -> anyhow::Result<()> {
    match announce::plan_channel_action(&message.channel, &message.message_kind, &message.body) {
        ChannelAction::ReplyWithInventory => {
            if !cache.options().advertise {
                return Ok(());
            }
            let payload = inventory_payload(cache);
            let reply = tdcc_plugin::json_reply_channel_message(
                &message,
                announce::KIND_INVENTORY,
                &payload,
            )?;
            context.send_channel_message(reply).await
        }
        ChannelAction::Record(payload) => {
            let local: Vec<AdvertisedArtifact> = cache
                .ready_entries()
                .iter()
                .map(AdvertisedArtifact::from)
                .collect();
            let conflicts =
                lock_peers(peers).record(&message.source_peer_id, payload, &local, epoch_secs());
            for conflict in &conflicts {
                // Loud on purpose. A digest disagreement is the signature of a
                // mirror serving something other than what it claims.
                eprintln!(
                    "model-mirror: DIGEST CONFLICT for {} — {} advertises {}, {} has {}",
                    conflict.canonical_ref,
                    conflict.peer_id,
                    conflict.peer_sha256,
                    conflict.conflicts_with,
                    conflict.other_sha256
                );
            }
            Ok(())
        }
        ChannelAction::Ignore => Ok(()),
    }
}

async fn handle_mesh_event(
    cache: &MirrorCache,
    peers: &Peers,
    event: proto::MeshEvent,
    context: &mut PluginContext<'_>,
) -> anyhow::Result<()> {
    let peer_id = event
        .peer
        .as_ref()
        .map(|peer| peer.peer_id.clone())
        .unwrap_or_default();
    if event.kind == proto::mesh_event::Kind::PeerUp as i32 && !peer_id.is_empty() {
        return announce_inventory(cache, context, &peer_id).await;
    }
    if event.kind == proto::mesh_event::Kind::PeerDown as i32 && !peer_id.is_empty() {
        lock_peers(peers).forget(&peer_id);
    }
    Ok(())
}

pub fn model_mirror_plugin(cache: MirrorCache) -> SimplePlugin {
    let peers: Peers = Arc::new(Mutex::new(PeerDirectory::new()));

    let cache_for_status = cache.clone();
    let cache_for_list = cache.clone();
    let cache_for_import = cache.clone();
    let cache_for_read = cache.clone();
    let cache_for_begin = cache.clone();
    let cache_for_receive = cache.clone();
    let cache_for_finalize = cache.clone();
    let cache_for_abort = cache.clone();
    let cache_for_verify = cache.clone();
    let cache_for_pin = cache.clone();
    let cache_for_evict = cache.clone();
    let cache_for_peers = cache.clone();
    let cache_for_find = cache.clone();
    let cache_for_http_status = cache.clone();
    let cache_for_http_inventory = cache.clone();
    let cache_for_http_chunk = cache.clone();
    let cache_for_health = cache.clone();
    let cache_for_init = cache.clone();
    let cache_for_channel = cache.clone();
    let cache_for_event = cache;

    let peers_for_peers = Arc::clone(&peers);
    let peers_for_find = Arc::clone(&peers);
    let peers_for_channel = Arc::clone(&peers);
    let peers_for_event = peers;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Model mirror",
                "Caches model artifacts on this node and serves them to mesh peers, \
                 digest-verified on write and on read",
                None::<String>,
            ),
        ),

        // A stable contract another component can depend on by name instead of
        // by plugin id.
        provides: [capability("model-mirror.v1")],

        mesh: [mesh::channel(announce::CHANNEL)],

        // Only what the advertisement loop needs: greet a peer that arrives,
        // forget one that leaves.
        events: [events::peer_up(), events::peer_down()],

        mcp: [
            mcp::tool("status")
                .description(
                    "Report this mirror's disk cap, current usage, bandwidth budget, and \
                     whether it is serving at all.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let cache = cache_for_status.clone();
                    Box::pin(async move { Ok::<StatusReport, tdcc_plugin::PluginError>(cache.status().await) })
                }),

            mcp::tool("list")
                .description("List the model artifacts this node holds, with their digests.")
                .input::<ListArgs>()
                .handle(move |args: ListArgs, _context| {
                    let cache = cache_for_list.clone();
                    Box::pin(async move {
                        let all = if args.include_quarantined {
                            cache.entries()
                        } else {
                            cache.ready_entries()
                        };
                        let limit = clamp_limit(args.limit);
                        let total = all.len();
                        let artifacts: Vec<MirrorEntry> = all.into_iter().take(limit).collect();
                        Ok::<ListResponse, tdcc_plugin::PluginError>(ListResponse {
                            total,
                            returned: artifacts.len(),
                            artifacts,
                        })
                    })
                }),

            mcp::tool("peers")
                .description(
                    "Show what each mesh peer advertises, and any digest disagreements \
                     between them.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let cache = cache_for_peers.clone();
                    let peers = Arc::clone(&peers_for_peers);
                    Box::pin(async move {
                        let local: Vec<AdvertisedArtifact> = cache
                            .ready_entries()
                            .iter()
                            .map(AdvertisedArtifact::from)
                            .collect();
                        let (listed, conflicts) = {
                            let directory = lock_peers(&peers);
                            (directory.peers(), conflicts_against_local(&directory, &local))
                        };
                        Ok::<PeersResponse, tdcc_plugin::PluginError>(PeersResponse {
                            peers: listed,
                            digest_conflicts: conflicts,
                        })
                    })
                }),

            mcp::tool("find")
                .description(
                    "Find which mesh peers advertise an artifact, and whether they agree \
                     on its digest.",
                )
                .input::<RefArgs>()
                .handle(move |args: RefArgs, _context| {
                    let cache = cache_for_find.clone();
                    let peers = Arc::clone(&peers_for_find);
                    Box::pin(async move {
                        let local = cache
                            .ready_entries()
                            .into_iter()
                            .find(|entry| entry.canonical_ref == args.canonical_ref);
                        let holders = lock_peers(&peers).holders(&args.canonical_ref);
                        let found: Vec<FoundOnPeer> = holders
                            .into_iter()
                            .map(|(peer_id, artifact)| FoundOnPeer {
                                peer_id,
                                sha256: artifact.sha256,
                                size_bytes: artifact.size_bytes,
                            })
                            .collect();
                        let peers_disagree = found.first().is_some_and(|first| {
                            found.iter().any(|other| other.sha256 != first.sha256)
                        });
                        Ok::<FindResponse, tdcc_plugin::PluginError>(FindResponse {
                            canonical_ref: args.canonical_ref,
                            held_locally: local.is_some(),
                            local_sha256: local.map(|entry| entry.sha256.to_string()),
                            peers: found,
                            peers_disagree,
                        })
                    })
                }),

            mcp::tool("import")
                .description(
                    "Take a model file this node already has on disk into the mirror, \
                     digesting it on the way in. The source must be inside a configured \
                     import root.",
                )
                .input::<ImportArgs>()
                .handle(move |args: ImportArgs, _context| {
                    let cache = cache_for_import.clone();
                    Box::pin(async move {
                        Ok::<ImportReport, tdcc_plugin::PluginError>(
                            cache
                                .import(
                                    std::path::Path::new(&args.path),
                                    args.canonical_ref.as_deref(),
                                    args.expected_sha256.as_deref(),
                                    args.pin,
                                )
                                .await?,
                        )
                    })
                }),

            mcp::tool("read_chunk")
                .description(
                    "Read one range of a held artifact, base64 encoded, with the digest of \
                     that range and of the whole artifact. Resume by asking for the next \
                     offset.",
                )
                .input::<ReadChunkArgs>()
                .handle(move |args: ReadChunkArgs, _context| {
                    let cache = cache_for_read.clone();
                    Box::pin(async move {
                        Ok::<ChunkResponse, tdcc_plugin::PluginError>(
                            cache
                                .read_chunk(&args.canonical_ref, args.offset, args.length)
                                .await?,
                        )
                    })
                }),

            mcp::tool("begin_receive")
                .description(
                    "Open or resume an inbound transfer, reserving disk for it and \
                     reporting how many bytes are already staged.",
                )
                .input::<BeginReceiveArgs>()
                .handle(move |args: BeginReceiveArgs, _context| {
                    let cache = cache_for_begin.clone();
                    Box::pin(async move {
                        Ok::<ReceiveProgress, tdcc_plugin::PluginError>(
                            cache
                                .begin_receive(
                                    &args.canonical_ref,
                                    &args.expected_sha256,
                                    args.total_bytes,
                                )
                                .await?,
                        )
                    })
                }),

            mcp::tool("receive_chunk")
                .description(
                    "Append one chunk to an open transfer and report progress. Transfers \
                     are append-only.",
                )
                .input::<ReceiveChunkArgs>()
                .handle(move |args: ReceiveChunkArgs, _context| {
                    let cache = cache_for_receive.clone();
                    Box::pin(async move {
                        Ok::<ReceiveProgress, tdcc_plugin::PluginError>(
                            cache
                                .receive_chunk(
                                    &args.canonical_ref,
                                    args.offset,
                                    &args.data_base64,
                                    args.chunk_sha256.as_deref(),
                                )
                                .await?,
                        )
                    })
                }),

            mcp::tool("finalize_receive")
                .description(
                    "Digest a completed transfer and publish it only if it matches the \
                     digest declared at begin_receive. A mismatch discards the staged copy.",
                )
                .input::<RefArgs>()
                .handle(move |args: RefArgs, _context| {
                    let cache = cache_for_finalize.clone();
                    Box::pin(async move {
                        Ok::<ImportReport, tdcc_plugin::PluginError>(
                            cache.finalize_receive(&args.canonical_ref).await?,
                        )
                    })
                }),

            mcp::tool("abort_receive")
                .description("Discard a partial transfer and free its staged bytes.")
                .input::<RefArgs>()
                .handle(move |args: RefArgs, _context| {
                    let cache = cache_for_abort.clone();
                    Box::pin(async move {
                        Ok::<ReceiveProgress, tdcc_plugin::PluginError>(
                            cache.abort_receive(&args.canonical_ref).await?,
                        )
                    })
                }),

            mcp::tool("verify")
                .description(
                    "Re-digest a held artifact end to end. A mismatch quarantines it so it \
                     stops being served and stops being advertised.",
                )
                .input::<RefArgs>()
                .handle(move |args: RefArgs, _context| {
                    let cache = cache_for_verify.clone();
                    Box::pin(async move {
                        Ok::<VerifyReport, tdcc_plugin::PluginError>(
                            cache.verify(&args.canonical_ref).await?,
                        )
                    })
                }),

            mcp::tool("pin")
                .description("Pin or unpin an artifact so eviction does or does not consider it.")
                .input::<PinArgs>()
                .handle(move |args: PinArgs, _context| {
                    let cache = cache_for_pin.clone();
                    Box::pin(async move {
                        Ok::<PinResponse, tdcc_plugin::PluginError>(PinResponse {
                            artifact: cache
                                .set_pinned(&args.canonical_ref, args.pinned)
                                .await?,
                        })
                    })
                }),

            mcp::tool("evict")
                .description(
                    "Drop a named artifact, or drop least-recently-served artifacts until \
                     the requested number of bytes is free.",
                )
                .input::<EvictArgs>()
                .handle(move |args: EvictArgs, _context| {
                    let cache = cache_for_evict.clone();
                    Box::pin(async move {
                        Ok::<EvictReport, tdcc_plugin::PluginError>(
                            cache
                                .evict(
                                    args.canonical_ref.as_deref(),
                                    args.reclaim_bytes,
                                    args.force,
                                )
                                .await?,
                        )
                    })
                }),
        ],

        // Read-only routes only. Everything that changes state is an MCP tool,
        // so a stray GET cannot admit, evict, or quarantine anything.
        http: [
            // GET /api/plugins/model-mirror/http/status
            http::get("/status")
                .description("This mirror's caps, usage, and bandwidth budget.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let cache = cache_for_http_status.clone();
                    Box::pin(async move { Ok::<StatusReport, tdcc_plugin::PluginError>(cache.status().await) })
                }),

            // GET /api/plugins/model-mirror/http/inventory
            http::get("/inventory")
                .description("What this node advertises to peers.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let cache = cache_for_http_inventory.clone();
                    Box::pin(async move {
                        Ok::<InventoryPayload, tdcc_plugin::PluginError>(inventory_payload(&cache))
                    })
                }),

            // GET /api/plugins/model-mirror/http/chunk?canonical_ref=…&offset=…
            http::get("/chunk")
                .description("Read one range of a held artifact, base64 encoded.")
                .input::<ReadChunkArgs>()
                .handle(move |args: ReadChunkArgs, _context| {
                    let cache = cache_for_http_chunk.clone();
                    Box::pin(async move {
                        Ok::<ChunkResponse, tdcc_plugin::PluginError>(
                            cache
                                .read_chunk(&args.canonical_ref, args.offset, args.length)
                                .await?,
                        )
                    })
                }),
        ],

        // Health must stay fast and independent of long-running work: it reads
        // the in-memory index and never touches the disk or a digest.
        health: move |_context: &mut PluginContext<'_>| {
            let cache = cache_for_health.clone();
            Box::pin(async move {
                let ready = cache.ready_entries().len();
                let options = cache.options();
                Ok(if options.holds_artifacts() {
                    format!("serving {ready} artifacts")
                } else {
                    "idle: max_cache_bytes is 0".to_string()
                })
            })
        },

        on_initialized: move |context: &mut PluginContext<'_>| {
            let cache = cache_for_init.clone();
            Box::pin(async move {
                // An empty target means "every peer": tell the mesh what is
                // here as soon as this node joins.
                announce_inventory(&cache, context, "").await
            })
        },

        on_channel_message: move |message: proto::ChannelMessage, context: &mut PluginContext<'_>| {
            let cache = cache_for_channel.clone();
            let peers = Arc::clone(&peers_for_channel);
            Box::pin(async move { handle_channel_message(&cache, &peers, message, context).await })
        },

        on_mesh_event: move |event: proto::MeshEvent, context: &mut PluginContext<'_>| {
            let cache = cache_for_event.clone();
            let peers = Arc::clone(&peers_for_event);
            Box::pin(async move { handle_mesh_event(&cache, &peers, event, context).await })
        },
    }
}

/// Recompute the digest disagreements currently implied by the directory.
///
/// Conflicts are derived on demand rather than cached, so an operator reading
/// `peers` sees the state as it is now, not a log of what it once was.
fn conflicts_against_local(
    directory: &PeerDirectory,
    local: &[AdvertisedArtifact],
) -> Vec<announce::DigestConflict> {
    let mut conflicts = Vec::new();
    let peers = directory.peers();
    for (index, peer) in peers.iter().enumerate() {
        for artifact in &peer.artifacts {
            if let Some(local_match) = local
                .iter()
                .find(|entry| entry.canonical_ref == artifact.canonical_ref)
                && local_match.sha256 != artifact.sha256
            {
                conflicts.push(announce::DigestConflict {
                    canonical_ref: artifact.canonical_ref.clone(),
                    peer_id: peer.peer_id.clone(),
                    peer_sha256: artifact.sha256.clone(),
                    conflicts_with: "this node".to_string(),
                    other_sha256: local_match.sha256.clone(),
                });
            }
            for other in peers.iter().skip(index + 1) {
                if let Some(other_match) = other
                    .artifacts
                    .iter()
                    .find(|entry| entry.canonical_ref == artifact.canonical_ref)
                    && other_match.sha256 != artifact.sha256
                {
                    conflicts.push(announce::DigestConflict {
                        canonical_ref: artifact.canonical_ref.clone(),
                        peer_id: peer.peer_id.clone(),
                        peer_sha256: artifact.sha256.clone(),
                        conflicts_with: other.peer_id.clone(),
                        other_sha256: other_match.sha256.clone(),
                    });
                }
            }
        }
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::announce::InventoryPayload;
    use crate::options::MirrorOptions;
    use tdcc_plugin::Plugin;

    async fn empty_cache() -> (tempfile::TempDir, MirrorCache) {
        let root = tempfile::tempdir().expect("cache dir");
        let options = MirrorOptions {
            cache_dir: root.path().to_path_buf(),
            import_roots: Vec::new(),
            max_cache_bytes: 1_000,
            max_chunk_bytes: 1_024,
            serve_bytes_per_minute: 0,
            reverify_after_secs: 60,
            advertise: true,
        };
        let cache = MirrorCache::open(options).await.expect("cache opens");
        (root, cache)
    }

    #[test]
    fn list_limits_are_clamped_into_the_advertised_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIST_LIMIT as usize);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10)), 10);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIST_LIMIT as usize);
    }

    #[tokio::test]
    async fn the_manifest_declares_exactly_the_surfaces_this_plugin_owns() {
        let (_root, cache) = empty_cache().await;

        let plugin = model_mirror_plugin(cache);
        let manifest = plugin
            .manifest()
            .expect("declarative plugins have a manifest");

        assert!(
            manifest
                .capabilities
                .contains(&"model-mirror.v1".to_string())
        );
        assert_eq!(
            manifest
                .mesh_channels
                .iter()
                .map(|channel| channel.name.as_str())
                .collect::<Vec<_>>(),
            vec![announce::CHANNEL]
        );
        // Operator limits are enforced in-process from `[[plugin]].args`, so
        // there is deliberately no console-editable settings schema.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
    }

    #[tokio::test]
    async fn every_http_route_is_read_only() {
        let (_root, cache) = empty_cache().await;

        let plugin = model_mirror_plugin(cache);
        let manifest = plugin.manifest().expect("manifest");

        assert_eq!(manifest.http_bindings.len(), 3);
        for binding in &manifest.http_bindings {
            assert_eq!(
                binding.method,
                proto::HttpMethod::Get as i32,
                "{} must not mutate mirror state over HTTP",
                binding.path
            );
        }
    }

    #[tokio::test]
    async fn an_empty_mirror_advertises_nothing_but_still_reports_its_limits() {
        let (_root, cache) = empty_cache().await;

        let payload: InventoryPayload = inventory_payload(&cache);

        assert!(payload.artifacts.is_empty());
        assert!(payload.serving);
        assert_eq!(payload.max_chunk_bytes, 1_024);
    }
}
