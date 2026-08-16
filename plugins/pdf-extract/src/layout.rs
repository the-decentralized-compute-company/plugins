//! Turning positioned runs into text somebody can read.
//!
//! Reading order is not in the file. It has to be recovered from geometry, and
//! the recovery has a name: a recursive XY cut. At each step the region is
//! examined for a vertical corridor of whitespace that runs its whole height —
//! a column gutter — and split there if one is found; failing that, for a
//! horizontal band of whitespace that runs its whole width — a block
//! separation — and split there. When neither exists the region is a leaf, and
//! its runs are grouped into lines and read top to bottom, left to right.
//!
//! Trying the vertical cut first is deliberate. A page with a full-width
//! heading above two columns has no clear gutter at the top level, because the
//! heading crosses it; the horizontal cut separates heading from body, and the
//! body then splits into columns. Reversing the order would find a horizontal
//! gap inside one column and cut the page across both.
//!
//! **The hard case**: two columns of prose and a two-column list of labels and
//! values look identical from above — two dense stacks of lines with a clear
//! corridor between them — and have to be read in opposite ways. Prose is read
//! one column at a time; a definition list is read one *row* at a time, because
//! cutting down the gutter would separate every term from its definition.
//!
//! Three guards decide it. Both sides must carry at least
//! [`MIN_LINES_PER_COLUMN`] lines, they must overlap vertically, and the region
//! must not read as a table — which [`crate::tables`] answers by asking whether
//! the text fills the width of its column, as prose does and a cell does not.
//! The number of columns cut is reported so a caller can see the decision, and
//! when it is wrong [`LayoutMode::Single`] turns column detection off and
//! [`LayoutMode::Preserve`] keeps the geometry instead of interpreting it.

use std::cmp::Ordering;

use crate::glyphs::Run;
use crate::tables;

/// Recursion depth for the XY cut. Enough for a heading, a footer, and three
/// columns nested inside each other; not enough for a pathological page to
/// spend the whole budget subdividing.
const MAX_CUT_DEPTH: u32 = 6;

/// A region with fewer runs than this is a leaf. Splitting three strings into
/// two columns says more about the threshold than about the page.
const MIN_RUNS_TO_SPLIT: usize = 8;

/// Lines each side of a vertical cut must have for the cut to be believed.
pub const MIN_LINES_PER_COLUMN: usize = 3;

/// A gutter must be at least this fraction of the region's width, and at least
/// as wide as one line is tall. Both matter: the ratio catches a wide page, the
/// height catches a narrow one, and either alone lets an ordinary word space
/// through on some page size.
const MIN_GUTTER_RATIO: f32 = 0.03;

/// Fraction of their heights two columns must share for them to be columns
/// rather than one block sitting above another.
const MIN_COLUMN_OVERLAP: f32 = 0.5;

/// A horizontal band of whitespace separates blocks when it exceeds this
/// multiple of the line height — comfortably more than the leading between two
/// lines of one paragraph.
const MIN_BLOCK_GAP_RATIO: f32 = 1.2;

/// Gap between two runs, as a fraction of the line height, that reads as a word
/// space rather than as kerning inside a word.
const WORD_GAP_RATIO: f32 = 0.22;

/// Widest line `LayoutMode::Preserve` will draw. A run positioned far off the
/// page cannot turn into a line of a million spaces.
const MAX_PRESERVED_COLUMNS: usize = 400;

/// Fallback character cell for `LayoutMode::Preserve` when a page offers no
/// measurable glyph.
const FALLBACK_CELL_WIDTH: f32 = 5.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// Detect columns and blocks and read them in order.
    #[default]
    Auto,
    /// One column: group every run into lines by position and read the lines
    /// top to bottom. Right when `Auto` splits something that was never a
    /// column.
    Single,
    /// Draw the page into a fixed-pitch character grid, keeping the horizontal
    /// positions as padding. Alignment survives; it is not prose.
    Preserve,
}

/// A row of runs sharing a baseline.
#[derive(Clone, Debug)]
pub struct Line {
    /// Baseline, in page points, growing upward.
    pub y: f32,
    /// Height of the tallest run on the line.
    pub height: f32,
    pub runs: Vec<Run>,
}

