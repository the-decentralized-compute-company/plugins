//! Deciding where to cut a long recording.
//!
//! Naive chunking cuts a sentence in half and loses it: the last word before
//! the boundary and the first after it are both spoken into a silence the model
//! never hears the rest of. So chunks overlap. Every moment near a boundary is
//! present in full inside at least one chunk, which means it is also present
//! *twice*, and the second half of the job is deciding which copy to keep.
//!
//! Each chunk therefore carries a keep-window as well as a time range. The
//! window boundary sits at the middle of the overlap, so a segment is attributed
//! to whichever chunk heard more of its surroundings, and every moment of the
//! recording belongs to exactly one chunk. [`crate::segments::stitch`] applies
//! the windows; nothing here touches audio or text.

use std::fmt;

/// One piece of the recording, in absolute seconds from the start of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub index: usize,
    /// Where this chunk is cut from, and the offset added to every timestamp
    /// the backend returns for it.
    pub start: f64,
    pub end: f64,
    /// Segments starting before this are the previous chunk's to report.
    pub keep_from: f64,
    /// Segments starting at or after this are the next chunk's to report.
    pub keep_until: f64,
}

impl Chunk {
    /// The single chunk covering a recording that is not being split, so the
    /// stitcher has one shape to work with either way.
    pub fn whole(duration: f64) -> Self {
        Self {
            index: 0,
            start: 0.0,
            end: duration,
            keep_from: f64::NEG_INFINITY,
            keep_until: f64::INFINITY,
        }
    }

