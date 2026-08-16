//! Wall-clock reading, isolated to one place.
//!
//! Reading the clock is the only impure input policy evaluation has. Confining
//! it to [`Timestamp::now`] means every time-of-day and rate-limit rule can be
//! tested by constructing a timestamp instead of waiting for Tuesday.

use std::fmt;

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use serde::Serialize;

/// Which clock time-of-day rules are read against.
///
/// `local` is the default because an operator who writes "22:00-06:00" means
/// 22:00 on the machine in their house. A headless server whose system zone is
/// UTC anyway can pin `timezone = "utc"` and stop thinking about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    Local,
    Utc,
}

impl Zone {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "utc" => Ok(Self::Utc),
            other => Err(format!(
                "unknown timezone '{other}'; expected \"local\" or \"utc\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Utc => "utc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// Accepts the three-letter and the full English name, any case.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mon" | "monday" => Ok(Self::Mon),
            "tue" | "tues" | "tuesday" => Ok(Self::Tue),
            "wed" | "weds" | "wednesday" => Ok(Self::Wed),
            "thu" | "thur" | "thurs" | "thursday" => Ok(Self::Thu),
            "fri" | "friday" => Ok(Self::Fri),
            "sat" | "saturday" => Ok(Self::Sat),
            "sun" | "sunday" => Ok(Self::Sun),
            other => Err(format!(
                "unknown weekday '{other}'; expected mon, tue, wed, thu, fri, sat, or sun"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }

    fn from_chrono(value: chrono::Weekday) -> Self {
        match value {
            chrono::Weekday::Mon => Self::Mon,
            chrono::Weekday::Tue => Self::Tue,
            chrono::Weekday::Wed => Self::Wed,
            chrono::Weekday::Thu => Self::Thu,
            chrono::Weekday::Fri => Self::Fri,
            chrono::Weekday::Sat => Self::Sat,
            chrono::Weekday::Sun => Self::Sun,
        }
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A single instant, reduced to exactly the three facts policy cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Timestamp {
    /// Unix epoch milliseconds. Used only for rate-limit accounting, which is
    /// why it stays absolute while the other two fields are zone-dependent.
    pub epoch_millis: i64,
    /// Minutes since midnight in the configured zone, `0..=1439`.
    pub minute_of_day: u16,
    /// Day of week in the configured zone.
    pub weekday: Weekday,
}

impl Timestamp {
    pub fn now(zone: Zone) -> Self {
        match zone {
            Zone::Local => Self::from_datetime(Local::now()),
            Zone::Utc => Self::from_datetime(Utc::now()),
        }
    }

    fn from_datetime<Tz: TimeZone>(value: DateTime<Tz>) -> Self {
        Self {
            epoch_millis: value.timestamp_millis(),
            // `hour()` is 0..=23 and `minute()` is 0..=59, so the sum fits u16.
            minute_of_day: (value.hour() * 60 + value.minute()) as u16,
            weekday: Weekday::from_chrono(value.weekday()),
        }
    }

    /// Test constructor. `minute_of_day` is clamped rather than asserted so a
    /// malformed fixture cannot panic a handler.
    #[cfg(test)]
    pub fn at(epoch_millis: i64, minute_of_day: u16, weekday: Weekday) -> Self {
        Self {
            epoch_millis,
            minute_of_day: minute_of_day.min(1439),
            weekday,
        }
    }
}

/// A half-open time-of-day window: start inclusive, end exclusive.
///
/// `22:00-06:00` wraps past midnight, which is the whole point — "I donate this
/// machine overnight" is the most common thing an operator wants to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourWindow {
    start_minute: u16,
    end_minute: u16,
}

impl HourWindow {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let trimmed = spec.trim();
        let Some((start, end)) = trimmed.split_once('-') else {
            return Err(format!(
                "invalid time window '{trimmed}'; expected \"HH:MM-HH:MM\", for example \"22:00-06:00\""
            ));
        };
        // Both halves quote the whole window, so an operator reading the error
        // can find the line without counting colons.
        let start_minute = parse_clock_time(start)
            .map_err(|error| format!("in time window '{trimmed}': {error}"))?;
        let end_minute = parse_clock_time(end)
            .map_err(|error| format!("in time window '{trimmed}': {error}"))?;
        if start_minute == end_minute {
            // Equal endpoints could mean "zero minutes" or "the whole day".
            // Refusing beats guessing on a rule that gates a machine.
            return Err(format!(
                "time window '{trimmed}' starts and ends at the same minute; write \"00:00-23:59\" for a whole day"
            ));
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }

    pub fn contains(&self, minute_of_day: u16) -> bool {
        if self.start_minute <= self.end_minute {
            minute_of_day >= self.start_minute && minute_of_day < self.end_minute
        } else {
            // Wrapped window: inside means "after the start, or before the end".
            minute_of_day >= self.start_minute || minute_of_day < self.end_minute
        }
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
impl Serialize for HourWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn parse_clock_time(value: &str) -> Result<u16, String> {
    let trimmed = value.trim();
    let Some((hours, minutes)) = trimmed.split_once(':') else {
        return Err(format!(
            "invalid clock time '{trimmed}'; expected \"HH:MM\""
        ));
    };
    let hours: u16 = hours
        .trim()
        .parse()
        .map_err(|_| format!("invalid hour '{}' in '{trimmed}'", hours.trim()))?;
    let minutes: u16 = minutes
        .trim()
        .parse()
        .map_err(|_| format!("invalid minute '{}' in '{trimmed}'", minutes.trim()))?;
    if hours > 23 {
        return Err(format!("hour {hours} in '{trimmed}' is not in 00..=23"));
    }
    if minutes > 59 {
        return Err(format!("minute {minutes} in '{trimmed}' is not in 00..=59"));
    }
    Ok(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_window_and_treats_the_end_as_exclusive() {
        let window = HourWindow::parse("09:00-17:00").expect("valid window");

        assert!(window.contains(9 * 60));
        assert!(window.contains(16 * 60 + 59));
        assert!(!window.contains(17 * 60));
        assert!(!window.contains(8 * 60 + 59));
    }

    #[test]
    fn a_window_that_crosses_midnight_wraps() {
        let window = HourWindow::parse("22:00-06:00").expect("valid window");

        assert!(window.contains(22 * 60));
        assert!(window.contains(23 * 60 + 59));
        assert!(window.contains(0));
        assert!(window.contains(5 * 60 + 59));
        assert!(!window.contains(6 * 60));
        assert!(!window.contains(12 * 60));
    }

    #[test]
    fn window_round_trips_through_display() {
        let window = HourWindow::parse("07:05-19:45").expect("valid window");

        assert_eq!(window.to_string(), "07:05-19:45");
    }

    #[test]
    fn malformed_windows_are_rejected_with_a_usable_message() {
        for spec in [
            "",
            "22:00",
            "22:00_06:00",
            "25:00-06:00",
            "22:60-06:00",
            "ten-six",
        ] {
            let error = HourWindow::parse(spec).expect_err("must be rejected");
            assert!(!error.is_empty(), "empty error for {spec:?}");
        }
    }

    #[test]
    fn an_ambiguous_zero_length_window_is_rejected() {
        let error = HourWindow::parse("09:00-09:00").expect_err("must be rejected");

        assert!(error.contains("same minute"), "unexpected message: {error}");
    }

    #[test]
    fn weekday_accepts_short_and_long_forms_in_any_case() {
        assert_eq!(Weekday::parse("Mon").expect("mon"), Weekday::Mon);
        assert_eq!(Weekday::parse(" TUESDAY ").expect("tue"), Weekday::Tue);
        assert_eq!(Weekday::parse("sun").expect("sun"), Weekday::Sun);
        assert!(Weekday::parse("caturday").is_err());
    }

    #[test]
    fn zone_parses_both_supported_values() {
        assert_eq!(Zone::parse("Local").expect("local"), Zone::Local);
        assert_eq!(Zone::parse("UTC").expect("utc"), Zone::Utc);
        assert!(Zone::parse("america/denver").is_err());
    }

    #[test]
    fn now_produces_an_in_range_minute_of_day() {
        let timestamp = Timestamp::now(Zone::Utc);

        assert!(timestamp.minute_of_day <= 1439);
        assert!(timestamp.epoch_millis > 0);
    }
}
