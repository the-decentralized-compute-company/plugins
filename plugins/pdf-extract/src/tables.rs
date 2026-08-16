//! Recovering tables from alignment.
//!
//! There is no table in a PDF. There is text at coordinates, and — sometimes —
//! lines drawn near it. What makes something a table is that the text lines up:
//! several consecutive rows whose cells start at the same handful of
//! horizontal positions. That is the signal this module reads, and the whole of
//! it.
//!
//! **Drawn borders are ignored.** A ruled table and an unruled one are detected
//! the same way, by alignment, so a bordered table with badly aligned text is
//! not recovered and an unbordered one with well aligned text is. That is
//! stated here and in the README rather than left to be discovered.
//!
//! The hard part is not finding tables, it is *not* finding them in ordinary
//! prose. Justified text has stretched word spaces that look like cell
//! separators, so lines split into pieces that a naive detector reads as two
//! columns. The discriminator is occupancy: a column is only a column if most
//! rows put something in it. Prose fails that test because its second piece
//! starts at a different place on every line.

use crate::glyphs::Run;
use crate::layout::Line;

/// A gap wider than this multiple of the line height separates two cells. Well
/// above the word-space threshold in [`crate::layout`], because the cost of
/// splitting a sentence into cells is higher than the cost of missing a narrow
/// column.
const CELL_GAP_RATIO: f32 = 0.75;

/// Cell starts within this multiple of the line height belong to one column.
const COLUMN_TOLERANCE_RATIO: f32 = 0.4;
const MIN_COLUMN_TOLERANCE: f32 = 4.0;

/// Two rows further apart than this many line heights are not in one table.
const MAX_ROW_PITCH_RATIO: f32 = 2.5;

/// A column has to appear in this fraction of a table's rows to be a column
/// rather than an accident of spacing.
const MIN_COLUMN_OCCUPANCY: f32 = 0.5;

/// A table is rejected when more than this fraction of its cells collide in a
/// column with another cell from the same row.
const MAX_COLLISION_RATIO: f32 = 0.2;

/// How much of the space up to the next column a cell may fill before the
/// "columns" are read as blocks of text rather than as cells.
///
/// This is what keeps the body of a two-column article from being reported as
/// a two-column table. Both look like aligned rows; the difference is that a
/// column of prose runs the full width of its band on nearly every line, and a
/// table cell does not.
const MAX_CELL_FILL_RATIO: f32 = 0.8;

/// Fraction of a region's lines that has to belong to a detected table before
/// [`crate::layout`] treats the region as tabular and declines to cut it into
/// columns.
const MIN_TABLE_COVERAGE: f32 = 0.6;
const MIN_TABLE_COVERAGE_LINES: usize = 3;

/// Cells kept per table, and rows per table. A page that produces more than
/// this is not a table anybody wants back.
const MAX_COLUMNS: usize = 64;
const MAX_ROWS: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableOptions {
    pub min_rows: usize,
    pub min_columns: usize,
}

