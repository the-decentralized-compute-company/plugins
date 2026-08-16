//! Timestamped segments: reading them out of a backend's reply, moving them
//! back to absolute time, and stitching overlapping chunks into one transcript.
//!
//! Segments are the valuable half of this plugin. A wall of text lets a model
//! say "they discussed the budget"; segments let it say "at 14:32 she said the
//! budget was already spent", and lets a person jump straight there. So they
//! are parsed carefully, kept structured, and never silently dropped.
//!
//! **On tolerance.** The canonical reply shape is OpenAI's `verbose_json`:
//! `start` and `end` in fractional seconds. Whisper implementations also emit
//! `offsets` in milliseconds, `timestamps` as `HH:MM:SS,mmm` strings, and `t0`
//! / `t1` in centiseconds. Which one arrives depends entirely on the backend an
//! operator pointed this plugin at, so the parser accepts all four rather than
//! returning an empty segment list against a server that was working fine.

use serde::Serialize;
use serde_json::Value;

use crate::plan::Chunk;

/// One timestamped piece of transcript, in seconds from the start of the whole
/// recording.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    /// Position in the returned list, renumbered from zero after stitching.
    pub id: u32,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

impl Segment {
    pub fn new(id: u32, start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            id,
            start,
            end,
            text: text.into(),
        }
    }

    /// Shift into absolute time and re-clamp.
    ///
    /// A backend occasionally returns an end before its start, or a timestamp
    /// past the chunk it was given; both are repaired here rather than passed
    /// on, because a negative-length segment breaks every consumer downstream.
    fn shifted(&self, offset: f64, ceiling: f64) -> Self {
        let start = (self.start + offset).max(0.0).min(ceiling);
        let end = (self.end + offset).max(start).min(ceiling);
        Self {
            id: self.id,
            start,
            end,
            text: self.text.clone(),
        }
    }
}

/// Pull the segment list out of one backend reply.
///
/// Objects that carry no usable timestamp are skipped rather than guessed at:
/// a segment with an invented time is worse than one fewer segment, because a
/// caller cannot tell the difference.
pub fn parse_segments(value: &Value) -> Vec<Segment> {
    let Some(items) = value.get("segments").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(parse_segment)
        .enumerate()
        .map(|(index, mut segment)| {
            segment.id = index as u32;
            segment
        })
        .collect()
}

fn parse_segment(value: &Value) -> Option<Segment> {
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let (start, end) = segment_bounds(value)?;
    if !start.is_finite() || !end.is_finite() {
        return None;
    }
    Some(Segment::new(
        0,
        start.max(0.0),
        end.max(start.max(0.0)),
        text,
    ))
}

/// The four shapes seen in the wild, in order of preference.
fn segment_bounds(value: &Value) -> Option<(f64, f64)> {
    // 1. OpenAI `verbose_json`: fractional seconds.
    if let (Some(start), Some(end)) = (
        value.get("start").and_then(parse_time),
        value.get("end").and_then(parse_time),
    ) {
        return Some((start, end));
    }
    // 2. whisper.cpp's own JSON: milliseconds under `offsets`.
    if let Some(offsets) = value.get("offsets")
        && let (Some(from), Some(to)) = (
            offsets.get("from").and_then(Value::as_f64),
            offsets.get("to").and_then(Value::as_f64),
        )
    {
        return Some((from / 1_000.0, to / 1_000.0));
    }
    // 3. whisper.cpp's human-readable pair: `HH:MM:SS,mmm` strings.
    if let Some(timestamps) = value.get("timestamps")
        && let (Some(from), Some(to)) = (
            timestamps.get("from").and_then(parse_time),
            timestamps.get("to").and_then(parse_time),
        )
    {
        return Some((from, to));
    }
    // 4. whisper's internal token times: centiseconds.
    if let (Some(t0), Some(t1)) = (
        value.get("t0").and_then(Value::as_f64),
        value.get("t1").and_then(Value::as_f64),
    ) {
        return Some((t0 / 100.0, t1 / 100.0));
    }
    None
}

/// A timestamp as either a number of seconds or a clock string.
///
/// `HH:MM:SS.mmm`, `HH:MM:SS,mmm` (the SRT comma), and `MM:SS.mmm` are all
/// accepted; the comma decimal separator is the one that trips a plain
/// `str::parse::<f64>()`.
///
/// A negative value is rejected rather than clamped. There is no such thing as
/// a moment before the start of a recording, so a negative number means the
/// field was not a timestamp, and reading it as `0.0` would put invented text
/// at the opening second of somebody's transcript.
pub fn parse_time(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return (number.is_finite() && number >= 0.0).then_some(number);
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }

    let normalized = text.replace(',', ".");
    let parts: Vec<&str> = normalized.split(':').collect();
    let mut seconds = 0.0f64;
    for part in &parts {
        let component: f64 = part.trim().parse().ok()?;
        if !component.is_finite() || component < 0.0 {
            return None;
        }
        seconds = seconds * 60.0 + component;
    }
    (parts.len() <= 3).then_some(seconds)
}

