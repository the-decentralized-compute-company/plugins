//! The three limits that make this safe to run on someone else's machine:
//! how much disk it may use, how fast it may push bytes out, and how much it
//! will put in a single control-plane message.
//!
//! Everything here is a pure function or a struct with an injected clock, so
//! the policy can be tested without a filesystem, a network, or a running
//! host.

use std::fmt;

use serde::Serialize;

/// Default chunk size for a single transfer message.
pub const DEFAULT_CHUNK_BYTES: u64 = 1024 * 1024;

/// Hard ceiling on a single chunk, regardless of what the operator configures.
///
/// Chunks ride the host control connection as base64 inside a JSON result, so
/// the wire cost is roughly 4/3 of this plus JSON overhead. 8 MiB of payload is
/// already ~11 MiB of JSON; anything larger starts to defeat the point of the
/// control plane staying responsive.
pub const MAX_CHUNK_BYTES_CEILING: u64 = 8 * 1024 * 1024;

/// Default egress allowance: 64 MiB per minute, about 8.9 Mbit/s.
///
/// Deliberately conservative. A mirror runs on a contributor's home
/// connection, and the failure mode of a too-low default is "slow", while the
/// failure mode of a too-high default is "the household's video call breaks".
pub const DEFAULT_SERVE_BYTES_PER_MINUTE: u64 = 64 * 1024 * 1024;

/// Re-digest a cached artifact if it has not been fully verified in this long.
pub const DEFAULT_REVERIFY_AFTER_SECS: u64 = 24 * 60 * 60;

/// Clamp a caller-requested chunk length into the configured window.
///
/// `None` and `0` both mean "the mirror decides", which keeps a naive client
/// from having to know the server's limits before its first request.
pub fn clamp_chunk_length(requested: Option<u64>, max_chunk_bytes: u64) -> u64 {
    let max = max_chunk_bytes.clamp(1, MAX_CHUNK_BYTES_CEILING);
    match requested {
        None | Some(0) => DEFAULT_CHUNK_BYTES.min(max),
        Some(value) => value.min(max),
    }
}

/// One cached artifact, as far as the eviction policy is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictionCandidate {
    pub canonical_ref: String,
    pub size_bytes: u64,
    pub pinned: bool,
    /// Epoch seconds of the last read; the LRU ordering key.
    pub last_used_at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EvictionPlan {
    /// Canonical refs to drop, oldest first.
    pub evict: Vec<String>,
    pub freed_bytes: u64,
    /// Bytes the cache will hold once the plan runs and the incoming artifact
    /// lands.
    pub resulting_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapacityError {
    /// `max_cache_bytes` is zero: the mirror is configured to hold nothing.
    Disabled,
    /// The artifact alone does not fit inside the configured cap.
    TooLarge { needed: u64, cap: u64 },
    /// It would fit, but only if pinned artifacts were dropped.
    PinnedFull {
        needed: u64,
        cap: u64,
        pinned_bytes: u64,
    },
}

impl fmt::Display for CapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str(
                "this mirror is configured to hold nothing: set --max-cache-bytes to the amount \
                 of disk you are willing to contribute",
            ),
            Self::TooLarge { needed, cap } => write!(
                formatter,
                "artifact needs {needed} bytes but the whole cache cap is {cap} bytes; \
                 raise --max-cache-bytes or mirror a smaller quantization"
            ),
            Self::PinnedFull {
                needed,
                cap,
                pinned_bytes,
            } => write!(
                formatter,
                "artifact needs {needed} bytes, the cache cap is {cap} bytes, and {pinned_bytes} \
                 bytes are pinned; unpin something or raise --max-cache-bytes"
            ),
        }
    }
}

impl std::error::Error for CapacityError {}

/// Decide what to drop so `incoming_bytes` fits under `cap_bytes`.
///
/// `candidates` must exclude the artifact being admitted: re-importing an
/// artifact the mirror already holds replaces it, so its current bytes are
/// already accounted for as free.
///
/// Ordering is least-recently-used, tie-broken by canonical ref so the plan is
/// deterministic and an operator sees the same answer twice in a row.
pub fn plan_eviction(
    candidates: &[EvictionCandidate],
    cap_bytes: u64,
    incoming_bytes: u64,
) -> Result<EvictionPlan, CapacityError> {
    if cap_bytes == 0 {
        return Err(CapacityError::Disabled);
    }
    if incoming_bytes > cap_bytes {
        return Err(CapacityError::TooLarge {
            needed: incoming_bytes,
            cap: cap_bytes,
        });
    }

    let used: u64 = candidates.iter().map(|entry| entry.size_bytes).sum();
    if used + incoming_bytes <= cap_bytes {
        return Ok(EvictionPlan {
            evict: Vec::new(),
            freed_bytes: 0,
            resulting_bytes: used + incoming_bytes,
        });
    }

    let mut evictable: Vec<&EvictionCandidate> =
        candidates.iter().filter(|entry| !entry.pinned).collect();
    evictable.sort_by(|left, right| {
        left.last_used_at
            .cmp(&right.last_used_at)
            .then_with(|| left.canonical_ref.cmp(&right.canonical_ref))
    });

    let mut plan = EvictionPlan::default();
    let mut remaining = used;
    for entry in evictable {
        if remaining + incoming_bytes <= cap_bytes {
            break;
        }
        remaining -= entry.size_bytes;
        plan.freed_bytes += entry.size_bytes;
        plan.evict.push(entry.canonical_ref.clone());
    }

    if remaining + incoming_bytes > cap_bytes {
        let pinned_bytes = candidates
            .iter()
            .filter(|entry| entry.pinned)
            .map(|entry| entry.size_bytes)
            .sum();
        return Err(CapacityError::PinnedFull {
            needed: incoming_bytes,
            cap: cap_bytes,
            pinned_bytes,
        });
    }

    plan.resulting_bytes = remaining + incoming_bytes;
    Ok(plan)
}

