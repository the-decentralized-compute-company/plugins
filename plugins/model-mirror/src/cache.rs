//! The mirror store: what this node holds, how it got there, and how it is
//! proved to still be the thing it claims to be.
//!
//! # The integrity contract
//!
//! A mirror that can serve a substituted model is a supply-chain attack with
//! extra steps, so every path in and out of this store is gated on a SHA-256
//! digest:
//!
//! * **On write.** [`MirrorCache::import`] and [`MirrorCache::finalize_receive`]
//!   digest the complete file before it is promoted into the blob store. A
//!   mismatch deletes the staged bytes and returns
//!   [`MirrorError::Integrity`] — the artifact is never published, not even
//!   quarantined, because nothing ever trusted it.
//! * **On read.** Every serve checks a cheap tamper tripwire (recorded size and
//!   mtime), re-digests the whole artifact when its last full verification has
//!   aged past `reverify_after_secs`, and returns a per-chunk digest so the
//!   receiver can verify incrementally instead of trusting a 20 GB stream. A
//!   failed check quarantines the entry: it stops being served and stops being
//!   advertised, and the bytes stay on disk for the operator to look at.
//! * **End to end.** The receiving side digests the whole staged file against
//!   the digest *it* pinned before accepting it. That is the check that
//!   actually matters, because it does not trust this node at all.
//!
//! # What a digest here does and does not prove
//!
//! It proves the bytes did not change between two points. It does not prove
//! they are the bytes the model's author published — Hugging Face's file
//! listing does not expose per-file SHA-256 (`model_hf`'s `list_files` leaves
//! `ModelArtifactFile::sha256` as `None`), so the first digest for an artifact
//! is always computed locally from a file somebody already downloaded. Treat a
//! digest learned from a peer as a peer's claim, and pin digests out of band
//! when the supply chain has to be provable.
//!
//! # Path safety
//!
//! No caller-supplied string ever becomes a path component. On-disk names are
//! `sha256(canonical_ref)` in hex; see [`crate::artifact::ArtifactId::cache_key`].
//! The one place a caller does hand over a path — `import` — is confined to the
//! configured import roots after full canonicalization, so a symlink pointing
//! out of the root resolves out of the root and is refused.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::artifact::{ArtifactId, canonical_ref_from_hf_cache_path};
use crate::digest::{HASH_BUFFER_BYTES, Sha256Hex, Sha256Stream, sha256_file};
use crate::options::{MirrorOptions, containing_root};
use crate::policy::{
    BandwidthBudget, CapacityError, EvictionCandidate, EvictionPlan, clamp_chunk_length,
    plan_eviction,
};

const ENTRY_DIR: &str = "entries";
const BLOB_DIR: &str = "blobs";
const STAGING_DIR: &str = "staging";

/// Whether an artifact may be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    /// Digest-verified and servable.
    Ready,
    /// Failed an integrity check. Never served, never advertised, never
    /// silently deleted.
    Quarantined,
}

