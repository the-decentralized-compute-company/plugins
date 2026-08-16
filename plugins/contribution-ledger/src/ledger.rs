//! The ledger itself: an in-memory accumulator over the open bucket, an
//! in-memory copy of the sealed history, and the aggregation that tool
//! responses are built from.
//!
//! History is read from disk exactly once, at startup, and kept in memory
//! afterwards. Fourteen days of hourly rows plus four hundred days of daily
//! rows is under a thousand records, so "aggregate on read" costs a walk over a
//! small `Vec` and never a file read. Disk is touched only when a bucket seals
//! (about once an hour) and when retention actually has something to do (about
//! once a day).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::clock::{DAY_MS, Granularity, HOUR_MS, bucket_start, format_utc, now_ms};
use crate::config::{LedgerConfig, StatusSource};
use crate::journal::{
    Counters, Cumulative, Cursor, EpochRecord, Journal, MAX_PEERS_PER_EPOCH, Polls, RECORD_VERSION,
    Record, SessionRecord, apply_retention,
};
use crate::source::StatusSample;

/// How much of a peer id is written down.
///
/// Shortening keeps the file from being a tidy export of full mesh identities
/// and keeps rows readable. It is **not** anonymisation: a 16-character prefix
/// still distinguishes peers, and anyone with the mesh's peer list can match it
/// back. `--no-peer-ids` is the switch for operators who want none of this
/// recorded at all.
pub const PEER_ID_PREFIX_CHARS: usize = 16;

/// Default window for the reading tools.
pub const DEFAULT_WINDOW_DAYS: u32 = 7;
pub const MAX_WINDOW_DAYS: u32 = 3_650;

/// Rows returned by `epochs` / `peers` when the caller does not ask.
pub const DEFAULT_ROW_LIMIT: u32 = 50;
pub const MAX_ROW_LIMIT: u32 = 1_000;

/// Retention runs at most this often; it is a whole-file rewrite.
const RETENTION_INTERVAL_MS: u64 = 6 * HOUR_MS;

/// Force a compaction regardless of the interval once the journal gets this
/// big. A well-behaved journal never reaches it; this is the backstop for a
/// clock that jumped or a policy that was widened and then narrowed.
const JOURNAL_SIZE_TRIGGER_BYTES: u64 = 8 * 1024 * 1024;

/// The stated purpose of this file, repeated in every summary so it cannot be
/// quoted out of context.
pub const DISCLAIMER: &str = "Self-reported local record. This node counted its own work from its \
     own host counters. It is not a balance, a credit, a currency, or a claim on anything, it is \
     not settled or settleable, and it is not evidence to any third party.";

/// The bucket currently being filled. Nothing here has been written yet.
#[derive(Clone, Debug)]
struct OpenEpoch {
    start_ms: u64,
    observed_ms: u64,
    accepting_ms: u64,
    counters: Counters,
    peers: BTreeSet<String>,
    peers_truncated: bool,
    polls: Polls,
    last_tick_ms: u64,
}

impl OpenEpoch {
    fn new(start_ms: u64, now_ms: u64) -> Self {
        Self {
            start_ms,
            observed_ms: 0,
            accepting_ms: 0,
            counters: Counters::default(),
            peers: BTreeSet::new(),
            peers_truncated: false,
            polls: Polls::default(),
            last_tick_ms: now_ms,
        }
    }

    fn is_empty(&self) -> bool {
        self.observed_ms == 0
            && self.counters.is_zero()
            && self.polls == Polls::default()
            && self.peers.is_empty()
    }

    fn snapshot(&self) -> EpochRecord {
        EpochRecord {
            v: RECORD_VERSION,
            granularity: Granularity::Hour,
            start_ms: self.start_ms,
            end_ms: self.start_ms + HOUR_MS,
            observed_ms: self.observed_ms,
            accepting_ms: self.accepting_ms,
            counters: self.counters,
            peers: self.peers.iter().cloned().collect(),
            peers_truncated: self.peers_truncated,
            polls: self.polls,
        }
    }

    fn note_peer(&mut self, peer_id: &str) {
        let short: String = peer_id.chars().take(PEER_ID_PREFIX_CHARS).collect();
        if short.is_empty() || self.peers.contains(&short) {
            return;
        }
        if self.peers.len() >= MAX_PEERS_PER_EPOCH {
            self.peers_truncated = true;
            return;
        }
        self.peers.insert(short);
    }
}

/// Everything the `status` tool needs in order to be honest about what the
/// ledger could and could not see.
#[derive(Clone, Debug, Default)]
struct Health {
    /// From `local_accepting` / `local_standby` mesh events, falling back to
    /// the polled `node_state` until one arrives.
    accepting: Option<bool>,
    node_id: Option<String>,
    mesh_id: Option<String>,
    started_ms: u64,
    polls_ok: u64,
    polls_failed: u64,
    last_poll_ok_ms: Option<u64>,
    last_poll_error: Option<String>,
    /// Counter fields the last poll could not find in the host payload.
    missing_fields: Vec<String>,
    last_journal_error: Option<String>,
    unreadable_lines: usize,
    truncated_tail: bool,
    last_retention_ms: u64,
}

struct State {
    /// Sealed buckets and session markers, mirroring the journal on disk.
    records: Vec<Record>,
    open: OpenEpoch,
    /// Last cumulative reading, used to difference the next one.
    cursor: Option<Cumulative>,
    health: Health,
}

/// Shared, cloneable handle. Every tool handler and the background sampler hold
/// an `Arc` of one of these.
pub struct Ledger {
    config: LedgerConfig,
    journal: Journal,
    state: Mutex<State>,
    /// Lock-free mirrors so the `health` hook stays independent of whatever the
    /// sampler is doing.
    journal_ok: AtomicBool,
    source_ok: AtomicBool,
    sampler_started: AtomicBool,
}

