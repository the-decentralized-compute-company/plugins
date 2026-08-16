//! Whether a job runs right now, as a pure function.
//!
//! The scheduler in [`crate::scheduler`] does the impure parts — reading the
//! clock, holding the lock, spawning the task. Everything about *whether* a run
//! should happen is decided here, from values, so every rule below is a test
//! rather than a claim.
//!
//! # The order the gates are applied
//!
//! 1. **The file disabled it.** Nothing else is considered.
//! 2. **It is paused**, by the `pause` tool or by its own quarantine.
//! 3. **Its cursor came from a schedule it no longer has**, because the
//!    operator edited the file. The cursor is rebuilt from the file and the job
//!    waits; the old schedule's occurrences are not counted as missed.
//! 4. **It is not due yet.**
//! 5. **It is already running.** A job never overlaps itself; the occurrence is
//!    skipped rather than queued.
//! 6. **It is backing off** after consecutive failures.
//! 7. **It is outside its window.** Checked against *now*, the instant the run
//!    would actually start, not against the time it came due.
//! 8. **The misfire policy applies**, because the occurrence is late.
//!
//! Concurrency across jobs is the one gate that is not here: it is a permit
//! that has to be taken at the moment of running, so [`crate::scheduler`] tries
//! for it after this function says yes and records `skipped_busy` if it cannot
//! get one.
//!
//! # Misfire, in one paragraph
//!
//! When a job comes due while the node is off, asleep, or busy, the plugin
//! **coalesces**: at most one catch-up run, and only if the missed occurrence
//! is younger than `catch_up_grace_secs`. It does not replay the backlog, and
//! there is no setting that makes it. A laptop that wakes after a week owes an
//! `@hourly` job 168 runs; delivering them is a way to melt a machine somebody
//! lent you, and by the time it wakes, 167 of those answers are stale anyway.

use crate::clock::{Zone, naive_at, next_occurrence};
use crate::history::{JobState, PauseReason};
use crate::jobs::{Job, Misfire};

/// How late an occurrence may be discovered and still count as on time.
///
/// The scheduler wakes every `--tick-secs` (20 by default, 300 at most), so a
/// perfectly healthy job is always found a little after it came due. Anything
/// later than this was genuinely missed.
pub const ON_TIME_TOLERANCE_MS: i64 = 90_000;

/// Upper bound on the missed-occurrence count.
///
/// Counting is a scan, and a minutely job that has been down for a month has
/// missed 43,200 occurrences. The number is reported as "at least this many"
/// past the cap rather than paid for.
pub const MAX_COUNTED_MISSES: u32 = 500;

/// First retry delay after a failure.
pub const INITIAL_BACKOFF_MS: i64 = 60_000;
/// Ceiling on the retry delay. A job that has been failing for hours retries
/// hourly, which is enough to notice a recovery and cheap enough to ignore.
pub const MAX_BACKOFF_MS: i64 = 3_600_000;

/// Why a run is starting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunReason {
    /// Due now, found on time.
    Scheduled,
    /// One coalesced run for occurrences missed while the node was unavailable.
    CatchUp { missed: u32 },
    /// Somebody called `run_now`.
    Manual,
}

impl RunReason {
    pub const fn trigger(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::CatchUp { .. } => "catch_up",
            Self::Manual => "manual",
        }
    }
}

/// Why a run is not starting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Blocked {
    /// `enabled = false` in the jobs file.
    FileDisabled,
    Paused {
        reason: PauseReason,
    },
    AlreadyRunning,
    BackingOff {
        until_ms: i64,
        failures: u32,
    },
    OutsideWindow {
        window: String,
    },
    /// The misfire policy is `skip`, and this occurrence was late.
    MisfireSkipped {
        missed: u32,
    },
    /// The misfire policy is `run_once`, but the occurrence is older than the
    /// catch-up grace period.
    MisfireStale {
        missed: u32,
        age_ms: i64,
    },
    /// No permit was free under `max_concurrent_runs`.
    NodeBusy {
        limit: u32,
    },
}

