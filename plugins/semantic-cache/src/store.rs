//! The cache itself: a bounded, in-process store with TTL + LRU eviction.
//!
//! # Where the data lives, and why
//!
//! **In memory only. Nothing is written to disk, ever.**
//!
//! That is a deliberate choice for software that runs on hardware other people
//! contributed to a mesh. Every entry holds a user's prompt, the model's
//! answer, and an embedding of the prompt — which is to say, the most
//! sensitive text passing through the node. Persisting that would leave a
//! searchable transcript on a contributor's disk that survives the process
//! they can see in their process list, and it would quietly outlive the
//! "restart clears it" mental model every operator already has. Losing the
//! cache on restart costs one cold period. The alternative costs somebody
//! else's privacy.
//!
//! # Bounds and eviction
//!
//! Two independent bounds, both enforced on every insert:
//!
//! - `max_entries` — predictable row count.
//! - `max_bytes` — an estimate of the payload bytes held (prompt, completion,
//!   vector, per-entry overhead). One 30 MiB completion must not be allowed to
//!   occupy the cache just because it is a single row.
//!
//! Eviction order is **expired first, then least-recently-used**. LRU rather
//! than LFU because a cache like this earns its keep on a hot working set of
//! repeatedly-asked questions, and LFU keeps a once-popular entry resident
//! long after the topic has moved on. Finding the LRU victim is an O(n) scan
//! over at most `max_entries` records — at the default 1000 that is
//! microseconds, and it avoids maintaining an intrusive linked list for no
//! measurable gain.
//!
//! # Time
//!
//! Every method takes `now_ms` as an argument instead of reading the clock.
//! Callers pass a **monotonic** millisecond counter (see
//! [`crate::store::Clock`]), so a wall-clock jump from NTP cannot resurrect an
//! expired entry, and tests can control time exactly.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::policy::{cosine_similarity, length_ratio_ok};

/// Fixed per-entry cost that is not captured by the string and vector lengths:
/// the id, timestamps, counters, the map node, and the `Vec`/`String` headers.
/// An estimate, and labelled as one everywhere it surfaces.
const ENTRY_OVERHEAD_BYTES: u64 = 192;

/// Monotonic milliseconds since process start, plus the wall clock for display.
///
/// TTL and LRU both run off the monotonic reading. `wall_ms` exists only so a
/// human reading `lookup` output can see when an entry was actually created.
pub struct Clock {
    started: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    pub fn monotonic_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn wall_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_entries: usize,
    pub max_bytes: u64,
    /// Zero means "never expire".
    pub ttl_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub id: u64,
    pub bucket: String,
    /// Completion model id, kept for per-model stats and targeted purges. It
    /// is already inside the bucket digest, which is not reversible.
    pub model: String,
    /// Normalized trailing user message.
    pub query: String,
    /// L2-normalized, so similarity is a dot product.
    pub embedding: Vec<f32>,
    pub completion: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub created_ms: u64,
    pub created_wall_ms: u64,
    /// `None` when the TTL is disabled for this entry.
    pub expires_ms: Option<u64>,
    pub last_used_ms: u64,
    pub hits: u64,
}

/// Estimated payload bytes for an entry. Not process RSS, and never presented
/// as such.
///
/// Shared by [`CacheEntry`] and [`NewEntry`] so the size a candidate is
/// *checked* against the budget is exactly the size it will *occupy*.
fn approx_bytes_of(
    bucket: &str,
    model: &str,
    query: &str,
    completion: &str,
    embedding_len: usize,
) -> u64 {
    ENTRY_OVERHEAD_BYTES
        + bucket.len() as u64
        + model.len() as u64
        + query.len() as u64
        + completion.len() as u64
        + (embedding_len as u64) * 4
}

impl CacheEntry {
    pub fn approx_bytes(&self) -> u64 {
        approx_bytes_of(
            &self.bucket,
            &self.model,
            &self.query,
            &self.completion,
            self.embedding.len(),
        )
    }

    fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_ms.is_some_and(|expires| now_ms >= expires)
    }

    pub fn age_seconds(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.created_ms) / 1_000
    }
}

/// A completion the caller has cleared for storage. Policy checks (see
/// [`crate::policy::store_decision`]) have already passed by this point.
#[derive(Clone, Debug)]
pub struct NewEntry {
    pub bucket: String,
    pub model: String,
    pub query: String,
    pub embedding: Vec<f32>,
    pub completion: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Per-call override of the configured TTL. `Some(0)` disables expiry for
    /// this entry; `None` uses the configured value.
    pub ttl_seconds: Option<u64>,
    pub created_wall_ms: u64,
}

impl NewEntry {
    /// What this candidate will cost the byte budget once admitted.
    pub fn approx_bytes(&self) -> u64 {
        approx_bytes_of(
            &self.bucket,
            &self.model,
            &self.query,
            &self.completion,
            self.embedding.len(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Byte-identical query text after whitespace normalization. Costs no
    /// embedding call at all.
    Exact,
    /// A different wording that scored above the threshold.
    Semantic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissReason {
    /// Nothing has ever been stored for this exact request shape (or
    /// everything that was has since expired).
    BucketEmpty,
    /// The nearest neighbour was not near enough.
    BelowThreshold,
    /// The nearest neighbour scored high enough but is far longer or shorter
    /// than the query, which usually means shared topic rather than shared
    /// meaning.
    LengthGuard,
    /// The request asked for more sampling variance than may be served from a
    /// frozen entry.
    TemperatureAboveLimit,
}

impl MissReason {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::BucketEmpty => "bucket_empty",
            Self::BelowThreshold => "below_threshold",
            Self::LengthGuard => "length_guard",
            Self::TemperatureAboveLimit => "temperature_above_limit",
        }
    }
}

#[derive(Clone, Debug)]
pub enum LookupResult {
    Hit {
        kind: MatchKind,
        similarity: f64,
        entry: CacheEntry,
    },
    Miss {
        reason: MissReason,
        /// Best score seen in the bucket, even when it lost. Present so an
        /// operator can tune `--min-similarity` against real traffic instead
        /// of guessing.
        best_similarity: Option<f64>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct InsertOutcome {
    pub entry_id: u64,
    /// True when this replaced an existing entry with the same query in the
    /// same bucket, rather than adding a row.
    pub replaced: bool,
    pub expires_in_seconds: Option<u64>,
    pub evicted: u64,
}

#[derive(Clone, Debug)]
pub enum PurgeScope {
    Expired,
    Model(String),
    All,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Counters {
    pub lookups: u64,
    pub hits_exact: u64,
    pub hits_semantic: u64,
    pub misses_by_reason: BTreeMap<String, u64>,
    pub stores_accepted: u64,
    pub stores_replaced: u64,
    pub stores_rejected_by_reason: BTreeMap<String, u64>,
    pub evictions_expired: u64,
    pub evictions_capacity: u64,
    pub evictions_bytes: u64,
    pub purged_manually: u64,
    pub embedding_calls: u64,
    pub embedding_failures: u64,
    /// Summed from the token counts recorded **when each entry was stored**.
    /// See the note on [`StatsSnapshot::tokens_saved_total`].
    pub prompt_tokens_saved: u64,
    pub completion_tokens_saved: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub counters: Counters,
    pub hits: u64,
    pub misses: u64,
    /// `hits / lookups`, or 0.0 before the first lookup.
    pub hit_rate: f64,
    /// Prompt + completion tokens that did not have to be sent to or produced
    /// by a model, summed over every hit.
    ///
    /// This is an accounting of the *stored* entry's token counts as the
    /// caller reported them at store time, not a measurement of the request
    /// that hit. For an exact hit the two are the same. For a semantic hit the
    /// reworded prompt may tokenize to a slightly different length, so treat
    /// the prompt half as an estimate. It is never larger than what a real
    /// call would have cost unless the caller reported inflated numbers.
    pub tokens_saved_total: u64,
    pub entries: usize,
    pub buckets: usize,
    /// Estimated payload bytes held, not process memory.
    pub approx_bytes: u64,
    pub entries_by_model: BTreeMap<String, usize>,
    pub oldest_entry_age_seconds: Option<u64>,
}

struct Inner {
    entries: BTreeMap<u64, CacheEntry>,
    next_id: u64,
    bytes: u64,
    counters: Counters,
}

pub struct CacheStore {
    limits: Limits,
    inner: Mutex<Inner>,
}

impl CacheStore {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner {
                entries: BTreeMap::new(),
                next_id: 1,
                bytes: 0,
                counters: Counters::default(),
            }),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// A poisoned lock means a handler panicked mid-update. Every field here
    /// is a counter or an independent row, with no cross-field invariant to
    /// repair, so recovering the data keeps the node serving instead of
    /// failing every later request.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Drop every expired entry. Called at the head of each lookup and store,
    /// so an expired answer is never a candidate even if nothing else has
    /// touched the cache since it aged out.
    pub fn purge_expired(&self, now_ms: u64) -> u64 {
        let mut inner = self.lock();
        let expired: Vec<u64> = inner
            .entries
            .values()
            .filter(|entry| entry.is_expired(now_ms))
            .map(|entry| entry.id)
            .collect();
        let count = expired.len() as u64;
        for id in expired {
            remove(&mut inner, id);
        }
        inner.counters.evictions_expired += count;
        count
    }

    /// Exact (post-normalization) query match within a bucket.
    ///
    /// This runs before any embedding call, so a repeated prompt costs nothing
    /// but a map scan.
    pub fn find_exact(&self, bucket: &str, query: &str, now_ms: u64) -> Option<CacheEntry> {
        let inner = self.lock();
        inner
            .entries
            .values()
            .find(|entry| {
                entry.bucket == bucket && entry.query == query && !entry.is_expired(now_ms)
            })
            .cloned()
    }

    /// Nearest neighbour within a bucket.
    ///
    /// The highest-scoring entry wins or nothing does: if the best match clears
    /// the threshold but fails the length guard, the lookup reports a miss
    /// rather than falling through to a lower-scoring entry. Falling through
    /// would mean a *less* similar answer got served because a more similar one
    /// was rejected as unsafe, which is exactly backwards.
    pub fn find_nearest(
        &self,
        bucket: &str,
        query_len: usize,
        embedding: &[f32],
        threshold: f64,
        max_length_ratio: f64,
        now_ms: u64,
    ) -> LookupResult {
        let inner = self.lock();
        let mut best: Option<(f64, &CacheEntry)> = None;
        for entry in inner.entries.values() {
            if entry.bucket != bucket || entry.is_expired(now_ms) {
                continue;
            }
            // `None` means the stored vector has a different dimension, which
            // means a different embedder produced it. Skip rather than compare.
            let Some(similarity) = cosine_similarity(embedding, &entry.embedding) else {
                continue;
            };
            if best.is_none_or(|(best_score, _)| similarity > best_score) {
                best = Some((similarity, entry));
            }
        }

        let Some((similarity, entry)) = best else {
            return LookupResult::Miss {
                reason: MissReason::BucketEmpty,
                best_similarity: None,
            };
        };
        if similarity < threshold {
            return LookupResult::Miss {
                reason: MissReason::BelowThreshold,
                best_similarity: Some(similarity),
            };
        }
        if !length_ratio_ok(query_len, entry.query.len(), max_length_ratio) {
            return LookupResult::Miss {
                reason: MissReason::LengthGuard,
                best_similarity: Some(similarity),
            };
        }
        LookupResult::Hit {
            kind: MatchKind::Semantic,
            similarity,
            entry: entry.clone(),
        }
    }

    /// Record exactly one lookup outcome.
    ///
    /// Callers build a [`LookupResult`], hand it here, and then render it —
    /// so the counters and the response can never disagree about what
    /// happened.
    pub fn record_lookup(&self, result: &LookupResult, now_ms: u64) {
        let mut inner = self.lock();
        inner.counters.lookups += 1;
        match result {
            LookupResult::Hit { kind, entry, .. } => {
                match kind {
                    MatchKind::Exact => inner.counters.hits_exact += 1,
                    MatchKind::Semantic => inner.counters.hits_semantic += 1,
                }
                inner.counters.prompt_tokens_saved += entry.prompt_tokens;
                inner.counters.completion_tokens_saved += entry.completion_tokens;
                if let Some(live) = inner.entries.get_mut(&entry.id) {
                    live.hits += 1;
                    live.last_used_ms = now_ms;
                }
            }
            LookupResult::Miss { reason, .. } => {
                *inner
                    .counters
                    .misses_by_reason
                    .entry(reason.slug().to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn record_embedding_call(&self, succeeded: bool) {
        let mut inner = self.lock();
        inner.counters.embedding_calls += 1;
        if !succeeded {
            inner.counters.embedding_failures += 1;
        }
    }

    pub fn record_store_rejection(&self, slug: &str) {
        let mut inner = self.lock();
        *inner
            .counters
            .stores_rejected_by_reason
            .entry(slug.to_string())
            .or_insert(0) += 1;
    }

    /// Insert or refresh an entry, then enforce both bounds.
    ///
    /// Storing the same query in the same bucket twice replaces the first
    /// entry rather than adding a row: a re-stored answer is a fresher answer,
    /// and duplicate rows would let one hot prompt evict the rest of the cache.
    pub fn insert(&self, candidate: NewEntry, now_ms: u64) -> InsertOutcome {
        self.purge_expired(now_ms);
        let mut inner = self.lock();

        let ttl_seconds = candidate.ttl_seconds.unwrap_or(self.limits.ttl_seconds);
        let expires_ms = (ttl_seconds > 0).then(|| now_ms.saturating_add(ttl_seconds * 1_000));

        let existing = inner
            .entries
            .values()
            .find(|entry| entry.bucket == candidate.bucket && entry.query == candidate.query)
            .map(|entry| entry.id);
        let replaced = existing.is_some();
        if let Some(id) = existing {
            remove(&mut inner, id);
        }

        let id = inner.next_id;
        inner.next_id += 1;
        let entry = CacheEntry {
            id,
            bucket: candidate.bucket,
            model: candidate.model,
            query: candidate.query,
            embedding: candidate.embedding,
            completion: candidate.completion,
            prompt_tokens: candidate.prompt_tokens,
            completion_tokens: candidate.completion_tokens,
            created_ms: now_ms,
            created_wall_ms: candidate.created_wall_ms,
            expires_ms,
            last_used_ms: now_ms,
            hits: 0,
        };
        inner.bytes += entry.approx_bytes();
        inner.entries.insert(id, entry);
        if replaced {
            inner.counters.stores_replaced += 1;
        }
        inner.counters.stores_accepted += 1;

        let evicted = evict_to_fit(&mut inner, self.limits, id);

        InsertOutcome {
            entry_id: id,
            replaced,
            expires_in_seconds: (ttl_seconds > 0).then_some(ttl_seconds),
            evicted,
        }
    }

    pub fn purge(&self, scope: PurgeScope, now_ms: u64) -> u64 {
        if let PurgeScope::Expired = scope {
            return self.purge_expired(now_ms);
        }
        let mut inner = self.lock();
        let doomed: Vec<u64> = inner
            .entries
            .values()
            .filter(|entry| match &scope {
                PurgeScope::All => true,
                PurgeScope::Model(model) => &entry.model == model,
                PurgeScope::Expired => unreachable!("handled above"),
            })
            .map(|entry| entry.id)
            .collect();
        let count = doomed.len() as u64;
        for id in doomed {
            remove(&mut inner, id);
        }
        inner.counters.purged_manually += count;
        count
    }

    pub fn snapshot(&self, now_ms: u64) -> StatsSnapshot {
        self.purge_expired(now_ms);
        let inner = self.lock();
        let counters = inner.counters.clone();
        let hits = counters.hits_exact + counters.hits_semantic;
        let misses: u64 = counters.misses_by_reason.values().sum();
        let hit_rate = if counters.lookups == 0 {
            0.0
        } else {
            hits as f64 / counters.lookups as f64
        };

        let mut entries_by_model: BTreeMap<String, usize> = BTreeMap::new();
        let mut buckets: Vec<&str> = Vec::with_capacity(inner.entries.len());
        let mut oldest: Option<u64> = None;
        for entry in inner.entries.values() {
            *entries_by_model.entry(entry.model.clone()).or_insert(0) += 1;
            buckets.push(entry.bucket.as_str());
            let age = entry.age_seconds(now_ms);
            oldest = Some(oldest.map_or(age, |current: u64| current.max(age)));
        }
        buckets.sort_unstable();
        buckets.dedup();

        StatsSnapshot {
            hits,
            misses,
            hit_rate,
            tokens_saved_total: counters.prompt_tokens_saved + counters.completion_tokens_saved,
            entries: inner.entries.len(),
            buckets: buckets.len(),
            approx_bytes: inner.bytes,
            entries_by_model,
            oldest_entry_age_seconds: oldest,
            counters,
        }
    }
}

fn remove(inner: &mut Inner, id: u64) {
    if let Some(entry) = inner.entries.remove(&id) {
        inner.bytes = inner.bytes.saturating_sub(entry.approx_bytes());
    }
}

/// Evict least-recently-used entries until both bounds hold.
///
/// `protect` is the row just inserted: without it, storing a single entry
/// larger than the byte budget would evict everything and then evict itself,
/// reporting a successful store that left nothing behind. Oversized entries
/// are refused up front by `policy::store_decision`, and this is the belt to
/// that braces.
fn evict_to_fit(inner: &mut Inner, limits: Limits, protect: u64) -> u64 {
    let mut evicted = 0;
    loop {
        let over_count = inner.entries.len() > limits.max_entries;
        let over_bytes = inner.bytes > limits.max_bytes;
        if !over_count && !over_bytes {
            break;
        }
        let victim = inner
            .entries
            .values()
            .filter(|entry| entry.id != protect)
            // Ties broken by id so eviction order is deterministic — two
            // entries touched in the same millisecond must not evict at random.
            .min_by_key(|entry| (entry.last_used_ms, entry.id))
            .map(|entry| entry.id);
        let Some(victim) = victim else {
            break;
        };
        remove(inner, victim);
        if over_count {
            inner.counters.evictions_capacity += 1;
        } else {
            inner.counters.evictions_bytes += 1;
        }
        evicted += 1;
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::normalize_l2;

    fn limits(max_entries: usize, max_bytes: u64, ttl_seconds: u64) -> Limits {
        Limits {
            max_entries,
            max_bytes,
            ttl_seconds,
        }
    }

    fn vector(values: &[f32]) -> Vec<f32> {
        normalize_l2(values.to_vec()).expect("test vectors have a direction")
    }

    fn entry(bucket: &str, query: &str, values: &[f32]) -> NewEntry {
        NewEntry {
            bucket: bucket.to_string(),
            model: "test-model".to_string(),
            query: query.to_string(),
            embedding: vector(values),
            completion: format!("answer to {query}"),
            prompt_tokens: 10,
            completion_tokens: 20,
            ttl_seconds: None,
            created_wall_ms: 1_700_000_000_000,
        }
    }

    fn store(ttl_seconds: u64) -> CacheStore {
        CacheStore::new(limits(100, 10_000_000, ttl_seconds))
    }

    #[test]
    fn an_exact_query_hits_without_any_vector_work() {
        let store = store(60);
        store.insert(entry("b1", "what is tls?", &[1.0, 0.0]), 0);
        let hit = store
            .find_exact("b1", "what is tls?", 1_000)
            .expect("exact hit");
        assert_eq!(hit.completion, "answer to what is tls?");
        assert!(store.find_exact("b1", "what is ssl?", 1_000).is_none());
    }

    #[test]
    fn an_exact_match_in_another_bucket_is_not_a_hit() {
        let store = store(60);
        store.insert(entry("b1", "what is tls?", &[1.0, 0.0]), 0);
        assert!(
            store.find_exact("b2", "what is tls?", 0).is_none(),
            "buckets are the whole safety story; they must not leak"
        );
    }

    #[test]
    fn a_close_neighbour_hits_and_a_distant_one_does_not() {
        let store = store(60);
        store.insert(entry("b1", "how do I reset my password?", &[1.0, 0.02]), 0);

        let near = vector(&[1.0, 0.0]);
        let result = store.find_nearest("b1", 27, &near, 0.95, 2.0, 0);
        let LookupResult::Hit {
            kind, similarity, ..
        } = result
        else {
            panic!("a near-parallel vector must hit: {result:?}");
        };
        assert_eq!(kind, MatchKind::Semantic);
        assert!(similarity > 0.99, "{similarity}");

        let far = vector(&[0.0, 1.0]);
        let result = store.find_nearest("b1", 27, &far, 0.95, 2.0, 0);
        let LookupResult::Miss {
            reason,
            best_similarity,
        } = result
        else {
            panic!("an orthogonal vector must miss");
        };
        assert_eq!(reason, MissReason::BelowThreshold);
        assert!(best_similarity.expect("a score is reported for tuning") < 0.1);
    }

    #[test]
    fn an_empty_bucket_reports_why_rather_than_a_score() {
        let store = store(60);
        let result = store.find_nearest("nothing-here", 10, &vector(&[1.0, 0.0]), 0.95, 2.0, 0);
        let LookupResult::Miss {
            reason,
            best_similarity,
        } = result
        else {
            panic!("nothing stored means nothing to hit");
        };
        assert_eq!(reason, MissReason::BucketEmpty);
        assert_eq!(best_similarity, None);
    }

    #[test]
    fn the_length_guard_blocks_a_high_scoring_but_mismatched_neighbour() {
        let store = store(60);
        let long = "a".repeat(400);
        store.insert(entry("b1", &long, &[1.0, 0.0]), 0);

        let result = store.find_nearest("b1", 5, &vector(&[1.0, 0.0]), 0.95, 2.0, 0);
        let LookupResult::Miss {
            reason,
            best_similarity,
        } = result
        else {
            panic!("a 5-character query must not be answered by a 400-character entry");
        };
        assert_eq!(reason, MissReason::LengthGuard);
        assert!(best_similarity.expect("the score is still reported") > 0.99);
    }

    #[test]
    fn a_vector_of_another_dimension_is_skipped_not_compared() {
        let store = store(60);
        store.insert(entry("b1", "hello", &[1.0, 0.0]), 0);
        let three_dimensional = vector(&[1.0, 0.0, 0.0]);
        let result = store.find_nearest("b1", 5, &three_dimensional, 0.95, 2.0, 0);
        assert!(
            matches!(
                result,
                LookupResult::Miss {
                    reason: MissReason::BucketEmpty,
                    ..
                }
            ),
            "entries from a different embedder are not candidates: {result:?}"
        );
    }

    #[test]
    fn entries_expire_and_stop_being_candidates() {
        let store = store(60);
        store.insert(entry("b1", "what is deployed?", &[1.0, 0.0]), 0);
        assert!(
            store
                .find_exact("b1", "what is deployed?", 59_000)
                .is_some()
        );
        assert!(
            store
                .find_exact("b1", "what is deployed?", 60_000)
                .is_none(),
            "a question about current state must not be answered from last week"
        );

        assert_eq!(store.purge_expired(60_000), 1);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_zero_ttl_disables_expiry_for_that_entry() {
        let store = store(60);
        let mut forever = entry("b1", "what is 2+2?", &[1.0, 0.0]);
        forever.ttl_seconds = Some(0);
        let outcome = store.insert(forever, 0);
        assert_eq!(outcome.expires_in_seconds, None);
        assert!(
            store
                .find_exact("b1", "what is 2+2?", 10_000_000_000)
                .is_some()
        );
    }

    #[test]
    fn re_storing_the_same_query_refreshes_rather_than_duplicating() {
        let store = store(60);
        store.insert(entry("b1", "hello", &[1.0, 0.0]), 0);
        let mut fresher = entry("b1", "hello", &[1.0, 0.0]);
        fresher.completion = "a better answer".to_string();
        let outcome = store.insert(fresher, 1_000);

        assert!(outcome.replaced);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .find_exact("b1", "hello", 1_000)
                .expect("still there")
                .completion,
            "a better answer"
        );
    }

    #[test]
    fn the_entry_bound_evicts_the_least_recently_used_row() {
        let store = CacheStore::new(limits(2, 10_000_000, 0));
        store.insert(entry("b1", "first", &[1.0, 0.0]), 0);
        store.insert(entry("b1", "second", &[0.0, 1.0]), 1_000);

        // Touch "first" so "second" becomes the coldest row.
        let hit = store.find_exact("b1", "first", 2_000).expect("present");
        store.record_lookup(
            &LookupResult::Hit {
                kind: MatchKind::Exact,
                similarity: 1.0,
                entry: hit,
            },
            2_000,
        );

        store.insert(entry("b1", "third", &[1.0, 1.0]), 3_000);

        assert_eq!(store.len(), 2);
        assert!(
            store.find_exact("b1", "first", 3_000).is_some(),
            "recently used stays"
        );
        assert!(
            store.find_exact("b1", "third", 3_000).is_some(),
            "the new row stays"
        );
        assert!(
            store.find_exact("b1", "second", 3_000).is_none(),
            "the coldest row goes"
        );
        assert_eq!(store.snapshot(3_000).counters.evictions_capacity, 1);
    }

    #[test]
    fn the_byte_bound_evicts_even_when_the_row_count_is_fine() {
        let budget = ENTRY_OVERHEAD_BYTES * 3;
        let store = CacheStore::new(limits(1_000, budget, 0));
        store.insert(entry("b1", "first", &[1.0, 0.0]), 0);
        store.insert(entry("b1", "second", &[0.0, 1.0]), 1_000);
        store.insert(entry("b1", "third", &[1.0, 1.0]), 2_000);

        let snapshot = store.snapshot(2_000);
        assert!(
            snapshot.entries < 3,
            "the byte budget must bite before the row budget"
        );
        assert!(
            snapshot.approx_bytes <= budget,
            "{} > {budget}",
            snapshot.approx_bytes
        );
        assert!(snapshot.counters.evictions_bytes > 0);
    }

    #[test]
    fn eviction_never_removes_the_row_it_just_inserted() {
        // Budget smaller than a single entry: without the protection this
        // would report a store that left nothing behind.
        let store = CacheStore::new(limits(1_000, 1, 0));
        let outcome = store.insert(entry("b1", "hello", &[1.0, 0.0]), 0);
        assert_eq!(store.len(), 1);
        assert!(store.find_exact("b1", "hello", 0).is_some());
        assert_eq!(outcome.entry_id, 1);
    }

    #[test]
    fn hit_rate_and_tokens_saved_add_up() {
        let store = store(600);
        store.insert(entry("b1", "hello", &[1.0, 0.0]), 0);

        for step in 0..3 {
            let hit = store.find_exact("b1", "hello", step).expect("present");
            store.record_lookup(
                &LookupResult::Hit {
                    kind: MatchKind::Exact,
                    similarity: 1.0,
                    entry: hit,
                },
                step,
            );
        }
        store.record_lookup(
            &LookupResult::Miss {
                reason: MissReason::BucketEmpty,
                best_similarity: None,
            },
            4,
        );

        let snapshot = store.snapshot(4);
        assert_eq!(snapshot.counters.lookups, 4);
        assert_eq!(snapshot.hits, 3);
        assert_eq!(snapshot.misses, 1);
        assert!(
            (snapshot.hit_rate - 0.75).abs() < 1e-9,
            "{}",
            snapshot.hit_rate
        );
        // Three hits on an entry recorded as 10 prompt + 20 completion tokens.
        assert_eq!(snapshot.counters.prompt_tokens_saved, 30);
        assert_eq!(snapshot.counters.completion_tokens_saved, 60);
        assert_eq!(snapshot.tokens_saved_total, 90);
        assert_eq!(
            snapshot.counters.misses_by_reason.get("bucket_empty"),
            Some(&1)
        );
    }

    #[test]
    fn hit_rate_is_zero_before_the_first_lookup() {
        assert_eq!(store(60).snapshot(0).hit_rate, 0.0);
    }

    #[test]
    fn recording_a_hit_updates_recency_on_the_live_row() {
        let store = store(600);
        store.insert(entry("b1", "hello", &[1.0, 0.0]), 0);
        let hit = store.find_exact("b1", "hello", 5_000).expect("present");
        store.record_lookup(
            &LookupResult::Hit {
                kind: MatchKind::Semantic,
                similarity: 0.99,
                entry: hit,
            },
            5_000,
        );
        let live = store.find_exact("b1", "hello", 5_000).expect("present");
        assert_eq!(live.hits, 1);
        assert_eq!(live.last_used_ms, 5_000);
        assert_eq!(store.snapshot(5_000).counters.hits_semantic, 1);
    }

    #[test]
    fn purge_scopes_do_what_they_say() {
        let store = store(0);
        store.insert(entry("b1", "one", &[1.0, 0.0]), 0);
        let mut other = entry("b2", "two", &[0.0, 1.0]);
        other.model = "other-model".to_string();
        store.insert(other, 0);

        assert_eq!(store.purge(PurgeScope::Model("other-model".into()), 0), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.purge(PurgeScope::All, 0), 1);
        assert_eq!(store.len(), 0);
        assert_eq!(store.snapshot(0).counters.purged_manually, 2);
    }

    #[test]
    fn the_snapshot_describes_what_is_actually_held() {
        let store = store(600);
        store.insert(entry("b1", "one", &[1.0, 0.0]), 0);
        let mut other = entry("b2", "two", &[0.0, 1.0]);
        other.model = "other-model".to_string();
        store.insert(other, 10_000);

        let snapshot = store.snapshot(20_000);
        assert_eq!(snapshot.entries, 2);
        assert_eq!(snapshot.buckets, 2);
        assert_eq!(snapshot.entries_by_model.get("test-model"), Some(&1));
        assert_eq!(snapshot.entries_by_model.get("other-model"), Some(&1));
        assert_eq!(snapshot.oldest_entry_age_seconds, Some(20));
        assert!(snapshot.approx_bytes >= ENTRY_OVERHEAD_BYTES * 2);
    }

    #[test]
    fn byte_accounting_returns_to_zero_when_everything_is_removed() {
        let store = store(0);
        store.insert(entry("b1", "one", &[1.0, 0.0]), 0);
        store.insert(entry("b1", "two", &[0.0, 1.0]), 0);
        assert!(store.snapshot(0).approx_bytes > 0);
        store.purge(PurgeScope::All, 0);
        assert_eq!(store.snapshot(0).approx_bytes, 0);
    }

    #[test]
    fn embedding_call_counters_separate_failures_from_attempts() {
        let store = store(60);
        store.record_embedding_call(true);
        store.record_embedding_call(false);
        let counters = store.snapshot(0).counters;
        assert_eq!(counters.embedding_calls, 2);
        assert_eq!(counters.embedding_failures, 1);
    }
}