impl Ledger {
    /// Load history, adopt the resume cursor, and mark the start of a session.
    pub fn open(config: LedgerConfig) -> Result<Self> {
        let journal = Journal::new(config.state_dir.clone());
        journal.ensure_dir()?;
        let loaded = journal.load()?;
        let cursor = journal.load_cursor();
        let started_ms = now_ms();

        let note = match (&config.source, cursor.is_some()) {
            (StatusSource::Endpoint(_), true) => {
                "ledger started; resuming host counters from the stored cursor"
            }
            (StatusSource::Endpoint(_), false) => {
                "ledger started; no cursor, so the first poll only establishes a baseline"
            }
            (StatusSource::OptedOut, _) => {
                "ledger started with host counter sampling disabled (--no-host-api)"
            }
            (StatusSource::Unset, _) => {
                "ledger started with no host API configured; counters are not being sampled"
            }
        };

        let health = Health {
            started_ms,
            unreadable_lines: loaded.unreadable_lines,
            truncated_tail: loaded.truncated_tail,
            last_retention_ms: started_ms,
            ..Health::default()
        };

        let session = Record::Session(SessionRecord {
            v: RECORD_VERSION,
            at_ms: started_ms,
            plugin_version: crate::PLUGIN_VERSION.to_string(),
            note: note.to_string(),
        });
        journal.append(&session)?;

        let mut records = loaded.records;
        records.push(session);

        let ledger = Self {
            config,
            journal,
            state: Mutex::new(State {
                records,
                open: OpenEpoch::new(bucket_start(started_ms, Granularity::Hour), started_ms),
                cursor: cursor.map(|cursor| cursor.cumulative),
                health,
            }),
            journal_ok: AtomicBool::new(true),
            source_ok: AtomicBool::new(false),
            sampler_started: AtomicBool::new(false),
        };
        // Compact on the way in as well as on the way through: a node that
        // restarts often may never survive long enough to seal a bucket.
        ledger.run_retention(started_ms, true);
        Ok(ledger)
    }

    pub fn config(&self) -> &LedgerConfig {
        &self.config
    }

    /// True the first time it is called, so `on_initialized` can spawn the
    /// sampler exactly once even if the host re-runs the hook.
    pub fn claim_sampler_slot(&self) -> bool {
        !self.sampler_started.swap(true, Ordering::SeqCst)
    }

    /// A poisoned lock means a handler panicked mid-update. The state has no
    /// cross-field invariant a partial update could break — the worst case is
    /// one bucket with a slightly wrong count — so recovering keeps the ledger
    /// usable instead of failing every later call.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One sampler iteration: advance observed time, fold in the poll result,
    /// and seal the bucket if the hour rolled over.
    pub fn tick(&self, now: u64, poll: Option<Result<StatusSample>>) {
        let max_gap = max_observed_gap_ms(self.config.poll_secs);

        let sealed = {
            let mut state = self.lock();
            let elapsed = observed_increment(state.open.last_tick_ms, now, max_gap);
            state.open.observed_ms = state.open.observed_ms.saturating_add(elapsed);
            if state.health.accepting.unwrap_or(false) {
                state.open.accepting_ms = state.open.accepting_ms.saturating_add(elapsed);
            }
            state.open.last_tick_ms = now;

            match poll {
                Some(Ok(sample)) => self.apply_sample(&mut state, sample, now),
                Some(Err(error)) => {
                    state.open.polls.failed = state.open.polls.failed.saturating_add(1);
                    state.health.polls_failed = state.health.polls_failed.saturating_add(1);
                    state.health.last_poll_error = Some(error.to_string());
                    self.source_ok.store(false, Ordering::Relaxed);
                }
                None => {}
            }

            // The interval that straddles an hour boundary is attributed to the
            // bucket it started in, so a delta is never split across two rows.
            // The error is bounded by one poll interval.
            let bucket = bucket_start(now, Granularity::Hour);
            if bucket == state.open.start_ms {
                None
            } else {
                let sealed = (!state.open.is_empty()).then(|| state.open.snapshot());
                state.open = OpenEpoch::new(bucket, now);
                sealed
            }
        };

        if let Some(record) = sealed {
            self.persist(record, now);
        }
    }

    fn apply_sample(&self, state: &mut State, sample: StatusSample, now: u64) {
        if let Some(previous) = state.cursor {
            let (delta, reset) = sample.cumulative.delta_since(&previous);
            state.open.counters.add(&delta);
            if reset {
                state.open.polls.counter_resets = state.open.polls.counter_resets.saturating_add(1);
            }
        }
        // Without a previous reading there is nothing to difference: the host's
        // running totals predate this ledger, so the first poll only sets the
        // baseline and contributes no work.
        state.cursor = Some(sample.cumulative);

        state.open.polls.ok = state.open.polls.ok.saturating_add(1);
        state.health.polls_ok = state.health.polls_ok.saturating_add(1);
        state.health.last_poll_ok_ms = Some(now);
        state.health.last_poll_error = None;
        state.health.missing_fields = sample
            .missing
            .iter()
            .map(|field| field.to_string())
            .collect();
        self.source_ok.store(true, Ordering::Relaxed);

        if sample.node_id.is_some() {
            state.health.node_id = sample.node_id.clone();
        }
        if sample.mesh_id.is_some() {
            state.health.mesh_id = sample.mesh_id.clone();
        }
        // Mesh events are authoritative for the serving flag; the polled state
        // only fills in until the first one arrives.
        if state.health.accepting.is_none() {
            state.health.accepting = sample.accepting();
        }
        if self.config.record_peers {
            for peer in &sample.peers {
                state.open.note_peer(peer);
            }
        }
    }

