//! The record of what the policy did, which is what makes dry-run useful.
//!
//! An operator who installs this plugin and writes nothing still gets a log of
//! what a policy *would* have matched, so the first policy they write can come
//! from their own traffic instead of guesswork.
//!
//! **What is retained:** identifiers and sizes — model id, peer id, owner id,
//! request kind, token counts, and the decision. **What is not:** anything from
//! the body of the request. There is no field for prompt text anywhere in this
//! plugin, so there is nothing here to leak. An operator who wants even less
//! can set `observe = 0`, which keeps the counters and drops the per-request
//! rows.

use std::collections::{BTreeMap, HashMap, VecDeque};

use schemars::JsonSchema;
use serde::Serialize;

/// Upper bound on distinct keys in a counter map, so a reload that renames
/// every rule cannot grow the map without limit.
const MAX_COUNTER_KEYS: usize = 1_024;
/// How many entries a "top values" breakdown returns.
const TOP_VALUES: usize = 10;
/// Counter key used when a decision came from the policy default.
const NO_RULE: &str = "(default)";
/// Counter key used once [`MAX_COUNTER_KEYS`] distinct keys exist.
const OTHER: &str = "(other)";

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observation {
    /// Unix epoch milliseconds at which the decision was made.
    pub at_epoch_ms: i64,
    /// The answer the caller received: `allow` or `deny`.
    pub decision: String,
    /// False while the policy is in dry-run: the decision was reported but not
    /// applied.
    pub enforced: bool,
    /// True when an enforcing policy would have refused this request.
    pub would_deny: bool,
    /// Stable outcome code, for example `policy.deny_rule`.
    pub code: String,
    /// Rule that decided, when a rule decided.
    pub rule_id: Option<String>,
    pub model: Option<String>,
    pub peer: Option<String>,
    pub owner: Option<String>,
    pub kind: Option<String>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct Counters {
    /// Requests evaluated since this plugin process started.
    pub evaluated: u64,
    /// Requests the caller was told to proceed with.
    pub allowed: u64,
    /// Requests actually refused.
    pub denied: u64,
    /// Requests an enforcing policy would have refused. In dry-run this climbs
    /// while `denied` stays at zero — that gap is the point of dry-run.
    pub would_deny: u64,
    /// Decisions by outcome code.
    pub by_code: BTreeMap<String, u64>,
    /// Decisions by deciding rule.
    pub by_rule: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TopValue {
    pub value: String,
    pub count: u64,
}

/// Ring buffer of recent decisions plus lifetime counters.
///
/// Counters are lifetime totals; the ring holds the last `capacity` decisions.
/// The two disagree on purpose — the counters survive a busy hour, the ring
/// shows what just happened.
#[derive(Debug)]
pub struct Ledger {
    capacity: usize,
    entries: VecDeque<Observation>,
    counters: Counters,
}

impl Ledger {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
            counters: Counters::default(),
        }
    }

    /// Resize on reload. Shrinking drops the oldest entries immediately rather
    /// than waiting for them to age out.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.trim();
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn retained(&self) -> usize {
        self.entries.len()
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    pub fn record(&mut self, observation: Observation) {
        self.counters.evaluated += 1;
        if observation.decision == "deny" {
            self.counters.denied += 1;
        } else {
            self.counters.allowed += 1;
        }
        if observation.would_deny {
            self.counters.would_deny += 1;
        }
        bump(&mut self.counters.by_code, &observation.code);
        bump(
            &mut self.counters.by_rule,
            observation.rule_id.as_deref().unwrap_or(NO_RULE),
        );

        if self.capacity == 0 {
            return;
        }
        self.entries.push_back(observation);
        self.trim();
    }

    /// Most recent decisions first.
    pub fn recent(&self, limit: usize) -> Vec<Observation> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    /// The most frequently seen values of one field across the retained ring.
    ///
    /// Computed on demand rather than counted continuously: the ring is already
    /// bounded, so this cannot be turned into unbounded state by a caller
    /// inventing model names.
    pub fn top_values(&self, select: fn(&Observation) -> Option<&String>) -> Vec<TopValue> {
        let mut counts: HashMap<&str, u64> = HashMap::new();
        for entry in &self.entries {
            if let Some(value) = select(entry) {
                *counts.entry(value.as_str()).or_default() += 1;
            }
        }
        let mut ranked: Vec<TopValue> = counts
            .into_iter()
            .map(|(value, count)| TopValue {
                value: value.to_string(),
                count,
            })
            .collect();
        // Ties break on the value so the output is stable between calls.
        ranked.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.value.cmp(&right.value))
        });
        ranked.truncate(TOP_VALUES);
        ranked
    }

    fn trim(&mut self) {
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }
}

