//! Request-rate accounting for `action = "limit"` rules.
//!
//! A token bucket, in integer arithmetic, over a caller-supplied `now`. That
//! combination buys three things that matter on donated hardware:
//!
//! * **Bounded memory.** One bucket is 24 bytes plus its key, regardless of how
//!   many requests it has seen. A queue-of-timestamps limiter would let a rule
//!   of `requests = 10000` pin ~80 KB per key.
//! * **Bounded cardinality.** Bucket keys contain caller-supplied peer and
//!   owner ids, so the map is capped and pruned; see [`MAX_TRACKED_BUCKETS`].
//! * **Determinism.** No floats and no internal clock, so the tests below pin
//!   down exact refill behaviour instead of sleeping.

use std::collections::HashMap;

/// Upper bound on distinct rate-limit buckets held at once.
///
/// `per = "peer"` and `per = "owner"` key on identifiers that come from the
/// caller, so an unbounded map would be a memory-exhaustion vector. 4096 covers
/// any plausible mesh; past that the tracker prunes, and if pruning does not
/// help it refuses new keys rather than growing (see [`Admission::NoCapacity`]).
pub const MAX_TRACKED_BUCKETS: usize = 4096;

/// Fixed-point scale for tokens. One whole token is `TOKEN_SCALE`.
const TOKEN_SCALE: i64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Under the limit; one token has been spent.
    Admitted,
    /// Over the limit. `retry_after_ms` is when the next token is available.
    OverLimit { retry_after_ms: i64 },
    /// The tracker is at [`MAX_TRACKED_BUCKETS`] and this key is new. Refusing
    /// is the conservative reading of a rule the operator wrote as a limit.
    NoCapacity,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens available, scaled by [`TOKEN_SCALE`].
    tokens: i64,
    /// When `tokens` was last recomputed.
    updated_ms: i64,
}

#[derive(Debug, Default)]
pub struct TokenBuckets {
    buckets: HashMap<String, Bucket>,
}

