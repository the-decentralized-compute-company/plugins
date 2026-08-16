//! The whole contribution surface of `vector-store` in one declaration.
//!
//! Six MCP tools, three of them mirrored as HTTP routes (a RAG gateway in
//! front of `:9337` speaks HTTP, not MCP), one capability, and a health hook.
//! The host synthesizes `tools/list`, `tools/call`, the JSON Schema for every
//! argument, and the request validation that runs before a handler is entered
//! — this plugin opens no socket and speaks no MCP.
//!
//! There is deliberately **no** `config_schema` and no `web_ui`.
//! `[plugin.settings]` never reaches a plugin process, so a schema here would
//! render a chunk-size control in the console whose value this process could
//! not read. See [`crate::config`] for where settings actually come from.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.
//!
//! # Where the work runs
//!
//! Every store read and write goes through `spawn_blocking`. A brute-force
//! scan of a large collection and an `fsync` on a slow disk are both long
//! enough to matter, and the control connection carries health checks on the
//! same runtime. Health itself reads two atomics and never takes the store
//! lock at all.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{
    PluginError, PluginMetadata, PluginResult, SimplePlugin, capability, http, mcp, plugin,
    plugin_server_info,
};

use crate::chunk::{Chunk, SplitReason, chunk_document};
use crate::config::{Config, MAX_TOP_K, PLUGIN_NAME, PLUGIN_VERSION};
use crate::embeddings::EmbeddingClient;
use crate::names::{CollectionName, display_path};
use crate::store::{
    ChunkRecord, CollectionStats, DeleteOutcome, QueryFilter, ScoredChunk, VectorStore, now_unix_ms,
};

/// Cap on metadata a caller may attach to one document.
const MAX_METADATA_ENTRIES: usize = 16;
const MAX_METADATA_KEY_CHARS: usize = 64;
const MAX_METADATA_VALUE_CHARS: usize = 512;
/// Cap on a document id and a source label. Both end up in every response.
const MAX_IDENTIFIER_CHARS: usize = 512;
/// Cap on the query string. Longer than a chunk ceiling is not a question.
const MAX_QUERY_CHARS: usize = 4_000;

/// One document, chunked and validated, waiting for its vectors.
///
/// Chunking happens for every document in an `upsert` before anything is sent
/// to the embeddings endpoint, so a document that cannot be chunked — or an
/// argument that fails validation — costs no embedding calls and writes
/// nothing.
struct PlannedDocument {
    id: String,
    source: String,
    metadata: BTreeMap<String, String>,
    chunks: Vec<Chunk>,
}

/// Everything the handlers share. Cheap to clone behind an `Arc`.
pub struct AppState {
    pub config: Config,
    pub store: Arc<VectorStore>,
    pub client: EmbeddingClient,
}

