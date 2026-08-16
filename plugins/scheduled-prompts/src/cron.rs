//! A five-field cron expression and the search for its next occurrence.
//!
//! Everything here is pure and works on `NaiveDateTime` — a wall-clock reading
//! with no zone attached. Turning that into an instant is [`crate::clock`]'s
//! job, because that is where DST lives and this is where the arithmetic lives.
//!
//! # The dialect
//!
//! `minute hour day-of-month month day-of-week`, minute resolution, with `*`,
//! `a`, `a,b`, `a-b`, `*/n`, `a/n`, and `a-b/n` in every field. Months accept
//! `jan`/`january` and days of week accept `sun`/`sunday`; `7` is a second
//! spelling of Sunday, as in Vixie cron. Six-field (seconds) expressions and
//! the Quartz extensions `?`, `L`, `W`, and `#` are **not** accepted, and are
//! rejected by name rather than silently misread — a schedule copied from a
//! Quartz example that quietly meant something else would be the worst possible
//! failure for a plugin that spends someone else's GPU time.
//!
//! # The day rule, which surprises people
//!
//! When **both** day-of-month and day-of-week are restricted, a day matches if
//! **either** does — `0 0 1 * mon` is "the 1st, and every Monday", not "Mondays
//! that fall on the 1st". That is Vixie cron's rule, it is what `crontab(5)`
//! documents, and it is what anyone who has written a crontab expects. When
//! only one of the two is restricted, that one alone decides.
//!
//! # `@reboot` is deliberately absent
//!
//! The named shorthands `@hourly`, `@daily`, `@midnight`, `@weekly`,
//! `@monthly`, `@yearly`, and `@annually` are accepted. `@reboot` is not:
//! "fire because the process started" is exactly the wake-up stampede the
//! misfire policy in [`crate::decide`] exists to control, and shipping it would
//! hand the problem back under a friendlier name.

use std::fmt;

use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

/// How far ahead [`Schedule::next_naive_after`] looks before giving up.
///
/// A whole leap-year cycle plus slack, so `0 0 29 2 *` (29 February) resolves
/// while `0 0 30 2 *` (30 February, which never happens) terminates instead of
/// scanning to the end of the calendar.
pub const SEARCH_HORIZON_DAYS: i64 = 1_500;

/// One cron field, as a bitmask over its legal values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Field {
    /// Bit `n` is set when value `n` is selected. Every field fits in 64 bits.
    mask: u64,
    /// True unless the field was written as a bare `*`.
    ///
    /// Only day-of-month and day-of-week use this: "was it restricted?" is what
    /// selects between the AND and the OR rule described above.
    restricted: bool,
}

impl Field {
    fn contains(self, value: u32) -> bool {
        value < 64 && self.mask & (1u64 << value) != 0
    }
}

/// A parsed cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
    /// The expression exactly as the operator wrote it, for display and for
    /// echoing back in tool responses.
    spec: String,
}

