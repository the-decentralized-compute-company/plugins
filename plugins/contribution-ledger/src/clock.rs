//! Wall-clock helpers: epoch bucketing and UTC formatting.
//!
//! Everything the ledger stores is stamped in Unix milliseconds, because that
//! is the only timestamp form that survives a JSON round-trip without a date
//! library. The formatting here exists purely so a human reading a tool
//! response does not have to convert epoch milliseconds in their head.
//!
//! No dependency on a calendar crate: the plugin already links protobuf and an
//! async runtime, and a date crate would be a third supply-chain surface for
//! something that is forty lines of well-known integer arithmetic.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const SECOND_MS: u64 = 1_000;
pub const HOUR_MS: u64 = 3_600_000;
pub const DAY_MS: u64 = 86_400_000;

/// How coarse a stored bucket is.
///
/// Fresh history is recorded hourly; once it ages past the compaction horizon
/// the hourly rows for a day are merged into one daily row. See
/// [`crate::journal`] for the retention policy this feeds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Hour,
    Day,
}

impl Granularity {
    pub const fn width_ms(self) -> u64 {
        match self {
            Self::Hour => HOUR_MS,
            Self::Day => DAY_MS,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }
}

/// Current wall-clock time in Unix milliseconds.
///
/// A clock set before 1970 is the only way this can fail, and there is nothing
/// useful to do about it in a background sampler, so it saturates at 0.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Start of the bucket containing `ms`, aligned to UTC.
pub const fn bucket_start(ms: u64, granularity: Granularity) -> u64 {
    let width = granularity.width_ms();
    ms - (ms % width)
}

/// Format a Unix-millisecond timestamp as `YYYY-MM-DDThh:mm:ssZ`.
///
/// Truncates sub-second precision: every timestamp the ledger emits is a bucket
/// boundary or a poll instant, and neither is meaningful below a second.
pub fn format_utc(ms: u64) -> String {
    let days = (ms / DAY_MS) as i64;
    let time_of_day = ms % DAY_MS;
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / HOUR_MS;
    let minute = (time_of_day % HOUR_MS) / 60_000;
    let second = (time_of_day % 60_000) / SECOND_MS;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a count of days since 1970-01-01 into a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range of
/// dates a `u64` millisecond timestamp can express.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_align_to_utc_boundaries() {
        let ms = 1_787_063_831_000;
        assert_eq!(format_utc(ms), "2026-08-18T14:37:11Z");
        assert_eq!(
            format_utc(bucket_start(ms, Granularity::Hour)),
            "2026-08-18T14:00:00Z"
        );
        assert_eq!(
            format_utc(bucket_start(ms, Granularity::Day)),
            "2026-08-18T00:00:00Z"
        );
    }

    #[test]
    fn bucket_start_is_idempotent() {
        let ms = 1_787_063_831_000;
        for granularity in [Granularity::Hour, Granularity::Day] {
            let once = bucket_start(ms, granularity);
            assert_eq!(bucket_start(once, granularity), once);
        }
    }

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, to exercise the era arithmetic rather than a happy path.
        assert_eq!(format_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        // 2100 is not a leap year; the century rule has to survive that.
        assert_eq!(format_utc(4_107_542_400_000), "2100-03-01T00:00:00Z");
    }

    #[test]
    fn sub_second_precision_is_truncated_not_rounded() {
        assert_eq!(format_utc(1_999), "1970-01-01T00:00:01Z");
    }
}
