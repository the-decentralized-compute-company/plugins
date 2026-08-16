//! The on-disk record: an append-only JSONL journal of aggregated buckets.
//!
//! Three properties drive the design.
//!
//! **Append-only on the hot path.** Sealing a bucket opens the file, writes one
//! line, flushes, and closes. Nothing rewrites earlier bytes, so a crash can
//! only ever truncate the newest line — never corrupt history. [`load`] treats
//! an unparseable *final* line in a file that does not end in a newline as
//! exactly that, drops it, and reports it instead of failing the whole read.
//!
//! **Aggregate, never per-request.** One record covers a whole UTC hour. A node
//! serving a million requests an hour writes the same 24 lines a day as an idle
//! one. There is no row per request to grow unbounded, and there is nothing in
//! a record that could describe an individual request even if you wanted it to.
//!
//! **Bounded forever.** Hourly rows older than the compaction horizon are
//! merged into one row per UTC day, and daily rows older than the retention
//! horizon are dropped. Compaction is the one operation that rewrites the file;
//! it writes a sibling temp file, fsyncs it, and renames it over the journal,
//! so an interrupted compaction leaves the previous journal intact.
//!
//! What is deliberately absent from every record: prompts, completions, model
//! names, request ids, and any other description of *what* was computed. The
//! ledger records that work happened and how much, never what it was.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::clock::{DAY_MS, Granularity, bucket_start};

/// Bumped only for a change that an older reader could misinterpret.
pub const RECORD_VERSION: u32 = 1;

/// Upper bound on peer ids stored in one bucket.
///
/// A bucket is a set, not a log, so this only bites on a mesh with more than 64
/// peers in a single hour — and then the record says so rather than silently
/// dropping names.
pub const MAX_PEERS_PER_EPOCH: usize = 64;

/// Upper bound on retained session markers.
///
/// Session records are written once per start, so a healthy node adds a handful
/// a month. A plugin that crash-loops would add one per restart and never reach
/// a bucket seal, so age alone is not enough to bound them — this cap is.
pub const MAX_SESSION_RECORDS: usize = 128;

pub const JOURNAL_FILE: &str = "journal.jsonl";
pub const CURSOR_FILE: &str = "cursor.json";

/// Work attributed to one bucket. Every field is a delta, never a running
/// total, so buckets add up by summation and a host restart cannot make the
/// history jump.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Counters {
    /// Requests this node accepted on its own API surface. Requests relayed in
    /// from a peer over the mesh re-enter through that same surface, so they
    /// are included here — and are not distinguishable from local ones. See
    /// the README's "What this cannot tell you".
    #[serde(default)]
    pub requests_fronted: u64,
    #[serde(default)]
    pub requests_succeeded: u64,
    /// Of the fronted requests, the ones answered by this node's own hardware.
    #[serde(default)]
    pub served_locally: u64,
    /// Answered by another mesh node. This is consumption, not contribution.
    #[serde(default)]
    pub served_remotely: u64,
    /// Answered by an attached inference endpoint.
    #[serde(default)]
    pub served_by_endpoint: u64,
    #[serde(default)]
    pub local_attempts: u64,
    #[serde(default)]
    pub remote_attempts: u64,
    #[serde(default)]
    pub endpoint_attempts: u64,
    /// Completion tokens the host observed. Counted across all target kinds.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Wall-clock milliseconds spent inside routing attempts, summed across
    /// local, remote, and endpoint targets. A busy-time estimate reconstructed
    /// from the host's running average — not GPU utilisation, and not
    /// splittable per target kind with what the host exposes today.
    #[serde(default)]
    pub attempt_ms: u64,
}

impl Counters {
    pub fn add(&mut self, other: &Self) {
        self.requests_fronted = self.requests_fronted.saturating_add(other.requests_fronted);
        self.requests_succeeded = self
            .requests_succeeded
            .saturating_add(other.requests_succeeded);
        self.served_locally = self.served_locally.saturating_add(other.served_locally);
        self.served_remotely = self.served_remotely.saturating_add(other.served_remotely);
        self.served_by_endpoint = self
            .served_by_endpoint
            .saturating_add(other.served_by_endpoint);
        self.local_attempts = self.local_attempts.saturating_add(other.local_attempts);
        self.remote_attempts = self.remote_attempts.saturating_add(other.remote_attempts);
        self.endpoint_attempts = self
            .endpoint_attempts
            .saturating_add(other.endpoint_attempts);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.attempt_ms = self.attempt_ms.saturating_add(other.attempt_ms);
    }

    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// How well the ledger could see during a bucket. Without this, an hour of zero
/// work and an hour of a dead poller look identical.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Polls {
    #[serde(default)]
    pub ok: u64,
    #[serde(default)]
    pub failed: u64,
    /// Times the host's counters went backwards, meaning `tdcc` restarted and
    /// whatever it served between the last poll and the restart is lost.
    #[serde(default)]
    pub counter_resets: u64,
}

