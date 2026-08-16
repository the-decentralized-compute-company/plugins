//! Wall-clock reading, time windows, and the one place a naive local time is
//! turned into an instant.
//!
//! [`crate::cron`] is pure arithmetic over `NaiveDateTime`. This module is
//! where that meets a real calendar: which zone the operator meant, what
//! happens on the two days a year a local wall clock skips or repeats an hour,
//! and whether "22:00-06:00" contains the current minute.
//!
//! # DST, decided rather than discovered
//!
//! * **Spring forward.** A schedule that names a wall-clock time inside the
//!   skipped hour has no instant to run at, so it does not run that day. It is
//!   not moved to 03:00 and it is not run twice the next day.
//! * **Fall back.** A schedule that names a wall-clock time inside the repeated
//!   hour runs at the **first** of the two, once. Running twice would double a
//!   job's cost for one night a year on hardware somebody lent you.
//!
//! Both fall out of taking the earliest valid mapping and requiring each
//! occurrence to be strictly later than the previous one. An operator who wants
//! neither surprise sets `timezone = "utc"`, which has no transitions.

use std::fmt;

use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc};

use crate::cron::Schedule;

/// Upper bound on retries while stepping over a DST gap.
///
/// A gap is at most a few hours, so even a minutely schedule clears it well
/// inside this. The bound exists so a pathological zone cannot spin a tick.
const MAX_MAPPING_ATTEMPTS: usize = 512;

/// Upper bound on occurrences examined when checking a schedule against a
/// window. Enough for a minutely schedule to walk into any window; small enough
/// that a load-time check stays instant.
const MAX_WINDOW_PROBE_OCCURRENCES: usize = 4_096;

/// Which clock schedules and windows are read against.
///
/// `local` is the default because an operator who writes `0 3 * * *` means 3am
/// on the machine in their house. A headless server whose system zone is UTC
/// anyway can pin `timezone = "utc"` and stop thinking about transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    Local,
    Utc,
}

impl Zone {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" | "system" => Ok(Self::Local),
            "utc" => Ok(Self::Utc),
            other => Err(format!(
                "unknown timezone \"{other}\"; this plugin understands \"local\" (the machine's \
                 own zone) and \"utc\". Named zones such as \"Europe/Berlin\" would need a \
                 bundled timezone database, which this plugin does not ship."
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Utc => "utc",
        }
    }
}

/// Current wall-clock time in Unix milliseconds.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// The wall-clock reading `ms` corresponds to in `zone`.
pub fn naive_at(zone: Zone, ms: i64) -> NaiveDateTime {
    let instant = DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH);
    match zone {
        Zone::Utc => instant.naive_utc(),
        Zone::Local => instant.with_timezone(&Local).naive_local(),
    }
}

/// The instant a wall-clock reading corresponds to, or `None` when that reading
/// does not exist in `zone` (the spring-forward gap).
pub fn to_ms(zone: Zone, naive: NaiveDateTime) -> Option<i64> {
    match zone {
        Zone::Utc => Some(Utc.from_utc_datetime(&naive).timestamp_millis()),
        // `earliest()` resolves the fall-back repeat to the first of the two
        // instants and yields `None` for a time inside the spring-forward gap.
        Zone::Local => Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|instant| instant.timestamp_millis()),
    }
}

/// Minutes since midnight in `zone`, `0..=1439`.
pub fn minute_of_day(zone: Zone, ms: i64) -> u16 {
    let naive = naive_at(zone, ms);
    // `hour()` is 0..=23 and `minute()` is 0..=59, so the sum fits a u16.
    (naive.hour() * 60 + naive.minute()) as u16
}

/// The first instant strictly after `after_ms` at which `schedule` fires.
///
/// Returns `None` when the schedule has no occurrence inside the cron search
/// horizon, which for a syntactically valid expression means it can never fire.
pub fn next_occurrence(schedule: &Schedule, zone: Zone, after_ms: i64) -> Option<i64> {
    let mut cursor = naive_at(zone, after_ms);
    for _ in 0..MAX_MAPPING_ATTEMPTS {
        let naive = schedule.next_naive_after(cursor)?;
        match to_ms(zone, naive) {
            // Strictly forward: a fall-back repeat can otherwise map two
            // distinct wall-clock minutes onto the same instant.
            Some(ms) if ms > after_ms => return Some(ms),
            _ => cursor = naive,
        }
    }
    None
}

/// Format an instant as UTC, `YYYY-MM-DDThh:mm:ssZ`.
pub fn format_utc(ms: i64) -> String {
    match DateTime::from_timestamp_millis(ms) {
        Some(instant) => instant.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => format!("{ms}ms"),
    }
}