impl Blocked {
    /// Stable code, keyed on by callers and stored in the run history.
    pub fn code(&self) -> &'static str {
        match self {
            Self::FileDisabled => "disabled",
            Self::Paused { .. } => "paused",
            Self::AlreadyRunning => "skipped_overlap",
            Self::BackingOff { .. } => "backing_off",
            Self::OutsideWindow { .. } => "skipped_window",
            Self::MisfireSkipped { .. } => "skipped_misfire",
            Self::MisfireStale { .. } => "skipped_stale",
            Self::NodeBusy { .. } => "skipped_busy",
        }
    }

    /// Whether this block describes an occurrence that came due and was not
    /// run, and so belongs in the job's skip counters.
    ///
    /// A job the file disabled, or one that is paused, has no occurrences to
    /// miss — it is simply off, and `list` says so. Counting a tick of "still
    /// off" every twenty seconds would turn the skip counters into a measure of
    /// uptime rather than of missed work.
    pub const fn counts_as_skip(&self) -> bool {
        !matches!(self, Self::FileDisabled | Self::Paused { .. })
    }

    /// One sentence an operator or a model can act on.
    pub fn message(&self) -> String {
        match self {
            Self::FileDisabled => {
                "this job is disabled in the jobs file. Only the operator can enable it, by \
                 editing the file and restarting the node — a tool cannot."
                    .to_string()
            }
            Self::Paused {
                reason: PauseReason::Requested,
            } => "this job is paused. Call `resume` to let it run again; a restart also clears \
                  the pause, because the jobs file is the only durable statement of intent."
                .to_string(),
            Self::Paused {
                reason: PauseReason::Quarantined,
            } => "this job quarantined itself after too many consecutive failures. Fix the cause \
                  and call `resume`, or restart the node."
                .to_string(),
            Self::AlreadyRunning => {
                "the previous run of this job is still going. A job never overlaps itself, so \
                 this occurrence is skipped rather than queued."
                    .to_string()
            }
            Self::BackingOff { until_ms, failures } => format!(
                "this job has failed {failures} time(s) in a row and is backing off until \
                 {}. Failures back off rather than retrying hot.",
                crate::clock::format_utc(*until_ms)
            ),
            Self::OutsideWindow { window } => format!(
                "this job may only run inside {window}, and it is not that time on this machine. \
                 The window is the operator's statement about when this hardware works."
            ),
            Self::MisfireSkipped { missed } => format!(
                "{missed} occurrence(s) came due while the node was unavailable, and this job's \
                 misfire policy is \"skip\". Waiting for the next scheduled occurrence."
            ),
            Self::MisfireStale { missed, age_ms } => format!(
                "{missed} occurrence(s) came due while the node was unavailable and the most \
                 recent is {} minute(s) old, past this job's catch_up_grace_secs. Running a \
                 stale job is usually worse than not running it.",
                age_ms / 60_000
            ),
            Self::NodeBusy { limit } => format!(
                "max_concurrent_runs is {limit} and every slot is in use. This occurrence is \
                 skipped rather than queued, so a slow job cannot build a backlog."
            ),
        }
    }
}

/// What the scheduler should do with one job on one tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Run(RunReason),
    Skip(Blocked),
    /// Not due. Nothing is recorded.
    Wait,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// The occurrence the job should wait for after this tick.
    pub next_due_ms: Option<i64>,
    /// Occurrences that came due since `next_due_ms` was set, not counting the
    /// one being acted on. Rolled into the job's totals.
    pub missed: u32,
}

/// Everything the decision needs about one job.
pub struct JobView<'a> {
    pub job: &'a Job,
    pub state: &'a JobState,
    pub running: bool,
}