/// One artifact this node holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MirrorEntry {
    pub canonical_ref: String,
    pub repo: String,
    pub revision: String,
    pub file: String,
    /// Display id in `tdcc`'s own form, e.g. `org/repo:Q4_K_M`.
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
    pub size_bytes: u64,
    pub sha256: Sha256Hex,
    pub state: EntryState,
    pub pinned: bool,
    pub imported_at: u64,
    /// Epoch seconds of the last full re-digest.
    pub last_verified_at: u64,
    pub last_served_at: u64,
    pub served_bytes: u64,
    /// Blob mtime at the last verification; half of the cheap tamper tripwire.
    /// `0` means the platform did not report one, and the tripwire degrades to
    /// a size check.
    pub mtime_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChunkResponse {
    pub canonical_ref: String,
    pub offset: u64,
    pub length: u64,
    pub total_bytes: u64,
    pub eof: bool,
    /// Digest of the whole artifact, so a receiver knows what to verify against.
    pub artifact_sha256: String,
    /// Digest of exactly the bytes in `data_base64`.
    pub chunk_sha256: String,
    pub encoding: &'static str,
    pub data_base64: String,
    /// True when the bandwidth budget shortened this chunk. Ask again for the
    /// next offset; nothing is lost.
    pub throttled: bool,
    pub retry_after_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReceiveProgress {
    pub canonical_ref: String,
    pub received_bytes: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub expected_sha256: String,
    pub complete: bool,
    /// True when this node already holds a verified copy: skip the transfer.
    pub already_held: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportReport {
    pub entry: MirrorEntry,
    pub evicted: Vec<String>,
    pub bytes_hashed: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerifyReport {
    pub canonical_ref: String,
    pub verified: bool,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvictReport {
    pub evicted: Vec<String>,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub cache_dir: String,
    pub import_roots: Vec<String>,
    pub serving: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_serving_reason: Option<String>,
    pub entries: usize,
    pub quarantined: usize,
    pub used_bytes: u64,
    pub max_cache_bytes: u64,
    pub pinned_bytes: u64,
    pub staging_bytes: u64,
    pub max_chunk_bytes: u64,
    pub serve_bytes_per_minute: u64,
    pub bandwidth_available_bytes: u64,
    pub reverify_after_secs: u64,
    pub advertise: bool,
    pub served_bytes_total: u64,
}

/// Every way a mirror operation can fail, kept distinct so a caller can tell
/// "you asked wrong" from "your model was tampered with".
#[derive(Clone, Debug)]
pub enum MirrorError {
    /// Caller-supplied input the mirror will not act on.
    Invalid(String),
    /// This node does not hold that artifact.
    NotFound(String),
    /// Another operation owns that artifact right now.
    Busy(String),
    /// Disk cap reached, or the mirror has no disk allowance at all.
    Capacity(String),
    /// Bandwidth budget exhausted.
    Throttled {
        message: String,
        retry_after_ms: u64,
    },
    /// A digest did not match. The loudest thing this plugin can say.
    Integrity(String),
    Io(String),
}

impl MirrorError {
    fn io(context: impl std::fmt::Display, error: std::io::Error) -> Self {
        Self::Io(format!("{context}: {error}"))
    }
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid request: {message}"),
            Self::NotFound(message) => write!(formatter, "not mirrored here: {message}"),
            Self::Busy(message) => write!(formatter, "busy: {message}"),
            Self::Capacity(message) => write!(formatter, "cache capacity: {message}"),
            Self::Throttled { message, .. } => write!(formatter, "throttled: {message}"),
            Self::Integrity(message) => write!(formatter, "INTEGRITY FAILURE: {message}"),
            Self::Io(message) => write!(formatter, "io error: {message}"),
        }
    }
}

impl std::error::Error for MirrorError {}

impl From<MirrorError> for tdcc_plugin::PluginError {
    fn from(value: MirrorError) -> Self {
        let message = value.to_string();
        match value {
            // Caller-fixable: the host surfaces these as invalid params.
            MirrorError::Invalid(_) | MirrorError::NotFound(_) => Self::invalid_params(message),
            _ => Self::internal(message),
        }
    }
}

type MirrorResult<T> = Result<T, MirrorError>;

/// Persisted alongside a partial transfer so a resume survives a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StagingRecord {
    canonical_ref: String,
    expected_sha256: Sha256Hex,
    total_bytes: u64,
    started_at: u64,
}

struct Inner {
    options: MirrorOptions,
    /// Import roots after canonicalization; roots that do not exist are
    /// dropped at open so a stale configuration cannot widen the boundary.
    import_roots: Vec<PathBuf>,
    index: Mutex<BTreeMap<String, MirrorEntry>>,
    /// Cache keys with an exclusive operation in flight.
    busy: Mutex<HashSet<String>>,
    budget: Mutex<BandwidthBudget>,
    started: Instant,
}

/// Cheap to clone; every clone shares one store, so each handler closure can
/// own one.
#[derive(Clone)]
pub struct MirrorCache {
    inner: Arc<Inner>,
}

impl MirrorCache {
    /// Create the store layout and load any entries left by a previous run.
    ///
    /// Reconciliation is deliberate rather than trusting: an entry whose blob
    /// has vanished is dropped, and an entry whose blob has changed size or
    /// mtime is quarantined rather than served.
    pub async fn open(options: MirrorOptions) -> anyhow::Result<Self> {
        for directory in [ENTRY_DIR, BLOB_DIR, STAGING_DIR] {
            tokio::fs::create_dir_all(options.cache_dir.join(directory))
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "create {}: {error}",
                        options.cache_dir.join(directory).display()
                    )
                })?;
        }

        let mut import_roots = Vec::new();
        for root in &options.import_roots {
            match tokio::fs::canonicalize(root).await {
                Ok(resolved) => import_roots.push(resolved),
                Err(error) => eprintln!(
                    "model-mirror: import root {} is unusable and will be ignored: {error}",
                    root.display()
                ),
            }
        }

        let budget = BandwidthBudget::per_minute(options.serve_bytes_per_minute);
        let cache = Self {
            inner: Arc::new(Inner {
                options,
                import_roots,
                index: Mutex::new(BTreeMap::new()),
                busy: Mutex::new(HashSet::new()),
                budget: Mutex::new(budget),
                started: Instant::now(),
            }),
        };
        cache.load_entries().await?;
        Ok(cache)
    }

    pub fn options(&self) -> &MirrorOptions {
        &self.inner.options
    }

    /// Snapshot of the index, ordered by canonical ref so a listing is stable
    /// between calls.
    pub fn entries(&self) -> Vec<MirrorEntry> {
        let mut entries: Vec<MirrorEntry> = self.lock_index().values().cloned().collect();
        entries.sort_by(|left, right| left.canonical_ref.cmp(&right.canonical_ref));
        entries
    }

    /// Entries this node is willing to tell peers about.
    ///
    /// Quarantined artifacts are excluded: advertising something that failed
    /// its own integrity check would make this node the source of a bad
    /// download.
    pub fn ready_entries(&self) -> Vec<MirrorEntry> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.state == EntryState::Ready)
            .collect()
    }

    pub async fn status(&self) -> StatusReport {
        let entries = self.entries();
        let used_bytes = entries.iter().map(|entry| entry.size_bytes).sum();
        let pinned_bytes = entries
            .iter()
            .filter(|entry| entry.pinned)
            .map(|entry| entry.size_bytes)
            .sum();
        let quarantined = entries
            .iter()
            .filter(|entry| entry.state == EntryState::Quarantined)
            .count();
        let served_bytes_total = entries.iter().map(|entry| entry.served_bytes).sum();
        let options = &self.inner.options;
        let not_serving_reason = if options.holds_artifacts() {
            None
        } else {
            Some(
                "max_cache_bytes is 0: pass --max-cache-bytes to [[plugin]].args with the amount \
                 of disk this node may contribute"
                    .to_string(),
            )
        };

        // Both computed before the struct literal so no lock guard and no
        // await can end up interleaved inside it.
        let staging_bytes = self.staging_bytes().await;
        let bandwidth_available_bytes = self.lock_budget().available();

        StatusReport {
            cache_dir: options.cache_dir.display().to_string(),
            import_roots: self
                .inner
                .import_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
            serving: options.holds_artifacts(),
            not_serving_reason,
            entries: entries.len(),
            quarantined,
            used_bytes,
            max_cache_bytes: options.max_cache_bytes,
            pinned_bytes,
            staging_bytes,
            max_chunk_bytes: options.max_chunk_bytes,
            serve_bytes_per_minute: options.serve_bytes_per_minute,
            bandwidth_available_bytes,
            reverify_after_secs: options.reverify_after_secs,
            advertise: options.advertise,
            served_bytes_total,
        }
    }

    /// Take a local file into the mirror.
    ///
    /// The file is copied, not linked: the mirror owns immutable bytes it can
    /// re-verify, which a hard link to somebody else's cache would not be. That
    /// costs a second copy on disk, and it is the price of being able to
    /// promise a peer that what is served is what was verified.
    pub async fn import(
        &self,
        source: &Path,
        canonical_ref: Option<&str>,
        expected_sha256: Option<&str>,
        pin: bool,
    ) -> MirrorResult<ImportReport> {
        self.require_disk()?;

        let resolved = tokio::fs::canonicalize(source).await.map_err(|error| {
            MirrorError::Invalid(format!("cannot read {}: {error}", source.display()))
        })?;
        if containing_root(&self.inner.import_roots, &resolved).is_none() {
            return Err(MirrorError::Invalid(format!(
                "{} is outside this mirror's import roots ({}); imports are confined to configured \
                 roots on purpose",
                resolved.display(),
                self.import_roots_display()
            )));
        }
        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(|error| MirrorError::io(format!("stat {}", resolved.display()), error))?;
        if !metadata.is_file() {
            return Err(MirrorError::Invalid(format!(
                "{} is not a regular file",
                resolved.display()
            )));
        }
        let size_bytes = metadata.len();

        let id = match canonical_ref {
            Some(value) => parse_id(value)?,
            None => self.derive_identity(&resolved)?,
        };
        let expected = expected_sha256.map(parse_digest).transpose()?;

        let key = id.cache_key();
        let _guard = self.acquire(&key, &id)?;

        let evicted = self.make_room(&key, size_bytes).await?;

        let staging = self.staging_path(&key);
        let (digest, bytes_hashed) = self.copy_and_digest(&resolved, &staging).await?;
        if bytes_hashed != size_bytes {
            // The file changed under us mid-copy. The digest describes what we
            // actually read, but the size mismatch means the source is not
            // stable, so refuse rather than publish a moving target.
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(MirrorError::Integrity(format!(
                "{} changed while it was being imported ({size_bytes} bytes at stat, \
                 {bytes_hashed} read)",
                resolved.display()
            )));
        }
        if let Some(expected) = &expected
            && expected != &digest
        {
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(MirrorError::Integrity(format!(
                "{} hashes to {digest}, not the expected {expected}; refusing to mirror it",
                resolved.display()
            )));
        }

        let entry = self
            .promote(&key, &id, digest, bytes_hashed, pin, &staging)
            .await?;
        Ok(ImportReport {
            entry,
            evicted,
            bytes_hashed,
        })
    }

    /// Serve one range of one artifact.
    pub async fn read_chunk(
        &self,
        canonical_ref: &str,
        offset: u64,
        length: Option<u64>,
    ) -> MirrorResult<ChunkResponse> {
        // A node configured to hold nothing is out of the mirror entirely: it
        // neither admits nor serves. Whatever is already on disk stays there
        // for the operator to evict deliberately.
        self.require_disk()?;
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        let mut entry = self.entry(&key, &id)?;
        if entry.state != EntryState::Ready {
            return Err(MirrorError::Integrity(format!(
                "{} is quarantined ({}); it will not be served until it verifies again",
                entry.canonical_ref,
                entry
                    .quarantine_reason
                    .clone()
                    .unwrap_or_else(|| "no reason recorded".to_string())
            )));
        }
        if offset > entry.size_bytes {
            return Err(MirrorError::Invalid(format!(
                "offset {offset} is past the end of {} ({} bytes)",
                entry.canonical_ref, entry.size_bytes
            )));
        }

        let blob = self.blob_path(&key);
        self.tripwire(&blob, &entry).await?;

        // Verify on read: a full re-digest at the start of a transfer, rate
        // limited by `reverify_after_secs` so a 20 GB artifact is not rehashed
        // for every chunk.
        let now = epoch_secs();
        if offset == 0
            && now.saturating_sub(entry.last_verified_at) >= self.inner.options.reverify_after_secs
        {
            entry = self.verify_entry(&key, entry).await?;
        }

        let remaining = entry.size_bytes - offset;
        let want = clamp_chunk_length(length, self.inner.options.max_chunk_bytes).min(remaining);
        if want == 0 {
            return Ok(ChunkResponse {
                canonical_ref: entry.canonical_ref.clone(),
                offset,
                length: 0,
                total_bytes: entry.size_bytes,
                eof: true,
                artifact_sha256: entry.sha256.to_string(),
                chunk_sha256: Sha256Hex::of_bytes(&[]).to_string(),
                encoding: "base64",
                data_base64: String::new(),
                throttled: false,
                retry_after_ms: 0,
            });
        }

        let (granted, retry_after_ms) = {
            let mut budget = self.lock_budget();
            let granted = budget.take(self.now_ms(), want);
            let retry = budget.retry_after_ms(want);
            (granted, retry)
        };
        if granted == 0 {
            return Err(MirrorError::Throttled {
                message: format!(
                    "this mirror is capped at {} bytes/minute; retry in {retry_after_ms} ms",
                    self.inner.options.serve_bytes_per_minute
                ),
                retry_after_ms,
            });
        }

        let data = read_range(&blob, offset, granted).await?;
        let chunk_sha256 = Sha256Hex::of_bytes(&data);
        let served = data.len() as u64;

        self.mutate_entry(&key, |entry| {
            entry.last_served_at = now;
            entry.served_bytes = entry.served_bytes.saturating_add(served);
        })
        .await;

        Ok(ChunkResponse {
            canonical_ref: entry.canonical_ref.clone(),
            offset,
            length: served,
            total_bytes: entry.size_bytes,
            eof: offset + served >= entry.size_bytes,
            artifact_sha256: entry.sha256.to_string(),
            chunk_sha256: chunk_sha256.to_string(),
            encoding: "base64",
            data_base64: BASE64.encode(&data),
            throttled: served < want,
            retry_after_ms: if served < want { retry_after_ms } else { 0 },
        })
    }

    /// Open (or resume) an inbound transfer.
    pub async fn begin_receive(
        &self,
        canonical_ref: &str,
        expected_sha256: &str,
        total_bytes: u64,
    ) -> MirrorResult<ReceiveProgress> {
        self.require_disk()?;
        let id = parse_id(canonical_ref)?;
        let expected = parse_digest(expected_sha256)?;
        let key = id.cache_key();
        let _guard = self.acquire(&key, &id)?;

        // Bound out of the lock first: nothing here may hold a guard across an
        // await.
        let existing = self.lock_index().get(&key).cloned();
        if let Some(existing) = existing
            && existing.state == EntryState::Ready
            && existing.sha256 == expected
        {
            return Ok(ReceiveProgress {
                canonical_ref: id.canonical_ref(),
                received_bytes: existing.size_bytes,
                total_bytes: existing.size_bytes,
                percent: 100.0,
                expected_sha256: expected.to_string(),
                complete: true,
                already_held: true,
            });
        }

        // Reserve space before a byte arrives, so a transfer that cannot
        // possibly fit fails immediately instead of after 20 GB.
        self.make_room(&key, total_bytes).await?;

        let staging = self.staging_path(&key);
        let received = match self.read_staging_record(&key).await? {
            Some(record) => {
                if record.expected_sha256 != expected || record.total_bytes != total_bytes {
                    return Err(MirrorError::Integrity(format!(
                        "a different transfer is already staged for {} (digest {}, {} bytes); \
                         call abort_receive first if you meant to replace it",
                        id.canonical_ref(),
                        record.expected_sha256,
                        record.total_bytes
                    )));
                }
                staged_len(&staging).await.min(total_bytes)
            }
            None => {
                self.write_staging_record(
                    &key,
                    &StagingRecord {
                        canonical_ref: id.canonical_ref(),
                        expected_sha256: expected.clone(),
                        total_bytes,
                        started_at: epoch_secs(),
                    },
                )
                .await?;
                tokio::fs::write(&staging, b"").await.map_err(|error| {
                    MirrorError::io(format!("create {}", staging.display()), error)
                })?;
                0
            }
        };

        Ok(ReceiveProgress {
            canonical_ref: id.canonical_ref(),
            received_bytes: received,
            total_bytes,
            percent: percent(received, total_bytes),
            expected_sha256: expected.to_string(),
            complete: received == total_bytes,
            already_held: false,
        })
    }

    /// Append one chunk to a staged transfer.
    ///
    /// Strictly append-only: `offset` must equal the number of bytes already
    /// staged. Out-of-order or overlapping writes are refused rather than
    /// stitched together, because a mirror that lets a sender place arbitrary
    /// bytes at arbitrary offsets is a mirror whose final digest check is the
    /// only thing standing between a peer and a crafted file — and one check is
    /// not where this should end.
    pub async fn receive_chunk(
        &self,
        canonical_ref: &str,
        offset: u64,
        data_base64: &str,
        chunk_sha256: Option<&str>,
    ) -> MirrorResult<ReceiveProgress> {
        self.require_disk()?;
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        let _guard = self.acquire(&key, &id)?;

        let record = self.read_staging_record(&key).await?.ok_or_else(|| {
            MirrorError::Invalid(format!(
                "no transfer is open for {}; call begin_receive first",
                id.canonical_ref()
            ))
        })?;

        // Reject the payload on its encoded size, before allocating a decode
        // buffer for it.
        let max_chunk = self.inner.options.max_chunk_bytes;
        if base64_decoded_upper_bound(data_base64.len()) > max_chunk {
            return Err(MirrorError::Invalid(format!(
                "chunk exceeds this mirror's {max_chunk} byte limit"
            )));
        }
        let data = BASE64
            .decode(data_base64.as_bytes())
            .map_err(|error| MirrorError::Invalid(format!("data is not valid base64: {error}")))?;
        if data.len() as u64 > max_chunk {
            return Err(MirrorError::Invalid(format!(
                "chunk of {} bytes exceeds this mirror's {max_chunk} byte limit",
                data.len()
            )));
        }
        if let Some(expected) = chunk_sha256 {
            let expected = parse_digest(expected)?;
            let actual = Sha256Hex::of_bytes(&data);
            if expected != actual {
                return Err(MirrorError::Integrity(format!(
                    "chunk at offset {offset} of {} hashes to {actual}, not the declared {expected}",
                    id.canonical_ref()
                )));
            }
        }

        let staging = self.staging_path(&key);
        let staged = staged_len(&staging).await;
        if offset != staged {
            return Err(MirrorError::Invalid(format!(
                "transfers are append-only: expected offset {staged} for {}, got {offset}",
                id.canonical_ref()
            )));
        }
        if staged + data.len() as u64 > record.total_bytes {
            return Err(MirrorError::Invalid(format!(
                "chunk would overrun the declared size of {} ({} bytes)",
                id.canonical_ref(),
                record.total_bytes
            )));
        }

        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&staging)
            .await
            .map_err(|error| MirrorError::io(format!("open {}", staging.display()), error))?;
        file.write_all(&data)
            .await
            .map_err(|error| MirrorError::io(format!("append to {}", staging.display()), error))?;
        file.flush()
            .await
            .map_err(|error| MirrorError::io(format!("flush {}", staging.display()), error))?;

        let received = staged + data.len() as u64;
        Ok(ReceiveProgress {
            canonical_ref: id.canonical_ref(),
            received_bytes: received,
            total_bytes: record.total_bytes,
            percent: percent(received, record.total_bytes),
            expected_sha256: record.expected_sha256.to_string(),
            complete: received == record.total_bytes,
            already_held: false,
        })
    }

    /// Digest the staged file and publish it only if it matches.
    pub async fn finalize_receive(&self, canonical_ref: &str) -> MirrorResult<ImportReport> {
        self.require_disk()?;
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        let _guard = self.acquire(&key, &id)?;

        let record = self.read_staging_record(&key).await?.ok_or_else(|| {
            MirrorError::Invalid(format!("no transfer is open for {}", id.canonical_ref()))
        })?;
        let staging = self.staging_path(&key);
        let staged = staged_len(&staging).await;
        if staged != record.total_bytes {
            return Err(MirrorError::Invalid(format!(
                "transfer of {} is incomplete: {staged} of {} bytes",
                id.canonical_ref(),
                record.total_bytes
            )));
        }

        let (digest, hashed) = sha256_file(&staging)
            .await
            .map_err(|error| MirrorError::io(format!("digest {}", staging.display()), error))?;
        if digest != record.expected_sha256 || hashed != record.total_bytes {
            // Loud, and destructive to the bad bytes: a failed transfer must
            // not leave a plausible-looking file behind for a later resume to
            // adopt.
            let _ = tokio::fs::remove_file(&staging).await;
            let _ = tokio::fs::remove_file(self.staging_record_path(&key)).await;
            return Err(MirrorError::Integrity(format!(
                "{} hashes to {digest} after transfer, not the expected {}; the staged copy has \
                 been discarded",
                id.canonical_ref(),
                record.expected_sha256
            )));
        }

        let evicted = self.make_room(&key, record.total_bytes).await?;
        let entry = self
            .promote(&key, &id, digest, record.total_bytes, false, &staging)
            .await?;
        let _ = tokio::fs::remove_file(self.staging_record_path(&key)).await;

        Ok(ImportReport {
            entry,
            evicted,
            bytes_hashed: hashed,
        })
    }

    /// Discard a partial transfer.
    pub async fn abort_receive(&self, canonical_ref: &str) -> MirrorResult<ReceiveProgress> {
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        let _guard = self.acquire(&key, &id)?;

        let record = self.read_staging_record(&key).await?.ok_or_else(|| {
            MirrorError::NotFound(format!("no transfer is open for {}", id.canonical_ref()))
        })?;
        let _ = tokio::fs::remove_file(self.staging_path(&key)).await;
        let _ = tokio::fs::remove_file(self.staging_record_path(&key)).await;

        Ok(ReceiveProgress {
            canonical_ref: id.canonical_ref(),
            received_bytes: 0,
            total_bytes: record.total_bytes,
            percent: 0.0,
            expected_sha256: record.expected_sha256.to_string(),
            complete: false,
            already_held: false,
        })
    }

    /// Re-digest a held artifact end to end.
    pub async fn verify(&self, canonical_ref: &str) -> MirrorResult<VerifyReport> {
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        let entry = self.entry(&key, &id)?;
        let expected = entry.sha256.to_string();
        let verified = self.verify_entry(&key, entry).await?;
        Ok(VerifyReport {
            canonical_ref: verified.canonical_ref.clone(),
            verified: true,
            expected_sha256: expected,
            actual_sha256: verified.sha256.to_string(),
            size_bytes: verified.size_bytes,
        })
    }

    /// Pin or unpin an artifact against eviction.
    pub async fn set_pinned(&self, canonical_ref: &str, pinned: bool) -> MirrorResult<MirrorEntry> {
        let id = parse_id(canonical_ref)?;
        let key = id.cache_key();
        self.entry(&key, &id)?;
        self.mutate_entry(&key, |entry| entry.pinned = pinned).await;
        self.entry(&key, &id)
    }

    /// Drop artifacts, either a named one or enough to reclaim `reclaim_bytes`.
    pub async fn evict(
        &self,
        canonical_ref: Option<&str>,
        reclaim_bytes: Option<u64>,
        force: bool,
    ) -> MirrorResult<EvictReport> {
        let mut evicted = Vec::new();
        let mut freed = 0_u64;

        if let Some(reference) = canonical_ref {
            let id = parse_id(reference)?;
            let key = id.cache_key();
            let entry = self.entry(&key, &id)?;
            if entry.pinned && !force {
                return Err(MirrorError::Invalid(format!(
                    "{} is pinned; unpin it or pass force=true",
                    entry.canonical_ref
                )));
            }
            let _guard = self.acquire(&key, &id)?;
            freed += self.remove(&key).await;
            evicted.push(entry.canonical_ref);
        } else {
            let reclaim = reclaim_bytes.ok_or_else(|| {
                MirrorError::Invalid(
                    "pass either canonical_ref or reclaim_bytes to say what to evict".to_string(),
                )
            })?;
            let mut candidates: Vec<MirrorEntry> = self
                .entries()
                .into_iter()
                .filter(|entry| force || !entry.pinned)
                .collect();
            candidates.sort_by(|left, right| {
                left.last_served_at
                    .cmp(&right.last_served_at)
                    .then_with(|| left.canonical_ref.cmp(&right.canonical_ref))
            });
            for entry in candidates {
                if freed >= reclaim {
                    break;
                }
                let Ok(id) = ArtifactId::parse(&entry.canonical_ref) else {
                    continue;
                };
                let key = id.cache_key();
                let Ok(_guard) = self.acquire(&key, &id) else {
                    continue;
                };
                freed += self.remove(&key).await;
                evicted.push(entry.canonical_ref);
            }
        }

        let remaining_bytes = self.entries().iter().map(|entry| entry.size_bytes).sum();
        Ok(EvictReport {
            evicted,
            freed_bytes: freed,
            remaining_bytes,
        })
    }

    // ---- internals -------------------------------------------------------

    fn lock_index(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, MirrorEntry>> {
        // A poisoned lock means a handler panicked mid-update. The index is a
        // plain map with no cross-entry invariant, and every durable fact lives
        // in the entry files on disk, so recovering keeps the mirror usable.
        self.inner
            .index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_budget(&self) -> std::sync::MutexGuard<'_, BandwidthBudget> {
        self.inner
            .budget
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ms(&self) -> u64 {
        self.inner.started.elapsed().as_millis() as u64
    }

    fn require_disk(&self) -> MirrorResult<()> {
        if self.inner.options.holds_artifacts() {
            return Ok(());
        }
        Err(MirrorError::Capacity(
            "this mirror is configured to hold nothing: pass --max-cache-bytes in \
             [[plugin]].args with the amount of disk this node may contribute"
                .to_string(),
        ))
    }

    fn import_roots_display(&self) -> String {
        if self.inner.import_roots.is_empty() {
            return "none configured".to_string();
        }
        self.inner
            .import_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn derive_identity(&self, resolved: &Path) -> MirrorResult<ArtifactId> {
        self.inner
            .import_roots
            .iter()
            .find_map(|root| canonical_ref_from_hf_cache_path(resolved, root))
            .ok_or_else(|| {
                MirrorError::Invalid(format!(
                    "cannot derive a canonical ref for {}: it is not in a Hugging Face snapshot \
                     layout, so pass canonical_ref explicitly (org/repo@revision/file)",
                    resolved.display()
                ))
            })
    }

    fn entry(&self, key: &str, id: &ArtifactId) -> MirrorResult<MirrorEntry> {
        self.lock_index()
            .get(key)
            .cloned()
            .ok_or_else(|| MirrorError::NotFound(id.canonical_ref()))
    }

    fn acquire(&self, key: &str, id: &ArtifactId) -> MirrorResult<BusyGuard> {
        let inserted = self
            .inner
            .busy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_string());
        if !inserted {
            return Err(MirrorError::Busy(format!(
                "another operation is already working on {}",
                id.canonical_ref()
            )));
        }
        Ok(BusyGuard {
            inner: Arc::clone(&self.inner),
            key: key.to_string(),
        })
    }

    fn blob_path(&self, key: &str) -> PathBuf {
        self.inner
            .options
            .cache_dir
            .join(BLOB_DIR)
            .join(format!("{key}.bin"))
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.inner
            .options
            .cache_dir
            .join(ENTRY_DIR)
            .join(format!("{key}.json"))
    }

    fn staging_path(&self, key: &str) -> PathBuf {
        self.inner
            .options
            .cache_dir
            .join(STAGING_DIR)
            .join(format!("{key}.part"))
    }

    fn staging_record_path(&self, key: &str) -> PathBuf {
        self.inner
            .options
            .cache_dir
            .join(STAGING_DIR)
            .join(format!("{key}.json"))
    }

    async fn load_entries(&self) -> anyhow::Result<()> {
        let directory = self.inner.options.cache_dir.join(ENTRY_DIR);
        let mut read_dir = tokio::fs::read_dir(&directory).await?;
        let mut loaded = BTreeMap::new();
        while let Some(item) = read_dir.next_entry().await? {
            let path = item.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("model-mirror: cannot read {}: {error}", path.display());
                    continue;
                }
            };
            let mut entry: MirrorEntry = match serde_json::from_slice(&bytes) {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!(
                        "model-mirror: {} is not a usable entry record and is being ignored: \
                         {error}",
                        path.display()
                    );
                    continue;
                }
            };

            let blob = self.blob_path(key);
            match tokio::fs::metadata(&blob).await {
                Err(_) => {
                    eprintln!(
                        "model-mirror: dropping the record for {} because its data is gone",
                        entry.canonical_ref
                    );
                    let _ = tokio::fs::remove_file(&path).await;
                    continue;
                }
                Ok(metadata) => {
                    let mtime = mtime_secs(&metadata);
                    if metadata.len() != entry.size_bytes
                        || (entry.mtime_secs != 0 && mtime != entry.mtime_secs)
                    {
                        eprintln!(
                            "model-mirror: quarantining {}: its data changed on disk since it was \
                             verified",
                            entry.canonical_ref
                        );
                        entry.state = EntryState::Quarantined;
                        // The recorded size and mtime are left alone; they are
                        // the evidence of what changed.
                        entry.quarantine_reason = Some(format!(
                            "recorded {} bytes / mtime {}, found {} bytes / mtime {mtime}",
                            entry.size_bytes,
                            entry.mtime_secs,
                            metadata.len()
                        ));
                        let _ = write_json(&path, &entry).await;
                    }
                }
            }
            loaded.insert(key.to_string(), entry);
        }
        *self.lock_index() = loaded;
        Ok(())
    }

    async fn staging_bytes(&self) -> u64 {
        let directory = self.inner.options.cache_dir.join(STAGING_DIR);
        let Ok(mut read_dir) = tokio::fs::read_dir(&directory).await else {
            return 0;
        };
        let mut total = 0;
        while let Ok(Some(item)) = read_dir.next_entry().await {
            if item.path().extension().and_then(|value| value.to_str()) != Some("part") {
                continue;
            }
            if let Ok(metadata) = item.metadata().await {
                total += metadata.len();
            }
        }
        total
    }

    /// Evict as needed so `incoming` bytes fit, then report what went.
    async fn make_room(&self, key: &str, incoming: u64) -> MirrorResult<Vec<String>> {
        let candidates: Vec<EvictionCandidate> = self
            .lock_index()
            .iter()
            .filter(|(entry_key, _)| entry_key.as_str() != key)
            .map(|(_, entry)| EvictionCandidate {
                canonical_ref: entry.canonical_ref.clone(),
                size_bytes: entry.size_bytes,
                pinned: entry.pinned,
                last_used_at: entry.last_served_at.max(entry.imported_at),
            })
            .collect();

        let plan: EvictionPlan =
            plan_eviction(&candidates, self.inner.options.max_cache_bytes, incoming)
                .map_err(|error: CapacityError| MirrorError::Capacity(error.to_string()))?;

        for reference in &plan.evict {
            let Ok(id) = ArtifactId::parse(reference) else {
                continue;
            };
            self.remove(&id.cache_key()).await;
        }
        Ok(plan.evict)
    }

    /// Move verified staged bytes into the blob store and record the entry.
    async fn promote(
        &self,
        key: &str,
        id: &ArtifactId,
        digest: Sha256Hex,
        size_bytes: u64,
        pin: bool,
        staging: &Path,
    ) -> MirrorResult<MirrorEntry> {
        let blob = self.blob_path(key);
        tokio::fs::rename(staging, &blob).await.map_err(|error| {
            MirrorError::io(
                format!("publish {} as {}", staging.display(), blob.display()),
                error,
            )
        })?;
        let metadata = tokio::fs::metadata(&blob)
            .await
            .map_err(|error| MirrorError::io(format!("stat {}", blob.display()), error))?;

        let now = epoch_secs();
        let previous = self.lock_index().get(key).cloned();
        let entry = MirrorEntry {
            canonical_ref: id.canonical_ref(),
            repo: id.repo().to_string(),
            revision: id.revision().to_string(),
            file: id.file().to_string(),
            model_id: id.model_id(),
            selector: id.selector(),
            distribution_id: id.distribution_id(),
            size_bytes,
            sha256: digest,
            state: EntryState::Ready,
            pinned: pin || previous.as_ref().is_some_and(|entry| entry.pinned),
            imported_at: previous
                .as_ref()
                .map(|entry| entry.imported_at)
                .unwrap_or(now),
            last_verified_at: now,
            last_served_at: previous
                .as_ref()
                .map(|entry| entry.last_served_at)
                .unwrap_or(0),
            served_bytes: previous
                .as_ref()
                .map(|entry| entry.served_bytes)
                .unwrap_or(0),
            mtime_secs: mtime_secs(&metadata),
            quarantine_reason: None,
        };
        write_json(&self.entry_path(key), &entry)
            .await
            .map_err(|error| MirrorError::Io(error.to_string()))?;
        self.lock_index().insert(key.to_string(), entry.clone());
        Ok(entry)
    }

    /// Cheap tamper tripwire before every serve.
    ///
    /// Size and mtime are not proof — the digest is. This exists so a replaced
    /// file is caught in microseconds instead of after a full re-digest, and so
    /// the mirror never streams bytes it has not at least sanity-checked.
    async fn tripwire(&self, blob: &Path, entry: &MirrorEntry) -> MirrorResult<()> {
        let metadata = match tokio::fs::metadata(blob).await {
            Ok(metadata) => metadata,
            Err(error) => {
                self.quarantine(entry, "the artifact data is missing").await;
                return Err(MirrorError::Integrity(format!(
                    "{} is recorded but its data cannot be read ({error}); it has been quarantined",
                    entry.canonical_ref
                )));
            }
        };
        let mtime = mtime_secs(&metadata);
        if metadata.len() != entry.size_bytes
            || (entry.mtime_secs != 0 && mtime != entry.mtime_secs)
        {
            self.quarantine(entry, "size or mtime changed on disk")
                .await;
            return Err(MirrorError::Integrity(format!(
                "{} changed on disk since it was verified ({} bytes now, {} when verified); it \
                 has been quarantined and will not be served",
                entry.canonical_ref,
                metadata.len(),
                entry.size_bytes
            )));
        }
        Ok(())
    }

    /// Full re-digest. Quarantines and errors on mismatch.
    async fn verify_entry(&self, key: &str, entry: MirrorEntry) -> MirrorResult<MirrorEntry> {
        let blob = self.blob_path(key);
        let (digest, size) = sha256_file(&blob)
            .await
            .map_err(|error| MirrorError::io(format!("digest {}", blob.display()), error))?;
        if digest != entry.sha256 || size != entry.size_bytes {
            self.quarantine(&entry, &format!("re-digest produced {digest}"))
                .await;
            return Err(MirrorError::Integrity(format!(
                "{} now hashes to {digest} ({size} bytes) but was mirrored as {} ({} bytes); it \
                 has been quarantined and will not be served",
                entry.canonical_ref, entry.sha256, entry.size_bytes
            )));
        }
        let metadata = tokio::fs::metadata(&blob)
            .await
            .map_err(|error| MirrorError::io(format!("stat {}", blob.display()), error))?;
        let mtime = mtime_secs(&metadata);
        self.mutate_entry(key, |entry| {
            entry.last_verified_at = epoch_secs();
            entry.mtime_secs = mtime;
            entry.state = EntryState::Ready;
            entry.quarantine_reason = None;
        })
        .await;
        self.lock_index()
            .get(key)
            .cloned()
            .ok_or_else(|| MirrorError::NotFound(entry.canonical_ref.clone()))
    }

    async fn quarantine(&self, entry: &MirrorEntry, reason: &str) {
        let Ok(id) = ArtifactId::parse(&entry.canonical_ref) else {
            return;
        };
        eprintln!(
            "model-mirror: QUARANTINE {} — {reason}",
            entry.canonical_ref
        );
        self.mutate_entry(&id.cache_key(), |entry| {
            entry.state = EntryState::Quarantined;
            entry.quarantine_reason = Some(reason.to_string());
        })
        .await;
    }

    async fn mutate_entry<F>(&self, key: &str, change: F)
    where
        F: FnOnce(&mut MirrorEntry),
    {
        let updated = {
            let mut index = self.lock_index();
            let Some(entry) = index.get_mut(key) else {
                return;
            };
            change(entry);
            entry.clone()
        };
        if let Err(error) = write_json(&self.entry_path(key), &updated).await {
            eprintln!(
                "model-mirror: cannot persist the record for {}: {error}",
                updated.canonical_ref
            );
        }
    }

    /// Delete an artifact's data and record, returning the bytes reclaimed.
    async fn remove(&self, key: &str) -> u64 {
        let removed = self.lock_index().remove(key);
        let _ = tokio::fs::remove_file(self.blob_path(key)).await;
        let _ = tokio::fs::remove_file(self.entry_path(key)).await;
        removed.map(|entry| entry.size_bytes).unwrap_or(0)
    }

    async fn read_staging_record(&self, key: &str) -> MirrorResult<Option<StagingRecord>> {
        let path = self.staging_record_path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                MirrorError::Invalid(format!(
                    "the staged transfer record at {} is unreadable ({error}); abort_receive will \
                     clear it",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MirrorError::io(format!("read {}", path.display()), error)),
        }
    }

    async fn write_staging_record(&self, key: &str, record: &StagingRecord) -> MirrorResult<()> {
        write_json(&self.staging_record_path(key), record)
            .await
            .map_err(|error| MirrorError::Io(error.to_string()))
    }

    /// Stream a file into `destination`, digesting as it goes.
    async fn copy_and_digest(
        &self,
        source: &Path,
        destination: &Path,
    ) -> MirrorResult<(Sha256Hex, u64)> {
        let mut input = tokio::fs::File::open(source)
            .await
            .map_err(|error| MirrorError::io(format!("open {}", source.display()), error))?;
        let mut output = tokio::fs::File::create(destination)
            .await
            .map_err(|error| MirrorError::io(format!("create {}", destination.display()), error))?;

        let mut hasher = Sha256Stream::new();
        let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
        let mut copied = 0_u64;
        loop {
            let read = input
                .read(&mut buffer)
                .await
                .map_err(|error| MirrorError::io(format!("read {}", source.display()), error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read]).await.map_err(|error| {
                MirrorError::io(format!("write {}", destination.display()), error)
            })?;
            copied += read as u64;
        }
        output
            .flush()
            .await
            .map_err(|error| MirrorError::io(format!("flush {}", destination.display()), error))?;

        Ok((hasher.finish(), copied))
    }
}

