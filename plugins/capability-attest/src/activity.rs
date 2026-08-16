//! When it is safe to benchmark, and when it is not.
//!
//! Benchmarking a node that is serving somebody's request does two kinds of
//! damage: it degrades that request, and it measures a machine under unknown
//! load, which is not the number the record claims to carry. So the run has to
//! be gated, and the gate has to fail *closed* — if the plugin cannot tell
//! whether the node is busy, it defers.
//!
//! Every decision in this module is a pure function of a timestamp, the
//! schedule, and a contention signal. Nothing here does I/O; the probes that
//! produce a [`Contention`] live in `bench`.

use serde::Serialize;

/// What the plugin believes about current node load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contention {
    /// A signal was obtained and says the node is free.
    Idle { detail: String },
    /// A signal was obtained and says the node is working.
    Busy { detail: String },
    /// No usable signal. Treated exactly like `Busy`, on purpose.
    Unknown { detail: String },
}

/// Mutable scheduling state. Held by the plugin, mutated only around a run.
#[derive(Clone, Debug, Default)]
pub struct Schedule {
    /// Operator-requested pause, from the `hold` tool.
    pub hold_until_unix_ms: Option<u64>,
    pub hold_reason: Option<String>,
    /// End of the last completed attempt, successful or not.
    pub last_finished_unix_ms: Option<u64>,
    /// Consecutive failed attempts, used for backoff.
    pub consecutive_failures: u32,
}

/// Fixed scheduling limits, derived from configuration.
#[derive(Clone, Copy, Debug)]
pub struct SchedulePolicy {
    pub min_interval_ms: u64,
    pub failure_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl SchedulePolicy {
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            min_interval_ms,
            failure_backoff_ms: 60_000,
            max_backoff_ms: 3_600_000,
        }
    }
}

/// Why a benchmark did not run. Serialised straight into tool responses so an
/// operator never has to guess why the record is not being refreshed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "deferred_because", rename_all = "snake_case")]
pub enum Deferral {
    /// An operator asked for quiet.
    Hold {
        until_unix_ms: u64,
        reason: Option<String>,
    },
    /// The previous run finished too recently.
    Cooldown { next_attempt_unix_ms: u64 },
    /// Previous attempts failed; backing off before trying again.
    Backoff {
        next_attempt_unix_ms: u64,
        consecutive_failures: u32,
    },
    /// The node is serving traffic.
    NodeBusy { detail: String },
    /// Load could not be determined, so the run was skipped rather than risked.
    ActivityUnknown { detail: String },
}

impl Deferral {
    pub fn summary(&self) -> String {
        match self {
            Self::Hold {
                until_unix_ms,
                reason,
            } => match reason {
                Some(reason) => format!("held until {until_unix_ms} ({reason})"),
                None => format!("held until {until_unix_ms}"),
            },
            Self::Cooldown {
                next_attempt_unix_ms,
            } => format!("cooling down until {next_attempt_unix_ms}"),
            Self::Backoff {
                next_attempt_unix_ms,
                consecutive_failures,
            } => format!(
                "backing off until {next_attempt_unix_ms} after {consecutive_failures} failed attempts"
            ),
            Self::NodeBusy { detail } => format!("node is serving traffic: {detail}"),
            Self::ActivityUnknown { detail } => {
                format!("node load could not be determined: {detail}")
            }
        }
    }
}

/// Schedule-only gate: hold, backoff, cooldown.
///
/// `ignore_cooldown` is what the `benchmark` tool's `ignore_cooldown` argument
/// sets. It skips cooldown and backoff — an operator asking for a run now has
/// better information than a timer does — but it never skips a hold, because a
/// hold is itself an explicit operator instruction.
pub fn schedule_deferral(
    now_unix_ms: u64,
    schedule: &Schedule,
    policy: &SchedulePolicy,
    ignore_cooldown: bool,
) -> Option<Deferral> {
    if let Some(until) = schedule.hold_until_unix_ms
        && until > now_unix_ms
    {
        return Some(Deferral::Hold {
            until_unix_ms: until,
            reason: schedule.hold_reason.clone(),
        });
    }
    if ignore_cooldown {
        return None;
    }
    let last_finished = schedule.last_finished_unix_ms?;

    if schedule.consecutive_failures > 0 {
        let next = last_finished.saturating_add(backoff_ms(schedule.consecutive_failures, policy));
        if next > now_unix_ms {
            return Some(Deferral::Backoff {
                next_attempt_unix_ms: next,
                consecutive_failures: schedule.consecutive_failures,
            });
        }
        return None;
    }

    let next = last_finished.saturating_add(policy.min_interval_ms);
    if next > now_unix_ms {
        return Some(Deferral::Cooldown {
            next_attempt_unix_ms: next,
        });
    }
    None
}