/// Decide what to do with a job on a scheduler tick.
pub fn decide(now_ms: i64, zone: Zone, view: &JobView<'_>) -> Decision {
    let job = view.job;
    let state = view.state;

    if !job.enabled {
        // A disabled job's cursor is left alone: re-enabling it should not be
        // able to produce a catch-up run for the months it was switched off.
        return Decision {
            action: Action::Skip(Blocked::FileDisabled),
            next_due_ms: state.next_due_ms,
            missed: 0,
        };
    }
    if let Some(pause) = &state.pause {
        return Decision {
            action: Action::Skip(Blocked::Paused {
                reason: pause.reason,
            }),
            // Advance the cursor while paused, so resuming does not fire a
            // catch-up run for the pause itself.
            next_due_ms: advance_past(job, zone, now_ms, state.next_due_ms),
            missed: 0,
        };
    }

    let Some(due_ms) = state.next_due_ms else {
        // First sight of this job: establish the cursor and wait.
        return Decision {
            action: Action::Wait,
            next_due_ms: next_occurrence(&job.schedule, zone, now_ms),
            missed: 0,
        };
    };
    if !job.schedule.matches(naive_at(zone, due_ms)) {
        // The cursor was written by a schedule this job no longer has: the
        // operator edited the file and restarted. The file wins, so the cursor
        // is re-derived from now — and the occurrences of the *old* schedule
        // are not treated as missed, because they were never this job's.
        return Decision {
            action: Action::Wait,
            next_due_ms: next_occurrence(&job.schedule, zone, now_ms),
            missed: 0,
        };
    }
    if now_ms < due_ms {
        return Decision {
            action: Action::Wait,
            next_due_ms: Some(due_ms),
            missed: 0,
        };
    }

    // The occurrence is due. Whatever happens next, the cursor moves past now,
    // so nothing is ever attempted twice and nothing queues up.
    let next_due_ms = next_occurrence(&job.schedule, zone, now_ms);
    let missed = count_missed(job, zone, due_ms, now_ms);
    let age_ms = now_ms - due_ms;
    let late = missed > 0 || age_ms > ON_TIME_TOLERANCE_MS;

    let skip = |blocked: Blocked| Decision {
        action: Action::Skip(blocked),
        next_due_ms,
        missed,
    };

    if view.running {
        return skip(Blocked::AlreadyRunning);
    }
    if let Some(until_ms) = state.backoff_until_ms
        && until_ms > now_ms
    {
        return skip(Blocked::BackingOff {
            until_ms,
            failures: state.consecutive_failures,
        });
    }
    if let Some(window) = &job.window
        && !window.contains(zone, now_ms)
    {
        return skip(Blocked::OutsideWindow {
            window: window.to_string(),
        });
    }
    if late {
        match job.misfire {
            Misfire::Skip => return skip(Blocked::MisfireSkipped { missed: missed + 1 }),
            Misfire::RunOnce if age_ms > job.catch_up_grace_ms => {
                return skip(Blocked::MisfireStale {
                    missed: missed + 1,
                    age_ms,
                });
            }
            Misfire::RunOnce => {}
        }
    }

    Decision {
        action: Action::Run(if late {
            RunReason::CatchUp { missed }
        } else {
            RunReason::Scheduled
        }),
        next_due_ms,
        missed,
    }
}

/// Whether `run_now` may run this job, and why not when it may not.
///
/// A manual trigger skips the schedule and the misfire policy — that is what it
/// is for — but not the gates that protect the machine. In particular it does
/// **not** override the window unless the *jobs file* opted that job in with
/// `manual_ignores_window`, and it does not override backoff. A tool argument
/// cannot widen either, because a model can call this tool.
pub fn manual_block(now_ms: i64, zone: Zone, view: &JobView<'_>) -> Option<Blocked> {
    let job = view.job;
    let state = view.state;

    if !job.enabled {
        return Some(Blocked::FileDisabled);
    }
    if let Some(pause) = &state.pause {
        return Some(Blocked::Paused {
            reason: pause.reason,
        });
    }
    if view.running {
        return Some(Blocked::AlreadyRunning);
    }
    if let Some(until_ms) = state.backoff_until_ms
        && until_ms > now_ms
    {
        return Some(Blocked::BackingOff {
            until_ms,
            failures: state.consecutive_failures,
        });
    }
    if !job.manual_ignores_window
        && let Some(window) = &job.window
        && !window.contains(zone, now_ms)
    {
        return Some(Blocked::OutsideWindow {
            window: window.to_string(),
        });
    }
    None
}

