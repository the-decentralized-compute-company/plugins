//! Splitting a document into retrievable pieces.
//!
//! This is the part of a retrieval system that decides whether anything else
//! matters. A brilliant index over chunks that cut a sentence in half, or that
//! separated a heading from the paragraph it introduces, returns confident
//! nonsense; a plain brute-force scan over well-formed chunks works. So the
//! splitting here is **structural first and length-bounded second**:
//!
//! 1. The text is broken into *blocks* — fenced code blocks (kept whole,
//!    blank lines and all), Markdown headings, and blank-line-separated
//!    paragraphs. A block boundary is a place the author chose.
//! 2. A block longer than the hard ceiling is broken at sentence boundaries,
//!    and a "sentence" longer than the ceiling is cut at a word boundary. Both
//!    are last resorts and both are reported, so an operator can see when the
//!    structural path gave up.
//! 3. Blocks are packed greedily up to the target size. A heading never ends a
//!    chunk — a trailing heading is pushed into the next chunk, because a
//!    heading's whole job is to introduce what follows it.
//! 4. Consecutive chunks **overlap by whole blocks**, so a fact that straddles
//!    a boundary appears intact in one of them.
//!
//! Every unit carries the 1-based, inclusive line span it came from and the
//! heading breadcrumb it sits under, and a chunk inherits both. That is what
//! makes a citation possible: a retrieved chunk can say
//! `docs/install.md:120-141` and name the section it belongs to.
//!
//! # Sizes are in characters, not tokens
//!
//! There is no tokenizer here, and no dependency that would supply one. Every
//! bound in this module counts Unicode scalar values. For English prose a
//! token is roughly four characters, so the 1200-character default is very
//! roughly 300 tokens — treat that as an order of magnitude, not a
//! measurement, and set the bounds against your embedding model's real input
//! limit with headroom.

use std::fmt;

/// Sizes, in characters, that shape the split.
#[derive(Clone, Copy, Debug)]
pub struct ChunkOptions {
    /// What a chunk should be, roughly. Packing stops adding blocks once the
    /// next one would cross this.
    pub target_chars: usize,
    /// How much of the previous chunk to repeat at the start of the next,
    /// rounded to whole blocks. `0` disables overlap.
    pub overlap_chars: usize,
    /// The hard ceiling. A single block longer than this is split; a chunk is
    /// never emitted longer than this.
    pub max_chars: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChunkOptionsError {
    TargetTooSmall,
    MaxBelowTarget,
    OverlapNotBelowTarget,
}

impl fmt::Display for ChunkOptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetTooSmall => write!(
                formatter,
                "chunk size must be at least {MIN_TARGET_CHARS} characters"
            ),
            Self::MaxBelowTarget => write!(
                formatter,
                "the maximum chunk size must be at least the target chunk size"
            ),
            Self::OverlapNotBelowTarget => write!(
                formatter,
                "chunk overlap must be smaller than the target chunk size, otherwise a \
                 chunk would repeat its predecessor and the split would not advance"
            ),
        }
    }
}

impl std::error::Error for ChunkOptionsError {}

/// Below this the splitter stops producing anything a retriever can use — a
/// 20-character chunk is a fragment, not a passage.
pub const MIN_TARGET_CHARS: usize = 64;

impl ChunkOptions {
    /// Reject a combination that cannot terminate or cannot retrieve.
    ///
    /// Checked once at startup rather than per document, so a bad combination
    /// is a startup error and never a surprise mid-ingest.
    pub fn validate(&self) -> Result<(), ChunkOptionsError> {
        if self.target_chars < MIN_TARGET_CHARS {
            return Err(ChunkOptionsError::TargetTooSmall);
        }
        if self.max_chars < self.target_chars {
            return Err(ChunkOptionsError::MaxBelowTarget);
        }
        if self.overlap_chars >= self.target_chars {
            return Err(ChunkOptionsError::OverlapNotBelowTarget);
        }
        Ok(())
    }
}

/// Why a block had to be cut somewhere its author did not choose.
///
/// Surfaced on the chunk and counted in the `upsert` response, because "your
/// document had a 40 000-character paragraph and we cut it mid-thought" is
/// something the person ingesting it should be told rather than left to infer
/// from bad retrieval later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitReason {
    /// Cut at a sentence boundary inside an over-long block.
    Sentence,
    /// Cut at a word boundary because a single sentence exceeded the ceiling.
    Word,
    /// Cut mid-word because a single run of non-whitespace exceeded the
    /// ceiling — a minified file, a base64 blob, a long URL.
    Hard,
}

/// What kind of source construct a unit came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitKind {
    /// A Markdown heading line. Never allowed to end a chunk.
    Heading,
    /// Anything else.
    Body,
}