impl Default for TableOptions {
    fn default() -> Self {
        Self {
            min_rows: 2,
            min_columns: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub columns: usize,
    /// Mean fraction of rows in which each kept column carried a cell. `1.0`
    /// is a table with no gaps; lower means the grid had holes.
    pub occupancy: f32,
    /// Baseline of the top row and of the bottom row, in page points.
    pub top: f32,
    pub bottom: f32,
}

/// One piece of a row, positioned.
#[derive(Clone, Debug, PartialEq)]
struct Cell {
    x: f32,
    /// Right edge of the text, which is what tells a cell from a line of prose.
    right: f32,
    text: String,
}

/// Split a line into cells at gaps wide enough to be column separators, both
/// between runs and inside a run whose producer wrote the padding as spaces.
fn cells_of(line: &Line) -> Vec<Cell> {
    let threshold = CELL_GAP_RATIO * line.height.max(1.0);
    let mut cells: Vec<Cell> = Vec::new();
    let mut previous_right: Option<f32> = None;

    for run in &line.runs {
        let pieces = split_run(run);
        for (index, piece) in pieces.iter().enumerate() {
            let starts_cell = match previous_right {
                None => true,
                // A run that was split internally starts a new cell at every
                // piece after the first: that is what the spaces meant.
                Some(_) if index > 0 => true,
                Some(right) => piece.x - right > threshold,
            };
            if starts_cell {
                cells.push(piece.clone());
            } else if let Some(last) = cells.last_mut() {
                if !last.text.ends_with(' ') {
                    last.text.push(' ');
                }
                last.text.push_str(&piece.text);
                last.right = piece.right;
            }
            previous_right = Some(piece.right);
        }
    }

    for cell in &mut cells {
        cell.text = cell.text.trim().to_string();
    }
    cells.retain(|cell| !cell.text.is_empty());
    cells
}

/// Split one run on two-or-more consecutive spaces, which is how a producer
/// that emitted a whole row as a single string wrote its column padding.
fn split_run(run: &Run) -> Vec<Cell> {
    let characters: Vec<char> = run.text.chars().collect();
    let advance = if characters.is_empty() {
        0.0
    } else {
        run.width / characters.len() as f32
    };

    let mut pieces: Vec<Cell> = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < characters.len() {
        if characters[index] == ' ' {
            let mut end = index;
            while end < characters.len() && characters[end] == ' ' {
                end += 1;
            }
            if end - index >= 2 {
                if index > start {
                    pieces.push(Cell {
                        x: run.x + start as f32 * advance,
                        right: run.x + index as f32 * advance,
                        text: characters[start..index].iter().collect(),
                    });
                }
                start = end;
            }
            index = end;
            continue;
        }
        index += 1;
    }
    if start < characters.len() {
        pieces.push(Cell {
            x: run.x + start as f32 * advance,
            right: run.x + characters.len() as f32 * advance,
            text: characters[start..].iter().collect(),
        });
    }
    if pieces.is_empty() {
        pieces.push(Cell {
            x: run.x,
            right: run.right(),
            text: run.text.clone(),
        });
    }
    pieces
}

/// Group cell start positions into columns.
fn cluster_columns(mut starts: Vec<f32>, tolerance: f32) -> Vec<f32> {
    starts.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let mut columns: Vec<f32> = Vec::new();
    for start in starts {
        match columns.last() {
            Some(anchor) if start - anchor <= tolerance => {}
            _ => columns.push(start),
        }
        if columns.len() > MAX_COLUMNS {
            break;
        }
    }
    columns
}

fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn nearest_column(columns: &[f32], x: f32) -> usize {
    let mut best = 0;
    let mut best_distance = f32::INFINITY;
    for (index, anchor) in columns.iter().enumerate() {
        let distance = (x - anchor).abs();
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }
    best
}

/// Split the lines into runs of consecutive rows that could be one table.
fn candidate_groups<'a>(lines: &'a [Line]) -> Vec<Vec<&'a Line>> {
    let mut groups: Vec<Vec<&'a Line>> = Vec::new();
    let mut current: Vec<&'a Line> = Vec::new();
    let mut previous: Option<&Line> = None;

