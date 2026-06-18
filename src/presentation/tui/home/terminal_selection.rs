//! Mouse text selection over the embedded terminal pane.
//!
//! Dragging the mouse across the live pane selects a run of text, exactly like a
//! standalone terminal: the press anchors one end, the drag moves the other, and
//! the selection is a *stream* — it follows reading order, so a multi-row
//! selection takes the rest of the first row, every column of the rows between,
//! and the start of the last row (not a rectangular block).
//!
//! This module is the pure core of that feature: it tracks the two endpoints in
//! grid coordinates, answers which cells fall inside the selection (so the view
//! can paint them inverted), and lifts the selected text out of a
//! [`vt100::Screen`] (so the pane can copy it). The terminal I/O — turning mouse
//! reports into [`extend`](Selection::extend) calls and writing the copied text —
//! lives in the (coverage-excluded) terminal pane; everything here is unit-tested
//! against a parser driven with bytes.

/// A cell position in the visible terminal grid, both 0-based: `row` from the
/// top, `col` from the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    pub row: u16,
    pub col: u16,
}

impl Cell {
    pub fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }

    /// Reading-order comparison: earlier rows come first, and within a row,
    /// earlier columns. This is the order a stream selection runs in.
    fn before_or_equal(self, other: Cell) -> bool {
        (self.row, self.col) <= (other.row, other.col)
    }
}

/// An in-progress or finished drag selection: the `anchor` is where the drag
/// began, the `head` is where it currently ends. Either may be the earlier of
/// the two in reading order, so callers work through [`bounds`](Self::bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    anchor: Cell,
    head: Cell,
}

impl Selection {
    /// Begin a selection anchored (and, until extended, ended) at `cell`.
    pub fn new(cell: Cell) -> Self {
        Self {
            anchor: cell,
            head: cell,
        }
    }

    /// Move the loose end of the selection to `cell` (the dragged-to position),
    /// leaving the anchor put.
    pub fn extend(&mut self, cell: Cell) {
        self.head = cell;
    }

    /// The selection's endpoints in reading order: `(start, end)` with `start`
    /// before or equal to `end`. `end` is inclusive — the cell under the loose
    /// end is part of the selection.
    fn bounds(&self) -> (Cell, Cell) {
        if self.anchor.before_or_equal(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether the cell at (`row`, `col`) lies within the stream selection, so
    /// the view paints it inverted. The first and last rows are bounded by the
    /// endpoints' columns; the rows between are selected in full.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (start, end) = self.bounds();
        if row < start.row || row > end.row {
            return false;
        }
        // On the start row the selection begins at its column; before that the
        // row is not yet selected. (When start and end share a row, both bounds
        // apply.)
        if row == start.row && col < start.col {
            return false;
        }
        // On the end row the selection stops at its column (inclusive).
        if row == end.row && col > end.col {
            return false;
        }
        true
    }

    /// Whether the selection covers a single cell (a click without a drag),
    /// which carries no text worth copying.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Lift the selected text out of `screen`, following the stream the same way
    /// [`contains`](Self::contains) does. Each row contributes its in-range cells
    /// (wide glyphs counted once, blanks as spaces) with trailing spaces trimmed,
    /// and rows are joined with `\n` — so the result pastes like the text looked.
    pub fn extract_text(&self, screen: &vt100::Screen) -> String {
        let (start, end) = self.bounds();
        let (_, cols) = screen.size();
        let mut lines: Vec<String> = Vec::new();
        for row in start.row..=end.row {
            // Stream bounds for this row: the start column only constrains the
            // first row, the end column only the last.
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row {
                end.col
            } else {
                cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in from..=to {
                let cell = screen.cell(row, col);
                // The trailing half of a wide glyph is already covered by the
                // wide cell itself, so it adds nothing.
                if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                    continue;
                }
                match cell {
                    Some(cell) if cell.has_contents() => line.push_str(cell.contents()),
                    _ => line.push(' '),
                }
            }
            // Trailing blanks are padding, not content; drop them so a copied
            // line ends where its text does.
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parser sized `rows`×`cols`, fed `bytes`, returned for inspection.
    fn parsed(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn a_fresh_selection_is_a_single_empty_cell() {
        let sel = Selection::new(Cell::new(1, 2));
        assert!(sel.is_empty());
        assert!(sel.contains(1, 2));
        assert!(!sel.contains(1, 3));
    }

    #[test]
    fn extending_within_a_row_selects_the_span_inclusive() {
        let mut sel = Selection::new(Cell::new(0, 1));
        sel.extend(Cell::new(0, 3));
        assert!(!sel.is_empty());
        assert!(!sel.contains(0, 0));
        assert!(sel.contains(0, 1));
        assert!(sel.contains(0, 3));
        assert!(!sel.contains(0, 4));
    }

    #[test]
    fn a_backward_drag_normalises_its_bounds() {
        // Anchor after the head: the selection still runs from the earlier cell.
        let mut sel = Selection::new(Cell::new(0, 3));
        sel.extend(Cell::new(0, 1));
        assert!(sel.contains(0, 1));
        assert!(sel.contains(0, 3));
        assert!(!sel.contains(0, 0));
        assert!(!sel.contains(0, 4));
    }

    #[test]
    fn a_multi_row_selection_is_a_stream_not_a_block() {
        // From (0,3) to (2,2): rest of row 0, all of row 1, start of row 2.
        let mut sel = Selection::new(Cell::new(0, 3));
        sel.extend(Cell::new(2, 2));
        // Row 0: from column 3 onward (an earlier column is excluded).
        assert!(!sel.contains(0, 2));
        assert!(sel.contains(0, 3));
        assert!(sel.contains(0, 9));
        // Row 1 (a middle row): every column.
        assert!(sel.contains(1, 0));
        assert!(sel.contains(1, 9));
        // Row 2: up to and including column 2, then nothing.
        assert!(sel.contains(2, 2));
        assert!(!sel.contains(2, 3));
        // Rows outside the span are untouched.
        assert!(!sel.contains(3, 0));
    }

    #[test]
    fn extract_text_lifts_a_single_row_span() {
        let parser = parsed(2, 6, b"hello");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 3));
        assert_eq!(sel.extract_text(parser.screen()), "hell");
    }

    #[test]
    fn extract_text_trims_trailing_blanks_per_row() {
        // Selecting past the end of "hi" picks up padding spaces, which are
        // trimmed so the copied line ends at the text.
        let parser = parsed(1, 6, b"hi");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 5));
        assert_eq!(sel.extract_text(parser.screen()), "hi");
    }

    #[test]
    fn extract_text_joins_stream_rows_with_newlines() {
        // "abcdef" wraps to a second row at width 3; a full selection reads both.
        let parser = parsed(2, 3, b"abcdef");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(1, 2));
        assert_eq!(sel.extract_text(parser.screen()), "abc\ndef");
    }

    #[test]
    fn extract_text_counts_a_wide_glyph_once() {
        // The full-width "あ" occupies two columns; its continuation cell must
        // not duplicate the character.
        let parser = parsed(1, 4, "あ".as_bytes());
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 1));
        assert_eq!(sel.extract_text(parser.screen()), "あ");
    }

    #[test]
    fn extract_text_renders_a_blank_gap_as_a_space() {
        // "a c" leaves column 1 blank; a span across it keeps the inner space.
        let parser = parsed(1, 4, b"a\x1b[Cc");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 2));
        assert_eq!(sel.extract_text(parser.screen()), "a c");
    }
}