impl TokenBuckets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spend one token from `key`'s bucket.
    ///
    /// `requests` is the burst capacity and the number of tokens that refill
    /// over `window_ms`. Both are validated at policy load time, but this
    /// function stays defensive: a zero or negative budget denies rather than
    /// dividing by zero.
    pub fn admit(&mut self, key: &str, now_ms: i64, requests: u32, window_ms: i64) -> Admission {
        if requests == 0 || window_ms <= 0 {
            return Admission::OverLimit {
                retry_after_ms: window_ms.max(0),
            };
        }
        let capacity = i64::from(requests) * TOKEN_SCALE;

        if !self.buckets.contains_key(key) && self.buckets.len() >= MAX_TRACKED_BUCKETS {
            self.prune(now_ms, window_ms);
            if self.buckets.len() >= MAX_TRACKED_BUCKETS {
                return Admission::NoCapacity;
            }
        }

        let bucket = self.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            updated_ms: now_ms,
        });

        // A system clock that steps backwards must not mint tokens, so elapsed
        // time is floored at zero. It is also capped at one window, because a
        // longer gap always refills to capacity and the cap keeps the
        // multiplication below well inside i64.
        let elapsed = (now_ms - bucket.updated_ms).clamp(0, window_ms);
        let refill = elapsed * i64::from(requests) * TOKEN_SCALE / window_ms;
        bucket.tokens = (bucket.tokens + refill).min(capacity);
        bucket.updated_ms = now_ms;

        if bucket.tokens >= TOKEN_SCALE {
            bucket.tokens -= TOKEN_SCALE;
            Admission::Admitted
        } else {
            // Milliseconds until one whole token is back:
            //   missing_tokens / (requests tokens per window_ms).
            // Rounded up, because reporting "retry in 0 ms" while still
            // refusing is worse than reporting one millisecond too many.
            let missing = TOKEN_SCALE - bucket.tokens;
            let numerator = missing * window_ms;
            let denominator = i64::from(requests) * TOKEN_SCALE;
            let retry_after_ms = (numerator + denominator - 1) / denominator;
            Admission::OverLimit {
                retry_after_ms: retry_after_ms.max(1),
            }
        }
    }

    /// Drop buckets that have been idle long enough to be back at capacity.
    /// Their absence is indistinguishable from their presence, so this is
    /// lossless.
    fn prune(&mut self, now_ms: i64, window_ms: i64) {
        self.buckets
            .retain(|_, bucket| now_ms.saturating_sub(bucket.updated_ms) < window_ms);
    }

    pub fn tracked_keys(&self) -> usize {
        self.buckets.len()
    }

    /// Forget all accounting. Used when a reload replaces the rule set: rule
    /// ids are part of the bucket key, so stale buckets would otherwise leak
    /// budget from rules that no longer exist.
    pub fn clear(&mut self) {
        self.buckets.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: i64 = 60_000;

    #[test]
    fn a_fresh_bucket_admits_its_full_burst_then_refuses() {
        let mut buckets = TokenBuckets::new();

        for index in 0..3 {
            assert_eq!(
                buckets.admit("rule/peer-a", 1_000, 3, WINDOW),
                Admission::Admitted,
                "burst request {index} should be admitted"
            );
        }

        let Admission::OverLimit { retry_after_ms } =
            buckets.admit("rule/peer-a", 1_000, 3, WINDOW)
        else {
            panic!("the fourth request in a burst of three must be refused");
        };
        // Three requests per minute means one token every 20 s.
        assert_eq!(retry_after_ms, WINDOW / 3);
    }

    #[test]
    fn tokens_refill_over_the_window() {
        let mut buckets = TokenBuckets::new();
        for _ in 0..3 {
            assert_eq!(buckets.admit("r", 0, 3, WINDOW), Admission::Admitted);
        }
        assert!(matches!(
            buckets.admit("r", 0, 3, WINDOW),
            Admission::OverLimit { .. }
        ));

        // One third of the window later, exactly one token is back.
        assert_eq!(
            buckets.admit("r", WINDOW / 3, 3, WINDOW),
            Admission::Admitted
        );
        assert!(matches!(
            buckets.admit("r", WINDOW / 3, 3, WINDOW),
            Admission::OverLimit { .. }
        ));
    }

    #[test]
    fn refill_never_exceeds_the_burst_capacity() {
        let mut buckets = TokenBuckets::new();
        assert_eq!(buckets.admit("r", 0, 2, WINDOW), Admission::Admitted);

        // An hour of idleness still only buys back the two-request burst.
        for _ in 0..2 {
            assert_eq!(
                buckets.admit("r", 3_600_000, 2, WINDOW),
                Admission::Admitted
            );
        }
        assert!(matches!(
            buckets.admit("r", 3_600_000, 2, WINDOW),
            Admission::OverLimit { .. }
        ));
    }

    #[test]
    fn separate_keys_have_separate_budgets() {
        let mut buckets = TokenBuckets::new();

        assert_eq!(buckets.admit("r/peer-a", 0, 1, WINDOW), Admission::Admitted);
        assert!(matches!(
            buckets.admit("r/peer-a", 0, 1, WINDOW),
            Admission::OverLimit { .. }
        ));
        assert_eq!(buckets.admit("r/peer-b", 0, 1, WINDOW), Admission::Admitted);
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_mint_tokens() {
        let mut buckets = TokenBuckets::new();
        assert_eq!(buckets.admit("r", 10_000, 1, WINDOW), Admission::Admitted);

        assert!(matches!(
            buckets.admit("r", 0, 1, WINDOW),
            Admission::OverLimit { .. }
        ));
    }

    #[test]
    fn idle_buckets_are_pruned_before_new_keys_are_refused() {
        let mut buckets = TokenBuckets::new();
        for index in 0..MAX_TRACKED_BUCKETS {
            assert_eq!(
                buckets.admit(&format!("r/{index}"), 0, 5, WINDOW),
                Admission::Admitted
            );
        }
        assert_eq!(buckets.tracked_keys(), MAX_TRACKED_BUCKETS);

        // A full window later every existing bucket is idle, so a new key is
        // admitted and the map does not grow past the cap.
        assert_eq!(
            buckets.admit("r/new", WINDOW, 5, WINDOW),
            Admission::Admitted
        );
        assert!(buckets.tracked_keys() <= MAX_TRACKED_BUCKETS);
    }

    #[test]
    fn a_new_key_is_refused_when_every_bucket_is_still_active() {
        let mut buckets = TokenBuckets::new();
        for index in 0..MAX_TRACKED_BUCKETS {
            buckets.admit(&format!("r/{index}"), 0, 5, WINDOW);
        }

        assert_eq!(buckets.admit("r/new", 0, 5, WINDOW), Admission::NoCapacity);
    }

    #[test]
    fn a_degenerate_limit_refuses_instead_of_dividing_by_zero() {
        let mut buckets = TokenBuckets::new();

        assert!(matches!(
            buckets.admit("r", 0, 0, WINDOW),
            Admission::OverLimit { .. }
        ));
        assert!(matches!(
            buckets.admit("r", 0, 5, 0),
            Admission::OverLimit { .. }
        ));
    }

    #[test]
    fn clear_forgets_every_bucket() {
        let mut buckets = TokenBuckets::new();
        buckets.admit("r", 0, 1, WINDOW);
        buckets.clear();

        assert_eq!(buckets.tracked_keys(), 0);
        assert_eq!(buckets.admit("r", 0, 1, WINDOW), Admission::Admitted);
    }
}
