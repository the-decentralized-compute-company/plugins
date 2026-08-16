//! What a file actually is, and — for WAV — how to cut a piece out of it.
//!
//! Two jobs, both pure functions over bytes:
//!
//! 1. [`sniff`] identifies a file from its own leading bytes rather than its
//!    extension, so a `.wav` that is really a PDF is refused by name instead of
//!    being uploaded to somebody's transcription endpoint.
//! 2. [`parse_wav`] and [`slice_wav`] give this plugin the one thing it needs
//!    to handle long recordings: the ability to take an exact time range out of
//!    a PCM WAV and hand it to the backend as a complete, valid WAV.
//!
//! **Only WAV can be cut here, and that is a real limit rather than an
//! oversight.** Cutting an MP3, an Ogg, or an M4A at an arbitrary second means
//! decoding it, which means a codec library or a subprocess. This plugin has
//! neither, so a compressed file is sent whole and refused with a distinct
//! message if it is over the request ceiling. The README says so plainly.

use std::fmt;

/// A container this plugin recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Wav,
    Mp3,
    Flac,
    Ogg,
    /// ISO base media: `.m4a`, `.mp4`, `.mov`.
    Mp4,
    /// Matroska and WebM share a signature; the backend sorts out which.
    Matroska,
    Aiff,
    /// A recognised file that is not audio. Named so the refusal can say what
    /// it actually saw.
    NotAudio(&'static str),
}

impl Format {
    /// The filename this plugin sends in the multipart part.
    ///
    /// Backends key off it: OpenAI's endpoint rejects an upload whose filename
    /// has no recognised extension, and whisper.cpp's server uses it to pick a
    /// decoder. It is derived from the sniffed bytes rather than the file's own
    /// name so a mislabelled file still goes up correctly described.
    pub fn upload_extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Mp4 => "m4a",
            Self::Matroska => "webm",
            Self::Aiff => "aiff",
            Self::NotAudio(_) => "bin",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::Ogg => "audio/ogg",
            Self::Mp4 => "audio/mp4",
            Self::Matroska => "audio/webm",
            Self::Aiff => "audio/aiff",
            Self::NotAudio(_) => "application/octet-stream",
        }
    }

    pub fn is_audio(self) -> bool {
        !matches!(self, Self::NotAudio(_))
    }

    /// Whether a time range can be cut out of this format in-process.
    pub fn is_sliceable(self) -> bool {
        matches!(self, Self::Wav)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV (RIFF)",
            Self::Mp3 => "MP3",
            Self::Flac => "FLAC",
            Self::Ogg => "Ogg (Vorbis or Opus)",
            Self::Mp4 => "MP4/M4A",
            Self::Matroska => "WebM or Matroska",
            Self::Aiff => "AIFF",
            Self::NotAudio(what) => what,
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Filename extensions `list_audio` will offer.
///
/// Listing is by extension because sniffing every file in a directory tree
/// means opening every file in a directory tree. The transcribe path sniffs for
/// real, so a mislabelled file is caught there rather than here.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aifc", "aiff", "flac", "m4a", "m4b", "mka", "mp3", "mp4", "mpeg", "mpga", "oga",
    "ogg", "opus", "wav", "weba", "webm", "wma",
];

pub fn has_audio_extension(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, extension)) => {
            let lowered = extension.to_ascii_lowercase();
            AUDIO_EXTENSIONS.contains(&lowered.as_str())
        }
        None => false,
    }
}