impl Schedule {
    /// Parse a cron expression or a named shorthand.
    ///
    /// Errors name the offending field and token: the operator is reading this
    /// out of a TOML file and needs to know which of five whitespace-separated
    /// things to look at.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("schedule is empty; write a cron expression such as \"0 3 * * *\"".into());
        }

        let expanded = expand_shorthand(trimmed)?;
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "schedule \"{trimmed}\" has {} fields; this plugin takes exactly 5 \
                 (minute hour day-of-month month day-of-week), for example \"*/15 * * * *\"",
                fields.len()
            ));
        }

        for field in &fields {
            reject_unsupported_syntax(field, trimmed)?;
        }

        Ok(Self {
            minute: parse_field(fields[0], 0, 59, &[], "minute")?,
            hour: parse_field(fields[1], 0, 23, &[], "hour")?,
            day_of_month: parse_field(fields[2], 1, 31, &[], "day-of-month")?,
            month: parse_field(fields[3], 1, 12, MONTH_NAMES, "month")?,
            day_of_week: parse_day_of_week(fields[4])?,
            spec: trimmed.to_string(),
        })
    }

    /// The expression as written.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// Whether this expression selects the given wall-clock minute.
    ///
    /// Used for one thing in production — deciding whether a stored cursor
    /// could still have come from this expression, which is how an edited
    /// schedule takes effect. It is also the independent oracle the search is
    /// checked against, in this module and in [`crate::clock`].
    pub fn matches(&self, at: NaiveDateTime) -> bool {
        self.day_matches(at.date())
            && self.hour.contains(at.hour())
            && self.minute.contains(at.minute())
    }

    /// The first minute strictly after `after` that this expression selects.
    ///
    /// Returns `None` when nothing matches inside [`SEARCH_HORIZON_DAYS`],
    /// which for a syntactically valid expression means it can never fire. The
    /// caller treats that as a configuration error rather than as a job that
    /// silently never runs.
    pub fn next_naive_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut candidate = truncate_to_minute(after).checked_add_signed(TimeDelta::minutes(1))?;
        let deadline = after
            .date()
            .checked_add_signed(TimeDelta::days(SEARCH_HORIZON_DAYS))?;

        loop {
            if candidate.date() > deadline {
                return None;
            }
            // Skip a whole day at a time when the date cannot match: a monthly
            // or yearly expression is otherwise 1,440 pointless minute steps
            // for every day it is not due.
            if !self.day_matches(candidate.date()) {
                candidate = candidate
                    .date()
                    .checked_add_signed(TimeDelta::days(1))?
                    .and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hour.contains(candidate.hour()) {
                // Rolls to 00:00 of the next day when the hour was 23, which
                // the day check above then re-evaluates.
                candidate = candidate
                    .with_minute(0)?
                    .checked_add_signed(TimeDelta::hours(1))?;
                continue;
            }
            if !self.minute.contains(candidate.minute()) {
                candidate = candidate.checked_add_signed(TimeDelta::minutes(1))?;
                continue;
            }
            return Some(candidate);
        }
    }

    /// Vixie cron's day rule. See the module documentation.
    fn day_matches(&self, date: NaiveDate) -> bool {
        if !self.month.contains(date.month()) {
            return false;
        }
        let by_month_day = self.day_of_month.contains(date.day());
        let by_week_day = self
            .day_of_week
            .contains(date.weekday().num_days_from_sunday());

        match (self.day_of_month.restricted, self.day_of_week.restricted) {
            (true, true) => by_month_day || by_week_day,
            (true, false) => by_month_day,
            (false, true) => by_week_day,
            (false, false) => true,
        }
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.spec)
    }
}

/// Serialized as the expression an operator wrote, not as five bitmasks.
impl serde::Serialize for Schedule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.spec)
    }
}

fn truncate_to_minute(value: NaiveDateTime) -> NaiveDateTime {
    value
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
        .unwrap_or(value)
}

const MONTH_NAMES: &[(&str, u32)] = &[
    ("jan", 1),
    ("january", 1),
    ("feb", 2),
    ("february", 2),
    ("mar", 3),
    ("march", 3),
    ("apr", 4),
    ("april", 4),
    ("may", 5),
    ("jun", 6),
    ("june", 6),
    ("jul", 7),
    ("july", 7),
    ("aug", 8),
    ("august", 8),
    ("sep", 9),
    ("september", 9),
    ("oct", 10),
    ("october", 10),
    ("nov", 11),
    ("november", 11),
    ("dec", 12),
    ("december", 12),
];

const DAY_NAMES: &[(&str, u32)] = &[
    ("sun", 0),
    ("sunday", 0),
    ("mon", 1),
    ("monday", 1),
    ("tue", 2),
    ("tues", 2),
    ("tuesday", 2),
    ("wed", 3),
    ("wednesday", 3),
    ("thu", 4),
    ("thur", 4),
    ("thurs", 4),
    ("thursday", 4),
    ("fri", 5),
    ("friday", 5),
    ("sat", 6),
    ("saturday", 6),
];