    for line in lines {
        let tabular = cells_of(line).len() >= 2;
        let far = previous.is_some_and(|previous| {
            previous.y - line.y > MAX_ROW_PITCH_RATIO * previous.height.max(line.height).max(1.0)
        });
        if !tabular || far {
            if current.len() >= 2 {
                groups.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        if tabular {
            current.push(line);
        }
        previous = Some(line);
    }
    if current.len() >= 2 {
        groups.push(current);
    }
    groups
}

/// Does this block of lines read as a table?
///
/// [`crate::layout`] asks before it cuts a region into columns. A table and a
/// pair of prose columns look identical from above — two stacks of text with a
/// corridor between them — but they must be read in opposite ways. Prose is
/// read one column at a time; a table is read one *row* at a time, because a
/// row's cells belong together and cutting down the gutter separates every
/// label from its value.
///
/// The discriminator is the one in [`build_table`]: a column of prose fills
/// the width of its band, and a table cell does not.
pub fn covers_as_table(lines: &[Line]) -> bool {
    if lines.len() < MIN_TABLE_COVERAGE_LINES {
        return false;
    }
    let rows: usize = find_tables(lines, &TableOptions::default())
        .iter()
        .map(|table| table.rows.len())
        .sum();
    rows as f32 / lines.len() as f32 >= MIN_TABLE_COVERAGE
}

/// Find every table in one already-ordered block of lines.
pub fn find_tables(lines: &[Line], options: &TableOptions) -> Vec<Table> {
    let min_rows = options.min_rows.max(2);
    let min_columns = options.min_columns.max(2);

    candidate_groups(lines)
        .into_iter()
        .filter_map(|group| build_table(&group, min_rows, min_columns))
        .collect()
}

fn build_table(group: &[&Line], min_rows: usize, min_columns: usize) -> Option<Table> {
    if group.len() < min_rows || group.len() > MAX_ROWS {
        return None;
    }

    let height = group
        .iter()
        .map(|line| line.height)
        .fold(0.0_f32, f32::max)
        .max(1.0);
    let tolerance = (COLUMN_TOLERANCE_RATIO * height).max(MIN_COLUMN_TOLERANCE);

    let rows: Vec<Vec<Cell>> = group.iter().map(|line| cells_of(line)).collect();
    let columns = cluster_columns(
        rows.iter().flatten().map(|cell| cell.x).collect(),
        tolerance,
    );
    if columns.len() < min_columns {
        return None;
    }

    // How many rows put something in each column. A column nothing lands in
    // reliably is spacing, not structure.
    let mut occupancy = vec![0usize; columns.len()];
    for row in &rows {
        let mut seen = vec![false; columns.len()];
        for cell in row {
            let index = nearest_column(&columns, cell.x);
            if !seen[index] {
                seen[index] = true;
                occupancy[index] += 1;
            }
        }
    }
    let kept: Vec<f32> = columns
        .iter()
        .zip(&occupancy)
        .filter(|(_, count)| **count as f32 / rows.len() as f32 >= MIN_COLUMN_OCCUPANCY)
        .map(|(anchor, _)| *anchor)
        .collect();
    if kept.len() < min_columns {
        return None;
    }

    let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    let mut collisions = 0usize;
    let mut cell_total = 0usize;
    let mut filled = vec![0usize; kept.len()];
    let mut fills: Vec<f32> = Vec::new();
    for row in &rows {
        let mut rendered = vec![String::new(); kept.len()];
        for cell in row {
            cell_total += 1;
            let index = nearest_column(&kept, cell.x);
            // How much of the room before the next column this cell used. The
            // last column has no next one, so it is not measured.
            if index + 1 < kept.len() {
                let band = kept[index + 1] - kept[index];
                if band > 0.0 {
                    fills.push(((cell.right - kept[index]).max(0.0) / band).min(2.0));
                }
            }
            if rendered[index].is_empty() {
                rendered[index] = cell.text.clone();
                filled[index] += 1;
            } else {
                // Two pieces of one row landing in one column means the grid
                // does not describe this row. Joined rather than dropped, and
                // counted against the table's credibility.
                collisions += 1;
                rendered[index].push(' ');
                rendered[index].push_str(&cell.text);
            }
        }
        grid.push(rendered);
    }

    if cell_total == 0 || collisions as f32 / cell_total as f32 > MAX_COLLISION_RATIO {
        return None;
    }
    // Cells that run the full width of their band are paragraphs sitting in
    // columns, not table cells.
    if median(&mut fills).is_some_and(|fill| fill > MAX_CELL_FILL_RATIO) {
        return None;
    }
    // Every row is entirely empty in some pathological case; a table of blanks
    // is not an answer.
    if grid.iter().all(|row| row.iter().all(String::is_empty)) {
        return None;
    }

    let occupancy = filled.iter().sum::<usize>() as f32 / (kept.len() * grid.len()) as f32;
    Some(Table {
        columns: kept.len(),
        rows: grid,
        occupancy,
        top: group.first().map(|line| line.y).unwrap_or_default(),
        bottom: group.last().map(|line| line.y).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::group_lines;

    fn run(x: f32, y: f32, text: &str) -> Run {
        Run {
            x,
            y,
            width: 5.0 * text.chars().count() as f32,
            height: 10.0,
            text: text.to_string(),
            invisible: false,
        }
    }

    fn tables_of(runs: &[Run]) -> Vec<Table> {
        find_tables(&group_lines(runs), &TableOptions::default())
    }

    /// A three-column table with a header, as a word processor would place it.
    fn invoice_runs() -> Vec<Run> {
        let mut runs = Vec::new();
        for (index, row) in [
            ["Item", "Quantity", "Total"],
            ["Widget", "2", "24.00"],
            ["Gasket", "10", "8.50"],
            ["Flange", "1", "119.00"],
        ]
        .iter()
        .enumerate()
        {
            let y = 700.0 - 14.0 * index as f32;
            runs.push(run(72.0, y, row[0]));
            runs.push(run(250.0, y, row[1]));
            runs.push(run(400.0, y, row[2]));
        }
        runs
    }

    #[test]
    fn an_aligned_grid_is_recovered_with_its_header_row() {
        let tables = tables_of(&invoice_runs());

        assert_eq!(tables.len(), 1, "{tables:?}");
        let table = &tables[0];
        assert_eq!(table.columns, 3);
        assert_eq!(
            table.rows,
            vec![
                vec!["Item", "Quantity", "Total"],
                vec!["Widget", "2", "24.00"],
                vec!["Gasket", "10", "8.50"],
                vec!["Flange", "1", "119.00"],
            ]
        );
        assert!((table.occupancy - 1.0).abs() < 0.001, "{table:?}");
        assert!(table.top > table.bottom);
    }

    #[test]
    fn a_row_missing_a_value_leaves_the_cell_empty_rather_than_shifting_the_others() {
        let mut runs = invoice_runs();
        // A fifth row with no quantity.
        runs.push(run(72.0, 644.0, "Bracket"));
        runs.push(run(400.0, 644.0, "3.00"));

        let tables = tables_of(&runs);

        let table = &tables[0];
        assert_eq!(
            table.rows.last().expect("last row"),
            &vec!["Bracket".to_string(), String::new(), "3.00".to_string()],
            "a hole in a row must stay a hole"
        );
        assert!(table.occupancy < 1.0);
    }

    #[test]
    fn a_row_written_as_one_string_with_space_padding_is_split_on_the_padding() {
        let runs = vec![
            run(72.0, 700.0, "Item      Quantity      Total"),
            run(72.0, 686.0, "Widget    2             24.00"),
            run(72.0, 672.0, "Gasket    10            8.50"),
        ];

        let tables = tables_of(&runs);

        assert_eq!(tables.len(), 1, "{tables:?}");
        assert_eq!(tables[0].columns, 3);
        assert_eq!(tables[0].rows[1], vec!["Widget", "2", "24.00"]);
    }

    #[test]
    fn ordinary_prose_is_not_a_table() {
        let lines = [
            "The quick brown fox jumps over the lazy dog and keeps",
            "running until it reaches the end of the paragraph where",
            "it stops and considers what it has done with its day so",
            "far, which on reflection is not very much at all today.",
        ];
        let runs: Vec<Run> = lines
            .iter()
            .enumerate()
            .map(|(index, text)| run(72.0, 700.0 - 14.0 * index as f32, text))
            .collect();

        assert!(tables_of(&runs).is_empty(), "prose must not become a table");
    }

    /// The case a naive detector gets wrong: justified prose has stretched word
    /// spaces, so every line splits into pieces — but the pieces start
    /// somewhere different on each line, so no column is occupied twice.
    #[test]
    fn justified_prose_with_stretched_spaces_is_not_a_table() {
        let mut runs = Vec::new();
        for (index, offsets) in [[72.0, 210.0], [72.0, 260.0], [72.0, 175.0], [72.0, 300.0]]
            .iter()
            .enumerate()
        {
            let y = 700.0 - 14.0 * index as f32;
            runs.push(run(offsets[0], y, "some words here"));
            runs.push(run(offsets[1], y, "and some more of them"));
        }

        assert!(
            tables_of(&runs).is_empty(),
            "columns that land somewhere new on every line are not columns"
        );
    }

    /// The other case a naive detector gets wrong, and the reason a cell
    /// carries its right edge: the body of a two-column article is aligned rows
    /// of text, and every one of its "cells" runs the full width of its column.
    #[test]
    fn the_body_of_a_two_column_article_is_not_a_table() {
        let mut runs = Vec::new();
        for index in 0..6 {
            let y = 700.0 - 14.0 * index as f32;
            // Forty-five characters at five points each is 225pt of text in a
            // 248pt band: prose filling its column.
            runs.push(run(72.0, y, &"x".repeat(45)));
            runs.push(run(320.0, y, &"y".repeat(45)));
        }

        assert!(
            tables_of(&runs).is_empty(),
            "columns of prose fill their band; table cells do not"
        );
    }

    #[test]
    fn a_single_tabular_row_is_not_a_table() {
        let runs = vec![
            run(72.0, 700.0, "Invoice"),
            run(300.0, 700.0, "INV-4491"),
            run(72.0, 600.0, "Something else entirely further down the page"),
        ];

        assert!(tables_of(&runs).is_empty());
    }

    #[test]
    fn two_tables_separated_by_a_paragraph_are_found_separately() {
        let mut runs = Vec::new();
        for index in 0..3 {
            let y = 700.0 - 14.0 * index as f32;
            runs.push(run(72.0, y, "left"));
            runs.push(run(300.0, y, "right"));
        }
        runs.push(run(
            72.0,
            600.0,
            "A sentence of prose between the two tables.",
        ));
        for index in 0..3 {
            let y = 500.0 - 14.0 * index as f32;
            runs.push(run(72.0, y, "alpha"));
            runs.push(run(300.0, y, "beta"));
        }

        let tables = tables_of(&runs);

        assert_eq!(tables.len(), 2, "{tables:?}");
        assert!(tables[0].top > tables[1].top);
    }

    #[test]
    fn rows_far_apart_are_not_glued_into_one_table() {
        let mut runs = Vec::new();
        for index in 0..3 {
            let y = 700.0 - 14.0 * index as f32;
            runs.push(run(72.0, y, "left"));
            runs.push(run(300.0, y, "right"));
        }
        // Same shape, but two hundred points lower.
        for index in 0..3 {
            let y = 460.0 - 14.0 * index as f32;
            runs.push(run(72.0, y, "left"));
            runs.push(run(300.0, y, "right"));
        }

        assert_eq!(tables_of(&runs).len(), 2);
    }

    #[test]
    fn a_higher_minimum_refuses_a_table_that_meets_only_the_default() {
        let lines = group_lines(&invoice_runs());

        assert!(
            find_tables(
                &lines,
                &TableOptions {
                    min_rows: 2,
                    min_columns: 4,
                }
            )
            .is_empty()
        );
        assert!(
            find_tables(
                &lines,
                &TableOptions {
                    min_rows: 9,
                    min_columns: 2,
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn splitting_a_run_needs_two_spaces_so_ordinary_words_stay_together() {
        let single = split_run(&run(0.0, 0.0, "one two three"));
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].text, "one two three");

        let padded = split_run(&run(0.0, 0.0, "one  two"));
        assert_eq!(padded.len(), 2);
        assert_eq!(padded[0].text, "one");
        assert_eq!(padded[1].text, "two");
        // The second piece starts where its characters do: five characters in,
        // at five points each.
        assert!((padded[1].x - 25.0).abs() < 0.01, "{padded:?}");
    }

    #[test]
    fn clustering_groups_nearby_starts_and_keeps_distant_ones_apart() {
        let columns = cluster_columns(vec![72.0, 73.5, 250.0, 251.0, 400.0], 4.0);

        assert_eq!(columns, vec![72.0, 250.0, 400.0]);
    }

    #[test]
    fn a_region_is_called_tabular_when_most_of_its_lines_are_rows() {
        assert!(covers_as_table(&group_lines(&invoice_runs())));

        // Two columns of prose are aligned rows too, and are not tabular.
        let mut prose = Vec::new();
        for index in 0..6 {
            let y = 700.0 - 14.0 * index as f32;
            prose.push(run(72.0, y, &"x".repeat(45)));
            prose.push(run(320.0, y, &"y".repeat(45)));
        }
        assert!(!covers_as_table(&group_lines(&prose)));

        // And too few lines to judge is not tabular either.
        assert!(!covers_as_table(&group_lines(&[
            run(72.0, 700.0, "a"),
            run(300.0, 700.0, "b"),
        ])));
    }

    #[test]
    fn no_lines_at_all_produce_no_tables_without_panicking() {
        assert!(find_tables(&[], &TableOptions::default()).is_empty());
    }
}