/// Holds the exclusive claim on one cache key for the life of an operation.
struct BusyGuard {
    inner: Arc<Inner>,
    key: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.inner
            .busy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

fn parse_id(canonical_ref: &str) -> MirrorResult<ArtifactId> {
    ArtifactId::parse(canonical_ref).map_err(|error| MirrorError::Invalid(error.to_string()))
}

fn parse_digest(value: &str) -> MirrorResult<Sha256Hex> {
    Sha256Hex::parse(value).map_err(|error| MirrorError::Invalid(error.to_string()))
}

/// Transfer progress as a percentage, with a zero-length artifact reported as
/// complete rather than as a division by zero.
pub fn percent(received: u64, total: u64) -> f64 {
    if total == 0 {
        return 100.0;
    }
    (received as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

/// Largest number of bytes a base64 string of this length can decode to.
pub fn base64_decoded_upper_bound(encoded_len: usize) -> u64 {
    (encoded_len as u64).div_ceil(4) * 3
}

fn mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub(crate) fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

async fn staged_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

async fn read_range(path: &Path, offset: u64, length: u64) -> MirrorResult<Vec<u8>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| MirrorError::io(format!("open {}", path.display()), error))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| MirrorError::io(format!("seek {}", path.display()), error))?;
    let mut buffer = vec![0_u8; length as usize];
    file.read_exact(&mut buffer)
        .await
        .map_err(|error| MirrorError::io(format!("read {}", path.display()), error))?;
    Ok(buffer)
}