impl Line {
    pub fn left(&self) -> f32 {
        self.runs
            .iter()
            .map(|run| run.x)
            .fold(f32::INFINITY, f32::min)
    }

    /// The line as text, with a space inserted wherever the gap between two
    /// runs is wider than kerning would explain.
    pub fn text(&self) -> String {
        let mut text = String::new();
        let mut previous_right: Option<f32> = None;
        for run in &self.runs {
            if let Some(right) = previous_right {
                let gap = run.x - right;
                let threshold = WORD_GAP_RATIO * self.height.max(run.height).max(1.0);
                let already_spaced = text.ends_with(char::is_whitespace)
                    || run.text.starts_with(char::is_whitespace);
                if gap > threshold && !already_spaced {
                    text.push(' ');
                }
            }
            text.push_str(&run.text);
            previous_right = Some(run.right());
        }
        text.trim_end().to_string()
    }
}

/// One page, rendered.
#[derive(Clone, Debug, Default)]
pub struct RenderedPage {
    pub text: String,
    /// Column bands the cut produced. `1` means the page was read as a single
    /// column; more means a gutter was believed.
    pub columns: usize,
    pub lines: usize,
    /// A character budget stopped the rendering early.
    pub truncated: bool,
}

/// Drop anything that cannot take part in geometry, and normalize what is left.
///
/// A malformed PDF can produce a non-finite coordinate, and one `NaN` in a
/// comparison sort is enough to make every later decision meaningless.
fn usable(runs: &[Run]) -> Vec<Run> {
    runs.iter()
        .filter(|run| {
            run.x.is_finite()
                && run.y.is_finite()
                && run.width.is_finite()
                && run.height.is_finite()
                && run.text.chars().any(|character| !character.is_whitespace())
        })
        .map(|run| Run {
            width: run.width.max(0.0),
            height: if run.height > 0.0 { run.height } else { 1.0 },
            ..run.clone()
        })
        .collect()
}

fn compare(left: f32, right: f32) -> Ordering {
    left.partial_cmp(&right).unwrap_or(Ordering::Equal)
}

fn median(mut values: Vec<f32>) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|left, right| compare(*left, *right));
    values[values.len() / 2]
}

/// Group runs into lines by baseline, and sort the lines top to bottom and
/// their runs left to right.
///
/// Two runs share a line when their baselines are within half the taller one's
/// height. That tolerance is what keeps a superscript, a footnote marker, or a
/// slightly mispositioned glyph on the line it belongs to.
pub fn group_lines(runs: &[Run]) -> Vec<Line> {
    let mut runs = usable(runs);
    runs.sort_by(|left, right| compare(right.y, left.y).then_with(|| compare(left.x, right.x)));

    let mut lines: Vec<Line> = Vec::new();
    for run in runs {
        let joined = lines.last_mut().is_some_and(|line| {
            let tolerance = 0.5 * line.height.max(run.height);
            (line.y - run.y).abs() <= tolerance
        });
        if joined {
            let line = lines.last_mut().expect("checked above");
            line.height = line.height.max(run.height);
            line.runs.push(run);
        } else {
            lines.push(Line {
                y: run.y,
                height: run.height,
                runs: vec![run],
            });
        }
    }

    for line in &mut lines {
        line.runs.sort_by(|left, right| compare(left.x, right.x));
    }
    lines
}

