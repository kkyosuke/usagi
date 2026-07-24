//! Rendering wrapper around the shared core VT parser.
//!
//! The VT state model, parser and resize live in [`usagi_core`]'s
//! [`VtScreen`](usagi_core::usecase::vt_screen::VtScreen): it is the single
//! parser authority the daemon and TUI share. This module keeps only the
//! **presentation** half — projecting the core screen's read-only cell API into
//! rendered rows with ANSI styling, an inverted cursor marker, cell-precise
//! selection highlight, and clickable-link underlines. Those depend on
//! presentation vocabulary (the `\u{e0001}` cursor marker, reverse-video and
//! underline escapes) and so never leak into core.
//!
//! [`TerminalScreen`] forwards feeding and resizing to the core screen and adds
//! the row projections the Home right pane renders.

use std::collections::HashSet;

use usagi_core::usecase::vt_screen::{Cell, VtScreen};

use super::terminal_link::scan_links;
use super::terminal_selection::TerminalPoint;

// Kept in sync with `presentation::frame::TERMINAL_CURSOR_MARKER`.  This
// use-case module deliberately does not depend on presentation, while the
// renderer consumes the marker before writing terminal output.
const TERMINAL_CURSOR_MARKER: char = '\u{e0001}';

/// Renders the shared core VT screen into the rows the pane draws.
///
/// The grid, scrollback, cursor, SGR and alternate/saved buffer state are owned
/// by the wrapped [`VtScreen`]; this type only projects them for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalScreen {
    screen: VtScreen,
}