/// Write JSON through a temporary file so a crash mid-write cannot leave a
/// truncated record that later parses as a smaller artifact.
async fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, &bytes).await?;
    tokio::fs::rename(&temporary, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(root: &Path, import: &Path, cap: u64) -> MirrorOptions {
        MirrorOptions {
            cache_dir: root.to_path_buf(),
            import_roots: vec![import.to_path_buf()],
            max_cache_bytes: cap,
            max_chunk_bytes: 64,
            serve_bytes_per_minute: 0,
            reverify_after_secs: u64::MAX,
            advertise: true,
        }
    }

    const REF_A: &str = "org/repo@abc123/Model-A-Q4_K_M.gguf";
    const REF_B: &str = "org/repo@abc123/Model-B-Q4_K_M.gguf";

    struct Fixture {
        _root: tempfile::TempDir,
        _import: tempfile::TempDir,
        cache: MirrorCache,
        import_dir: PathBuf,
    }

    async fn fixture(cap: u64) -> Fixture {
        let root = tempfile::tempdir().expect("cache dir");
        let import = tempfile::tempdir().expect("import dir");
        let import_dir = import.path().to_path_buf();
        let cache = MirrorCache::open(options(root.path(), &import_dir, cap))
            .await
            .expect("cache opens");
        Fixture {
            _root: root,
            _import: import,
            cache,
            import_dir,
        }
    }

    async fn write_source(fixture: &Fixture, name: &str, bytes: &[u8]) -> PathBuf {
        let path = fixture.import_dir.join(name);
        tokio::fs::write(&path, bytes).await.expect("write source");
        path
    }

    #[test]
    fn percent_reports_an_empty_artifact_as_complete() {
        assert_eq!(percent(0, 0), 100.0);
        assert_eq!(percent(0, 10), 0.0);
        assert_eq!(percent(5, 10), 50.0);
        assert_eq!(percent(10, 10), 100.0);
    }

    #[test]
    fn base64_bound_is_an_upper_bound() {
        assert_eq!(base64_decoded_upper_bound(0), 0);
        assert_eq!(base64_decoded_upper_bound(4), 3);
        assert_eq!(base64_decoded_upper_bound(8), 6);
        // A padded encoding decodes to fewer bytes than the bound, never more.
        let encoded = BASE64.encode([1_u8, 2, 3, 4]);
        assert!(base64_decoded_upper_bound(encoded.len()) >= 4);
    }

    #[tokio::test]
    async fn a_mirror_with_no_disk_allowance_refuses_instead_of_pretending() {
        let fixture = fixture(0).await;
        let source = write_source(&fixture, "model.gguf", b"payload").await;

        let error = fixture
            .cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect_err("no disk allowance");

        assert!(matches!(error, MirrorError::Capacity(_)), "{error}");
        let status = fixture.cache.status().await;
        assert!(!status.serving);
        assert!(status.not_serving_reason.is_some());
    }

    #[tokio::test]
    async fn import_records_identity_and_digest() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "model.gguf", b"payload").await;

        let report = fixture
            .cache
            .import(&source, Some(REF_A), None, true)
            .await
            .expect("import succeeds");

        assert_eq!(report.entry.canonical_ref, REF_A);
        assert_eq!(report.entry.repo, "org/repo");
        assert_eq!(report.entry.revision, "abc123");
        assert_eq!(report.entry.model_id, "org/repo:Q4_K_M");
        assert_eq!(report.entry.sha256, Sha256Hex::of_bytes(b"payload"));
        assert_eq!(report.entry.size_bytes, 7);
        assert!(report.entry.pinned);
        assert_eq!(report.entry.state, EntryState::Ready);
    }

    #[tokio::test]
    async fn import_refuses_a_file_outside_the_import_roots() {
        let fixture = fixture(1_000).await;
        let outside = tempfile::tempdir().expect("outside dir");
        let path = outside.path().join("model.gguf");
        tokio::fs::write(&path, b"payload").await.expect("write");

        let error = fixture
            .cache
            .import(&path, Some(REF_A), None, false)
            .await
            .expect_err("outside the root");

        assert!(
            matches!(&error, MirrorError::Invalid(message) if message.contains("import roots")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn import_refuses_a_digest_that_does_not_match() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "model.gguf", b"payload").await;
        let wrong = Sha256Hex::of_bytes(b"something else").to_string();

        let error = fixture
            .cache
            .import(&source, Some(REF_A), Some(&wrong), false)
            .await
            .expect_err("digest mismatch");

        assert!(matches!(error, MirrorError::Integrity(_)), "{error}");
        assert!(fixture.cache.entries().is_empty());
    }

    #[tokio::test]
    async fn read_chunk_is_resumable_and_carries_a_per_chunk_digest() {
        let fixture = fixture(1_000).await;
        let payload: Vec<u8> = (0..200_u32).map(|value| value as u8).collect();
        let source = write_source(&fixture, "model.gguf", &payload).await;
        fixture
            .cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");

        let mut assembled = Vec::new();
        let mut offset = 0;
        loop {
            let chunk = fixture
                .cache
                .read_chunk(REF_A, offset, Some(64))
                .await
                .expect("chunk");
            let bytes = BASE64.decode(chunk.data_base64).expect("base64");
            assert_eq!(
                chunk.chunk_sha256,
                Sha256Hex::of_bytes(&bytes).to_string(),
                "each chunk carries its own digest"
            );
            assembled.extend_from_slice(&bytes);
            offset += chunk.length;
            if chunk.eof {
                break;
            }
        }

        assert_eq!(assembled, payload);
        assert_eq!(
            Sha256Hex::of_bytes(&assembled),
            Sha256Hex::of_bytes(&payload)
        );
    }

    #[tokio::test]
    async fn read_chunk_refuses_an_artifact_that_changed_size_underneath_it() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "model.gguf", b"original bytes").await;
        let report = fixture
            .cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");

        // Substitute the blob the way a compromised host would.
        let key = ArtifactId::parse(REF_A).expect("ref").cache_key();
        tokio::fs::write(fixture.cache.blob_path(&key), b"substituted")
            .await
            .expect("tamper");

        let error = fixture
            .cache
            .read_chunk(REF_A, 0, Some(64))
            .await
            .expect_err("tamper is caught");

        assert!(matches!(error, MirrorError::Integrity(_)), "{error}");
        let entry = fixture
            .cache
            .entries()
            .into_iter()
            .find(|entry| entry.canonical_ref == report.entry.canonical_ref)
            .expect("entry survives so the operator can see what happened");
        assert_eq!(entry.state, EntryState::Quarantined);
        assert!(fixture.cache.ready_entries().is_empty());
    }

    #[tokio::test]
    async fn read_re_verifies_and_catches_a_same_length_substitution() {
        let root = tempfile::tempdir().expect("cache dir");
        let import = tempfile::tempdir().expect("import dir");
        let mut settings = options(root.path(), import.path(), 1_000);
        // Re-digest on every transfer start: the size and mtime tripwire
        // cannot see a same-length swap, so only the digest can.
        settings.reverify_after_secs = 0;
        let cache = MirrorCache::open(settings).await.expect("cache opens");
        let source = import.path().join("model.gguf");
        tokio::fs::write(&source, b"the genuine weights")
            .await
            .expect("write");
        cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");

        let key = ArtifactId::parse(REF_A).expect("ref").cache_key();
        let blob = cache.blob_path(&key);
        let substitute = b"the sneaky weights!";
        assert_eq!(substitute.len(), b"the genuine weights".len());
        tokio::fs::write(&blob, substitute).await.expect("tamper");
        // Restore the recorded mtime so only the digest can tell the
        // difference; this is the worst case an attacker can arrange.
        let recorded = cache
            .entries()
            .into_iter()
            .find(|entry| entry.canonical_ref == REF_A)
            .expect("entry");
        cache
            .mutate_entry(&key, move |entry| entry.mtime_secs = 0)
            .await;
        assert_eq!(recorded.size_bytes, substitute.len() as u64);

        let error = cache
            .read_chunk(REF_A, 0, Some(64))
            .await
            .expect_err("re-verification catches it");

        assert!(matches!(error, MirrorError::Integrity(_)), "{error}");
        assert!(cache.ready_entries().is_empty());
    }

    #[tokio::test]
    async fn a_full_transfer_verifies_end_to_end() {
        let fixture = fixture(1_000).await;
        let payload = b"a fairly convincing model file";
        let digest = Sha256Hex::of_bytes(payload).to_string();

        let begun = fixture
            .cache
            .begin_receive(REF_A, &digest, payload.len() as u64)
            .await
            .expect("begin");
        assert_eq!(begun.received_bytes, 0);
        assert!(!begun.already_held);

        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + 16).min(payload.len());
            let slice = &payload[offset..end];
            let progress = fixture
                .cache
                .receive_chunk(
                    REF_A,
                    offset as u64,
                    &BASE64.encode(slice),
                    Some(&Sha256Hex::of_bytes(slice).to_string()),
                )
                .await
                .expect("chunk accepted");
            assert_eq!(progress.received_bytes, end as u64);
            offset = end;
        }

        let report = fixture
            .cache
            .finalize_receive(REF_A)
            .await
            .expect("finalize");
        assert_eq!(report.entry.sha256.to_string(), digest);
        assert_eq!(report.entry.state, EntryState::Ready);
    }

    #[tokio::test]
    async fn an_interrupted_transfer_resumes_from_where_it_stopped() {
        let fixture = fixture(1_000).await;
        let payload = b"0123456789abcdef";
        let digest = Sha256Hex::of_bytes(payload).to_string();

        fixture
            .cache
            .begin_receive(REF_A, &digest, payload.len() as u64)
            .await
            .expect("begin");
        fixture
            .cache
            .receive_chunk(REF_A, 0, &BASE64.encode(&payload[..6]), None)
            .await
            .expect("first chunk");

        // A second begin_receive is what a peer does after a dropped
        // connection; it must report the resume offset, not restart.
        let resumed = fixture
            .cache
            .begin_receive(REF_A, &digest, payload.len() as u64)
            .await
            .expect("resume");
        assert_eq!(resumed.received_bytes, 6);
        assert!(!resumed.complete);

        fixture
            .cache
            .receive_chunk(REF_A, 6, &BASE64.encode(&payload[6..]), None)
            .await
            .expect("rest");
        let report = fixture
            .cache
            .finalize_receive(REF_A)
            .await
            .expect("finalize");
        assert_eq!(report.entry.size_bytes, payload.len() as u64);
    }

    #[tokio::test]
    async fn a_substituted_transfer_is_discarded_not_published() {
        let fixture = fixture(1_000).await;
        let promised = b"the model the caller asked for";
        let delivered = b"the model an attacker sent!!!!";
        assert_eq!(promised.len(), delivered.len());
        let digest = Sha256Hex::of_bytes(promised).to_string();

        fixture
            .cache
            .begin_receive(REF_A, &digest, promised.len() as u64)
            .await
            .expect("begin");
        fixture
            .cache
            .receive_chunk(REF_A, 0, &BASE64.encode(delivered), None)
            .await
            .expect("bytes are accepted into staging");

        let error = fixture
            .cache
            .finalize_receive(REF_A)
            .await
            .expect_err("finalize catches the substitution");

        assert!(matches!(error, MirrorError::Integrity(_)), "{error}");
        assert!(
            fixture.cache.entries().is_empty(),
            "a substituted artifact must never be published"
        );
        let error = fixture
            .cache
            .read_chunk(REF_A, 0, None)
            .await
            .expect_err("nothing to serve");
        assert!(matches!(error, MirrorError::NotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn receive_chunk_rejects_a_chunk_whose_declared_digest_is_wrong() {
        let fixture = fixture(1_000).await;
        let payload = b"0123456789";
        let digest = Sha256Hex::of_bytes(payload).to_string();
        fixture
            .cache
            .begin_receive(REF_A, &digest, payload.len() as u64)
            .await
            .expect("begin");

        let error = fixture
            .cache
            .receive_chunk(
                REF_A,
                0,
                &BASE64.encode(b"0123456789"),
                Some(&Sha256Hex::of_bytes(b"different").to_string()),
            )
            .await
            .expect_err("chunk digest mismatch");

        assert!(matches!(error, MirrorError::Integrity(_)), "{error}");
    }

    #[tokio::test]
    async fn transfers_are_append_only() {
        let fixture = fixture(1_000).await;
        let payload = b"0123456789";
        let digest = Sha256Hex::of_bytes(payload).to_string();
        fixture
            .cache
            .begin_receive(REF_A, &digest, payload.len() as u64)
            .await
            .expect("begin");

        let error = fixture
            .cache
            .receive_chunk(REF_A, 4, &BASE64.encode(b"4567"), None)
            .await
            .expect_err("gap refused");

        assert!(
            matches!(&error, MirrorError::Invalid(message) if message.contains("append-only")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_transfer_that_cannot_possibly_fit_fails_before_any_bytes_move() {
        let fixture = fixture(100).await;

        let error = fixture
            .cache
            .begin_receive(REF_A, &Sha256Hex::of_bytes(b"x").to_string(), 5_000)
            .await
            .expect_err("too large");

        assert!(matches!(error, MirrorError::Capacity(_)), "{error}");
    }

    #[tokio::test]
    async fn admitting_an_artifact_evicts_the_least_recently_used_one() {
        let fixture = fixture(20).await;
        let first = write_source(&fixture, "a.gguf", &[1_u8; 10]).await;
        let second = write_source(&fixture, "b.gguf", &[2_u8; 10]).await;
        fixture
            .cache
            .import(&first, Some(REF_A), None, false)
            .await
            .expect("first import");
        fixture
            .cache
            .import(&second, Some(REF_B), None, false)
            .await
            .expect("second import");

        let third = write_source(&fixture, "c.gguf", &[3_u8; 10]).await;
        let report = fixture
            .cache
            .import(
                &third,
                Some("org/repo@abc123/Model-C-Q4_K_M.gguf"),
                None,
                false,
            )
            .await
            .expect("third import evicts");

        assert_eq!(report.evicted.len(), 1);
        assert_eq!(fixture.cache.entries().len(), 2);
    }

    #[tokio::test]
    async fn pinned_artifacts_survive_eviction_pressure() {
        let fixture = fixture(20).await;
        let first = write_source(&fixture, "a.gguf", &[1_u8; 10]).await;
        let second = write_source(&fixture, "b.gguf", &[2_u8; 10]).await;
        fixture
            .cache
            .import(&first, Some(REF_A), None, true)
            .await
            .expect("pinned import");
        fixture
            .cache
            .import(&second, Some(REF_B), None, true)
            .await
            .expect("pinned import");

        let third = write_source(&fixture, "c.gguf", &[3_u8; 10]).await;
        let error = fixture
            .cache
            .import(
                &third,
                Some("org/repo@abc123/Model-C-Q4_K_M.gguf"),
                None,
                false,
            )
            .await
            .expect_err("everything is pinned");

        assert!(
            matches!(&error, MirrorError::Capacity(message) if message.contains("pinned")),
            "{error}"
        );
        assert_eq!(fixture.cache.entries().len(), 2);
    }

    #[tokio::test]
    async fn the_bandwidth_cap_shortens_a_chunk_rather_than_dropping_it() {
        let root = tempfile::tempdir().expect("cache dir");
        let import = tempfile::tempdir().expect("import dir");
        let mut options = options(root.path(), import.path(), 1_000);
        options.serve_bytes_per_minute = 32;
        let cache = MirrorCache::open(options).await.expect("cache opens");
        let source = import.path().join("model.gguf");
        tokio::fs::write(&source, vec![7_u8; 200])
            .await
            .expect("write source");
        cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");

        let chunk = cache.read_chunk(REF_A, 0, Some(64)).await.expect("chunk");
        assert_eq!(chunk.length, 32, "the chunk is trimmed to the budget");
        assert!(chunk.throttled);
        assert!(!chunk.eof);

        let error = cache
            .read_chunk(REF_A, 32, Some(64))
            .await
            .expect_err("budget is spent");
        assert!(
            matches!(error, MirrorError::Throttled { .. }),
            "an exhausted budget is an error, never an empty success"
        );
    }

    #[tokio::test]
    async fn verify_re_digests_and_reports() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "model.gguf", b"payload").await;
        fixture
            .cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");

        let report = fixture.cache.verify(REF_A).await.expect("verify");

        assert!(report.verified);
        assert_eq!(report.expected_sha256, report.actual_sha256);
        assert_eq!(report.size_bytes, 7);
    }

    #[tokio::test]
    async fn a_restart_quarantines_an_artifact_that_changed_while_the_node_was_down() {
        let root = tempfile::tempdir().expect("cache dir");
        let import = tempfile::tempdir().expect("import dir");
        let source = import.path().join("model.gguf");
        tokio::fs::write(&source, b"original").await.expect("write");
        let cache = MirrorCache::open(options(root.path(), import.path(), 1_000))
            .await
            .expect("cache opens");
        cache
            .import(&source, Some(REF_A), None, false)
            .await
            .expect("import");
        let key = ArtifactId::parse(REF_A).expect("ref").cache_key();
        tokio::fs::write(cache.blob_path(&key), b"tampered with while offline")
            .await
            .expect("tamper");
        drop(cache);

        let reopened = MirrorCache::open(options(root.path(), import.path(), 1_000))
            .await
            .expect("cache reopens");

        let entry = reopened
            .entries()
            .into_iter()
            .find(|entry| entry.canonical_ref == REF_A)
            .expect("entry is kept for the operator to see");
        assert_eq!(entry.state, EntryState::Quarantined);
        assert!(reopened.ready_entries().is_empty());
    }

    #[tokio::test]
    async fn eviction_and_pinning_are_operator_controllable() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "model.gguf", b"payload").await;
        fixture
            .cache
            .import(&source, Some(REF_A), None, true)
            .await
            .expect("import");

        let error = fixture
            .cache
            .evict(Some(REF_A), None, false)
            .await
            .expect_err("pinned");
        assert!(matches!(error, MirrorError::Invalid(_)), "{error}");

        fixture.cache.set_pinned(REF_A, false).await.expect("unpin");
        let report = fixture
            .cache
            .evict(Some(REF_A), None, false)
            .await
            .expect("evict");

        assert_eq!(report.evicted, vec![REF_A.to_string()]);
        assert_eq!(report.freed_bytes, 7);
        assert!(fixture.cache.entries().is_empty());
    }

    #[tokio::test]
    async fn import_can_derive_identity_from_a_hugging_face_snapshot_path() {
        let root = tempfile::tempdir().expect("cache dir");
        let import = tempfile::tempdir().expect("import dir");
        let snapshot = import
            .path()
            .join("models--org--repo")
            .join("snapshots")
            .join("abc123");
        tokio::fs::create_dir_all(&snapshot)
            .await
            .expect("snapshot layout");
        let source = snapshot.join("Qwen3-8B-Q4_K_M.gguf");
        tokio::fs::write(&source, b"weights").await.expect("write");
        let cache = MirrorCache::open(options(root.path(), import.path(), 1_000))
            .await
            .expect("cache opens");

        let report = cache
            .import(&source, None, None, false)
            .await
            .expect("identity is derived");

        assert_eq!(report.entry.repo, "org/repo");
        assert_eq!(report.entry.revision, "abc123");
        assert_eq!(report.entry.file, "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(report.entry.model_id, "org/repo:Q4_K_M");
    }

    #[tokio::test]
    async fn import_says_so_when_it_cannot_derive_an_identity() {
        let fixture = fixture(1_000).await;
        let source = write_source(&fixture, "mystery.gguf", b"weights").await;

        let error = fixture
            .cache
            .import(&source, None, None, false)
            .await
            .expect_err("no identity");

        assert!(
            matches!(&error, MirrorError::Invalid(message) if message.contains("canonical_ref")),
            "{error}"
        );
    }
}