fn expand_shorthand(spec: &str) -> Result<String, String> {
    if !spec.starts_with('@') {
        return Ok(spec.to_string());
    }
    match spec.to_ascii_lowercase().as_str() {
        "@hourly" => Ok("0 * * * *".into()),
        "@daily" | "@midnight" => Ok("0 0 * * *".into()),
        "@weekly" => Ok("0 0 * * 0".into()),
        "@monthly" => Ok("0 0 1 * *".into()),
        "@yearly" | "@annually" => Ok("0 0 1 1 *".into()),
        "@reboot" => Err(
            "@reboot is not supported: a job that fires because the process started is exactly \
             the wake-up stampede this plugin's misfire policy exists to prevent. Write a real \
             schedule, and set misfire = \"run_once\" if you want one catch-up run on wake."
                .into(),
        ),
        other => Err(format!(
            "unknown shorthand \"{other}\"; supported: @hourly, @daily, @midnight, @weekly, \
             @monthly, @yearly, @annually"
        )),
    }
}

/// Refuse dialects this parser does not implement, rather than misreading them.
///
/// Quartz `?` in a day field means "no value here", `L` means "last", `W` means
/// "nearest weekday", and `#` means "nth weekday of the month". Reading any of
/// them as an ordinary token would produce a schedule that fires on days the
/// operator did not choose.
///
/// `L` and `W` are only rejected in their Quartz shapes — as a whole token, or
/// attached to a number — because `jul` and `wed` are perfectly ordinary
/// values that happen to contain those letters.
fn reject_unsupported_syntax(field: &str, spec: &str) -> Result<(), String> {
    for (character, meaning) in [
        ('?', "Quartz \"no specific value\""),
        ('#', "Quartz \"nth weekday of month\""),
        ('%', "the crontab command-line percent escape"),
    ] {
        if field.contains(character) {
            return Err(unsupported(spec, character, meaning));
        }
    }

    for token in field.split(',') {
        let token = token.trim().to_ascii_uppercase();
        let (head, tail) = token.split_at(token.len().saturating_sub(1));
        let numeric_head =
            !head.is_empty() && head.chars().all(|character| character.is_ascii_digit());

        if token == "L" || token == "LW" || (numeric_head && tail == "L") {
            return Err(unsupported(spec, 'L', "Quartz \"last\""));
        }
        if token == "W" || (numeric_head && tail == "W") {
            return Err(unsupported(spec, 'W', "Quartz \"nearest weekday\""));
        }
    }
    Ok(())
}

fn unsupported(spec: &str, character: char, meaning: &str) -> String {
    format!(
        "schedule \"{spec}\" uses `{character}` ({meaning}), which this plugin does not \
         implement. Supported syntax is `*`, `a`, `a,b`, `a-b`, `*/n`, `a/n`, and `a-b/n`."
    )
}

fn parse_day_of_week(raw: &str) -> Result<Field, String> {
    // `7` is a second spelling of Sunday, so the field is parsed over 0..=7 and
    // bit 7 is folded onto bit 0 afterwards.
    let mut field = parse_field(raw, 0, 7, DAY_NAMES, "day-of-week")?;
    if field.contains(7) {
        field.mask |= 1;
        field.mask &= !(1u64 << 7);
    }
    Ok(field)
}