impl Polls {
    pub fn add(&mut self, other: &Self) {
        self.ok = self.ok.saturating_add(other.ok);
        self.failed = self.failed.saturating_add(other.failed);
        self.counter_resets = self.counter_resets.saturating_add(other.counter_resets);
    }
}

/// The host's cumulative, process-lifetime counters at one instant.
///
/// Only ever used to compute a delta against the previous sample. Never stored
/// in the journal, because a running total is meaningless once the host
/// restarts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Cumulative {
    #[serde(default)]
    pub requests_fronted: u64,
    #[serde(default)]
    pub requests_succeeded: u64,
    #[serde(default)]
    pub served_locally: u64,
    #[serde(default)]
    pub served_remotely: u64,
    #[serde(default)]
    pub served_by_endpoint: u64,
    #[serde(default)]
    pub local_attempts: u64,
    #[serde(default)]
    pub remote_attempts: u64,
    #[serde(default)]
    pub endpoint_attempts: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub attempt_ms: u64,
}

impl Cumulative {
    /// Work done between `previous` and `self`.
    ///
    /// Returns `reset = true` when any counter went backwards, which only
    /// happens when the host process restarted. In that case the current
    /// reading *is* the delta, because the host started counting from zero;
    /// anything it served between the last poll and the restart is
    /// unrecoverable and the reset count records that a gap exists.
    pub fn delta_since(&self, previous: &Self) -> (Counters, bool) {
        let reset = self.requests_fronted < previous.requests_fronted
            || self.requests_succeeded < previous.requests_succeeded
            || self.served_locally < previous.served_locally
            || self.served_remotely < previous.served_remotely
            || self.served_by_endpoint < previous.served_by_endpoint
            || self.local_attempts < previous.local_attempts
            || self.remote_attempts < previous.remote_attempts
            || self.endpoint_attempts < previous.endpoint_attempts
            || self.completion_tokens < previous.completion_tokens
            || self.attempt_ms < previous.attempt_ms;

        let base = if reset { &Self::default() } else { previous };
        let counters = Counters {
            requests_fronted: self.requests_fronted.saturating_sub(base.requests_fronted),
            requests_succeeded: self
                .requests_succeeded
                .saturating_sub(base.requests_succeeded),
            served_locally: self.served_locally.saturating_sub(base.served_locally),
            served_remotely: self.served_remotely.saturating_sub(base.served_remotely),
            served_by_endpoint: self
                .served_by_endpoint
                .saturating_sub(base.served_by_endpoint),
            local_attempts: self.local_attempts.saturating_sub(base.local_attempts),
            remote_attempts: self.remote_attempts.saturating_sub(base.remote_attempts),
            endpoint_attempts: self
                .endpoint_attempts
                .saturating_sub(base.endpoint_attempts),
            completion_tokens: self
                .completion_tokens
                .saturating_sub(base.completion_tokens),
            attempt_ms: self.attempt_ms.saturating_sub(base.attempt_ms),
        };
        (counters, reset)
    }
}

/// Resume point, so a plugin restart does not discard work the host already
/// counted. Disposable by design: losing it costs at most the counts between
/// the last bucket seal and the restart, never any recorded history.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Cursor {
    pub v: u32,
    pub at_ms: u64,
    pub cumulative: Cumulative,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SessionRecord {
    pub v: u32,
    pub at_ms: u64,
    pub plugin_version: String,
    /// Why this session record exists, in plain words, for whoever reads the
    /// raw file.
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EpochRecord {
    pub v: u32,
    pub granularity: Granularity,
    pub start_ms: u64,
    /// Exclusive end of the bucket.
    pub end_ms: u64,
    /// How much of the bucket the ledger was actually running for. Less than
    /// the bucket width means the rest is simply unobserved.
    pub observed_ms: u64,
    /// Of `observed_ms`, how long this node reported it was accepting work.
    pub accepting_ms: u64,
    pub counters: Counters,
    /// Peers seen in the mesh during the bucket, as id prefixes. Presence only:
    /// this does **not** mean these peers sent work here (see the README).
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default)]
    pub peers_truncated: bool,
    pub polls: Polls,
}