/// Format an instant in the configured zone, with its offset, so an operator
/// reading a tool response sees the time their schedule was written in.
pub fn format_in(zone: Zone, ms: i64) -> String {
    let Some(instant) = DateTime::from_timestamp_millis(ms) else {
        return format!("{ms}ms");
    };
    match zone {
        Zone::Utc => instant.to_rfc3339_opts(SecondsFormat::Secs, true),
        Zone::Local => instant
            .with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::Secs, false),
    }
}

/// A half-open time-of-day window: start inclusive, end exclusive.
///
/// `22:00-06:00` wraps past midnight, which is the whole point — "this machine
/// is yours overnight" is the most common thing an operator wants to say about
/// hardware they also use themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HourWindow {
    start_minute: u16,
    end_minute: u16,
}

impl HourWindow {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let trimmed = spec.trim();
        let Some((start, end)) = trimmed.split_once('-') else {
            return Err(format!(
                "invalid window \"{trimmed}\"; expected \"HH:MM-HH:MM\", for example \
                 \"22:00-06:00\""
            ));
        };
        let start_minute =
            parse_clock_time(start).map_err(|error| format!("in window \"{trimmed}\": {error}"))?;
        let end_minute =
            parse_clock_time(end).map_err(|error| format!("in window \"{trimmed}\": {error}"))?;
        if start_minute == end_minute {
            // Equal endpoints could mean "no minutes" or "every minute".
            // Refusing beats guessing on a setting that gates a machine.
            return Err(format!(
                "window \"{trimmed}\" starts and ends at the same minute; write \"00:00-23:59\" \
                 for a whole day, or leave the window out entirely"
            ));
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }

    pub fn contains_minute(&self, minute_of_day: u16) -> bool {
        if self.start_minute <= self.end_minute {
            minute_of_day >= self.start_minute && minute_of_day < self.end_minute
        } else {
            // Wrapped: inside means after the start, or before the end.
            minute_of_day >= self.start_minute || minute_of_day < self.end_minute
        }
    }

    /// Whether `ms` falls inside this window, read on `zone`'s clock.
    pub fn contains(&self, zone: Zone, ms: i64) -> bool {
        self.contains_minute(minute_of_day(zone, ms))
    }
}

impl fmt::Display for HourWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:02}:{:02}-{:02}:{:02}",
            self.start_minute / 60,
            self.start_minute % 60,
            self.end_minute / 60,
            self.end_minute % 60
        )
    }
}

/// Serialized as the spec an operator wrote, not as a pair of integers.
impl serde::Serialize for HourWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn parse_clock_time(value: &str) -> Result<u16, String> {
    let trimmed = value.trim();
    let Some((hours, minutes)) = trimmed.split_once(':') else {
        return Err(format!(
            "invalid clock time \"{trimmed}\"; expected \"HH:MM\""
        ));
    };
    let hours: u16 = hours
        .trim()
        .parse()
        .map_err(|_| format!("invalid hour \"{}\" in \"{trimmed}\"", hours.trim()))?;
    let minutes: u16 = minutes
        .trim()
        .parse()
        .map_err(|_| format!("invalid minute \"{}\" in \"{trimmed}\"", minutes.trim()))?;
    if hours > 23 {
        return Err(format!("hour {hours} in \"{trimmed}\" is not in 00..=23"));
    }
    if minutes > 59 {
        return Err(format!(
            "minute {minutes} in \"{trimmed}\" is not in 00..=59"
        ));
    }
    Ok(hours * 60 + minutes)
}