/// A token bucket over outbound artifact bytes.
///
/// The clock is passed in as monotonic milliseconds rather than read
/// internally, so the refill arithmetic is testable and a backwards clock jump
/// cannot mint free budget.
#[derive(Clone, Debug)]
pub struct BandwidthBudget {
    /// `0` means unlimited: the operator explicitly opted out.
    bytes_per_minute: u64,
    /// Burst size — one minute of allowance.
    capacity_bytes: u64,
    available_bytes: u64,
    /// Sub-byte refill carry, numerator over 60_000 ms.
    carry: u64,
    last_refill_ms: u64,
}

const MILLIS_PER_MINUTE: u64 = 60_000;

impl BandwidthBudget {
    /// `bytes_per_minute == 0` disables throttling entirely.
    ///
    /// The bucket starts full, so a mirror that has been idle can answer the
    /// first request of a transfer at full speed.
    pub fn per_minute(bytes_per_minute: u64) -> Self {
        Self {
            bytes_per_minute,
            capacity_bytes: bytes_per_minute,
            available_bytes: bytes_per_minute,
            carry: 0,
            last_refill_ms: 0,
        }
    }

    pub fn is_unlimited(&self) -> bool {
        self.bytes_per_minute == 0
    }

    /// Bytes currently available without waiting.
    pub fn available(&self) -> u64 {
        self.available_bytes
    }

    /// Take up to `want` bytes, returning how many were granted.
    ///
    /// A partial grant is normal and useful: the caller serves a shorter chunk
    /// and the peer simply asks for the next offset. A zero grant is the
    /// caller's cue to report a throttle with [`BandwidthBudget::retry_after_ms`]
    /// rather than return an empty success.
    pub fn take(&mut self, now_ms: u64, want: u64) -> u64 {
        if self.is_unlimited() {
            return want;
        }
        self.refill(now_ms);
        let granted = want.min(self.available_bytes);
        self.available_bytes -= granted;
        granted
    }

    /// How long until `want` bytes could be granted, in milliseconds.
    pub fn retry_after_ms(&self, want: u64) -> u64 {
        if self.is_unlimited() || want <= self.available_bytes {
            return 0;
        }
        let target = want.min(self.capacity_bytes);
        let deficit = target.saturating_sub(self.available_bytes);
        // Round up so the caller never retries a millisecond too early.
        (deficit as u128 * MILLIS_PER_MINUTE as u128)
            .div_ceil(self.bytes_per_minute.max(1) as u128)
            .min(u64::MAX as u128) as u64
    }

    fn refill(&mut self, now_ms: u64) {
        // `saturating_sub` makes a backwards clock a no-op instead of a
        // negative elapsed time that mints budget, and the timestamp is only
        // ever moved forward so a clock that jumps back and then forward again
        // cannot be replayed for free budget either.
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms);
        if elapsed_ms == 0 {
            return;
        }
        self.last_refill_ms = now_ms;
        let numerator = elapsed_ms as u128 * self.bytes_per_minute as u128 + self.carry as u128;
        let gained = (numerator / MILLIS_PER_MINUTE as u128).min(u64::MAX as u128) as u64;
        self.carry = (numerator % MILLIS_PER_MINUTE as u128) as u64;
        self.available_bytes = self
            .available_bytes
            .saturating_add(gained)
            .min(self.capacity_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, size: u64, pinned: bool, last_used: u64) -> EvictionCandidate {
        EvictionCandidate {
            canonical_ref: name.to_string(),
            size_bytes: size,
            pinned,
            last_used_at: last_used,
        }
    }

    #[test]
    fn chunk_length_defaults_and_clamps() {
        assert_eq!(
            clamp_chunk_length(None, 4 * 1024 * 1024),
            DEFAULT_CHUNK_BYTES
        );
        assert_eq!(
            clamp_chunk_length(Some(0), 4 * 1024 * 1024),
            DEFAULT_CHUNK_BYTES
        );
        assert_eq!(clamp_chunk_length(Some(4096), 4 * 1024 * 1024), 4096);
        assert_eq!(
            clamp_chunk_length(Some(u64::MAX), 4 * 1024 * 1024),
            4 * 1024 * 1024
        );
    }