fn bump(counters: &mut BTreeMap<String, u64>, key: &str) {
    if let Some(count) = counters.get_mut(key) {
        *count += 1;
        return;
    }
    if counters.len() >= MAX_COUNTER_KEYS {
        *counters.entry(OTHER.to_string()).or_default() += 1;
        return;
    }
    counters.insert(key.to_string(), 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(decision: &str, would_deny: bool, model: &str) -> Observation {
        Observation {
            at_epoch_ms: 1,
            decision: decision.to_string(),
            enforced: false,
            would_deny,
            code: "policy.deny_rule".to_string(),
            rule_id: Some("a-rule".to_string()),
            model: Some(model.to_string()),
            peer: None,
            owner: None,
            kind: None,
            context_tokens: None,
            max_output_tokens: None,
        }
    }

    #[test]
    fn dry_run_counts_the_refusals_it_did_not_make() {
        let mut ledger = Ledger::new(10);
        ledger.record(observation("allow", true, "qwen/qwen3-8b"));
        ledger.record(observation("allow", false, "qwen/qwen3-8b"));

        let counters = ledger.counters();
        assert_eq!(counters.evaluated, 2);
        assert_eq!(counters.allowed, 2);
        assert_eq!(counters.denied, 0);
        assert_eq!(counters.would_deny, 1);
        assert_eq!(counters.by_rule.get("a-rule"), Some(&2));
    }

    #[test]
    fn the_ring_keeps_the_newest_entries_and_reports_them_newest_first() {
        let mut ledger = Ledger::new(2);
        for index in 0..5 {
            ledger.record(observation("allow", false, &format!("model-{index}")));
        }

        assert_eq!(ledger.retained(), 2);
        let recent = ledger.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].model.as_deref(), Some("model-4"));
        assert_eq!(recent[1].model.as_deref(), Some("model-3"));
        // Counters are lifetime totals, not ring contents.
        assert_eq!(ledger.counters().evaluated, 5);
    }

    #[test]
    fn a_capacity_of_zero_keeps_counters_and_no_rows() {
        let mut ledger = Ledger::new(0);
        ledger.record(observation("deny", true, "qwen/qwen3-8b"));

        assert_eq!(ledger.retained(), 0);
        assert!(ledger.recent(10).is_empty());
        assert_eq!(ledger.counters().denied, 1);
    }

    #[test]
    fn shrinking_the_ring_drops_the_oldest_entries_immediately() {
        let mut ledger = Ledger::new(5);
        for index in 0..5 {
            ledger.record(observation("allow", false, &format!("model-{index}")));
        }

        ledger.set_capacity(2);

        assert_eq!(ledger.retained(), 2);
        assert_eq!(ledger.recent(1)[0].model.as_deref(), Some("model-4"));
    }

    #[test]
    fn top_values_rank_by_count_then_name() {
        let mut ledger = Ledger::new(10);
        for _ in 0..3 {
            ledger.record(observation("allow", false, "busy/model"));
        }
        ledger.record(observation("allow", false, "quiet/model"));

        let top = ledger.top_values(|entry| entry.model.as_ref());

        assert_eq!(top[0].value, "busy/model");
        assert_eq!(top[0].count, 3);
        assert_eq!(top[1].value, "quiet/model");
    }

    #[test]
    fn counter_keys_are_capped_so_a_rename_storm_cannot_grow_them() {
        let mut counters = BTreeMap::new();
        for index in 0..MAX_COUNTER_KEYS + 50 {
            bump(&mut counters, &format!("rule-{index}"));
        }

        assert_eq!(counters.len(), MAX_COUNTER_KEYS + 1);
        assert_eq!(counters.get(OTHER), Some(&50));
    }
}