impl EpochRecord {
    /// Fold `other` into `self`, which must cover a subrange of the same day.
    fn absorb(&mut self, other: &Self) {
        self.start_ms = self.start_ms.min(other.start_ms);
        self.end_ms = self.end_ms.max(other.end_ms);
        self.observed_ms = self.observed_ms.saturating_add(other.observed_ms);
        self.accepting_ms = self.accepting_ms.saturating_add(other.accepting_ms);
        self.counters.add(&other.counters);
        self.polls.add(&other.polls);
        self.peers_truncated |= other.peers_truncated;
        for peer in &other.peers {
            if self.peers.contains(peer) {
                continue;
            }
            if self.peers.len() >= MAX_PEERS_PER_EPOCH {
                self.peers_truncated = true;
                break;
            }
            self.peers.push(peer.clone());
        }
        self.peers.sort();
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Session(SessionRecord),
    Epoch(EpochRecord),
}

impl Record {
    /// Timestamp used to order and age a record.
    pub fn at_ms(&self) -> u64 {
        match self {
            Self::Session(session) => session.at_ms,
            Self::Epoch(epoch) => epoch.start_ms,
        }
    }
}

/// Outcome of reading the journal, including what could not be read.
///
/// The counts are surfaced by the `status` tool rather than swallowed: a
/// growing `unreadable_lines` means something other than this plugin is writing
/// to the file, and an operator should know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedJournal {
    pub records: Vec<Record>,
    pub unreadable_lines: usize,
    /// The last line was incomplete, which is the expected shape of a crash
    /// during an append.
    pub truncated_tail: bool,
}

/// Handle to one ledger directory. Holds no file descriptors: every write opens
/// and closes, which keeps a rename-over-the-journal compaction legal on
/// Windows and means a crash never leaves a half-flushed buffer.
#[derive(Clone, Debug)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }

    pub fn cursor_path(&self) -> PathBuf {
        self.dir.join(CURSOR_FILE)
    }

    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating ledger directory {}", self.dir.display()))
    }

    /// Size of the journal in bytes, or 0 when it does not exist yet.
    pub fn journal_bytes(&self) -> u64 {
        fs::metadata(self.journal_path())
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }

    pub fn load(&self) -> Result<LoadedJournal> {
        let path = self.journal_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedJournal::default());
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(parse_journal(&text))
    }

    /// Append one record. The whole line goes out in a single `write_all`, so
    /// the only failure a crash can produce is a truncated final line.
    pub fn append(&self, record: &Record) -> Result<()> {
        self.ensure_dir()?;
        let mut line = serde_json::to_string(record).context("serializing ledger record")?;
        line.push('\n');
        let path = self.journal_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {} for append", path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flushing {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("syncing {}", path.display()))?;
        Ok(())
    }

    /// Replace the journal with `records`, atomically.
    ///
    /// Only compaction calls this. The temp file is fully written and synced
    /// before the rename, so an interruption leaves the previous journal in
    /// place and the next start simply compacts again.
    pub fn rewrite(&self, records: &[Record]) -> Result<()> {
        self.ensure_dir()?;
        let mut body = String::new();
        for record in records {
            body.push_str(&serde_json::to_string(record).context("serializing ledger record")?);
            body.push('\n');
        }
        write_atomically(&self.journal_path(), body.as_bytes())
    }

    /// Load the resume cursor, treating any problem as "no cursor".
    ///
    /// A missing or unreadable cursor is not an error: it only means the next
    /// poll establishes a fresh baseline.
    pub fn load_cursor(&self) -> Option<Cursor> {
        let text = fs::read_to_string(self.cursor_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn store_cursor(&self, cursor: &Cursor) -> Result<()> {
        self.ensure_dir()?;
        let body = serde_json::to_vec(cursor).context("serializing ledger cursor")?;
        write_atomically(&self.cursor_path(), &body)
    }
}

/// Write `bytes` to `path` via a sibling temp file and a rename.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    {
        let mut file =
            File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temp.display()))?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), temp.display()))
}

