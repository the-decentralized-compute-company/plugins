//! A per-endpoint call budget.
//!
//! A model that has been handed a callable API will, sooner or later, call it
//! in a loop. On hardware somebody lent to a mesh that is somebody else's rate
//! limit being spent, somebody else's bill, and somebody else's IP address
//! getting blocked. So every endpoint carries a `max_calls_per_minute` and this
//! is what enforces it.
//!
//! The state is a fixed-size map, seeded once from the declared endpoint names
//! and never inserted into again: a caller cannot grow it by naming endpoints
//! that do not exist. Time is passed in rather than read, so the whole thing is
//! testable without sleeping.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Window length. A fixed window rather than a sliding one: an operator
/// reasoning about "60 a minute" should not have to reason about a decay curve
/// as well.
pub const WINDOW_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    started_ms: u64,
    used: u32,
}

#[derive(Debug)]
pub struct RateLimiter {
    windows: Mutex<BTreeMap<String, Window>>,
}

/// What a caller is told after a call is admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub used: u32,
    pub limit: u32,
    /// Milliseconds until the current window rolls over.
    pub resets_in_ms: u64,
}

impl RateLimiter {
    /// Seed one window per declared endpoint. Endpoints are fixed for the life
    /// of the process, so this map never grows.
    pub fn new(endpoints: &[String]) -> Self {
        Self {
            windows: Mutex::new(
                endpoints
                    .iter()
                    .map(|name| {
                        (
                            name.clone(),
                            Window {
                                started_ms: 0,
                                used: 0,
                            },
                        )
                    })
                    .collect(),
            ),
        }
    }

    /// Count one call against `endpoint`, or refuse it.
    ///
    /// The refusal names how long the caller should wait, because "try again
    /// later" without a number is an invitation to retry immediately.
    pub fn admit(&self, endpoint: &str, limit: u32, now_ms: u64) -> Result<Budget, String> {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let window = windows.get_mut(endpoint).ok_or_else(|| {
            format!("endpoint `{endpoint}` has no rate-limit window; it is not declared")
        })?;

        if now_ms.saturating_sub(window.started_ms) >= WINDOW_MS {
            window.started_ms = now_ms;
            window.used = 0;
        }

        if window.used >= limit {
            let resets_in_ms = WINDOW_MS.saturating_sub(now_ms.saturating_sub(window.started_ms));
            return Err(format!(
                "endpoint `{endpoint}` has used its budget of {limit} calls per minute. The \
                 window resets in {} seconds. Raise `max_calls_per_minute` on that endpoint in \
                 rest-client.toml if the limit is wrong.",
                resets_in_ms.div_ceil(1_000)
            ));
        }

        window.used += 1;
        Ok(Budget {
            used: window.used,
            limit,
            resets_in_ms: WINDOW_MS.saturating_sub(now_ms.saturating_sub(window.started_ms)),
        })
    }

    /// What the current window looks like, without counting a call. Used by the
    /// `status` tool, which must never have a side effect.
    pub fn peek(&self, endpoint: &str, limit: u32, now_ms: u64) -> Budget {
        let windows = self
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match windows.get(endpoint) {
            Some(window) if now_ms.saturating_sub(window.started_ms) < WINDOW_MS => Budget {
                used: window.used,
                limit,
                resets_in_ms: WINDOW_MS.saturating_sub(now_ms.saturating_sub(window.started_ms)),
            },
            _ => Budget {
                used: 0,
                limit,
                resets_in_ms: WINDOW_MS,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        RateLimiter::new(&["one".to_string(), "two".to_string()])
    }

    #[test]
    fn calls_are_admitted_up_to_the_limit_and_then_refused() {
        let limiter = limiter();

        for expected in 1..=3 {
            let budget = limiter.admit("one", 3, 1_000).expect("within budget");
            assert_eq!(budget.used, expected);
            assert_eq!(budget.limit, 3);
        }

        let error = limiter.admit("one", 3, 1_000).expect_err("over budget");
        assert!(error.contains("3 calls per minute"), "{error}");
        assert!(error.contains("max_calls_per_minute"), "{error}");
    }

    #[test]
    fn the_window_rolls_over_after_a_minute() {
        let limiter = limiter();
        limiter.admit("one", 1, 0).expect("first call");
        assert!(limiter.admit("one", 1, WINDOW_MS - 1).is_err());

        let budget = limiter
            .admit("one", 1, WINDOW_MS)
            .expect("a new window has started");
        assert_eq!(budget.used, 1);
    }

    #[test]
    fn endpoints_have_separate_budgets() {
        let limiter = limiter();
        limiter.admit("one", 1, 0).expect("first call");

        assert!(limiter.admit("one", 1, 0).is_err());
        assert!(limiter.admit("two", 1, 0).is_ok());
    }

    #[test]
    fn an_undeclared_endpoint_cannot_create_a_window() {
        let limiter = limiter();

        let error = limiter
            .admit("three", 10, 0)
            .expect_err("unknown endpoints have no budget");

        assert!(error.contains("not declared"), "{error}");
    }

    #[test]
    fn peeking_reports_the_window_without_spending_it() {
        let limiter = limiter();
        limiter.admit("one", 5, 0).expect("first call");

        assert_eq!(limiter.peek("one", 5, 0).used, 1);
        assert_eq!(limiter.peek("one", 5, 0).used, 1);
        assert_eq!(limiter.peek("one", 5, WINDOW_MS).used, 0);
        assert_eq!(limiter.peek("three", 5, 0).used, 0);
    }

    #[test]
    fn the_refusal_says_how_long_to_wait() {
        let limiter = limiter();
        // The first call opens a window at 60_000; 30 seconds of it are gone by
        // the time the second one is refused.
        limiter.admit("one", 1, 60_000).expect("first call");

        let error = limiter.admit("one", 1, 90_000).expect_err("over budget");

        assert!(error.contains("30 seconds"), "{error}");
    }
}