/// The smallest thing the packer moves around: one block, or one slice of an
/// over-long block.
#[derive(Clone, Debug)]
struct Unit {
    text: String,
    line_start: u32,
    line_end: u32,
    heading_path: Vec<String>,
    kind: UnitKind,
    /// Index of the source block this came from. Two units sharing a group are
    /// slices of one paragraph and are rejoined with a space; units from
    /// different groups are rejoined with a blank line.
    group: usize,
    split_reason: Option<SplitReason>,
    /// Cached `text.chars().count()` — the packer asks for it repeatedly.
    chars: usize,
}

/// One retrievable passage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The passage itself, ready to embed and to show in a citation.
    pub text: String,
    /// First source line, 1-based and inclusive.
    pub line_start: u32,
    /// Last source line, 1-based and inclusive.
    pub line_end: u32,
    /// Heading breadcrumb the passage sits under, outermost first. Empty for a
    /// document with no headings.
    pub heading_path: Vec<String>,
    /// Set when this chunk contains text that had to be cut somewhere the
    /// author did not choose.
    pub split_reason: Option<SplitReason>,
}

impl Chunk {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// Split a document into overlapping, structure-aligned chunks.
///
/// An empty or whitespace-only document yields no chunks — that is a fact
/// about the input, and the caller reports it rather than storing a row that
/// can never be retrieved usefully.
pub fn chunk_document(text: &str, options: &ChunkOptions) -> Vec<Chunk> {
    let units = build_units(text, options.max_chars);
    pack(units, options)
}

// ---------------------------------------------------------------------------
// Stage 1 — blocks
// ---------------------------------------------------------------------------

/// A fence is three or more backticks or tildes, optionally indented.
///
/// Returns the fence character and its run length, which is what a closing
/// fence has to match or exceed.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    (run >= 3).then_some((marker, run))
}

/// An ATX heading: one to six leading `#` followed by a space.
///
/// Returns the level and the heading text with its markers removed. A `#`
/// with no space after it is not a heading — that is a Rust attribute, a CSS
/// id selector, or a shell comment, and treating it as one would produce a
/// breadcrumb full of code.
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim().to_string();
    (!title.is_empty()).then_some((level, title))
}

