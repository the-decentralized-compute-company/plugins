//! The collections themselves: durable on disk, brute-force in memory.
//!
//! # Where the data lives
//!
//! One append-only JSONL log per collection, under
//! `<data-dir>/collections/<name>.jsonl`. The first line is a header pinning
//! the collection's embedding model and dimensions; every line after it is a
//! `put` or a `delete`. Replaying the log rebuilds the collection, so the
//! store survives a restart, a crash mid-write (the torn final line is
//! discarded, not guessed at), and being copied to another machine.
//!
//! Every mutation is one append plus one `sync_all`. Deletes leave tombstones,
//! and the log is compacted — written to a temporary file and renamed over the
//! original — once the dead weight passes [`COMPACT_MIN_LINES`] lines and
//! [`COMPACT_DEAD_RATIO`].
//!
//! It is JSON, deliberately, with the vectors as plain arrays of numbers. A
//! packed binary format would be about half the size, but the person whose
//! machine this runs on can read JSONL with `head` and see exactly what was
//! stored about them, and for a store this size that is worth more than the
//! bytes. The cost is stated in the README rather than hidden: roughly 10 KB
//! per chunk at 768 dimensions.
//!
//! # Search is a brute-force cosine scan, and that is a choice
//!
//! Every query normalizes its vector and compares it against every live chunk
//! in one collection. At 768 dimensions a scan of 50 000 chunks is about
//! 38 million multiply-adds — tens of milliseconds — and it is *exact*: the
//! nearest neighbour is the nearest neighbour, not the nearest one an index
//! happened to visit. An approximate index would be faster and would introduce
//! a recall parameter that silently drops results, which is a much harder
//! thing to trust and a much harder thing to debug when retrieval goes wrong.
//!
//! The honest ceiling is stated in [`DEFAULT_MAX_CHUNKS_PER_COLLECTION`] and
//! in the README: past a few tens of thousands of chunks per collection, this
//! design is the wrong one and you want a real vector database.
//!
//! # Embedding spaces are never mixed
//!
//! A collection pins the embedding model that created it, in its header. An
//! `upsert` or a `query` using a different model is **refused**, naming both
//! models and the two ways out. Two embedders produce coordinates in unrelated
//! spaces, and a cosine between them is a plausible-looking number that means
//! nothing — the single most expensive mistake a store like this can make,
//! because it fails silently and looks like a quality problem with the model.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::names::{CollectionName, NameError, collection_path, collections_dir, name_from_file};
use crate::similarity::{cosine_similarity, vector_bytes};

/// On-disk format marker. Read on load; a log written by a future version with
/// a higher number is refused rather than misread.
pub const FORMAT_VERSION: u32 = 1;

/// Chunks per collection before the brute-force scan stops being the right
/// tool. Enforced as a hard cap, not a warning: an unbounded store on hardware
/// somebody lent you is a denial of service you shipped.
pub const DEFAULT_MAX_CHUNKS_PER_COLLECTION: usize = 50_000;

/// Below this many lines a log is small enough that compaction costs more than
/// the tombstones do.
pub const COMPACT_MIN_LINES: u64 = 512;

/// Fraction of a log that must be dead before it is rewritten.
pub const COMPACT_DEAD_RATIO: f64 = 0.3;

