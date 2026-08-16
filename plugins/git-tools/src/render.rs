//! Turning libgit2 values into the JSON a model reads, and keeping that JSON
//! inside a budget.
//!
//! Three jobs live here, all of them pure functions so all of them are tested
//! directly:
//!
//! - **Timestamps.** Git stores a commit time as "seconds since the epoch" plus
//!   "minutes east of UTC". Both are rendered, and an ISO-8601 string is
//!   derived from them with a hand-rolled civil-calendar conversion rather than
//!   a date crate — it is thirty lines, it is exhaustively testable, and this
//!   plugin's release dependency set stays as small as the job.
//! - **Signatures.** Author and committer identity, with the optional email
//!   redaction an operator turns on with `--redact-emails`.
//! - **Budgets.** Every list and every block of text a caller can grow is
//!   pushed through a [`Budget`], and a response says plainly when one stopped
//!   it early rather than presenting a short answer as a complete one.

use serde::Serialize;

use crate::settings::Disclosure;

/// The string that replaces an email address under `--redact-emails`.
pub const REDACTED_EMAIL: &str = "<redacted>";

/// Characters of a commit id shown as its short form.
///
/// Twelve, not seven: git's default abbreviation grows with repository size,
/// and a fixed twelve is both unambiguous in practice and stable across calls,
/// which matters when a model quotes one back as an argument.
pub const SHORT_OID_LEN: usize = 12;

pub fn short_oid(oid: git2::Oid) -> String {
    let full = oid.to_string();
    full.chars().take(SHORT_OID_LEN).collect()
}

/// An author or committer as a response carries them.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Identity {
    /// Display name recorded in the commit. Never redacted — it is what makes
    /// a blame answer useful.
    pub name: String,
    /// Email recorded in the commit, or `<redacted>` under `--redact-emails`.
    pub email: String,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    /// Minutes east of UTC, as recorded in the commit.
    pub offset_minutes: i32,
    /// ISO-8601 rendering of `timestamp` in the commit's own offset, or `null`
    /// when the recorded time is outside the years 1..=9999.
    pub date: Option<String>,
}

/// Build an [`Identity`] from a libgit2 signature.
///
/// A signature whose name or email is not valid UTF-8 is rendered lossily
/// rather than dropped: an unreadable name is still evidence, and refusing the
/// whole commit because one byte is Latin-1 would make old histories
/// unreadable.
pub fn identity(signature: &git2::Signature<'_>, disclosure: Disclosure) -> Identity {
    let when = signature.when();
    Identity {
        name: String::from_utf8_lossy(signature.name_bytes()).into_owned(),
        email: if disclosure.redact_emails {
            REDACTED_EMAIL.to_string()
        } else {
            String::from_utf8_lossy(signature.email_bytes()).into_owned()
        },
        timestamp: when.seconds(),
        offset_minutes: when.offset_minutes(),
        date: format_timestamp(when.seconds(), when.offset_minutes()),
    }
}

/// Render an epoch second and a UTC offset as ISO-8601.
///
/// Returns `None` when the result would fall outside years 1..=9999, which
/// happens with commits carrying a corrupt or joke timestamp. The raw
/// `timestamp` is still reported in that case, so nothing is hidden — only the
/// derived string is withheld, because a formatted year 0 is a lie about
/// precision.
///
/// An offset outside ±23:59 is treated as zero for rendering only; the raw
/// value is still reported alongside.
pub fn format_timestamp(seconds: i64, offset_minutes: i32) -> Option<String> {
    let offset_minutes = if offset_minutes.abs() >= 24 * 60 {
        0
    } else {
        offset_minutes
    };
    let local = seconds.checked_add(i64::from(offset_minutes).checked_mul(60)?)?;

    let days = floor_div(local, 86_400);
    let second_of_day = floor_mod(local, 86_400);
    let (year, month, day) = civil_from_days(days)?;

    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;

    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let offset_hours = offset_minutes.abs() / 60;
    let offset_rest = offset_minutes.abs() % 60;

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{sign}{offset_hours:02}:\
         {offset_rest:02}"
    ))
}

