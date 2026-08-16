//! The running scheduler: one timer, one lock, and the answers the tools give.
//!
//! # How a tick works
//!
//! Every `--tick-secs` the loop takes the lock once and, for each job, asks
//! [`crate::decide::decide`] what should happen. Everything that follows from
//! that answer — advancing the cursor, counting a skip, taking a concurrency
//! permit, marking the job as running — happens inside that single critical
//! section, and there is no `await` in it. Runs are spawned outside the lock,
//! so a job that takes four minutes never delays a tick, a health check, or a
//! tool call.
//!
//! # Two guarantees the lock gives, cheaply
//!
//! * **A job never overlaps itself.** A job id is in `running` or it is not,
//!   and the decision to run is taken in the same critical section that inserts
//!   it.
//! * **Nothing queues.** A permit is taken with `try_acquire`, never awaited.
//!   An occurrence that cannot get one is recorded as `skipped_busy` and the
//!   cursor moves on. A scheduler that queued would turn a slow evening into a
//!   burst of work at midnight, which is the failure mode this plugin exists to
//!   avoid.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::clock::{Zone, format_in, format_utc, now_ms};
use crate::config::Config;
use crate::decide::{Action, Blocked, JobView, RunReason, backoff_ms, decide, manual_block};
use crate::history::{JobState, Outcome, Pause, PauseReason, RunRecord, Store, truncate_detail};
use crate::jobs::{Job, JobsFile};
use crate::sink::RunPayload;
use crate::{openai, sink};

/// How long `run_now` waits for a run before answering "still running".
///
/// A job's own `timeout_secs` may be an hour, and a tool call that took an hour
/// to answer would be a broken tool. Past this the run continues in the
/// background and its outcome lands in `history`.
pub const RUN_NOW_WAIT: Duration = Duration::from_secs(45);

/// Mutable scheduler state. Guarded by one `std::sync::Mutex`, never held
/// across an `await`.
#[derive(Debug, Default)]
struct Runtime {
    states: BTreeMap<String, JobState>,
    running: BTreeSet<String>,
    ticks: u64,
    last_tick_ms: Option<i64>,
    /// Set when the last attempt to persist run history failed.
    save_error: Option<String>,
}

/// How the jobs file was loaded.
#[derive(Clone, Debug)]
pub struct JobsSource {
    pub file: JobsFile,
    /// The file exists but could not be read or validated. The scheduler does
    /// not start in that case: running half of what the operator wrote is worse
    /// than running none of it, and the error appears on every tool response.
    pub error: Option<String>,
    pub present: bool,
}

pub struct Scheduler {
    config: Arc<Config>,
    source: JobsSource,
    store: Store,
    state: Mutex<Runtime>,
    permits: Arc<Semaphore>,
    completion_client: Client,
    sink_client: Client,
    /// Set once when the tick loop starts, so a re-established control session
    /// cannot start a second one.
    loop_started: AtomicBool,
    /// Reported by `status` when the state file could not be read.
    state_error: Option<String>,
    dropped_unknown_jobs: usize,
    /// Cheap counter read by `health`, which must never take the lock.
    running_now: AtomicU64,
}

impl Scheduler {
    pub fn new(config: Config, source: JobsSource, store: Store) -> Result<Self> {
        let ids: Vec<String> = source.file.jobs.iter().map(|job| job.id.clone()).collect();
        let loaded = store.load(&ids, source.file.history_per_job);

        Ok(Self {
            permits: Arc::new(Semaphore::new(
                source.file.max_concurrent_runs.max(1) as usize
            )),
            state: Mutex::new(Runtime {
                states: loaded.jobs,
                ..Runtime::default()
            }),
            state_error: loaded.error,
            dropped_unknown_jobs: loaded.dropped_unknown_jobs,
            config: Arc::new(config),
            source,
            store,
            completion_client: openai::build_client().context("building the completion client")?,
            sink_client: sink::build_client().context("building the delivery client")?,
            loop_started: AtomicBool::new(false),
            running_now: AtomicU64::new(0),
        })
    }

    fn zone(&self) -> Zone {
        self.source.file.zone
    }

    /// Whether the tick loop may start. False when the jobs file is broken.
    pub fn can_schedule(&self) -> bool {
        self.source.error.is_none()
    }