fn parse_field(
    raw: &str,
    min: u32,
    max: u32,
    names: &[(&str, u32)],
    field_name: &str,
) -> Result<Field, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!("{field_name} field is empty"));
    }

    let mut mask = 0u64;
    for term in raw.split(',') {
        let term = term.trim();
        if term.is_empty() {
            return Err(format!(
                "{field_name} field \"{raw}\" has an empty item; write `1,2,3` without a trailing \
                 comma"
            ));
        }

        let (range_part, step) = match term.split_once('/') {
            Some((range_part, step_part)) => {
                let step: u32 = step_part.trim().parse().map_err(|_| {
                    format!("{field_name} step \"{step_part}\" in \"{term}\" is not a number")
                })?;
                if step == 0 {
                    return Err(format!(
                        "{field_name} step in \"{term}\" is 0; a step must be at least 1"
                    ));
                }
                (range_part.trim(), step)
            }
            None => (term, 1),
        };

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((low, high)) = range_part.split_once('-') {
            let low = parse_value(low, min, max, names, field_name)?;
            let high = parse_value(high, min, max, names, field_name)?;
            if low > high {
                return Err(format!(
                    "{field_name} range \"{range_part}\" runs backwards; write \"{high}-{low}\", \
                     or two items separated by a comma if you meant both ends"
                ));
            }
            (low, high)
        } else {
            let single = parse_value(range_part, min, max, names, field_name)?;
            // `5/20` means "from 5 onwards, every 20", the same as `5-max/20`.
            if step > 1 {
                (single, max)
            } else {
                (single, single)
            }
        };

        let mut value = start;
        while value <= end {
            mask |= 1u64 << value;
            value += step;
        }
    }

    Ok(Field {
        mask,
        restricted: raw != "*",
    })
}

