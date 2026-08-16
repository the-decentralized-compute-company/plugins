//! Turning a Docker log stream into bounded, labelled lines.
//!
//! **Container logs are the most dangerous thing this plugin can return.**
//! Applications print connection strings on startup, echo bearer tokens in
//! debug builds, and dump whole request headers when something fails. Every
//! line handed back here goes into a model's context and, from there, wherever
//! that conversation goes. The caps below are the plugin's answer to that: a
//! line budget, a byte budget, and a per-line character budget, all enforced
//! here rather than trusted to the daemon.
//!
//! The wire format has two shapes and getting them confused produces garbage:
//!
//! * A container started **without** a TTY has its output multiplexed, so the
//!   daemon writes an eight-byte header before each chunk: one byte of stream
//!   id (`0` stdin, `1` stdout, `2` stderr), three zero bytes, then a
//!   big-endian `u32` length.
//! * A container started **with** a TTY has a single stream and no headers at
//!   all, because there is nothing to distinguish.
//!
//! Which one applies is read from the container's own `Config.Tty` rather than
//! guessed — but a malformed header still falls back to raw rather than
//! emitting eight bytes of binary at the start of every line.

use serde::Serialize;

/// Which of the container's streams a line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
    /// Frame type `0`. Docker uses it for stdin echo in a few configurations.
    Stdin,
    /// A TTY container, where the two streams are already merged by the pty.
    Merged,
}

impl Stream {
    fn from_frame_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Stdin),
            1 => Some(Self::Stdout),
            2 => Some(Self::Stderr),
            _ => None,
        }
    }
}

/// One log line as it is returned to a caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogLine {
    pub stream: Stream,
    /// Present only when the caller asked for timestamps; it is exactly what
    /// the daemon prefixed, which is RFC 3339 with nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub text: String,
    /// Set when this line alone was longer than the per-line budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// The caps a log read is performed under.
#[derive(Clone, Copy, Debug)]
pub struct LogOptions {
    /// Keep at most this many lines, newest first-to-last.
    pub max_lines: usize,
    /// Cut any single line longer than this many characters.
    pub max_line_chars: usize,
    /// Whether the daemon was asked to prefix each line with a timestamp.
    pub timestamps: bool,
}

/// The result of assembling a log stream, including what was dropped.
#[derive(Clone, Debug)]
pub struct LogOutput {
    pub lines: Vec<LogLine>,
    /// Older lines dropped to honour `max_lines`. A tail keeps the newest.
    pub dropped_leading_lines: usize,
    /// Lines that were individually cut to `max_line_chars`.
    pub truncated_lines: usize,
}

/// Assemble a raw log body into bounded lines.
pub fn assemble(body: &[u8], tty: bool, options: &LogOptions) -> LogOutput {
    let chunks = if tty {
        vec![(Stream::Merged, body.to_vec())]
    } else {
        demultiplex(body)
    };

    let mut lines = Vec::new();
    // One pending buffer per stream: a frame boundary is not a line boundary,
    // and a line split across two frames must not become two lines.
    let mut pending: Vec<(Stream, String)> = Vec::new();

    for (stream, payload) in chunks {
        let decoded = String::from_utf8_lossy(&payload);
        let slot = match pending.iter().position(|(kind, _)| *kind == stream) {
            Some(index) => &mut pending[index].1,
            None => {
                pending.push((stream, String::new()));
                &mut pending.last_mut().expect("just pushed").1
            }
        };
        slot.push_str(&decoded);

        while let Some(newline) = slot.find('\n') {
            let line: String = slot.drain(..=newline).collect();
            lines.push((stream, line.trim_end_matches(['\n', '\r']).to_string()));
        }
    }
    // Output that has not ended with a newline yet is still a line.
    for (stream, rest) in pending {
        if !rest.is_empty() {
            lines.push((stream, rest.trim_end_matches(['\n', '\r']).to_string()));
        }
    }

    let dropped_leading_lines = lines.len().saturating_sub(options.max_lines);
    let kept = lines.split_off(dropped_leading_lines);

    let mut truncated_lines = 0;
    let lines = kept
        .into_iter()
        .map(|(stream, text)| {
            let (timestamp, text) = if options.timestamps {
                split_timestamp(&text)
            } else {
                (None, text)
            };
            let (text, truncated) = cut(&text, options.max_line_chars);
            if truncated {
                truncated_lines += 1;
            }
            LogLine {
                stream,
                timestamp,
                text,
                truncated,
            }
        })
        .collect();

    LogOutput {
        lines,
        dropped_leading_lines,
        truncated_lines,
    }
}