fn floor_div(numerator: i64, denominator: i64) -> i64 {
    let quotient = numerator / denominator;
    if numerator % denominator != 0 && ((numerator < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

fn floor_mod(numerator: i64, denominator: i64) -> i64 {
    numerator - floor_div(numerator, denominator) * denominator
}

/// Days since 1970-01-01 to a proleptic Gregorian year/month/day.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range this
/// function accepts and needs no lookup tables. Returns `None` outside years
/// 1..=9999 so the caller can decline to format rather than emit a year that
/// no reader would believe.
fn civil_from_days(days: i64) -> Option<(i64, u32, u32)> {
    let shifted = days + 719_468;
    let era = floor_div(shifted, 146_097);
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32; // [1, 31]
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32; // [1, 12]
    let year = year + i64::from(month <= 2);

    if !(1..=9_999).contains(&year) {
        return None;
    }
    Some((year, month, day))
}

/// The inverse of [`civil_from_days`], for turning a caller's `since` /
/// `until` date into an epoch second.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = floor_div(year, 400);
    let year_of_era = year - era * 400; // [0, 399]
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year; // [0, 146096]
    era * 146_097 + day_of_era - 719_468
}

/// Parse a caller-supplied date bound into an epoch second.
///
/// Three forms, all interpreted as UTC, because a date filter that silently
/// meant "the node's local midnight" would give different answers on different
/// machines in the mesh:
///
/// - `YYYY-MM-DD` — midnight at the start of that day
/// - `YYYY-MM-DDTHH:MM:SS`, with an optional trailing `Z`
/// - a bare integer, taken as seconds since the epoch (negative allowed)
///
/// Nothing else is accepted. Relative expressions like `2 weeks ago` are
/// deliberately absent: they need a "now" that differs between the asking node
/// and the answering one, and a filter whose meaning depends on which machine
/// evaluated it is worse than no filter.
pub fn parse_time_bound(input: &str) -> Result<i64, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("a date must not be empty".to_string());
    }
    if let Ok(seconds) = text.parse::<i64>() {
        return Ok(seconds);
    }

    let (date, time) = match text.split_once(['T', ' ']) {
        Some((date, time)) => (date, Some(time.trim_end_matches('Z'))),
        None => (text, None),
    };

    let mut date_parts = date.split('-');
    let year = numeric_part(date_parts.next(), 4, "year")?;
    let month = numeric_part(date_parts.next(), 2, "month")? as u32;
    let day = numeric_part(date_parts.next(), 2, "day")? as u32;
    if date_parts.next().is_some() {
        return Err(format!("{text:?} has too many parts to be a date"));
    }
    if !(1..=9_999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("{text:?} is not a real calendar date"));
    }

    let (hour, minute, second) = match time {
        None => (0, 0, 0),
        Some(time) => {
            let mut parts = time.split(':');
            let hour = numeric_part(parts.next(), 2, "hour")?;
            let minute = numeric_part(parts.next(), 2, "minute")?;
            let second = match parts.next() {
                Some(value) => numeric_part(Some(value), 2, "second")?,
                None => 0,
            };
            if parts.next().is_some() {
                return Err(format!("{text:?} has too many parts to be a time"));
            }
            if !(0..=23).contains(&hour)
                || !(0..=59).contains(&minute)
                || !(0..=60).contains(&second)
            {
                return Err(format!("{text:?} is not a real time of day"));
            }
            (hour, minute, second)
        }
    };

    // Round-trip through the calendar: this is what rejects 2023-02-30, which
    // passes every range check above and is still not a day that existed.
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != Some((year, month, day)) {
        return Err(format!("{text:?} is not a real calendar date"));
    }

    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn numeric_part(part: Option<&str>, width: usize, name: &str) -> Result<i64, String> {
    let part = part.ok_or_else(|| format!("a date is missing its {name}"))?;
    if part.len() != width || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "the {name} must be {width} digits, got {part:?}. Use YYYY-MM-DD, \
             YYYY-MM-DDTHH:MM:SSZ, or a plain epoch second"
        ));
    }
    part.parse::<i64>()
        .map_err(|_| format!("the {name} {part:?} is not a number"))
}

/// A byte budget shared by everything one response emits.
///
/// The point is that `truncated` is sticky: once anything is dropped the whole
/// response says so, so a caller never treats a shortened diff as the complete
/// one.
#[derive(Debug, Clone)]
pub struct Budget {
    limit: usize,
    used: usize,
    truncated: bool,
    reason: Option<String>,
}

