//! The roll-off task: the one thing in this plugin that runs without being
//! asked.
//!
//! Reads prune as a side effect, so an actively used node never accumulates
//! expired notes. A node that is written to once and then left alone would
//! otherwise keep that note in memory — and on disk — long past its TTL, which
//! would make "notes always expire" true only for readers. One timer fixes
//! that, and it is the whole of the plugin's background work.

use std::sync::Arc;
use std::time::Duration;

use crate::note::epoch_secs;
use crate::store::NoteStore;

/// How often expired notes are swept.
///
/// The shortest TTL this plugin accepts is 60 seconds, so a minute is the
/// coarsest interval that still makes the shortest-lived note disappear roughly
/// when it said it would.
pub const INTERVAL_SECS: u64 = 60;

pub fn spawn(store: Arc<NoteStore>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(INTERVAL_SECS));
        // The first tick of a tokio interval completes immediately; the store
        // has already pruned on load, so skip it.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            store.roll_off(epoch_secs());
        }
    });
}