/// Split a stream of length-prefixed frames.
///
/// Falls back to treating everything as one raw chunk if the first header is
/// not a plausible one, which is what a TTY container's output looks like when
/// something upstream reported `Tty` wrongly.
pub fn demultiplex(body: &[u8]) -> Vec<(Stream, Vec<u8>)> {
    let mut chunks = Vec::new();
    let mut offset = 0usize;

    while offset + 8 <= body.len() {
        let header = &body[offset..offset + 8];
        let Some(stream) = Stream::from_frame_byte(header[0]) else {
            return fallback(body, chunks, offset);
        };
        if header[1..4] != [0, 0, 0] {
            return fallback(body, chunks, offset);
        }
        let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let start = offset + 8;
        // A frame cut short by the byte cap is still worth returning; the
        // caller reports the truncation separately.
        let end = start.saturating_add(length).min(body.len());
        if end > start {
            chunks.push((stream, body[start..end].to_vec()));
        }
        offset = end;
        if end == body.len() {
            break;
        }
    }

    // Trailing bytes too short to be a header: a frame cut by the byte cap.
    if offset < body.len() && body.len() - offset < 8 && chunks.is_empty() {
        return vec![(Stream::Merged, body[offset..].to_vec())];
    }
    chunks
}

/// Give up on framing and return what is left as one merged chunk, keeping
/// whatever frames were already decoded.
fn fallback(
    body: &[u8],
    mut decoded: Vec<(Stream, Vec<u8>)>,
    offset: usize,
) -> Vec<(Stream, Vec<u8>)> {
    if decoded.is_empty() {
        return vec![(Stream::Merged, body.to_vec())];
    }
    decoded.push((Stream::Merged, body[offset..].to_vec()));
    decoded
}

/// Split the `2024-05-01T10:00:00.000000000Z ` prefix the daemon adds when
/// timestamps are requested. Anything that does not look like one is left in
/// the text rather than being cut off blindly.
fn split_timestamp(line: &str) -> (Option<String>, String) {
    match line.split_once(' ') {
        Some((candidate, rest)) if looks_like_timestamp(candidate) => {
            (Some(candidate.to_string()), rest.to_string())
        }
        _ => (None, line.to_string()),
    }
}

fn looks_like_timestamp(candidate: &str) -> bool {
    candidate.len() >= 20
        && candidate.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && candidate.as_bytes()[4] == b'-'
        && candidate.contains('T')
        && (candidate.ends_with('Z') || candidate.contains('+'))
}

/// Cut a line to a character budget, counting characters rather than bytes so a
/// cut never lands inside a multi-byte character.
fn cut(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let kept: String = text.chars().take(max_chars).collect();
    (format!("{kept}…"), true)
}