impl TerminalScreen {
    /// Creates a blank screen at `rows × cols` (each clamped to at least one).
    #[must_use]
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            screen: VtScreen::new(rows, cols),
        }
    }

    /// Feeds a chunk of raw PTY output into the shared parser.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.screen.advance(bytes);
    }

    /// Changes the visible geometry without replaying historical control bytes.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.screen.resize(rows, cols);
    }

    /// Renders the visible grid as one `String` per row with trailing blanks
    /// trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.screen
            .grid()
            .iter()
            .map(|row| render_row(row, None, ""))
            .collect()
    }

    /// Renders retained scrollback followed by the visible terminal grid.
    #[must_use]
    pub fn rows_with_scrollback(&self) -> Vec<String> {
        let links = self.link_cells();
        let scrollback_len = self.screen.scrollback().len();
        let mut rows: Vec<_> = self
            .screen
            .scrollback()
            .iter()
            .enumerate()
            .map(|(row, cells)| render_row_selected(cells, None, "", None, Some((row, &links))))
            .chain(self.screen.grid().iter().enumerate().map(|(index, cells)| {
                render_row_selected(
                    cells,
                    None,
                    "",
                    None,
                    Some((scrollback_len + index, &links)),
                )
            }))
            .collect();
        // The visible grid is fixed-height, but its unused tail is not terminal
        // content. Dropping it lets the live viewport stay anchored to the last
        // meaningful output instead of a screenful of padding.
        while matches!(rows.last(), Some(last) if last.is_empty()) {
            rows.pop();
        }
        rows
    }

    /// Grid cells — retained scrollback then the visible grid, joined untrimmed so
    /// their row indices match the render iteration above — that sit on an
    /// `http(s)` URL. Rendering underlines these to mark links clickable (#389);
    /// detection is the pure #387 core over the ANSI-free grid.
    fn link_cells(&self) -> HashSet<TerminalPoint> {
        let viewport: Vec<String> = self
            .screen
            .scrollback()
            .iter()
            .chain(self.screen.grid())
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.continuation())
                    .map(Cell::ch)
                    .collect()
            })
            .collect();
        scan_links(&viewport).cells
    }

    /// Renders retained scrollback and the visible grid with the current PTY
    /// cursor as an inverted cell.
    #[must_use]
    pub fn rows_with_scrollback_and_cursor(&self) -> Vec<String> {
        let links = self.link_cells();
        let scrollback_len = self.screen.scrollback().len();
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cursor_style = self.screen.cursor_style();
        let mut rows: Vec<_> = self
            .screen
            .scrollback()
            .iter()
            .enumerate()
            .map(|(row, cells)| render_row_selected(cells, None, "", None, Some((row, &links))))
            .chain(self.screen.grid().iter().enumerate().map(|(index, cells)| {
                let cursor = (index == cursor_row).then_some(cursor_col);
                render_row_selected(
                    cells,
                    cursor,
                    cursor_style,
                    None,
                    Some((scrollback_len + index, &links)),
                )
            }))
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// Renders scrollback and the visible grid with a cell-precise selection.
    #[must_use]
    pub fn rows_with_scrollback_and_cursor_selection(
        &self,
        anchor: (usize, usize),
        focus: (usize, usize),
    ) -> Vec<String> {
        let (first, last) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let links = self.link_cells();
        let scrollback_len = self.screen.scrollback().len();
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cursor_style = self.screen.cursor_style();
        let mut rows: Vec<_> = self
            .screen
            .scrollback()
            .iter()
            .enumerate()
            .map(|(row, cells)| {
                render_row_selected(
                    cells,
                    None,
                    "",
                    selection_for(row, first, last),
                    Some((row, &links)),
                )
            })
            .chain(self.screen.grid().iter().enumerate().map(|(index, cells)| {
                let row = scrollback_len + index;
                let cursor = (index == cursor_row).then_some(cursor_col);
                render_row_selected(
                    cells,
                    cursor,
                    cursor_style,
                    selection_for(row, first, last),
                    Some((row, &links)),
                )
            }))
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// Renders the visible grid with the current PTY cursor as an inverted cell.
    #[must_use]
    pub fn rows_with_cursor(&self) -> Vec<String> {
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cursor_style = self.screen.cursor_style();
        self.screen
            .grid()
            .iter()
            .enumerate()
            .map(|(row, cells)| {
                let cursor = (row == cursor_row).then_some(cursor_col);
                render_row(cells, cursor, cursor_style)
            })
            .collect()
    }

    /// Returns the complete visible grid untrimmed (keeps trailing spaces for
    /// copy) and free of ANSI styling.
    #[must_use]
    pub fn cells(&self) -> Vec<String> {
        self.screen.cells()
    }

    /// Returns retained scrollback followed by the complete visible grid,
    /// untrimmed within each row (keeps trailing spaces for copy) and free of
    /// ANSI styling.
    #[must_use]
    pub fn cells_with_scrollback(&self) -> Vec<String> {
        self.screen.cells_with_scrollback()
    }
}

fn render_row(row: &[Cell], cursor: Option<usize>, cursor_style: &str) -> String {
    render_row_selected(row, cursor, cursor_style, None, None)
}

fn selection_for(
    row: usize,
    first: (usize, usize),
    last: (usize, usize),
) -> Option<(usize, usize)> {
    (first.0..=last.0).contains(&row).then_some((
        if row == first.0 { first.1 } else { 0 },
        if row == last.0 { last.1 } else { usize::MAX },
    ))
}

fn render_row_selected(
    row: &[Cell],
    cursor: Option<usize>,
    cursor_style: &str,
    selection: Option<(usize, usize)>,
    links: Option<(usize, &HashSet<TerminalPoint>)>,
) -> String {
    // A cell sits on a detected link when its (row, column) is in the scanned
    // set; such cells render underlined to mark them clickable (#389).
    let is_link = |column: usize| {
        links.is_some_and(|(row, set)| set.contains(&TerminalPoint { row, column }))
    };
    let cursor = cursor.filter(|column| *column < row.len());
    // A selection extends the rendered extent past the row's trailing blanks so
    // selected padding — and fully blank lines that fall inside a multi-row
    // selection — are highlighted instead of being trimmed away. Without this,
    // dragging across the space-padded, mostly-blank screens agents draw leaves
    // the selection invisible even though copy still captures the cells. `end`
    // is `usize::MAX` for a non-final selected row, so clamp it to the last real
    // column and never past the grid width.
    let selection_last =
        selection.and_then(|(_, end)| row.len().checked_sub(1).map(|last| end.min(last)));
    let last = row
        .iter()
        .rposition(|cell| cell.ch() != ' ' && !cell.continuation())
        .into_iter()
        .chain(cursor)
        .chain(selection_last)
        .max();
    let Some(last) = last else {
        return String::new();
    };
    let mut rendered = String::new();
    let mut active = String::new();
    for (column, cell) in row[..=last].iter().enumerate() {
        if cell.continuation() {
            continue;
        }
        let width = if row.get(column + 1).is_some_and(Cell::continuation) {
            2
        } else {
            1
        };
        let selected = selection
            .is_some_and(|(start, end)| column <= end && column.saturating_add(width) > start);
        let mut style = if cursor == Some(column) {
            let base = if cell.style().is_empty() {
                cursor_style
            } else {
                cell.style()
            };
            format!("{base}\u{1b}[7m")
        } else {
            cell.style().to_owned()
        };
        if selected {
            style.push_str("\u{1b}[7m");
        }
        if is_link(column) {
            style.push_str("\u{1b}[4m");
        }
        if style != active {
            if !active.is_empty() {
                rendered.push_str("\u{1b}[0m");
            }
            rendered.push_str(&style);
            active = style;
        }
        if cursor == Some(column) {
            rendered.push(TERMINAL_CURSOR_MARKER);
        }
        rendered.push(cell.ch());
    }
    if !active.is_empty() {
        rendered.push_str("\u{1b}[0m");
    }
    rendered
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;

    #[test]
    fn plain_rows_trim_trailing_blanks_and_keep_blank_rows() {
        let mut screen = TerminalScreen::new(2, 10);
        screen.advance(b"hello");
        assert_eq!(screen.rows(), vec!["hello", ""]);
    }

    #[test]
    fn resize_reprojects_the_clipped_grid_and_scrollback() {
        let mut screen = TerminalScreen::new(2, 10);
        screen.advance(b"first-row\r\nsecond-row\r\nthird-row");
        screen.resize(2, 5);
        assert_eq!(
            screen.rows_with_scrollback(),
            vec!["first", "secon", "third"]
        );
    }

    #[test]
    fn cells_keep_trailing_spaces_of_the_visible_grid() {
        let mut screen = TerminalScreen::new(1, 5);
        screen.advance(b"a b");
        assert_eq!(screen.cells(), vec!["a b  "]);
    }

    #[test]
    fn cells_with_scrollback_keeps_trailing_spaces_without_styling() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo\r\nthree");
        assert_eq!(
            screen.cells_with_scrollback(),
            vec!["one     ", "two     ", "three   "]
        );
    }

    #[test]
    fn sgr_colors_and_attributes_are_preserved_in_rendered_rows() {
        let mut plain = TerminalScreen::new(1, 10);
        plain.advance(b"\x1b[31mred\x1b[0m");
        assert_eq!(plain.rows(), vec!["\x1b[31mred\x1b[0m"]);
        let mut compound = TerminalScreen::new(1, 10);
        compound.advance(b"\x1b[1;38;5;208mhi\x1b[0mok");
        assert_eq!(compound.rows(), vec!["\x1b[1;38;5;208mhi\x1b[0mok"]);
    }

    #[test]
    fn cursor_is_visible_at_the_input_position_without_losing_cell_style() {
        let mut screen = TerminalScreen::new(1, 8);
        screen.advance(b"\x1b[32mgo");
        assert_eq!(
            screen.rows_with_cursor(),
            vec![format!(
                "\x1b[32mgo\x1b[0m\x1b[32m\x1b[7m{TERMINAL_CURSOR_MARKER} \x1b[0m"
            )]
        );
        screen.advance(b"\r");
        assert_eq!(
            screen.rows_with_cursor(),
            vec![format!(
                "\x1b[32m\x1b[7m{TERMINAL_CURSOR_MARKER}g\x1b[0m\x1b[32mo\x1b[0m"
            )]
        );
    }

    #[test]
    fn scrollback_and_cursor_render_history_then_the_inverted_cursor_cell() {
        let mut screen = TerminalScreen::new(2, 4);
        screen.advance(b"ab\r\ncd\r\nef");
        assert_eq!(
            screen.rows_with_scrollback_and_cursor(),
            vec![
                "ab".to_owned(),
                "cd".to_owned(),
                format!("ef\x1b[7m{TERMINAL_CURSOR_MARKER} \x1b[0m"),
            ]
        );
    }

    #[test]
    fn wide_characters_selection_marks_their_cell() {
        let mut screen = TerminalScreen::new(2, 4);
        screen.advance("AあB".as_bytes());
        assert_eq!(
            screen.rows_with_scrollback_and_cursor_selection((0, 1), (0, 2)),
            vec!["A\u{1b}[7mあ\u{1b}[0mB"]
        );
    }

    #[test]
    fn detected_links_render_underlined_and_compose_with_selection() {
        let mut screen = TerminalScreen::new(2, 20);
        screen.advance(b"see https://a.io");
        // The URL cells (cols 4..=15) are underlined so the link reads as
        // clickable; the surrounding "see " prose carries no styling. The blank
        // second row is trimmed from the projection.
        assert_eq!(
            screen.rows_with_scrollback(),
            vec!["see \u{1b}[4mhttps://a.io\u{1b}[0m"]
        );
        // Selecting the first URL cell keeps the underline and adds the selection
        // inverse on that cell, so the two affordances coexist. The live cursor
        // (col 16, just past the text) still renders as its reverse-video cell.
        assert_eq!(
            screen.rows_with_scrollback_and_cursor_selection((0, 4), (0, 4)),
            vec![
                "see \u{1b}[7m\u{1b}[4mh\u{1b}[0m\u{1b}[4mttps://a.io\u{1b}[0m\u{1b}[7m\u{e0001} \u{1b}[0m"
            ]
        );
    }

    #[test]
    fn selection_highlights_trailing_padding_and_blank_lines_inside_the_range() {
        // Row 0 has text padded by blanks, row 1 is fully blank, row 2 has text:
        // the shape agents draw. A block drag over all three must stay visible.
        let mut screen = TerminalScreen::new(3, 6);
        screen.advance(b"ab\r\n\r\ncd");
        assert_eq!(screen.rows(), vec!["ab", "", "cd"]);

        // Select trailing padding only (cols 2..=4 on row 0, past "ab"). The
        // selected blanks are rendered as reverse-video spaces so the drag is
        // visible even though it covers no glyphs.
        let trailing = screen.rows_with_scrollback_and_cursor_selection((0, 2), (0, 4));
        assert_eq!(trailing[0], "ab\u{1b}[7m   \u{1b}[0m");

        // A block selection spanning the blank middle row highlights every
        // in-range column of that row instead of collapsing it to "".
        let block = screen.rows_with_scrollback_and_cursor_selection((0, 0), (1, 5));
        assert_eq!(block[0], "\u{1b}[7mab    \u{1b}[0m");
        assert_eq!(block[1], "\u{1b}[7m      \u{1b}[0m");
    }

    #[test]
    fn reverse_order_selection_is_normalized_before_rendering() {
        // Anchor after focus selects the same span as the forward drag.
        let mut screen = TerminalScreen::new(1, 6);
        screen.advance(b"abcdef");
        assert_eq!(
            screen.rows_with_scrollback_and_cursor_selection((0, 2), (0, 0)),
            screen.rows_with_scrollback_and_cursor_selection((0, 0), (0, 2)),
        );
    }

    #[test]
    fn selection_spanning_scrollback_highlights_history_rows() {
        // A screen that scrolled retains "ab" in scrollback while "cd"/"ef" stay
        // visible. Selecting across all three exercises the scrollback render
        // branch, which the single-row selection tests never reach.
        let mut screen = TerminalScreen::new(2, 4);
        screen.advance(b"ab\r\ncd\r\nef");
        let rows = screen.rows_with_scrollback_and_cursor_selection((0, 0), (2, 3));
        assert_eq!(rows.len(), 3);
        // The scrolled-off history row renders with the selection highlight.
        assert_eq!(rows[0], "\u{1b}[7mab  \u{1b}[0m");
    }

    #[test]
    fn selection_ending_within_content_does_not_add_trailing_highlight() {
        // Regression guard: a selection that stops inside the text must not
        // extend reverse-video into the trailing padding.
        let mut screen = TerminalScreen::new(1, 6);
        screen.advance(b"abcdef");
        assert_eq!(
            screen.rows_with_scrollback_and_cursor_selection((0, 0), (0, 2)),
            vec!["\u{1b}[7mabc\u{1b}[0mdef"]
        );
    }
}