/// The whole-transcript text a backend returns alongside its segments.
pub fn parse_text(value: &Value) -> Option<String> {
    let text = value.get("text").and_then(Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One chunk's contribution to the final transcript.
pub struct ChunkResult {
    /// Where this chunk sat in the recording. Its `start` is the offset added
    /// to every timestamp below, and its keep-window decides which of them
    /// survive — so the plan stays the single authority on both.
    pub chunk: Chunk,
    /// Segments exactly as the backend reported them, in chunk-local time.
    pub segments: Vec<Segment>,
}

/// Stitch overlapping chunks into one timeline.
///
/// Each chunk's timestamps are shifted into absolute time, then each segment is
/// attributed to the single chunk whose keep-window contains its start. Because
/// the windows partition the recording (see [`crate::plan`]), a sentence spoken
/// across a boundary — and therefore transcribed by both neighbours — is
/// reported once, by the chunk that heard more of what surrounds it.
///
/// A final defensive pass drops a segment repeating its predecessor verbatim at
/// nearly the same moment, which happens when a backend's own timestamps
/// disagree with the cut by a fraction of a second.
pub fn stitch(chunks: &[ChunkResult], ceiling: f64) -> Vec<Segment> {
    let mut collected: Vec<Segment> = Vec::new();
    for result in chunks {
        for segment in &result.segments {
            let shifted = segment.shifted(result.chunk.start, ceiling);
            if result.chunk.keeps(shifted.start) {
                collected.push(shifted);
            }
        }
    }

    collected.sort_by(|left, right| {
        left.start
            .partial_cmp(&right.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut stitched: Vec<Segment> = Vec::with_capacity(collected.len());
    for segment in collected {
        if let Some(previous) = stitched.last()
            && is_repeat(previous, &segment)
        {
            continue;
        }
        stitched.push(segment);
    }
    for (index, segment) in stitched.iter_mut().enumerate() {
        segment.id = index as u32;
    }
    stitched
}

/// How close two identical segments must be before the second is treated as an
/// echo of the first rather than a genuine repetition.
const REPEAT_WINDOW_SECONDS: f64 = 1.5;

fn is_repeat(previous: &Segment, candidate: &Segment) -> bool {
    (candidate.start - previous.start).abs() <= REPEAT_WINDOW_SECONDS
        && normalize(&previous.text) == normalize(&candidate.text)
}

/// Compare what was said, not how it was punctuated: a backend that heard the
/// same sentence in two chunks often ends one copy with a comma and the other
/// with a full stop.
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric() || character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Join segment texts into a single readable transcript.
pub fn join_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|segment| segment.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `HH:MM:SS.mmm`, for a person reading a status line or a preview.
pub fn format_timestamp(seconds: f64) -> String {
    let clamped = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total_millis = (clamped * 1_000.0).round() as u64;
    let hours = total_millis / 3_600_000;
    let minutes = (total_millis / 60_000) % 60;
    let secs = (total_millis / 1_000) % 60;
    let millis = total_millis % 1_000;
    format!("{hours:02}:{minutes:02}:{secs:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A chunk cut at `offset` whose keep-window is stated directly, so each
    /// test can describe the boundary it is about without re-deriving a plan.
    fn chunk(offset: f64, keep_from: f64, keep_until: f64, segments: Vec<Segment>) -> ChunkResult {
        ChunkResult {
            chunk: Chunk {
                index: 0,
                start: offset,
                end: offset + 10.0,
                keep_from,
                keep_until,
            },
            segments,
        }
    }

    #[test]
    fn the_openai_verbose_json_shape_parses() {
        let reply = json!({
            "task": "transcribe",
            "language": "english",
            "duration": 8.47,
            "text": "Hello there. General Kenobi.",
            "segments": [
                {"id": 0, "seek": 0, "start": 0.0, "end": 2.5, "text": " Hello there."},
                {"id": 1, "seek": 0, "start": 2.5, "end": 4.75, "text": " General Kenobi."}
            ]
        });

        let segments = parse_segments(&reply);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], Segment::new(0, 0.0, 2.5, "Hello there."));
        assert_eq!(segments[1], Segment::new(1, 2.5, 4.75, "General Kenobi."));
        assert_eq!(
            parse_text(&reply).as_deref(),
            Some("Hello there. General Kenobi.")
        );
    }

    #[test]
    fn millisecond_offsets_parse() {
        let reply = json!({
            "segments": [{"offsets": {"from": 1_500, "to": 3_250}, "text": "in the middle"}]
        });

        assert_eq!(
            parse_segments(&reply),
            [Segment::new(0, 1.5, 3.25, "in the middle")]
        );
    }

    #[test]
    fn srt_shaped_timestamp_strings_parse_including_the_comma_decimal() {
        let reply = json!({
            "segments": [{
                "timestamps": {"from": "00:00:01,500", "to": "00:01:03.250"},
                "text": "spoken"
            }]
        });

        assert_eq!(
            parse_segments(&reply),
            [Segment::new(0, 1.5, 63.25, "spoken")]
        );
    }

    #[test]
    fn centisecond_token_times_parse() {
        let reply = json!({"segments": [{"t0": 250, "t1": 480, "text": "quick"}]});

        assert_eq!(parse_segments(&reply), [Segment::new(0, 2.5, 4.8, "quick")]);
    }

    #[test]
    fn a_reply_with_no_segments_yields_none_rather_than_a_guess() {
        assert!(parse_segments(&json!({"text": "just the words"})).is_empty());
        assert!(parse_segments(&json!({"segments": "not an array"})).is_empty());
        assert!(parse_segments(&json!({})).is_empty());
    }

    #[test]
    fn a_segment_without_a_usable_timestamp_is_skipped_not_invented() {
        let reply = json!({
            "segments": [
                {"text": "no times at all"},
                {"start": 1.0, "end": 2.0, "text": "usable"},
                {"start": "later", "end": "sooner", "text": "unparseable"},
                {"start": 3.0, "end": 4.0, "text": "   "}
            ]
        });

        let segments = parse_segments(&reply);
        assert_eq!(segments, [Segment::new(0, 1.0, 2.0, "usable")]);
    }

    #[test]
    fn timestamps_parse_from_numbers_and_from_every_clock_spelling() {
        assert_eq!(parse_time(&json!(12.5)), Some(12.5));
        assert_eq!(parse_time(&json!("12.5")), Some(12.5));
        assert_eq!(parse_time(&json!("00:00:01.250")), Some(1.25));
        assert_eq!(parse_time(&json!("00:00:01,250")), Some(1.25));
        assert_eq!(parse_time(&json!("01:30")), Some(90.0));
        assert_eq!(parse_time(&json!("1:02:03")), Some(3_723.0));

        assert_eq!(parse_time(&json!("")), None);
        assert_eq!(parse_time(&json!("soon")), None);
        assert_eq!(parse_time(&json!("1:2:3:4")), None, "more than HH:MM:SS");
        assert_eq!(parse_time(&json!(f64::NAN)), None);
    }

    #[test]
    fn a_negative_timestamp_is_refused_rather_than_read_as_the_opening_second() {
        assert_eq!(parse_time(&json!(-5.0)), None);
        assert_eq!(parse_time(&json!("-5")), None);
        assert_eq!(parse_time(&json!("00:-1:00")), None);

        // And such a segment is skipped, not moved to 0.0, where it would put
        // words at the start of the transcript that were never said there.
        let reply = json!({"segments": [{"start": -1.0, "end": 2.0, "text": "misplaced"}]});
        assert!(parse_segments(&reply).is_empty());
    }

    #[test]
    fn a_single_chunk_passes_through_with_its_own_timestamps() {
        let stitched = stitch(
            &[chunk(
                0.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                vec![
                    Segment::new(0, 0.0, 2.0, "first"),
                    Segment::new(1, 2.0, 4.0, "second"),
                ],
            )],
            60.0,
        );

        assert_eq!(stitched.len(), 2);
        assert_eq!(stitched[1], Segment::new(1, 2.0, 4.0, "second"));
    }

    #[test]
    fn a_second_chunks_timestamps_are_corrected_back_to_absolute_time() {
        // Chunk 1 was cut starting at 295s, so its local 4.0 is really 299.0.
        let stitched = stitch(
            &[
                chunk(
                    0.0,
                    f64::NEG_INFINITY,
                    297.5,
                    vec![Segment::new(0, 10.0, 12.0, "early")],
                ),
                chunk(
                    295.0,
                    297.5,
                    f64::INFINITY,
                    vec![Segment::new(0, 4.0, 7.0, "late")],
                ),
            ],
            3_600.0,
        );

        assert_eq!(
            stitched,
            [
                Segment::new(0, 10.0, 12.0, "early"),
                Segment::new(1, 299.0, 302.0, "late"),
            ]
        );
    }

    /// The whole reason the overlap exists: a sentence spanning the boundary is
    /// heard in full by both neighbours, and must survive exactly once.
    #[test]
    fn a_sentence_across_a_boundary_is_reported_once_by_the_chunk_that_heard_more() {
        // Chunks [0,10) and [6,16), cut at 8. The sentence runs 7.2 → 9.4.
        let stitched = stitch(
            &[
                chunk(
                    0.0,
                    f64::NEG_INFINITY,
                    8.0,
                    vec![
                        Segment::new(0, 5.0, 7.0, "before it"),
                        // Chunk 0 heard the sentence start but was cut off, so
                        // its copy is the truncated one.
                        Segment::new(1, 7.2, 10.0, "the sentence that spans"),
                    ],
                ),
                chunk(
                    6.0,
                    8.0,
                    f64::INFINITY,
                    vec![
                        // Local 1.2 = absolute 7.2: the same sentence, complete.
                        Segment::new(0, 1.2, 3.4, "the sentence that spans the cut"),
                        Segment::new(1, 4.0, 6.0, "after it"),
                    ],
                ),
            ],
            16.0,
        );

        let texts: Vec<&str> = stitched.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            ["before it", "the sentence that spans", "after it"],
            "each moment is reported once"
        );
        // And the ids are contiguous after the drop.
        assert_eq!(stitched.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn a_segment_starting_outside_a_chunks_window_is_dropped() {
        let stitched = stitch(
            &[chunk(
                0.0,
                5.0,
                10.0,
                vec![
                    Segment::new(0, 4.9, 6.0, "too early"),
                    Segment::new(1, 5.0, 6.0, "on the boundary, kept"),
                    Segment::new(2, 10.0, 11.0, "on the far boundary, dropped"),
                ],
            )],
            60.0,
        );

        assert_eq!(
            stitched.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["on the boundary, kept"]
        );
    }

    #[test]
    fn an_echo_differing_only_in_punctuation_or_case_is_dropped() {
        let stitched = stitch(
            &[
                chunk(
                    0.0,
                    f64::NEG_INFINITY,
                    8.0,
                    vec![Segment::new(0, 7.4, 9.0, "So, that's the plan.")],
                ),
                chunk(
                    6.0,
                    8.0,
                    f64::INFINITY,
                    vec![Segment::new(0, 2.1, 3.2, "So that's the plan")],
                ),
            ],
            20.0,
        );

        assert_eq!(stitched.len(), 1, "{stitched:?}");
        assert_eq!(stitched[0].text, "So, that's the plan.");
    }

    #[test]
    fn a_genuine_repetition_far_apart_in_time_is_kept() {
        let stitched = stitch(
            &[chunk(
                0.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                vec![
                    Segment::new(0, 1.0, 2.0, "yes"),
                    Segment::new(1, 30.0, 31.0, "yes"),
                ],
            )],
            60.0,
        );

        assert_eq!(stitched.len(), 2, "a word said twice really was said twice");
    }

    #[test]
    fn a_backend_returning_an_end_before_its_start_is_repaired_rather_than_propagated() {
        let stitched = stitch(
            &[chunk(
                10.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                vec![Segment::new(0, 5.0, 1.0, "inverted")],
            )],
            60.0,
        );

        assert_eq!(stitched[0].start, 15.0);
        assert_eq!(stitched[0].end, 15.0, "never a negative-length segment");
    }

    #[test]
    fn timestamps_are_clamped_to_the_length_of_the_recording() {
        let stitched = stitch(
            &[chunk(
                295.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                // A backend padding the last chunk reports past the real end.
                vec![Segment::new(0, 4.0, 12.0, "tail")],
            )],
            300.0,
        );

        assert_eq!(stitched[0].start, 299.0);
        assert_eq!(stitched[0].end, 300.0);
    }

    #[test]
    fn segments_come_back_sorted_even_if_a_backend_returned_them_out_of_order() {
        let stitched = stitch(
            &[chunk(
                0.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                vec![
                    Segment::new(0, 8.0, 9.0, "third"),
                    Segment::new(1, 1.0, 2.0, "first"),
                    Segment::new(2, 4.0, 5.0, "second"),
                ],
            )],
            60.0,
        );

        assert_eq!(
            stitched.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(stitched.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn joined_text_reads_as_one_transcript() {
        let segments = vec![
            Segment::new(0, 0.0, 1.0, " Hello there. "),
            Segment::new(1, 1.0, 2.0, "General Kenobi."),
            Segment::new(2, 2.0, 3.0, "   "),
        ];

        assert_eq!(join_text(&segments), "Hello there. General Kenobi.");
    }

    #[test]
    fn timestamps_render_as_a_clock_a_person_can_seek_to() {
        assert_eq!(format_timestamp(0.0), "00:00:00.000");
        assert_eq!(format_timestamp(1.25), "00:00:01.250");
        assert_eq!(format_timestamp(3_723.456), "01:02:03.456");
        assert_eq!(format_timestamp(-5.0), "00:00:00.000");
        assert_eq!(format_timestamp(f64::NAN), "00:00:00.000");
    }
}