/// Identify a file from its leading bytes.
///
/// Returns `None` only when the bytes match nothing known, which is reported to
/// the caller as "not a recognised audio file" rather than uploaded on the
/// chance that the backend copes.
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    if bytes.len() < 4 {
        return None;
    }
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);

    // RIFF/WAVE needs the form type too: RIFF also carries AVI and WebP.
    if starts(b"RIFF") && bytes.len() >= 12 {
        return match &bytes[8..12] {
            b"WAVE" => Some(Format::Wav),
            b"AVI " => Some(Format::NotAudio("an AVI video container")),
            b"WEBP" => Some(Format::NotAudio("a WebP image")),
            _ => Some(Format::NotAudio("a RIFF container that is not WAVE audio")),
        };
    }
    // RF64 is the >4 GB successor to RIFF. Recognised so the refusal can say
    // what it is instead of "unknown".
    if starts(b"RF64") {
        return Some(Format::NotAudio(
            "an RF64 container (the >4 GB successor to WAV), which this plugin cannot read",
        ));
    }
    if starts(b"fLaC") {
        return Some(Format::Flac);
    }
    if starts(b"OggS") {
        return Some(Format::Ogg);
    }
    if starts(b"FORM") && bytes.len() >= 12 && matches!(&bytes[8..12], b"AIFF" | b"AIFC") {
        return Some(Format::Aiff);
    }
    if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Format::Matroska);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(Format::Mp4);
    }
    // MP3: either an ID3 tag or a raw frame sync (11 set bits).
    if starts(b"ID3") {
        return Some(Format::Mp3);
    }
    if bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0 {
        return Some(Format::Mp3);
    }

    // Recognised non-audio, so the message can name it.
    if starts(b"%PDF-") {
        return Some(Format::NotAudio("a PDF document"));
    }
    if starts(&[0x89, b'P', b'N', b'G']) {
        return Some(Format::NotAudio("a PNG image"));
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some(Format::NotAudio("a JPEG image"));
    }
    if starts(b"PK\x03\x04") {
        return Some(Format::NotAudio("a ZIP archive"));
    }
    if starts(&[0x1F, 0x8B]) {
        return Some(Format::NotAudio("a gzip archive"));
    }
    if starts(b"\x7FELF") {
        return Some(Format::NotAudio("an ELF executable"));
    }
    if starts(b"MZ") {
        return Some(Format::NotAudio("a Windows executable"));
    }
    if starts(b"{\"") || starts(b"<?xml") || starts(b"<!DOC") {
        return Some(Format::NotAudio("a text document"));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WavError {
    NotRiffWave,
    Truncated,
    MissingFmt,
    MissingData,
    /// `fmt ` was present but too short to describe the stream.
    ShortFmt,
    /// Sample rate or block alignment was zero, so no time-to-byte mapping
    /// exists.
    DegenerateFmt,
    /// A compressed payload inside a WAV wrapper — ADPCM, µ-law, MP3-in-WAV.
    /// Readable as a file, not cuttable by byte arithmetic.
    CompressedPayload {
        format_tag: u16,
    },
}

impl fmt::Display for WavError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRiffWave => {
                write!(formatter, "the file does not begin with a RIFF/WAVE header")
            }
            Self::Truncated => write!(
                formatter,
                "the RIFF chunk table runs past the end of the file, so the recording is truncated \
                 or corrupt"
            ),
            Self::MissingFmt => write!(
                formatter,
                "the WAV has no `fmt ` chunk describing its audio"
            ),
            Self::MissingData => write!(
                formatter,
                "the WAV has no `data` chunk, so it holds no audio"
            ),
            Self::ShortFmt => write!(
                formatter,
                "the WAV's `fmt ` chunk is too short to describe the stream"
            ),
            Self::DegenerateFmt => write!(
                formatter,
                "the WAV declares a zero sample rate or block alignment, so no timestamp can be \
                 computed from it"
            ),
            Self::CompressedPayload { format_tag } => write!(
                formatter,
                "the WAV wraps a compressed payload (format tag {format_tag}, not PCM or IEEE \
                 float), which cannot be cut into chunks without decoding it"
            ),
        }
    }
}

impl std::error::Error for WavError {}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Everything needed to map a time range to a byte range, plus the raw `fmt `
/// payload so a slice can be re-wrapped without reinterpreting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavShape {
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    /// Bytes per frame. The authority for every byte offset here — for
    /// extensible formats it is more reliable than channels × bits.
    pub block_align: u16,
    /// The `fmt ` chunk's payload, copied verbatim so an extensible header
    /// stays extensible when a slice is re-wrapped.
    pub fmt_payload: Vec<u8>,
    pub data_offset: usize,
    pub data_len: usize,
}