impl Budget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: 0,
            truncated: false,
            reason: None,
        }
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    pub fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Record that a cap stopped the work early. The first reason wins, since
    /// it is the one that actually shortened the answer.
    pub fn mark_truncated(&mut self, reason: impl Into<String>) {
        self.truncated = true;
        if self.reason.is_none() {
            self.reason = Some(reason.into());
        }
    }

    /// Append as much of `chunk` as fits, on a UTF-8 boundary.
    ///
    /// Returns false when the chunk did not fit whole, which is the signal to
    /// stop feeding the budget rather than to keep trying smaller pieces.
    pub fn push_str(&mut self, sink: &mut String, chunk: &str, reason: &str) -> bool {
        if chunk.len() <= self.remaining() {
            self.used += chunk.len();
            sink.push_str(chunk);
            return true;
        }
        let take = floor_char_boundary(chunk, self.remaining());
        self.used += take;
        sink.push_str(&chunk[..take]);
        self.mark_truncated(reason);
        false
    }
}

/// Largest index `<= max` that lies on a character boundary of `text`.
///
/// `str::floor_char_boundary` is still unstable, and truncating a diff hunk in
/// the middle of a multi-byte character would panic on the slice.
pub fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut index = max;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Shorten `text` to at most `limit` bytes on a character boundary.
///
/// Returns the text and whether anything was dropped. Nothing is appended —
/// an ellipsis inside a commit message is indistinguishable from one the author
/// typed, and the boolean is unambiguous.
pub fn truncate_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let take = floor_char_boundary(text, limit);
    (text[..take].to_string(), true)
}

/// Decode raw commit-message bytes for a response.
///
/// Lossy on purpose: a commit message in a legacy encoding is still worth
/// reading, and git does not require UTF-8.
pub fn message_text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// The libgit2 name for a change, as a stable lowercase string.
///
/// These strings appear in responses, so they are part of the contract: a
/// caller may match on them.
pub fn delta_status(status: git2::Delta) -> &'static str {
    match status {
        git2::Delta::Unmodified => "unmodified",
        git2::Delta::Added => "added",
        git2::Delta::Deleted => "deleted",
        git2::Delta::Modified => "modified",
        git2::Delta::Renamed => "renamed",
        git2::Delta::Copied => "copied",
        git2::Delta::Ignored => "ignored",
        git2::Delta::Untracked => "untracked",
        git2::Delta::Typechange => "typechange",
        git2::Delta::Unreadable => "unreadable",
        git2::Delta::Conflicted => "conflicted",
    }
}