    /// Fold a mesh lifecycle event into the open bucket.
    ///
    /// Presence and serving-availability only. The host does not deliver
    /// per-request events to plugins, so nothing here counts work.
    pub fn note_mesh_event(
        &self,
        kind: MeshEventKind,
        peer_id: Option<&str>,
        local_peer_id: Option<&str>,
        mesh_id: Option<&str>,
    ) {
        let mut state = self.lock();
        match kind {
            MeshEventKind::PeerUp | MeshEventKind::PeerDown => {
                if self.config.record_peers
                    && let Some(peer_id) = peer_id
                {
                    state.open.note_peer(peer_id);
                }
            }
            MeshEventKind::LocalAccepting => state.health.accepting = Some(true),
            MeshEventKind::LocalStandby => state.health.accepting = Some(false),
            MeshEventKind::MeshIdUpdated => {}
        }
        if let Some(mesh_id) = mesh_id.map(str::trim).filter(|id| !id.is_empty()) {
            state.health.mesh_id = Some(mesh_id.to_string());
        }
        if let Some(local) = local_peer_id.map(str::trim).filter(|id| !id.is_empty()) {
            state.health.node_id = Some(local.to_string());
        }
    }

    /// Append a sealed bucket, refresh the cursor, and run retention if due.
    fn persist(&self, record: EpochRecord, now: u64) {
        let cursor = self.lock().cursor;
        let mut errors: Vec<String> = Vec::new();

        if let Err(error) = self.journal.append(&Record::Epoch(record.clone())) {
            errors.push(format!("append: {error:#}"));
        }
        if let Err(error) = self.advance_cursor(cursor, now) {
            errors.push(format!("cursor: {error:#}"));
        }

        {
            let mut state = self.lock();
            state.records.push(Record::Epoch(record));
            if errors.is_empty() {
                state.health.last_journal_error = None;
            } else {
                state.health.last_journal_error = Some(errors.join("; "));
            }
        }
        self.journal_ok.store(errors.is_empty(), Ordering::Relaxed);

        self.run_retention(now, false);
    }

    /// Move the on-disk resume point up to the last reading folded into a
    /// bucket that is now durable.
    ///
    /// This has to happen on **every** path that writes a bucket, including
    /// `flush`. A bucket on disk whose work is still ahead of the cursor would
    /// be counted a second time after a restart, because the next delta would
    /// span it again.
    fn advance_cursor(&self, cursor: Option<Cumulative>, now: u64) -> Result<()> {
        let Some(cumulative) = cursor else {
            return Ok(());
        };
        self.journal.store_cursor(&Cursor {
            v: RECORD_VERSION,
            at_ms: now,
            cumulative,
        })
    }