impl WavShape {
    pub fn duration_seconds(&self) -> f64 {
        let bytes_per_second = u32::from(self.block_align) as f64 * self.sample_rate as f64;
        if bytes_per_second <= 0.0 {
            return 0.0;
        }
        self.data_len as f64 / bytes_per_second
    }

    /// Whether byte arithmetic on this stream is meaningful — i.e. every frame
    /// is the same size and decodes independently.
    pub fn is_linear_pcm(&self) -> bool {
        let effective = if self.format_tag == WAVE_FORMAT_EXTENSIBLE {
            self.subformat_tag().unwrap_or(0)
        } else {
            self.format_tag
        };
        matches!(effective, WAVE_FORMAT_PCM | WAVE_FORMAT_IEEE_FLOAT)
    }

    /// The real format tag hidden in an extensible header's SubFormat GUID.
    ///
    /// The GUID's first two bytes are the format tag in little-endian; the rest
    /// is a fixed suffix that this plugin has no need to inspect.
    fn subformat_tag(&self) -> Option<u16> {
        let bytes = self.fmt_payload.get(24..26)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Byte offset within the data chunk for a point in time, snapped down to a
    /// frame boundary. Cutting mid-frame would shift every following sample and
    /// turn a stereo recording into noise.
    fn byte_offset(&self, seconds: f64) -> usize {
        if seconds <= 0.0 {
            return 0;
        }
        let frame = (seconds * self.sample_rate as f64).floor().max(0.0);
        let offset = frame * f64::from(self.block_align);
        if offset >= self.data_len as f64 {
            return self.data_len;
        }
        (offset as usize / usize::from(self.block_align)) * usize::from(self.block_align)
    }
}

/// Read a WAV's chunk table.
///
/// The RIFF size field is ignored and the table is walked to end-of-file
/// instead, because a truncated or streamed recording routinely carries a
/// declared size that does not match the bytes actually present, and refusing
/// those would refuse a lot of real files for no benefit.
pub fn parse_wav(bytes: &[u8]) -> Result<WavShape, WavError> {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotRiffWave);
    }

    let mut fmt_payload: Option<Vec<u8>> = None;
    let mut data: Option<(usize, usize)> = None;
    let mut cursor = 12usize;

    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let declared = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        // A declared size that overruns the file means the recording was cut
        // short mid-write. Clamping keeps whatever audio did land usable.
        let body_len = declared.min(bytes.len().saturating_sub(body_start));

        match id {
            b"fmt " => fmt_payload = Some(bytes[body_start..body_start + body_len].to_vec()),
            b"data" => {
                // A streamed WAV writes 0 or 0xFFFFFFFF here and lets the file
                // length speak for itself.
                let effective = if declared == 0 || declared == u32::MAX as usize {
                    bytes.len().saturating_sub(body_start)
                } else {
                    body_len
                };
                data = Some((body_start, effective));
            }
            _ => {}
        }

        // Chunks are padded to an even length; the pad byte is not counted in
        // the declared size.
        let advance = 8 + body_len + (body_len & 1);
        if advance == 0 {
            return Err(WavError::Truncated);
        }
        cursor += advance;
    }

    let fmt_payload = fmt_payload.ok_or(WavError::MissingFmt)?;
    if fmt_payload.len() < 16 {
        return Err(WavError::ShortFmt);
    }
    let (data_offset, data_len) = data.ok_or(WavError::MissingData)?;

    let read_u16 = |at: usize| u16::from_le_bytes([fmt_payload[at], fmt_payload[at + 1]]);
    let read_u32 = |at: usize| {
        u32::from_le_bytes([
            fmt_payload[at],
            fmt_payload[at + 1],
            fmt_payload[at + 2],
            fmt_payload[at + 3],
        ])
    };

    let shape = WavShape {
        format_tag: read_u16(0),
        channels: read_u16(2),
        sample_rate: read_u32(4),
        block_align: read_u16(12),
        bits_per_sample: read_u16(14),
        fmt_payload,
        data_offset,
        data_len,
    };
    if shape.sample_rate == 0 || shape.block_align == 0 {
        return Err(WavError::DegenerateFmt);
    }
    Ok(shape)
}