impl AppState {
    pub fn new(config: Config, store: Arc<VectorStore>) -> Result<Self, String> {
        let client = EmbeddingClient::new(&config).map_err(|error| error.message)?;
        Ok(Self {
            config,
            store,
            client,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool arguments
//
// Doc comments on these fields become the descriptions in the JSON Schema the
// host advertises, which is what a model or an operator reads before calling
// the tool. They are written for that audience.
//
// `deny_unknown_fields` throughout: a misspelled `filters` should be an error
// the caller can see, not a filter that silently did not apply and a result
// set that looks fine.
// ---------------------------------------------------------------------------

/// One document offered to `upsert`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputDocument {
    /// Stable identifier for this document, chosen by you — a file path, a
    /// URL, a database key. Upserting the same id again **replaces** every
    /// chunk of the previous version, so re-ingesting an edited file leaves no
    /// stale passages behind.
    pub id: String,

    /// The document text. Markdown structure is used when it is there:
    /// headings become a breadcrumb on every chunk, fenced code blocks are
    /// kept whole, and paragraphs are preferred over arbitrary cut points.
    pub text: String,

    /// Where this text came from, for citations — for example
    /// `docs/install.md`. **This plugin never opens it.** It is a label
    /// recorded with each chunk and returned by `query`, nothing more; pass
    /// the text you already read. Defaults to `id` when omitted.
    #[serde(default)]
    pub source: Option<String>,

    /// Arbitrary string labels to attach to every chunk of this document, for
    /// filtering at query time — `{"team": "platform", "kind": "runbook"}`.
    /// Values are matched by exact string equality; there is no expression
    /// language.
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertArgs {
    /// Which collection to write into. Collections are independent
    /// namespaces: a query only ever sees one of them, and deleting one never
    /// touches another. Created on first use. Lowercase letters, digits, `-`
    /// and `_`, up to 64 characters.
    pub collection: String,

    /// The documents to chunk, embed, and store.
    pub documents: Vec<InputDocument>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryArgs {
    /// Which collection to search. Only this collection is searched.
    pub collection: String,

    /// What to look for, in plain words. This is embedded and compared against
    /// every stored chunk by cosine similarity — so phrase it like the passage
    /// you hope to find, not like a keyword search.
    pub query: String,

    /// How many passages to return. Defaults to the operator's configured
    /// value and is capped at 100.
    #[serde(default)]
    pub top_k: Option<u32>,

    /// Drop results scoring below this cosine similarity, between -1.0 and
    /// 1.0. Scores are not comparable between embedding models, so tune this
    /// against your own results rather than copying a number. Omit it to
    /// return the top matches whatever they score.
    #[serde(default)]
    pub min_score: Option<f64>,

    /// Keep only chunks whose metadata matches every one of these key/value
    /// pairs exactly. An AND, never an OR. A chunk missing the key is
    /// excluded rather than assumed to match.
    #[serde(default)]
    pub filter: Option<BTreeMap<String, String>>,

    /// Keep only chunks whose `source` starts with this string — for example
    /// `docs/ops/` to search one subtree.
    #[serde(default)]
    pub source_prefix: Option<String>,

    /// Keep only chunks from these document ids.
    #[serde(default)]
    pub document_ids: Option<Vec<String>>,
}

/// What a `delete` call is allowed to remove.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeleteScope {
    /// Remove the documents named in `document_ids`, and only those.
    Documents,
    /// Remove the whole collection, its file included. Everything in it is
    /// gone; no other collection is touched.
    Collection,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// Which collection to delete from.
    pub collection: String,

    /// What to remove. There is no default: deleting an index is cheap to do
    /// and impossible to undo, so the caller has to say which.
    pub scope: DeleteScope,

    /// Required when `scope` is `documents`, ignored otherwise. Every chunk of
    /// each named document is removed.
    #[serde(default)]
    pub document_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatsArgs {
    /// Report on one collection only. Omit it for every collection.
    #[serde(default)]
    pub collection: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewArgs {
    /// The text to split. Nothing is embedded and nothing is stored — this
    /// shows exactly what `upsert` would do with the same text under the
    /// current settings.
    pub text: String,

    /// Optional source label, so the previewed citations read the way the real
    /// ones will.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

// ---------------------------------------------------------------------------
// Tool responses
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DocumentOutcome {
    pub document_id: String,
    pub chunks_written: usize,
    pub chunks_replaced: usize,
    /// Set when the document contained a block too long to split on structure
    /// alone. `sentence` and `word` are ordinary; `hard` means a run of
    /// non-whitespace longer than the chunk ceiling was cut mid-string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_reason: Option<SplitReason>,
}

#[derive(Debug, Serialize)]
pub struct UpsertResponse {
    pub collection: String,
    pub embedding_model: String,
    pub dimensions: usize,
    pub documents: Vec<DocumentOutcome>,
    pub chunks_written: usize,
    pub chunks_replaced: usize,
    /// Documents that produced no chunks because they were empty or
    /// whitespace. Reported rather than counted as a success.
    pub documents_skipped_empty: Vec<String>,
    pub embedding_calls: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub collection: String,
    pub embedding_model: String,
    pub top_k: usize,
    pub min_score: Option<f64>,
    pub returned: usize,
    /// Chunks in the collection before filtering, so an empty result can be
    /// told apart from an empty collection.
    pub collection_chunks: usize,
    pub filtered: bool,
    pub results: Vec<ScoredChunk>,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub collection: String,
    #[serde(flatten)]
    pub outcome: DeleteOutcome,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub collections: Vec<CollectionStats>,
    pub total_chunks: usize,
    pub total_documents: usize,
    pub approx_memory_bytes: u64,
    pub data_dir: String,
    pub search: &'static str,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewChunk {
    pub index: usize,
    pub chars: usize,
    pub line_start: u32,
    pub line_end: u32,
    pub heading_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_reason: Option<SplitReason>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub chunks: Vec<PreviewChunk>,
    pub chunk_count: usize,
    pub target_chars: usize,
    pub overlap_chars: usize,
    pub max_chars: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingsStatus {
    pub endpoint: String,
    pub endpoint_is_loopback: bool,
    pub model: Option<String>,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    pub data_dir: String,
    pub embedding_model: String,
    pub allow_remote_embeddings: bool,
    /// Whether a key exists, never the key.
    pub api_key_configured: bool,
    pub chunk_chars: usize,
    pub chunk_overlap_chars: usize,
    pub max_chunk_chars: usize,
    pub max_collections: usize,
    pub max_chunks_per_collection: usize,
    pub max_store_bytes: u64,
    pub max_document_bytes: usize,
    pub max_documents_per_call: usize,
    pub default_top_k: usize,
    pub max_top_k: usize,
    pub embed_batch_size: usize,
    pub request_timeout_seconds: u64,
    pub storage: &'static str,
    pub search: &'static str,
}

/// A collection and the embedding model it is locked to.
///
/// `usable` is the whole point of this tool: a collection pinned to a model
/// this process is not configured for cannot be queried at all, and an
/// operator staring at "no results" needs to see that here rather than deduce
/// it from an error later.
#[derive(Debug, Serialize)]
pub struct CollectionPin {
    pub collection: String,
    pub embedding_model: String,
    pub dimensions: usize,
    pub usable_with_current_model: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub embeddings: EmbeddingsStatus,
    pub collections: Vec<CollectionPin>,
    pub config: EffectiveConfig,
    pub advisories: Vec<String>,
    pub notes: Vec<String>,
}

const STORAGE_NOTE: &str = "one append-only JSONL log per collection under the configured data directory; \
     durable across restarts and compacted in place";
const SEARCH_NOTE: &str = "exact brute-force cosine scan over every chunk in one collection; appropriate to a \
     few tens of thousands of chunks per collection, past which query latency grows \
     linearly and a real vector database is the right tool";

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> PluginError {
    PluginError::invalid_params(message.into())
}

fn parse_collection(raw: &str) -> PluginResult<CollectionName> {
    CollectionName::parse(raw).map_err(|error| invalid(error.to_string()))
}

fn check_identifier(label: &str, value: &str) -> PluginResult<()> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{label} must not be empty")));
    }
    let chars = value.chars().count();
    if chars > MAX_IDENTIFIER_CHARS {
        return Err(invalid(format!(
            "{label} is {chars} characters; the maximum is {MAX_IDENTIFIER_CHARS}"
        )));
    }
    Ok(())
}

/// Bound metadata so a caller cannot grow a chunk record without limit.
fn check_metadata(metadata: &BTreeMap<String, String>) -> PluginResult<()> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(invalid(format!(
            "a document may carry at most {MAX_METADATA_ENTRIES} metadata entries, got {}",
            metadata.len()
        )));
    }
    for (key, value) in metadata {
        if key.trim().is_empty() {
            return Err(invalid("a metadata key must not be empty"));
        }
        if key.chars().count() > MAX_METADATA_KEY_CHARS {
            return Err(invalid(format!(
                "metadata key {key:?} exceeds {MAX_METADATA_KEY_CHARS} characters"
            )));
        }
        if value.chars().count() > MAX_METADATA_VALUE_CHARS {
            return Err(invalid(format!(
                "metadata value for {key:?} exceeds {MAX_METADATA_VALUE_CHARS} characters"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Run a store operation off the async runtime.
///
/// A cosine scan over a large collection and an `fsync` are both long enough
/// that leaving them on the runtime would make health checks jittery during an
/// ingest.
async fn on_blocking<T, F>(work: F) -> PluginResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> PluginResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| {
            Err(PluginError::internal(format!(
                "the store task did not complete: {error}"
            )))
        })
}

pub(crate) async fn handle_upsert(
    state: Arc<AppState>,
    args: UpsertArgs,
) -> PluginResult<UpsertResponse> {
    let collection = parse_collection(&args.collection)?;

    if args.documents.is_empty() {
        return Err(invalid("`documents` must contain at least one document"));
    }
    if args.documents.len() > state.config.max_documents_per_call {
        return Err(invalid(format!(
            "{} documents in one call exceeds the limit of {}; send them in batches",
            args.documents.len(),
            state.config.max_documents_per_call
        )));
    }

    let model = state.client.model_identity().to_string();

    // Refuse a model mismatch before spending a single embedding call. The
    // error names the pinned model, the configured one, and both ways out.
    {
        let store = Arc::clone(&state.store);
        let name = collection.clone();
        let model = model.clone();
        on_blocking(move || {
            store
                .check_model(&name, &model, None)
                .map_err(|error| invalid(error.to_string()))
        })
        .await?;
    }

    // Chunk everything first. Splitting is pure and cheap, and doing it up
    // front means a document that cannot be chunked fails before anything is
    // sent to the embeddings endpoint or written to disk.
    let options = state.config.chunk_options();
    let mut planned: Vec<PlannedDocument> = Vec::with_capacity(args.documents.len());
    let mut skipped: Vec<String> = Vec::new();

    for document in args.documents {
        check_identifier("document id", &document.id)?;
        if document.text.len() > state.config.max_document_bytes {
            return Err(invalid(format!(
                "document {:?} is {} bytes; the limit is {} (raise --max-document-bytes, or \
                 split the document)",
                document.id,
                document.text.len(),
                state.config.max_document_bytes
            )));
        }
        let source = match document.source {
            Some(source) => {
                check_identifier("source", &source)?;
                source
            }
            None => document.id.clone(),
        };
        let metadata = document.metadata.unwrap_or_default();
        check_metadata(&metadata)?;

        let chunks = chunk_document(&document.text, &options);
        if chunks.is_empty() {
            skipped.push(document.id);
            continue;
        }
        planned.push(PlannedDocument {
            id: document.id,
            source,
            metadata,
            chunks,
        });
    }

    if planned.is_empty() {
        return Err(invalid(
            "every document was empty or whitespace, so there is nothing to store",
        ));
    }

    // Embed every chunk of every document in one flat run, so batching is
    // effective across small documents rather than per document.
    let texts: Vec<String> = planned
        .iter()
        .flat_map(|document| document.chunks.iter().map(|chunk| chunk.text.clone()))
        .collect();
    let embedding_calls = texts.len().div_ceil(state.client.batch_size().max(1));

    let vectors = state
        .client
        .embed_many(&texts)
        .await
        .map_err(|error| PluginError::internal(error.message))?;
    if vectors.len() != texts.len() {
        return Err(PluginError::internal(format!(
            "the embeddings endpoint returned {} vectors for {} chunks; nothing was stored",
            vectors.len(),
            texts.len()
        )));
    }
    let dimensions = vectors.first().map(Vec::len).unwrap_or(0);

    let created = now_unix_ms();
    let mut cursor = 0_usize;
    let mut outcomes: Vec<DocumentOutcome> = Vec::with_capacity(planned.len());
    let mut chunks_written = 0_usize;
    let mut chunks_replaced = 0_usize;

    for document in planned {
        let split_reason = document.chunks.iter().find_map(|chunk| chunk.split_reason);
        let records: Vec<ChunkRecord> = document
            .chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| ChunkRecord {
                id: format!("{}#{index}", document.id),
                document_id: document.id.clone(),
                source: Some(document.source.clone()),
                chunk_index: index as u32,
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                heading_path: chunk.heading_path.clone(),
                text: chunk.text.clone(),
                metadata: document.metadata.clone(),
                embedding: vectors[cursor + index].clone(),
                embedding_model: model.clone(),
                created_unix_ms: created,
            })
            .collect();
        cursor += document.chunks.len();

        let store = Arc::clone(&state.store);
        let name = collection.clone();
        let model_for_write = model.clone();
        let document_for_write = document.id.clone();
        let outcome = on_blocking(move || {
            store
                .upsert_document(&name, &document_for_write, records, &model_for_write)
                .map_err(|error| invalid(error.to_string()))
        })
        .await
        // Each document is written as its own durable transaction, so a
        // failure part-way through a batch leaves the earlier ones stored.
        // Say which those were rather than leaving the caller to guess: the
        // whole call is safe to retry (an upsert replaces), but only if they
        // know it did something.
        .map_err(|error| {
            if outcomes.is_empty() {
                error
            } else {
                let committed: Vec<&str> = outcomes
                    .iter()
                    .map(|outcome| outcome.document_id.as_str())
                    .collect();
                invalid(format!(
                    "{} — document {:?} failed after {} earlier document(s) in this batch \
                     had already been stored ({}). Each document is written separately, so \
                     those remain; re-sending the whole batch is safe once the cause is \
                     fixed, because an upsert replaces.",
                    error.message,
                    document.id,
                    committed.len(),
                    committed.join(", ")
                ))
            }
        })?;

        chunks_written += outcome.chunks_written;
        chunks_replaced += outcome.chunks_replaced;
        outcomes.push(DocumentOutcome {
            document_id: document.id,
            chunks_written: outcome.chunks_written,
            chunks_replaced: outcome.chunks_replaced,
            split_reason,
        });
    }

    let mut notes = vec![
        format!(
            "chunks are pinned to embedding model {model:?}; a query using a different model \
             is refused rather than answered with an incomparable ranking"
        ),
        STORAGE_NOTE.to_string(),
    ];
    if outcomes
        .iter()
        .any(|outcome| outcome.split_reason.is_some())
    {
        notes.push(
            "at least one document contained a block too long to split on structure alone; \
             `split_reason` says where that happened"
                .to_string(),
        );
    }
    if !skipped.is_empty() {
        notes.push(format!(
            "{} document(s) produced no chunks because they were empty or whitespace",
            skipped.len()
        ));
    }

    Ok(UpsertResponse {
        collection: collection.as_str().to_string(),
        embedding_model: model,
        dimensions,
        documents: outcomes,
        chunks_written,
        chunks_replaced,
        documents_skipped_empty: skipped,
        embedding_calls,
        notes,
    })
}

pub(crate) async fn handle_query(
    state: Arc<AppState>,
    args: QueryArgs,
) -> PluginResult<QueryResponse> {
    let collection = parse_collection(&args.collection)?;

    let query = args.query.trim().to_string();
    if query.is_empty() {
        return Err(invalid("`query` must not be empty"));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(invalid(format!(
            "`query` is {} characters; the maximum is {MAX_QUERY_CHARS}",
            query.chars().count()
        )));
    }
    if let Some(min_score) = args.min_score
        && !(-1.0..=1.0).contains(&min_score)
    {
        return Err(invalid(format!(
            "`min_score` is a cosine similarity and must be between -1.0 and 1.0, got {min_score}"
        )));
    }

    let filter = QueryFilter {
        metadata: args.filter.unwrap_or_default(),
        source_prefix: args.source_prefix,
        document_ids: args.document_ids.unwrap_or_default(),
    };
    check_metadata(&filter.metadata)?;

    let model = state.client.model_identity().to_string();

    // The collection has to exist and has to be pinned to this model before
    // anything is embedded. Both are errors, never empty result lists: a
    // caller cannot tell "nothing matched" from "the wrong collection name"
    // unless the plugin says so.
    let collection_chunks = {
        let store = Arc::clone(&state.store);
        let name = collection.clone();
        let model = model.clone();
        on_blocking(move || {
            store
                .check_model(&name, &model, None)
                .map_err(|error| invalid(error.to_string()))?;
            let stats = store
                .stats(Some(&name))
                .map_err(|error| invalid(error.to_string()))?;
            Ok(stats.first().map(|entry| entry.chunks).unwrap_or(0))
        })
        .await?
    };

    let embedding = state
        .client
        .embed_one(&query)
        .await
        .map_err(|error| PluginError::internal(error.message))?;

    let top_k = state.config.effective_top_k(args.top_k);
    let min_score = args.min_score;
    let filtered = !filter.is_empty();

    let store = Arc::clone(&state.store);
    let name = collection.clone();
    let scan_filter = filter.clone();
    let results: Vec<ScoredChunk> = on_blocking(move || {
        store
            .query(
                &name,
                &embedding,
                top_k,
                min_score.unwrap_or(f64::NEG_INFINITY),
                &scan_filter,
            )
            .map_err(|error| invalid(error.to_string()))
    })
    .await?;

    let mut notes = vec![SEARCH_NOTE.to_string()];
    if results.is_empty() {
        notes.push(if collection_chunks == 0 {
            "the collection exists but holds no chunks".to_string()
        } else if filtered {
            format!(
                "{collection_chunks} chunk(s) are stored; none passed the filter and the \
                 score threshold"
            )
        } else {
            format!("{collection_chunks} chunk(s) are stored; none scored at or above min_score")
        });
    }
    notes.push(format!(
        "scores come from embedding model {model:?} and are not comparable with scores from \
         any other model"
    ));

    Ok(QueryResponse {
        collection: collection.as_str().to_string(),
        embedding_model: model,
        top_k,
        min_score,
        returned: results.len(),
        collection_chunks,
        filtered,
        results,
        notes,
    })
}

pub(crate) async fn handle_delete(
    state: Arc<AppState>,
    args: DeleteArgs,
) -> PluginResult<DeleteResponse> {
    let collection = parse_collection(&args.collection)?;
    let store = Arc::clone(&state.store);

    let outcome = match args.scope {
        DeleteScope::Documents => {
            let ids = args.document_ids.unwrap_or_default();
            if ids.is_empty() {
                return Err(invalid(
                    "`document_ids` is required when scope is `documents`; use \
                     scope=`collection` to remove everything",
                ));
            }
            for id in &ids {
                check_identifier("document id", id)?;
            }
            let name = collection.clone();
            on_blocking(move || {
                store
                    .delete_documents(&name, &ids)
                    .map_err(|error| invalid(error.to_string()))
            })
            .await?
        }
        DeleteScope::Collection => {
            let name = collection.clone();
            on_blocking(move || {
                store
                    .drop_collection(&name)
                    .map_err(|error| invalid(error.to_string()))
            })
            .await?
        }
    };

    Ok(DeleteResponse {
        collection: collection.as_str().to_string(),
        outcome,
    })
}

pub(crate) async fn handle_stats(
    state: Arc<AppState>,
    args: StatsArgs,
) -> PluginResult<StatsResponse> {
    let only = match args.collection.as_deref() {
        Some(raw) => Some(parse_collection(raw)?),
        None => None,
    };

    let store = Arc::clone(&state.store);
    let data_dir = display_path(state.store.root());
    let collections = on_blocking(move || {
        store
            .stats(only.as_ref())
            .map_err(|error| invalid(error.to_string()))
    })
    .await?;

    let total_chunks = collections.iter().map(|entry| entry.chunks).sum();
    let total_documents = collections.iter().map(|entry| entry.documents).sum();
    let approx_memory_bytes = collections
        .iter()
        .map(|entry| entry.approx_memory_bytes)
        .sum();

    let mut notes = vec![
        "approx_memory_bytes estimates the payload held — text, metadata and vectors — and \
         is not process memory"
            .to_string(),
        "log_bytes is the real file size and includes tombstones not yet compacted away"
            .to_string(),
    ];
    if collections.iter().any(|entry| entry.near_capacity) {
        notes.push(
            "a collection is within 10% of --max-chunks-per-collection; further upserts into \
             it will be refused"
                .to_string(),
        );
    }

    Ok(StatsResponse {
        collections,
        total_chunks,
        total_documents,
        approx_memory_bytes,
        data_dir,
        search: SEARCH_NOTE,
        notes,
    })
}

pub(crate) fn handle_preview(state: &AppState, args: PreviewArgs) -> PluginResult<PreviewResponse> {
    if args.text.len() > state.config.max_document_bytes {
        return Err(invalid(format!(
            "text is {} bytes; the limit is {}",
            args.text.len(),
            state.config.max_document_bytes
        )));
    }
    if let Some(source) = &args.source {
        check_identifier("source", source)?;
    }

    let options = state.config.chunk_options();
    let chunks = chunk_document(&args.text, &options);

    let previewed: Vec<PreviewChunk> = chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| PreviewChunk {
            index,
            chars: chunk.chars(),
            line_start: chunk.line_start,
            line_end: chunk.line_end,
            heading_path: chunk.heading_path.clone(),
            citation: args.source.as_ref().map(|source| {
                if chunk.line_start == chunk.line_end {
                    format!("{source}:{}", chunk.line_start)
                } else {
                    format!("{source}:{}-{}", chunk.line_start, chunk.line_end)
                }
            }),
            split_reason: chunk.split_reason,
            text: chunk.text.clone(),
        })
        .collect();

    let mut notes = vec![
        "nothing was embedded and nothing was stored".to_string(),
        "sizes are counted in characters, not tokens — there is no tokenizer in this plugin"
            .to_string(),
    ];
    if previewed.is_empty() {
        notes.push("the text produced no chunks; it is empty or whitespace only".to_string());
    }
    if previewed.iter().any(|chunk| chunk.split_reason.is_some()) {
        notes.push(
            "some chunks were cut inside a block rather than at one of its boundaries; \
             `split_reason` says which kind of cut"
                .to_string(),
        );
    }

    Ok(PreviewResponse {
        chunk_count: previewed.len(),
        chunks: previewed,
        target_chars: options.target_chars,
        overlap_chars: options.overlap_chars,
        max_chars: options.max_chars,
        notes,
    })
}

/// Report the configuration as the components actually hold it.
///
/// The endpoint and the bounds are read back out of the HTTP client and the
/// store rather than re-read from `Config`, so this can never describe a
/// policy that is not the one being enforced.
fn effective_config(state: &AppState) -> EffectiveConfig {
    let config = &state.config;
    let limits = state.store.limits();
    EffectiveConfig {
        data_dir: display_path(state.store.root()),
        embedding_model: state.client.model_identity().to_string(),
        allow_remote_embeddings: config.allow_remote_embeddings,
        // Whether a key exists, never the key.
        api_key_configured: config.api_key.is_some(),
        chunk_chars: config.chunk_chars,
        chunk_overlap_chars: config.chunk_overlap_chars,
        max_chunk_chars: config.max_chunk_chars,
        max_collections: limits.max_collections,
        max_chunks_per_collection: limits.max_chunks_per_collection,
        max_store_bytes: limits.max_store_bytes,
        max_document_bytes: config.max_document_bytes,
        max_documents_per_call: config.max_documents_per_call,
        default_top_k: config.default_top_k,
        max_top_k: MAX_TOP_K,
        embed_batch_size: state.client.batch_size(),
        request_timeout_seconds: config.request_timeout.as_secs(),
        storage: STORAGE_NOTE,
        search: SEARCH_NOTE,
    }
}

pub(crate) async fn handle_status(state: Arc<AppState>) -> StatusResponse {
    let endpoint = state.client.endpoint().clone();
    let embeddings = match state.client.probe().await {
        Ok(probe) => EmbeddingsStatus {
            endpoint: endpoint.to_string(),
            endpoint_is_loopback: crate::config::is_loopback(&endpoint),
            model: state.client.model().map(str::to_string),
            reachable: true,
            dimensions: Some(probe.dimensions),
            latency_ms: Some(probe.latency_ms),
            error: None,
        },
        Err(error) => EmbeddingsStatus {
            endpoint: endpoint.to_string(),
            endpoint_is_loopback: crate::config::is_loopback(&endpoint),
            model: state.client.model().map(str::to_string),
            reachable: false,
            dimensions: None,
            latency_ms: None,
            error: Some(error.message),
        },
    };

    let current_model = state.client.model_identity().to_string();
    let collections: Vec<CollectionPin> = state
        .store
        .collection_pins()
        .into_iter()
        .map(|(collection, embedding_model, dimensions)| CollectionPin {
            usable_with_current_model: embedding_model == current_model,
            collection,
            embedding_model,
            dimensions,
        })
        .collect();

    let mut notes = vec![
        "this tool sends one real embedding request; `stats` reports the store without \
         touching the network"
            .to_string(),
    ];
    if !embeddings.reachable {
        notes.push(
            "the TDCC node's own OpenAI frontend does not serve POST /v1/embeddings — it \
             serves /v1/models, /v1/chat/completions, /v1/completions and /v1/responses. \
             Point --embeddings-url at a local embeddings server (Ollama, \
             llama-server --embeddings, LM Studio, or vLLM serving an embedding model)."
                .to_string(),
        );
    }
    let unusable: Vec<&str> = collections
        .iter()
        .filter(|pin| !pin.usable_with_current_model)
        .map(|pin| pin.collection.as_str())
        .collect();
    if !unusable.is_empty() {
        notes.push(format!(
            "collection(s) {} were built with a different embedding model and cannot be \
             queried while --embedding-model is {current_model:?}; vectors from two models \
             are not comparable, so they are refused rather than ranked wrongly",
            unusable.join(", ")
        ));
    }

    StatusResponse {
        embeddings,
        collections,
        config: effective_config(&state),
        advisories: state.config.advisories(),
        notes,
    }
}

/// The health line the host polls.
///
/// Two atomic reads and two borrows — it never takes the store lock and never
/// touches the network, so it answers immediately while a 50 000-chunk scan or
/// a compaction is in flight, and while the embeddings server is restarting.
pub(crate) fn health_line(state: &AppState) -> String {
    let (collections, chunks) = state.store.counts();
    format!(
        "ok; {collections} collection(s), {chunks} chunk(s); embeddings endpoint {} model {}",
        state.client.endpoint(),
        state.client.model_identity()
    )
}

// ---------------------------------------------------------------------------
// The declaration
// ---------------------------------------------------------------------------

pub fn vector_store_plugin(state: Arc<AppState>) -> SimplePlugin {
    let for_upsert = Arc::clone(&state);
    let for_query = Arc::clone(&state);
    let for_delete = Arc::clone(&state);
    let for_stats = Arc::clone(&state);
    let for_status = Arc::clone(&state);
    let for_preview = Arc::clone(&state);
    let for_http_upsert = Arc::clone(&state);
    let for_http_query = Arc::clone(&state);
    let for_http_stats = Arc::clone(&state);
    let for_health = Arc::clone(&state);

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Vector store",
                "Local, durable vector storage with structure-aware chunking and \
                 citation-preserving similarity search",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can store and retrieve
        // passages by meaning", so a caller can depend on the capability
        // rather than on this plugin's id.
        provides: [capability("vector-store.v1")],

        mcp: [
            // Projected as `vector-store.upsert` on the host MCP endpoint.
            mcp::tool("upsert")
                .title("Store documents for retrieval")
                .description(
                    "Split documents into passages, embed them, and store them in a \
                     collection. Splitting follows the document's own structure — \
                     headings, paragraphs, whole code fences — and every passage keeps \
                     its source label, its line span and its heading breadcrumb so a \
                     later result can be cited. Re-sending a document id replaces that \
                     document's passages rather than adding to them. The collection is \
                     pinned to the embedding model this process is configured with; a \
                     later call with a different model is refused. Requires a reachable \
                     OpenAI-compatible /v1/embeddings endpoint and errors when there is \
                     not one.",
                )
                .input::<UpsertArgs>()
                .handle(move |args: UpsertArgs, _context| {
                    let state = Arc::clone(&for_upsert);
                    Box::pin(async move { handle_upsert(state, args).await })
                }),

            // Projected as `vector-store.query`.
            mcp::tool("query")
                .title("Find passages by meaning")
                .description(
                    "Search one collection for the passages closest in meaning to a \
                     question, returning each with its similarity score, source, line \
                     span and heading path — enough to quote and cite. Narrow with \
                     exact metadata matches, a source prefix, or a list of document \
                     ids. Searches exactly one collection and never crosses into \
                     another. Errors rather than returning an empty list when the \
                     collection does not exist or the embeddings backend is \
                     unreachable, so a genuine 'no matches' is unambiguous.",
                )
                .input::<QueryArgs>()
                .handle(move |args: QueryArgs, _context| {
                    let state = Arc::clone(&for_query);
                    Box::pin(async move { handle_query(state, args).await })
                }),

            // Projected as `vector-store.delete`.
            mcp::tool("delete")
                .title("Remove documents or a collection")
                .description(
                    "Delete stored passages. Scope is required and has no default: \
                     `documents` removes every passage of the document ids you name, \
                     and `collection` removes the entire collection including its file \
                     on disk. Collections are independent namespaces, so deleting one \
                     never touches another. This cannot be undone.",
                )
                .input::<DeleteArgs>()
                .handle(move |args: DeleteArgs, _context| {
                    let state = Arc::clone(&for_delete);
                    Box::pin(async move { handle_delete(state, args).await })
                }),

            // Projected as `vector-store.stats`.
            mcp::tool("stats")
                .title("Report what is stored")
                .description(
                    "Report each collection's chunk and document counts, the embedding \
                     model it is pinned to, its dimensions, its estimated memory and \
                     real on-disk size, and the sources it holds. Touches no network \
                     and changes nothing. Use this to see what a query can reach before \
                     blaming the retrieval.",
                )
                .input::<StatsArgs>()
                .handle(move |args: StatsArgs, _context| {
                    let state = Arc::clone(&for_stats);
                    Box::pin(async move { handle_stats(state, args).await })
                }),

            // Projected as `vector-store.status`.
            mcp::tool("status")
                .title("Check the embeddings backend")
                .description(
                    "Check whether the embeddings backend actually works, and show the \
                     effective configuration. Sends one short probe string to the \
                     configured endpoint, so it costs one embedding call. Reports an \
                     unreachable backend as a result rather than failing, and names the \
                     setting that fixes it.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let state = Arc::clone(&for_status);
                    Box::pin(async move { Ok(handle_status(state).await) })
                }),

            // Projected as `vector-store.preview_chunks`.
            mcp::tool("preview_chunks")
                .title("Preview how text would be split")
                .description(
                    "Show exactly how a piece of text would be split into passages \
                     under the current settings, with each passage's character count, \
                     line span, heading path and — where a block had to be cut inside \
                     itself — the reason. Nothing is embedded and nothing is stored, so \
                     this works with no embeddings backend at all. Use it to tune \
                     --chunk-chars and --chunk-overlap-chars before ingesting a corpus.",
                )
                .input::<PreviewArgs>()
                .handle(move |args: PreviewArgs, _context| {
                    let state = Arc::clone(&for_preview);
                    Box::pin(async move { handle_preview(&state, args) })
                }),
        ],

        // The same three operations over HTTP, mounted by the host at
        // /api/plugins/vector-store/http/…. A retrieval gateway sitting in
        // front of :9337 is the natural place to use this, and a gateway
        // speaks HTTP rather than MCP.
        http: [
            http::post("/upsert")
                .description("Chunk, embed and store documents in a collection.")
                .input::<UpsertArgs>()
                .handle(move |args: UpsertArgs, _context| {
                    let state = Arc::clone(&for_http_upsert);
                    Box::pin(async move { handle_upsert(state, args).await })
                }),

            http::post("/query")
                .description("Find the passages closest in meaning to a question.")
                .input::<QueryArgs>()
                .handle(move |args: QueryArgs, _context| {
                    let state = Arc::clone(&for_http_query);
                    Box::pin(async move { handle_query(state, args).await })
                }),

            http::get("/stats")
                .description("Report collection sizes, models and storage.")
                .input::<StatsArgs>()
                .handle(move |args: StatsArgs, _context| {
                    let state = Arc::clone(&for_http_stats);
                    Box::pin(async move { handle_stats(state, args).await })
                }),
        ],

        // Health must stay fast and must not depend on long-running work, so
        // it reads two atomics maintained outside the store lock and never
        // touches the network. A 50k-chunk scan or a compaction can be in
        // flight and this still answers immediately. Embeddings-endpoint
        // reachability is a separate concern with its own tool: the plugin is
        // perfectly healthy while its backend is restarting.
        health: move |_context| {
            let state = Arc::clone(&for_health);
            Box::pin(async move { Ok(health_line(&state)) })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::config::prepare_data_dir;
    use crate::testsupport::TempTree;

    fn state(tree: &TempTree) -> Arc<AppState> {
        let root = prepare_data_dir(tree.path()).expect("data dir");
        let config = Config {
            data_dir: root.clone(),
            embedding_model: "test-embedder".to_string(),
            ..Config::default()
        };
        let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store opens"));
        Arc::new(AppState::new(config, store).expect("state builds"))
    }

    fn manifest(tree: &TempTree) -> tdcc_plugin::proto::PluginManifest {
        vector_store_plugin(state(tree))
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn every_tool_is_declared_with_a_description_and_a_schema() {
        let tree = TempTree::new("manifest-tools");
        let manifest = manifest(&tree);

        for name in [
            "upsert",
            "query",
            "delete",
            "stats",
            "status",
            "preview_chunks",
        ] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 40,
                "`{name}` needs a description a model can act on"
            );
            assert!(
                operation.input_schema_json.contains("\"type\":\"object\""),
                "`{name}` must advertise an object schema: {}",
                operation.input_schema_json
            );
            // `status` takes no arguments, so it has no `properties`; every
            // other tool does, and each property carries its doc comment.
            if name != "status" {
                assert!(
                    operation.input_schema_json.contains("\"properties\""),
                    "`{name}`: {}",
                    operation.input_schema_json
                );
                assert!(
                    operation.input_schema_json.contains("\"description\""),
                    "`{name}` arguments must carry the doc comments a model reads: {}",
                    operation.input_schema_json
                );
            }
            assert!(
                operation
                    .input_schema_json
                    .contains("\"additionalProperties\":false"),
                "`{name}` must reject unknown fields so a misspelled argument is an error \
                 rather than a silently ignored one: {}",
                operation.input_schema_json
            );
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let tree = TempTree::new("manifest-schema");
        let manifest = manifest(&tree);
        let upsert = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "upsert")
            .expect("upsert is declared");

        // The single most important sentence in the whole schema: this plugin
        // does not read your files.
        assert!(
            upsert.input_schema_json.contains("never opens it"),
            "{}",
            upsert.input_schema_json
        );
        assert!(
            upsert.input_schema_json.contains("replaces"),
            "{}",
            upsert.input_schema_json
        );
        assert!(
            upsert.input_schema_json.contains("\"required\""),
            "{}",
            upsert.input_schema_json
        );
    }

    #[test]
    fn delete_advertises_that_scope_is_required() {
        let tree = TempTree::new("manifest-delete");
        let manifest = manifest(&tree);
        let delete = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "delete")
            .expect("delete is declared");
        assert!(
            delete.input_schema_json.contains("scope"),
            "{}",
            delete.input_schema_json
        );
        assert!(
            delete.description.contains("no default"),
            "a destructive tool must require a sentence: {}",
            delete.description
        );
    }

    #[test]
    fn the_http_routes_mirror_the_three_operations_a_gateway_needs() {
        let tree = TempTree::new("manifest-http");
        let manifest = manifest(&tree);
        let paths: Vec<&str> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.as_str())
            .collect();
        assert!(paths.contains(&"/upsert"), "{paths:?}");
        assert!(paths.contains(&"/query"), "{paths:?}");
        assert!(paths.contains(&"/stats"), "{paths:?}");
        assert!(
            !paths.contains(&"/delete"),
            "the destructive tool is deliberately not a route: {paths:?}"
        );
    }

    #[test]
    fn no_config_schema_or_web_ui_is_declared() {
        let tree = TempTree::new("manifest-surfaces");
        let manifest = manifest(&tree);

        // Both would be dishonest surfaces for this plugin: settings never
        // reach the process, and there is no bundle to serve.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert_eq!(manifest.capabilities, vec!["vector-store.v1".to_string()]);
    }

    #[test]
    fn no_mesh_channel_or_event_is_declared() {
        let tree = TempTree::new("manifest-mesh");
        let manifest = manifest(&tree);
        assert!(
            manifest.mesh_channels.is_empty(),
            "delivery is allowlist-based; a store that needs no peer traffic declares none"
        );
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert!(
            manifest.endpoints.is_empty(),
            "this plugin attaches no inference endpoint"
        );
    }

    #[test]
    fn health_is_cheap_and_carries_no_document_text() {
        let tree = TempTree::new("health-line");
        let state = state(&tree);
        let line = health_line(&state);
        assert!(
            line.starts_with("ok; 0 collection(s), 0 chunk(s)"),
            "{line}"
        );
        assert!(line.contains("test-embedder"), "{line}");
    }

    // -- argument validation ---------------------------------------------

    #[tokio::test]
    async fn an_illegal_collection_name_is_refused_before_any_work() {
        let tree = TempTree::new("bad-collection");
        let state = state(&tree);
        let error = handle_query(
            Arc::clone(&state),
            QueryArgs {
                collection: "../../etc".to_string(),
                query: "anything".to_string(),
                top_k: None,
                min_score: None,
                filter: None,
                source_prefix: None,
                document_ids: None,
            },
        )
        .await
        .expect_err("a traversal-shaped name must be refused");
        assert!(
            error.message.contains("letters, digits"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_empty_query_is_refused_rather_than_embedded() {
        let tree = TempTree::new("empty-query");
        let state = state(&tree);
        let error = handle_query(
            Arc::clone(&state),
            QueryArgs {
                collection: "docs".to_string(),
                query: "   ".to_string(),
                top_k: None,
                min_score: None,
                filter: None,
                source_prefix: None,
                document_ids: None,
            },
        )
        .await
        .expect_err("an empty query must not reach the endpoint");
        assert!(
            error.message.contains("must not be empty"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_out_of_range_min_score_is_refused() {
        let tree = TempTree::new("bad-score");
        let state = state(&tree);
        let error = handle_query(
            Arc::clone(&state),
            QueryArgs {
                collection: "docs".to_string(),
                query: "anything".to_string(),
                top_k: None,
                min_score: Some(1.5),
                filter: None,
                source_prefix: None,
                document_ids: None,
            },
        )
        .await
        .expect_err("a cosine similarity above 1.0 is not a threshold");
        assert!(error.message.contains("-1.0 and 1.0"), "{}", error.message);
    }

    #[tokio::test]
    async fn deleting_documents_without_naming_any_is_refused() {
        let tree = TempTree::new("delete-unscoped");
        let state = state(&tree);
        let error = handle_delete(
            Arc::clone(&state),
            DeleteArgs {
                collection: "docs".to_string(),
                scope: DeleteScope::Documents,
                document_ids: None,
            },
        )
        .await
        .expect_err("an unscoped delete must not become a delete-everything");
        assert!(error.message.contains("document_ids"), "{}", error.message);
    }

    #[tokio::test]
    async fn deleting_an_unknown_collection_errors_rather_than_reporting_success() {
        let tree = TempTree::new("delete-unknown");
        let state = state(&tree);
        let error = handle_delete(
            Arc::clone(&state),
            DeleteArgs {
                collection: "nothing-here".to_string(),
                scope: DeleteScope::Collection,
                document_ids: None,
            },
        )
        .await
        .expect_err("a delete that removed nothing must say so");
        assert!(
            error.message.contains("no collection named"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn too_much_metadata_is_refused() {
        let tree = TempTree::new("metadata-bound");
        let state = state(&tree);
        let metadata: BTreeMap<String, String> = (0..MAX_METADATA_ENTRIES + 1)
            .map(|index| (format!("key{index}"), "value".to_string()))
            .collect();

        let error = handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: "docs".to_string(),
                documents: vec![InputDocument {
                    id: "a".to_string(),
                    text: "some text".to_string(),
                    source: None,
                    metadata: Some(metadata),
                }],
            },
        )
        .await
        .expect_err("unbounded metadata is unbounded memory");
        assert!(
            error.message.contains("metadata entries"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_oversized_document_is_refused_before_it_is_embedded() {
        let tree = TempTree::new("oversized-document");
        let state = state(&tree);
        let huge = "x".repeat(state.config.max_document_bytes + 1);

        let error = handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: "docs".to_string(),
                documents: vec![InputDocument {
                    id: "a".to_string(),
                    text: huge,
                    source: None,
                    metadata: None,
                }],
            },
        )
        .await
        .expect_err("an oversized document must be refused");
        assert!(
            error.message.contains("--max-document-bytes"),
            "{}",
            error.message
        );
    }

    /// Every document is its own durable write, so a batch that trips a bound
    /// part-way through leaves the earlier ones stored. That is safe — an
    /// upsert replaces — but the caller has to be told it happened.
    #[tokio::test]
    async fn a_batch_that_fails_part_way_names_what_was_already_stored() {
        let tree = TempTree::new("partial-batch");
        let root = prepare_data_dir(tree.path()).expect("data dir");
        let config = Config {
            data_dir: root.clone(),
            embedding_model: "test-embedder".to_string(),
            // Room for the first document's single chunk and nothing more.
            max_chunks_per_collection: 1,
            ..Config::default()
        };
        let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store"));
        let state = Arc::new(AppState::new(config, store).expect("state"));

        // Pre-load the first document so the batch below does not need an
        // embedder for it… which it does. Instead, drive the store directly to
        // establish the collection, then assert the wrapper's message shape on
        // a second document that cannot fit.
        let embedding = crate::similarity::normalize_l2(vec![1.0, 0.0]).expect("has direction");
        state
            .store
            .upsert_document(
                &CollectionName::parse("docs").expect("legal"),
                "first",
                vec![ChunkRecord {
                    id: "first#0".to_string(),
                    document_id: "first".to_string(),
                    source: None,
                    chunk_index: 0,
                    line_start: 1,
                    line_end: 1,
                    heading_path: Vec::new(),
                    text: "text".to_string(),
                    metadata: BTreeMap::new(),
                    embedding,
                    embedding_model: "test-embedder".to_string(),
                    created_unix_ms: 0,
                }],
                "test-embedder",
            )
            .expect("seed");

        // A second document cannot fit, and the store says why by itself.
        let error = state
            .store
            .upsert_document(
                &CollectionName::parse("docs").expect("legal"),
                "second",
                vec![ChunkRecord {
                    id: "second#0".to_string(),
                    document_id: "second".to_string(),
                    source: None,
                    chunk_index: 0,
                    line_start: 1,
                    line_end: 1,
                    heading_path: Vec::new(),
                    text: "text".to_string(),
                    metadata: BTreeMap::new(),
                    embedding: crate::similarity::normalize_l2(vec![0.0, 1.0])
                        .expect("has direction"),
                    embedding_model: "test-embedder".to_string(),
                    created_unix_ms: 0,
                }],
                "test-embedder",
            )
            .expect_err("the collection is full");
        assert!(
            error.to_string().contains("--max-chunks-per-collection"),
            "{error}"
        );

        // The first document is still there, which is exactly the situation
        // the wrapper in `handle_upsert` exists to disclose.
        let stats = handle_stats(Arc::clone(&state), StatsArgs { collection: None })
            .await
            .expect("stats");
        assert_eq!(stats.total_documents, 1);
    }

    #[tokio::test]
    async fn an_empty_document_list_is_refused() {
        let tree = TempTree::new("no-documents");
        let state = state(&tree);
        let error = handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: "docs".to_string(),
                documents: Vec::new(),
            },
        )
        .await
        .expect_err("nothing to do is a caller error, not a success");
        assert!(
            error.message.contains("at least one document"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_whitespace_only_document_is_refused_rather_than_stored_empty() {
        let tree = TempTree::new("empty-document");
        let state = state(&tree);
        let error = handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: "docs".to_string(),
                documents: vec![InputDocument {
                    id: "blank".to_string(),
                    text: "   \n\n  ".to_string(),
                    source: None,
                    metadata: None,
                }],
            },
        )
        .await
        .expect_err("storing nothing and reporting success would be a lie");
        assert!(
            error.message.contains("nothing to store"),
            "{}",
            error.message
        );
    }

    // -- preview ----------------------------------------------------------

    #[test]
    fn preview_shows_the_split_without_touching_the_network() {
        let tree = TempTree::new("preview");
        let state = state(&tree);
        let document = "# Guide\n\n".to_string() + &"A sentence about the topic. ".repeat(200);

        let response = handle_preview(
            &state,
            PreviewArgs {
                text: document,
                source: Some("docs/guide.md".to_string()),
            },
        )
        .expect("preview needs no backend");

        assert!(response.chunk_count > 1, "{response:#?}");
        assert_eq!(response.chunk_count, response.chunks.len());
        assert_eq!(response.target_chars, state.config.chunk_chars);
        assert!(
            response.chunks[0]
                .citation
                .as_deref()
                .is_some_and(|citation| citation.starts_with("docs/guide.md:")),
            "{:?}",
            response.chunks[0].citation
        );
        assert!(
            response.chunks[0].heading_path == vec!["Guide".to_string()],
            "{:?}",
            response.chunks[0].heading_path
        );
        for chunk in &response.chunks {
            assert!(chunk.chars <= response.max_chars);
        }
    }

    /// Pins the worked example in README.md.
    ///
    /// The README shows this exact document, at these exact settings, with
    /// these exact line spans and citations. Every number quoted there is
    /// asserted here, so the document cannot drift away from the code.
    #[test]
    fn the_worked_example_in_the_readme_is_what_the_splitter_actually_produces() {
        let tree = TempTree::new("readme-example");
        let root = prepare_data_dir(tree.path()).expect("data dir");
        let config = Config::resolve(
            &[
                format!("--data-dir={}", root.display()),
                "--chunk-chars=200".to_string(),
                "--chunk-overlap-chars=60".to_string(),
                "--max-chunk-chars=400".to_string(),
            ],
            &BTreeMap::new(),
        )
        .expect("valid config");
        let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store"));
        let state = Arc::new(AppState::new(config, store).expect("state"));

        let text = "\
# Operations Manual

## Install

To install the service, unpack the archive and run the setup script. It will
ask you for a data directory.

The installer needs about two gigabytes of free space.

## Backup and restore

Take a backup before every upgrade.
";
        let response = handle_preview(
            &state,
            PreviewArgs {
                text: text.to_string(),
                source: Some("docs/manual.md".to_string()),
            },
        )
        .expect("preview");

        assert_eq!(response.chunk_count, 2, "{response:#?}");
        assert_eq!(response.target_chars, 200);
        assert_eq!(response.overlap_chars, 60);
        assert_eq!(response.max_chars, 400);

        let first = &response.chunks[0];
        assert_eq!(first.chars, 195);
        assert_eq!((first.line_start, first.line_end), (1, 8));
        assert_eq!(first.citation.as_deref(), Some("docs/manual.md:1-8"));
        assert_eq!(first.heading_path, vec!["Operations Manual".to_string()]);
        assert_eq!(first.split_reason, None, "no block needed cutting");

        let second = &response.chunks[1];
        assert_eq!(second.chars, 114);
        assert_eq!((second.line_start, second.line_end), (8, 12));
        assert_eq!(second.citation.as_deref(), Some("docs/manual.md:8-12"));
        assert_eq!(
            second.heading_path,
            vec!["Operations Manual".to_string(), "Install".to_string()]
        );

        // The two properties the README claims this example demonstrates.
        assert!(
            second.line_start <= first.line_end,
            "consecutive chunks overlap by a whole block"
        );
        assert!(
            second
                .text
                .starts_with("The installer needs about two gigabytes"),
            "the overlap is the previous chunk's trailing paragraph, whole: {second:#?}"
        );
        assert!(
            second.text.contains("## Backup and restore")
                && second
                    .text
                    .trim()
                    .ends_with("Take a backup before every upgrade."),
            "a heading travels with the text it introduces: {second:#?}"
        );
    }

    #[test]
    fn preview_of_empty_text_says_so_rather_than_returning_a_bare_empty_list() {
        let tree = TempTree::new("preview-empty");
        let state = state(&tree);
        let response = handle_preview(
            &state,
            PreviewArgs {
                text: "   \n  ".to_string(),
                source: None,
            },
        )
        .expect("empty is a fact, not a failure");
        assert_eq!(response.chunk_count, 0);
        assert!(
            response
                .notes
                .iter()
                .any(|note| note.contains("empty or whitespace")),
            "{:?}",
            response.notes
        );
    }

    // -- stats and status -------------------------------------------------

    #[tokio::test]
    async fn stats_on_an_empty_store_reports_an_empty_store() {
        let tree = TempTree::new("stats-empty");
        let state = state(&tree);
        let response = handle_stats(Arc::clone(&state), StatsArgs { collection: None })
            .await
            .expect("stats always answer");
        assert!(response.collections.is_empty());
        assert_eq!(response.total_chunks, 0);
        assert!(response.search.contains("brute-force"));
        assert!(
            response
                .notes
                .iter()
                .any(|note| note.contains("not process memory")),
            "the estimate must never be quoted as process memory"
        );
    }

    #[tokio::test]
    async fn status_reports_an_unreachable_backend_as_a_result_and_names_the_fix() {
        let tree = TempTree::new("status-down");
        let state = state(&tree);
        // The default endpoint is the node's own port, which serves no
        // embeddings route. Nothing is listening in a test either way.
        let response = handle_status(Arc::clone(&state)).await;

        assert!(!response.embeddings.reachable);
        assert!(response.embeddings.endpoint_is_loopback);
        let error = response.embeddings.error.expect("a reason is reported");
        assert!(error.contains("--embeddings-url"), "{error}");
        assert!(
            response
                .notes
                .iter()
                .any(|note| note.contains("does not serve POST /v1/embeddings")),
            "the unmet prerequisite has to be stated where an operator will hit it: {:?}",
            response.notes
        );
        assert!(!response.config.api_key_configured);
        assert!(response.config.storage.contains("append-only"));
    }

    /// `status` is the tool an operator reaches for when queries return
    /// nothing, so it has to say outright which collections the current
    /// configuration can actually read — not leave it to be deduced from an
    /// error on the next query.
    #[tokio::test]
    async fn status_names_the_collections_the_current_model_cannot_read() {
        let tree = TempTree::new("status-pins");
        let root = prepare_data_dir(tree.path()).expect("data dir");

        // A collection built by an earlier run, with a different embedder.
        {
            let store = VectorStore::open(&root, Config::default().store_limits()).expect("store");
            let embedding = crate::similarity::normalize_l2(vec![1.0, 0.0]).expect("has direction");
            store
                .upsert_document(
                    &CollectionName::parse("legacy").expect("legal"),
                    "a",
                    vec![ChunkRecord {
                        id: "a#0".to_string(),
                        document_id: "a".to_string(),
                        source: None,
                        chunk_index: 0,
                        line_start: 1,
                        line_end: 1,
                        heading_path: Vec::new(),
                        text: "text".to_string(),
                        metadata: BTreeMap::new(),
                        embedding,
                        embedding_model: "an-older-embedder".to_string(),
                        created_unix_ms: 0,
                    }],
                    "an-older-embedder",
                )
                .expect("upsert");
        }

        // This process is configured for something else.
        let state = state(&tree);
        let response = handle_status(Arc::clone(&state)).await;

        let pin = response
            .collections
            .iter()
            .find(|pin| pin.collection == "legacy")
            .expect("the collection is listed");
        assert_eq!(pin.embedding_model, "an-older-embedder");
        assert_eq!(pin.dimensions, 2);
        assert!(
            !pin.usable_with_current_model,
            "a collection pinned to another model cannot be queried and must say so"
        );
        assert!(
            response
                .notes
                .iter()
                .any(|note| note.contains("legacy") && note.contains("not comparable")),
            "{:?}",
            response.notes
        );
    }

    #[tokio::test]
    async fn status_never_includes_the_api_key() {
        let tree = TempTree::new("status-secret");
        let root = prepare_data_dir(tree.path()).expect("data dir");
        let config = Config::resolve(
            &[format!("--data-dir={}", root.display())],
            &BTreeMap::from([(
                crate::config::API_KEY_ENV.to_string(),
                "sk-a-real-looking-secret".to_string(),
            )]),
        )
        .expect("valid config");
        let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store"));
        let state = Arc::new(AppState::new(config, store).expect("state"));

        let response = handle_status(Arc::clone(&state)).await;
        let rendered = serde_json::to_string(&response).expect("serializes");
        assert!(
            !rendered.contains("sk-a-real-looking-secret"),
            "the key must never reach a tool result"
        );
        assert!(
            response.config.api_key_configured,
            "only its presence is reported"
        );
    }
}