    #[test]
    fn chunk_length_never_exceeds_the_hard_ceiling() {
        assert_eq!(
            clamp_chunk_length(Some(u64::MAX), u64::MAX),
            MAX_CHUNK_BYTES_CEILING
        );
        // A misconfigured zero maximum still yields a usable single byte
        // rather than a zero-length read loop that never terminates.
        assert_eq!(clamp_chunk_length(Some(100), 0), 1);
    }

    #[test]
    fn eviction_is_a_noop_when_the_artifact_already_fits() {
        let candidates = [candidate("a", 100, false, 1)];

        let plan = plan_eviction(&candidates, 1_000, 200).expect("fits");

        assert!(plan.evict.is_empty());
        assert_eq!(plan.freed_bytes, 0);
        assert_eq!(plan.resulting_bytes, 300);
    }

    #[test]
    fn eviction_drops_least_recently_used_first_and_stops_early() {
        let candidates = [
            candidate("newest", 100, false, 300),
            candidate("oldest", 100, false, 100),
            candidate("middle", 100, false, 200),
        ];

        let plan = plan_eviction(&candidates, 300, 150).expect("fits after eviction");

        assert_eq!(plan.evict, vec!["oldest".to_string(), "middle".to_string()]);
        assert_eq!(plan.freed_bytes, 200);
        assert_eq!(plan.resulting_bytes, 250);
    }

    #[test]
    fn eviction_ties_break_deterministically_by_ref() {
        let candidates = [
            candidate("b", 100, false, 100),
            candidate("a", 100, false, 100),
        ];

        let plan = plan_eviction(&candidates, 250, 100).expect("fits after eviction");

        assert_eq!(plan.evict, vec!["a".to_string()]);
        assert_eq!(plan.resulting_bytes, 200);
    }

    #[test]
    fn eviction_never_touches_pinned_artifacts() {
        let candidates = [
            candidate("pinned", 800, true, 1),
            candidate("loose", 100, false, 2),
        ];

        let error = plan_eviction(&candidates, 1_000, 500).expect_err("cannot fit");

        assert_eq!(
            error,
            CapacityError::PinnedFull {
                needed: 500,
                cap: 1_000,
                pinned_bytes: 800,
            }
        );
    }

    #[test]
    fn eviction_refuses_an_artifact_larger_than_the_whole_cap() {
        assert_eq!(
            plan_eviction(&[], 1_000, 5_000).expect_err("too large"),
            CapacityError::TooLarge {
                needed: 5_000,
                cap: 1_000
            }
        );
    }

    #[test]
    fn a_zero_cap_means_the_mirror_holds_nothing() {
        assert_eq!(
            plan_eviction(&[], 0, 1).expect_err("disabled"),
            CapacityError::Disabled
        );
    }

    #[test]
    fn budget_grants_up_to_the_burst_then_refills_over_time() {
        let mut budget = BandwidthBudget::per_minute(60_000);

        assert_eq!(budget.take(0, 60_000), 60_000);
        assert_eq!(budget.take(0, 1), 0);
        // One second of refill is one sixtieth of the per-minute rate.
        assert_eq!(budget.take(1_000, 2_000), 1_000);
    }

    #[test]
    fn budget_grants_partially_rather_than_refusing() {
        let mut budget = BandwidthBudget::per_minute(60_000);
        assert_eq!(budget.take(0, 59_000), 59_000);

        assert_eq!(budget.take(0, 5_000), 1_000);
    }

    #[test]
    fn budget_carries_sub_byte_refill_instead_of_losing_it() {
        let mut budget = BandwidthBudget::per_minute(90_000);
        assert_eq!(budget.take(0, 90_000), 90_000);

        // 1 ms at 90_000 bytes/minute is 1.5 bytes. Without the carry the
        // fractional half would be rounded away on every poll and the mirror
        // would quietly serve a third slower than the operator configured.
        let granted: u64 = (1..=10).map(|step| budget.take(step, 10)).sum();

        assert_eq!(granted, 15);
    }

    #[test]
    fn budget_ignores_a_clock_that_moves_backwards() {
        let mut budget = BandwidthBudget::per_minute(60_000);
        assert_eq!(budget.take(10_000, 60_000), 60_000);

        assert_eq!(budget.take(0, 1_000), 0);
        assert_eq!(budget.take(1, 1_000), 0);
    }

    #[test]
    fn budget_reports_a_retry_delay_it_can_actually_meet() {
        let mut budget = BandwidthBudget::per_minute(60_000);
        assert_eq!(budget.take(0, 60_000), 60_000);

        let wait = budget.retry_after_ms(1_000);
        assert_eq!(wait, 1_000);
        assert_eq!(budget.take(wait, 1_000), 1_000);
    }

    #[test]
    fn an_unlimited_budget_grants_everything_and_never_asks_for_a_retry() {
        let mut budget = BandwidthBudget::per_minute(0);

        assert!(budget.is_unlimited());
        assert_eq!(budget.take(0, u64::MAX), u64::MAX);
        assert_eq!(budget.retry_after_ms(u64::MAX), 0);
    }
}