#[derive(Debug)]
pub enum StoreError {
    /// The collection name did not survive validation.
    Name(NameError),
    /// The caller's embedding model is not the one this collection was built
    /// with. The most important error in this file.
    ModelMismatch {
        collection: String,
        pinned_model: String,
        pinned_dimensions: usize,
        offered_model: String,
    },
    /// The embedder returned a vector of a different width than the collection
    /// was built with, while claiming the same model id.
    DimensionMismatch {
        collection: String,
        pinned_dimensions: usize,
        offered_dimensions: usize,
    },
    NoSuchCollection {
        collection: String,
    },
    CollectionLimit {
        limit: usize,
    },
    ChunkLimit {
        collection: String,
        limit: usize,
    },
    ByteLimit {
        limit_bytes: u64,
        would_be_bytes: u64,
    },
    /// A log on disk is unreadable or internally inconsistent. Never repaired
    /// silently: a store that quietly drops half a collection is worse than
    /// one that refuses to start.
    Corrupt {
        collection: String,
        detail: String,
    },
    Io {
        operation: &'static str,
        detail: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(error) => write!(formatter, "{error}"),
            Self::ModelMismatch {
                collection,
                pinned_model,
                pinned_dimensions,
                offered_model,
            } => write!(
                formatter,
                "collection {collection:?} holds vectors from embedding model \
                 {pinned_model:?} ({pinned_dimensions} dimensions) and this process is \
                 configured for {offered_model:?}. Vectors from two models are not \
                 comparable, so this is refused rather than answered with a confident \
                 wrong ranking. Either set --embedding-model back to {pinned_model:?}, \
                 or delete the collection and rebuild it with the new model."
            ),
            Self::DimensionMismatch {
                collection,
                pinned_dimensions,
                offered_dimensions,
            } => write!(
                formatter,
                "collection {collection:?} holds {pinned_dimensions}-dimension vectors but \
                 the embeddings endpoint returned {offered_dimensions} dimensions for the \
                 same model id. The endpoint is serving a different model than it was when \
                 this collection was built; delete and rebuild the collection, or point \
                 --embeddings-url back at the original server."
            ),
            Self::NoSuchCollection { collection } => write!(
                formatter,
                "no collection named {collection:?}; `stats` lists the collections that exist"
            ),
            Self::CollectionLimit { limit } => write!(
                formatter,
                "this node holds the maximum of {limit} collections; delete one or raise \
                 --max-collections"
            ),
            Self::ChunkLimit { collection, limit } => write!(
                formatter,
                "collection {collection:?} is at its limit of {limit} chunks. A brute-force \
                 cosine scan is the wrong tool much past this size; delete some documents, \
                 split the corpus across collections, or raise \
                 --max-chunks-per-collection knowing that queries get linearly slower."
            ),
            Self::ByteLimit {
                limit_bytes,
                would_be_bytes,
            } => write!(
                formatter,
                "storing this would take the store to about {would_be_bytes} bytes, past the \
                 {limit_bytes}-byte limit; delete something or raise --max-store-bytes"
            ),
            Self::Corrupt { collection, detail } => write!(
                formatter,
                "collection {collection:?} could not be loaded: {detail}. The file was not \
                 modified. Move it aside to start that collection again."
            ),
            Self::Io { operation, detail } => {
                write!(formatter, "{operation} failed: {detail}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<NameError> for StoreError {
    fn from(error: NameError) -> Self {
        Self::Name(error)
    }
}

fn io<T>(operation: &'static str, error: std::io::Error) -> Result<T, StoreError> {
    Err(StoreError::Io {
        operation,
        detail: error.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One stored passage: the vector, the text, and everything a citation needs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ChunkRecord {
    /// `<document_id>#<chunk_index>`. Stable across re-upserts of the same
    /// document, so replacing a document replaces its chunks rather than
    /// accumulating duplicates.
    pub id: String,
    pub document_id: String,
    /// Where the text came from, verbatim as the caller gave it. Never opened,
    /// never resolved — see the `upsert` documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub chunk_index: u32,
    /// 1-based, inclusive.
    pub line_start: u32,
    /// 1-based, inclusive.
    pub line_end: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// L2-normalized, so a cosine similarity is a dot product.
    pub embedding: Vec<f32>,
    /// Recorded per chunk as well as in the header. The header is the
    /// enforcement point; this is what catches a hand-edited or concatenated
    /// log at load time.
    pub embedding_model: String,
    pub created_unix_ms: u64,
}

impl ChunkRecord {
    pub fn dimensions(&self) -> usize {
        self.embedding.len()
    }

    /// Estimated bytes held in memory for this record. An estimate, and
    /// labelled as one everywhere it surfaces — never process memory.
    pub fn approx_bytes(&self) -> u64 {
        const RECORD_OVERHEAD_BYTES: u64 = 256;
        RECORD_OVERHEAD_BYTES
            + self.id.len() as u64
            + self.document_id.len() as u64
            + self.source.as_ref().map_or(0, |s| s.len() as u64)
            + self.text.len() as u64
            + self
                .heading_path
                .iter()
                .map(|entry| entry.len() as u64)
                .sum::<u64>()
            + self
                .metadata
                .iter()
                .map(|(key, value)| (key.len() + value.len()) as u64)
                .sum::<u64>()
            + vector_bytes(self.embedding.len())
    }
}

/// The first line of every log.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CollectionHeader {
    pub format_version: u32,
    pub collection: String,
    /// The embedding model this collection is pinned to. Comparing against
    /// anything else is refused.
    pub embedding_model: String,
    pub dimensions: usize,
    pub created_unix_ms: u64,
}

/// One line of a collection log.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LogLine {
    Header(CollectionHeader),
    Put { chunk: Box<ChunkRecord> },
    Delete { id: String },
    DeleteDocument { document_id: String },
}

// ---------------------------------------------------------------------------
// In-memory collection
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Collection {
    header: CollectionHeader,
    /// Keyed by chunk id, so a re-upsert of one document replaces exactly its
    /// own chunks.
    chunks: BTreeMap<String, ChunkRecord>,
    path: PathBuf,
    /// Lines written to the log, live or dead. Drives compaction.
    log_lines: u64,
    /// Lines that no longer contribute to the current state.
    dead_lines: u64,
    approx_bytes: u64,
}

impl Collection {
    fn documents(&self) -> BTreeSet<&str> {
        self.chunks
            .values()
            .map(|chunk| chunk.document_id.as_str())
            .collect()
    }
}

/// What a query is allowed to narrow by.
///
/// Every filter is string equality or a string prefix. There is deliberately
/// no expression language: a filter that a model can compose from a document
/// it just read is a filter that can be made to do something surprising, and
/// exact matching on operator-chosen keys covers the cases a retriever
/// actually needs.
#[derive(Clone, Debug, Default)]
pub struct QueryFilter {
    /// Every entry must match exactly. An AND, never an OR.
    pub metadata: BTreeMap<String, String>,
    /// Keep only chunks whose `source` starts with this.
    pub source_prefix: Option<String>,
    /// Keep only chunks from these documents.
    pub document_ids: Vec<String>,
}

impl QueryFilter {
    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty() && self.source_prefix.is_none() && self.document_ids.is_empty()
    }

    fn matches(&self, chunk: &ChunkRecord) -> bool {
        for (key, value) in &self.metadata {
            if chunk.metadata.get(key).map(String::as_str) != Some(value.as_str()) {
                return false;
            }
        }
        if let Some(prefix) = &self.source_prefix {
            match &chunk.source {
                Some(source) if source.starts_with(prefix) => {}
                _ => return false,
            }
        }
        if !self.document_ids.is_empty() && !self.document_ids.contains(&chunk.document_id) {
            return false;
        }
        true
    }
}

/// One result of a similarity query.
#[derive(Clone, Debug, Serialize)]
pub struct ScoredChunk {
    pub score: f64,
    #[serde(flatten)]
    pub chunk: ChunkRecordView,
}

/// A chunk as a caller sees it: everything except the vector.
///
/// The embedding is 768 or 3072 numbers of no use to the caller and would
/// dominate every tool response, so it is not projected.
#[derive(Clone, Debug, Serialize)]
pub struct ChunkRecordView {
    pub id: String,
    pub document_id: String,
    pub source: Option<String>,
    pub chunk_index: u32,
    pub line_start: u32,
    pub line_end: u32,
    pub heading_path: Vec<String>,
    pub text: String,
    pub metadata: BTreeMap<String, String>,
    /// A `path:line-line` string when a source is known, so a citation can be
    /// copied straight out of the response.
    pub citation: Option<String>,
}

impl From<&ChunkRecord> for ChunkRecordView {
    fn from(record: &ChunkRecord) -> Self {
        Self {
            id: record.id.clone(),
            document_id: record.document_id.clone(),
            source: record.source.clone(),
            chunk_index: record.chunk_index,
            line_start: record.line_start,
            line_end: record.line_end,
            heading_path: record.heading_path.clone(),
            text: record.text.clone(),
            metadata: record.metadata.clone(),
            citation: record.source.as_ref().map(|source| {
                if record.line_start == record.line_end {
                    format!("{source}:{}", record.line_start)
                } else {
                    format!("{source}:{}-{}", record.line_start, record.line_end)
                }
            }),
        }
    }
}

/// Per-collection statistics, as reported by the `stats` tool.
#[derive(Clone, Debug, Serialize)]
pub struct CollectionStats {
    pub collection: String,
    pub embedding_model: String,
    pub dimensions: usize,
    pub chunks: usize,
    pub documents: usize,
    /// Estimated bytes held in memory. Not process memory.
    pub approx_memory_bytes: u64,
    /// Actual size of the log on disk, including tombstones.
    pub log_bytes: u64,
    pub log_lines: u64,
    pub dead_log_lines: u64,
    pub created_unix_ms: u64,
    /// Distinct `source` values, capped so one pathological collection cannot
    /// return a megabyte of strings.
    pub sources: Vec<String>,
    pub sources_truncated: bool,
    /// True once the collection is close enough to the cap that the operator
    /// should be thinking about it.
    pub near_capacity: bool,
}

/// Cap on the `sources` list in a stats response.
const MAX_REPORTED_SOURCES: usize = 100;

/// Bounds the store enforces on every write.
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    pub max_collections: usize,
    pub max_chunks_per_collection: usize,
    pub max_store_bytes: u64,
}

/// What one `upsert` did.
#[derive(Clone, Debug, Serialize)]
pub struct UpsertOutcome {
    pub chunks_written: usize,
    /// Chunks removed because a previous version of the same document had more
    /// of them.
    pub chunks_replaced: usize,
    pub compacted: bool,
}

/// What one `delete` did.
#[derive(Clone, Debug, Serialize)]
pub struct DeleteOutcome {
    pub chunks_deleted: usize,
    pub documents_deleted: usize,
    pub collection_removed: bool,
    pub compacted: bool,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

pub struct VectorStore {
    root: PathBuf,
    limits: StoreLimits,
    inner: Mutex<BTreeMap<CollectionName, Collection>>,
    /// Kept outside the mutex so the health hook can answer while a scan or a
    /// compaction holds the lock. Health has to stay fast and independent of
    /// long-running work.
    collection_count: AtomicU64,
    chunk_count: AtomicU64,
}

/// `Debug` is written by hand rather than derived.
///
/// The derived form would print every stored passage — which is to say, other
/// people's documents — the first time somebody `{:?}`s the store while
/// debugging a startup problem. Shape and counts are what is useful there
/// anyway.
impl fmt::Debug for VectorStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (collections, chunks) = self.counts();
        formatter
            .debug_struct("VectorStore")
            .field("root", &crate::names::display_path(&self.root))
            .field("collections", &collections)
            .field("chunks", &chunks)
            .finish_non_exhaustive()
    }
}

impl VectorStore {
    /// Open (or create) a store rooted at an existing, canonical directory.
    ///
    /// Every collection log in the directory is replayed now rather than
    /// lazily, so a corrupt file is a startup failure an operator sees in the
    /// host log, not a query failure three days later.
    pub fn open(root: &Path, limits: StoreLimits) -> Result<Self, StoreError> {
        let directory = collections_dir(root);
        if let Err(error) = std::fs::create_dir_all(&directory) {
            return io("creating the collections directory", error);
        }

        let mut collections = BTreeMap::new();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => return io("reading the collections directory", error),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return io("reading a collections directory entry", error),
            };
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(name) = name_from_file(&file_name) else {
                continue;
            };
            let path = collection_path(root, &name)?;
            let collection = load_collection(&name, &path)?;
            collections.insert(name, collection);
        }

