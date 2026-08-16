//! The background task that samples the host's counters.
//!
//! One tokio task, one timer, one loop. It never touches the journal directly:
//! it hands each poll result to [`Ledger::tick`], which owns all the state and
//! all the persistence decisions.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use crate::clock::now_ms;
use crate::ledger::Ledger;
use crate::source;

/// Start sampling. Call once; [`Ledger::claim_sampler_slot`] enforces that.
pub fn spawn(ledger: Arc<Ledger>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(ledger.config().poll_secs));
        // Skipped ticks are dropped rather than replayed in a burst: a stalled
        // sampler should resume, not fire ten polls at once. The gap it leaves
        // is visible as unobserved time in the bucket.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // The first tick fires immediately, so the baseline reading is
            // taken as soon as the plugin finishes initialising.
            ticker.tick().await;
            let poll = match ledger.config().source.endpoint() {
                Some(endpoint) => Some(source::poll(endpoint).await),
                // Nothing to sample. The tick still runs, because observed
                // time and mesh-event presence are recorded either way.
                None => None,
            };
            ledger.tick(now_ms(), poll);
        }
    });
}