/// Exponential backoff, capped. Saturating so a long-broken endpoint cannot
/// overflow its way back into a tight retry loop.
pub fn backoff_ms(consecutive_failures: u32, policy: &SchedulePolicy) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    let doublings = consecutive_failures.saturating_sub(1).min(20);
    policy
        .failure_backoff_ms
        .saturating_mul(1u64 << doublings)
        .min(policy.max_backoff_ms)
}

/// Load gate. `Unknown` defers, which is the whole point of the module.
pub fn contention_deferral(contention: &Contention) -> Option<Deferral> {
    match contention {
        Contention::Idle { .. } => None,
        Contention::Busy { detail } => Some(Deferral::NodeBusy {
            detail: detail.clone(),
        }),
        Contention::Unknown { detail } => Some(Deferral::ActivityUnknown {
            detail: detail.clone(),
        }),
    }
}

/// Read an in-flight request count out of a busy-probe response.
///
/// The probe is whatever the operator points `--busy-url` at, so the response
/// is untrusted: anything other than a number at the configured pointer is
/// `Unknown`, never an optimistic zero.
pub fn classify_busy_report(body: &serde_json::Value, pointer: &str, threshold: u64) -> Contention {
    let Some(value) = body.pointer(pointer) else {
        return Contention::Unknown {
            detail: format!("busy probe response has nothing at {pointer}"),
        };
    };
    let Some(active) = value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number.round() as u64)
    }) else {
        return Contention::Unknown {
            detail: format!("busy probe value at {pointer} is not a non-negative number: {value}"),
        };
    };

    if active <= threshold {
        Contention::Idle {
            detail: format!("{active} in-flight request(s) at {pointer}, threshold {threshold}"),
        }
    } else {
        Contention::Busy {
            detail: format!("{active} in-flight request(s) at {pointer}, threshold {threshold}"),
        }
    }
}