/// Parse a whole journal body, tolerating a torn final line.
pub fn parse_journal(text: &str) -> LoadedJournal {
    let ends_with_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.split('\n').collect();
    let last_non_empty = lines.iter().rposition(|line| !line.trim().is_empty());

    let mut loaded = LoadedJournal::default();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(record) => loaded.records.push(record),
            Err(_) if !ends_with_newline && Some(index) == last_non_empty => {
                // Exactly the shape of a crash mid-append: the file does not
                // end in a newline and its final line does not parse.
                loaded.truncated_tail = true;
            }
            Err(_) => loaded.unreadable_lines += 1,
        }
    }
    loaded
}

/// Result of applying the retention policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retention {
    pub records: Vec<Record>,
    /// True when the journal on disk no longer matches `records`.
    pub changed: bool,
    pub compacted_epochs: usize,
    pub dropped_records: usize,
}

/// Merge aged hourly rows into daily rows and drop rows past the horizon.
///
/// Pure: takes the records, returns the records it wants written. The caller
/// decides whether to pay for a rewrite.
pub fn apply_retention(
    records: Vec<Record>,
    now_ms: u64,
    compact_after_days: u64,
    retain_days: u64,
) -> Retention {
    let drop_before = now_ms.saturating_sub(retain_days.saturating_mul(DAY_MS));
    let compact_before = now_ms.saturating_sub(compact_after_days.saturating_mul(DAY_MS));

    let mut kept: Vec<Record> = Vec::with_capacity(records.len());
    let mut days: BTreeMap<u64, EpochRecord> = BTreeMap::new();
    let mut compacted_epochs = 0;
    let mut dropped_records = 0;

    for record in records {
        match record {
            Record::Session(session) => {
                if session.at_ms < drop_before {
                    dropped_records += 1;
                } else {
                    kept.push(Record::Session(session));
                }
            }
            Record::Epoch(epoch) => {
                if epoch.end_ms <= drop_before {
                    dropped_records += 1;
                    continue;
                }
                let needs_compaction =
                    epoch.granularity == Granularity::Hour && epoch.end_ms <= compact_before;
                if !needs_compaction && epoch.granularity == Granularity::Hour {
                    kept.push(Record::Epoch(epoch));
                    continue;
                }
                // Daily rows always route through the map so a freshly
                // compacted hour lands in the day row that already exists.
                let day_start = bucket_start(epoch.start_ms, Granularity::Day);
                if needs_compaction {
                    compacted_epochs += 1;
                }
                match days.get_mut(&day_start) {
                    Some(existing) => existing.absorb(&epoch),
                    None => {
                        let mut rolled = epoch;
                        rolled.granularity = Granularity::Day;
                        rolled.start_ms = day_start;
                        rolled.end_ms = rolled.end_ms.max(day_start + DAY_MS);
                        rolled.peers.sort();
                        days.insert(day_start, rolled);
                    }
                }
            }
        }
    }

    kept.extend(days.into_values().map(Record::Epoch));
    kept.sort_by_key(Record::at_ms);

    // Age cannot bound session markers on a node that restarts without ever
    // sealing a bucket, so drop the oldest ones past the cap.
    let session_count = kept
        .iter()
        .filter(|record| matches!(record, Record::Session(_)))
        .count();
    if session_count > MAX_SESSION_RECORDS {
        let mut surplus = session_count - MAX_SESSION_RECORDS;
        dropped_records += surplus;
        kept.retain(|record| match record {
            Record::Session(_) if surplus > 0 => {
                surplus -= 1;
                false
            }
            _ => true,
        });
    }

    Retention {
        changed: compacted_epochs > 0 || dropped_records > 0,
        records: kept,
        compacted_epochs,
        dropped_records,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::HOUR_MS;

    fn epoch(start_ms: u64, granularity: Granularity, fronted: u64) -> Record {
        Record::Epoch(EpochRecord {
            v: RECORD_VERSION,
            granularity,
            start_ms,
            end_ms: start_ms + granularity.width_ms(),
            observed_ms: granularity.width_ms(),
            accepting_ms: granularity.width_ms(),
            counters: Counters {
                requests_fronted: fronted,
                served_locally: fronted,
                completion_tokens: fronted * 10,
                ..Counters::default()
            },
            peers: vec![format!("peer{start_ms}")],
            peers_truncated: false,
            polls: Polls {
                ok: 2,
                ..Polls::default()
            },
        })
    }

    #[test]
    fn a_torn_final_line_is_dropped_not_fatal() {
        let good = serde_json::to_string(&epoch(0, Granularity::Hour, 3)).unwrap();
        let text = format!("{good}\n{{\"kind\":\"epo");

        let loaded = parse_journal(&text);

        assert_eq!(loaded.records.len(), 1);
        assert!(loaded.truncated_tail);
        assert_eq!(loaded.unreadable_lines, 0);
    }

    #[test]
    fn a_broken_line_in_the_middle_is_counted_not_hidden() {
        let good = serde_json::to_string(&epoch(0, Granularity::Hour, 3)).unwrap();
        let text = format!("{good}\nnot json at all\n{good}\n");

        let loaded = parse_journal(&text);

        assert_eq!(loaded.records.len(), 2);
        assert_eq!(loaded.unreadable_lines, 1);
        assert!(!loaded.truncated_tail);
    }

    #[test]
    fn an_empty_or_missing_journal_reads_as_empty() {
        assert_eq!(parse_journal(""), LoadedJournal::default());
        assert_eq!(parse_journal("\n\n"), LoadedJournal::default());
    }

    #[test]
    fn deltas_subtract_the_previous_sample() {
        let previous = Cumulative {
            requests_fronted: 10,
            completion_tokens: 500,
            ..Cumulative::default()
        };
        let current = Cumulative {
            requests_fronted: 14,
            completion_tokens: 900,
            ..Cumulative::default()
        };

        let (delta, reset) = current.delta_since(&previous);

        assert!(!reset);
        assert_eq!(delta.requests_fronted, 4);
        assert_eq!(delta.completion_tokens, 400);
    }

    #[test]
    fn a_backwards_counter_is_treated_as_a_host_restart() {
        let previous = Cumulative {
            requests_fronted: 1_000,
            completion_tokens: 50_000,
            ..Cumulative::default()
        };
        let current = Cumulative {
            requests_fronted: 3,
            completion_tokens: 120,
            ..Cumulative::default()
        };

        let (delta, reset) = current.delta_since(&previous);

        assert!(reset, "a lower reading can only mean the host restarted");
        assert_eq!(delta.requests_fronted, 3, "the new reading is the delta");
        assert_eq!(delta.completion_tokens, 120);
    }

    #[test]
    fn hourly_rows_past_the_horizon_become_one_row_per_day() {
        let now = 30 * DAY_MS;
        let day = 5 * DAY_MS;
        let records = vec![
            epoch(day, Granularity::Hour, 1),
            epoch(day + HOUR_MS, Granularity::Hour, 2),
            epoch(day + 2 * HOUR_MS, Granularity::Hour, 4),
            // Recent enough to stay hourly.
            epoch(now - HOUR_MS, Granularity::Hour, 8),
        ];

        let retention = apply_retention(records, now, 14, 400);

        assert!(retention.changed);
        assert_eq!(retention.compacted_epochs, 3);
        assert_eq!(retention.records.len(), 2);

        let Record::Epoch(rolled) = &retention.records[0] else {
            panic!("the compacted day comes first");
        };
        assert_eq!(rolled.granularity, Granularity::Day);
        assert_eq!(rolled.start_ms, day);
        assert_eq!(rolled.counters.requests_fronted, 7);
        assert_eq!(rolled.counters.completion_tokens, 70);
        assert_eq!(rolled.observed_ms, 3 * HOUR_MS);
        assert_eq!(rolled.polls.ok, 6);
        assert_eq!(rolled.peers.len(), 3);
    }

    #[test]
    fn rows_past_the_retention_horizon_are_dropped() {
        let now = 500 * DAY_MS;
        let records = vec![
            epoch(10 * DAY_MS, Granularity::Day, 5),
            epoch(now - DAY_MS, Granularity::Hour, 9),
        ];

        let retention = apply_retention(records, now, 14, 400);

        assert_eq!(retention.dropped_records, 1);
        assert_eq!(retention.records.len(), 1);
        assert_eq!(retention.records[0].at_ms(), now - DAY_MS);
    }

    #[test]
    fn session_markers_are_capped_so_a_crash_loop_cannot_grow_the_journal() {
        let now = 10 * DAY_MS;
        let mut records: Vec<Record> = (0..MAX_SESSION_RECORDS + 40)
            .map(|index| {
                Record::Session(SessionRecord {
                    v: RECORD_VERSION,
                    at_ms: now - (MAX_SESSION_RECORDS + 40 - index) as u64,
                    plugin_version: "0.1.0".into(),
                    note: format!("restart {index}"),
                })
            })
            .collect();
        records.push(epoch(now - HOUR_MS, Granularity::Hour, 1));

        let retention = apply_retention(records, now, 14, 400);

        let sessions = retention
            .records
            .iter()
            .filter(|record| matches!(record, Record::Session(_)))
            .count();
        assert_eq!(sessions, MAX_SESSION_RECORDS);
        assert_eq!(retention.dropped_records, 40, "the oldest markers go first");
        assert!(retention.changed);
        // The newest marker survives, so the most recent restart stays visible.
        let Some(Record::Session(newest)) = retention
            .records
            .iter()
            .rfind(|record| matches!(record, Record::Session(_)))
        else {
            panic!("expected session markers to remain");
        };
        assert_eq!(newest.note, format!("restart {}", MAX_SESSION_RECORDS + 39));
    }

    #[test]
    fn retention_is_a_no_op_when_nothing_has_aged() {
        let now = 30 * DAY_MS;
        let records = vec![epoch(now - HOUR_MS, Granularity::Hour, 1)];

        let retention = apply_retention(records.clone(), now, 14, 400);

        assert!(!retention.changed);
        assert_eq!(retention.records, records);
    }

    #[test]
    fn compaction_folds_into_a_day_row_that_already_exists() {
        let now = 60 * DAY_MS;
        let day = 5 * DAY_MS;
        let records = vec![
            epoch(day, Granularity::Day, 100),
            epoch(day + 3 * HOUR_MS, Granularity::Hour, 7),
        ];

        let retention = apply_retention(records, now, 14, 400);

        assert_eq!(retention.records.len(), 1, "one day gets one row");
        let Record::Epoch(rolled) = &retention.records[0] else {
            panic!("expected an epoch record");
        };
        assert_eq!(rolled.counters.requests_fronted, 107);
    }

    #[test]
    fn peer_sets_stay_capped_when_days_merge() {
        let now = 60 * DAY_MS;
        let day = 5 * DAY_MS;
        let mut first = epoch(day, Granularity::Hour, 1);
        let mut second = epoch(day + HOUR_MS, Granularity::Hour, 1);
        if let Record::Epoch(record) = &mut first {
            record.peers = (0..MAX_PEERS_PER_EPOCH)
                .map(|n| format!("a{n:03}"))
                .collect();
        }
        if let Record::Epoch(record) = &mut second {
            record.peers = (0..MAX_PEERS_PER_EPOCH)
                .map(|n| format!("b{n:03}"))
                .collect();
        }

        let retention = apply_retention(vec![first, second], now, 14, 400);

        let Record::Epoch(rolled) = &retention.records[0] else {
            panic!("expected an epoch record");
        };
        assert_eq!(rolled.peers.len(), MAX_PEERS_PER_EPOCH);
        assert!(rolled.peers_truncated, "the overflow has to be admitted");
    }

    #[test]
    fn journal_round_trips_through_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("tdcc-ledger-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let journal = Journal::new(&dir);

        journal.append(&epoch(0, Granularity::Hour, 1)).unwrap();
        journal
            .append(&epoch(HOUR_MS, Granularity::Hour, 2))
            .unwrap();
        let loaded = journal.load().unwrap();
        assert_eq!(loaded.records.len(), 2);
        assert!(journal.journal_bytes() > 0);

        journal.rewrite(&loaded.records[..1]).unwrap();
        assert_eq!(journal.load().unwrap().records.len(), 1);

        let cursor = Cursor {
            v: RECORD_VERSION,
            at_ms: 42,
            cumulative: Cumulative {
                requests_fronted: 7,
                ..Cumulative::default()
            },
        };
        journal.store_cursor(&cursor).unwrap();
        assert_eq!(journal.load_cursor(), Some(cursor));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_reads_as_an_empty_journal() {
        let journal = Journal::new(std::env::temp_dir().join("tdcc-ledger-does-not-exist"));

        assert_eq!(journal.load().unwrap(), LoadedJournal::default());
        assert_eq!(journal.load_cursor(), None);
        assert_eq!(journal.journal_bytes(), 0);
    }
}
