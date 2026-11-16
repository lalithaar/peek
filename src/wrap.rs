//! Soft wrapping. One logical line can occupy several rows on screen, so the
//! buffer needs to know where those rows start: everything here works in char
//! indices into the line, because that is what the cursor is measured in.
//!
//! Rows are measured in terminal cells, with tabs expanded from the row's own
//! left edge, so the caret arithmetic and the drawn text always agree.

use std::ops::Range;

pub const TAB_WIDTH: usize = 4;

fn cell(ch: char, col: usize) -> usize {
    if ch == '\t' {
        TAB_WIDTH - (col % TAB_WIDTH)
    } else {
        1
    }
}

/// Split `line` into the visual rows it occupies at `width` cells. Always
/// returns at least one row, and the rows together cover the whole line.
pub fn segments(line: &str, width: usize) -> Vec<Range<usize>> {
    let chars: Vec<char> = line.chars().collect();
    let total = chars.len();
    if width == 0 || total == 0 {
        return std::iter::once(0..total).collect();
    }

    let mut rows: Vec<Range<usize>> = Vec::new();
    let mut start = 0usize;
    while start < total {
        let mut col = 0usize;
        let mut end = start;
        while end < total {
            let step = cell(chars[end], col);
            if col + step > width {
                break;
            }
            col += step;
            end += 1;
        }
        if end >= total {
            rows.push(start..total);
            break;
        }
        if end == start {
            // One character wider than the whole row (a tab at a narrow
            // width, say). Show it rather than looping forever.
            rows.push(start..start + 1);
            start += 1;
            continue;
        }
        // Break after the last space that still fits, so words stay whole and
        // a row never starts with the space that ended the previous one.
        if let Some(space) = (start..end).rev().find(|&i| chars[i].is_whitespace()) {
            end = space + 1;
        }
        rows.push(start..end);
        start = end;
    }

    if rows.is_empty() {
        rows.push(0..total);
    }
    rows
}

/// Which row of `rows` the cursor column `col` belongs to. A column sitting
/// exactly on a row boundary belongs to the following row, which is where a
/// typed character would appear.
pub fn segment_of(rows: &[Range<usize>], col: usize) -> usize {
    for (index, row) in rows.iter().enumerate() {
        if col < row.end {
            return index;
        }
    }
    rows.len().saturating_sub(1)
}

/// Display column of `col` measured from `start`, i.e. how far into its row
/// the caret sits.
pub fn col_in_row(line: &str, start: usize, col: usize) -> usize {
    let mut col_here = 0usize;
    for (index, ch) in line.chars().enumerate() {
        if index < start {
            continue;
        }
        if index >= col {
            break;
        }
        col_here += cell(ch, col_here);
    }
    col_here
}

/// Inverse of `col_in_row`: the char index at display offset `offset` in the
/// row starting at `start`. Lands before a character that would not fit.
pub fn col_at_offset(line: &str, start: usize, offset: usize) -> usize {
    let mut col_here = 0usize;
    for (index, ch) in line.chars().enumerate() {
        if index < start {
            continue;
        }
        let step = cell(ch, col_here);
        if col_here + step > offset {
            return index;
        }
        col_here += step;
    }
    line.chars().count()
}

/// The text of one row, with tabs expanded so it lines up with the caret.
pub fn expand_range(line: &str, row: &Range<usize>) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for (index, ch) in line.chars().enumerate() {
        if index < row.start {
            continue;
        }
        if index >= row.end {
            break;
        }
        if ch == '\t' {
            let fill = TAB_WIDTH - (col % TAB_WIDTH);
            out.push_str(&" ".repeat(fill));
            col += fill;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &str, rows: &[Range<usize>]) -> Vec<String> {
        rows.iter().map(|row| expand_range(line, row)).collect()
    }

    #[test]
    fn short_line_is_a_single_row() {
        let rows = segments("hello", 20);
        assert_eq!(rows.len(), 1);
        assert_eq!(text("hello", &rows), vec!["hello".to_string()]);
    }

    #[test]
    fn empty_line_still_occupies_a_row() {
        assert_eq!(segments("", 10), vec![0..0]);
    }

    #[test]
    fn breaks_between_words_not_through_them() {
        let line = "one two three four";
        let rows = segments(line, 10);
        assert_eq!(
            text(line, &rows),
            vec!["one two ".to_string(), "three four".to_string()]
        );
    }

    #[test]
    fn long_word_is_split_at_the_edge() {
        let line = "abcdefghijklmno";
        let rows = segments(line, 5);
        assert_eq!(text(line, &rows), vec!["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn rows_cover_the_line_without_gaps_or_overlap() {
        let line = "  fn wrap(text: &str) -> Vec<usize> { }";
        let rows = segments(line, 7);
        assert_eq!(rows.first().unwrap().start, 0);
        assert_eq!(rows.last().unwrap().end, line.chars().count());
        for pair in rows.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        for row in &rows {
            let width = col_in_row(line, row.start, row.end);
            assert!(width <= 7, "row {row:?} is {width} cells wide");
        }
    }

    #[test]
    fn caret_offsets_round_trip() {
        let line = "\tlet answer = 42;";
        let rows = segments(line, 6);
        for row in &rows {
            for col in row.start..=row.end {
                let offset = col_in_row(line, row.start, col);
                assert!(offset <= 6, "offset {offset} for col {col} in {row:?}");
                let here = segment_of(&rows, col);
                assert!(
                    col >= rows[here].start && col <= rows[here].end,
                    "col {col} landed on row {here} = {:?}",
                    rows[here]
                );
            }
        }
        // Column 0 of the second row maps back to its first character.
        let second = rows[1].clone();
        let offset = col_in_row(line, second.start, second.start + 2);
        assert_eq!(col_at_offset(line, second.start, offset), second.start + 2);
    }

    #[test]
    fn cursor_on_a_row_boundary_moves_to_the_next_row() {
        let rows = segments("one two three", 8);
        // "one two " is row 0, so the caret there belongs to row 1.
        assert_eq!(segment_of(&rows, 0), 0);
        assert_eq!(segment_of(&rows, rows[0].end), 1);
    }
}