/// Build one multiplexed frame. Used by the tests here and by the transport
/// tests, which serve real log bodies from a stub server.
#[cfg(test)]
pub fn frame(stream: u8, payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let mut out = vec![stream, 0, 0, 0];
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LogOptions {
        LogOptions {
            max_lines: 100,
            max_line_chars: 2_000,
            timestamps: false,
        }
    }

    #[test]
    fn framed_output_keeps_stdout_and_stderr_apart() {
        let mut body = frame(1, "starting\n");
        body.extend(frame(2, "warning: disk almost full\n"));
        body.extend(frame(1, "ready\n"));

        let output = assemble(&body, false, &options());

        assert_eq!(
            output
                .lines
                .iter()
                .map(|line| (line.stream, line.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Stream::Stdout, "starting"),
                (Stream::Stderr, "warning: disk almost full"),
                (Stream::Stdout, "ready"),
            ]
        );
    }

    #[test]
    fn one_frame_holding_several_lines_becomes_several_lines() {
        let body = frame(1, "one\ntwo\nthree\n");

        let output = assemble(&body, false, &options());

        assert_eq!(output.lines.len(), 3);
        assert_eq!(output.lines[2].text, "three");
    }

    #[test]
    fn a_line_split_across_two_frames_is_rejoined() {
        let mut body = frame(1, "half a ");
        body.extend(frame(1, "line\n"));

        let output = assemble(&body, false, &options());

        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].text, "half a line");
    }

    #[test]
    fn output_without_a_trailing_newline_is_still_returned() {
        let body = frame(1, "no newline at the end");

        let output = assemble(&body, false, &options());

        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].text, "no newline at the end");
    }

    #[test]
    fn a_tty_container_has_no_frames_and_one_merged_stream() {
        let body = b"plain\r\noutput\r\n";

        let output = assemble(body, true, &options());

        assert_eq!(output.lines.len(), 2);
        assert_eq!(output.lines[0].stream, Stream::Merged);
        assert_eq!(output.lines[0].text, "plain");
    }

    #[test]
    fn a_body_that_is_not_framed_falls_back_to_raw_rather_than_emitting_garbage() {
        // `Tty` was reported false but the body has no frame headers.
        let body = b"2024-05-01 plain text that is not framed\n";

        let output = assemble(body, false, &options());

        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].stream, Stream::Merged);
        assert!(output.lines[0].text.starts_with("2024-05-01 plain"));
    }

    #[test]
    fn a_frame_cut_short_by_the_byte_cap_returns_what_arrived() {
        let mut body = frame(1, "complete\n");
        // A header promising 100 bytes, followed by only a few.
        body.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 100]);
        body.extend_from_slice(b"partial");

        let output = assemble(&body, false, &options());

        assert_eq!(output.lines.len(), 2);
        assert_eq!(output.lines[1].text, "partial");
    }

    #[test]
    fn the_line_cap_keeps_the_newest_lines_and_counts_what_it_dropped() {
        let body: Vec<u8> = (0..50)
            .flat_map(|index| frame(1, &format!("line {index}\n")))
            .collect();
        let options = LogOptions {
            max_lines: 10,
            ..options()
        };

        let output = assemble(&body, false, &options);

        assert_eq!(output.lines.len(), 10);
        assert_eq!(output.dropped_leading_lines, 40);
        assert_eq!(output.lines[0].text, "line 40");
        assert_eq!(output.lines[9].text, "line 49");
    }

    #[test]
    fn a_single_enormous_line_is_cut_and_flagged() {
        let body = frame(1, &format!("{}\n", "x".repeat(5_000)));
        let options = LogOptions {
            max_line_chars: 100,
            ..options()
        };

        let output = assemble(&body, false, &options);

        assert_eq!(output.truncated_lines, 1);
        assert!(output.lines[0].truncated);
        assert_eq!(
            output.lines[0].text.chars().count(),
            101,
            "100 plus the ellipsis"
        );
    }

    #[test]
    fn a_multibyte_line_is_cut_on_a_character_boundary() {
        let body = frame(1, &format!("{}\n", "é".repeat(50)));
        let options = LogOptions {
            max_line_chars: 10,
            ..options()
        };

        let output = assemble(&body, false, &options);

        assert_eq!(output.lines[0].text.chars().count(), 11);
        assert!(output.lines[0].text.starts_with("éé"));
    }

    #[test]
    fn timestamps_are_split_off_only_when_they_were_requested() {
        let body = frame(1, "2024-05-01T10:00:00.123456789Z hello\n");

        let with = assemble(
            &body,
            false,
            &LogOptions {
                timestamps: true,
                ..options()
            },
        );
        assert_eq!(
            with.lines[0].timestamp.as_deref(),
            Some("2024-05-01T10:00:00.123456789Z")
        );
        assert_eq!(with.lines[0].text, "hello");

        let without = assemble(&body, false, &options());
        assert_eq!(without.lines[0].timestamp, None);
        assert!(without.lines[0].text.starts_with("2024-05-01T10"));
    }

    #[test]
    fn a_line_that_only_looks_like_it_starts_with_a_timestamp_is_left_alone() {
        let body = frame(1, "GET /health 200\n");

        let output = assemble(
            &body,
            false,
            &LogOptions {
                timestamps: true,
                ..options()
            },
        );

        assert_eq!(output.lines[0].timestamp, None);
        assert_eq!(output.lines[0].text, "GET /health 200");
    }

    #[test]
    fn invalid_utf8_degrades_instead_of_losing_the_whole_read() {
        let mut body = vec![1, 0, 0, 0, 0, 0, 0, 4];
        body.extend_from_slice(&[0xff, 0xfe, b'o', b'k']);

        let output = assemble(&body, false, &options());

        assert_eq!(output.lines.len(), 1);
        assert!(output.lines[0].text.ends_with("ok"));
    }

    #[test]
    fn an_empty_body_produces_no_lines_rather_than_one_empty_one() {
        let output = assemble(b"", false, &options());
        assert!(output.lines.is_empty());
        assert_eq!(output.dropped_leading_lines, 0);
    }
}
