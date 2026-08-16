//! The whole contribution surface of `semantic-cache` in one declaration.
//!
//! Five MCP tools, the same three of them mirrored as HTTP routes (a proxy
//! sitting in front of `:9337` can use HTTP without speaking MCP), one
//! capability, and a health hook. No `config` block and no `web_ui` block —
//! see the module comment in [`crate::config`] for why declaring settings this
//! process cannot read would be worse than declaring none.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{
    PluginError, PluginMetadata, PluginResult, SimplePlugin, capability, http, mcp, plugin,
    plugin_server_info,
};

use crate::config::Config;
use crate::embeddings::EmbeddingClient;
use crate::keying::{ChatMessage, KeyInputs, shape};
use crate::policy::{
    StoreCandidate, StoreRejection, effective_threshold, fits_budget, store_decision,
    temperature_gate,
};
use crate::store::{
    CacheEntry, CacheStore, Clock, InsertOutcome, Limits, LookupResult, MatchKind, MissReason,
    NewEntry, PurgeScope, StatsSnapshot,
};

/// Text sent to the embeddings endpoint by `status`.
const PROBE_TEXT: &str = "tdcc semantic-cache endpoint probe";

/// Everything the handlers share. Cheap to clone behind an `Arc`.
pub struct AppState {
    pub config: Config,
    pub store: CacheStore,
    pub client: EmbeddingClient,
    pub clock: Clock,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, String> {
        let client = EmbeddingClient::new(&config).map_err(|error| error.message)?;
        let store = CacheStore::new(Limits {
            max_entries: config.max_entries,
            max_bytes: config.max_bytes,
            ttl_seconds: config.ttl_seconds,
        });
        Ok(Self {
            config,
            store,
            client,
            clock: Clock::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tool arguments
//
// Doc comments on these fields become the descriptions in the JSON Schema the
// host advertises, which is what a model or an operator reads before calling
// the tool. They are written for that audience.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupArgs {
    /// Completion model the request would go to, e.g. `qwen3-8b`. Answers are
    /// never shared between models.
    pub model: String,
    /// The full conversation, in order, ending on the `user` turn you want an
    /// answer for. Everything before that final turn — including the system
    /// prompt — must match a cached entry exactly; only the final turn is
    /// matched by meaning.
    pub messages: Vec<ChatMessage>,
    /// Sampling temperature, exactly as it would be sent upstream. Omit it
    /// only if you would also omit it upstream: an omitted value and an
    /// explicit `1.0` are kept in separate cache buckets.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter, exactly as it would be sent upstream.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Tool definitions the request would offer, exactly as sent upstream.
    /// Order does not matter; presence does.
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// Any other request field that changes the answer and is not listed above
    /// — `response_format`, `seed`, a prompt-template version, a retrieval
    /// corpus id. Requests with different values here never match each other.
    #[serde(default)]
    pub extra_key: Option<String>,
    /// Raise the similarity threshold for this call only. Values below the
    /// operator's configured minimum are clamped up to it: a caller may be
    /// stricter than the node's policy, never looser.
    #[serde(default)]
    pub min_similarity: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreArgs {
    /// Completion model that produced this answer.
    pub model: String,
    /// The conversation the answer responds to, ending on the `user` turn.
    pub messages: Vec<ChatMessage>,
    /// The answer text to cache.
    pub completion: String,
    /// Sampling temperature the answer was produced with.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter the answer was produced with.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Tool definitions that were offered when the answer was produced.
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// Same discriminator as on `lookup`; must match for a later lookup to
    /// find this entry.
    #[serde(default)]
    pub extra_key: Option<String>,
    /// The upstream `finish_reason`. Only `stop` is cached — `length` is a
    /// truncated answer, `tool_calls` is a request to go and do something, and
    /// `content_filter` is a refusal that may not apply to another wording.
    /// Omitting this is treated as `stop`.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Set to true when the upstream call failed. An errored response is never
    /// cached: doing so would serve the error back for the whole TTL, long
    /// after whatever caused it had recovered.
    #[serde(default)]
    pub is_error: bool,
    /// Prompt tokens the upstream call consumed, for the tokens-saved figure
    /// in `stats`. Unknown counts may be omitted; the saving is then
    /// undercounted rather than guessed.
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Completion tokens the upstream call produced.
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// Override the configured TTL for this entry, in seconds. Use a short
    /// value for anything that reads current state. `0` means never expire and
    /// should be reserved for genuinely timeless answers.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PurgeSelector {
    /// Drop only entries whose TTL has elapsed. Always safe.
    Expired,
    /// Drop every entry for one completion model.
    Model,
    /// Drop the entire cache.
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PurgeArgs {
    /// What to drop. There is no default: clearing a cache is cheap to do and
    /// impossible to undo, so the caller has to say which.
    pub scope: PurgeSelector,
    /// Required when `scope` is `model`; ignored otherwise.
    #[serde(default)]
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool responses
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EntryInfo {
    pub entry_id: u64,
    pub age_seconds: u64,
    pub stored_at_unix_ms: u64,
    pub expires_in_seconds: Option<u64>,
    pub previous_hits: u64,
    /// The wording that was originally cached, so a caller can see what the
    /// reworded prompt actually matched.
    pub cached_query: String,
}

#[derive(Debug, Serialize)]
pub struct TokenCounts {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct LookupResponse {
    pub hit: bool,
    /// `exact` when the wording was identical after whitespace normalization
    /// (which costs no embedding call), `semantic` when it was a different
    /// wording that scored above the threshold.
    pub match_kind: Option<MatchKind>,
    pub similarity: Option<f64>,
    /// The threshold this call was decided against, after clamping.
    pub threshold: f64,
    pub completion: Option<String>,
    pub entry: Option<EntryInfo>,
    /// Tokens this hit avoided, as reported when the entry was stored.
    pub tokens_saved: Option<TokenCounts>,
    pub miss_reason: Option<String>,
    /// The best score in the bucket even when it lost — the number to tune
    /// `--min-similarity` against.
    pub best_similarity: Option<f64>,
    /// Entries dropped by the TTL sweep this lookup performed.
    pub expired_removed: u64,
    /// Digest of the exact-match part of the key. Two requests with the same
    /// value here are comparable; two with different values never are.
    pub bucket: String,
}

#[derive(Debug, Serialize)]
pub struct StoreResponse {
    pub stored: bool,
    pub entry_id: Option<u64>,
    /// True when this replaced an existing entry for the same wording rather
    /// than adding a row.
    pub replaced: bool,
    pub expires_in_seconds: Option<u64>,
    /// Entries evicted to make room for this one.
    pub evicted: u64,
    /// Present only when `stored` is false, naming the rule that refused it.
    pub rejected: Option<StoreRejection>,
    pub bucket: String,
}

#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    pub embeddings_endpoint: String,
    pub embedding_model: Option<String>,
    pub endpoint_is_loopback: bool,
    pub allow_remote_embeddings: bool,
    pub api_key_configured: bool,
    pub min_similarity: f64,
    pub ttl_seconds: u64,
    pub max_entries: usize,
    pub max_bytes: u64,
    pub max_temperature: f64,
    pub max_length_ratio: f64,
    pub request_timeout_seconds: u64,
    /// Storage is in-process memory. Nothing is written to disk, and the cache
    /// is empty again after a restart.
    pub storage: &'static str,
    pub eviction: &'static str,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    #[serde(flatten)]
    pub snapshot: StatsSnapshot,
    pub config: EffectiveConfig,
    /// How to read the numbers above, carried with them so the figure is never
    /// quoted without its caveat.
    pub notes: [&'static str; 2],
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum EndpointProbe {
    Reachable {
        reachable: bool,
        dimensions: usize,
        latency_ms: u64,
    },
    Unreachable {
        reachable: bool,
        error: String,
    },
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub plugin: &'static str,
    pub version: &'static str,
    pub embeddings: EndpointProbe,
    pub entries: usize,
    pub config: EffectiveConfig,
}

#[derive(Debug, Serialize)]
pub struct PurgeResponse {
    pub removed: u64,
    pub remaining: usize,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn key_inputs<'a>(
    model: &'a str,
    embedding_model: &'a str,
    temperature: Option<f64>,
    top_p: Option<f64>,
    tools: &'a [serde_json::Value],
    extra: &'a str,
) -> KeyInputs<'a> {
    KeyInputs {
        model,
        embedding_model,
        temperature,
        top_p,
        tools,
        extra,
    }
}

/// The embedding model as it appears in cache keys. An unset model is its own
/// distinct value, so entries created before one was configured never get
/// compared against entries created after.
fn embedder_id(state: &AppState) -> &str {
    state.client.model().unwrap_or("<unset>")
}

fn entry_info(entry: &CacheEntry, now_ms: u64) -> EntryInfo {
    EntryInfo {
        entry_id: entry.id,
        age_seconds: entry.age_seconds(now_ms),
        stored_at_unix_ms: entry.created_wall_ms,
        expires_in_seconds: entry
            .expires_ms
            .map(|expires| expires.saturating_sub(now_ms) / 1_000),
        previous_hits: entry.hits,
        cached_query: entry.query.clone(),
    }
}

fn token_counts(entry: &CacheEntry) -> TokenCounts {
    TokenCounts {
        prompt_tokens: entry.prompt_tokens,
        completion_tokens: entry.completion_tokens,
        total_tokens: entry.prompt_tokens + entry.completion_tokens,
    }
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
        embeddings_endpoint: state.client.endpoint().to_string(),
        embedding_model: state.client.model().map(str::to_string),
        endpoint_is_loopback: crate::config::is_loopback(state.client.endpoint()),
        allow_remote_embeddings: config.allow_remote_embeddings,
        // Whether a key exists, never the key.
        api_key_configured: config.api_key.is_some(),
        min_similarity: config.min_similarity,
        ttl_seconds: limits.ttl_seconds,
        max_entries: limits.max_entries,
        max_bytes: limits.max_bytes,
        max_temperature: config.max_temperature,
        max_length_ratio: config.max_length_ratio,
        request_timeout_seconds: config.request_timeout.as_secs(),
        storage: "in-process memory; nothing is written to disk and the cache is \
                  empty after a restart",
        eviction: "expired entries first, then least-recently-used, until both the \
                   entry-count and byte bounds hold",
    }
}

fn miss(reason: MissReason, best_similarity: Option<f64>) -> LookupResult {
    LookupResult::Miss {
        reason,
        best_similarity,
    }
}

fn render_lookup(
    result: &LookupResult,
    threshold: f64,
    bucket: String,
    expired_removed: u64,
    now_ms: u64,
) -> LookupResponse {
    match result {
        LookupResult::Hit {
            kind,
            similarity,
            entry,
        } => LookupResponse {
            hit: true,
            match_kind: Some(*kind),
            similarity: Some(*similarity),
            threshold,
            completion: Some(entry.completion.clone()),
            entry: Some(entry_info(entry, now_ms)),
            tokens_saved: Some(token_counts(entry)),
            miss_reason: None,
            best_similarity: Some(*similarity),
            expired_removed,
            bucket,
        },
        LookupResult::Miss {
            reason,
            best_similarity,
        } => LookupResponse {
            hit: false,
            match_kind: None,
            similarity: None,
            threshold,
            completion: None,
            entry: None,
            tokens_saved: None,
            miss_reason: Some(reason.slug().to_string()),
            best_similarity: *best_similarity,
            expired_removed,
            bucket,
        },
    }
}

pub(crate) async fn handle_lookup(
    state: Arc<AppState>,
    args: LookupArgs,
) -> PluginResult<LookupResponse> {
    let tools = args.tools.unwrap_or_default();
    let extra = args.extra_key.unwrap_or_default();
    let shaped = shape(
        key_inputs(
            &args.model,
            embedder_id(&state),
            args.temperature,
            args.top_p,
            &tools,
            &extra,
        ),
        &args.messages,
    )
    .map_err(|error| PluginError::invalid_params(error.to_string()))?;

    let now_ms = state.clock.monotonic_ms();
    let expired_removed = state.store.purge_expired(now_ms);
    let threshold = effective_threshold(state.config.min_similarity, args.min_similarity);

    // A request hotter than the limit is refused before anything else: there
    // is no point embedding a query whose answer may not be reused.
    if temperature_gate(args.temperature, state.config.max_temperature).is_err() {
        let result = miss(MissReason::TemperatureAboveLimit, None);
        state.store.record_lookup(&result, now_ms);
        return Ok(render_lookup(
            &result,
            threshold,
            shaped.bucket,
            expired_removed,
            now_ms,
        ));
    }

    // Identical wording needs no vector at all, and still works when the
    // embeddings endpoint is down.
    if let Some(entry) = state
        .store
        .find_exact(&shaped.bucket, &shaped.query, now_ms)
    {
        let result = LookupResult::Hit {
            kind: MatchKind::Exact,
            similarity: 1.0,
            entry,
        };
        state.store.record_lookup(&result, now_ms);
        return Ok(render_lookup(
            &result,
            threshold,
            shaped.bucket,
            expired_removed,
            now_ms,
        ));
    }

    let embedding = embed(&state, &shaped.query).await?;
    let result = state.store.find_nearest(
        &shaped.bucket,
        shaped.query.len(),
        &embedding,
        threshold,
        state.config.max_length_ratio,
        now_ms,
    );
    state.store.record_lookup(&result, now_ms);
    Ok(render_lookup(
        &result,
        threshold,
        shaped.bucket,
        expired_removed,
        now_ms,
    ))
}

pub(crate) async fn handle_store(
    state: Arc<AppState>,
    args: StoreArgs,
) -> PluginResult<StoreResponse> {
    let tools = args.tools.unwrap_or_default();
    let extra = args.extra_key.unwrap_or_default();
    let shaped = shape(
        key_inputs(
            &args.model,
            embedder_id(&state),
            args.temperature,
            args.top_p,
            &tools,
            &extra,
        ),
        &args.messages,
    )
    .map_err(|error| PluginError::invalid_params(error.to_string()))?;

    let refuse = |rejection: StoreRejection| {
        state.store.record_store_rejection(rejection.slug());
        Ok(StoreResponse {
            stored: false,
            entry_id: None,
            replaced: false,
            expires_in_seconds: None,
            evicted: 0,
            rejected: Some(rejection),
            bucket: shaped.bucket.clone(),
        })
    };

    // Cheap rules first, so an uncacheable response costs no embedding call.
    if let Err(rejection) = store_decision(
        StoreCandidate {
            completion: &args.completion,
            finish_reason: args.finish_reason.as_deref(),
            is_error: args.is_error,
            temperature: args.temperature,
        },
        state.config.max_temperature,
    ) {
        return refuse(rejection);
    }

    let now_ms = state.clock.monotonic_ms();
    let embedding = embed(&state, &shaped.query).await?;

    let candidate = NewEntry {
        bucket: shaped.bucket.clone(),
        model: args.model.clone(),
        query: shaped.query.clone(),
        embedding,
        completion: args.completion,
        prompt_tokens: args.prompt_tokens.unwrap_or(0),
        completion_tokens: args.completion_tokens.unwrap_or(0),
        ttl_seconds: args.ttl_seconds,
        created_wall_ms: state.clock.wall_ms(),
    };

    // The vector's size is only known now, so the byte-budget rule runs here.
    if let Err(rejection) = fits_budget(candidate.approx_bytes(), state.config.max_bytes) {
        return refuse(rejection);
    }

    let InsertOutcome {
        entry_id,
        replaced,
        expires_in_seconds,
        evicted,
    } = state.store.insert(candidate, now_ms);

    Ok(StoreResponse {
        stored: true,
        entry_id: Some(entry_id),
        replaced,
        expires_in_seconds,
        evicted,
        rejected: None,
        bucket: shaped.bucket,
    })
}

/// Embed a query, counting the call and turning a backend failure into a real
/// error.
///
/// A silent miss here would be the wrong kindness: an unreachable embedder and
/// a cold cache look identical from the outside, and the operator would be
/// left believing the cache simply was not helping.
async fn embed(state: &AppState, query: &str) -> PluginResult<Vec<f32>> {
    match state.client.embed(query).await {
        Ok(embedding) => {
            state.store.record_embedding_call(true);
            Ok(embedding)
        }
        Err(error) => {
            state.store.record_embedding_call(false);
            Err(PluginError::internal(format!(
                "semantic-cache could not embed the prompt: {error}. \
                 Check `semantic-cache.status` — the node's own OpenAI frontend on \
                 127.0.0.1:9337 does not serve /v1/embeddings, so this plugin has to be \
                 pointed at an embeddings server with --embeddings-url."
            )))
        }
    }
}

pub(crate) fn handle_stats(state: &AppState) -> StatsResponse {
    StatsResponse {
        snapshot: state.store.snapshot(state.clock.monotonic_ms()),
        config: effective_config(state),
        notes: [
            "tokens_saved_total sums the token counts the caller reported when each \
             entry was stored, so it is exact for an exact hit and an estimate for a \
             reworded one.",
            "approx_bytes estimates the payload held (prompt, completion, vector, \
             per-entry overhead). It is not process memory.",
        ],
    }
}

pub(crate) async fn handle_status(state: Arc<AppState>) -> StatusResponse {
    // `status` reports rather than fails: an unreachable endpoint is the
    // answer to the question being asked, not an error in answering it. It
    // does cost one real embedding call, which is counted in `stats`.
    let started = std::time::Instant::now();
    let embeddings = match state.client.embed(PROBE_TEXT).await {
        Ok(vector) => {
            state.store.record_embedding_call(true);
            EndpointProbe::Reachable {
                reachable: true,
                dimensions: vector.len(),
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
        Err(error) => {
            state.store.record_embedding_call(false);
            EndpointProbe::Unreachable {
                reachable: false,
                error: error.message,
            }
        }
    };

    StatusResponse {
        plugin: PLUGIN_NAME,
        version: PLUGIN_VERSION,
        embeddings,
        entries: state.store.len(),
        config: effective_config(&state),
    }
}

pub(crate) fn handle_purge(state: &AppState, args: PurgeArgs) -> PluginResult<PurgeResponse> {
    let scope = match args.scope {
        PurgeSelector::Expired => PurgeScope::Expired,
        PurgeSelector::All => PurgeScope::All,
        PurgeSelector::Model => {
            let model = args.model.unwrap_or_default();
            if model.trim().is_empty() {
                return Err(PluginError::invalid_params(
                    "purge scope \"model\" requires a non-empty `model`",
                ));
            }
            PurgeScope::Model(model)
        }
    };
    let removed = state.store.purge(scope, state.clock.monotonic_ms());
    Ok(PurgeResponse {
        removed,
        remaining: state.store.len(),
    })
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

pub const PLUGIN_NAME: &str = "semantic-cache";
pub const PLUGIN_VERSION: &str = "0.1.0";

pub fn semantic_cache_plugin(state: Arc<AppState>) -> SimplePlugin {
    let for_lookup = Arc::clone(&state);
    let for_store = Arc::clone(&state);
    let for_stats = Arc::clone(&state);
    let for_status = Arc::clone(&state);
    let for_purge = Arc::clone(&state);
    let for_http_lookup = Arc::clone(&state);
    let for_http_store = Arc::clone(&state);
    let for_http_stats = Arc::clone(&state);
    let for_health = Arc::clone(&state);

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Semantic cache",
                "Caches completions by meaning, so a reworded but equivalent prompt \
                 reuses the answer instead of paying for it again",
                None::<String>,
            ),
        ),

        provides: [capability("semantic-cache.v1")],

        mcp: [
            mcp::tool("lookup")
                .description(
                    "Look for a cached answer to a prompt. Matches on meaning, not on \
                     exact text, but only within an identical request shape: the model, \
                     the sampling parameters, the tool set and the whole message prefix \
                     (including the system prompt) must all match exactly. Returns \
                     hit=false with best_similarity when nothing was close enough. \
                     Errors if the embeddings backend cannot be reached, so an outage \
                     is never mistaken for a cold cache.",
                )
                .input::<LookupArgs>()
                .handle(move |args: LookupArgs, _context| {
                    let state = Arc::clone(&for_lookup);
                    Box::pin(async move { handle_lookup(state, args).await })
                }),

            mcp::tool("store")
                .description(
                    "Record a completion so later equivalent prompts can reuse it. \
                     Refuses errored responses, empty answers, anything that did not \
                     finish with finish_reason=stop, and requests hotter than the \
                     configured temperature limit. Entries expire on a TTL and are \
                     evicted when the cache is full.",
                )
                .input::<StoreArgs>()
                .handle(move |args: StoreArgs, _context| {
                    let state = Arc::clone(&for_store);
                    Box::pin(async move { handle_store(state, args).await })
                }),

            mcp::tool("stats")
                .description(
                    "Report cache effectiveness: hit rate, hits split into exact and \
                     semantic, misses by reason, tokens saved, entries held, estimated \
                     bytes, evictions, and the configuration in force. This is the \
                     measurement — no network calls, no side effects.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let state = Arc::clone(&for_stats);
                    Box::pin(async move { Ok(handle_stats(&state)) })
                }),

            mcp::tool("status")
                .description(
                    "Check whether the embeddings backend actually works, and show the \
                     effective configuration. Sends one short probe string to the \
                     configured endpoint, so it costs one embedding call. Reports an \
                     unreachable backend as a result rather than failing.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let state = Arc::clone(&for_status);
                    Box::pin(async move { Ok(handle_status(state).await) })
                }),

            mcp::tool("purge")
                .description(
                    "Drop cached entries. Scope is required: `expired` removes only \
                     entries past their TTL, `model` removes every entry for one \
                     completion model, `all` empties the cache.",
                )
                .input::<PurgeArgs>()
                .handle(move |args: PurgeArgs, _context| {
                    let state = Arc::clone(&for_purge);
                    Box::pin(async move { handle_purge(&state, args) })
                }),
        ],

        // The same three read/write operations over HTTP, mounted by the host
        // at /api/plugins/semantic-cache/http/…. A proxy in front of :9337 is
        // the natural place to use this cache, and a proxy speaks HTTP.
        http: [
            http::post("/lookup")
                .description("Look for a cached answer to a prompt.")
                .input::<LookupArgs>()
                .handle(move |args: LookupArgs, _context| {
                    let state = Arc::clone(&for_http_lookup);
                    Box::pin(async move { handle_lookup(state, args).await })
                }),

            http::post("/store")
                .description("Record a completion for later reuse.")
                .input::<StoreArgs>()
                .handle(move |args: StoreArgs, _context| {
                    let state = Arc::clone(&for_http_store);
                    Box::pin(async move { handle_store(state, args).await })
                }),

            http::get("/stats")
                .description("Hit rate, tokens saved, and the configuration in force.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let state = Arc::clone(&for_http_stats);
                    Box::pin(async move { Ok(handle_stats(&state)) })
                }),
        ],

        // Health must stay fast and must not depend on long-running work, so
        // it reports local state only. Embeddings-endpoint reachability is a
        // separate concern with its own tool: the plugin is perfectly healthy
        // while its backend is restarting.
        health: move |_context| {
            let state = Arc::clone(&for_health);
            Box::pin(async move { Ok(format!("ok; entries={}", state.store.len())) })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    // Every test that builds an `AppState` runs on a Tokio runtime, because
    // building the HTTP client is the sort of thing that expects a reactor to
    // exist. None of them reach the network unless the test says so.
    fn state() -> Arc<AppState> {
        let config = Config {
            embedding_model: "test-embedder".to_string(),
            ..Config::default()
        };
        Arc::new(AppState::new(config).expect("state builds"))
    }

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn lookup_args(model: &str, prompt: &str) -> LookupArgs {
        LookupArgs {
            model: model.to_string(),
            messages: vec![message("user", prompt)],
            temperature: Some(0.0),
            top_p: None,
            tools: None,
            extra_key: None,
            min_similarity: None,
        }
    }

    fn store_args(model: &str, prompt: &str, completion: &str) -> StoreArgs {
        StoreArgs {
            model: model.to_string(),
            messages: vec![message("user", prompt)],
            completion: completion.to_string(),
            temperature: Some(0.0),
            top_p: None,
            tools: None,
            extra_key: None,
            finish_reason: None,
            is_error: false,
            prompt_tokens: Some(12),
            completion_tokens: Some(34),
            ttl_seconds: None,
        }
    }

    #[tokio::test]
    async fn the_manifest_declares_the_documented_surface() {
        let plugin = semantic_cache_plugin(state());
        let manifest = plugin
            .manifest()
            .expect("declarative plugins have a manifest");

        let mut operations: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        operations.sort_unstable();
        for expected in ["lookup", "store", "stats", "status", "purge"] {
            assert!(
                operations.contains(&expected),
                "missing tool {expected}: {operations:?}"
            );
        }

        let mut paths: Vec<&str> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["/lookup", "/stats", "/store"]);

        assert!(
            manifest
                .capabilities
                .iter()
                .any(|entry| entry == "semantic-cache.v1")
        );
        assert!(
            manifest.config_schema.is_none(),
            "declaring settings this process cannot read would mislead the operator"
        );
        assert!(manifest.web_ui.is_none());
    }

    #[tokio::test]
    async fn a_hot_request_is_refused_without_touching_the_backend() {
        // The default endpoint is not reachable in a test environment, so any
        // path that reached the network would surface as an error rather than
        // the miss asserted here.
        let state = state();
        let mut args = lookup_args("m", "pick a random number");
        args.temperature = Some(1.9);

        let response = handle_lookup(Arc::clone(&state), args)
            .await
            .expect("no network needed");
        assert!(!response.hit);
        assert_eq!(
            response.miss_reason.as_deref(),
            Some(MissReason::TemperatureAboveLimit.slug())
        );
        assert_eq!(state.store.snapshot(0).counters.embedding_calls, 0);
    }

    #[tokio::test]
    async fn an_uncacheable_response_is_refused_before_any_embedding_call() {
        let state = state();
        for mutate in [
            (|args: &mut StoreArgs| args.is_error = true) as fn(&mut StoreArgs),
            |args: &mut StoreArgs| args.completion = "  ".to_string(),
            |args: &mut StoreArgs| args.finish_reason = Some("length".to_string()),
            |args: &mut StoreArgs| args.temperature = Some(1.9),
        ] {
            let mut args = store_args("m", "what is TLS?", "a long answer");
            mutate(&mut args);
            let response = handle_store(Arc::clone(&state), args)
                .await
                .expect("a refusal is a result, not an error");
            assert!(!response.stored);
            assert!(response.rejected.is_some());
        }
        assert_eq!(
            state.store.snapshot(0).counters.embedding_calls,
            0,
            "the rules that need no vector must run first"
        );
        assert_eq!(
            state
                .store
                .snapshot(0)
                .counters
                .stores_rejected_by_reason
                .len(),
            4,
            "each refusal is counted under its own reason"
        );
    }

    #[tokio::test]
    async fn a_malformed_conversation_is_a_parameter_error() {
        let state = state();
        let mut args = lookup_args("m", "hello");
        args.messages = vec![message("user", "hi"), message("assistant", "hello")];
        let error = handle_lookup(state, args)
            .await
            .expect_err("must be rejected");
        assert!(error.message.contains("user"), "{}", error.message);
    }

    #[tokio::test]
    async fn an_unreachable_backend_errors_instead_of_reporting_a_miss() {
        // Port 1 on loopback has nothing listening, which is exactly the
        // "backend is down" case.
        let config = Config {
            embeddings_url: crate::config::resolve_embeddings_url("http://127.0.0.1:1/v1")
                .expect("valid URL"),
            embedding_model: "test-embedder".to_string(),
            request_timeout: std::time::Duration::from_secs(2),
            ..Config::default()
        };
        let state = Arc::new(AppState::new(config).expect("state builds"));

        let error = handle_lookup(Arc::clone(&state), lookup_args("m", "what is TLS?"))
            .await
            .expect_err("a silent miss would hide the outage");
        assert!(
            error.message.contains("could not embed"),
            "{}",
            error.message
        );
        assert_eq!(state.store.snapshot(0).counters.embedding_failures, 1);
    }

    #[tokio::test]
    async fn status_reports_an_unreachable_backend_rather_than_failing() {
        let config = Config {
            embeddings_url: crate::config::resolve_embeddings_url("http://127.0.0.1:1/v1")
                .expect("valid URL"),
            request_timeout: std::time::Duration::from_secs(2),
            ..Config::default()
        };
        let state = Arc::new(AppState::new(config).expect("state builds"));

        let response = handle_status(state).await;
        let EndpointProbe::Unreachable { reachable, error } = response.embeddings else {
            panic!("nothing is listening on port 1");
        };
        assert!(!reachable);
        assert!(error.contains("unreachable"), "{error}");
    }

    #[tokio::test]
    async fn purge_by_model_requires_a_model() {
        let state = state();
        let error = handle_purge(
            &state,
            PurgeArgs {
                scope: PurgeSelector::Model,
                model: None,
            },
        )
        .expect_err("an unscoped model purge is ambiguous");
        assert!(error.message.contains("model"), "{}", error.message);
    }

    #[tokio::test]
    async fn purge_expired_is_always_available_and_reports_what_it_did() {
        let state = state();
        let response = handle_purge(
            &state,
            PurgeArgs {
                scope: PurgeSelector::Expired,
                model: None,
            },
        )
        .expect("expired purges never need an argument");
        assert_eq!(response.removed, 0);
        assert_eq!(response.remaining, 0);
    }

    #[tokio::test]
    async fn stats_never_leak_the_api_key_and_always_carry_their_caveats() {
        let environment = [(
            crate::config::API_KEY_ENV.to_string(),
            "sk-secret".to_string(),
        )]
        .into_iter()
        .collect();
        let config = Config::resolve(&Vec::new(), &environment).expect("valid config");
        assert!(
            config.api_key.is_some(),
            "the fixture needs a key to try to leak"
        );
        let state = Arc::new(AppState::new(config).expect("state builds"));

        let rendered = serde_json::to_string(&handle_stats(&state)).expect("serializes");
        assert!(
            !rendered.contains("sk-secret"),
            "the key must never reach a tool result"
        );
        assert!(rendered.contains("\"api_key_configured\":true"));
        assert!(rendered.contains("tokens_saved_total"));
        assert!(rendered.contains("not process memory"));
        assert!(rendered.contains("in-process memory"));
    }

    #[tokio::test]
    async fn the_reported_configuration_matches_what_the_cache_will_actually_do() {
        // `effective_config` reads the bounds back out of the store, so this
        // is really asserting that the launch config reached the component
        // that enforces it.
        let state = state();
        let stats = handle_stats(&state);
        assert_eq!(stats.config.min_similarity, state.config.min_similarity);
        assert_eq!(stats.config.max_entries, state.config.max_entries);
        assert_eq!(stats.config.max_bytes, state.config.max_bytes);
        assert_eq!(stats.config.ttl_seconds, state.config.ttl_seconds);
        assert_eq!(
            stats.config.embeddings_endpoint,
            state.config.embeddings_url.to_string()
        );
        assert!(stats.config.endpoint_is_loopback);
        assert!(!stats.config.allow_remote_embeddings);
        assert!(!stats.config.api_key_configured);
    }
}
