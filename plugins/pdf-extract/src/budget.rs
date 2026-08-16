//! The wall-clock budget for one call.
//!
//! A PDF is a parser-hostile format and this plugin runs on hardware somebody
//! lent us, so no call may run forever. The budget is enforced in two places
//! that do different jobs:
//!
//! * [`Deadline`] is cooperative. The content-stream walk and the page loop
//!   check it, so work stops on its own between operators rather than being
//!   abandoned mid-parse.
//! * The tool handler additionally races the whole blocking task against the
//!   same duration, so a caller always gets an answer even if the cooperative
//!   check is somewhere the walk never reaches.
//!
//! Both are needed. The race alone would leave a runaway task burning a core
//! after the caller gave up; the cooperative check alone would miss time spent
//! inside a single `lopdf` call.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    pub fn starting_now(budget: Duration) -> Self {
        Self {
            start: Instant::now(),
            budget,
        }
    }

    /// A deadline that never expires, for tests of everything except the
    /// deadline itself.
    #[cfg(test)]
    pub fn unlimited() -> Self {
        Self {
            start: Instant::now(),
            budget: Duration::from_secs(u64::from(u32::MAX)),
        }
    }

    /// A deadline that has already passed, for testing what callers see when
    /// the budget runs out.
    #[cfg(test)]
    pub fn expired() -> Self {
        Self {
            start: Instant::now() - Duration::from_secs(1),
            budget: Duration::ZERO,
        }
    }

    pub fn budget(&self) -> Duration {
        self.budget
    }

    pub fn expired_now(&self) -> bool {
        self.start.elapsed() >= self.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_deadline_has_not_expired_and_an_expired_one_has() {
        assert!(!Deadline::starting_now(Duration::from_secs(30)).expired_now());
        assert!(!Deadline::unlimited().expired_now());
        assert!(Deadline::expired().expired_now());
    }

    #[test]
    fn a_zero_budget_is_expired_immediately_rather_than_running_one_page() {
        assert!(Deadline::starting_now(Duration::ZERO).expired_now());
    }
}