/// The repository's in-progress operation, as a stable lowercase string.
pub fn repository_state(state: git2::RepositoryState) -> &'static str {
    match state {
        git2::RepositoryState::Clean => "clean",
        git2::RepositoryState::Merge => "merge",
        git2::RepositoryState::Revert => "revert",
        git2::RepositoryState::RevertSequence => "revert_sequence",
        git2::RepositoryState::CherryPick => "cherry_pick",
        git2::RepositoryState::CherryPickSequence => "cherry_pick_sequence",
        git2::RepositoryState::Bisect => "bisect",
        git2::RepositoryState::Rebase => "rebase",
        git2::RepositoryState::RebaseInteractive => "rebase_interactive",
        git2::RepositoryState::RebaseMerge => "rebase_merge",
        git2::RepositoryState::ApplyMailbox => "apply_mailbox",
        git2::RepositoryState::ApplyMailboxOrRebase => "apply_mailbox_or_rebase",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_utc_timestamp_renders_as_iso8601() {
        assert_eq!(
            format_timestamp(0, 0).as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        assert_eq!(
            format_timestamp(1_710_495_667, 0).as_deref(),
            Some("2024-03-15T09:41:07+00:00")
        );
    }

    #[test]
    fn the_commits_own_offset_is_applied_and_reported_in_the_string() {
        // Same instant, three offsets. This is the whole reason git records an
        // offset instead of only an epoch second.
        assert_eq!(
            format_timestamp(1_710_495_667, 60).as_deref(),
            Some("2024-03-15T10:41:07+01:00")
        );
        assert_eq!(
            format_timestamp(1_710_495_667, -300).as_deref(),
            Some("2024-03-15T04:41:07-05:00")
        );
        assert_eq!(
            format_timestamp(1_710_495_667, 330).as_deref(),
            Some("2024-03-15T15:11:07+05:30")
        );
    }

    #[test]
    fn leap_days_and_century_rules_are_right() {
        // 2000 is a leap year, 1900 is not, 2024 is.
        assert_eq!(
            format_timestamp(951_782_400, 0).as_deref(),
            Some("2000-02-29T00:00:00+00:00")
        );
        assert_eq!(
            format_timestamp(1_709_164_800, 0).as_deref(),
            Some("2024-02-29T00:00:00+00:00")
        );
        // 1900-02-28, one day before the non-existent 1900-02-29.
        assert_eq!(
            format_timestamp(-2_203_977_600, 0).as_deref(),
            Some("1900-02-28T00:00:00+00:00")
        );
    }

    #[test]
    fn timestamps_before_the_epoch_render_correctly() {
        assert_eq!(
            format_timestamp(-1, 0).as_deref(),
            Some("1969-12-31T23:59:59+00:00")
        );
        assert_eq!(
            format_timestamp(-86_400, 0).as_deref(),
            Some("1969-12-31T00:00:00+00:00")
        );
    }

    #[test]
    fn an_offset_pushing_across_midnight_moves_the_date() {
        // 1970-01-01T00:30:00Z in +02:00 is still 1970-01-01, but in -01:00 it
        // is 1969-12-31.
        assert_eq!(
            format_timestamp(1_800, -60).as_deref(),
            Some("1969-12-31T23:30:00-01:00")
        );
    }

    #[test]
    fn an_absurd_timestamp_yields_no_date_rather_than_a_fake_one() {
        assert_eq!(format_timestamp(i64::MAX / 2, 0), None);
        assert_eq!(format_timestamp(-100_000_000_000_000, 0), None);
        // The saturating path: adding the offset must not panic.
        assert_eq!(format_timestamp(i64::MAX, 60), None);
        assert_eq!(format_timestamp(i64::MIN, -60), None);
    }

    #[test]
    fn an_impossible_offset_is_rendered_as_utc_without_hiding_the_raw_value() {
        // The Identity carries offset_minutes verbatim; only the string falls
        // back, and it says +00:00 rather than inventing +99:00.
        assert_eq!(
            format_timestamp(0, 5_000).as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn the_calendar_round_trips_in_both_directions() {
        for seconds in [
            0_i64,
            1_710_495_667,
            951_782_400,
            -2_206_483_200,
            -1,
            4_102_444_800,
        ] {
            let rendered = format_timestamp(seconds, 0).expect("in range");
            let parsed = parse_time_bound(&rendered.replace("+00:00", "Z")).expect("parses back");
            assert_eq!(parsed, seconds, "{rendered}");
        }
    }

    #[test]
    fn date_bounds_accept_the_three_documented_forms() {
        assert_eq!(parse_time_bound("1970-01-01").expect("date"), 0);
        assert_eq!(parse_time_bound("2024-03-15").expect("date"), 1_710_460_800);
        assert_eq!(
            parse_time_bound("2024-03-15T08:21:07Z").expect("datetime"),
            1_710_490_867
        );
        assert_eq!(
            parse_time_bound("2024-03-15 08:21:07").expect("space separator"),
            1_710_490_867
        );
        assert_eq!(
            parse_time_bound("2024-03-15T08:21").expect("no seconds"),
            1_710_490_860
        );
        assert_eq!(
            parse_time_bound("1710495667").expect("epoch"),
            1_710_495_667
        );
        assert_eq!(parse_time_bound("-86400").expect("negative epoch"), -86_400);
        assert_eq!(
            parse_time_bound("  2024-03-15  ").expect("trimmed"),
            1_710_460_800
        );
    }

    #[test]
    fn a_date_that_never_existed_is_refused_rather_than_rolled_over() {
        // The check that matters: every part is in range, and the day still is
        // not real. A naive parser turns this into 1 March.
        for input in [
            "2023-02-30",
            "2023-02-29",
            "2023-04-31",
            "2023-13-01",
            "2023-00-10",
        ] {
            let error = parse_time_bound(input).expect_err("not a real date");
            assert!(error.contains("real calendar date"), "{input}: {error}");
        }
        // 2024 is a leap year, so this one is real.
        assert!(parse_time_bound("2024-02-29").is_ok());
    }

    #[test]
    fn a_malformed_date_names_the_accepted_forms() {
        for input in [
            "",
            "yesterday",
            "2 weeks ago",
            "15/03/2024",
            "2024-3-15",
            "2024-03",
        ] {
            let error = parse_time_bound(input).expect_err("not a date");
            assert!(!error.is_empty(), "{input}");
        }
        // Not a mistake: a bare integer is the documented epoch-second form, so
        // "2024" is 1970-01-01T00:33:44Z rather than a malformed year.
        assert_eq!(parse_time_bound("2024").expect("epoch seconds"), 2_024);
        let error = parse_time_bound("2024-3-15").expect_err("not zero padded");
        assert!(error.contains("YYYY-MM-DD"), "{error}");
    }

    #[test]
    fn an_impossible_time_of_day_is_refused() {
        for input in [
            "2024-03-15T25:00:00",
            "2024-03-15T12:61:00",
            "2024-03-15T12:00:99",
        ] {
            let error = parse_time_bound(input).expect_err("not a real time");
            assert!(error.contains("real time of day"), "{input}: {error}");
        }
    }

    #[test]
    fn short_oids_are_a_fixed_twelve_characters() {
        let oid = git2::Oid::from_str("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0").expect("oid");
        assert_eq!(short_oid(oid), "a1b2c3d4e5f6");
        assert_eq!(short_oid(oid).len(), SHORT_OID_LEN);
    }

    #[test]
    fn a_budget_admits_what_fits_and_reports_what_did_not() {
        let mut budget = Budget::new(10);
        let mut sink = String::new();

        assert!(budget.push_str(&mut sink, "12345", "patch cap"));
        assert!(!budget.truncated());
        assert_eq!(budget.remaining(), 5);

        assert!(!budget.push_str(&mut sink, "1234567890", "patch cap"));
        assert_eq!(sink, "1234512345");
        assert!(budget.truncated());
        assert_eq!(budget.reason(), Some("patch cap"));
        assert!(budget.is_exhausted());
    }

    #[test]
    fn the_first_truncation_reason_is_the_one_reported() {
        let mut budget = Budget::new(4);
        let mut sink = String::new();
        budget.push_str(&mut sink, "abcdef", "patch byte cap");
        budget.mark_truncated("file count cap");
        assert_eq!(budget.reason(), Some("patch byte cap"));
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        // Four-byte characters, cut at a byte offset that lands mid-character.
        let text = "🚀🚀🚀";
        for limit in 0..text.len() + 2 {
            let (cut, truncated) = truncate_text(text, limit);
            assert!(text.starts_with(&cut), "limit {limit} produced {cut:?}");
            assert_eq!(truncated, limit < text.len(), "limit {limit}");
            assert_eq!(cut.len() % 4, 0, "limit {limit} split a character");
        }

        let mut budget = Budget::new(6);
        let mut sink = String::new();
        assert!(!budget.push_str(&mut sink, text, "cap"));
        assert_eq!(sink, "🚀");
    }

    #[test]
    fn text_shorter_than_the_limit_is_returned_whole_and_unmarked() {
        let (text, truncated) = truncate_text("fix the parser", 100);
        assert_eq!(text, "fix the parser");
        assert!(!truncated);
    }

    #[test]
    fn invalid_utf8_in_a_message_is_read_lossily_rather_than_dropped() {
        assert_eq!(message_text(b"caf\xe9 fix"), "caf\u{fffd} fix");
    }

    #[test]
    fn an_identity_redacts_only_the_email_and_only_when_asked() {
        let time = git2::Time::new(1_710_495_667, 60);
        let signature = git2::Signature::new("Ada Lovelace", "ada@example.org", &time)
            .expect("a signature with no config dependency");

        let open = identity(&signature, Disclosure::default());
        assert_eq!(open.name, "Ada Lovelace");
        assert_eq!(open.email, "ada@example.org");
        assert_eq!(open.timestamp, 1_710_495_667);
        assert_eq!(open.offset_minutes, 60);
        assert_eq!(open.date.as_deref(), Some("2024-03-15T10:41:07+01:00"));

        let redacted = identity(
            &signature,
            Disclosure {
                redact_emails: true,
                ..Disclosure::default()
            },
        );
        assert_eq!(redacted.name, "Ada Lovelace");
        assert_eq!(redacted.email, REDACTED_EMAIL);
        assert_eq!(redacted.date, open.date);
    }

    #[test]
    fn delta_and_state_strings_are_stable_lowercase_identifiers() {
        assert_eq!(delta_status(git2::Delta::Added), "added");
        assert_eq!(delta_status(git2::Delta::Typechange), "typechange");
        assert_eq!(repository_state(git2::RepositoryState::Clean), "clean");
        assert_eq!(
            repository_state(git2::RepositoryState::RebaseInteractive),
            "rebase_interactive"
        );
    }
}
