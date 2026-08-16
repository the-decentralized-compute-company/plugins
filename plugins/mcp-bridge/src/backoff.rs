//! When to try a dead upstream again.
//!
//! An MCP server that crashes on every request must not be relaunched in a
//! tight loop: that is a fork bomb with extra steps on hardware somebody else
//! paid for. An upstream that dropped once because a laptop slept must not take
//! a minute to come back either.
//!
//! The schedule is a pure function of the attempt number — 1 s, 2 s, 4 s, 8 s,
//! 16 s, 32 s, then 60 s for ever — with **no jitter**, deliberately. Jitter
//! spreads a herd of clients across a shared server; here the "herd" is at most
//! [`crate::config::MAX_SERVERS`] processes on one machine, each with its own
//! backend, and a reconnect schedule an operator can predict from `status` is
//! worth more than a smoother graph.

use std::time::Duration;

/// Delay before the first reconnect attempt.
pub const BASE_DELAY: Duration = Duration::from_secs(1);
/// Ceiling the schedule flattens out at.
pub const MAX_DELAY: Duration = Duration::from_secs(60);
/// How often a live connection is checked for having gone away.
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// Delay before attempt number `attempt`, counting from 1.
pub fn delay_for_attempt(attempt: u32) -> Duration {
    if attempt <= 1 {
        return BASE_DELAY;
    }
    // `saturating_sub` then a checked shift: attempt 33 would overflow a u32
    // shift, and this runs unattended for as long as the node does.
    let doublings = attempt - 1;
    let multiplier = 1u64.checked_shl(doublings.min(63)).unwrap_or(u64::MAX);
    let delay = BASE_DELAY.saturating_mul(multiplier.min(u64::from(u32::MAX)) as u32);
    delay.min(MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_attempt_is_immediate_enough_to_ride_out_a_blip() {
        assert_eq!(delay_for_attempt(0), BASE_DELAY);
        assert_eq!(delay_for_attempt(1), BASE_DELAY);
    }

    #[test]
    fn the_schedule_doubles_until_it_reaches_the_ceiling() {
        let delays: Vec<u64> = (1..=8)
            .map(|attempt| delay_for_attempt(attempt).as_secs())
            .collect();

        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn the_ceiling_holds_for_an_attempt_count_that_would_overflow_a_shift() {
        for attempt in [40u32, 64, 1_000, u32::MAX] {
            assert_eq!(
                delay_for_attempt(attempt),
                MAX_DELAY,
                "attempt {attempt} must not overflow or wrap"
            );
        }
    }

    /// A crash loop must cost the machine less over time, not the same.
    #[test]
    fn a_crash_loop_costs_at_most_one_relaunch_a_minute_once_it_settles() {
        assert_eq!(delay_for_attempt(u32::MAX), MAX_DELAY);
        assert!(MAX_DELAY >= Duration::from_secs(60));
    }

    #[test]
    fn the_schedule_never_returns_zero() {
        for attempt in 0..64 {
            assert!(
                delay_for_attempt(attempt) > Duration::ZERO,
                "attempt {attempt}"
            );
        }
    }
}