/// A setext heading: a line of text underlined with `===` or `---`.
///
/// `underline` is the line after `title`. Requires at least two markers so a
/// single `-` bullet or a `---` front-matter rule with no title above it does
/// not become a heading.
fn setext_level(title: &str, underline: &str) -> Option<usize> {
    if title.trim().is_empty() || atx_heading(title).is_some() {
        return None;
    }
    let trimmed = underline.trim();
    if trimmed.len() < 2 {
        return None;
    }
    if trimmed.chars().all(|c| c == '=') {
        return Some(1);
    }
    if trimmed.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

/// Maintain the heading breadcrumb.
///
/// A level-2 heading closes every level-2-or-deeper section above it, which is
/// what makes the stack a path rather than a log.
fn push_heading(stack: &mut Vec<(usize, String)>, level: usize, title: String) {
    stack.retain(|(existing, _)| *existing < level);
    stack.push((level, title));
}

fn path_of(stack: &[(usize, String)]) -> Vec<String> {
    stack.iter().map(|(_, title)| title.clone()).collect()
}

/// Break the document into units, splitting any block over `max_chars`.
fn build_units(text: &str, max_chars: usize) -> Vec<Unit> {
    let lines: Vec<&str> = text.lines().collect();
    let mut units: Vec<Unit> = Vec::new();
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut group = 0_usize;
    let mut index = 0_usize;

    while index < lines.len() {
        let line = lines[index];

        if line.trim().is_empty() {
            index += 1;
            continue;
        }

        // A fenced block is opaque: blank lines inside it do not end it, and
        // its contents are never scanned for headings. Splitting a code fence
        // on a blank line is how a retrieved snippet ends up missing its
        // closing brace.
        if let Some((marker, run)) = fence_marker(line) {
            let start = index;
            index += 1;
            while index < lines.len() {
                if let Some((close_marker, close_run)) = fence_marker(lines[index])
                    && close_marker == marker
                    && close_run >= run
                {
                    index += 1;
                    break;
                }
                index += 1;
            }
            let block = lines[start..index].join("\n");
            emit_block(
                &mut units,
                &block,
                start as u32 + 1,
                &headings,
                UnitKind::Body,
                group,
                max_chars,
            );
            group += 1;
            continue;
        }

        if let Some((level, title)) = atx_heading(line) {
            push_heading(&mut headings, level, title);
            units.push(Unit {
                chars: line.trim().chars().count(),
                text: line.trim().to_string(),
                line_start: index as u32 + 1,
                line_end: index as u32 + 1,
                heading_path: path_of(&headings),
                kind: UnitKind::Heading,
                group,
                split_reason: None,
            });
            group += 1;
            index += 1;
            continue;
        }

        if index + 1 < lines.len()
            && let Some(level) = setext_level(line, lines[index + 1])
        {
            push_heading(&mut headings, level, line.trim().to_string());
            let block = lines[index..index + 2].join("\n");
            units.push(Unit {
                chars: block.chars().count(),
                text: block,
                line_start: index as u32 + 1,
                line_end: index as u32 + 2,
                heading_path: path_of(&headings),
                kind: UnitKind::Heading,
                group,
                split_reason: None,
            });
            group += 1;
            index += 2;
            continue;
        }

        // An ordinary paragraph: everything up to the next blank line, the
        // next heading, or the next fence.
        let start = index;
        while index < lines.len() {
            let candidate = lines[index];
            if candidate.trim().is_empty() || fence_marker(candidate).is_some() {
                break;
            }
            if index > start && atx_heading(candidate).is_some() {
                break;
            }
            if index + 1 < lines.len()
                && index > start
                && setext_level(candidate, lines[index + 1]).is_some()
            {
                break;
            }
            index += 1;
        }
        let block = lines[start..index].join("\n");
        emit_block(
            &mut units,
            &block,
            start as u32 + 1,
            &headings,
            UnitKind::Body,
            group,
            max_chars,
        );
        group += 1;
    }

    units
}

/// Push one block, splitting it if it exceeds the ceiling.
#[allow(clippy::too_many_arguments)]
fn emit_block(
    units: &mut Vec<Unit>,
    block: &str,
    line_start: u32,
    headings: &[(usize, String)],
    kind: UnitKind,
    group: usize,
    max_chars: usize,
) {
    if block.trim().is_empty() {
        return;
    }
    for piece in split_oversized(block, line_start, max_chars) {
        units.push(Unit {
            chars: piece.text.chars().count(),
            text: piece.text,
            line_start: piece.line_start,
            line_end: piece.line_end,
            heading_path: path_of(headings),
            kind,
            group,
            split_reason: piece.split_reason,
        });
    }
}

/// A slice of an over-long block, with the lines it covers.
struct Piece {
    text: String,
    line_start: u32,
    line_end: u32,
    split_reason: Option<SplitReason>,
}

/// Break a block that exceeds `max_chars` at the least damaging boundary
/// available.
///
/// Sentences first, then words, then — for a run of non-whitespace longer than
/// the ceiling, such as a base64 blob — mid-string. A block that already fits
/// comes back untouched with no split reason, which is the common case.
fn split_oversized(block: &str, line_start: u32, max_chars: usize) -> Vec<Piece> {
    if block.chars().count() <= max_chars {
        return vec![Piece {
            line_end: line_start + newlines(block),
            text: block.to_string(),
            line_start,
            split_reason: None,
        }];
    }

    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_line = line_start;
    let mut reason = SplitReason::Sentence;

    for sentence in sentences(block) {
        let sentence_chars = sentence.chars().count();

        if sentence_chars > max_chars {
            if !current.is_empty() {
                pieces.push(finish(&mut current, &mut current_line, reason));
                reason = SplitReason::Sentence;
            }
            for (fragment, fragment_reason) in break_words(&sentence, max_chars) {
                let mut owned = fragment;
                pieces.push(finish(&mut owned, &mut current_line, fragment_reason));
            }
            continue;
        }

        if !current.is_empty() && current.chars().count() + sentence_chars > max_chars {
            pieces.push(finish(&mut current, &mut current_line, reason));
            reason = SplitReason::Sentence;
        }
        current.push_str(&sentence);
    }

    if !current.trim().is_empty() {
        pieces.push(finish(&mut current, &mut current_line, reason));
    }
    pieces
}

/// Close off one piece and advance the running line cursor past it.
fn finish(buffer: &mut String, line_cursor: &mut u32, reason: SplitReason) -> Piece {
    let consumed = newlines(buffer);
    let piece = Piece {
        text: buffer.trim().to_string(),
        line_start: *line_cursor,
        line_end: *line_cursor + consumed,
        split_reason: Some(reason),
    };
    // Slices of one paragraph share the lines they were cut out of, so the
    // cursor advances only by the newlines actually consumed. Two pieces cut
    // from a single long line therefore report the same line span, which is
    // the truth.
    *line_cursor += consumed;
    buffer.clear();
    piece
}

fn newlines(text: &str) -> u32 {
    text.chars().filter(|c| *c == '\n').count() as u32
}

/// Split on sentence-ending punctuation followed by whitespace.
///
/// Trailing whitespace stays attached to the sentence it follows, so
/// concatenating the results reproduces the input exactly. Deliberately naive:
/// "e.g." and "Dr. Smith" produce a split. That costs a slightly early
/// boundary inside an already-oversized paragraph, which is a far smaller
/// problem than the abbreviation dictionary it would take to avoid.
fn sentences(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = block.chars().peekable();

    while let Some(c) = chars.next() {
        current.push(c);
        if matches!(c, '.' | '!' | '?' | '\n')
            && chars.peek().is_some_and(|next| next.is_whitespace())
        {
            while let Some(next) = chars.peek() {
                if next.is_whitespace() {
                    current.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Last resort: cut an over-long sentence at word boundaries, and a
/// word longer than the ceiling mid-string.
fn break_words(sentence: &str, max_chars: usize) -> Vec<(String, SplitReason)> {
    let mut out: Vec<(String, SplitReason)> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0_usize;

    for word in split_keeping_whitespace(sentence) {
        let word_chars = word.chars().count();

        if word_chars > max_chars {
            if current_chars > 0 {
                out.push((std::mem::take(&mut current), SplitReason::Word));
                current_chars = 0;
            }
            // A single unbroken run longer than the ceiling: a minified
            // bundle, a data URI, a base64 blob. Cut it on character
            // boundaries and label it, because there is no better boundary
            // and pretending otherwise would silently drop the tail.
            let mut buffer = String::new();
            let mut buffered = 0_usize;
            for c in word.chars() {
                buffer.push(c);
                buffered += 1;
                if buffered == max_chars {
                    out.push((std::mem::take(&mut buffer), SplitReason::Hard));
                    buffered = 0;
                }
            }
            if buffered > 0 {
                current = buffer;
                current_chars = buffered;
            }
            continue;
        }

        if current_chars + word_chars > max_chars {
            out.push((std::mem::take(&mut current), SplitReason::Word));
            current_chars = 0;
        }
        current.push_str(&word);
        current_chars += word_chars;
    }

    if !current.trim().is_empty() {
        out.push((current, SplitReason::Word));
    }
    out
}

/// Split into words with their trailing whitespace attached, so rejoining is
/// lossless.
fn split_keeping_whitespace(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_whitespace = false;

    for c in text.chars() {
        if c.is_whitespace() {
            in_whitespace = true;
            current.push(c);
        } else {
            if in_whitespace {
                out.push(std::mem::take(&mut current));
                in_whitespace = false;
            }
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// ---------------------------------------------------------------------------
// Stage 2 — packing
// ---------------------------------------------------------------------------

/// The separator that rejoins two units.
///
/// Slices of one paragraph go back together with a space; separate blocks are
/// separated by a blank line, which is what they had in the source.
fn separator(previous: &Unit, next: &Unit) -> &'static str {
    if previous.group == next.group {
        " "
    } else {
        "\n\n"
    }
}

fn render(units: &[Unit]) -> Chunk {
    let mut text = String::new();
    for (position, unit) in units.iter().enumerate() {
        if position > 0 {
            text.push_str(separator(&units[position - 1], unit));
        }
        text.push_str(&unit.text);
    }

    Chunk {
        text,
        line_start: units.iter().map(|unit| unit.line_start).min().unwrap_or(1),
        line_end: units.iter().map(|unit| unit.line_end).max().unwrap_or(1),
        // The breadcrumb of the *first* unit. A chunk that spans a section
        // boundary is filed under the section it starts in, which is the one
        // its opening sentences are about.
        heading_path: units
            .first()
            .map(|unit| unit.heading_path.clone())
            .unwrap_or_default(),
        split_reason: units.iter().find_map(|unit| unit.split_reason),
    }
}

/// How many trailing units of a finished chunk to repeat at the head of the
/// next one.
///
/// Whole units, never a partial one, and never so many that the overlap could
/// fill the next chunk on its own — that would stop the split from advancing.
/// A trailing heading is not counted as overlap because it is already being
/// carried forward for its own reason.
fn overlap_tail(units: &[Unit], options: &ChunkOptions) -> Vec<Unit> {
    if options.overlap_chars == 0 {
        return Vec::new();
    }
    let budget = options.overlap_chars.min(options.target_chars / 2);
    let mut taken: Vec<Unit> = Vec::new();
    let mut total = 0_usize;

    for unit in units.iter().rev() {
        if total + unit.chars > budget && !taken.is_empty() {
            break;
        }
        if unit.chars > budget {
            break;
        }
        total += unit.chars;
        taken.push(unit.clone());
    }
    taken.reverse();
    taken
}

fn pack(units: Vec<Unit>, options: &ChunkOptions) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current: Vec<Unit> = Vec::new();
    let mut current_chars = 0_usize;

    for unit in units {
        let joined = if current.is_empty() {
            unit.chars
        } else {
            current_chars + separator(current.last().expect("non-empty"), &unit).len() + unit.chars
        };

        if !current.is_empty() && joined > options.target_chars {
            // A heading exists to introduce what comes after it. Ending a
            // chunk on one strands the heading in the wrong passage and leaves
            // the next passage unlabelled, so trailing headings move forward.
            let mut carried: Vec<Unit> = Vec::new();
            while current
                .last()
                .is_some_and(|last| last.kind == UnitKind::Heading)
            {
                carried.insert(0, current.pop().expect("checked"));
            }

            if current.is_empty() {
                // The chunk was headings and nothing else. Keep them together
                // with the unit that follows rather than emitting a chunk of
                // pure headings.
                current = carried;
            } else {
                let tail = overlap_tail(&current, options);
                chunks.push(render(&current));
                current = tail;
                current.extend(carried);
            }

            // The ceiling is an invariant, not a preference: a caller sizes
            // `--max-chunk-chars` against the embedding model's real input
            // limit. A unit is already known to fit on its own, so when the
            // carried context would push it over, the context gives way —
            // overlap first (it is a convenience), then headings (they are
            // context), never the passage itself.
            while !current.is_empty()
                && measure(&current)
                    + separator(current.last().expect("non-empty"), &unit).len()
                    + unit.chars
                    > options.max_chars
            {
                current.remove(0);
            }

            current_chars = measure(&current);
        }

        current_chars += if current.is_empty() {
            unit.chars
        } else {
            separator(current.last().expect("non-empty"), &unit).len() + unit.chars
        };
        current.push(unit);
    }

    if !current.is_empty() && current.iter().any(|unit| !unit.text.trim().is_empty()) {
        chunks.push(render(&current));
    }
    chunks
}

fn measure(units: &[Unit]) -> usize {
    let mut total = 0_usize;
    for (position, unit) in units.iter().enumerate() {
        if position > 0 {
            total += separator(&units[position - 1], unit).len();
        }
        total += unit.chars;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(target: usize, overlap: usize, max: usize) -> ChunkOptions {
        let options = ChunkOptions {
            target_chars: target,
            overlap_chars: overlap,
            max_chars: max,
        };
        options.validate().expect("test options are valid");
        options
    }

    // -- option validation ------------------------------------------------

    #[test]
    fn an_overlap_at_or_above_the_target_is_refused() {
        let bad = ChunkOptions {
            target_chars: 200,
            overlap_chars: 200,
            max_chars: 400,
        };
        assert_eq!(
            bad.validate(),
            Err(ChunkOptionsError::OverlapNotBelowTarget),
            "an overlap that fills a chunk would stop the split advancing"
        );
    }

    #[test]
    fn a_ceiling_below_the_target_is_refused() {
        let bad = ChunkOptions {
            target_chars: 400,
            overlap_chars: 50,
            max_chars: 200,
        };
        assert_eq!(bad.validate(), Err(ChunkOptionsError::MaxBelowTarget));
    }

    #[test]
    fn a_target_too_small_to_retrieve_is_refused() {
        let bad = ChunkOptions {
            target_chars: 10,
            overlap_chars: 0,
            max_chars: 100,
        };
        assert_eq!(bad.validate(), Err(ChunkOptionsError::TargetTooSmall));
    }

    // -- headings ---------------------------------------------------------

    #[test]
    fn atx_headings_are_recognised_and_stripped() {
        assert_eq!(atx_heading("# Title"), Some((1, "Title".to_string())));
        assert_eq!(
            atx_heading("### Deep  section  "),
            Some((3, "Deep  section".to_string()))
        );
        assert_eq!(
            atx_heading("## Closed ##"),
            Some((2, "Closed".to_string())),
            "the closing run of hashes is decoration, not text"
        );
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        // These are a Rust attribute, a CSS selector, and a shell comment. A
        // breadcrumb built from them would be full of code.
        for line in ["#[derive(Debug)]", "#main { color: red }", "#!/bin/sh"] {
            assert_eq!(atx_heading(line), None, "{line}");
        }
        assert_eq!(atx_heading("####### seven"), None, "no level 7 in Markdown");
    }

    #[test]
    fn setext_headings_are_recognised() {
        assert_eq!(setext_level("Title", "====="), Some(1));
        assert_eq!(setext_level("Section", "---"), Some(2));
        assert_eq!(setext_level("Section", "--="), None);
        assert_eq!(setext_level("", "---"), None, "a rule with no title above");
        assert_eq!(setext_level("Section", "-"), None, "one dash is a bullet");
    }

    #[test]
    fn a_sibling_heading_closes_its_predecessors_subtree() {
        // The stack rule on its own: `## Configure` must close `### Linux`
        // *and* `## Install` rather than stacking on top of them.
        let mut stack = Vec::new();
        push_heading(&mut stack, 1, "Guide".to_string());
        push_heading(&mut stack, 2, "Install".to_string());
        push_heading(&mut stack, 3, "Linux".to_string());
        assert_eq!(path_of(&stack), vec!["Guide", "Install", "Linux"]);

        push_heading(&mut stack, 2, "Configure".to_string());
        assert_eq!(
            path_of(&stack),
            vec!["Guide", "Configure"],
            "a sibling must not inherit its predecessor's children"
        );

        push_heading(&mut stack, 1, "Appendix".to_string());
        assert_eq!(
            path_of(&stack),
            vec!["Appendix"],
            "a new root closes everything"
        );
    }

    #[test]
    fn each_section_of_a_document_is_filed_under_its_own_breadcrumb() {
        // Sections long enough that greedy packing keeps them apart, which is
        // what makes the per-chunk breadcrumb observable.
        let body = |topic: &str| format!("{} ", topic).repeat(40);
        let document = format!(
            "# Guide\n\n## Install\n\n{}\n\n### Linux\n\n{}\n\n## Configure\n\n{}\n",
            body("installing"),
            body("linux"),
            body("configuring")
        );
        let chunks = chunk_document(&document, &options(300, 0, 600));
        let paths: Vec<Vec<String>> = chunks
            .iter()
            .map(|chunk| chunk.heading_path.clone())
            .collect();

        assert!(
            paths.contains(&vec![
                "Guide".to_string(),
                "Install".to_string(),
                "Linux".to_string()
            ]),
            "{paths:?}"
        );
        assert!(
            paths.contains(&vec!["Guide".to_string(), "Configure".to_string()]),
            "a sibling heading must close its predecessor's subtree: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.contains(&"Linux".to_string())
                && path.contains(&"Configure".to_string())),
            "Linux and Configure are in sibling subtrees and must never share a path: {paths:?}"
        );
    }

    #[test]
    fn a_document_without_headings_has_an_empty_breadcrumb() {
        let chunks = chunk_document("Just a paragraph of prose.", &options(64, 0, 200));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].heading_path.is_empty());
    }

    // -- line spans -------------------------------------------------------

    #[test]
    fn line_spans_point_at_the_real_source_lines() {
        // Two paragraphs, each on two lines, separated by a blank line, and
        // each long enough that the packer keeps them apart.
        let filler = "word ".repeat(12);
        let document = format!("{filler}\n{filler}\n\n{filler}\n{filler}\n");

        let chunks = chunk_document(&document, &options(80, 0, 200));
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 2));
        assert_eq!(
            (chunks[1].line_start, chunks[1].line_end),
            (4, 5),
            "line 3 is blank; the second paragraph starts on line 4"
        );
    }

    #[test]
    fn one_paragraph_that_fits_stays_one_chunk_with_one_span() {
        let document = "line one\nline two\n\nline four\nline five\n";
        let chunks = chunk_document(document, &options(200, 0, 400));
        assert_eq!(chunks.len(), 1, "{chunks:#?}");
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 5));
    }

    #[test]
    fn a_citation_can_be_reconstructed_from_the_span() {
        let document = "\
# Install

Run the installer.
It asks for a directory.

# Configure

Edit the file.
";
        let lines: Vec<&str> = document.lines().collect();
        for chunk in chunk_document(document, &options(64, 0, 200)) {
            let span = &lines[(chunk.line_start - 1) as usize..=(chunk.line_end - 1) as usize];
            let quoted = span.join("\n");
            // Every non-overlap word of the chunk really is on those lines.
            for word in chunk.text.split_whitespace() {
                assert!(
                    quoted.contains(word.trim_matches('#').trim()),
                    "chunk {chunk:?} claims lines {}-{} but {word:?} is not there:\n{quoted}",
                    chunk.line_start,
                    chunk.line_end
                );
            }
        }
    }

    #[test]
    fn spans_are_one_based_and_inclusive() {
        let chunks = chunk_document("only line", &options(64, 0, 200));
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 1));
    }

    // -- structural splitting ---------------------------------------------

    #[test]
    fn a_fenced_code_block_survives_the_blank_lines_inside_it() {
        let document = "\
Intro paragraph.

```rust
fn main() {

    println!(\"hi\");

}
```

Outro paragraph.
";
        let chunks = chunk_document(document, &options(64, 0, 400));
        let code = chunks
            .iter()
            .find(|chunk| chunk.text.contains("fn main()"))
            .expect("the code block is somewhere");
        assert!(
            code.text.contains("println!") && code.text.contains('}'),
            "a fence must not be cut at a blank line, or a snippet loses its \
             closing brace: {code:#?}"
        );
    }

    #[test]
    fn a_heading_inside_a_fence_is_not_a_heading() {
        let document = "\
# Real Heading

```sh
# this is a shell comment, not a section
echo hi
```

Body text.
";
        let chunks = chunk_document(document, &options(64, 0, 400));
        for chunk in &chunks {
            assert!(
                !chunk
                    .heading_path
                    .iter()
                    .any(|title| title.contains("shell comment")),
                "a comment inside a fence became a breadcrumb: {chunk:#?}"
            );
        }
    }

    #[test]
    fn tilde_fences_work_and_a_shorter_run_does_not_close_a_longer_one() {
        let document = "~~~~\ncontent ``` here\n~~~\nstill inside\n~~~~\n\nAfter.\n";
        let chunks = chunk_document(document, &options(64, 0, 400));
        let fenced = chunks
            .iter()
            .find(|chunk| chunk.text.contains("content"))
            .expect("fence present");
        assert!(
            fenced.text.contains("still inside"),
            "a 3-tilde line must not close a 4-tilde fence: {fenced:#?}"
        );
    }

    #[test]
    fn a_heading_never_ends_a_chunk() {
        // Sized so that greedy packing would naturally put the heading last.
        let document = "\
Paragraph one is reasonably long so that it nearly fills a chunk on its own.

## A Heading

Paragraph two follows the heading and belongs with it.
";
        let chunks = chunk_document(document, &options(80, 0, 300));
        for chunk in &chunks {
            let last = chunk.text.lines().last().unwrap_or_default().trim();
            assert!(
                !last.starts_with("## "),
                "a chunk ended on a heading, stranding it from what it introduces: {chunk:#?}"
            );
        }
        let with_heading = chunks
            .iter()
            .find(|chunk| chunk.text.contains("## A Heading"))
            .expect("the heading is somewhere");
        assert!(
            with_heading.text.contains("Paragraph two"),
            "a heading must travel with the text it introduces: {with_heading:#?}"
        );
    }

    // -- size bounds ------------------------------------------------------

    #[test]
    fn no_chunk_exceeds_the_hard_ceiling() {
        let document = "word ".repeat(4_000);
        let options = options(200, 40, 400);
        for chunk in chunk_document(&document, &options) {
            assert!(
                chunk.chars() <= options.max_chars,
                "chunk of {} characters exceeds the {} ceiling",
                chunk.chars(),
                options.max_chars
            );
        }
    }

    /// Regression: a short paragraph is small enough to be carried forward as
    /// overlap, and the block after it is at the ceiling. Adding the two
    /// together used to produce a chunk past `max_chars` — which matters,
    /// because that number is what a caller sets against their embedding
    /// model's real input limit.
    #[test]
    fn an_overlap_tail_never_pushes_a_chunk_past_the_ceiling() {
        let options = options(1_200, 200, 2_400);

        // A small paragraph (eligible as overlap), then a paragraph that is a
        // single unbreakable run right at the ceiling.
        let small = "A short paragraph that is well under the overlap budget.";
        let at_ceiling = "x".repeat(options.max_chars);
        let document = format!("{small}\n\n{at_ceiling}\n\n{small}\n\n{at_ceiling}\n");

        let chunks = chunk_document(&document, &options);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(
                chunk.chars() <= options.max_chars,
                "chunk of {} characters exceeds the {} ceiling: {:?}…",
                chunk.chars(),
                options.max_chars,
                chunk.text.chars().take(60).collect::<String>()
            );
        }
        // The passage itself is what survives: the ceiling is not enforced by
        // truncating content.
        assert!(
            chunks.iter().any(|chunk| chunk.text.contains(&at_ceiling)),
            "the at-ceiling block must still appear whole somewhere"
        );
    }

    #[test]
    fn the_ceiling_holds_across_a_wide_range_of_shapes() {
        // A mix of headings, short and long paragraphs, and a code fence, at
        // several size settings — the combination that produced the overlap
        // regression above.
        let document = format!(
            "# Title\n\n{}\n\n## Section\n\n{}\n\n```\n{}\n```\n\n{}\n\n### Sub\n\n{}\n",
            "short. ".repeat(3),
            "A sentence of moderate length goes here. ".repeat(40),
            "y".repeat(900),
            "tiny.",
            "Another moderate sentence for good measure. ".repeat(30),
        );

        for (target, overlap, max) in [
            (64_usize, 0_usize, 64_usize),
            (100, 90, 120),
            (200, 60, 400),
            (1_200, 200, 2_400),
            (500, 499, 500),
        ] {
            let options = ChunkOptions {
                target_chars: target,
                overlap_chars: overlap,
                max_chars: max,
            };
            options.validate().expect("test options are valid");

            for chunk in chunk_document(&document, &options) {
                assert!(
                    chunk.chars() <= max,
                    "at target={target} overlap={overlap} max={max}: chunk of {} characters",
                    chunk.chars()
                );
            }
        }
    }

    #[test]
    fn an_oversized_paragraph_is_cut_at_sentence_boundaries_and_says_so() {
        let sentence = "This is a complete sentence of a reasonable length. ";
        let document = sentence.repeat(60);
        let chunks = chunk_document(&document, &options(200, 0, 300));

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.split_reason == Some(SplitReason::Sentence)),
            "the caller has to be told the author's boundaries were not enough"
        );
        for chunk in &chunks {
            let trimmed = chunk.text.trim();
            assert!(
                trimmed.ends_with('.'),
                "a sentence-boundary split should not end mid-sentence: {trimmed:?}"
            );
        }
    }

    #[test]
    fn an_unbreakable_run_is_cut_hard_and_labelled() {
        // A base64 blob: one "word", far longer than the ceiling. There is no
        // good boundary, and dropping the tail would be worse than saying so.
        let blob = "A".repeat(5_000);
        let chunks = chunk_document(&blob, &options(200, 0, 300));

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.split_reason == Some(SplitReason::Hard)),
            "a mid-word cut must be reported: {chunks:#?}"
        );
        let recovered: String = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        assert!(
            recovered.chars().filter(|c| *c == 'A').count() >= 5_000,
            "no input may be silently dropped by a hard split"
        );
    }

    #[test]
    fn a_long_sentence_is_cut_at_word_boundaries_before_mid_word() {
        let document = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(20);
        let chunks = chunk_document(&document, &options(100, 0, 150));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.split_reason != Some(SplitReason::Hard)),
            "whitespace was available; nothing should have been cut mid-word"
        );
        for chunk in &chunks {
            assert!(
                !chunk.text.contains("alph ") && !chunk.text.ends_with("alph"),
                "a word was cut in half: {chunk:#?}"
            );
        }
    }

    #[test]
    fn a_document_that_fits_stays_one_chunk() {
        let document = "# Title\n\nA short paragraph.\n";
        let chunks = chunk_document(document, &options(500, 100, 1_000));
        assert_eq!(chunks.len(), 1, "{chunks:#?}");
        assert_eq!(chunks[0].split_reason, None);
    }

    // -- overlap ----------------------------------------------------------

    #[test]
    fn consecutive_chunks_overlap_by_whole_blocks() {
        let document = (1..=12)
            .map(|n| format!("Paragraph number {n} carries a distinct fact about topic {n}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_document(&document, &options(200, 80, 400));

        assert!(chunks.len() > 2, "{chunks:#?}");
        for pair in chunks.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            assert!(
                next.line_start <= previous.line_end,
                "chunks {}-{} and {}-{} do not overlap",
                previous.line_start,
                previous.line_end,
                next.line_start,
                next.line_end
            );
        }
    }

    #[test]
    fn zero_overlap_produces_disjoint_chunks() {
        let document = (1..=12)
            .map(|n| format!("Paragraph number {n} carries a distinct fact about topic {n}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_document(&document, &options(200, 0, 400));

        for pair in chunks.windows(2) {
            assert!(
                pair[1].line_start > pair[0].line_end,
                "overlap is off, so the spans must not intersect: {pair:#?}"
            );
        }
    }

    #[test]
    fn overlap_never_stops_the_split_from_advancing() {
        // The pathological shape: one block just under the target, repeated.
        // If the overlap were allowed to fill a chunk, this would loop or emit
        // an unbounded number of identical chunks.
        let block = "x".repeat(150);
        let document = vec![block; 30].join("\n\n");
        let chunks = chunk_document(&document, &options(200, 190, 400));

        assert!(chunks.len() < 60, "{} chunks is runaway", chunks.len());
        for pair in chunks.windows(2) {
            assert!(
                pair[1].line_end > pair[0].line_end,
                "every chunk must consume new ground: {pair:#?}"
            );
        }
    }

    // -- degenerate input -------------------------------------------------

    #[test]
    fn empty_and_whitespace_documents_produce_nothing() {
        for document in ["", "   ", "\n\n\n", "\t\n \n"] {
            assert!(
                chunk_document(document, &options(200, 40, 400)).is_empty(),
                "{document:?} should produce no chunks"
            );
        }
    }

    #[test]
    fn a_document_of_only_headings_still_produces_a_chunk() {
        // The heading-carry rule must not swallow a document whole.
        let document = "# One\n\n## Two\n\n### Three\n";
        let chunks = chunk_document(document, &options(64, 0, 200));
        assert!(!chunks.is_empty(), "headings-only input vanished");
        let text: String = chunks.iter().map(|chunk| chunk.text.clone()).collect();
        assert!(text.contains("One") && text.contains("Three"), "{text}");
    }

    #[test]
    fn crlf_input_does_not_leave_stray_carriage_returns_in_the_breadcrumb() {
        let document = "# Title\r\n\r\nBody text here.\r\n";
        let chunks = chunk_document(document, &options(200, 0, 400));
        assert_eq!(chunks[0].heading_path, vec!["Title".to_string()]);
    }

    #[test]
    fn multibyte_text_is_never_cut_mid_codepoint() {
        // Byte-index slicing would panic here; character counting must not.
        let document = "日本語のテキストです。".repeat(500);
        let chunks = chunk_document(&document, &options(100, 20, 200));
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(chunk.chars() <= 200, "{}", chunk.chars());
            assert!(chunk.text.chars().all(|c| c != '\u{FFFD}'));
        }
    }

    #[test]
    fn a_lone_heading_with_no_body_is_not_lost() {
        let document = "Body before.\n\n## Trailing Heading\n";
        let chunks = chunk_document(document, &options(64, 0, 200));
        let text: String = chunks.iter().map(|chunk| chunk.text.clone()).collect();
        assert!(
            text.contains("Trailing Heading"),
            "a heading with nothing after it must still be stored: {chunks:#?}"
        );
    }
}