        let store = Self {
            root: root.to_path_buf(),
            limits,
            inner: Mutex::new(collections),
            collection_count: AtomicU64::new(0),
            chunk_count: AtomicU64::new(0),
        };
        store.refresh_counters();
        Ok(store)
    }

    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A poisoned lock means a handler panicked mid-update. Each collection is
    /// rebuilt from its log at startup and every mutation is written before
    /// the in-memory map is updated, so recovering keeps the node serving
    /// rather than failing every later request.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<CollectionName, Collection>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refresh_counters(&self) {
        let inner = self.lock();
        self.collection_count
            .store(inner.len() as u64, Ordering::Relaxed);
        self.chunk_count.store(
            inner
                .values()
                .map(|collection| collection.chunks.len() as u64)
                .sum(),
            Ordering::Relaxed,
        );
    }

    /// Cheap, lock-free counts for the health hook.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.collection_count.load(Ordering::Relaxed),
            self.chunk_count.load(Ordering::Relaxed),
        )
    }

    /// Every collection with the embedding model and width it is locked to.
    ///
    /// Cheap: it reads headers, never chunks. `status` uses it to say outright
    /// which collections the current configuration can actually query.
    pub fn collection_pins(&self) -> Vec<(String, String, usize)> {
        self.lock()
            .iter()
            .map(|(name, collection)| {
                (
                    name.as_str().to_string(),
                    collection.header.embedding_model.clone(),
                    collection.header.dimensions,
                )
            })
            .collect()
    }

    /// Refuse a model that is not the one this collection was built with.
    ///
    /// Called before embedding anything, so a mismatched configuration costs
    /// no embedding calls and the error names the fix.
    pub fn check_model(
        &self,
        name: &CollectionName,
        model: &str,
        dimensions: Option<usize>,
    ) -> Result<(), StoreError> {
        let inner = self.lock();
        let Some(collection) = inner.get(name) else {
            return Ok(());
        };
        if collection.header.embedding_model != model {
            return Err(StoreError::ModelMismatch {
                collection: name.as_str().to_string(),
                pinned_model: collection.header.embedding_model.clone(),
                pinned_dimensions: collection.header.dimensions,
                offered_model: model.to_string(),
            });
        }
        if let Some(dimensions) = dimensions
            && collection.header.dimensions != dimensions
        {
            return Err(StoreError::DimensionMismatch {
                collection: name.as_str().to_string(),
                pinned_dimensions: collection.header.dimensions,
                offered_dimensions: dimensions,
            });
        }
        Ok(())
    }

    /// Replace one document's chunks in a collection, creating the collection
    /// if it does not exist.
    ///
    /// Replacement, not addition: every existing chunk with this `document_id`
    /// is deleted first, so re-ingesting an edited file leaves no stale
    /// passages behind. That is the property that makes an index maintainable.
    pub fn upsert_document(
        &self,
        name: &CollectionName,
        document_id: &str,
        chunks: Vec<ChunkRecord>,
        embedding_model: &str,
    ) -> Result<UpsertOutcome, StoreError> {
        if chunks.is_empty() {
            return Ok(UpsertOutcome {
                chunks_written: 0,
                chunks_replaced: 0,
                compacted: false,
            });
        }
        let dimensions = chunks[0].dimensions();
        if let Some(odd) = chunks.iter().find(|chunk| chunk.dimensions() != dimensions) {
            return Err(StoreError::DimensionMismatch {
                collection: name.as_str().to_string(),
                pinned_dimensions: dimensions,
                offered_dimensions: odd.dimensions(),
            });
        }

        self.check_model(name, embedding_model, Some(dimensions))?;

        let mut inner = self.lock();

        if !inner.contains_key(name) && inner.len() >= self.limits.max_collections {
            return Err(StoreError::CollectionLimit {
                limit: self.limits.max_collections,
            });
        }

        if !inner.contains_key(name) {
            let header = CollectionHeader {
                format_version: FORMAT_VERSION,
                collection: name.as_str().to_string(),
                embedding_model: embedding_model.to_string(),
                dimensions,
                created_unix_ms: now_unix_ms(),
            };
            let path = collection_path(&self.root, name)?;
            create_log(&path, &header)?;
            inner.insert(
                name.clone(),
                Collection {
                    header,
                    chunks: BTreeMap::new(),
                    path,
                    log_lines: 1,
                    dead_lines: 0,
                    approx_bytes: 0,
                },
            );
        }

        let collection = inner.get_mut(name).expect("just inserted");

        let superseded: Vec<String> = collection
            .chunks
            .values()
            .filter(|chunk| chunk.document_id == document_id)
            .map(|chunk| chunk.id.clone())
            .collect();

        let live_after = collection.chunks.len() - superseded.len() + chunks.len();
        if live_after > self.limits.max_chunks_per_collection {
            return Err(StoreError::ChunkLimit {
                collection: name.as_str().to_string(),
                limit: self.limits.max_chunks_per_collection,
            });
        }

        let freed: u64 = superseded
            .iter()
            .filter_map(|id| collection.chunks.get(id))
            .map(ChunkRecord::approx_bytes)
            .sum();
        let added: u64 = chunks.iter().map(ChunkRecord::approx_bytes).sum();
        let total_now: u64 = inner
            .values()
            .map(|collection| collection.approx_bytes)
            .sum();
        let would_be = total_now.saturating_sub(freed).saturating_add(added);
        if would_be > self.limits.max_store_bytes {
            return Err(StoreError::ByteLimit {
                limit_bytes: self.limits.max_store_bytes,
                would_be_bytes: would_be,
            });
        }

        let collection = inner.get_mut(name).expect("present");

        // Write first, then mutate memory. A crash between the two leaves a
        // log that replays to exactly this state; the reverse order would lose
        // the write.
        let mut lines: Vec<LogLine> = Vec::with_capacity(chunks.len() + 1);
        if !superseded.is_empty() {
            lines.push(LogLine::DeleteDocument {
                document_id: document_id.to_string(),
            });
        }
        for chunk in &chunks {
            lines.push(LogLine::Put {
                chunk: Box::new(chunk.clone()),
            });
        }
        append_lines(&collection.path, &lines)?;
        collection.log_lines += lines.len() as u64;
        collection.dead_lines += superseded.len() as u64;
        if !superseded.is_empty() {
            // The delete marker itself becomes dead weight once compacted.
            collection.dead_lines += 1;
        }

        for id in &superseded {
            if let Some(removed) = collection.chunks.remove(id) {
                collection.approx_bytes = collection
                    .approx_bytes
                    .saturating_sub(removed.approx_bytes());
            }
        }
        let chunks_written = chunks.len();
        for chunk in chunks {
            collection.approx_bytes += chunk.approx_bytes();
            collection.chunks.insert(chunk.id.clone(), chunk);
        }

        let compacted = maybe_compact(collection)?;
        drop(inner);
        self.refresh_counters();

        Ok(UpsertOutcome {
            chunks_written,
            chunks_replaced: superseded.len(),
            compacted,
        })
    }

    /// Delete whole documents from one collection.
    pub fn delete_documents(
        &self,
        name: &CollectionName,
        document_ids: &[String],
    ) -> Result<DeleteOutcome, StoreError> {
        let mut inner = self.lock();
        let Some(collection) = inner.get_mut(name) else {
            return Err(StoreError::NoSuchCollection {
                collection: name.as_str().to_string(),
            });
        };

        let mut present: Vec<String> = Vec::new();
        for document_id in document_ids {
            if collection
                .chunks
                .values()
                .any(|chunk| &chunk.document_id == document_id)
            {
                present.push(document_id.clone());
            }
        }
        if present.is_empty() {
            return Ok(DeleteOutcome {
                chunks_deleted: 0,
                documents_deleted: 0,
                collection_removed: false,
                compacted: false,
            });
        }

        let lines: Vec<LogLine> = present
            .iter()
            .map(|document_id| LogLine::DeleteDocument {
                document_id: document_id.clone(),
            })
            .collect();
        append_lines(&collection.path, &lines)?;
        collection.log_lines += lines.len() as u64;

        let doomed: Vec<String> = collection
            .chunks
            .values()
            .filter(|chunk| present.contains(&chunk.document_id))
            .map(|chunk| chunk.id.clone())
            .collect();
        for id in &doomed {
            if let Some(removed) = collection.chunks.remove(id) {
                collection.approx_bytes = collection
                    .approx_bytes
                    .saturating_sub(removed.approx_bytes());
            }
        }
        collection.dead_lines += doomed.len() as u64 + lines.len() as u64;

        let compacted = maybe_compact(collection)?;
        drop(inner);
        self.refresh_counters();

        Ok(DeleteOutcome {
            chunks_deleted: doomed.len(),
            documents_deleted: present.len(),
            collection_removed: false,
            compacted,
        })
    }

    /// Delete an entire collection, log and all.
    ///
    /// Collections are namespaces: this removes exactly one file and touches
    /// no other collection's data.
    pub fn drop_collection(&self, name: &CollectionName) -> Result<DeleteOutcome, StoreError> {
        let mut inner = self.lock();
        let Some(collection) = inner.remove(name) else {
            return Err(StoreError::NoSuchCollection {
                collection: name.as_str().to_string(),
            });
        };
        let chunks_deleted = collection.chunks.len();
        let documents_deleted = collection.documents().len();

        if let Err(error) = std::fs::remove_file(&collection.path) {
            // Put it back rather than leaving memory and disk disagreeing.
            inner.insert(name.clone(), collection);
            return io("deleting the collection log", error);
        }
        drop(inner);
        self.refresh_counters();

        Ok(DeleteOutcome {
            chunks_deleted,
            documents_deleted,
            collection_removed: true,
            compacted: false,
        })
    }

    /// Exact nearest-neighbour search within one collection.
    ///
    /// Brute force: every live chunk that survives the filter is scored. The
    /// filter runs first, because rejecting on a metadata key is far cheaper
    /// than a 768-dimension dot product.
    pub fn query(
        &self,
        name: &CollectionName,
        embedding: &[f32],
        top_k: usize,
        min_score: f64,
        filter: &QueryFilter,
    ) -> Result<Vec<ScoredChunk>, StoreError> {
        let inner = self.lock();
        let Some(collection) = inner.get(name) else {
            return Err(StoreError::NoSuchCollection {
                collection: name.as_str().to_string(),
            });
        };

        let mut scored: Vec<(f64, &ChunkRecord)> = Vec::new();
        for chunk in collection.chunks.values() {
            if !filter.matches(chunk) {
                continue;
            }
            // `None` means the widths differ, which means a different
            // embedder. Skip rather than score: a number here would be
            // meaningless, and the header check should already have made this
            // unreachable.
            let Some(score) = cosine_similarity(embedding, &chunk.embedding) else {
                continue;
            };
            if score < min_score {
                continue;
            }
            scored.push((score, chunk));
        }

        // Descending by score, then by id so equal scores rank deterministically
        // — two runs of the same query must not disagree.
        scored.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        scored.truncate(top_k);

        Ok(scored
            .into_iter()
            .map(|(score, chunk)| ScoredChunk {
                score,
                chunk: ChunkRecordView::from(chunk),
            })
            .collect())
    }

    /// Per-collection statistics. No network, no writes.
    pub fn stats(&self, only: Option<&CollectionName>) -> Result<Vec<CollectionStats>, StoreError> {
        let inner = self.lock();
        if let Some(name) = only
            && !inner.contains_key(name)
        {
            return Err(StoreError::NoSuchCollection {
                collection: name.as_str().to_string(),
            });
        }

        let mut out = Vec::new();
        for (name, collection) in inner.iter() {
            if only.is_some_and(|wanted| wanted != name) {
                continue;
            }
            let mut sources: Vec<String> = collection
                .chunks
                .values()
                .filter_map(|chunk| chunk.source.clone())
                .collect();
            sources.sort_unstable();
            sources.dedup();
            let sources_truncated = sources.len() > MAX_REPORTED_SOURCES;
            sources.truncate(MAX_REPORTED_SOURCES);

            out.push(CollectionStats {
                collection: name.as_str().to_string(),
                embedding_model: collection.header.embedding_model.clone(),
                dimensions: collection.header.dimensions,
                chunks: collection.chunks.len(),
                documents: collection.documents().len(),
                approx_memory_bytes: collection.approx_bytes,
                log_bytes: std::fs::metadata(&collection.path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                log_lines: collection.log_lines,
                dead_log_lines: collection.dead_lines,
                created_unix_ms: collection.header.created_unix_ms,
                sources,
                sources_truncated,
                near_capacity: collection.chunks.len() * 10
                    >= self.limits.max_chunks_per_collection * 9,
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Log I/O
// ---------------------------------------------------------------------------

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn create_log(path: &Path, header: &CollectionHeader) -> Result<(), StoreError> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return io("creating the collections directory", error);
    }
    let mut file = match OpenOptions::new().create_new(true).append(true).open(path) {
        Ok(file) => file,
        Err(error) => return io("creating a collection log", error),
    };
    let line = match serde_json::to_string(&LogLine::Header(header.clone())) {
        Ok(line) => line,
        Err(error) => {
            return Err(StoreError::Io {
                operation: "encoding the collection header",
                detail: error.to_string(),
            });
        }
    };
    if let Err(error) = writeln!(file, "{line}") {
        return io("writing the collection header", error);
    }
    if let Err(error) = file.sync_all() {
        return io("flushing the collection header", error);
    }
    Ok(())
}

/// Append lines and flush them to the platter before returning.
///
/// One `sync_all` for the whole batch rather than one per line: the batch is a
/// single logical operation, and a torn tail is discarded on load anyway.
fn append_lines(path: &Path, lines: &[LogLine]) -> Result<(), StoreError> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut file = match OpenOptions::new().append(true).open(path) {
        Ok(file) => file,
        Err(error) => return io("opening a collection log for append", error),
    };
    let mut buffer = String::new();
    for line in lines {
        match serde_json::to_string(line) {
            Ok(encoded) => {
                buffer.push_str(&encoded);
                buffer.push('\n');
            }
            Err(error) => {
                return Err(StoreError::Io {
                    operation: "encoding a log line",
                    detail: error.to_string(),
                });
            }
        }
    }
    if let Err(error) = file.write_all(buffer.as_bytes()) {
        return io("appending to a collection log", error);
    }
    if let Err(error) = file.sync_all() {
        return io("flushing a collection log", error);
    }
    Ok(())
}

/// Replay one log into memory.
///
/// A line that does not parse is fatal **except** the final one: an
/// interrupted append leaves a partial line, and discarding exactly that is
/// the whole point of an append-only log. Anything else is corruption and is
/// reported rather than silently skipped.
fn load_collection(name: &CollectionName, path: &Path) -> Result<Collection, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return io("opening a collection log", error),
    };
    let reader = BufReader::new(file);
    let raw_lines: Vec<String> = match reader.lines().collect::<Result<Vec<_>, _>>() {
        Ok(lines) => lines,
        Err(error) => return io("reading a collection log", error),
    };

    let corrupt = |detail: String| StoreError::Corrupt {
        collection: name.as_str().to_string(),
        detail,
    };

    let mut iterator = raw_lines.iter().enumerate().peekable();
    let Some((_, first)) = iterator.next() else {
        return Err(corrupt("the log is empty and has no header".to_string()));
    };
    let header = match serde_json::from_str::<LogLine>(first) {
        Ok(LogLine::Header(header)) => header,
        Ok(_) => return Err(corrupt("the first line is not a header".to_string())),
        Err(error) => return Err(corrupt(format!("the header line is unreadable: {error}"))),
    };
    if header.format_version > FORMAT_VERSION {
        return Err(corrupt(format!(
            "it was written in on-disk format {} and this build understands up to {FORMAT_VERSION}",
            header.format_version
        )));
    }

    let mut chunks: BTreeMap<String, ChunkRecord> = BTreeMap::new();
    let mut dead_lines = 0_u64;
    let total = raw_lines.len();

    for (index, raw) in iterator {
        let is_last = index + 1 == total;
        let line = match serde_json::from_str::<LogLine>(raw) {
            Ok(line) => line,
            Err(error) => {
                if is_last {
                    // A crash mid-append. The record was never acknowledged,
                    // so dropping it is correct and silent recovery is safe.
                    dead_lines += 1;
                    break;
                }
                return Err(corrupt(format!(
                    "line {} is unreadable: {error}",
                    index + 1
                )));
            }
        };
        match line {
            LogLine::Header(_) => {
                return Err(corrupt(format!(
                    "line {} is a second header; two logs were concatenated",
                    index + 1
                )));
            }
            LogLine::Put { chunk } => {
                // Belt to the header's braces: this is what catches a
                // hand-edited log or two collections spliced together.
                if chunk.embedding_model != header.embedding_model {
                    return Err(corrupt(format!(
                        "line {} holds a vector from embedding model {:?} but the collection \
                         is pinned to {:?}; mixing embedding spaces would return confident \
                         nonsense",
                        index + 1,
                        chunk.embedding_model,
                        header.embedding_model
                    )));
                }
                if chunk.dimensions() != header.dimensions {
                    return Err(corrupt(format!(
                        "line {} holds a {}-dimension vector but the collection is pinned to {}",
                        index + 1,
                        chunk.dimensions(),
                        header.dimensions
                    )));
                }
                if chunks.insert(chunk.id.clone(), *chunk).is_some() {
                    dead_lines += 1;
                }
            }
            LogLine::Delete { id } => {
                dead_lines += 1;
                if chunks.remove(&id).is_some() {
                    dead_lines += 1;
                }
            }
            LogLine::DeleteDocument { document_id } => {
                dead_lines += 1;
                let doomed: Vec<String> = chunks
                    .values()
                    .filter(|chunk| chunk.document_id == document_id)
                    .map(|chunk| chunk.id.clone())
                    .collect();
                dead_lines += doomed.len() as u64;
                for id in doomed {
                    chunks.remove(&id);
                }
            }
        }
    }

    let approx_bytes = chunks.values().map(ChunkRecord::approx_bytes).sum();
    Ok(Collection {
        header,
        chunks,
        path: path.to_path_buf(),
        log_lines: total as u64,
        dead_lines,
        approx_bytes,
    })
}

