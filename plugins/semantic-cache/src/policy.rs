//! The decisions that make the cache safe, as pure functions.
//!
//! Everything here is deliberately free of I/O and free of the store, because
//! these are the rules that have to be right: what may be written, what may be
//! served, and how close "close enough" is.

use serde::Serialize;

/// Bumped whenever the canonical key encoding changes.
///
/// The cache is in-memory and dies with the process, so this is not a
/// migration marker — it exists so that a key format change cannot silently
/// alias with the old one during a rolling restart of a mesh.
pub const CANONICAL_KEY_VERSION: &str = "1";

/// Why an entry was not written.
///
/// No `Eq`: two of these carry `f64` payloads. `PartialEq` is enough for the
/// tests and nothing keys a map on a rejection value.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StoreRejection {
    /// The caller told us the response carried an error. Caching an error
    /// means every reworded form of the question gets the error back for the
    /// whole TTL, long after the transient cause is gone.
    ResponseWasAnError,
    /// Nothing to serve.
    EmptyCompletion,
    /// Only a naturally finished response is a reusable answer. `length` is a
    /// truncated response, `tool_calls` is a request to go and do something
    /// (the answer depends on what that call returns), and `content_filter` is
    /// a refusal that may not apply to a differently-worded prompt.
    UnfinishedResponse { finish_reason: String },
    /// The request asked for more variance than the operator allows to be
    /// frozen into a cache entry.
    TemperatureAboveLimit { temperature: f64, limit: f64 },
    /// A single entry larger than the whole byte budget can never be stored
    /// without evicting everything else, so it is refused outright rather than
    /// admitted and immediately evicted.
    EntryLargerThanBudget { entry_bytes: u64, budget_bytes: u64 },
}

/// Refuse an entry that cannot coexist with anything else.
///
/// Checked after embedding rather than with the rest of the rules, because the
/// vector's size is not known until the embedder has answered.
pub fn fits_budget(entry_bytes: u64, budget_bytes: u64) -> Result<(), StoreRejection> {
    if entry_bytes > budget_bytes {
        return Err(StoreRejection::EntryLargerThanBudget {
            entry_bytes,
            budget_bytes,
        });
    }
    Ok(())
}

impl StoreRejection {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::ResponseWasAnError => "response_was_an_error",
            Self::EmptyCompletion => "empty_completion",
            Self::UnfinishedResponse { .. } => "unfinished_response",
            Self::TemperatureAboveLimit { .. } => "temperature_above_limit",
            Self::EntryLargerThanBudget { .. } => "entry_larger_than_budget",
        }
    }
}

/// The only `finish_reason` a completion may be cached under.
pub const CACHEABLE_FINISH_REASON: &str = "stop";

/// Facts about a completion the caller is offering to the cache.
#[derive(Clone, Copy, Debug)]
pub struct StoreCandidate<'a> {
    pub completion: &'a str,
    /// `None` is treated as `stop`: callers that do not track finish reasons
    /// (a plain text pipeline, for instance) should not be locked out, but a
    /// caller that *does* pass one gets it enforced.
    pub finish_reason: Option<&'a str>,
    /// Set by the caller when the upstream request failed. Explicit rather
    /// than inferred, because an error body is often a perfectly well-formed
    /// string.
    pub is_error: bool,
    pub temperature: Option<f64>,
}

/// Decide whether a completion may enter the cache.
///
/// These are the checks that need no network: they run *before* the prompt is
/// embedded, so a response that was never cacheable does not cost an embedding
/// call. The size check ([`fits_budget`]) runs afterwards, once the vector's
/// dimension is known.
pub fn store_decision(
    candidate: StoreCandidate<'_>,
    max_temperature: f64,
) -> Result<(), StoreRejection> {
    if candidate.is_error {
        return Err(StoreRejection::ResponseWasAnError);
    }
    if candidate.completion.trim().is_empty() {
        return Err(StoreRejection::EmptyCompletion);
    }
    let finish_reason = candidate.finish_reason.unwrap_or(CACHEABLE_FINISH_REASON);
    if !finish_reason.eq_ignore_ascii_case(CACHEABLE_FINISH_REASON) {
        return Err(StoreRejection::UnfinishedResponse {
            finish_reason: finish_reason.to_string(),
        });
    }
    temperature_gate(candidate.temperature, max_temperature)
}

/// Gate both reads and writes on temperature.
///
/// An omitted temperature is treated as the OpenAI default of 1.0, which is
/// the conservative reading: assume the caller wanted variance unless they
/// said otherwise.
pub fn temperature_gate(temperature: Option<f64>, limit: f64) -> Result<(), StoreRejection> {
    let effective = temperature.unwrap_or(1.0);
    if effective > limit {
        return Err(StoreRejection::TemperatureAboveLimit {
            temperature: effective,
            limit,
        });
    }
    Ok(())
}

