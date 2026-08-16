//! Counters, so `status` can answer "is it working?" with numbers.
//!
//! Every path that discards an event increments something here. A webhook
//! integration that quietly drops messages is worse than one that is visibly
//! broken, so nothing is allowed to vanish uncounted.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::format::format_rfc3339_utc;

#[derive(Debug, Default)]
pub struct Stats {
    received: AtomicU64,
    filtered: AtomicU64,
    coalesced: AtomicU64,
    queued: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_no_target: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    last_delivery_ms: AtomicU64,
    /// Already redacted by the caller; see [`crate::config::scrub`].
    last_error: Mutex<Option<String>>,
}

macro_rules! counter {
    ($increment:ident, $get:ident, $field:ident) => {
        pub fn $increment(&self) -> u64 {
            self.$field.fetch_add(1, Ordering::Relaxed) + 1
        }

        pub fn $get(&self) -> u64 {
            self.$field.load(Ordering::Relaxed)
        }
    };
}

impl Stats {
    counter!(record_received, received, received);
    counter!(record_filtered, filtered, filtered);
    counter!(record_coalesced, coalesced, coalesced);
    counter!(record_queued, queued, queued);
    counter!(
        record_dropped_queue_full,
        dropped_queue_full,
        dropped_queue_full
    );
    counter!(
        record_dropped_no_target,
        dropped_no_target,
        dropped_no_target
    );
    counter!(record_delivered, delivered, delivered);
    counter!(record_failed, failed, failed);

    pub fn record_retries(&self, count: u64) {
        self.retries.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_delivery_time(&self, timestamp_ms: u64) {
        self.last_delivery_ms.store(timestamp_ms, Ordering::Relaxed);
    }

    /// `message` must already be scrubbed of the webhook URL.
    pub fn set_last_error(&self, message: Option<String>) {
        // `unwrap_or_else(PoisonError::into_inner)` keeps counters readable even
        // if some other thread panicked while holding the lock: a poisoned
        // mutex must not take the status tool down with it.
        let mut slot = self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = message;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn to_json(&self) -> Value {
        let last_delivery_ms = self.last_delivery_ms.load(Ordering::Relaxed);
        json!({
            "received": self.received(),
            "filtered_out": self.filtered(),
            "coalesced": self.coalesced(),
            "queued": self.queued(),
            "dropped_queue_full": self.dropped_queue_full(),
            "dropped_no_target": self.dropped_no_target(),
            "delivered": self.delivered(),
            "failed": self.failed(),
            "retries": self.retries.load(Ordering::Relaxed),
            "last_delivery_at": (last_delivery_ms > 0)
                .then(|| format_rfc3339_utc(last_delivery_ms)),
            "last_error": self.last_error(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_report_their_new_value() {
        let stats = Stats::default();

        assert_eq!(stats.received(), 0);
        assert_eq!(stats.record_received(), 1);
        assert_eq!(stats.record_received(), 2);
        assert_eq!(stats.received(), 2);
    }

    #[test]
    fn the_snapshot_reports_every_drop_path_separately() {
        let stats = Stats::default();
        stats.record_dropped_queue_full();
        stats.record_dropped_no_target();
        stats.record_filtered();
        stats.record_coalesced();

        let snapshot = stats.to_json();

        assert_eq!(snapshot["dropped_queue_full"], json!(1));
        assert_eq!(snapshot["dropped_no_target"], json!(1));
        assert_eq!(snapshot["filtered_out"], json!(1));
        assert_eq!(snapshot["coalesced"], json!(1));
    }

    #[test]
    fn a_never_delivered_plugin_reports_null_rather_than_the_epoch() {
        let stats = Stats::default();
        assert_eq!(stats.to_json()["last_delivery_at"], Value::Null);

        stats.record_delivery_time(1_700_000_000_000);
        assert_eq!(
            stats.to_json()["last_delivery_at"],
            json!("2023-11-14T22:13:20.000Z")
        );
    }

    #[test]
    fn the_last_error_can_be_set_and_cleared() {
        let stats = Stats::default();
        stats.set_last_error(Some("503 from https://example.com/[redacted]".to_string()));
        assert!(stats.last_error().expect("error").contains("[redacted]"));

        stats.set_last_error(None);
        assert_eq!(stats.last_error(), None);
    }
}