/// Rewrite a log without its tombstones, if it has earned it.
///
/// Temporary file plus rename, so a crash during compaction leaves the
/// original log intact — the store comes back exactly as it was, just still
/// uncompacted.
fn maybe_compact(collection: &mut Collection) -> Result<bool, StoreError> {
    if collection.log_lines < COMPACT_MIN_LINES {
        return Ok(false);
    }
    let ratio = collection.dead_lines as f64 / collection.log_lines.max(1) as f64;
    if ratio < COMPACT_DEAD_RATIO {
        return Ok(false);
    }
    compact(collection)?;
    Ok(true)
}

fn compact(collection: &mut Collection) -> Result<(), StoreError> {
    let temporary = collection.path.with_extension("jsonl.compact");

    let mut lines: Vec<LogLine> = Vec::with_capacity(collection.chunks.len() + 1);
    lines.push(LogLine::Header(collection.header.clone()));
    for chunk in collection.chunks.values() {
        lines.push(LogLine::Put {
            chunk: Box::new(chunk.clone()),
        });
    }

    let mut buffer = String::new();
    for line in &lines {
        match serde_json::to_string(line) {
            Ok(encoded) => {
                buffer.push_str(&encoded);
                buffer.push('\n');
            }
            Err(error) => {
                return Err(StoreError::Io {
                    operation: "encoding a log line during compaction",
                    detail: error.to_string(),
                });
            }
        }
    }

    {
        let mut file = match File::create(&temporary) {
            Ok(file) => file,
            Err(error) => return io("creating the compaction temporary file", error),
        };
        if let Err(error) = file.write_all(buffer.as_bytes()) {
            let _ = std::fs::remove_file(&temporary);
            return io("writing the compaction temporary file", error);
        }
        if let Err(error) = file.sync_all() {
            let _ = std::fs::remove_file(&temporary);
            return io("flushing the compaction temporary file", error);
        }
    }

    if let Err(error) = std::fs::rename(&temporary, &collection.path) {
        let _ = std::fs::remove_file(&temporary);
        return io(
            "replacing the collection log with its compacted form",
            error,
        );
    }

    collection.log_lines = lines.len() as u64;
    collection.dead_lines = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::similarity::normalize_l2;
    use crate::testsupport::TempTree;

    fn limits() -> StoreLimits {
        StoreLimits {
            max_collections: 8,
            max_chunks_per_collection: 1_000,
            max_store_bytes: 64 * 1024 * 1024,
        }
    }

    fn name(raw: &str) -> CollectionName {
        CollectionName::parse(raw).expect("legal test name")
    }

    fn record(document: &str, index: u32, text: &str, vector: &[f32], model: &str) -> ChunkRecord {
        ChunkRecord {
            id: format!("{document}#{index}"),
            document_id: document.to_string(),
            source: Some(format!("docs/{document}.md")),
            chunk_index: index,
            line_start: index * 10 + 1,
            line_end: index * 10 + 9,
            heading_path: vec!["Guide".to_string()],
            text: text.to_string(),
            metadata: BTreeMap::new(),
            embedding: normalize_l2(vector.to_vec()).expect("has direction"),
            embedding_model: model.to_string(),
            created_unix_ms: 1_700_000_000_000,
        }
    }

    fn store(tree: &TempTree) -> VectorStore {
        VectorStore::open(&tree.canonical_root(), limits()).expect("opens")
    }

    // -- basic round trip -------------------------------------------------

    #[test]
    fn a_stored_chunk_comes_back_from_a_query() {
        let tree = TempTree::new("round-trip");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "install",
                vec![record("install", 0, "how to install", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let query = normalize_l2(vec![1.0, 0.05]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 5, 0.0, &QueryFilter::default())
            .expect("query");

        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0.99, "{}", results[0].score);
        assert_eq!(results[0].chunk.text, "how to install");
        assert_eq!(
            results[0].chunk.citation.as_deref(),
            Some("docs/install.md:1-9"),
            "a citation must be usable straight out of the response"
        );
    }

    #[test]
    fn results_are_ranked_and_limited() {
        let tree = TempTree::new("ranking");
        let store = store(&tree);
        for (index, vector) in [[1.0, 0.0], [0.9, 0.4], [0.0, 1.0]].into_iter().enumerate() {
            store
                .upsert_document(
                    &name("docs"),
                    &format!("doc{index}"),
                    vec![record(
                        &format!("doc{index}"),
                        0,
                        &format!("text {index}"),
                        &vector,
                        "m1",
                    )],
                    "m1",
                )
                .expect("upsert");
        }

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 2, 0.0, &QueryFilter::default())
            .expect("query");

        assert_eq!(results.len(), 2, "top_k is honoured");
        assert!(
            results[0].score >= results[1].score,
            "results must be ranked: {results:#?}"
        );
        assert_eq!(results[0].chunk.text, "text 0");
    }

    #[test]
    fn min_score_removes_weak_matches_rather_than_padding_the_list() {
        let tree = TempTree::new("min-score");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "unrelated", &[0.0, 1.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 10, 0.5, &QueryFilter::default())
            .expect("query");
        assert!(
            results.is_empty(),
            "an orthogonal chunk must not be returned just to fill top_k"
        );
    }

    #[test]
    fn equal_scores_rank_deterministically() {
        let tree = TempTree::new("ties");
        let store = store(&tree);
        for id in ["b", "a", "c"] {
            store
                .upsert_document(
                    &name("docs"),
                    id,
                    vec![record(id, 0, id, &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let first = store
            .query(&name("docs"), &query, 3, 0.0, &QueryFilter::default())
            .expect("query");
        let second = store
            .query(&name("docs"), &query, 3, 0.0, &QueryFilter::default())
            .expect("query");

        let ids: Vec<&str> = first.iter().map(|hit| hit.chunk.id.as_str()).collect();
        let repeat: Vec<&str> = second.iter().map(|hit| hit.chunk.id.as_str()).collect();
        assert_eq!(ids, repeat, "two runs of one query must agree");
        assert_eq!(ids, vec!["a#0", "b#0", "c#0"]);
    }

    // -- namespacing ------------------------------------------------------

    #[test]
    fn collections_are_namespaces_that_cannot_see_each_other() {
        let tree = TempTree::new("namespaces");
        let store = store(&tree);
        store
            .upsert_document(
                &name("alpha"),
                "a",
                vec![record("a", 0, "alpha text", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");
        store
            .upsert_document(
                &name("beta"),
                "b",
                vec![record("b", 0, "beta text", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let alpha = store
            .query(&name("alpha"), &query, 10, 0.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].chunk.text, "alpha text");
    }

    #[test]
    fn deleting_one_collection_leaves_the_others_untouched() {
        let tree = TempTree::new("drop-isolation");
        let store = store(&tree);
        for collection in ["alpha", "beta"] {
            store
                .upsert_document(
                    &name(collection),
                    "d",
                    vec![record("d", 0, collection, &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }

        let outcome = store.drop_collection(&name("alpha")).expect("drop");
        assert!(outcome.collection_removed);
        assert_eq!(outcome.chunks_deleted, 1);

        assert_eq!(
            store
                .collection_pins()
                .into_iter()
                .map(|(name, _, _)| name)
                .collect::<Vec<_>>(),
            vec!["beta".to_string()]
        );
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        assert_eq!(
            store
                .query(&name("beta"), &query, 10, 0.0, &QueryFilter::default())
                .expect("query")
                .len(),
            1,
            "dropping one namespace must not touch another"
        );
        assert!(
            !collections_dir(&tree.canonical_root())
                .join("alpha.jsonl")
                .exists(),
            "the log should be gone from disk too"
        );
    }

    #[test]
    fn querying_an_unknown_collection_errors_rather_than_returning_nothing() {
        let tree = TempTree::new("unknown");
        let store = store(&tree);
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let error = store
            .query(&name("missing"), &query, 5, 0.0, &QueryFilter::default())
            .expect_err("an empty list would look like 'no matches'");
        assert!(error.to_string().contains("no collection named"), "{error}");
    }

    // -- the embedding-model pin ------------------------------------------

    #[test]
    fn a_second_embedding_model_is_refused_with_both_names_and_the_fix() {
        let tree = TempTree::new("model-pin");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "text", &[1.0, 0.0], "nomic-embed-text")],
                "nomic-embed-text",
            )
            .expect("upsert");

        let error = store
            .upsert_document(
                &name("docs"),
                "b",
                vec![record(
                    "b",
                    0,
                    "text",
                    &[1.0, 0.0],
                    "text-embedding-3-small",
                )],
                "text-embedding-3-small",
            )
            .expect_err("mixing embedding spaces must be refused");

        let message = error.to_string();
        assert!(message.contains("nomic-embed-text"), "{message}");
        assert!(message.contains("text-embedding-3-small"), "{message}");
        assert!(message.contains("delete the collection"), "{message}");
    }

    #[test]
    fn the_model_check_runs_before_any_embedding_work() {
        let tree = TempTree::new("model-precheck");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "text", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        // This is what `query` and `upsert` call before spending anything on
        // the embeddings endpoint.
        assert!(store.check_model(&name("docs"), "m1", Some(2)).is_ok());
        assert!(store.check_model(&name("docs"), "m2", None).is_err());
        assert!(
            store
                .check_model(&name("brand-new"), "anything", None)
                .is_ok(),
            "a collection that does not exist yet pins nothing"
        );
    }

    #[test]
    fn the_same_model_serving_a_different_width_is_refused() {
        let tree = TempTree::new("dimension-pin");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "text", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let error = store
            .check_model(&name("docs"), "m1", Some(768))
            .expect_err("same id, different width, is still a different model");
        assert!(error.to_string().contains("768"), "{error}");
    }

    #[test]
    fn chunks_of_differing_widths_in_one_call_are_refused() {
        let tree = TempTree::new("ragged-batch");
        let store = store(&tree);
        let error = store
            .upsert_document(
                &name("docs"),
                "a",
                vec![
                    record("a", 0, "one", &[1.0, 0.0], "m1"),
                    record("a", 1, "two", &[1.0, 0.0, 0.0], "m1"),
                ],
                "m1",
            )
            .expect_err("a ragged batch means the endpoint changed under us");
        assert!(matches!(error, StoreError::DimensionMismatch { .. }));
    }

    // -- upsert semantics -------------------------------------------------

    #[test]
    fn re_upserting_a_document_replaces_its_chunks_rather_than_adding_to_them() {
        let tree = TempTree::new("replace");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![
                    record("a", 0, "old one", &[1.0, 0.0], "m1"),
                    record("a", 1, "old two", &[0.9, 0.1], "m1"),
                    record("a", 2, "old three", &[0.8, 0.2], "m1"),
                ],
                "m1",
            )
            .expect("upsert");

        let outcome = store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "new one", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        assert_eq!(outcome.chunks_written, 1);
        assert_eq!(outcome.chunks_replaced, 3);

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(
            results.len(),
            1,
            "a shortened document must leave no stale chunks: {results:#?}"
        );
        assert_eq!(results[0].chunk.text, "new one");
    }

    #[test]
    fn deleting_a_document_leaves_its_neighbours_alone() {
        let tree = TempTree::new("delete-document");
        let store = store(&tree);
        for id in ["a", "b"] {
            store
                .upsert_document(
                    &name("docs"),
                    id,
                    vec![record(id, 0, id, &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }

        let outcome = store
            .delete_documents(&name("docs"), &["a".to_string()])
            .expect("delete");
        assert_eq!(outcome.documents_deleted, 1);
        assert_eq!(outcome.chunks_deleted, 1);

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let remaining = store
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chunk.document_id, "b");
    }

    #[test]
    fn deleting_a_document_that_is_not_there_is_a_zero_not_an_error() {
        let tree = TempTree::new("delete-absent");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "a", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let outcome = store
            .delete_documents(&name("docs"), &["nope".to_string()])
            .expect("delete");
        assert_eq!(outcome.chunks_deleted, 0);
        assert_eq!(outcome.documents_deleted, 0);
    }

    // -- filtering --------------------------------------------------------

    #[test]
    fn metadata_filters_are_an_and_not_an_or() {
        let tree = TempTree::new("filter-and");
        let store = store(&tree);
        let mut both = record("a", 0, "both", &[1.0, 0.0], "m1");
        both.metadata
            .insert("team".to_string(), "platform".to_string());
        both.metadata
            .insert("kind".to_string(), "runbook".to_string());
        let mut one = record("b", 0, "one", &[1.0, 0.0], "m1");
        one.metadata
            .insert("team".to_string(), "platform".to_string());

        store
            .upsert_document(&name("docs"), "a", vec![both], "m1")
            .expect("upsert");
        store
            .upsert_document(&name("docs"), "b", vec![one], "m1")
            .expect("upsert");

        let filter = QueryFilter {
            metadata: BTreeMap::from([
                ("team".to_string(), "platform".to_string()),
                ("kind".to_string(), "runbook".to_string()),
            ]),
            ..QueryFilter::default()
        };
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 10, -1.0, &filter)
            .expect("query");

        assert_eq!(results.len(), 1, "{results:#?}");
        assert_eq!(results[0].chunk.text, "both");
    }

    #[test]
    fn a_source_prefix_filter_narrows_to_a_subtree() {
        let tree = TempTree::new("filter-prefix");
        let store = store(&tree);
        let mut inside = record("a", 0, "inside", &[1.0, 0.0], "m1");
        inside.source = Some("docs/ops/runbook.md".to_string());
        let mut outside = record("b", 0, "outside", &[1.0, 0.0], "m1");
        outside.source = Some("docs/dev/design.md".to_string());

        store
            .upsert_document(&name("docs"), "a", vec![inside], "m1")
            .expect("upsert");
        store
            .upsert_document(&name("docs"), "b", vec![outside], "m1")
            .expect("upsert");

        let filter = QueryFilter {
            source_prefix: Some("docs/ops/".to_string()),
            ..QueryFilter::default()
        };
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = store
            .query(&name("docs"), &query, 10, -1.0, &filter)
            .expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.text, "inside");
    }

    #[test]
    fn a_chunk_missing_the_filtered_key_is_excluded_rather_than_assumed() {
        let tree = TempTree::new("filter-missing");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, "no metadata", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let filter = QueryFilter {
            metadata: BTreeMap::from([("team".to_string(), "platform".to_string())]),
            ..QueryFilter::default()
        };
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        assert!(
            store
                .query(&name("docs"), &query, 10, -1.0, &filter)
                .expect("query")
                .is_empty()
        );
    }

    // -- durability -------------------------------------------------------

    #[test]
    fn a_store_survives_a_restart() {
        let tree = TempTree::new("restart");
        let root = tree.canonical_root();
        {
            let store = VectorStore::open(&root, limits()).expect("opens");
            store
                .upsert_document(
                    &name("docs"),
                    "a",
                    vec![
                        record("a", 0, "first passage", &[1.0, 0.0], "m1"),
                        record("a", 1, "second passage", &[0.0, 1.0], "m1"),
                    ],
                    "m1",
                )
                .expect("upsert");
        }

        let reopened = VectorStore::open(&root, limits()).expect("reopens");
        assert_eq!(
            reopened.collection_pins(),
            vec![("docs".to_string(), "m1".to_string(), 2)],
            "the embedding-model pin has to survive a restart, or the next process could \
             quietly write a second embedding space into this collection"
        );

        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = reopened
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(results.len(), 2, "both chunks came back from disk");
        assert_eq!(results[0].chunk.text, "first passage");
    }

    #[test]
    fn a_delete_survives_a_restart() {
        let tree = TempTree::new("restart-delete");
        let root = tree.canonical_root();
        {
            let store = VectorStore::open(&root, limits()).expect("opens");
            for id in ["a", "b"] {
                store
                    .upsert_document(
                        &name("docs"),
                        id,
                        vec![record(id, 0, id, &[1.0, 0.0], "m1")],
                        "m1",
                    )
                    .expect("upsert");
            }
            store
                .delete_documents(&name("docs"), &["a".to_string()])
                .expect("delete");
        }

        let reopened = VectorStore::open(&root, limits()).expect("reopens");
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = reopened
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(results.len(), 1, "a tombstone must replay as a deletion");
        assert_eq!(results[0].chunk.document_id, "b");
    }

    #[test]
    fn a_torn_final_line_is_discarded_and_everything_before_it_survives() {
        let tree = TempTree::new("torn-tail");
        let root = tree.canonical_root();
        {
            let store = VectorStore::open(&root, limits()).expect("opens");
            store
                .upsert_document(
                    &name("docs"),
                    "a",
                    vec![record("a", 0, "committed", &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }

        // Simulate a crash part-way through an append.
        let path = collections_dir(&root).join("docs.jsonl");
        let mut existing = std::fs::read_to_string(&path).expect("read log");
        existing.push_str("{\"op\":\"put\",\"chunk\":{\"id\":\"a#1\",\"docu");
        std::fs::write(&path, existing).expect("write torn log");

        let reopened = VectorStore::open(&root, limits()).expect("a torn tail is recoverable");
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = reopened
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.text, "committed");
    }

    #[test]
    fn corruption_in_the_middle_is_reported_rather_than_skipped() {
        let tree = TempTree::new("mid-corruption");
        let root = tree.canonical_root();
        {
            let store = VectorStore::open(&root, limits()).expect("opens");
            store
                .upsert_document(
                    &name("docs"),
                    "a",
                    vec![
                        record("a", 0, "one", &[1.0, 0.0], "m1"),
                        record("a", 1, "two", &[0.0, 1.0], "m1"),
                    ],
                    "m1",
                )
                .expect("upsert");
        }

        let path = collections_dir(&root).join("docs.jsonl");
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(str::to_string)
            .collect();
        lines[1] = "{ this is not json".to_string();
        std::fs::write(&path, lines.join("\n") + "\n").expect("write log");

        let error = VectorStore::open(&root, limits())
            .expect_err("silently dropping half a collection would be worse");
        let message = error.to_string();
        assert!(message.contains("could not be loaded"), "{message}");
        assert!(message.contains("was not modified"), "{message}");
    }

    #[test]
    fn a_log_holding_two_embedding_models_refuses_to_load() {
        let tree = TempTree::new("spliced-log");
        let root = tree.canonical_root();
        {
            let store = VectorStore::open(&root, limits()).expect("opens");
            store
                .upsert_document(
                    &name("docs"),
                    "a",
                    vec![record("a", 0, "one", &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }

        // A hand-edited or concatenated log: the header says one model, a
        // record says another.
        let path = collections_dir(&root).join("docs.jsonl");
        let foreign = record("b", 0, "two", &[1.0, 0.0], "some-other-model");
        let line = serde_json::to_string(&LogLine::Put {
            chunk: Box::new(foreign),
        })
        .expect("encodes");
        let mut existing = std::fs::read_to_string(&path).expect("read log");
        existing.push_str(&line);
        existing.push('\n');
        std::fs::write(&path, existing).expect("write log");

        let error = VectorStore::open(&root, limits()).expect_err("mixed spaces must not load");
        assert!(
            error.to_string().contains("mixing embedding spaces"),
            "{error}"
        );
    }

    #[test]
    fn a_future_on_disk_format_is_refused_rather_than_misread() {
        let tree = TempTree::new("future-format");
        let root = tree.canonical_root();
        std::fs::create_dir_all(collections_dir(&root)).expect("create dir");
        let header = LogLine::Header(CollectionHeader {
            format_version: FORMAT_VERSION + 1,
            collection: "docs".to_string(),
            embedding_model: "m1".to_string(),
            dimensions: 2,
            created_unix_ms: 0,
        });
        std::fs::write(
            collections_dir(&root).join("docs.jsonl"),
            serde_json::to_string(&header).expect("encodes") + "\n",
        )
        .expect("write log");

        let error = VectorStore::open(&root, limits()).expect_err("a newer format must be refused");
        assert!(error.to_string().contains("on-disk format"), "{error}");
    }

    #[test]
    fn a_foreign_file_in_the_collections_directory_is_ignored() {
        let tree = TempTree::new("foreign-file");
        let root = tree.canonical_root();
        std::fs::create_dir_all(collections_dir(&root)).expect("create dir");
        std::fs::write(collections_dir(&root).join("README.md"), "not a collection")
            .expect("write");
        std::fs::write(collections_dir(&root).join("notes.json"), "{}").expect("write");

        let store = VectorStore::open(&root, limits()).expect("opens");
        assert!(store.collection_pins().is_empty());
    }

    // -- compaction -------------------------------------------------------

    #[test]
    fn a_log_full_of_tombstones_is_compacted_and_still_replays() {
        let tree = TempTree::new("compaction");
        let root = tree.canonical_root();
        let store = VectorStore::open(&root, limits()).expect("opens");

        let path = collections_dir(&root).join("docs.jsonl");
        let line_count = || {
            std::fs::read_to_string(&path)
                .expect("read log")
                .lines()
                .count()
        };

        // Rewrite one document over and over. Each round appends a tombstone
        // and a fresh record, so the log is almost entirely dead weight.
        let mut compacted_at = None;
        for round in 0..400 {
            let outcome = store
                .upsert_document(
                    &name("docs"),
                    "a",
                    vec![record("a", 0, &format!("round {round}"), &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");

            if outcome.compacted && compacted_at.is_none() {
                compacted_at = Some(round);
                // Checked at the exact moment it happened: a compacted log is
                // the header plus one line per live chunk, and nothing else.
                assert_eq!(
                    line_count(),
                    2,
                    "a compacted log should be a header plus the single live chunk"
                );
            }
        }

        let compacted_at = compacted_at.expect("400 rewrites must trigger a compaction");
        assert!(
            compacted_at < 400,
            "compaction happened at round {compacted_at}"
        );
        assert!(
            line_count() < 400,
            "compaction must bound the log below the number of writes, got {}",
            line_count()
        );
        assert!(
            !path.with_extension("jsonl.compact").exists(),
            "the temporary file must not survive"
        );

        let reopened = VectorStore::open(&root, limits()).expect("reopens");
        let query = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let results = reopened
            .query(&name("docs"), &query, 10, -1.0, &QueryFilter::default())
            .expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.text, "round 399");
    }

    // -- bounds -----------------------------------------------------------

    #[test]
    fn the_collection_limit_is_enforced() {
        let tree = TempTree::new("collection-limit");
        let store = VectorStore::open(
            &tree.canonical_root(),
            StoreLimits {
                max_collections: 2,
                ..limits()
            },
        )
        .expect("opens");

        for collection in ["one", "two"] {
            store
                .upsert_document(
                    &name(collection),
                    "d",
                    vec![record("d", 0, "text", &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }
        let error = store
            .upsert_document(
                &name("three"),
                "d",
                vec![record("d", 0, "text", &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect_err("the third collection must be refused");
        assert!(error.to_string().contains("--max-collections"), "{error}");
    }

    #[test]
    fn the_chunk_limit_is_enforced_and_names_the_reason() {
        let tree = TempTree::new("chunk-limit");
        let store = VectorStore::open(
            &tree.canonical_root(),
            StoreLimits {
                max_chunks_per_collection: 2,
                ..limits()
            },
        )
        .expect("opens");

        let error = store
            .upsert_document(
                &name("docs"),
                "a",
                (0..3)
                    .map(|index| record("a", index, "text", &[1.0, 0.0], "m1"))
                    .collect(),
                "m1",
            )
            .expect_err("three chunks into a two-chunk collection");
        let message = error.to_string();
        assert!(message.contains("brute-force"), "{message}");
        assert!(message.contains("--max-chunks-per-collection"), "{message}");
    }

    #[test]
    fn replacing_a_document_does_not_trip_the_chunk_limit() {
        let tree = TempTree::new("limit-replace");
        let store = VectorStore::open(
            &tree.canonical_root(),
            StoreLimits {
                max_chunks_per_collection: 2,
                ..limits()
            },
        )
        .expect("opens");

        store
            .upsert_document(
                &name("docs"),
                "a",
                (0..2)
                    .map(|index| record("a", index, "text", &[1.0, 0.0], "m1"))
                    .collect(),
                "m1",
            )
            .expect("upsert fills the collection");

        store
            .upsert_document(
                &name("docs"),
                "a",
                (0..2)
                    .map(|index| record("a", index, "fresh", &[1.0, 0.0], "m1"))
                    .collect(),
                "m1",
            )
            .expect("replacing the same document must count the chunks it frees");
    }

    #[test]
    fn the_byte_limit_is_enforced() {
        let tree = TempTree::new("byte-limit");
        let store = VectorStore::open(
            &tree.canonical_root(),
            StoreLimits {
                max_store_bytes: 300,
                ..limits()
            },
        )
        .expect("opens");

        let error = store
            .upsert_document(
                &name("docs"),
                "a",
                vec![record("a", 0, &"x".repeat(2_000), &[1.0, 0.0], "m1")],
                "m1",
            )
            .expect_err("an oversized document must be refused");
        assert!(error.to_string().contains("--max-store-bytes"), "{error}");
    }

    // -- statistics -------------------------------------------------------

    #[test]
    fn stats_describe_what_is_actually_held() {
        let tree = TempTree::new("stats");
        let store = store(&tree);
        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![
                    record("a", 0, "one", &[1.0, 0.0], "m1"),
                    record("a", 1, "two", &[0.0, 1.0], "m1"),
                ],
                "m1",
            )
            .expect("upsert");
        store
            .upsert_document(
                &name("docs"),
                "b",
                vec![record("b", 0, "three", &[1.0, 1.0], "m1")],
                "m1",
            )
            .expect("upsert");

        let stats = store.stats(None).expect("stats");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].collection, "docs");
        assert_eq!(stats[0].chunks, 3);
        assert_eq!(stats[0].documents, 2);
        assert_eq!(stats[0].embedding_model, "m1");
        assert_eq!(stats[0].dimensions, 2);
        assert!(stats[0].approx_memory_bytes > 0);
        assert!(stats[0].log_bytes > 0, "the log exists on disk");
        assert_eq!(
            stats[0].sources,
            vec!["docs/a.md".to_string(), "docs/b.md".to_string()]
        );
        assert!(!stats[0].sources_truncated);
        assert!(!stats[0].near_capacity);
    }

    #[test]
    fn stats_for_a_named_collection_can_be_asked_for_alone() {
        let tree = TempTree::new("stats-one");
        let store = store(&tree);
        for collection in ["alpha", "beta"] {
            store
                .upsert_document(
                    &name(collection),
                    "d",
                    vec![record("d", 0, "text", &[1.0, 0.0], "m1")],
                    "m1",
                )
                .expect("upsert");
        }
        let stats = store.stats(Some(&name("beta"))).expect("stats");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].collection, "beta");

        assert!(store.stats(Some(&name("gamma"))).is_err());
    }

    #[test]
    fn the_health_counters_track_the_store_without_taking_the_lock() {
        let tree = TempTree::new("counters");
        let store = store(&tree);
        assert_eq!(store.counts(), (0, 0));

        store
            .upsert_document(
                &name("docs"),
                "a",
                vec![
                    record("a", 0, "one", &[1.0, 0.0], "m1"),
                    record("a", 1, "two", &[0.0, 1.0], "m1"),
                ],
                "m1",
            )
            .expect("upsert");
        assert_eq!(store.counts(), (1, 2));

        store.drop_collection(&name("docs")).expect("drop");
        assert_eq!(store.counts(), (0, 0));
    }
}