    /// Whether a segment beginning at `start` (absolute seconds) is this
    /// chunk's to report.
    pub fn keeps(&self, start: f64) -> bool {
        start >= self.keep_from && start < self.keep_until
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    /// The recording has no measurable length, so there is nothing to cut.
    EmptyRecording,
    /// The window settings cannot advance through a recording.
    DegenerateWindow { chunk: f64, overlap: f64 },
    /// Honest refusal rather than transcribing the first hour and calling it
    /// the file.
    TooManyChunks {
        needed: u64,
        max: u64,
        chunk_seconds: f64,
        duration_seconds: f64,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRecording => write!(
                formatter,
                "the recording contains no audio frames, so there is nothing to transcribe"
            ),
            Self::DegenerateWindow { chunk, overlap } => write!(
                formatter,
                "a chunk length of {chunk}s with {overlap}s of overlap cannot advance through a \
                 recording; lower `--overlap-seconds` or raise `--chunk-seconds`"
            ),
            Self::TooManyChunks {
                needed,
                max,
                chunk_seconds,
                duration_seconds,
            } => write!(
                formatter,
                "this recording is {duration_seconds:.0}s long, which needs {needed} chunks of \
                 {chunk_seconds:.0}s and the limit is {max}. Nothing was transcribed rather than \
                 returning a partial answer that looks complete. Raise `--max-chunks`, raise \
                 `--chunk-seconds`, or split the file yourself."
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Tolerance for "this chunk already reached the end". Well below one audio
/// frame at any sample rate anyone transcribes, and enough that float drift
/// across dozens of additions cannot append a zero-length chunk.
const EPSILON: f64 = 1e-6;

/// Lay out the chunks for a recording of `duration` seconds.
///
/// A recording that fits in one chunk gets exactly one chunk with no overlap
/// arithmetic at all, which is the common case and stays exact.
pub fn plan(
    duration: f64,
    chunk: f64,
    overlap: f64,
    max_chunks: u64,
) -> Result<Vec<Chunk>, PlanError> {
    if !duration.is_finite() || duration <= EPSILON {
        return Err(PlanError::EmptyRecording);
    }
    if !chunk.is_finite() || !overlap.is_finite() || chunk <= 0.0 || overlap < 0.0 {
        return Err(PlanError::DegenerateWindow { chunk, overlap });
    }
    let stride = chunk - overlap;
    // A stride at or below half the chunk means neighbouring keep-windows can
    // invert, and a stride of zero never advances at all.
    if stride <= 0.0 || overlap >= chunk / 2.0 {
        return Err(PlanError::DegenerateWindow { chunk, overlap });
    }

    // Counted before anything is built, so the refusal can name the real
    // number rather than "more than the limit". The same `EPSILON` the loop
    // below breaks on appears here too: without it, a duration that is an exact
    // multiple of the stride plus one bit of float noise would be counted one
    // chunk higher than the loop actually produces, and a file right at the
    // limit would be refused for a rounding error.
    let needed = if duration <= chunk + EPSILON {
        1
    } else {
        1 + ((duration - chunk - EPSILON) / stride).ceil().max(0.0) as u64
    };
    if needed > max_chunks {
        return Err(PlanError::TooManyChunks {
            needed,
            max: max_chunks,
            chunk_seconds: chunk,
            duration_seconds: duration,
        });
    }

    let mut ranges: Vec<(f64, f64)> = Vec::new();
    let mut start = 0.0f64;
    loop {
        let end = (start + chunk).min(duration);
        ranges.push((start, end));
        if end >= duration - EPSILON {
            break;
        }
        start += stride;
    }

    // The cut between neighbours is the middle of the overlap they share.
    // Derived from the ranges actually produced rather than from the stride, so
    // a final chunk shortened by the end of the recording still cuts fairly.
    let last = ranges.len() - 1;
    let chunks = ranges
        .iter()
        .enumerate()
        .map(|(index, &(start, end))| Chunk {
            index,
            start,
            end,
            keep_from: if index == 0 {
                f64::NEG_INFINITY
            } else {
                midpoint(ranges[index - 1].1, start)
            },
            keep_until: if index == last {
                f64::INFINITY
            } else {
                midpoint(end, ranges[index + 1].0)
            },
        })
        .collect();
    Ok(chunks)
}

fn midpoint(previous_end: f64, next_start: f64) -> f64 {
    (previous_end + next_start) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(chunks: &[Chunk]) -> Vec<(f64, f64)> {
        chunks
            .iter()
            .map(|chunk| (chunk.start, chunk.end))
            .collect()
    }

    #[test]
    fn a_recording_that_fits_in_one_chunk_is_not_chunked_at_all() {
        let chunks = plan(120.0, 300.0, 5.0, 64).expect("short recording");

        assert_eq!(ranges(&chunks), [(0.0, 120.0)]);
        assert_eq!(chunks[0], Chunk::whole(120.0));
        assert!(chunks[0].keeps(0.0) && chunks[0].keeps(119.9));
    }

    #[test]
    fn the_whole_recording_shape_keeps_every_moment() {
        let whole = Chunk::whole(42.0);
        assert!(whole.keeps(0.0));
        assert!(whole.keeps(41.9));
        // Even a timestamp a backend invented past the end stays attributed.
        assert!(whole.keeps(1_000.0));
    }

    #[test]
    fn a_recording_exactly_one_chunk_long_is_still_one_chunk() {
        let chunks = plan(300.0, 300.0, 5.0, 64).expect("exact fit");
        assert_eq!(ranges(&chunks), [(0.0, 300.0)]);
    }

    #[test]
    fn chunks_advance_by_the_stride_and_overlap_by_the_configured_amount() {
        let chunks = plan(25.0, 10.0, 2.0, 64).expect("plans");

        // stride = 8, so starts are 0, 8, 16, 24 and the last is clamped.
        assert_eq!(
            ranges(&chunks),
            [(0.0, 10.0), (8.0, 18.0), (16.0, 25.0)],
            "the third chunk reaches the end, so there is no fourth"
        );
        for pair in chunks.windows(2) {
            let shared = pair[0].end - pair[1].start;
            assert!(
                (shared - 2.0).abs() < 1e-9 || pair[1].end >= 25.0,
                "{shared}"
            );
        }
    }

    #[test]
    fn the_last_chunk_is_never_shorter_than_the_overlap() {
        // Every duration in this sweep would produce a sliver under a naive
        // "advance by stride until past the end" loop.
        for tenths in 1..600u32 {
            let duration = f64::from(tenths) / 10.0;
            let chunks = plan(duration, 10.0, 3.0, 512).expect("plans");
            let last = chunks.last().expect("at least one chunk");
            if chunks.len() > 1 {
                assert!(
                    last.end - last.start > 3.0,
                    "duration {duration}: last chunk {last:?} is inside the overlap"
                );
            }
        }
    }

    #[test]
    fn every_moment_of_the_recording_belongs_to_exactly_one_chunk() {
        let chunks = plan(100.0, 20.0, 4.0, 64).expect("plans");

        for step in 0..1_000 {
            let moment = f64::from(step) / 10.0;
            let owners = chunks.iter().filter(|chunk| chunk.keeps(moment)).count();
            assert_eq!(owners, 1, "moment {moment} has {owners} owners");
        }
    }

    #[test]
    fn a_kept_moment_is_always_inside_the_chunk_that_keeps_it() {
        let chunks = plan(100.0, 20.0, 4.0, 64).expect("plans");

        for chunk in &chunks {
            for step in 0..1_000 {
                let moment = f64::from(step) / 10.0;
                if chunk.keeps(moment) && moment < 100.0 {
                    assert!(
                        moment >= chunk.start - 1e-9 && moment <= chunk.end + 1e-9,
                        "chunk {chunk:?} claims a moment it never heard: {moment}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_cut_sits_in_the_middle_of_the_shared_overlap() {
        let chunks = plan(30.0, 10.0, 4.0, 64).expect("plans");

        // Chunk 0 is [0,10), chunk 1 is [6,16): they share [6,10], midpoint 8.
        assert!((chunks[0].keep_until - 8.0).abs() < 1e-9, "{chunks:?}");
        assert!((chunks[1].keep_from - 8.0).abs() < 1e-9, "{chunks:?}");
    }

    #[test]
    fn a_recording_needing_more_chunks_than_allowed_is_refused_with_the_real_count() {
        let error = plan(3_600.0, 300.0, 5.0, 4).expect_err("over the limit");

        let PlanError::TooManyChunks { needed, max, .. } = &error else {
            panic!("expected TooManyChunks, got {error:?}");
        };
        assert_eq!(*max, 4);
        assert_eq!(*needed, 13, "3600s at a 295s stride");
        let message = error.to_string();
        assert!(message.contains("--max-chunks"), "{message}");
        assert!(message.contains("Nothing was transcribed"), "{message}");
    }

    #[test]
    fn the_predicted_chunk_count_matches_the_plan_that_gets_built() {
        for seconds in [1u32, 9, 10, 11, 100, 999, 1_000, 3_601] {
            let duration = f64::from(seconds);
            let chunks = plan(duration, 10.0, 2.0, 10_000).expect("plans");
            // Re-running with the produced count as the ceiling must succeed,
            // and one fewer must not: that is the count being exact.
            assert!(
                plan(duration, 10.0, 2.0, chunks.len() as u64).is_ok(),
                "{seconds}"
            );
            if chunks.len() > 1 {
                assert!(
                    plan(duration, 10.0, 2.0, chunks.len() as u64 - 1).is_err(),
                    "{seconds}s: the predicted count is not tight"
                );
            }
        }
    }

    #[test]
    fn an_empty_or_unmeasurable_recording_is_refused() {
        assert_eq!(
            plan(0.0, 300.0, 5.0, 64).unwrap_err(),
            PlanError::EmptyRecording
        );
        assert_eq!(
            plan(f64::NAN, 300.0, 5.0, 64).unwrap_err(),
            PlanError::EmptyRecording
        );
    }

    #[test]
    fn window_settings_that_cannot_advance_are_refused_rather_than_looping() {
        for (chunk, overlap) in [(10.0, 10.0), (10.0, 12.0), (10.0, 5.0), (0.0, 0.0)] {
            assert!(
                matches!(
                    plan(100.0, chunk, overlap, 64),
                    Err(PlanError::DegenerateWindow { .. })
                ),
                "chunk {chunk} overlap {overlap} should not plan"
            );
        }
    }

    #[test]
    fn a_zero_overlap_still_partitions_the_recording_without_gaps() {
        let chunks = plan(25.0, 10.0, 0.0, 64).expect("plans");

        assert_eq!(ranges(&chunks), [(0.0, 10.0), (10.0, 20.0), (20.0, 25.0)]);
        for step in 0..250 {
            let moment = f64::from(step) / 10.0;
            assert_eq!(chunks.iter().filter(|chunk| chunk.keeps(moment)).count(), 1);
        }
    }
}