/// Merged occupied intervals along one axis, sorted.
fn merge_intervals(mut intervals: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    intervals.sort_by(|left, right| compare(left.0, right.0));
    let mut merged: Vec<(f32, f32)> = Vec::new();
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The widest gap between merged intervals, if any is at least `minimum`.
fn widest_gap(intervals: &[(f32, f32)], minimum: f32) -> Option<(f32, f32)> {
    intervals
        .windows(2)
        .map(|pair| (pair[0].1, pair[1].0))
        .filter(|(start, end)| end - start >= minimum)
        .max_by(|left, right| compare(left.1 - left.0, right.1 - right.0))
}

/// Fraction of the shorter side's height that the two sides share.
fn vertical_overlap(left: &[Run], right: &[Run]) -> f32 {
    let extent = |runs: &[Run]| {
        let low = runs.iter().map(|run| run.y).fold(f32::INFINITY, f32::min);
        let high = runs
            .iter()
            .map(|run| run.y + run.height)
            .fold(f32::NEG_INFINITY, f32::max);
        (low, high)
    };
    let (left_low, left_high) = extent(left);
    let (right_low, right_high) = extent(right);
    let overlap = left_high.min(right_high) - left_low.max(right_low);
    let shorter = (left_high - left_low).min(right_high - right_low);
    if shorter <= 0.0 {
        return 0.0;
    }
    (overlap / shorter).clamp(0.0, 1.0)
}

fn line_count(runs: &[Run]) -> usize {
    group_lines(runs).len()
}

/// Split a region into reading-ordered leaves, counting the vertical cuts.
fn cut(runs: Vec<Run>, depth: u32, leaves: &mut Vec<Vec<Run>>, vertical_cuts: &mut usize) {
    if runs.len() < MIN_RUNS_TO_SPLIT || depth >= MAX_CUT_DEPTH {
        leaves.push(runs);
        return;
    }

    let heights = median(runs.iter().map(|run| run.height).collect());
    let left_edge = runs.iter().map(|run| run.x).fold(f32::INFINITY, f32::min);
    let right_edge = runs
        .iter()
        .map(Run::right)
        .fold(f32::NEG_INFINITY, f32::max);
    let region_width = (right_edge - left_edge).max(1.0);

    // A column gutter: clear from the top of the region to the bottom.
    let horizontal_spans = merge_intervals(runs.iter().map(|run| (run.x, run.right())).collect());
    let gutter_minimum = (MIN_GUTTER_RATIO * region_width).max(heights);
    if let Some((start, end)) = widest_gap(&horizontal_spans, gutter_minimum) {
        let boundary = (start + end) / 2.0;
        let (left, right): (Vec<Run>, Vec<Run>) = runs
            .iter()
            .cloned()
            .partition(|run| run.right() <= boundary);
        if line_count(&left) >= MIN_LINES_PER_COLUMN
            && line_count(&right) >= MIN_LINES_PER_COLUMN
            && vertical_overlap(&left, &right) >= MIN_COLUMN_OVERLAP
            // Asked last because it is the expensive question, and only of
            // regions that have already qualified for a column split on
            // geometry alone.
            && !tables::covers_as_table(&group_lines(&runs))
        {
            *vertical_cuts += 1;
            cut(left, depth + 1, leaves, vertical_cuts);
            cut(right, depth + 1, leaves, vertical_cuts);
            return;
        }
    }

    // A block separation: clear from the left of the region to the right.
    let vertical_spans =
        merge_intervals(runs.iter().map(|run| (run.y, run.y + run.height)).collect());
    let block_minimum = (MIN_BLOCK_GAP_RATIO * heights).max(1.0);
    if let Some((start, end)) = widest_gap(&vertical_spans, block_minimum) {
        let boundary = (start + end) / 2.0;
        let (top, bottom): (Vec<Run>, Vec<Run>) =
            runs.iter().cloned().partition(|run| run.y >= boundary);
        if !top.is_empty() && !bottom.is_empty() {
            cut(top, depth + 1, leaves, vertical_cuts);
            cut(bottom, depth + 1, leaves, vertical_cuts);
            return;
        }
    }

    leaves.push(runs);
}

/// Split a page into reading-ordered regions. Exposed for `extract_tables`,
/// which wants the same regions but not the rendered text.
pub fn reading_order(runs: &[Run]) -> (Vec<Vec<Run>>, usize) {
    let mut leaves = Vec::new();
    let mut vertical_cuts = 0;
    cut(usable(runs), 0, &mut leaves, &mut vertical_cuts);
    leaves.retain(|leaf| !leaf.is_empty());
    (leaves, vertical_cuts + 1)
}

/// Render one page's runs as text.
pub fn render_page(runs: &[Run], mode: LayoutMode, max_characters: usize) -> RenderedPage {
    match mode {
        LayoutMode::Preserve => render_preserved(runs, max_characters),
        LayoutMode::Single => {
            let lines = group_lines(runs);
            let mut page = render_lines(&[lines], max_characters);
            page.columns = 1;
            page
        }
        LayoutMode::Auto => {
            let (leaves, columns) = reading_order(runs);
            let blocks: Vec<Vec<Line>> = leaves.iter().map(|leaf| group_lines(leaf)).collect();
            let mut page = render_lines(&blocks, max_characters);
            page.columns = columns;
            page
        }
    }
}

fn render_lines(blocks: &[Vec<Line>], max_characters: usize) -> RenderedPage {
    let mut page = RenderedPage {
        columns: 1,
        ..RenderedPage::default()
    };
    for block in blocks {
        if block.is_empty() {
            continue;
        }
        if !page.text.is_empty() {
            page.text.push_str("\n\n");
        }
        for (index, line) in block.iter().enumerate() {
            if index > 0 {
                page.text.push('\n');
            }
            page.text.push_str(&line.text());
            page.lines += 1;
            if page.text.chars().count() >= max_characters {
                page.truncated = true;
                truncate_to(&mut page.text, max_characters);
                return page;
            }
        }
    }
    page
}

/// Draw the page into a fixed-pitch grid.
fn render_preserved(runs: &[Run], max_characters: usize) -> RenderedPage {
    let lines = group_lines(runs);
    let mut page = RenderedPage {
        columns: 1,
        ..RenderedPage::default()
    };
    if lines.is_empty() {
        return page;
    }

    // One character cell is the page's median glyph advance, measured from the
    // runs themselves rather than assumed.
    let cell = {
        let advances: Vec<f32> = lines
            .iter()
            .flat_map(|line| line.runs.iter())
            .filter_map(|run| {
                let characters = run.text.chars().count();
                (characters > 0 && run.width > 0.0).then(|| run.width / characters as f32)
            })
            .collect();
        let measured = median(advances);
        if measured > 0.1 {
            measured
        } else {
            FALLBACK_CELL_WIDTH
        }
    };
    // Column zero is the leftmost text on the page, not the page's own left
    // edge: what this mode is for is relative alignment, and a 72pt margin
    // would otherwise spend fourteen of the four hundred available columns
    // reproducing whitespace nobody asked for.
    let left_edge = {
        let leftmost = lines.iter().map(Line::left).fold(f32::INFINITY, f32::min);
        if leftmost.is_finite() { leftmost } else { 0.0 }
    };
    let pitch = median(
        lines
            .windows(2)
            .map(|pair| pair[0].y - pair[1].y)
            .collect::<Vec<f32>>(),
    );

    let mut previous_y: Option<f32> = None;
    for line in &lines {
        if let Some(previous) = previous_y {
            page.text.push('\n');
            // One blank line where the page left a gap bigger than its own
            // line pitch, so a heading does not glue itself to a paragraph.
            if pitch > 0.0 && previous - line.y > 1.6 * pitch {
                page.text.push('\n');
            }
        }
        let mut rendered = String::new();
        let mut previous_right: Option<f32> = None;
        for run in &line.runs {
            // A run that continues the previous one — the pieces of a kerned
            // word, which a producer emits as separate strings a fraction of a
            // point apart — is appended with nothing between it and its
            // neighbour. Placing it on the character grid instead would round
            // the fraction up to a whole cell and cut the word in half, which
            // is the difference between "KUANTUM" and "KU ANTUM".
            let continues_a_word = previous_right.is_some_and(|right| {
                run.x - right <= WORD_GAP_RATIO * line.height.max(run.height).max(1.0)
            });
            previous_right = Some(run.right());
            if continues_a_word {
                rendered.push_str(run.text.trim_end());
                continue;
            }

            let column =
                (((run.x - left_edge) / cell).round().max(0.0) as usize).min(MAX_PRESERVED_COLUMNS);
            let current = rendered.chars().count();
            if column > current {
                rendered.extend(std::iter::repeat_n(' ', column - current));
            } else if current > 0 && !rendered.ends_with(' ') {
                rendered.push(' ');
            }
            rendered.push_str(run.text.trim_end());
        }
        page.text.push_str(rendered.trim_end());
        page.lines += 1;
        if page.text.chars().count() >= max_characters {
            page.truncated = true;
            truncate_to(&mut page.text, max_characters);
            return page;
        }
        previous_y = Some(line.y);
    }
    page
}

fn truncate_to(text: &mut String, max_characters: usize) {
    if text.chars().count() <= max_characters {
        return;
    }
    let end = text
        .char_indices()
        .nth(max_characters)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(x: f32, y: f32, text: &str) -> Run {
        sized_run(x, y, 10.0, text)
    }

    /// A run whose width is what the test font would give it: half an em per
    /// character.
    fn sized_run(x: f32, y: f32, size: f32, text: &str) -> Run {
        Run {
            x,
            y,
            width: 0.5 * size * text.chars().count() as f32,
            height: size,
            text: text.to_string(),
            invisible: false,
        }
    }

    /// A block of `count` lines starting at `top` and descending, each filling
    /// `width` points, as a column of prose would.
    ///
    /// The padding out to `width` is the point of the helper. Prose runs to the
    /// edge of its column on nearly every line, and that is exactly what tells
    /// a column of text apart from a column of table cells — so a test that
    /// used four-character "lines" would be testing a page nobody has.
    fn prose_column(x: f32, top: f32, count: usize, width: f32, label: &str) -> Vec<Run> {
        let characters = (width / 5.0).round().max(1.0) as usize;
        let filler: String = label
            .chars()
            .cycle()
            .take(characters.saturating_sub(label.chars().count() + 1))
            .collect();
        let text = format!("{label} {filler}");
        (0..count)
            .map(|index| run(x, top - 14.0 * index as f32, &text))
            .collect()
    }

    /// The two body columns of a US Letter page: 72pt margins, a 24pt gutter,
    /// and 222pt of text in each column.
    fn two_body_columns(rows: usize) -> Vec<Run> {
        let mut runs = prose_column(72.0, 700.0, rows, 222.0, "left");
        runs.extend(prose_column(318.0, 700.0, rows, 222.0, "right"));
        runs
    }

    fn render(runs: &[Run], mode: LayoutMode) -> String {
        render_page(runs, mode, 100_000).text
    }

    #[test]
    fn runs_on_one_baseline_become_one_line_in_left_to_right_order() {
        let lines = group_lines(&[run(200.0, 700.0, "second"), run(72.0, 700.0, "first")]);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "first second");
    }

    #[test]
    fn a_superscript_stays_on_the_line_it_annotates() {
        // A footnote marker sits four points above the baseline and directly
        // against the word it annotates.
        let lines = group_lines(&[
            run(72.0, 700.0, "citation"),
            sized_run(112.0, 704.0, 6.0, "12"),
        ]);

        assert_eq!(lines.len(), 1, "{lines:?}");
        // Joined without a space, because there is no gap: the marker belongs
        // to the word, and inserting a space would invent one.
        assert_eq!(lines[0].text(), "citation12");
    }

    #[test]
    fn a_kerning_gap_joins_and_a_word_gap_separates() {
        // "Wa" then "ter" tucked in tight is one word.
        let tight = group_lines(&[run(72.0, 700.0, "Wa"), run(81.5, 700.0, "ter")]);
        assert_eq!(tight[0].text(), "Water");

        // The same two strings with a real space between them are two words.
        let loose = group_lines(&[run(72.0, 700.0, "Wa"), run(87.0, 700.0, "ter")]);
        assert_eq!(loose[0].text(), "Wa ter");
    }

    #[test]
    fn a_run_that_already_carries_its_space_does_not_get_another() {
        let lines = group_lines(&[run(72.0, 700.0, "first "), run(120.0, 700.0, "second")]);

        assert_eq!(lines[0].text(), "first second");
    }

    #[test]
    fn lines_are_read_top_to_bottom() {
        let lines = group_lines(&[
            run(72.0, 600.0, "lower"),
            run(72.0, 700.0, "upper"),
            run(72.0, 650.0, "middle"),
        ]);

        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(text, vec!["upper", "middle", "lower"]);
    }

    /// The failure this whole module exists to prevent: two columns read in
    /// operator order, or in naive baseline order, interleave.
    #[test]
    fn two_columns_are_read_one_after_the_other_and_not_interleaved() {
        let runs = two_body_columns(6);

        let text = render(&runs, LayoutMode::Auto);

        let left_end = text.rfind("left").expect("left column present");
        let right_start = text.find("right").expect("right column present");
        assert!(
            left_end < right_start,
            "every left-column line must precede every right-column line:\n{text}"
        );
        assert_eq!(render_page(&runs, LayoutMode::Auto, 100_000).columns, 2);
    }

    #[test]
    fn single_mode_reads_the_same_two_columns_across_the_page() {
        let runs = two_body_columns(6);

        let page = render_page(&runs, LayoutMode::Single, 100_000);

        let first = page.text.lines().next().expect("a first line");
        assert!(
            first.starts_with("left") && first.contains("right"),
            "single mode must read straight across the page:\n{first}"
        );
        assert_eq!(page.columns, 1);
    }

    #[test]
    fn a_full_width_heading_above_two_columns_is_read_first() {
        let mut runs = vec![sized_run(
            72.0,
            740.0,
            18.0,
            "A heading that spans the whole page",
        )];
        runs.extend(two_body_columns(6));

        let text = render(&runs, LayoutMode::Auto);

        assert!(text.starts_with("A heading"), "{text}");
        let left_end = text.rfind("left").expect("left column present");
        let right_start = text.find("right").expect("right column present");
        assert!(left_end < right_start, "{text}");
        assert_eq!(render_page(&runs, LayoutMode::Auto, 100_000).columns, 2);
    }

    #[test]
    fn three_columns_are_reported_and_read_left_to_right() {
        let mut runs = prose_column(50.0, 700.0, 6, 170.0, "one");
        runs.extend(prose_column(250.0, 700.0, 6, 170.0, "two"));
        runs.extend(prose_column(450.0, 700.0, 6, 130.0, "three"));

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(page.columns, 3, "{}", page.text);
        let one = page.text.rfind("one").expect("one");
        let two = page.text.rfind("two").expect("two");
        let three = page.text.find("three").expect("three");
        assert!(one < two && two < three, "{}", page.text);
    }

    #[test]
    fn a_single_column_page_is_not_split_by_an_indent_or_a_ragged_edge() {
        let mut runs = prose_column(72.0, 700.0, 8, 400.0, "body");
        // An indented first line and a short last line: both leave whitespace
        // on one side, neither is a gutter.
        runs.push(run(108.0, 560.0, "an indented line"));
        runs.push(run(72.0, 546.0, "short"));

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(page.columns, 1, "{}", page.text);
    }

    #[test]
    fn a_two_row_label_and_value_pair_is_not_mistaken_for_two_columns() {
        // Only two rows, so the line-count guard refuses the cut.
        let runs = vec![
            run(72.0, 700.0, "Invoice"),
            run(300.0, 700.0, "INV-4491"),
            run(72.0, 686.0, "Date"),
            run(300.0, 686.0, "2024-11-02"),
        ];

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(page.columns, 1, "{}", page.text);
        assert!(page.text.starts_with("Invoice INV-4491"), "{}", page.text);
    }

    /// The other side of the coin from
    /// `two_columns_are_read_one_after_the_other_and_not_interleaved`: the same
    /// geometry, but with short cells instead of full-width prose, has to stay
    /// row by row so each term keeps its definition.
    #[test]
    fn a_two_column_definition_list_is_read_row_by_row_rather_than_column_by_column() {
        let terms = [
            "Entanglement",
            "Superposition",
            "Interference",
            "Decoherence",
            "Qubit",
        ];
        let mut runs = Vec::new();
        for (index, term) in terms.iter().enumerate() {
            let y = 700.0 - 20.0 * index as f32;
            runs.push(run(72.0, y, term));
            runs.push(run(250.0, y, &format!("what {term} means")));
        }

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(
            page.columns, 1,
            "a definition list is not two columns:\n{}",
            page.text
        );
        assert!(
            page.text
                .starts_with("Entanglement what Entanglement means"),
            "each term must keep its definition:\n{}",
            page.text
        );
    }

    #[test]
    fn side_by_side_blocks_that_do_not_overlap_vertically_are_not_columns() {
        // A block on the left at the top of the page and one on the right at
        // the bottom share a corridor but are not columns.
        let mut runs = prose_column(72.0, 700.0, 4, 222.0, "upper");
        runs.extend(prose_column(318.0, 400.0, 4, 222.0, "lower"));

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(page.columns, 1, "{}", page.text);
    }

    #[test]
    fn a_wide_vertical_gap_separates_blocks_with_a_blank_line() {
        let mut runs = prose_column(72.0, 700.0, 4, 300.0, "first");
        runs.extend(prose_column(72.0, 500.0, 4, 300.0, "second"));

        let text = render(&runs, LayoutMode::Auto);

        let split = text.find("\n\n").expect("a blank line between the blocks");
        assert!(text[..split].contains("first"), "{text}");
        assert!(!text[..split].contains("second"), "{text}");
    }

    #[test]
    fn preserve_mode_keeps_horizontal_alignment_as_padding() {
        // Indented by a 72pt page margin, as every real page is.
        let runs = vec![
            run(72.0, 700.0, "Name"),
            run(172.0, 700.0, "Amount"),
            run(72.0, 686.0, "Widget"),
            run(172.0, 686.0, "12.00"),
        ];

        let text = render(&runs, LayoutMode::Preserve);

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        // Both second-column entries start at the same character column, which
        // is the whole point of the mode.
        assert_eq!(
            lines[0].find("Amount"),
            lines[1].find("12.00"),
            "columns must line up:\n{text}"
        );
        // And the page margin is not reproduced: column zero is the leftmost
        // text, not the left edge of the paper.
        assert!(lines[0].starts_with("Name"), "{text}");
    }

    /// Real producers emit a kerned word as several strings a fraction of a
    /// point apart. Rounding each of those onto the character grid would put a
    /// space inside the word — "KU ANTUM" instead of "KUANTUM" — which is the
    /// way a fixed-pitch rendering usually goes wrong.
    #[test]
    fn preserve_mode_does_not_break_a_kerned_word_onto_the_character_grid() {
        let runs = vec![
            run(0.0, 700.0, "KU"),
            run(10.3, 700.0, "AN"),
            run(20.1, 700.0, "TUM"),
            run(60.0, 700.0, "ALGORITMA"),
        ];

        let text = render(&runs, LayoutMode::Preserve);

        assert!(text.starts_with("KUANTUM"), "{text}");
        assert!(text.contains("ALGORITMA"), "{text}");
    }

    #[test]
    fn preserve_mode_cannot_be_made_to_draw_an_enormous_line() {
        let runs = vec![run(0.0, 700.0, "here"), run(1.0e9, 700.0, "far away")];

        let text = render(&runs, LayoutMode::Preserve);

        assert!(
            text.lines()
                .all(|line| line.chars().count() <= MAX_PRESERVED_COLUMNS + 16),
            "a run positioned off the page must not become a line of a million spaces"
        );
    }

    #[test]
    fn a_character_budget_truncates_and_says_so() {
        let runs = prose_column(72.0, 700.0, 20, 300.0, "line");

        let page = render_page(&runs, LayoutMode::Auto, 50);

        assert!(page.truncated);
        assert_eq!(page.text.chars().count(), 50);
    }

    #[test]
    fn runs_with_impossible_geometry_are_dropped_rather_than_poisoning_the_sort() {
        let runs = vec![
            Run {
                x: f32::NAN,
                y: 700.0,
                width: 10.0,
                height: 10.0,
                text: "poison".to_string(),
                invisible: false,
            },
            Run {
                x: 72.0,
                y: f32::INFINITY,
                width: 10.0,
                height: 10.0,
                text: "also poison".to_string(),
                invisible: false,
            },
            run(72.0, 700.0, "real text"),
        ];

        let page = render_page(&runs, LayoutMode::Auto, 100_000);

        assert_eq!(page.text, "real text");
    }

    #[test]
    fn whitespace_only_runs_do_not_become_lines() {
        let page = render_page(
            &[run(72.0, 700.0, "   "), run(72.0, 686.0, "text")],
            LayoutMode::Auto,
            1000,
        );

        assert_eq!(page.text, "text");
        assert_eq!(page.lines, 1);
    }

    #[test]
    fn an_empty_page_renders_to_nothing_without_panicking() {
        for mode in [LayoutMode::Auto, LayoutMode::Single, LayoutMode::Preserve] {
            let page = render_page(&[], mode, 1000);
            assert!(page.text.is_empty(), "{mode:?}");
            assert_eq!(page.lines, 0);
        }
    }

    #[test]
    fn merging_intervals_finds_the_corridor_between_them() {
        let merged = merge_intervals(vec![(0.0, 10.0), (5.0, 20.0), (60.0, 80.0)]);

        assert_eq!(merged, vec![(0.0, 20.0), (60.0, 80.0)]);
        assert_eq!(widest_gap(&merged, 10.0), Some((20.0, 60.0)));
        assert_eq!(widest_gap(&merged, 50.0), None);
    }
}