/// Fallback signal when no busy probe is configured: how long a one-token
/// request took to produce its first token.
///
/// This is a proxy, not a measurement of queue depth — a cold model or a slow
/// disk looks like a busy node. It is documented as such, and it errs towards
/// deferring. A probe that fails outright never reaches here; the caller turns
/// that into [`Contention::Unknown`].
pub fn classify_guard_probe(time_to_first_token_us: u64, max_ttft_ms: u64) -> Contention {
    let detail = format!(
        "guard probe first token in {}ms, limit {max_ttft_ms}ms \
         (latency proxy, not a queue depth)",
        time_to_first_token_us / 1000
    );
    if time_to_first_token_us <= max_ttft_ms.saturating_mul(1000) {
        Contention::Idle { detail }
    } else {
        Contention::Busy { detail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MINUTE: u64 = 60_000;

    fn policy() -> SchedulePolicy {
        SchedulePolicy::new(5 * MINUTE)
    }

    #[test]
    fn a_fresh_schedule_runs_immediately() {
        assert_eq!(
            schedule_deferral(1_000, &Schedule::default(), &policy(), false),
            None
        );
    }

    #[test]
    fn a_recent_run_cools_down_and_then_stops_cooling_down() {
        let schedule = Schedule {
            last_finished_unix_ms: Some(1_000_000),
            ..Schedule::default()
        };

        assert_eq!(
            schedule_deferral(1_000_000 + MINUTE, &schedule, &policy(), false),
            Some(Deferral::Cooldown {
                next_attempt_unix_ms: 1_000_000 + 5 * MINUTE
            })
        );
        assert_eq!(
            schedule_deferral(1_000_000 + 5 * MINUTE, &schedule, &policy(), false),
            None
        );
    }

    #[test]
    fn ignore_cooldown_skips_the_timer_but_never_an_operator_hold() {
        let cooling = Schedule {
            last_finished_unix_ms: Some(1_000_000),
            ..Schedule::default()
        };
        assert_eq!(
            schedule_deferral(1_000_000 + MINUTE, &cooling, &policy(), true),
            None
        );

        let held = Schedule {
            hold_until_unix_ms: Some(2_000_000),
            hold_reason: Some("firmware update".into()),
            ..Schedule::default()
        };
        assert_eq!(
            schedule_deferral(1_500_000, &held, &policy(), true),
            Some(Deferral::Hold {
                until_unix_ms: 2_000_000,
                reason: Some("firmware update".into()),
            })
        );
    }

    #[test]
    fn an_expired_hold_stops_blocking() {
        let held = Schedule {
            hold_until_unix_ms: Some(2_000_000),
            ..Schedule::default()
        };

        assert_eq!(schedule_deferral(2_000_000, &held, &policy(), false), None);
    }

    #[test]
    fn failures_back_off_exponentially_up_to_the_cap() {
        let policy = policy();
        assert_eq!(backoff_ms(0, &policy), 0);
        assert_eq!(backoff_ms(1, &policy), MINUTE);
        assert_eq!(backoff_ms(2, &policy), 2 * MINUTE);
        assert_eq!(backoff_ms(3, &policy), 4 * MINUTE);
        assert_eq!(backoff_ms(60, &policy), policy.max_backoff_ms);
    }

    #[test]
    fn backoff_replaces_cooldown_while_failures_are_outstanding() {
        let failing = Schedule {
            last_finished_unix_ms: Some(1_000_000),
            consecutive_failures: 3,
            ..Schedule::default()
        };

        assert_eq!(
            schedule_deferral(1_000_000 + MINUTE, &failing, &policy(), false),
            Some(Deferral::Backoff {
                next_attempt_unix_ms: 1_000_000 + 4 * MINUTE,
                consecutive_failures: 3,
            })
        );
        assert_eq!(
            schedule_deferral(1_000_000 + 4 * MINUTE, &failing, &policy(), false),
            None
        );
    }

    #[test]
    fn unknown_load_defers_exactly_like_a_busy_node() {
        assert!(
            contention_deferral(&Contention::Idle {
                detail: "quiet".into()
            })
            .is_none()
        );
        assert!(matches!(
            contention_deferral(&Contention::Busy {
                detail: "2 requests".into()
            }),
            Some(Deferral::NodeBusy { .. })
        ));
        assert!(
            matches!(
                contention_deferral(&Contention::Unknown {
                    detail: "probe unreachable".into()
                }),
                Some(Deferral::ActivityUnknown { .. })
            ),
            "an unreadable probe must never be read as idle"
        );
    }

    #[test]
    fn the_busy_probe_only_reports_idle_for_a_real_number_under_the_threshold() {
        let idle = classify_busy_report(&json!({ "active_requests": 0 }), "/active_requests", 0);
        assert!(matches!(idle, Contention::Idle { .. }));

        let under = classify_busy_report(&json!({ "active_requests": 2 }), "/active_requests", 2);
        assert!(matches!(under, Contention::Idle { .. }));

        let busy = classify_busy_report(&json!({ "active_requests": 3 }), "/active_requests", 2);
        assert!(matches!(busy, Contention::Busy { .. }));

        let nested = classify_busy_report(&json!({ "vllm": { "running": 1 } }), "/vllm/running", 0);
        assert!(matches!(nested, Contention::Busy { .. }));

        for hostile in [
            json!({}),
            json!({ "active_requests": "0" }),
            json!({ "active_requests": null }),
            json!({ "active_requests": -1 }),
            json!({ "active_requests": { "count": 0 } }),
        ] {
            assert!(
                matches!(
                    classify_busy_report(&hostile, "/active_requests", 0),
                    Contention::Unknown { .. }
                ),
                "{hostile} must not be read as idle"
            );
        }
    }

    #[test]
    fn the_guard_probe_treats_slow_first_tokens_as_contention() {
        assert!(matches!(
            classify_guard_probe(120_000, 750),
            Contention::Idle { .. }
        ));
        assert!(matches!(
            classify_guard_probe(750_000, 750),
            Contention::Idle { .. }
        ));
        assert!(matches!(
            classify_guard_probe(750_001, 750),
            Contention::Busy { .. }
        ));
        assert!(
            matches!(classify_guard_probe(0, 0), Contention::Idle { .. }),
            "a zero limit must not overflow into permanently busy"
        );
    }

    #[test]
    fn a_deferral_explains_itself_in_one_line() {
        let held = Deferral::Hold {
            until_unix_ms: 42,
            reason: Some("maintenance".into()),
        };
        assert!(held.summary().contains("maintenance"));

        let busy = Deferral::NodeBusy {
            detail: "3 in-flight".into(),
        };
        assert!(busy.summary().contains("3 in-flight"));
    }
}