/// The first occurrence of `schedule` that also falls inside `window`.
///
/// Used at load time: a schedule and a window that never coincide is a
/// contradiction the operator wrote by accident, and finding it when the file
/// is read is far better than discovering it after a week of nothing happening.
pub fn first_occurrence_inside(
    schedule: &Schedule,
    zone: Zone,
    window: Option<&HourWindow>,
    after_ms: i64,
) -> Option<i64> {
    let mut cursor = after_ms;
    for _ in 0..MAX_WINDOW_PROBE_OCCURRENCES {
        let occurrence = next_occurrence(schedule, zone, cursor)?;
        match window {
            None => return Some(occurrence),
            Some(window) if window.contains(zone, occurrence) => return Some(occurrence),
            Some(_) => cursor = occurrence,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_ms(text: &str) -> i64 {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
            .expect("fixture parses")
            .and_utc()
            .timestamp_millis()
    }

    #[test]
    fn zone_parses_both_supported_spellings_and_refuses_named_zones() {
        assert_eq!(Zone::parse("Local").expect("local"), Zone::Local);
        assert_eq!(Zone::parse(" UTC ").expect("utc"), Zone::Utc);

        let error = Zone::parse("Europe/Berlin").expect_err("named zones are refused");
        assert!(error.contains("timezone database"), "{error}");
    }

    #[test]
    fn utc_occurrences_are_exact_instants() {
        let schedule = Schedule::parse("0 3 * * *").expect("parses");

        let next = next_occurrence(&schedule, Zone::Utc, utc_ms("2026-03-01 02:59:00"))
            .expect("fires today");

        assert_eq!(format_utc(next), "2026-03-01T03:00:00Z");
    }

    #[test]
    fn successive_occurrences_move_strictly_forward_in_the_machines_own_zone() {
        // Runs in whatever zone the test machine has, DST or not: the property
        // that matters is that the scheduler can never be handed the same
        // instant twice, which is what would double-fire a job.
        let schedule = Schedule::parse("*/30 * * * *").expect("parses");
        let mut cursor = now_ms();

        for _ in 0..96 {
            let next = next_occurrence(&schedule, Zone::Local, cursor).expect("fires");
            assert!(
                next > cursor,
                "occurrence {next} did not advance past {cursor}"
            );
            // And the instant it returned really is a minute the schedule
            // selects, once read back on the same clock.
            assert!(
                schedule.matches(naive_at(Zone::Local, next)),
                "{next} does not match the schedule when read back"
            );
            cursor = next;
        }
    }

    #[test]
    fn a_wall_clock_reading_round_trips_through_an_instant() {
        let ms = utc_ms("2026-06-15 12:34:00");

        assert_eq!(to_ms(Zone::Utc, naive_at(Zone::Utc, ms)), Some(ms));
        // In the local zone the round trip holds except inside a DST gap, which
        // no real instant can land in.
        assert_eq!(to_ms(Zone::Local, naive_at(Zone::Local, ms)), Some(ms));
    }

    #[test]
    fn windows_are_half_open_and_wrap_past_midnight() {
        let day = HourWindow::parse("09:00-17:00").expect("valid");
        assert!(day.contains_minute(9 * 60));
        assert!(day.contains_minute(16 * 60 + 59));
        assert!(!day.contains_minute(17 * 60));
        assert!(!day.contains_minute(8 * 60 + 59));

        let night = HourWindow::parse("22:00-06:00").expect("valid");
        assert!(night.contains_minute(22 * 60));
        assert!(night.contains_minute(0));
        assert!(night.contains_minute(5 * 60 + 59));
        assert!(!night.contains_minute(6 * 60));
        assert!(!night.contains_minute(12 * 60));
    }

    #[test]
    fn a_window_round_trips_through_display_and_serialization() {
        let window = HourWindow::parse(" 07:05-19:45 ").expect("valid");

        assert_eq!(window.to_string(), "07:05-19:45");
        assert_eq!(
            serde_json::to_value(window).expect("serializes"),
            serde_json::json!("07:05-19:45")
        );
    }

    #[test]
    fn malformed_windows_are_refused_with_a_usable_message() {
        for spec in [
            "",
            "22:00",
            "22:00_06:00",
            "25:00-06:00",
            "22:60-06:00",
            "ten-six",
        ] {
            let error = HourWindow::parse(spec).expect_err("must be refused");
            assert!(!error.is_empty(), "empty error for {spec:?}");
        }

        let error = HourWindow::parse("09:00-09:00").expect_err("must be refused");
        assert!(error.contains("same minute"), "{error}");
    }

    #[test]
    fn a_schedule_that_never_lands_in_its_window_is_detectable() {
        let noon_daily = Schedule::parse("0 12 * * *").expect("parses");
        let overnight = HourWindow::parse("22:00-06:00").expect("valid");
        let from = utc_ms("2026-03-01 00:00:00");

        assert_eq!(
            first_occurrence_inside(&noon_daily, Zone::Utc, Some(&overnight), from),
            None,
            "a daily noon job can never run inside an overnight window"
        );

        // The same schedule with no window, and a compatible schedule with one,
        // both resolve immediately.
        assert!(first_occurrence_inside(&noon_daily, Zone::Utc, None, from).is_some());
        let half_hourly = Schedule::parse("*/30 * * * *").expect("parses");
        let inside = first_occurrence_inside(&half_hourly, Zone::Utc, Some(&overnight), from)
            .expect("a half-hourly job lands in any window");
        assert!(overnight.contains(Zone::Utc, inside));
    }

    #[test]
    fn formatting_names_the_zone_it_used() {
        let ms = utc_ms("2026-03-01 03:00:00");

        assert_eq!(format_utc(ms), "2026-03-01T03:00:00Z");
        assert_eq!(format_in(Zone::Utc, ms), "2026-03-01T03:00:00Z");
        // The local rendering carries an explicit offset rather than pretending
        // to be UTC, whatever the machine's zone happens to be.
        let local = format_in(Zone::Local, ms);
        assert!(
            local.ends_with('Z') || local.contains('+') || local.matches('-').count() >= 3,
            "{local} carries no offset"
        );
    }
}
