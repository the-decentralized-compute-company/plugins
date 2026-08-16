//! Flood control.
//!
//! A peer that flaps once a second for ten minutes is one operational fact, not
//! six hundred chat messages. The coalescer enforces **at most one delivery per
//! key per window**, and carries the number it swallowed into the next delivery
//! for that key, so the count is late but never lost.
//!
//! Deliberately timer-free: a timer per key would mean a task per flapping
//! peer, and the whole point is to not let mesh churn create work. The tradeoff
//! is that the final suppressed run for a key is only reported when that key
//! next fires — which the payload states explicitly rather than hiding.

use std::collections::HashMap;
use std::time::Duration;

/// What the coalescer decided about one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Send it, and mention `suppressed` earlier occurrences of the same key.
    Deliver { suppressed: u32 },
    /// Drop it; an identical event went out less than one window ago.
    Suppress,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    last_delivered_ms: u64,
    last_seen_ms: u64,
    suppressed: u32,
}

pub struct Coalescer {
    window: Duration,
    max_keys: usize,
    entries: HashMap<String, Entry>,
}

impl Coalescer {
    pub fn new(window: Duration, max_keys: usize) -> Self {
        Self {
            window,
            max_keys: max_keys.max(1),
            entries: HashMap::new(),
        }
    }

    pub fn tracked_keys(&self) -> usize {
        self.entries.len()
    }

    pub fn admit(&mut self, key: &str, now_ms: u64) -> Admission {
        if self.window.is_zero() {
            return Admission::Deliver { suppressed: 0 };
        }
        let window_ms = self.window.as_millis() as u64;

        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_seen_ms = now_ms;
            // `saturating_sub` also covers a backwards clock: a jump into the
            // past reads as "0 elapsed", which suppresses rather than floods.
            if now_ms.saturating_sub(entry.last_delivered_ms) >= window_ms {
                let suppressed = entry.suppressed;
                entry.suppressed = 0;
                entry.last_delivered_ms = now_ms;
                return Admission::Deliver { suppressed };
            }
            entry.suppressed = entry.suppressed.saturating_add(1);
            return Admission::Suppress;
        }

        self.make_room(now_ms, window_ms);
        self.entries.insert(
            key.to_string(),
            Entry {
                last_delivered_ms: now_ms,
                last_seen_ms: now_ms,
                suppressed: 0,
            },
        );
        Admission::Deliver { suppressed: 0 }
    }

    /// Keeps the map bounded. Expired entries go first; if that is not enough,
    /// the whole map is dropped. Losing coalescing state costs at most one
    /// extra message per key — cheaper than an unbounded map on a long-lived
    /// node someone else is paying to run.
    fn make_room(&mut self, now_ms: u64, window_ms: u64) {
        if self.entries.len() < self.max_keys {
            return;
        }
        let ttl = (window_ms.saturating_mul(8)).max(60_000);
        self.entries
            .retain(|_, entry| now_ms.saturating_sub(entry.last_seen_ms) < ttl);
        if self.entries.len() >= self.max_keys {
            self.entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(10);

    #[test]
    fn the_first_event_for_a_key_always_goes_out() {
        let mut coalescer = Coalescer::new(WINDOW, 64);
        assert_eq!(
            coalescer.admit("peer.up|a|-", 0),
            Admission::Deliver { suppressed: 0 }
        );
    }

    #[test]
    fn a_flapping_peer_produces_one_message_per_window_not_hundreds() {
        let mut coalescer = Coalescer::new(WINDOW, 64);
        let key = "peer.down|a|-";

        assert_eq!(
            coalescer.admit(key, 0),
            Admission::Deliver { suppressed: 0 }
        );
        let mut delivered = 1;
        for second in 1..400u64 {
            if coalescer.admit(key, second * 1_000) != Admission::Suppress {
                delivered += 1;
            }
        }

        // 400 seconds of one-per-second flapping over a 10s window.
        assert_eq!(delivered, 40, "expected one delivery per window");
    }

    #[test]
    fn the_suppressed_count_rides_along_on_the_next_delivery() {
        let mut coalescer = Coalescer::new(WINDOW, 64);
        let key = "peer.down|a|-";

        coalescer.admit(key, 0);
        for tick in 1..=4 {
            assert_eq!(coalescer.admit(key, tick * 1_000), Admission::Suppress);
        }

        assert_eq!(
            coalescer.admit(key, 10_000),
            Admission::Deliver { suppressed: 4 }
        );
        // The counter resets, so the next window starts from zero.
        assert_eq!(
            coalescer.admit(key, 20_000),
            Admission::Deliver { suppressed: 0 }
        );
    }

    #[test]
    fn distinct_keys_do_not_suppress_each_other() {
        let mut coalescer = Coalescer::new(WINDOW, 64);

        assert_eq!(
            coalescer.admit("peer.down|a|-", 0),
            Admission::Deliver { suppressed: 0 }
        );
        assert_eq!(
            coalescer.admit("peer.down|b|-", 0),
            Admission::Deliver { suppressed: 0 }
        );
        assert_eq!(
            coalescer.admit("model.loaded|a|qwen3-8b", 0),
            Admission::Deliver { suppressed: 0 }
        );
    }

    #[test]
    fn a_zero_window_disables_coalescing_entirely() {
        let mut coalescer = Coalescer::new(Duration::ZERO, 64);
        for tick in 0..100 {
            assert_eq!(
                coalescer.admit("peer.down|a|-", tick),
                Admission::Deliver { suppressed: 0 }
            );
        }
        assert_eq!(
            coalescer.tracked_keys(),
            0,
            "no state is kept when disabled"
        );
    }

    #[test]
    fn a_clock_that_jumps_backwards_suppresses_rather_than_floods() {
        let mut coalescer = Coalescer::new(WINDOW, 64);
        let key = "peer.down|a|-";

        coalescer.admit(key, 1_000_000);
        assert_eq!(coalescer.admit(key, 5), Admission::Suppress);
    }

    #[test]
    fn the_key_map_stays_bounded_under_unique_key_pressure() {
        let mut coalescer = Coalescer::new(WINDOW, 16);
        for index in 0..10_000u64 {
            coalescer.admit(&format!("peer.up|peer-{index}|-"), index);
        }
        assert!(
            coalescer.tracked_keys() <= 16,
            "unbounded growth: {}",
            coalescer.tracked_keys()
        );
    }

    #[test]
    fn expired_entries_are_reclaimed_before_the_map_is_dropped() {
        let mut coalescer = Coalescer::new(WINDOW, 4);
        for index in 0..4u64 {
            coalescer.admit(&format!("old-{index}"), 0);
        }
        assert_eq!(
            coalescer.tracked_keys(),
            4,
            "at the cap, nothing reclaimed yet"
        );
        // Far beyond the 8x window TTL, so the stale entries are reclaimed and
        // the newcomer lands in an otherwise-empty map.
        coalescer.admit("fresh", 10_000_000);
        assert_eq!(coalescer.tracked_keys(), 1);
    }
}