fn parse_value(
    raw: &str,
    min: u32,
    max: u32,
    names: &[(&str, u32)],
    field_name: &str,
) -> Result<u32, String> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(format!("{field_name} field has an empty value"));
    }

    let lowered = token.to_ascii_lowercase();
    if let Some((_, value)) = names.iter().find(|(name, _)| *name == lowered) {
        return Ok(*value);
    }

    let value: u32 = token.parse().map_err(|_| {
        if names.is_empty() {
            format!("{field_name} value \"{token}\" is not a number in {min}..={max}")
        } else {
            format!(
                "{field_name} value \"{token}\" is neither a number in {min}..={max} nor a name \
                 like \"{}\"",
                names[0].0
            )
        }
    })?;
    if value < min || value > max {
        return Err(format!(
            "{field_name} value {value} is outside {min}..={max}"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").expect("fixture parses")
    }

    fn next(spec: &str, from: &str) -> String {
        Schedule::parse(spec)
            .unwrap_or_else(|error| panic!("{spec}: {error}"))
            .next_naive_after(at(from))
            .unwrap_or_else(|| panic!("{spec} never fires after {from}"))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    #[test]
    fn every_minute_advances_by_one_minute() {
        assert_eq!(
            next("* * * * *", "2026-03-01 08:14:00"),
            "2026-03-01 08:15:00"
        );
    }

    #[test]
    fn seconds_in_the_starting_instant_are_ignored_rather_than_rounded_up() {
        // 08:14:59 is inside the 08:14 minute, so the next fire is 08:15, not
        // 08:16. Getting this wrong drops one occurrence on every tick.
        assert_eq!(
            next("* * * * *", "2026-03-01 08:14:59"),
            "2026-03-01 08:15:00"
        );
    }

    #[test]
    fn a_daily_expression_lands_on_the_next_day_when_the_hour_has_passed() {
        assert_eq!(
            next("0 3 * * *", "2026-03-01 03:00:00"),
            "2026-03-02 03:00:00"
        );
        assert_eq!(
            next("0 3 * * *", "2026-03-01 02:59:00"),
            "2026-03-01 03:00:00"
        );
    }

    #[test]
    fn steps_lists_and_ranges_select_what_they_read_like() {
        assert_eq!(
            next("*/15 * * * *", "2026-03-01 08:01:00"),
            "2026-03-01 08:15:00"
        );
        assert_eq!(
            next("0,30 * * * *", "2026-03-01 08:01:00"),
            "2026-03-01 08:30:00"
        );
        assert_eq!(
            next("0 9-17 * * *", "2026-03-01 08:30:00"),
            "2026-03-01 09:00:00"
        );
        assert_eq!(
            next("0 9-17 * * *", "2026-03-01 17:30:00"),
            "2026-03-02 09:00:00"
        );
        // `a-b/n`: every second hour between 8 and 14.
        assert_eq!(
            next("0 8-14/2 * * *", "2026-03-01 09:00:00"),
            "2026-03-01 10:00:00"
        );
        // `a/n`: from 5 past, every 20 minutes.
        assert_eq!(
            next("5/20 * * * *", "2026-03-01 08:06:00"),
            "2026-03-01 08:25:00"
        );
    }

    #[test]
    fn month_and_weekday_names_are_accepted_short_long_and_in_any_case() {
        assert_eq!(
            next("0 0 1 JAN *", "2026-03-01 00:00:00"),
            "2027-01-01 00:00:00"
        );
        // 2026-03-02 is a Monday, so the next Sunday is the 8th.
        assert_eq!(
            next("0 0 * * Sunday", "2026-03-02 00:00:00"),
            "2026-03-08 00:00:00"
        );
        assert_eq!(
            next("0 0 * * sat,sun", "2026-03-02 00:00:00"),
            "2026-03-07 00:00:00"
        );
        assert_eq!(
            next("0 0 * * mon-fri", "2026-03-07 00:00:00"),
            "2026-03-09 00:00:00"
        );
    }

    #[test]
    fn sunday_is_both_zero_and_seven() {
        let zero = Schedule::parse("0 0 * * 0").expect("0 parses");
        let seven = Schedule::parse("0 0 * * 7").expect("7 parses");

        assert_eq!(
            zero.next_naive_after(at("2026-03-02 00:00:00")),
            seven.next_naive_after(at("2026-03-02 00:00:00"))
        );
    }

    #[test]
    fn a_restricted_day_of_month_and_day_of_week_are_ored_as_vixie_cron_does() {
        // "The 1st, and every Monday". 2026-03-01 is a Sunday, so from the 1st
        // the next fire is Monday the 2nd — not the 1st of April.
        assert_eq!(
            next("0 0 1 * mon", "2026-03-01 00:00:00"),
            "2026-03-02 00:00:00"
        );
    }

    #[test]
    fn one_restricted_day_field_decides_alone() {
        // Only day-of-month is restricted: weekdays are irrelevant.
        assert_eq!(
            next("0 0 15 * *", "2026-03-01 00:00:00"),
            "2026-03-15 00:00:00"
        );
        // Only day-of-week is restricted: the day of the month is irrelevant.
        assert_eq!(
            next("0 0 * * wed", "2026-03-01 00:00:00"),
            "2026-03-04 00:00:00"
        );
    }

    #[test]
    fn shorthands_expand_to_the_expressions_they_name() {
        assert_eq!(
            next("@hourly", "2026-03-01 08:30:00"),
            "2026-03-01 09:00:00"
        );
        assert_eq!(next("@daily", "2026-03-01 08:30:00"), "2026-03-02 00:00:00");
        assert_eq!(
            next("@midnight", "2026-03-01 08:30:00"),
            "2026-03-02 00:00:00"
        );
        // 2026-03-01 is a Sunday; @weekly is Sunday midnight.
        assert_eq!(
            next("@weekly", "2026-03-01 08:30:00"),
            "2026-03-08 00:00:00"
        );
        assert_eq!(
            next("@monthly", "2026-03-01 08:30:00"),
            "2026-04-01 00:00:00"
        );
        assert_eq!(
            next("@yearly", "2026-03-01 08:30:00"),
            "2027-01-01 00:00:00"
        );
    }

    #[test]
    fn reboot_is_refused_with_the_reason_and_the_alternative() {
        let error = Schedule::parse("@reboot").expect_err("@reboot is refused");

        assert!(error.contains("misfire"), "{error}");
        assert!(error.contains("run_once"), "{error}");
    }

    #[test]
    fn leap_day_resolves_and_the_thirtieth_of_february_terminates() {
        assert_eq!(
            next("0 0 29 2 *", "2026-03-01 00:00:00"),
            "2028-02-29 00:00:00"
        );

        let never = Schedule::parse("0 0 30 2 *").expect("syntactically valid");
        assert_eq!(
            never.next_naive_after(at("2026-03-01 00:00:00")),
            None,
            "30 February never happens; the search must terminate rather than run forever"
        );
    }

    #[test]
    fn the_search_and_the_matcher_agree_minute_by_minute() {
        let schedule = Schedule::parse("*/10 9-17 * * mon-fri").expect("parses");
        let mut cursor = at("2026-03-02 08:00:00");

        for _ in 0..40 {
            let fire = schedule.next_naive_after(cursor).expect("fires");
            assert!(
                schedule.matches(fire),
                "{fire} was returned but does not match"
            );
            assert!(fire > cursor, "the search must move strictly forward");
            // Every minute strictly between the two must not match, or the
            // search skipped an occurrence.
            let mut between = cursor + TimeDelta::minutes(1);
            while between < fire {
                assert!(
                    !schedule.matches(between),
                    "{between} was skipped but matches"
                );
                between += TimeDelta::minutes(1);
            }
            cursor = fire;
        }
    }

    #[test]
    fn quartz_syntax_is_refused_by_name_rather_than_misread() {
        for spec in ["0 0 ? * MON", "0 0 L * *", "0 0 15W * *", "0 0 * * MON#2"] {
            let error = Schedule::parse(spec).expect_err("must be refused");
            assert!(error.contains("does not implement"), "{spec}: {error}");
        }
    }

    #[test]
    fn month_and_day_names_containing_l_or_w_are_not_mistaken_for_quartz() {
        // `jul` contains an L and `wed` contains a W. Rejecting either would
        // make two perfectly ordinary schedules unwritable.
        assert_eq!(
            next("0 0 4 jul *", "2026-03-01 00:00:00"),
            "2026-07-04 00:00:00"
        );
        assert_eq!(
            next("0 0 * * wed", "2026-03-01 00:00:00"),
            "2026-03-04 00:00:00"
        );
    }

    #[test]
    fn a_six_field_expression_is_refused_with_the_field_count() {
        let error = Schedule::parse("0 */5 * * * *").expect_err("six fields are refused");

        assert!(error.contains("6 fields"), "{error}");
        assert!(error.contains("exactly 5"), "{error}");
    }

    #[test]
    fn malformed_fields_name_the_field_and_the_token() {
        let cases = [
            ("60 * * * *", "minute"),
            ("* 24 * * *", "hour"),
            ("* * 0 * *", "day-of-month"),
            ("* * * 13 *", "month"),
            ("* * * * 8", "day-of-week"),
            ("*/0 * * * *", "step"),
            ("5-1 * * * *", "backwards"),
            ("1,,2 * * * *", "empty item"),
            ("x * * * *", "minute"),
            ("* * * xyz *", "month"),
        ];
        for (spec, expected) in cases {
            let error = Schedule::parse(spec).expect_err("must be refused");
            assert!(error.contains(expected), "{spec} -> {error}");
        }
    }

    #[test]
    fn an_empty_schedule_says_what_one_looks_like() {
        let error = Schedule::parse("   ").expect_err("must be refused");

        assert!(error.contains("0 3 * * *"), "{error}");
    }

    #[test]
    fn the_expression_round_trips_through_display_and_serialization() {
        let schedule = Schedule::parse("  0 3 * * 1-5  ").expect("parses");

        assert_eq!(schedule.spec(), "0 3 * * 1-5");
        assert_eq!(schedule.to_string(), "0 3 * * 1-5");
        assert_eq!(
            serde_json::to_value(&schedule).expect("serializes"),
            serde_json::json!("0 3 * * 1-5")
        );
    }
}