/// Duration of a WAV from a bounded prefix of it plus the file's real length.
///
/// `list_audio` wants a duration for every WAV in a directory tree without
/// reading every WAV in a directory tree, and the header carries everything
/// needed. Returns `None` — never a guess — when `fmt ` and `data` are not both
/// inside the prefix, which happens with an unusually large metadata block.
pub fn wav_duration_from_prefix(prefix: &[u8], file_len: u64) -> Option<f64> {
    if prefix.len() < 12 || !prefix.starts_with(b"RIFF") || &prefix[8..12] != b"WAVE" {
        return None;
    }

    let mut sample_rate = 0u32;
    let mut block_align = 0u16;
    let mut data_len: Option<u64> = None;
    let mut cursor = 12usize;

    while cursor + 8 <= prefix.len() {
        let id = &prefix[cursor..cursor + 4];
        let declared = u32::from_le_bytes([
            prefix[cursor + 4],
            prefix[cursor + 5],
            prefix[cursor + 6],
            prefix[cursor + 7],
        ]) as u64;
        let body_start = cursor as u64 + 8;

        match id {
            b"fmt " if declared >= 16 && body_start as usize + 16 <= prefix.len() => {
                let at = body_start as usize;
                sample_rate = u32::from_le_bytes([
                    prefix[at + 4],
                    prefix[at + 5],
                    prefix[at + 6],
                    prefix[at + 7],
                ]);
                block_align = u16::from_le_bytes([prefix[at + 12], prefix[at + 13]]);
            }
            b"data" => {
                let remaining = file_len.saturating_sub(body_start);
                data_len = Some(if declared == 0 || declared == u64::from(u32::MAX) {
                    remaining
                } else {
                    declared.min(remaining)
                });
                break;
            }
            _ => {}
        }

        // Jump by the declared size even when it reaches past the prefix; the
        // loop condition then ends the walk rather than reading past the end.
        cursor = cursor.saturating_add(8 + declared as usize + (declared as usize & 1));
    }

    let bytes_per_second = f64::from(block_align) * f64::from(sample_rate);
    if bytes_per_second <= 0.0 {
        return None;
    }
    Some(data_len? as f64 / bytes_per_second)
}

/// Cut `[start, end)` seconds out of a parsed WAV and return a complete WAV.
///
/// The `fmt ` chunk is copied verbatim and only the `data` chunk is replaced,
/// so channel layout, bit depth and any extensible header survive the cut. The
/// range is clamped to the recording, and a range that lands entirely past the
/// end yields a valid, empty WAV rather than an error — the chunk planner never
/// asks for one, and a panic here would be a worse answer.
pub fn slice_wav(
    bytes: &[u8],
    shape: &WavShape,
    start: f64,
    end: f64,
) -> Result<Vec<u8>, WavError> {
    if !shape.is_linear_pcm() {
        return Err(WavError::CompressedPayload {
            format_tag: shape.format_tag,
        });
    }
    let available = bytes
        .len()
        .saturating_sub(shape.data_offset)
        .min(shape.data_len);
    let from = shape.byte_offset(start).min(available);
    let to = shape.byte_offset(end).min(available).max(from);
    let payload = &bytes[shape.data_offset + from..shape.data_offset + to];
    Ok(wav_container(&shape.fmt_payload, payload))
}

/// Wrap a `fmt ` payload and sample bytes in a minimal RIFF/WAVE file.
pub fn wav_container(fmt_payload: &[u8], data: &[u8]) -> Vec<u8> {
    let fmt_len = fmt_payload.len();
    let fmt_pad = fmt_len & 1;
    let data_pad = data.len() & 1;
    let riff_len = 4 + (8 + fmt_len + fmt_pad) + (8 + data.len() + data_pad);

    let mut out = Vec::with_capacity(12 + riff_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&(fmt_len as u32).to_le_bytes());
    out.extend_from_slice(fmt_payload);
    out.extend(std::iter::repeat_n(0u8, fmt_pad));
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(0u8, data_pad));
    out
}