/// Resolve a per-call similarity override against the operator's floor.
///
/// A caller may only ever be **stricter** than the operator configured. That
/// asymmetry is the point: the operator owns the machine and sets the risk
/// budget, and a tool argument — which on this surface is frequently chosen by
/// a language model — must not be able to talk the cache into a looser match.
pub fn effective_threshold(configured: f64, requested: Option<f64>) -> f64 {
    match requested {
        Some(requested) if requested.is_finite() => requested.clamp(configured, 1.0),
        _ => configured,
    }
}

/// Reject a neighbour whose length is wildly different from the query's.
///
/// A paraphrase is about as long as what it paraphrases. When a two-word
/// question scores 0.96 against a 600-word one, the score is measuring shared
/// topic, not shared meaning. Character length is a crude proxy for token
/// length and is used because it needs no tokenizer and no per-model
/// vocabulary.
pub fn length_ratio_ok(query_len: usize, candidate_len: usize, max_ratio: f64) -> bool {
    let (shorter, longer) = if query_len <= candidate_len {
        (query_len, candidate_len)
    } else {
        (candidate_len, query_len)
    };
    if shorter == 0 {
        return longer == 0;
    }
    (longer as f64 / shorter as f64) <= max_ratio
}

/// Scale a vector to unit length so that a dot product *is* cosine similarity.
///
/// Returns `Err` for a vector that is empty, non-finite, or all zeros — none
/// of which have a direction, and all of which would otherwise produce a
/// meaningless similarity instead of an error.
pub fn normalize_l2(mut vector: Vec<f32>) -> Result<Vec<f32>, String> {
    if vector.is_empty() {
        return Err("embedding is empty".to_string());
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err("embedding contains a non-finite value".to_string());
    }
    // Accumulate in f64: a 3072-dimension vector of ~1.0 components overflows
    // f32 precision long before it overflows range, and the resulting norm is
    // visibly wrong.
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm <= 0.0 || !norm.is_finite() {
        return Err("embedding has zero magnitude".to_string());
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(vector)
}

/// Cosine similarity of two already-normalized vectors.
///
/// `None` on a dimension mismatch rather than a panic or a truncated compare:
/// mismatched dimensions mean the two vectors came from different embedders,
/// and the honest answer is "these are not comparable".
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let dot: f64 = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    // Rounding can push a self-comparison a hair past 1.0; clamping keeps the
    // reported number inside the range the threshold is expressed in.
    Some(dot.clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(completion: &str) -> StoreCandidate<'_> {
        StoreCandidate {
            completion,
            finish_reason: None,
            is_error: false,
            temperature: Some(0.0),
        }
    }

    #[test]
    fn a_normal_completion_is_cacheable() {
        assert_eq!(store_decision(candidate("4"), 1.0), Ok(()));
    }

    #[test]
    fn an_errored_response_is_never_cached() {
        let mut offered = candidate("upstream returned 503");
        offered.is_error = true;
        assert_eq!(
            store_decision(offered, 1.0),
            Err(StoreRejection::ResponseWasAnError)
        );
    }

    #[test]
    fn an_empty_completion_is_never_cached() {
        assert_eq!(
            store_decision(candidate("   \n\t "), 1.0),
            Err(StoreRejection::EmptyCompletion)
        );
    }

    #[test]
    fn only_a_naturally_finished_response_is_cached() {
        for reason in ["length", "tool_calls", "content_filter", "function_call"] {
            let mut offered = candidate("partial answer");
            offered.finish_reason = Some(reason);
            assert_eq!(
                store_decision(offered, 1.0),
                Err(StoreRejection::UnfinishedResponse {
                    finish_reason: reason.to_string()
                }),
                "{reason} must not be cached"
            );
        }
    }

    #[test]
    fn stop_is_accepted_in_any_casing_and_when_omitted() {
        for reason in [Some("stop"), Some("STOP"), None] {
            let mut offered = candidate("answer");
            offered.finish_reason = reason;
            assert_eq!(store_decision(offered, 1.0), Ok(()), "{reason:?}");
        }
    }

    #[test]
    fn a_request_hotter_than_the_limit_is_not_cached() {
        let mut offered = candidate("answer");
        offered.temperature = Some(1.4);
        assert_eq!(
            store_decision(offered, 1.0),
            Err(StoreRejection::TemperatureAboveLimit {
                temperature: 1.4,
                limit: 1.0
            })
        );
    }

    #[test]
    fn an_omitted_temperature_is_treated_as_the_openai_default() {
        assert!(temperature_gate(None, 1.0).is_ok());
        assert!(
            temperature_gate(None, 0.0).is_err(),
            "with a greedy-only limit, an unspecified temperature must not sneak through"
        );
    }

    #[test]
    fn an_entry_larger_than_the_whole_budget_is_refused() {
        assert_eq!(fits_budget(999, 1_000), Ok(()));
        assert_eq!(fits_budget(1_000, 1_000), Ok(()));
        assert_eq!(
            fits_budget(2_000, 1_000),
            Err(StoreRejection::EntryLargerThanBudget {
                entry_bytes: 2_000,
                budget_bytes: 1_000
            })
        );
    }

    #[test]
    fn rejection_slugs_are_stable_and_distinct() {
        let rejections = [
            StoreRejection::ResponseWasAnError,
            StoreRejection::EmptyCompletion,
            StoreRejection::UnfinishedResponse {
                finish_reason: "length".into(),
            },
            StoreRejection::TemperatureAboveLimit {
                temperature: 2.0,
                limit: 1.0,
            },
            StoreRejection::EntryLargerThanBudget {
                entry_bytes: 2,
                budget_bytes: 1,
            },
        ];
        let mut slugs: Vec<&str> = rejections.iter().map(StoreRejection::slug).collect();
        slugs.sort_unstable();
        let count = slugs.len();
        slugs.dedup();
        assert_eq!(
            slugs.len(),
            count,
            "slugs are stats keys and must not collide"
        );
    }

    #[test]
    fn a_caller_may_tighten_the_threshold_but_never_loosen_it() {
        assert_eq!(effective_threshold(0.95, Some(0.99)), 0.99);
        assert_eq!(
            effective_threshold(0.95, Some(0.10)),
            0.95,
            "a tool argument must not be able to widen the operator's risk budget"
        );
        assert_eq!(effective_threshold(0.95, None), 0.95);
        assert_eq!(effective_threshold(0.95, Some(f64::NAN)), 0.95);
        assert_eq!(effective_threshold(0.95, Some(5.0)), 1.0);
    }

    #[test]
    fn the_length_guard_admits_paraphrases_and_rejects_mismatches() {
        assert!(
            length_ratio_ok(40, 44, 2.0),
            "a reworded question is about as long"
        );
        assert!(
            length_ratio_ok(40, 80, 2.0),
            "exactly at the limit is allowed"
        );
        assert!(!length_ratio_ok(40, 81, 2.0));
        assert!(
            !length_ratio_ok(12, 600, 2.0),
            "a short question vs an essay"
        );
        assert!(length_ratio_ok(0, 0, 2.0));
        assert!(!length_ratio_ok(0, 5, 2.0));
    }

    #[test]
    fn normalizing_makes_the_dot_product_a_cosine() {
        let left = normalize_l2(vec![3.0, 4.0]).expect("normalizes");
        let right = normalize_l2(vec![30.0, 40.0]).expect("normalizes");
        let similarity = cosine_similarity(&left, &right).expect("same dimension");
        assert!(
            (similarity - 1.0).abs() < 1e-6,
            "parallel vectors: {similarity}"
        );

        let orthogonal = normalize_l2(vec![-4.0, 3.0]).expect("normalizes");
        let similarity = cosine_similarity(&left, &orthogonal).expect("same dimension");
        assert!(similarity.abs() < 1e-6, "orthogonal vectors: {similarity}");

        let opposite = normalize_l2(vec![-3.0, -4.0]).expect("normalizes");
        let similarity = cosine_similarity(&left, &opposite).expect("same dimension");
        assert!(
            (similarity + 1.0).abs() < 1e-6,
            "opposed vectors: {similarity}"
        );
    }

    #[test]
    fn similarity_never_escapes_its_range() {
        let vector = normalize_l2(vec![1.0; 1536]).expect("normalizes");
        let similarity = cosine_similarity(&vector, &vector).expect("same dimension");
        assert!((0.0..=1.0).contains(&similarity), "{similarity}");
    }

    #[test]
    fn a_directionless_embedding_is_an_error_not_a_similarity() {
        assert!(normalize_l2(vec![]).is_err());
        assert!(normalize_l2(vec![0.0, 0.0, 0.0]).is_err());
        assert!(normalize_l2(vec![1.0, f32::NAN]).is_err());
        assert!(normalize_l2(vec![f32::INFINITY, 1.0]).is_err());
    }

    #[test]
    fn vectors_from_different_embedders_are_not_comparable() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), None);
        assert_eq!(cosine_similarity(&[], &[]), None);
    }
}