    /// Claim the right to start the tick loop. Returns true exactly once.
    pub fn claim_loop_slot(&self) -> bool {
        self.loop_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// One line for the host's health probe.
    ///
    /// Reads two atomics and a slice length, so it stays instant whatever a run
    /// or a tick is doing.
    pub fn health_line(&self) -> String {
        if let Some(error) = &self.source.error {
            return format!("jobs file not loaded: {error}");
        }
        let declared = self.source.file.jobs.len();
        let enabled = self
            .source
            .file
            .jobs
            .iter()
            .filter(|job| job.enabled)
            .count();
        format!(
            "{declared} job(s), {enabled} enabled, {} running, endpoint {}",
            self.running_now.load(Ordering::Relaxed),
            self.config.endpoint
        )
    }

    // -----------------------------------------------------------------------
    // The tick
    // -----------------------------------------------------------------------

    /// Start the background loop. Call once; [`Self::claim_loop_slot`] enforces
    /// that.
    pub fn spawn_loop(self: Arc<Self>) {
        let interval = Duration::from_secs(self.config.tick_secs);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skipped ticks are dropped rather than replayed: a stalled
            // scheduler should resume, not fire a burst. Occurrences it missed
            // are handled by the misfire policy, which is the one place that
            // decision belongs.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                Arc::clone(&self).tick(now_ms());
            }
        });
    }

    /// Evaluate every job once. Spawns runs; never blocks on one.
    pub fn tick(self: Arc<Self>, now: i64) {
        let mut to_run: Vec<(usize, RunReason, OwnedSemaphorePermit)> = Vec::new();
        let mut dirty = false;

        {
            let mut runtime = self.state.lock().expect("scheduler state poisoned");
            runtime.ticks += 1;
            runtime.last_tick_ms = Some(now);

            for (index, job) in self.source.file.jobs.iter().enumerate() {
                let running = runtime.running.contains(&job.id);
                let decision = {
                    let state = runtime.states.entry(job.id.clone()).or_default();
                    decide(
                        now,
                        self.source.file.zone,
                        &JobView {
                            job,
                            state,
                            running,
                        },
                    )
                };

                let mut started: Option<(RunReason, OwnedSemaphorePermit)> = None;
                {
                    let state = runtime
                        .states
                        .get_mut(&job.id)
                        .expect("the entry was created above");
                    if state.next_due_ms != decision.next_due_ms {
                        state.next_due_ms = decision.next_due_ms;
                        dirty = true;
                    }
                    if decision.missed > 0 {
                        state.totals.missed_occurrences = state
                            .totals
                            .missed_occurrences
                            .saturating_add(u64::from(decision.missed));
                        dirty = true;
                    }

                    match decision.action {
                        Action::Wait => {}
                        Action::Skip(blocked) => {
                            if blocked.counts_as_skip() {
                                state.last_fire_ms = Some(now);
                                state.record_skip(now, blocked.code(), &blocked.message());
                                dirty = true;
                            }
                        }
                        // Concurrency is the one gate that cannot be decided
                        // from values: it is a permit, and it is taken here,
                        // in the same critical section that marks the job as
                        // running.
                        Action::Run(reason) => {
                            match Arc::clone(&self.permits).try_acquire_owned() {
                                Ok(permit) => {
                                    state.last_fire_ms = Some(now);
                                    started = Some((reason, permit));
                                    dirty = true;
                                }
                                Err(_) => {
                                    let blocked = Blocked::NodeBusy {
                                        limit: self.source.file.max_concurrent_runs,
                                    };
                                    state.last_fire_ms = Some(now);
                                    state.record_skip(now, blocked.code(), &blocked.message());
                                    dirty = true;
                                }
                            }
                        }
                    }
                }

                if let Some((reason, permit)) = started {
                    runtime.running.insert(job.id.clone());
                    to_run.push((index, reason, permit));
                }
            }
        }

        if dirty {
            self.persist();
        }
        for (index, reason, permit) in to_run {
            // The receiver is not needed here: a scheduled run reports through
            // the history rather than to a caller.
            drop(Arc::clone(&self).spawn_run(index, reason, permit));
        }
    }

    /// Run one job on its own task, returning a receiver for its record.
    fn spawn_run(
        self: Arc<Self>,
        index: usize,
        reason: RunReason,
        permit: OwnedSemaphorePermit,
    ) -> oneshot::Receiver<RunRecord> {
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            self.running_now.fetch_add(1, Ordering::Relaxed);
            let record = self.execute(index, reason).await;
            self.running_now.fetch_sub(1, Ordering::Relaxed);
            // The permit is released after the record is written, so a slot is
            // never handed on while this run is still finishing its delivery.
            drop(permit);
            let _ = sender.send(record);
        });
        receiver
    }

    /// The whole of one run: one completion, one delivery, one record.
    async fn execute(&self, index: usize, reason: RunReason) -> RunRecord {
        let job = &self.source.file.jobs[index];
        let started_ms = now_ms();
        let trigger = reason.trigger();

        let outcome = self.attempt(job, trigger, started_ms).await;
        let duration_ms = (now_ms() - started_ms).max(0);

        let record = match outcome {
            Ok((completion, delivered)) => RunRecord {
                started_ms,
                duration_ms,
                outcome: Outcome::Success,
                code: "ok".to_string(),
                trigger: trigger.to_string(),
                model: job.model.clone(),
                output_chars: Some(completion.text.chars().count()),
                prompt_tokens: completion.prompt_tokens,
                completion_tokens: completion.completion_tokens,
                sink: delivered.target,
                detail: delivered
                    .rotated
                    .then(|| "the sink file reached its size cap and was rotated".to_string()),
            },
            Err((code, message)) => RunRecord {
                started_ms,
                duration_ms,
                outcome: Outcome::Failed,
                code: code.to_string(),
                trigger: trigger.to_string(),
                model: job.model.clone(),
                output_chars: None,
                prompt_tokens: None,
                completion_tokens: None,
                sink: job.sink.label(),
                detail: Some(truncate_detail(&message)),
            },
        };

        self.finish_run(&job.id, record.clone(), job.quarantine_after_failures);
        record
    }

    /// The two fallible steps, in order, with a stable code for each failure.
    ///
    /// A completion that cannot be delivered is a **failed run**, not a partial
    /// success: from the operator's side, nothing arrived where they asked for
    /// it, and the job should back off exactly as if the model had refused.
    async fn attempt(
        &self,
        job: &Job,
        trigger: &str,
        started_ms: i64,
    ) -> Result<(openai::Completion, sink::Delivered), (&'static str, String)> {
        let completion = openai::complete(
            &self.completion_client,
            &self.config.completions_url(),
            self.config.api_key.as_ref(),
            job,
        )
        .await
        .map_err(|error| ("endpoint_error", error))?;

        let payload = RunPayload {
            job_id: job.id.clone(),
            trigger: trigger.to_string(),
            model: job.model.clone(),
            answered_by: completion.model.clone(),
            started_ms,
            duration_ms: (now_ms() - started_ms).max(0),
            text: completion.text.clone(),
            prompt_tokens: completion.prompt_tokens,
            completion_tokens: completion.completion_tokens,
        };
        let delivered = sink::deliver(
            &self.sink_client,
            &job.sink,
            &self.config.output_dir,
            &payload,
        )
        .await
        .map_err(|error| ("delivery_error", error))?;

        Ok((completion, delivered))
    }

    /// Record a finished run: history, backoff, and quarantine.
    fn finish_run(&self, job_id: &str, record: RunRecord, quarantine_after: u32) {
        {
            let mut runtime = self.state.lock().expect("scheduler state poisoned");
            runtime.running.remove(job_id);
            let limit = self.source.file.history_per_job;
            let state = runtime.states.entry(job_id.to_string()).or_default();

            match record.outcome {
                Outcome::Success => {
                    state.consecutive_failures = 0;
                    state.backoff_until_ms = None;
                }
                Outcome::Failed => {
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    let delay = backoff_ms(
                        state.consecutive_failures,
                        (record.started_ms as u64) ^ 0x9E37_79B9,
                    );
                    state.backoff_until_ms = Some(record.started_ms.saturating_add(delay));
                    if quarantine_after > 0 && state.consecutive_failures >= quarantine_after {
                        state.pause = Some(Pause {
                            reason: PauseReason::Quarantined,
                            at_ms: record.started_ms,
                            note: Some(format!(
                                "{} consecutive failures; parked so it stops spending GPU time. \
                                 Fix the cause and call `resume`, or restart the node.",
                                state.consecutive_failures
                            )),
                        });
                    }
                }
            }
            state.record_run(record, limit);
        }
        self.persist();
    }

    /// Write run history, recording rather than propagating a failure.
    ///
    /// A node whose disk is full should keep answering prompts; it should not
    /// keep quiet about having stopped recording them, so the error lands in
    /// `status`.
    fn persist(&self) {
        let snapshot = {
            let runtime = self.state.lock().expect("scheduler state poisoned");
            runtime.states.clone()
        };
        let result = self.store.save(&snapshot);
        let mut runtime = self.state.lock().expect("scheduler state poisoned");
        runtime.save_error = result.err().map(|error| error.to_string());
    }

    // -----------------------------------------------------------------------
    // Tools
    // -----------------------------------------------------------------------

    /// Every declared job, with its schedule, its state, and its last run.
    pub fn list(&self) -> Value {
        let now = now_ms();
        let runtime = self.state.lock().expect("scheduler state poisoned");
        let jobs: Vec<Value> = self
            .source
            .file
            .jobs
            .iter()
            .map(|job| {
                self.render_job(
                    job,
                    runtime.states.get(&job.id),
                    runtime.running.contains(&job.id),
                    now,
                )
            })
            .collect();

        json!({
            "jobs_file": self.config.jobs_path.display().to_string(),
            "jobs_file_error": self.source.error,
            "timezone": self.zone().as_str(),
            "max_concurrent_runs": self.source.file.max_concurrent_runs,
            "now_utc": format_utc(now),
            "count": jobs.len(),
            "jobs": jobs,
            "note": "Jobs are declared only in the jobs file. No tool in this plugin can create, \
                     edit, or delete one, and `resume` cannot start a job the file disabled.",
        })
    }

    fn render_job(&self, job: &Job, state: Option<&JobState>, running: bool, now: i64) -> Value {
        let default = JobState::default();
        let state = state.unwrap_or(&default);
        json!({
            "id": job.id,
            "description": job.description,
            "schedule": job.schedule.spec(),
            "window": job.window.map(|window| window.to_string()),
            "model": job.model,
            "sink": job.sink.label(),
            "sink_kind": job.sink.kind(),
            "enabled": job.enabled,
            "paused": state.pause,
            "running": running,
            "misfire": job.misfire.as_str(),
            "catch_up_grace_secs": job.catch_up_grace_ms / 1_000,
            "timeout_secs": job.timeout_secs,
            "next_due_ms": state.next_due_ms,
            "next_due_utc": state.next_due_ms.map(format_utc),
            "next_due_local": state.next_due_ms.map(|ms| format_in(self.zone(), ms)),
            "due_in_secs": state.next_due_ms.map(|ms| (ms - now) / 1_000),
            "consecutive_failures": state.consecutive_failures,
            "backoff_until_utc": state.backoff_until_ms.map(format_utc),
            "last_run": state.last_run(),
            "last_skip": state.last_skip,
            "totals": state.totals,
            "skips_by_reason": state.skips,
        })
    }

    /// What this plugin is configured as, and what it is doing. No network.
    pub fn status(&self) -> Value {
        let runtime = self.state.lock().expect("scheduler state poisoned");
        json!({
            "plugin": crate::config::PLUGIN_NAME,
            "version": crate::config::PLUGIN_VERSION,
            "jobs_file": self.config.jobs_path.display().to_string(),
            "jobs_file_present": self.source.present,
            "jobs_file_error": self.source.error,
            "jobs_declared": self.source.file.jobs.len(),
            "jobs_enabled": self.source.file.jobs.iter().filter(|job| job.enabled).count(),
            "scheduler_running": self.loop_started.load(Ordering::SeqCst),
            "tick_secs": self.config.tick_secs,
            "ticks": runtime.ticks,
            "last_tick_utc": runtime.last_tick_ms.map(format_utc),
            "timezone": self.zone().as_str(),
            "max_concurrent_runs": self.source.file.max_concurrent_runs,
            "running_now": runtime.running.len(),
            "free_slots": self.permits.available_permits(),
            "endpoint": self.config.endpoint.to_string(),
            "endpoint_source": self.config.endpoint_source,
            "endpoint_is_loopback": self.config.endpoint_is_loopback,
            "api_key_configured": self.config.api_key.is_some(),
            "output_dir": self.config.output_dir.display().to_string(),
            "state_file": self.store.path().display().to_string(),
            "state_error": self.state_error,
            "save_error": runtime.save_error,
            "dropped_unknown_jobs": self.dropped_unknown_jobs,
            "history_per_job": self.source.file.history_per_job,
            "note": "The schedule belongs to the operator: jobs come from the jobs file only. \
                     See README.md > Why a model cannot create a job.",
        })
    }

    /// Recent runs, newest first.
    pub fn history(&self, job_id: Option<&str>, limit: Option<u32>) -> Result<Value> {
        let limit = limit.unwrap_or(20).clamp(1, 200) as usize;
        let selected: Vec<&Job> = match job_id {
            Some(id) => vec![self.require_job(id)?],
            None => self.source.file.jobs.iter().collect(),
        };

        let runtime = self.state.lock().expect("scheduler state poisoned");
        let jobs: Vec<Value> = selected
            .into_iter()
            .map(|job| {
                let default = JobState::default();
                let state = runtime.states.get(&job.id).unwrap_or(&default);
                let runs: Vec<&RunRecord> = state.history.iter().rev().take(limit).collect();
                json!({
                    "id": job.id,
                    "runs_shown": runs.len(),
                    "runs": runs,
                    "totals": state.totals,
                    "skips_by_reason": state.skips,
                    "last_skip": state.last_skip,
                })
            })
            .collect();

        Ok(json!({
            "limit": limit,
            "jobs": jobs,
            "note": "Only runs are listed. A skip spends no time, so skips are counted by reason \
                     rather than recorded one by one — see `skips_by_reason`. Model output is \
                     never stored here; it goes to the job's sink.",
        }))
    }

    /// Run one job now, if the guards allow it.
    pub async fn run_now(self: &Arc<Self>, job_id: &str) -> Result<Value> {
        self.require_loaded()?;
        let index = self.job_index(job_id)?;
        let job = &self.source.file.jobs[index];
        let now = now_ms();

        let permit = {
            let mut runtime = self.state.lock().expect("scheduler state poisoned");
            let running = runtime.running.contains(&job.id);
            let blocked = {
                let state = runtime.states.entry(job.id.clone()).or_default();
                manual_block(
                    now,
                    self.source.file.zone,
                    &JobView {
                        job,
                        state,
                        running,
                    },
                )
            };
            if let Some(blocked) = blocked {
                bail!("{} cannot run now: {}", job.id, blocked.message());
            }
            match Arc::clone(&self.permits).try_acquire_owned() {
                Ok(permit) => {
                    runtime.running.insert(job.id.clone());
                    permit
                }
                Err(_) => {
                    let blocked = Blocked::NodeBusy {
                        limit: self.source.file.max_concurrent_runs,
                    };
                    bail!("{} cannot run now: {}", job.id, blocked.message());
                }
            }
        };

        let receiver = Arc::clone(self).spawn_run(index, RunReason::Manual, permit);
        match tokio::time::timeout(RUN_NOW_WAIT, receiver).await {
            Ok(Ok(record)) => Ok(json!({
                "id": job.id,
                "status": "finished",
                "outcome": record.outcome.as_str(),
                "code": record.code,
                "duration_ms": record.duration_ms,
                "output_chars": record.output_chars,
                "completion_tokens": record.completion_tokens,
                "sink": record.sink,
                "detail": record.detail,
            })),
            // The task was dropped without sending, which can only happen while
            // the runtime is shutting down.
            Ok(Err(_)) => bail!("the run task for {} ended without a result", job.id),
            Err(_) => Ok(json!({
                "id": job.id,
                "status": "running",
                "waited_secs": RUN_NOW_WAIT.as_secs(),
                "detail": format!(
                    "still running after {}s; this tool does not hold the connection open for a \
                     whole job. Read `history` with job_id = \"{}\" for the outcome.",
                    RUN_NOW_WAIT.as_secs(),
                    job.id
                ),
            })),
        }
    }

    /// Stop a job running until it is resumed or the node restarts.
    pub fn pause(&self, job_id: &str, note: Option<&str>) -> Result<Value> {
        self.require_loaded()?;
        let job = self.require_job(job_id)?;
        if !job.enabled {
            bail!(
                "{} is already disabled in the jobs file; there is nothing to pause",
                job.id
            );
        }

        let at_ms = now_ms();
        let note = note
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(truncate_detail);
        {
            let mut runtime = self.state.lock().expect("scheduler state poisoned");
            let state = runtime.states.entry(job.id.clone()).or_default();
            state.pause = Some(Pause {
                reason: PauseReason::Requested,
                at_ms,
                note: note.clone(),
            });
        }

        Ok(json!({
            "id": job.id,
            "paused": true,
            "at_utc": format_utc(at_ms),
            "note": note,
            "detail": "A run already in flight is not cancelled; the pause takes effect from the \
                       next occurrence. It is not written to disk, so restarting the node clears \
                       it — the jobs file is the only durable statement of what runs.",
        }))
    }

    /// Let a paused or quarantined job run again.
    pub fn resume(&self, job_id: &str) -> Result<Value> {
        self.require_loaded()?;
        let job = self.require_job(job_id)?;
        if !job.enabled {
            // The one thing `resume` deliberately cannot do.
            bail!(
                "{} is disabled in the jobs file, and `resume` cannot start a job the operator \
                 switched off. Set enabled = true in {} and restart the node.",
                job.id,
                self.config.jobs_path.display()
            );
        }

        let previous = {
            let mut runtime = self.state.lock().expect("scheduler state poisoned");
            let state = runtime.states.entry(job.id.clone()).or_default();
            let previous = state.pause.take();
            if previous
                .as_ref()
                .is_some_and(|pause| pause.reason == PauseReason::Quarantined)
            {
                // Clearing the quarantine clears the backoff with it; otherwise
                // the operator fixes the cause and still waits an hour.
                state.consecutive_failures = 0;
                state.backoff_until_ms = None;
            }
            previous
        };

        Ok(json!({
            "id": job.id,
            "resumed": previous.is_some(),
            "was": previous,
            "detail": match previous {
                Some(_) => "The job runs again from its next scheduled occurrence. Occurrences \
                            missed while it was paused are not replayed.",
                None => "This job was not paused; nothing changed.",
            },
        }))
    }

    fn require_loaded(&self) -> Result<()> {
        match &self.source.error {
            Some(error) => Err(anyhow!(
                "the jobs file at {} did not load, so no job is scheduled: {error}",
                self.config.jobs_path.display()
            )),
            None => Ok(()),
        }
    }

    fn job_index(&self, job_id: &str) -> Result<usize> {
        self.source
            .file
            .jobs
            .iter()
            .position(|job| job.id == job_id)
            .ok_or_else(|| self.unknown_job(job_id))
    }

    fn require_job(&self, job_id: &str) -> Result<&Job> {
        self.source
            .file
            .job(job_id)
            .ok_or_else(|| self.unknown_job(job_id))
    }

    fn unknown_job(&self, job_id: &str) -> anyhow::Error {
        let known: Vec<&str> = self
            .source
            .file
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect();
        if known.is_empty() {
            anyhow!(
                "there is no job \"{job_id}\"; no jobs are declared in {}. Jobs are written in \
                 that file by the operator — no tool can create one.",
                self.config.jobs_path.display()
            )
        } else {
            anyhow!(
                "there is no job \"{job_id}\". Declared jobs: {}.",
                known.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvMap;
    use crate::jobs::parse_jobs;
    use std::path::{Path, PathBuf};

    /// 2026-03-01T00:00:00Z.
    const NOW: i64 = 1_772_323_200_000;
    const HOUR: i64 = 3_600_000;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tdcc-scheduled-prompts-sched-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config_for(dir: &Path) -> Config {
        Config::parse(
            &[
                "--state-dir".to_string(),
                dir.display().to_string(),
                "--output-dir".to_string(),
                dir.join("out").display().to_string(),
            ],
            &EnvMap::from([("HOME".to_string(), "/home/tester".to_string())]),
        )
        .expect("config parses")
    }

    fn build(dir: &Path, jobs_text: &str) -> Arc<Scheduler> {
        let file = parse_jobs(jobs_text, &EnvMap::new(), NOW).expect("fixture loads");
        Arc::new(
            Scheduler::new(
                config_for(dir),
                JobsSource {
                    file,
                    error: None,
                    present: true,
                },
                Store::new(dir),
            )
            .expect("scheduler builds"),
        )
    }

    fn scheduler(tag: &str, jobs_text: &str) -> (Arc<Scheduler>, PathBuf) {
        let dir = scratch(tag);
        let scheduler = build(&dir, jobs_text);
        (scheduler, dir)
    }

    fn two_jobs() -> &'static str {
        "version = 1\n\
         timezone = \"utc\"\n\
         \n\
         [[job]]\n\
         id = \"digest\"\n\
         description = \"Nightly digest\"\n\
         schedule = \"0 3 * * *\"\n\
         model = \"qwen3:8b\"\n\
         prompt = \"Summarise.\"\n\
         sink = { kind = \"file\", path = \"digest.md\" }\n\
         \n\
         [[job]]\n\
         id = \"off\"\n\
         schedule = \"@hourly\"\n\
         enabled = false\n\
         model = \"qwen3:8b\"\n\
         prompt = \"Never.\"\n\
         sink = { kind = \"file\", path = \"off.md\" }\n"
    }

    #[test]
    fn list_reports_every_declared_job_and_says_where_jobs_come_from() {
        let (scheduler, dir) = scheduler("list", two_jobs());

        let listed = scheduler.list();

        assert_eq!(listed["count"], 2);
        assert_eq!(listed["jobs"][0]["id"], "digest");
        assert_eq!(listed["jobs"][0]["description"], "Nightly digest");
        assert_eq!(listed["jobs"][0]["schedule"], "0 3 * * *");
        assert_eq!(listed["jobs"][0]["sink"], "file:digest.md (text)");
        assert_eq!(listed["jobs"][1]["enabled"], false);
        let note = listed["note"].as_str().expect("a note");
        assert!(note.contains("No tool"), "{note}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tick_gives_every_enabled_job_a_cursor_without_running_anything() {
        let (scheduler, dir) = scheduler("tick", two_jobs());

        Arc::clone(&scheduler).tick(NOW);

        let listed = scheduler.list();
        assert_eq!(listed["jobs"][0]["next_due_utc"], "2026-03-01T03:00:00Z");
        assert_eq!(scheduler.status()["ticks"], 1);
        // A disabled job never gets a cursor, so enabling it later cannot
        // produce a catch-up run for the months it was off.
        assert_eq!(listed["jobs"][1]["next_due_utc"], Value::Null);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_skip_is_counted_by_reason_rather_than_filling_the_run_history() {
        let (scheduler, dir) = scheduler(
            "skip",
            "version = 1\n\
             timezone = \"utc\"\n\
             [[job]]\n\
             id = \"halfhourly\"\n\
             schedule = \"*/30 * * * *\"\n\
             window = \"22:00-06:00\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\" }\n",
        );

        // Establish the cursor at midday, then tick through the afternoon, when
        // the window is closed.
        Arc::clone(&scheduler).tick(NOW + 12 * HOUR);
        for step in 1..=6 {
            Arc::clone(&scheduler).tick(NOW + 12 * HOUR + step * 30 * 60_000);
        }

        let listed = scheduler.list();
        let job = &listed["jobs"][0];
        assert_eq!(job["totals"]["skipped"], 6);
        assert_eq!(job["skips_by_reason"]["skipped_window"], 6);
        assert_eq!(job["totals"]["attempts"], 0, "nothing was run");
        assert_eq!(
            job["last_skip"]["code"], "skipped_window",
            "the most recent skip is kept in full"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_written_by_a_tick_is_read_back_on_the_next_start() {
        let dir = scratch("persist");
        let scheduler = build(&dir, two_jobs());
        Arc::clone(&scheduler).tick(NOW);
        drop(scheduler);

        let reloaded = build(&dir, two_jobs());

        assert_eq!(
            reloaded.list()["jobs"][0]["next_due_utc"],
            "2026-03-01T03:00:00Z",
            "the cursor survives a restart"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pause_and_resume_move_a_job_in_and_out_of_the_schedule() {
        let (scheduler, dir) = scheduler("pause", two_jobs());

        let paused = scheduler
            .pause("digest", Some("noisy neighbour"))
            .expect("pauses");
        assert_eq!(paused["paused"], true);
        assert_eq!(scheduler.list()["jobs"][0]["paused"]["reason"], "requested");

        let resumed = scheduler.resume("digest").expect("resumes");
        assert_eq!(resumed["resumed"], true);
        assert_eq!(scheduler.list()["jobs"][0]["paused"], Value::Null);

        // Resuming a job that was not paused is honest about having done
        // nothing rather than reporting a success.
        assert_eq!(
            scheduler.resume("digest").expect("answers")["resumed"],
            false
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_cannot_start_a_job_the_file_disabled() {
        let (scheduler, dir) = scheduler("enable", two_jobs());

        let error = scheduler
            .resume("off")
            .expect_err("must refuse")
            .to_string();

        assert!(error.contains("disabled in the jobs file"), "{error}");
        assert!(error.contains("enabled = true"), "{error}");
        // And pausing it is refused too, rather than pretending to work.
        assert!(scheduler.pause("off", None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_disabled_job_cannot_be_triggered_by_hand() {
        let (scheduler, dir) = scheduler("manual", two_jobs());

        let error = scheduler
            .run_now("off")
            .await
            .expect_err("must refuse")
            .to_string();

        assert!(error.contains("disabled in the jobs file"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unknown_job_id_lists_the_ones_that_exist() {
        let (scheduler, dir) = scheduler("unknown", two_jobs());

        let error = scheduler
            .run_now("typo")
            .await
            .expect_err("must refuse")
            .to_string();

        assert!(error.contains("no job \"typo\""), "{error}");
        assert!(error.contains("digest"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jobs_file_that_did_not_load_fails_every_tool_rather_than_running_half_of_it() {
        let dir = scratch("broken");
        let scheduler = Scheduler::new(
            config_for(&dir),
            JobsSource {
                file: JobsFile::empty(),
                error: Some("line 4: unknown field `scheduel`".to_string()),
                present: true,
            },
            Store::new(&dir),
        )
        .expect("scheduler builds");

        assert!(!scheduler.can_schedule());
        assert!(scheduler.health_line().contains("not loaded"));
        assert!(scheduler.status()["jobs_file_error"].is_string());
        assert!(scheduler.pause("anything", None).is_err());
        assert!(scheduler.resume("anything").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_answers_without_touching_the_network() {
        let (scheduler, dir) = scheduler("status", two_jobs());

        let status = scheduler.status();

        assert_eq!(status["plugin"], "scheduled-prompts");
        assert_eq!(status["jobs_declared"], 2);
        assert_eq!(status["jobs_enabled"], 1);
        assert_eq!(status["max_concurrent_runs"], 1);
        assert_eq!(status["free_slots"], 1);
        assert_eq!(status["endpoint_is_loopback"], true);
        assert_eq!(status["api_key_configured"], false);
        assert_eq!(status["scheduler_running"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_answers_before_anything_has_run() {
        let (scheduler, dir) = scheduler("history", two_jobs());

        let all = scheduler.history(None, None).expect("answers");
        assert_eq!(all["jobs"].as_array().expect("array").len(), 2);
        assert_eq!(all["jobs"][0]["runs_shown"], 0);

        let one = scheduler.history(Some("digest"), Some(5)).expect("answers");
        assert_eq!(one["jobs"].as_array().expect("array").len(), 1);
        assert_eq!(one["limit"], 5);
        assert!(scheduler.history(Some("nope"), None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loop_slot_can_only_be_claimed_once() {
        let (scheduler, dir) = scheduler("slot", two_jobs());

        assert!(scheduler.claim_loop_slot());
        assert!(!scheduler.claim_loop_slot());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_run_backs_off_and_eventually_quarantines_itself() {
        let (scheduler, dir) = scheduler("backoff", two_jobs());
        let failure = |started_ms: i64| RunRecord {
            started_ms,
            duration_ms: 10,
            outcome: Outcome::Failed,
            code: "endpoint_error".into(),
            trigger: "scheduled".into(),
            model: "qwen3:8b".into(),
            output_chars: None,
            prompt_tokens: None,
            completion_tokens: None,
            sink: "file:digest.md (text)".into(),
            detail: Some("unreachable".into()),
        };

        scheduler.finish_run("digest", failure(NOW), 3);
        let listed = scheduler.list();
        assert_eq!(listed["jobs"][0]["consecutive_failures"], 1);
        assert!(
            listed["jobs"][0]["backoff_until_utc"].is_string(),
            "a failure must schedule a delay rather than retrying hot"
        );
        assert_eq!(listed["jobs"][0]["paused"], Value::Null);

        scheduler.finish_run("digest", failure(NOW + 1), 3);
        scheduler.finish_run("digest", failure(NOW + 2), 3);
        let listed = scheduler.list();
        assert_eq!(listed["jobs"][0]["consecutive_failures"], 3);
        assert_eq!(
            listed["jobs"][0]["paused"]["reason"], "quarantined",
            "a job that only fails parks itself"
        );

        // Resuming clears both the quarantine and the delay it earned.
        scheduler.resume("digest").expect("resumes");
        let listed = scheduler.list();
        assert_eq!(listed["jobs"][0]["consecutive_failures"], 0);
        assert_eq!(listed["jobs"][0]["backoff_until_utc"], Value::Null);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_run_clears_the_failure_streak() {
        let (scheduler, dir) = scheduler("recover", two_jobs());
        scheduler.finish_run(
            "digest",
            RunRecord {
                started_ms: NOW,
                duration_ms: 10,
                outcome: Outcome::Failed,
                code: "endpoint_error".into(),
                trigger: "scheduled".into(),
                model: "qwen3:8b".into(),
                output_chars: None,
                prompt_tokens: None,
                completion_tokens: None,
                sink: "file:digest.md (text)".into(),
                detail: None,
            },
            0,
        );
        scheduler.finish_run(
            "digest",
            RunRecord {
                started_ms: NOW + 1,
                duration_ms: 20,
                outcome: Outcome::Success,
                code: "ok".into(),
                trigger: "scheduled".into(),
                model: "qwen3:8b".into(),
                output_chars: Some(12),
                prompt_tokens: Some(3),
                completion_tokens: Some(4),
                sink: "file:digest.md (text)".into(),
                detail: None,
            },
            0,
        );

        let listed = scheduler.list();
        assert_eq!(listed["jobs"][0]["consecutive_failures"], 0);
        assert_eq!(listed["jobs"][0]["backoff_until_utc"], Value::Null);
        assert_eq!(listed["jobs"][0]["totals"]["succeeded"], 1);
        assert_eq!(listed["jobs"][0]["totals"]["failed"], 1);
        assert_eq!(listed["jobs"][0]["last_run"]["outcome"], "success");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