/// A short silent 16 kHz mono PCM WAV, for probing a backend end to end.
///
/// 16 kHz mono is what Whisper resamples everything to anyway, so this is the
/// smallest upload that exercises the real decode path rather than a special
/// case.
pub fn silence_wav(seconds: f64) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    let frames = (SAMPLE_RATE as f64 * seconds.clamp(0.05, 5.0)).round() as usize;
    let mut fmt_payload = Vec::with_capacity(16);
    fmt_payload.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    fmt_payload.extend_from_slice(&1u16.to_le_bytes()); // channels
    fmt_payload.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    fmt_payload.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    fmt_payload.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt_payload.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav_container(&fmt_payload, &vec![0u8; frames * 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{wav_fixture, wav_with_data};

    #[test]
    fn a_real_wav_is_recognised_and_a_riff_that_is_not_wave_is_not() {
        assert_eq!(sniff(&wav_fixture(16_000, 1, 0.1)), Some(Format::Wav));
        assert_eq!(
            sniff(b"RIFF\x24\x00\x00\x00AVI LIST"),
            Some(Format::NotAudio("an AVI video container"))
        );
        assert_eq!(
            sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(Format::NotAudio("a WebP image"))
        );
    }

    #[test]
    fn the_common_compressed_containers_are_recognised() {
        assert_eq!(sniff(b"fLaC\x00\x00\x00\x22"), Some(Format::Flac));
        assert_eq!(
            sniff(b"OggS\x00\x02\x00\x00\x00\x00\x00\x00"),
            Some(Format::Ogg)
        );
        assert_eq!(sniff(b"ID3\x04\x00\x00\x00\x00\x00\x00"), Some(Format::Mp3));
        // A raw MPEG frame sync, which is what a tagless MP3 starts with.
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x64]), Some(Format::Mp3));
        assert_eq!(sniff(b"\x00\x00\x00\x20ftypM4A "), Some(Format::Mp4));
        assert_eq!(
            sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00, 0x00, 0x00]),
            Some(Format::Matroska)
        );
        assert_eq!(sniff(b"FORM\x00\x00\x00\x1cAIFFCOMM"), Some(Format::Aiff));
    }

    #[test]
    fn a_file_that_is_not_audio_is_named_rather_than_uploaded() {
        for (bytes, expected) in [
            (&b"%PDF-1.7 rest"[..], "a PDF document"),
            (
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A][..],
                "a PNG image",
            ),
            (&[0xFF, 0xD8, 0xFF, 0xE0][..], "a JPEG image"),
            (&b"PK\x03\x04rest"[..], "a ZIP archive"),
            (
                &b"RF64\x00\x00\x00\x00WAVE"[..],
                "an RF64 container (the >4 GB successor to WAV), which this plugin cannot read",
            ),
        ] {
            let format = sniff(bytes).unwrap_or_else(|| panic!("recognised: {expected}"));
            assert!(!format.is_audio(), "{expected} is not audio");
            assert_eq!(format.label(), expected);
        }
    }

    #[test]
    fn unrecognisable_bytes_sniff_to_nothing() {
        assert_eq!(sniff(b"hello there, this is prose"), None);
        assert_eq!(sniff(b"ab"), None, "too short to decide");
    }

    #[test]
    fn only_wav_can_be_cut_in_process() {
        assert!(Format::Wav.is_sliceable());
        for other in [
            Format::Mp3,
            Format::Flac,
            Format::Ogg,
            Format::Mp4,
            Format::Matroska,
        ] {
            assert!(
                !other.is_sliceable(),
                "{other} must not claim to be sliceable"
            );
        }
    }

    #[test]
    fn the_upload_filename_extension_comes_from_the_sniffed_bytes() {
        assert_eq!(Format::Wav.upload_extension(), "wav");
        assert_eq!(Format::Mp4.upload_extension(), "m4a");
        assert_eq!(Format::Matroska.upload_extension(), "webm");
    }

    #[test]
    fn listing_matches_extensions_case_insensitively() {
        assert!(has_audio_extension("interview.WAV"));
        assert!(has_audio_extension("a.b.mp3"));
        assert!(!has_audio_extension("notes.txt"));
        assert!(!has_audio_extension("wav"), "no dot means no extension");
    }

    #[test]
    fn a_canonical_wav_parses_into_its_shape_and_duration() {
        let bytes = wav_fixture(16_000, 1, 2.5);
        let shape = parse_wav(&bytes).expect("canonical WAV");

        assert_eq!(shape.format_tag, WAVE_FORMAT_PCM);
        assert_eq!(shape.channels, 1);
        assert_eq!(shape.sample_rate, 16_000);
        assert_eq!(shape.bits_per_sample, 16);
        assert_eq!(shape.block_align, 2);
        assert_eq!(shape.data_offset, 44);
        assert_eq!(shape.data_len, 16_000 * 2 * 5 / 2);
        assert!((shape.duration_seconds() - 2.5).abs() < 1e-9);
        assert!(shape.is_linear_pcm());
    }

    #[test]
    fn a_stereo_wav_reports_half_the_duration_of_the_same_byte_count_in_mono() {
        let mono = parse_wav(&wav_fixture(8_000, 1, 4.0)).expect("mono");
        let stereo = parse_wav(&wav_fixture(8_000, 2, 4.0)).expect("stereo");

        assert_eq!(stereo.data_len, mono.data_len * 2);
        assert!((stereo.duration_seconds() - mono.duration_seconds()).abs() < 1e-9);
    }

    #[test]
    fn chunks_before_the_data_chunk_are_walked_past_rather_than_tripped_over() {
        // A `LIST`/`INFO` block is what every recorder and editor writes.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes()); // deliberately wrong
        bytes.extend_from_slice(b"WAVE");
        // An odd-length chunk, so the pad byte is exercised too.
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.extend_from_slice(b"INFOIART\x00");
        bytes.push(0);
        let inner = wav_with_data(16_000, 1, 16, &vec![7u8; 320]);
        bytes.extend_from_slice(&inner[12..]);

        let shape = parse_wav(&bytes).expect("the LIST chunk is skipped");
        assert_eq!(shape.sample_rate, 16_000);
        assert_eq!(shape.data_len, 320);
        assert_eq!(
            &bytes[shape.data_offset..shape.data_offset + 4],
            &[7, 7, 7, 7]
        );
    }

    #[test]
    fn a_streamed_data_size_falls_back_to_the_bytes_actually_present() {
        let mut bytes = wav_with_data(16_000, 1, 16, &[3u8; 200]);
        // A recorder writing as it goes leaves the size at 0 or 0xFFFFFFFF.
        let data_size_at = bytes.len() - 200 - 4;
        bytes[data_size_at..data_size_at + 4].copy_from_slice(&0u32.to_le_bytes());

        let shape = parse_wav(&bytes).expect("streamed WAV");
        assert_eq!(shape.data_len, 200);
    }

    #[test]
    fn a_truncated_recording_keeps_the_audio_that_did_land() {
        let mut bytes = wav_with_data(16_000, 1, 16, &vec![5u8; 1_000]);
        bytes.truncate(bytes.len() - 400);

        let shape = parse_wav(&bytes).expect("truncated but readable");
        assert_eq!(shape.data_len, 600, "clamped to what is really there");
        assert!((shape.duration_seconds() - 600.0 / 32_000.0).abs() < 1e-9);
    }

    #[test]
    fn malformed_headers_are_named_individually() {
        assert_eq!(
            parse_wav(b"not a wav at all").unwrap_err(),
            WavError::NotRiffWave
        );

        let mut no_data = Vec::new();
        no_data.extend_from_slice(b"RIFF\x00\x00\x00\x00WAVE");
        no_data.extend_from_slice(b"fmt ");
        no_data.extend_from_slice(&16u32.to_le_bytes());
        no_data.extend_from_slice(&[0u8; 16]);
        assert_eq!(parse_wav(&no_data).unwrap_err(), WavError::MissingData);

        // A `fmt ` that describes no playable stream, with a data chunk present
        // so the missing-data check does not fire first.
        let degenerate = wav_container(&[0u8; 16], &[1, 2, 3, 4]);
        assert_eq!(parse_wav(&degenerate).unwrap_err(), WavError::DegenerateFmt);

        let mut short_fmt = Vec::new();
        short_fmt.extend_from_slice(b"RIFF\x00\x00\x00\x00WAVE");
        short_fmt.extend_from_slice(b"fmt ");
        short_fmt.extend_from_slice(&8u32.to_le_bytes());
        short_fmt.extend_from_slice(&[0u8; 8]);
        assert_eq!(parse_wav(&short_fmt).unwrap_err(), WavError::ShortFmt);

        let mut fmt_only = wav_with_data(16_000, 1, 16, &[]);
        fmt_only.truncate(36);
        assert_eq!(parse_wav(&fmt_only).unwrap_err(), WavError::MissingData);
    }

    #[test]
    fn a_wav_with_no_fmt_chunk_says_so() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\x00\x00\x00\x00WAVE");
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(parse_wav(&bytes).unwrap_err(), WavError::MissingFmt);
    }

    #[test]
    fn a_compressed_payload_inside_a_wav_wrapper_is_not_cuttable() {
        let mut bytes = wav_with_data(16_000, 1, 16, &[0u8; 64]);
        // 0x0011 is IMA ADPCM: a WAV file, but not one byte arithmetic can cut.
        bytes[20..22].copy_from_slice(&0x0011u16.to_le_bytes());

        let shape = parse_wav(&bytes).expect("still a readable header");
        assert!(!shape.is_linear_pcm());
        assert_eq!(
            slice_wav(&bytes, &shape, 0.0, 1.0).unwrap_err(),
            WavError::CompressedPayload { format_tag: 0x0011 }
        );
    }

    #[test]
    fn an_extensible_header_is_read_through_to_its_subformat() {
        let mut fmt_payload = Vec::new();
        fmt_payload.extend_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
        fmt_payload.extend_from_slice(&2u16.to_le_bytes()); // channels
        fmt_payload.extend_from_slice(&48_000u32.to_le_bytes());
        fmt_payload.extend_from_slice(&(48_000u32 * 6).to_le_bytes());
        fmt_payload.extend_from_slice(&6u16.to_le_bytes()); // block align
        fmt_payload.extend_from_slice(&24u16.to_le_bytes()); // bits
        fmt_payload.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt_payload.extend_from_slice(&24u16.to_le_bytes()); // valid bits
        fmt_payload.extend_from_slice(&3u32.to_le_bytes()); // channel mask
        fmt_payload.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes()); // SubFormat GUID head
        fmt_payload.extend_from_slice(&[0u8; 14]);
        let bytes = wav_container(&fmt_payload, &vec![9u8; 6 * 48_000]);

        let shape = parse_wav(&bytes).expect("extensible WAV");
        assert_eq!(shape.format_tag, WAVE_FORMAT_EXTENSIBLE);
        assert!(shape.is_linear_pcm(), "the subformat says PCM");
        assert!((shape.duration_seconds() - 1.0).abs() < 1e-9);

        // And the extensible header survives a cut verbatim.
        let cut = slice_wav(&bytes, &shape, 0.25, 0.75).expect("sliceable");
        let recut = parse_wav(&cut).expect("the slice is a valid WAV");
        assert_eq!(recut.fmt_payload, fmt_payload);
        assert_eq!(recut.channels, 2);
        assert!((recut.duration_seconds() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_slice_carries_the_bytes_from_exactly_that_time_range() {
        // 1 kHz mono 16-bit: one frame is 2 bytes, so second n starts at
        // byte 2000n and the ramp makes each frame identifiable.
        let bytes = wav_fixture(1_000, 1, 10.0);
        let shape = parse_wav(&bytes).expect("fixture");

        let cut = slice_wav(&bytes, &shape, 3.0, 5.0).expect("PCM slices");
        let cut_shape = parse_wav(&cut).expect("the slice is a valid WAV");

        assert_eq!(cut_shape.sample_rate, 1_000);
        assert_eq!(cut_shape.data_len, 2 * 2_000);
        assert!((cut_shape.duration_seconds() - 2.0).abs() < 1e-9);
        // The first frame of the slice is frame 3000 of the original.
        let first = &cut[cut_shape.data_offset..cut_shape.data_offset + 2];
        assert_eq!(i16::from_le_bytes([first[0], first[1]]), 3_000i16);
    }

    #[test]
    fn a_slice_snaps_to_a_frame_boundary_rather_than_splitting_a_stereo_pair() {
        // 3 channels × 16-bit = 6 bytes per frame, so a naive byte offset for
        // an awkward time would land mid-frame and shift every later sample.
        let bytes = wav_fixture(1_000, 3, 2.0);
        let shape = parse_wav(&bytes).expect("fixture");

        let cut = slice_wav(&bytes, &shape, 0.5001, 1.0).expect("PCM slices");
        let cut_shape = parse_wav(&cut).expect("valid WAV");

        assert_eq!(
            cut_shape.data_len % usize::from(shape.block_align),
            0,
            "a slice must be a whole number of frames"
        );
    }

    #[test]
    fn slice_bounds_are_clamped_to_the_recording() {
        let bytes = wav_fixture(1_000, 1, 1.0);
        let shape = parse_wav(&bytes).expect("fixture");

        let past_the_end = slice_wav(&bytes, &shape, 0.5, 99.0).expect("clamped");
        assert_eq!(parse_wav(&past_the_end).unwrap().data_len, 1_000);

        let entirely_past = slice_wav(&bytes, &shape, 50.0, 99.0).expect("empty but valid");
        let empty = parse_wav(&entirely_past).expect("still a valid WAV");
        assert_eq!(empty.data_len, 0);

        let inverted = slice_wav(&bytes, &shape, 0.8, 0.2).expect("never negative");
        assert_eq!(parse_wav(&inverted).unwrap().data_len, 0);
    }

    #[test]
    fn a_probe_tone_is_a_valid_short_mono_wav() {
        let bytes = silence_wav(0.2);
        let shape = parse_wav(&bytes).expect("valid WAV");

        assert_eq!(shape.sample_rate, 16_000);
        assert_eq!(shape.channels, 1);
        assert_eq!(shape.bits_per_sample, 16);
        assert!((shape.duration_seconds() - 0.2).abs() < 1e-6);
        assert_eq!(sniff(&bytes), Some(Format::Wav));
    }

    #[test]
    fn a_duration_can_be_read_from_a_header_without_reading_the_whole_recording() {
        let bytes = wav_fixture(44_100, 2, 30.0);
        // A listing reads a bounded prefix; the file length comes from the
        // directory entry, which is where it is free.
        let prefix = &bytes[..4_096];

        let duration = wav_duration_from_prefix(prefix, bytes.len() as u64).expect("header only");
        assert!((duration - 30.0).abs() < 1e-6, "{duration}");
        // And it agrees with the answer from the whole file.
        assert!((duration - parse_wav(&bytes).unwrap().duration_seconds()).abs() < 1e-9);
    }

    #[test]
    fn a_header_that_does_not_fit_the_prefix_returns_no_duration_rather_than_a_guess() {
        let bytes = wav_fixture(16_000, 1, 5.0);
        assert_eq!(
            wav_duration_from_prefix(&bytes[..20], bytes.len() as u64),
            None
        );
        assert_eq!(wav_duration_from_prefix(b"not a wav", 9), None);
        assert_eq!(wav_duration_from_prefix(&[], 0), None);
    }

    #[test]
    fn a_prefix_duration_uses_the_real_file_length_for_a_streamed_size() {
        let mut bytes = wav_with_data(8_000, 1, 16, &vec![0u8; 16_000]);
        let data_size_at = bytes.len() - 16_000 - 4;
        bytes[data_size_at..data_size_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        let duration =
            wav_duration_from_prefix(&bytes[..64], bytes.len() as u64).expect("streamed header");
        assert!((duration - 1.0).abs() < 1e-9, "{duration}");
    }

    #[test]
    fn an_odd_length_payload_is_padded_so_the_container_stays_well_formed() {
        let container = wav_container(&wav_fixture(8_000, 1, 0.0)[20..36], &[1, 2, 3]);
        assert_eq!(container.len() % 2, 0, "RIFF chunks are even-aligned");
        let shape = parse_wav(&container).expect("valid WAV");
        assert_eq!(shape.data_len, 3, "the pad byte is not counted as audio");
    }
}