    /// Merge aged buckets and drop expired ones.
    ///
    /// Runs entirely under the state lock, including the file rewrite. The
    /// rewrite is a few hundred kilobytes at most and happens about once a day,
    /// and holding the lock removes any chance of a bucket sealed mid-rewrite
    /// going missing from either memory or disk.
    ///
    /// `force` is used at startup. Without it, a plugin that crash-looped
    /// before its first bucket seal would append a session marker per restart
    /// and never reach the periodic check.
    fn run_retention(&self, now: u64, force: bool) {
        let mut state = self.lock();
        let elapsed = now.saturating_sub(state.health.last_retention_ms);
        if !force
            && elapsed < RETENTION_INTERVAL_MS
            && self.journal.journal_bytes() < JOURNAL_SIZE_TRIGGER_BYTES
        {
            return;
        }

        let original = state.records.clone();
        let retention = apply_retention(
            original.clone(),
            now,
            self.config.compact_after_days,
            self.config.retain_days,
        );
        if !retention.changed {
            state.health.last_retention_ms = now;
            return;
        }

        match self.journal.rewrite(&retention.records) {
            Ok(()) => {
                state.records = retention.records;
                state.health.last_retention_ms = now;
                state.health.last_journal_error = None;
                self.journal_ok.store(true, Ordering::Relaxed);
            }
            Err(error) => {
                // The temp file never replaced the journal, so disk still holds
                // `original`. Keep memory matching it and retry next time.
                state.records = original;
                state.health.last_journal_error = Some(format!("compaction: {error:#}"));
                self.journal_ok.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Seal whatever is in the open bucket right now.
    ///
    /// Called by the `flush` tool and worth calling before reading the raw file
    /// or stopping the node; nothing else forces a partial hour to disk. The
    /// whole operation holds the lock, so a failed write leaves the open bucket
    /// exactly as it was rather than discarding it.
    pub fn flush(&self) -> Result<Value> {
        let now = now_ms();
        let mut state = self.lock();
        if state.open.is_empty() {
            return Ok(json!({
                "flushed": false,
                "reason": "the open bucket is empty; nothing to write",
                "journal": self.journal.journal_path().display().to_string(),
            }));
        }

        let record = state.open.snapshot();
        if let Err(error) = self.journal.append(&Record::Epoch(record.clone())) {
            state.health.last_journal_error = Some(format!("flush: {error:#}"));
            self.journal_ok.store(false, Ordering::Relaxed);
            return Err(error);
        }

        state.open = OpenEpoch::new(bucket_start(now, Granularity::Hour), now);
        state.records.push(Record::Epoch(record.clone()));
        // The flushed bucket is durable, so the resume point has to move with
        // it or a restart would count this work again.
        state.health.last_journal_error = match self.advance_cursor(state.cursor, now) {
            Ok(()) => None,
            Err(error) => Some(format!("flush cursor: {error:#}")),
        };
        self.journal_ok
            .store(state.health.last_journal_error.is_none(), Ordering::Relaxed);

        Ok(json!({
            "flushed": true,
            "journal": self.journal.journal_path().display().to_string(),
            "bucket": epoch_json(&record),
        }))
    }

    /// Short, allocation-light health string. Reads only atomics, so it stays
    /// responsive no matter what the sampler holds.
    pub fn health_line(&self) -> String {
        let journal = if self.journal_ok.load(Ordering::Relaxed) {
            "journal=ok"
        } else {
            "journal=failing"
        };
        let source = match (&self.config.source, self.source_ok.load(Ordering::Relaxed)) {
            (StatusSource::Endpoint(_), true) => "source=ok",
            (StatusSource::Endpoint(_), false) => "source=unreachable",
            (StatusSource::OptedOut, _) => "source=disabled",
            (StatusSource::Unset, _) => "source=unconfigured",
        };
        format!("{journal} {source}")
    }

    // ---- reading -----------------------------------------------------------

    /// Aggregated contribution over the requested window.
    ///
    /// Errors rather than returning zeroes when the ledger has measured
    /// nothing: a summary of unmeasured work is worse than no summary.
    pub fn summary(&self, days: Option<u32>) -> Result<Value> {
        let days = clamp_window(days);
        let now = now_ms();
        let from = now.saturating_sub(u64::from(days) * DAY_MS);

        let (rows, health) = {
            let state = self.lock();
            (
                window_rows(&state.records, &state.open.snapshot(), from),
                state.health.clone(),
            )
        };
        let totals = aggregate(&rows);

        if matches!(self.config.source, StatusSource::Unset) {
            bail!(
                "no host API is configured, so this ledger has never measured any work. Set \
                 `url = \"http://127.0.0.1:{port}\"` on this plugin's `[[plugin]]` table in \
                 config.toml (or pass `--host-api`), then restart tdcc. Pass `--no-host-api` if \
                 you deliberately want a presence-only record.",
                port = crate::config::DEFAULT_CONSOLE_PORT,
            );
        }
        if matches!(self.config.source, StatusSource::Endpoint(_)) && totals.polls.ok == 0 {
            bail!(
                "the host API at {endpoint} has not answered a single status poll in this window, \
                 so there is nothing measured to summarise. Last error: {error}",
                endpoint = self.endpoint_label(),
                error = health
                    .last_poll_error
                    .as_deref()
                    .unwrap_or("none recorded yet; the sampler may not have run"),
            );
        }

        let window_ms = now.saturating_sub(from).max(1);
        let observed_fraction = totals.observed_ms as f64 / window_ms as f64;
        let measured = matches!(self.config.source, StatusSource::Endpoint(_));

        let mut caveats = vec![
            "`served_locally` counts requests this node answered on its own hardware. Work \
             relayed in from a peer re-enters through this node's own API surface and is counted \
             here too, and the host does not attribute inbound work to the peer that sent it, so \
             this ledger cannot separate the two."
                .to_string(),
        ];
        if !measured {
            caveats.push(
                "Host counter sampling is disabled (--no-host-api). Every request and token total \
                 below is zero because nothing was measured, not because nothing happened."
                    .to_string(),
            );
        }
        if totals.polls.failed > 0 {
            caveats.push(format!(
                "{} of {} status polls in this window failed; totals under-count by an unknown \
                 amount.",
                totals.polls.failed,
                totals.polls.failed + totals.polls.ok
            ));
        }
        if totals.polls.counter_resets > 0 {
            caveats.push(format!(
                "The host's counters restarted {} time(s) in this window; work served between the \
                 last poll and each restart is not counted.",
                totals.polls.counter_resets
            ));
        }
        if observed_fraction < 0.99 {
            caveats.push(format!(
                "The ledger was running for {:.1}% of this window; the remainder is unobserved, \
                 not idle.",
                observed_fraction * 100.0
            ));
        }
        if !health.missing_fields.is_empty() {
            caveats.push(format!(
                "This host build did not expose {} in /api/status, so the matching totals read as \
                 zero.",
                health.missing_fields.join(", ")
            ));
        }
        if health.truncated_tail {
            caveats.push(
                "The journal's final line was incomplete when the ledger started, which is what a \
                 crash during an append looks like. It was discarded; earlier history is intact."
                    .to_string(),
            );
        }
        if health.unreadable_lines > 0 {
            caveats.push(format!(
                "{count} journal line(s) could not be parsed and were skipped; something other \
                 than this plugin may be writing to the file.",
                count = health.unreadable_lines,
            ));
        }
        if !self.config.record_peers {
            caveats.push(
                "Peer id recording is off (--no-peer-ids), so `peers_seen` is always zero."
                    .to_string(),
            );
        }

        Ok(json!({
            "disclaimer": DISCLAIMER,
            "measured": measured,
            "window": window_json(days, from, now),
            "node": node_json(&health),
            "totals": {
                "requests_fronted": totals.counters.requests_fronted,
                "requests_succeeded": totals.counters.requests_succeeded,
                "served_locally": totals.counters.served_locally,
                "served_remotely": totals.counters.served_remotely,
                "served_by_endpoint": totals.counters.served_by_endpoint,
                "local_attempts": totals.counters.local_attempts,
                "remote_attempts": totals.counters.remote_attempts,
                "endpoint_attempts": totals.counters.endpoint_attempts,
                "completion_tokens": totals.counters.completion_tokens,
                "attempt_seconds": totals.counters.attempt_ms / 1_000,
                "observed_hours": round_hours(totals.observed_ms),
                "accepting_hours": round_hours(totals.accepting_ms),
            },
            "coverage": {
                "buckets": totals.buckets,
                "observed_ms": totals.observed_ms,
                "window_ms": window_ms,
                "observed_fraction": (observed_fraction * 1_000.0).round() / 1_000.0,
                "polls_ok": totals.polls.ok,
                "polls_failed": totals.polls.failed,
                "counter_resets": totals.polls.counter_resets,
            },
            "peers_seen": totals.peers.len(),
            "peers_truncated": totals.peers_truncated,
            "source": self.source_json(&health),
            "caveats": caveats,
        }))
    }

    /// Raw buckets, newest first.
    pub fn epochs(&self, days: Option<u32>, limit: Option<u32>) -> Result<Value> {
        let days = clamp_window(days);
        let limit = clamp_limit(limit);
        let now = now_ms();
        let from = now.saturating_sub(u64::from(days) * DAY_MS);

        let mut rows = {
            let state = self.lock();
            window_rows(&state.records, &state.open.snapshot(), from)
        };
        rows.sort_by_key(|row| std::cmp::Reverse(row.start_ms));
        let total = rows.len();
        rows.truncate(limit);

        Ok(json!({
            "disclaimer": DISCLAIMER,
            "window": window_json(days, from, now),
            "granularity_note": format!(
                "Buckets are hourly for the most recent {compact} day(s) and daily beyond that; \
                 daily rows past {retain} day(s) are dropped. The newest row is the bucket still \
                 being filled and is not on disk yet.",
                compact = self.config.compact_after_days,
                retain = self.config.retain_days,
            ),
            "returned": rows.len(),
            "total_in_window": total,
            "truncated": total > rows.len(),
            "rows": rows.iter().map(epoch_json).collect::<Vec<_>>(),
        }))
    }

    /// Peers seen in the mesh during the window.
    pub fn peers(&self, days: Option<u32>, limit: Option<u32>) -> Result<Value> {
        let days = clamp_window(days);
        let limit = clamp_limit(limit);
        let now = now_ms();
        let from = now.saturating_sub(u64::from(days) * DAY_MS);

        let rows = {
            let state = self.lock();
            window_rows(&state.records, &state.open.snapshot(), from)
        };
        let mut peers = peer_presence(&rows);
        peers.sort_by(|left, right| {
            right
                .buckets
                .cmp(&left.buckets)
                .then_with(|| left.peer.cmp(&right.peer))
        });
        let total = peers.len();
        peers.truncate(limit);

        Ok(json!({
            "disclaimer": DISCLAIMER,
            "window": window_json(days, from, now),
            "recorded": self.config.record_peers,
            "note": "Presence only. A peer listed here shared a mesh with this node during the \
                     bucket; it does not mean this node served that peer. The host does not \
                     attribute inbound requests to the peer that sent them, so no plugin can \
                     claim otherwise.",
            "id_note": format!(
                "Ids are the first {PEER_ID_PREFIX_CHARS} characters of the peer id the host \
                 reported. Shortening keeps rows readable; it is not anonymisation.",
            ),
            "returned": peers.len(),
            "total_in_window": total,
            "truncated": total > peers.len(),
            "peers": peers
                .iter()
                .map(|peer| {
                    json!({
                        "peer": peer.peer,
                        "buckets": peer.buckets,
                        "first_seen": format_utc(peer.first_seen_ms),
                        "first_seen_ms": peer.first_seen_ms,
                        "last_seen": format_utc(peer.last_seen_ms),
                        "last_seen_ms": peer.last_seen_ms,
                    })
                })
                .collect::<Vec<_>>(),
        }))
    }

    /// Diagnostics. Always succeeds — this is the tool you call when `summary`
    /// refused to answer.
    pub fn status(&self) -> Value {
        let state = self.lock();
        let open = state.open.snapshot();
        json!({
            "plugin": crate::PLUGIN_NAME,
            "version": crate::PLUGIN_VERSION,
            "disclaimer": DISCLAIMER,
            "state_dir": self.journal.dir().display().to_string(),
            "journal": {
                "path": self.journal.journal_path().display().to_string(),
                "bytes": self.journal.journal_bytes(),
                "records_in_memory": state.records.len(),
                "unreadable_lines_at_startup": state.health.unreadable_lines,
                "truncated_tail_at_startup": state.health.truncated_tail,
                "last_write_error": state.health.last_journal_error,
                "durability": "One line per sealed bucket, appended and fsynced. Only compaction \
                               rewrites the file, and it does so via a temp file and a rename.",
            },
            "source": self.source_json(&state.health),
            "retention": {
                "compact_after_days": self.config.compact_after_days,
                "retain_days": self.config.retain_days,
                "policy": "Hourly buckets for the compaction window, then one bucket per UTC day, \
                           then dropped. Never one row per request.",
                "last_run": format_utc(state.health.last_retention_ms),
            },
            "current_bucket": epoch_json(&open),
            "node": node_json(&state.health),
            "privacy": {
                "peer_ids_recorded": self.config.record_peers,
                "peer_id_prefix_chars": PEER_ID_PREFIX_CHARS,
                "records_request_content": false,
                "records_model_names": false,
                "note": "No prompt, completion, model name, or request identifier is written to \
                         disk or returned by any tool.",
            },
            "started": format_utc(state.health.started_ms),
        })
    }

    fn endpoint_label(&self) -> String {
        match &self.config.source {
            StatusSource::Endpoint(endpoint) => {
                format!(
                    "http://{}{}",
                    endpoint.authority,
                    crate::source::STATUS_PATH
                )
            }
            StatusSource::OptedOut => "disabled".to_string(),
            StatusSource::Unset => "unconfigured".to_string(),
        }
    }

    fn source_json(&self, health: &Health) -> Value {
        json!({
            "mode": self.config.source.mode(),
            "endpoint": self.endpoint_label(),
            "poll_secs": self.config.poll_secs,
            "polls_ok": health.polls_ok,
            "polls_failed": health.polls_failed,
            "last_success": health.last_poll_ok_ms.map(format_utc),
            "last_error": health.last_poll_error,
            "missing_fields": health.missing_fields,
            "note": "Counters come from differencing GET /api/status. The host delivers no \
                     per-request events to plugins, so sampling is the only measurement available.",
        })
    }
}

/// Mesh lifecycle events the ledger subscribes to, decoupled from the protobuf
/// enum so the accumulator stays testable without a control connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeshEventKind {
    PeerUp,
    PeerDown,
    LocalAccepting,
    LocalStandby,
    MeshIdUpdated,
}

/// Sum of a set of buckets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    pub buckets: usize,
    pub observed_ms: u64,
    pub accepting_ms: u64,
    pub counters: Counters,
    pub polls: Polls,
    pub peers: BTreeSet<String>,
    pub peers_truncated: bool,
}

/// Buckets overlapping `[from_ms, ∞)`, including the bucket still being filled.
///
/// A bucket that only partially overlaps the window is included whole: buckets
/// are the ledger's unit of resolution and splitting one would invent numbers.
pub fn window_rows(records: &[Record], open: &EpochRecord, from_ms: u64) -> Vec<EpochRecord> {
    let mut rows: Vec<EpochRecord> = records
        .iter()
        .filter_map(|record| match record {
            Record::Epoch(epoch) if epoch.end_ms > from_ms => Some(epoch.clone()),
            _ => None,
        })
        .collect();
    if open.end_ms > from_ms && !epoch_is_empty(open) {
        rows.push(open.clone());
    }
    rows
}

/// A bucket nothing has happened in yet. Reporting one would add a row that
/// says only "time exists".
fn epoch_is_empty(record: &EpochRecord) -> bool {
    record.observed_ms == 0
        && record.counters.is_zero()
        && record.polls == Polls::default()
        && record.peers.is_empty()
}

/// Ceiling on how much wall time one sampler interval may claim as observed.
///
/// A tick that arrives long after the previous one means the machine slept or
/// the sampler was starved. Counting that gap as observed time would claim
/// uptime the ledger cannot vouch for, so two poll intervals is the most any
/// single interval contributes.
pub const fn max_observed_gap_ms(poll_secs: u64) -> u64 {
    poll_secs.saturating_mul(2_000)
}

pub const fn observed_increment(previous_tick_ms: u64, now_ms: u64, max_gap_ms: u64) -> u64 {
    let elapsed = now_ms.saturating_sub(previous_tick_ms);
    if elapsed < max_gap_ms {
        elapsed
    } else {
        max_gap_ms
    }
}

pub fn aggregate(rows: &[EpochRecord]) -> Aggregate {
    let mut total = Aggregate::default();
    for row in rows {
        total.buckets += 1;
        total.observed_ms = total.observed_ms.saturating_add(row.observed_ms);
        total.accepting_ms = total.accepting_ms.saturating_add(row.accepting_ms);
        total.counters.add(&row.counters);
        total.polls.add(&row.polls);
        total.peers_truncated |= row.peers_truncated;
        for peer in &row.peers {
            total.peers.insert(peer.clone());
        }
    }
    total
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPresence {
    pub peer: String,
    pub buckets: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

pub fn peer_presence(rows: &[EpochRecord]) -> Vec<PeerPresence> {
    let mut seen: BTreeMap<&str, PeerPresence> = BTreeMap::new();
    for row in rows {
        for peer in &row.peers {
            seen.entry(peer.as_str())
                .and_modify(|presence| {
                    presence.buckets += 1;
                    presence.first_seen_ms = presence.first_seen_ms.min(row.start_ms);
                    presence.last_seen_ms = presence.last_seen_ms.max(row.start_ms);
                })
                .or_insert_with(|| PeerPresence {
                    peer: peer.clone(),
                    buckets: 1,
                    first_seen_ms: row.start_ms,
                    last_seen_ms: row.start_ms,
                });
        }
    }
    seen.into_values().collect()
}

pub fn clamp_window(days: Option<u32>) -> u32 {
    days.unwrap_or(DEFAULT_WINDOW_DAYS)
        .clamp(1, MAX_WINDOW_DAYS)
}

pub fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_ROW_LIMIT).clamp(1, MAX_ROW_LIMIT) as usize
}

fn round_hours(ms: u64) -> f64 {
    (ms as f64 / HOUR_MS as f64 * 100.0).round() / 100.0
}

fn window_json(days: u32, from_ms: u64, to_ms: u64) -> Value {
    json!({
        "days": days,
        "from": format_utc(from_ms),
        "from_ms": from_ms,
        "to": format_utc(to_ms),
        "to_ms": to_ms,
    })
}

fn node_json(health: &Health) -> Value {
    json!({
        "node_id": health.node_id,
        "mesh_id": health.mesh_id,
        "accepting_work": health.accepting,
    })
}

fn epoch_json(record: &EpochRecord) -> Value {
    json!({
        "start": format_utc(record.start_ms),
        "start_ms": record.start_ms,
        "granularity": record.granularity.as_str(),
        "observed_ms": record.observed_ms,
        "accepting_ms": record.accepting_ms,
        "counters": {
            "requests_fronted": record.counters.requests_fronted,
            "requests_succeeded": record.counters.requests_succeeded,
            "served_locally": record.counters.served_locally,
            "served_remotely": record.counters.served_remotely,
            "served_by_endpoint": record.counters.served_by_endpoint,
            "local_attempts": record.counters.local_attempts,
            "remote_attempts": record.counters.remote_attempts,
            "endpoint_attempts": record.counters.endpoint_attempts,
            "completion_tokens": record.counters.completion_tokens,
            "attempt_ms": record.counters.attempt_ms,
        },
        "peers_seen": record.peers.len(),
        "peers_truncated": record.peers_truncated,
        "polls": {
            "ok": record.polls.ok,
            "failed": record.polls.failed,
            "counter_resets": record.polls.counter_resets,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StatusSource;
    use std::path::PathBuf;

    fn config(dir: PathBuf, source: StatusSource) -> LedgerConfig {
        LedgerConfig {
            state_dir: dir,
            source,
            poll_secs: 30,
            record_peers: true,
            compact_after_days: 14,
            retain_days: 400,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tdcc-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn row(start_ms: u64, fronted: u64, peers: &[&str]) -> EpochRecord {
        EpochRecord {
            v: RECORD_VERSION,
            granularity: Granularity::Hour,
            start_ms,
            end_ms: start_ms + HOUR_MS,
            observed_ms: HOUR_MS,
            accepting_ms: HOUR_MS / 2,
            counters: Counters {
                requests_fronted: fronted,
                served_locally: fronted,
                completion_tokens: fronted * 100,
                attempt_ms: fronted * 250,
                ..Counters::default()
            },
            peers: peers.iter().map(|peer| peer.to_string()).collect(),
            peers_truncated: false,
            polls: Polls {
                ok: 120,
                ..Polls::default()
            },
        }
    }

    fn sample(fronted: u64, tokens: u64) -> StatusSample {
        StatusSample {
            node_id: Some("node-1".into()),
            mesh_id: Some("mesh-1".into()),
            node_state: Some("serving".into()),
            peers: vec!["peer-aaaaaaaaaaaaaaaaaaaa".into()],
            cumulative: Cumulative {
                requests_fronted: fronted,
                served_locally: fronted,
                completion_tokens: tokens,
                local_attempts: fronted,
                ..Cumulative::default()
            },
            missing: Vec::new(),
        }
    }

    #[test]
    fn aggregate_sums_buckets_and_unions_peers() {
        let rows = vec![
            row(0, 3, &["peer-a", "peer-b"]),
            row(HOUR_MS, 5, &["peer-b", "peer-c"]),
        ];

        let total = aggregate(&rows);

        assert_eq!(total.buckets, 2);
        assert_eq!(total.counters.requests_fronted, 8);
        assert_eq!(total.counters.completion_tokens, 800);
        assert_eq!(total.observed_ms, 2 * HOUR_MS);
        assert_eq!(total.polls.ok, 240);
        assert_eq!(total.peers.len(), 3, "peer sets union, they do not add");
    }

    #[test]
    fn window_rows_include_partial_overlaps_and_the_open_bucket() {
        let records = vec![
            Record::Epoch(row(0, 1, &[])),
            Record::Epoch(row(10 * HOUR_MS, 2, &[])),
            Record::Session(SessionRecord {
                v: RECORD_VERSION,
                at_ms: 5 * HOUR_MS,
                plugin_version: "0.1.0".into(),
                note: "start".into(),
            }),
        ];
        let open = row(20 * HOUR_MS, 4, &[]);

        // A cut halfway through the second bucket keeps it whole.
        let rows = window_rows(&records, &open, 10 * HOUR_MS + HOUR_MS / 2);

        assert_eq!(rows.len(), 2, "session records are not buckets");
        assert_eq!(aggregate(&rows).counters.requests_fronted, 6);
    }

    #[test]
    fn an_untouched_open_bucket_is_not_reported_as_a_bucket() {
        let open = EpochRecord {
            observed_ms: 0,
            counters: Counters::default(),
            peers: Vec::new(),
            polls: Polls::default(),
            ..row(0, 0, &[])
        };

        assert!(window_rows(&[], &open, 0).is_empty());
    }

    #[test]
    fn peer_presence_counts_buckets_and_first_and_last_sighting() {
        let rows = vec![
            row(0, 1, &["peer-a"]),
            row(HOUR_MS, 1, &["peer-a", "peer-b"]),
            row(2 * HOUR_MS, 1, &["peer-b"]),
        ];

        let mut presence = peer_presence(&rows);
        presence.sort_by(|left, right| left.peer.cmp(&right.peer));

        assert_eq!(presence[0].peer, "peer-a");
        assert_eq!(presence[0].buckets, 2);
        assert_eq!(presence[0].first_seen_ms, 0);
        assert_eq!(presence[0].last_seen_ms, HOUR_MS);
        assert_eq!(presence[1].buckets, 2);
        assert_eq!(presence[1].first_seen_ms, HOUR_MS);
    }

    #[test]
    fn peer_ids_are_shortened_deduplicated_and_capped() {
        let mut open = OpenEpoch::new(0, 0);
        open.note_peer("0123456789abcdefghijklmnop");
        open.note_peer("0123456789abcdefZZZZ");

        assert_eq!(open.peers.len(), 1, "a shared prefix is one peer");
        assert!(open.peers.contains("0123456789abcdef"));

        // Ids that differ only past the prefix would collapse, so vary the
        // leading characters the way real peer ids do.
        for index in 0..MAX_PEERS_PER_EPOCH + 10 {
            open.note_peer(&format!("{index:016x}-tail-that-is-ignored"));
        }
        assert_eq!(open.peers.len(), MAX_PEERS_PER_EPOCH);
        assert!(open.peers_truncated, "overflow has to be admitted");
    }

    #[test]
    fn window_and_limit_arguments_are_clamped_to_the_advertised_range() {
        assert_eq!(clamp_window(None), DEFAULT_WINDOW_DAYS);
        assert_eq!(clamp_window(Some(0)), 1);
        assert_eq!(clamp_window(Some(u32::MAX)), MAX_WINDOW_DAYS);
        assert_eq!(clamp_limit(None), DEFAULT_ROW_LIMIT as usize);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(u32::MAX)), MAX_ROW_LIMIT as usize);
    }

    #[test]
    fn summary_refuses_to_report_zeroes_when_nothing_was_ever_measured() {
        let ledger = Ledger::open(config(temp_dir("unset"), StatusSource::Unset))
            .expect("an empty directory is a valid start");

        let error = ledger
            .summary(None)
            .expect_err("an unmeasured summary is an error");
        let message = error.to_string();
        assert!(message.contains("no host API is configured"), "{message}");
        // The diagnostic tool still answers, which is the point of having it.
        assert_eq!(ledger.status()["source"]["mode"], "unset");

        std::fs::remove_dir_all(ledger.journal.dir()).unwrap();
    }

    #[test]
    fn an_opted_out_ledger_reports_presence_without_pretending_to_measure() {
        let ledger = Ledger::open(config(temp_dir("optout"), StatusSource::OptedOut)).unwrap();
        ledger.note_mesh_event(
            MeshEventKind::LocalAccepting,
            None,
            Some("node-9"),
            Some("mesh-9"),
        );
        ledger.note_mesh_event(
            MeshEventKind::PeerUp,
            Some("peer-0123456789abcdef"),
            None,
            None,
        );
        ledger.tick(now_ms(), None);

        let summary = ledger
            .summary(Some(1))
            .expect("opting out is not a failure");

        assert_eq!(summary["measured"], false);
        assert_eq!(summary["totals"]["requests_fronted"], 0);
        assert_eq!(summary["peers_seen"], 1);
        assert_eq!(summary["node"]["mesh_id"], "mesh-9");
        let caveats = summary["caveats"].as_array().unwrap();
        assert!(
            caveats.iter().any(|caveat| caveat
                .as_str()
                .unwrap()
                .contains("zero because nothing was measured")),
            "{caveats:?}"
        );

        std::fs::remove_dir_all(ledger.journal.dir()).unwrap();
    }

    #[test]
    fn the_first_poll_is_a_baseline_and_later_polls_are_deltas() {
        let ledger = Ledger::open(config(
            temp_dir("deltas"),
            StatusSource::Endpoint(
                crate::config::parse_loopback_base("http://127.0.0.1:3131").unwrap(),
            ),
        ))
        .unwrap();

        let now = now_ms();
        ledger.tick(now, Some(Ok(sample(1_000, 90_000))));
        let baseline = ledger.summary(Some(1)).expect("a poll happened");
        assert_eq!(
            baseline["totals"]["requests_fronted"], 0,
            "the host's pre-existing totals are not this ledger's to claim"
        );

        ledger.tick(now + 1_000, Some(Ok(sample(1_007, 90_500))));
        let after = ledger.summary(Some(1)).unwrap();
        assert_eq!(after["totals"]["requests_fronted"], 7);
        assert_eq!(after["totals"]["completion_tokens"], 500);

        // A lower reading is a host restart, and the new reading is the delta.
        ledger.tick(now + 2_000, Some(Ok(sample(2, 10))));
        let after_restart = ledger.summary(Some(1)).unwrap();
        assert_eq!(after_restart["totals"]["requests_fronted"], 9);
        assert_eq!(after_restart["coverage"]["counter_resets"], 1);

        std::fs::remove_dir_all(ledger.journal.dir()).unwrap();
    }

    #[test]
    fn a_configured_but_unreachable_source_errors_instead_of_reporting_zeroes() {
        let ledger = Ledger::open(config(
            temp_dir("unreachable"),
            StatusSource::Endpoint(
                crate::config::parse_loopback_base("http://127.0.0.1:3131").unwrap(),
            ),
        ))
        .unwrap();
        ledger.tick(now_ms(), Some(Err(anyhow::anyhow!("connection refused"))));

        let error = ledger
            .summary(None)
            .expect_err("a dead source is a failure");

        assert!(error.to_string().contains("connection refused"), "{error}");
        assert!(ledger.health_line().contains("source=unreachable"));

        std::fs::remove_dir_all(ledger.journal.dir()).unwrap();
    }

    #[test]
    fn flush_seals_the_open_bucket_and_history_survives_a_restart() {
        let dir = temp_dir("restart");
        let endpoint = StatusSource::Endpoint(
            crate::config::parse_loopback_base("http://127.0.0.1:3131").unwrap(),
        );

        let ledger = Ledger::open(config(dir.clone(), endpoint.clone())).unwrap();
        let now = now_ms();
        ledger.tick(now, Some(Ok(sample(10, 100))));
        ledger.tick(now + 1_000, Some(Ok(sample(20, 400))));
        let flushed = ledger.flush().unwrap();
        assert_eq!(flushed["flushed"], true);
        assert_eq!(flushed["bucket"]["counters"]["requests_fronted"], 10);
        // Nothing left in the open bucket, so a second flush writes nothing.
        assert_eq!(ledger.flush().unwrap()["flushed"], false);
        drop(ledger);

        let reopened = Ledger::open(config(dir.clone(), endpoint)).unwrap();
        let summary = reopened.summary(Some(1)).unwrap();
        assert_eq!(
            summary["totals"]["requests_fronted"], 10,
            "sealed history is read back from disk"
        );

        // The flush moved the resume point with it, so the next reading is
        // differenced against 20 rather than replaying the flushed work.
        reopened.tick(now_ms(), Some(Ok(sample(25, 600))));
        let resumed = reopened.summary(Some(1)).unwrap();
        assert_eq!(
            resumed["totals"]["requests_fronted"], 15,
            "10 flushed + 5 new; a flushed bucket must not be counted twice"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_interval_never_claims_more_than_two_poll_periods_of_observed_time() {
        let cap = max_observed_gap_ms(30);
        assert_eq!(cap, 60_000);
        assert_eq!(observed_increment(1_000, 1_000, cap), 0);
        assert_eq!(observed_increment(1_000, 31_000, cap), 30_000);
        assert_eq!(observed_increment(1_000, 1_000 + HOUR_MS, cap), cap);
        // A clock that went backwards contributes nothing rather than wrapping.
        assert_eq!(observed_increment(HOUR_MS, 1_000, cap), 0);
    }

    #[test]
    fn a_long_gap_between_ticks_is_not_counted_as_observed_time() {
        let ledger = Ledger::open(config(temp_dir("gap"), StatusSource::OptedOut)).unwrap();
        let start = now_ms();

        // The machine slept for an hour between ticks, which may also roll the
        // bucket over — so sum every bucket rather than inspecting the open one.
        ledger.tick(start + HOUR_MS, None);

        let rows = ledger.epochs(Some(1), None).unwrap();
        let observed: u64 = rows["rows"]
            .as_array()
            .expect("rows is an array")
            .iter()
            .map(|row| row["observed_ms"].as_u64().unwrap_or(0))
            .sum();
        assert!(
            observed <= max_observed_gap_ms(ledger.config.poll_secs),
            "observed {observed}ms should be capped at two poll intervals"
        );

        std::fs::remove_dir_all(ledger.journal.dir()).unwrap();
    }
}