/// How long to wait before attempting a job that has failed `failures` times.
///
/// Exponential with full jitter, capped. The jitter is seeded rather than drawn
/// from a global RNG so the range is testable and no random-number dependency
/// is needed; the guarantee callers rely on is that the result lies in
/// `[capped/2, capped]`.
pub fn backoff_ms(failures: u32, seed: u64) -> i64 {
    let exponent = failures.saturating_sub(1).min(16);
    let capped = INITIAL_BACKOFF_MS
        .saturating_mul(1i64 << exponent)
        .min(MAX_BACKOFF_MS);
    let half = capped / 2;
    let spread = (capped - half + 1) as u64;
    half + (xorshift(seed) % spread) as i64
}

fn xorshift(seed: u64) -> u64 {
    let mut state = seed | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

/// Occurrences strictly after `due_ms` and no later than `now_ms`.
fn count_missed(job: &Job, zone: Zone, due_ms: i64, now_ms: i64) -> u32 {
    let mut cursor = due_ms;
    let mut missed = 0;
    while missed < MAX_COUNTED_MISSES {
        match next_occurrence(&job.schedule, zone, cursor) {
            Some(next) if next <= now_ms => {
                missed += 1;
                cursor = next;
            }
            _ => break,
        }
    }
    missed
}

/// The first occurrence strictly after `now`, keeping an existing cursor when
/// it is already in the future.
fn advance_past(job: &Job, zone: Zone, now_ms: i64, current: Option<i64>) -> Option<i64> {
    match current {
        Some(due) if due > now_ms => Some(due),
        _ => next_occurrence(&job.schedule, zone, now_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::HourWindow;
    use crate::config::EnvMap;
    use crate::history::Pause;
    use crate::jobs::parse_jobs;

    /// 2026-03-01T00:00:00Z, a Sunday.
    const NOW: i64 = 1_772_323_200_000;
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 3_600_000;

    fn make_job(schedule: &str, extra: &str) -> Job {
        let text = format!(
            "version = 1\n\
             timezone = \"utc\"\n\
             \n\
             [[job]]\n\
             id = \"digest\"\n\
             schedule = \"{schedule}\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = {{ kind = \"file\", path = \"a.md\" }}\n\
             {extra}"
        );
        parse_jobs(&text, &EnvMap::new(), NOW)
            .unwrap_or_else(|error| panic!("fixture must load: {error}"))
            .jobs
            .pop()
            .expect("one job")
    }

    /// The default fixture: 03:00 UTC every day.
    fn job_with(extra: &str) -> Job {
        make_job("0 3 * * *", extra)
    }

    fn view<'a>(job: &'a Job, state: &'a JobState, running: bool) -> JobView<'a> {
        JobView {
            job,
            state,
            running,
        }
    }

    /// 03:00 UTC on 2026-03-01, the first occurrence of the fixture schedule.
    fn first_due() -> i64 {
        NOW + 3 * HOUR
    }

    #[test]
    fn a_job_the_scheduler_has_never_seen_gets_a_cursor_and_waits() {
        let job = job_with("");
        let state = JobState::default();

        let decision = decide(NOW, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.action, Action::Wait);
        assert_eq!(decision.next_due_ms, Some(first_due()));
    }

    #[test]
    fn a_cursor_from_an_edited_schedule_is_rebuilt_rather_than_obeyed() {
        // The stored cursor is 03:00, which the old daily schedule produced.
        // The file now says seven minutes past every hour, so 03:00 is not an
        // occurrence at all, and waiting for it would delay the new schedule.
        let job = make_job("7 * * * *", "");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(NOW, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.action, Action::Wait);
        assert_eq!(
            decision.next_due_ms,
            Some(NOW + 7 * MINUTE),
            "the file wins over a cursor it did not write"
        );
        assert_eq!(
            decision.missed, 0,
            "occurrences of the previous schedule were never this job's to miss"
        );
    }

    #[test]
    fn a_job_that_is_not_due_waits_without_moving_its_cursor() {
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(NOW + HOUR, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.action, Action::Wait);
        assert_eq!(decision.next_due_ms, Some(first_due()));
    }

    #[test]
    fn a_due_job_runs_and_the_cursor_moves_to_tomorrow() {
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        // Found ten seconds late, which is what a 20-second tick looks like.
        let decision = decide(first_due() + 10_000, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.action, Action::Run(RunReason::Scheduled));
        assert_eq!(decision.missed, 0);
        assert_eq!(decision.next_due_ms, Some(first_due() + 24 * HOUR));
    }

    #[test]
    fn a_job_never_overlaps_itself_and_the_occurrence_is_dropped_not_queued() {
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(first_due(), Zone::Utc, &view(&job, &state, true));

        assert_eq!(decision.action, Action::Skip(Blocked::AlreadyRunning));
        assert_eq!(
            decision.next_due_ms,
            Some(first_due() + 24 * HOUR),
            "the skipped occurrence must not be retried on the next tick"
        );
    }

    #[test]
    fn one_catch_up_run_covers_every_missed_occurrence() {
        // Hourly job, node off for five hours, discovered on wake.
        let job = make_job("0 * * * *", "catch_up_grace_secs = 86400\n");
        let state = JobState {
            next_due_ms: Some(NOW + HOUR),
            ..JobState::default()
        };

        let decision = decide(NOW + 6 * HOUR, Zone::Utc, &view(&job, &state, false));

        match decision.action {
            Action::Run(RunReason::CatchUp { missed }) => assert_eq!(missed, 5),
            other => panic!("expected one catch-up run, got {other:?}"),
        }
        assert_eq!(decision.missed, 5);
        assert_eq!(
            decision.next_due_ms,
            Some(NOW + 7 * HOUR),
            "the backlog is coalesced into one run, not replayed"
        );
    }

    #[test]
    fn a_stale_occurrence_is_not_run_at_all() {
        // Default grace is one hour; this occurrence is nine hours old.
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(
            first_due() + 9 * HOUR,
            Zone::Utc,
            &view(&job, &state, false),
        );

        match decision.action {
            Action::Skip(Blocked::MisfireStale { missed, age_ms }) => {
                assert_eq!(missed, 1);
                assert_eq!(age_ms, 9 * HOUR);
            }
            other => panic!("expected a stale skip, got {other:?}"),
        }
    }

    #[test]
    fn the_skip_policy_never_catches_up_even_inside_the_grace_period() {
        let job = job_with("misfire = \"skip\"\n");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(
            first_due() + 5 * MINUTE,
            Zone::Utc,
            &view(&job, &state, false),
        );

        assert_eq!(
            decision.action,
            Action::Skip(Blocked::MisfireSkipped { missed: 1 })
        );
    }

    #[test]
    fn a_slightly_late_discovery_is_still_an_on_time_run() {
        let job = job_with("misfire = \"skip\"\n");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        // 60 seconds late: inside the tick tolerance, so not a misfire even
        // under the strictest policy.
        let decision = decide(first_due() + 60_000, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.action, Action::Run(RunReason::Scheduled));
    }

    #[test]
    fn a_job_outside_its_window_is_skipped_rather_than_deferred() {
        // Every half hour, but only overnight.
        let job = make_job("*/30 * * * *", "window = \"22:00-06:00\"\n");
        let noon = NOW + 12 * HOUR;
        let state = JobState {
            next_due_ms: Some(noon),
            ..JobState::default()
        };

        let decision = decide(noon, Zone::Utc, &view(&job, &state, false));

        match decision.action {
            Action::Skip(Blocked::OutsideWindow { window }) => assert_eq!(window, "22:00-06:00"),
            other => panic!("expected a window skip, got {other:?}"),
        }
        assert_eq!(decision.next_due_ms, Some(noon + 30 * MINUTE));

        // The same job inside the window runs.
        let night = NOW + 23 * HOUR;
        let state = JobState {
            next_due_ms: Some(night),
            ..JobState::default()
        };
        assert_eq!(
            decide(night, Zone::Utc, &view(&job, &state, false)).action,
            Action::Run(RunReason::Scheduled)
        );
    }

    #[test]
    fn a_backing_off_job_is_skipped_until_the_delay_expires() {
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            consecutive_failures: 3,
            backoff_until_ms: Some(first_due() + 10 * MINUTE),
            ..JobState::default()
        };

        let decision = decide(first_due(), Zone::Utc, &view(&job, &state, false));

        match decision.action {
            Action::Skip(Blocked::BackingOff { failures, .. }) => assert_eq!(failures, 3),
            other => panic!("expected a backoff skip, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_job_is_reported_and_its_cursor_is_left_alone() {
        let job = job_with("enabled = false\n");
        let state = JobState {
            next_due_ms: Some(first_due()),
            ..JobState::default()
        };

        let decision = decide(
            first_due() + 10 * 24 * HOUR,
            Zone::Utc,
            &view(&job, &state, false),
        );

        assert_eq!(decision.action, Action::Skip(Blocked::FileDisabled));
        assert_eq!(
            decision.next_due_ms,
            Some(first_due()),
            "a disabled job's cursor must not be advanced, so nothing is 'missed'"
        );
    }

    #[test]
    fn a_paused_job_does_not_accumulate_a_catch_up_run() {
        let job = job_with("");
        let state = JobState {
            next_due_ms: Some(first_due()),
            pause: Some(Pause {
                reason: PauseReason::Requested,
                at_ms: NOW,
                note: None,
            }),
            ..JobState::default()
        };

        let decision = decide(
            first_due() + 3 * 24 * HOUR,
            Zone::Utc,
            &view(&job, &state, false),
        );

        assert_eq!(
            decision.action,
            Action::Skip(Blocked::Paused {
                reason: PauseReason::Requested
            })
        );
        assert!(
            decision.next_due_ms.expect("a cursor") > first_due() + 3 * 24 * HOUR,
            "resuming must not fire three days of backlog"
        );
    }

    #[test]
    fn a_manual_run_skips_the_schedule_but_not_the_guards() {
        let job = make_job("0 23 * * *", "window = \"22:00-06:00\"\n");
        let noon = NOW + 12 * HOUR;

        let idle = JobState::default();
        match manual_block(noon, Zone::Utc, &view(&job, &idle, false)) {
            Some(Blocked::OutsideWindow { .. }) => {}
            other => panic!("a manual run must honour the window, got {other:?}"),
        }

        // Inside the window it is allowed, with no cursor and no due time.
        assert_eq!(
            manual_block(NOW + 23 * HOUR, Zone::Utc, &view(&job, &idle, false)),
            None
        );

        // And the machine-protecting gates still apply.
        assert_eq!(
            manual_block(NOW + 23 * HOUR, Zone::Utc, &view(&job, &idle, true)),
            Some(Blocked::AlreadyRunning)
        );
        let backing_off = JobState {
            consecutive_failures: 2,
            backoff_until_ms: Some(NOW + 24 * HOUR),
            ..JobState::default()
        };
        assert!(matches!(
            manual_block(NOW + 23 * HOUR, Zone::Utc, &view(&job, &backing_off, false)),
            Some(Blocked::BackingOff { .. })
        ));
    }

    #[test]
    fn the_window_override_lives_in_the_file_not_in_a_tool_argument() {
        let job = make_job(
            "0 23 * * *",
            "window = \"22:00-06:00\"\nmanual_ignores_window = true\n",
        );
        let state = JobState::default();

        assert_eq!(
            manual_block(NOW + 12 * HOUR, Zone::Utc, &view(&job, &state, false)),
            None,
            "the operator opted this job in, in the file"
        );
    }

    #[test]
    fn a_disabled_job_cannot_be_run_by_hand_either() {
        let job = job_with("enabled = false\n");
        let state = JobState::default();

        assert_eq!(
            manual_block(NOW, Zone::Utc, &view(&job, &state, false)),
            Some(Blocked::FileDisabled)
        );
    }

    #[test]
    fn the_missed_count_is_capped_rather_than_scanned_forever() {
        let job = make_job("* * * * *", "");
        let state = JobState {
            next_due_ms: Some(NOW),
            ..JobState::default()
        };

        // A minutely job that has been down for a month.
        let decision = decide(NOW + 30 * 24 * HOUR, Zone::Utc, &view(&job, &state, false));

        assert_eq!(decision.missed, MAX_COUNTED_MISSES);
        assert!(matches!(
            decision.action,
            Action::Skip(Blocked::MisfireStale { .. })
        ));
    }

    #[test]
    fn backoff_grows_stays_inside_its_jitter_band_and_respects_the_cap() {
        for failures in 1..=12u32 {
            for seed in [1u64, 7, 12_345, u64::MAX] {
                let delay = backoff_ms(failures, seed);
                let uncapped = INITIAL_BACKOFF_MS.saturating_mul(1i64 << (failures - 1).min(16));
                let capped = uncapped.min(MAX_BACKOFF_MS);

                assert!(delay >= capped / 2, "failures {failures}: {delay}");
                assert!(delay <= capped, "failures {failures}: {delay}");
                assert!(delay <= MAX_BACKOFF_MS);
            }
        }
        // An absurd failure count must not overflow into a negative delay.
        assert!(backoff_ms(u32::MAX, 5) <= MAX_BACKOFF_MS);
        assert!(backoff_ms(u32::MAX, 5) > 0);
    }

    #[test]
    fn every_block_has_a_stable_code_and_an_actionable_message() {
        let blocks = [
            Blocked::FileDisabled,
            Blocked::Paused {
                reason: PauseReason::Requested,
            },
            Blocked::Paused {
                reason: PauseReason::Quarantined,
            },
            Blocked::AlreadyRunning,
            Blocked::BackingOff {
                until_ms: NOW,
                failures: 2,
            },
            Blocked::OutsideWindow {
                window: "22:00-06:00".into(),
            },
            Blocked::MisfireSkipped { missed: 3 },
            Blocked::MisfireStale {
                missed: 3,
                age_ms: 9 * HOUR,
            },
            Blocked::NodeBusy { limit: 1 },
        ];

        let mut codes = std::collections::BTreeSet::new();
        for block in &blocks {
            assert!(!block.code().is_empty());
            assert!(
                block.message().len() > 30,
                "{}: {}",
                block.code(),
                block.message()
            );
            codes.insert(block.code());
        }
        // Two pause reasons share a code deliberately; everything else is
        // distinct, because callers key on these strings.
        assert_eq!(codes.len(), blocks.len() - 1);
    }

    #[test]
    fn being_switched_off_is_not_a_missed_occurrence() {
        // These two are standing states, reported by `list`. Counting them on
        // every tick would make the skip counters a measure of uptime.
        assert!(!Blocked::FileDisabled.counts_as_skip());
        assert!(
            !Blocked::Paused {
                reason: PauseReason::Requested
            }
            .counts_as_skip()
        );

        // These describe an occurrence that came due and did not run.
        for block in [
            Blocked::AlreadyRunning,
            Blocked::BackingOff {
                until_ms: NOW,
                failures: 1,
            },
            Blocked::OutsideWindow {
                window: "22:00-06:00".into(),
            },
            Blocked::MisfireSkipped { missed: 1 },
            Blocked::MisfireStale {
                missed: 1,
                age_ms: HOUR,
            },
            Blocked::NodeBusy { limit: 1 },
        ] {
            assert!(block.counts_as_skip(), "{}", block.code());
        }
    }

    #[test]
    fn the_window_is_read_on_the_configured_clock() {
        let window = HourWindow::parse("22:00-06:00").expect("valid");

        assert!(window.contains(Zone::Utc, NOW + 23 * HOUR));
        assert!(!window.contains(Zone::Utc, NOW + 12 * HOUR));
    }
}
