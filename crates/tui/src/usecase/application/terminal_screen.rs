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

use usagi_core::usecase::vt_screen::{
    ActiveBuffer, Cell, CheckpointError, MouseProtocolEncoding, ScreenCheckpoint, VtScreen,
};

use super::terminal_link::scan_links;
use super::terminal_selection::TerminalPoint;

// Kept in sync with `presentation::frame::TERMINAL_CURSOR_MARKER`.  This
// use-case module deliberately does not depend on presentation, while the
// renderer consumes the marker before writing terminal output.
const TERMINAL_CURSOR_MARKER: char = '\u{e0001}';

/// Visible VT buffer lineage. A primary transcript and a full-screen alternate
/// frame have independent retained-row coordinates even when their origins are
/// numerically equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalBuffer {
    Primary,
    Alternate,
}

/// Input modes requested by the program currently drawing the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalInputModes {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub mouse_protocol: bool,
    pub mouse_encoding: MouseProtocolEncoding,
}

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

    /// Returns the DEC input modes needed to route a physical mouse wheel like
    /// a standalone terminal would.
    #[must_use]
    pub fn input_modes(&self) -> TerminalInputModes {
        TerminalInputModes {
            alternate_screen: self.screen.active_buffer() == ActiveBuffer::Alternate,
            application_cursor: self.screen.application_cursor(),
            mouse_protocol: self.screen.mouse_protocol(),
            mouse_encoding: self.screen.mouse_encoding(),
        }
    }

    /// Restores a screen from the daemon's semantic checkpoint.
    ///
    /// This is the only way an attaching pane rebuilds retained history: the
    /// daemon is the grid authority, so its checkpoint — not a raw byte tail fed
    /// to a blank parser — carries the cursor, SGR, scroll region, alternate and
    /// saved primary buffer established before the retained window.
    ///
    /// # Errors
    ///
    /// Returns the core [`CheckpointError`] when the checkpoint violates a
    /// bound; the caller keeps its current screen and requests a resync.
    pub fn from_checkpoint(checkpoint: &ScreenCheckpoint) -> Result<Self, CheckpointError> {
        VtScreen::from_checkpoint(checkpoint).map(|screen| Self { screen })
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
        self.rows_with_scrollback_window(0, usize::MAX, false)
    }

    /// Renders retained scrollback and the visible grid with the current PTY
    /// cursor as an inverted cell.
    #[must_use]
    pub fn rows_with_scrollback_and_cursor(&self) -> Vec<String> {
        self.rows_with_scrollback_window(0, usize::MAX, true)
    }

    /// Number of retained rows that have visible content, including the cursor
    /// row while the terminal is live.
    ///
    /// The fixed-height grid can end in blank padding. Walking backward finds
    /// the visible tail without projecting or scanning the preceding
    /// scrollback, so a viewport can be placed in constant time for the common
    /// case where the cursor is live.
    #[must_use]
    pub fn rows_with_scrollback_count(&self, include_cursor: bool) -> usize {
        let total = self.retained_row_count();
        let content = (0..total)
            .rev()
            .find(|row| row_has_content(self.retained_row(*row)))
            .map_or(0, |row| row + 1);
        if include_cursor {
            content.max(self.cursor_retained_row() + 1)
        } else {
            content
        }
    }

    /// Monotonic logical index of the oldest row retained by the active buffer.
    #[must_use]
    pub const fn retained_row_origin(&self) -> u64 {
        self.screen.scrollback_origin()
    }

    /// Buffer lineage for [`Self::retained_row_origin`].
    #[must_use]
    pub fn retained_buffer(&self) -> TerminalBuffer {
        match self.screen.active_buffer() {
            ActiveBuffer::Primary => TerminalBuffer::Primary,
            ActiveBuffer::Alternate => TerminalBuffer::Alternate,
        }
    }

    /// Number of retained rows needed to display both live content and a
    /// selection. A pointer may select blank fixed-grid padding below the live
    /// cursor, so the highlight extends the projected tail without exceeding
    /// the retained screen.
    #[must_use]
    pub fn rows_with_scrollback_selection_count(
        &self,
        anchor: (usize, usize),
        focus: (usize, usize),
    ) -> usize {
        self.rows_with_scrollback_count(true)
            .max(anchor.0.max(focus.0).saturating_add(1))
            .min(self.retained_row_count())
    }

    /// Renders only the requested retained-row window.
    ///
    /// Link detection expands the request to the logical lines which touch the
    /// window, because a URL may wrap across its top or bottom boundary. Rows
    /// outside those lines are neither converted to strings nor scanned. This
    /// keeps steady terminal output proportional to the visible viewport rather
    /// than the 10,000-row scrollback bound (#637).
    #[must_use]
    pub fn rows_with_scrollback_window(
        &self,
        start: usize,
        end: usize,
        include_cursor: bool,
    ) -> Vec<String> {
        let count = self.rows_with_scrollback_count(include_cursor);
        let start = start.min(count);
        let end = end.min(count);
        if start >= end {
            return Vec::new();
        }

        let (scan_start, scan_end) = self.logical_scan_range(start, end, count);
        let plain = (scan_start..scan_end)
            .map(|row| unstyled_row(self.retained_row(row)))
            .collect::<Vec<_>>();
        let links = scan_links(&plain)
            .cells
            .into_iter()
            .map(|point| TerminalPoint {
                row: point.row + scan_start,
                column: point.column,
            })
            .collect::<HashSet<_>>();
        let cursor_row = self.cursor_retained_row();
        let (_, cursor_col) = self.screen.cursor();
        let cursor_style = self.screen.cursor_style();
        (start..end)
            .map(|row| {
                let cursor = (include_cursor && row == cursor_row).then_some(cursor_col);
                render_row_selected(
                    self.retained_row(row),
                    cursor,
                    cursor_style,
                    None,
                    Some((row, &links)),
                )
            })
            .collect()
    }

    /// Renders scrollback and the visible grid with a cell-precise selection.
    #[must_use]
    pub fn rows_with_scrollback_and_cursor_selection(
        &self,
        anchor: (usize, usize),
        focus: (usize, usize),
    ) -> Vec<String> {
        self.rows_with_scrollback_window_selection(0, usize::MAX, anchor, focus)
    }

    /// Renders only the requested retained-row window with a cell-precise
    /// selection.
    ///
    /// The selection owns a complete ANSI-free snapshot for copying, but a
    /// lingering highlight must not force every redraw or scroll action to
    /// project the complete 10,000-row history. Link detection is expanded only
    /// to logical lines touching this window, matching the unselected path.
    #[must_use]
    pub fn rows_with_scrollback_window_selection(
        &self,
        start: usize,
        end: usize,
        anchor: (usize, usize),
        focus: (usize, usize),
    ) -> Vec<String> {
        let count = self.rows_with_scrollback_selection_count(anchor, focus);
        let start = start.min(count);
        let end = end.min(count);
        if start >= end {
            return Vec::new();
        }
        let (first, last) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let (scan_start, scan_end) = self.logical_scan_range(start, end, count);
        let plain = (scan_start..scan_end)
            .map(|row| unstyled_row(self.retained_row(row)))
            .collect::<Vec<_>>();
        let links = scan_links(&plain)
            .cells
            .into_iter()
            .map(|point| TerminalPoint {
                row: point.row + scan_start,
                column: point.column,
            })
            .collect::<HashSet<_>>();
        let cursor_row = self.cursor_retained_row();
        let (_, cursor_col) = self.screen.cursor();
        let cursor_style = self.screen.cursor_style();
        (start..end)
            .map(|row| {
                let cursor = (row == cursor_row).then_some(cursor_col);
                render_row_selected(
                    self.retained_row(row),
                    cursor,
                    cursor_style,
                    selection_for(row, first, last),
                    Some((row, &links)),
                )
            })
            .collect()
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

    fn retained_row_count(&self) -> usize {
        self.screen.scrollback_len() + self.screen.grid().len()
    }

    fn retained_row(&self, row: usize) -> &[Cell] {
        if row < self.screen.scrollback_len() {
            self.screen
                .scrollback_row(row)
                .expect("row is bounded by scrollback_len")
        } else {
            &self.screen.grid()[row - self.screen.scrollback_len()]
        }
    }

    fn cursor_retained_row(&self) -> usize {
        self.screen.scrollback_len() + self.screen.cursor().0
    }

    fn logical_scan_range(&self, start: usize, end: usize, count: usize) -> (usize, usize) {
        let mut scan_start = start;
        while scan_start > 0 && row_wraps(self.retained_row(scan_start - 1)) {
            scan_start -= 1;
        }
        let mut scan_end = end;
        while scan_end < count && row_wraps(self.retained_row(scan_end - 1)) {
            scan_end += 1;
        }
        (scan_start, scan_end)
    }
}

fn unstyled_row(row: &[Cell]) -> String {
    row.iter()
        .filter(|cell| !cell.continuation())
        .map(Cell::ch)
        .collect()
}

fn row_has_content(row: &[Cell]) -> bool {
    row.iter()
        .any(|cell| !cell.continuation() && cell.ch() != ' ')
}

// Keep the ambiguity of terminal_link's wrap reconstruction exactly aligned:
// a wide glyph whose continuation occupies the last cell is treated as blank
// in the expanded ANSI-free grid and therefore does not imply wrapping.
fn row_wraps(row: &[Cell]) -> bool {
    row.last()
        .is_some_and(|cell| !cell.continuation() && cell.ch() != ' ')
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
    // set; such cells render underlined to mark them clickable.
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
    fn retained_window_matches_full_projection_across_a_wrapped_link() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"prefix\r\nhttps://example.com/path\r\ntail");
        let full = screen.rows_with_scrollback_and_cursor();
        // Begin inside the wrapped URL rather than at its first physical row.
        // The window renderer must scan back to the logical-line boundary so
        // underline cells and global row coordinates match the full reference.
        let start = 2;
        let end = full.len().saturating_sub(1);
        assert_eq!(
            screen.rows_with_scrollback_window(start, end, true),
            full[start..end]
        );
        let count = screen.rows_with_scrollback_count(true);
        let (scan_start, scan_end) = screen.logical_scan_range(start, end, count);
        assert!(scan_start < start, "wrapped predecessor was not scanned");
        assert!(scan_end >= end);
    }

    #[test]
    fn long_history_window_scans_only_the_requested_unwrapped_rows() {
        let mut screen = TerminalScreen::new(2, 80);
        for row in 0..10_000 {
            screen.advance(format!("row-{row:05}\r\n").as_bytes());
        }
        let count = screen.rows_with_scrollback_count(true);
        assert!(count >= 9_999);
        let start = count.saturating_sub(24);
        let (scan_start, scan_end) = screen.logical_scan_range(start, count, count);
        assert_eq!(scan_start, start);
        assert_eq!(scan_end, count);
        assert_eq!(
            screen.rows_with_scrollback_window(start, count, true).len(),
            count - start
        );
    }

    #[test]
    fn long_history_selection_projects_only_the_requested_window() {
        let mut screen = TerminalScreen::new(2, 80);
        for row in 0..10_000 {
            screen.advance(format!("row-{row:05}\r\n").as_bytes());
        }
        let count = screen.rows_with_scrollback_count(true);
        let start = count.saturating_sub(24);
        let anchor = (start, 0);
        let focus = (count - 1, 2);
        let window = screen.rows_with_scrollback_window_selection(start, count, anchor, focus);
        assert_eq!(window.len(), count - start);
        assert!(window.iter().all(|row| row.contains("\x1b[7m")));
        assert_eq!(
            window,
            screen.rows_with_scrollback_and_cursor_selection(anchor, focus)[start..count]
        );
    }

    #[test]
    fn retained_window_is_empty_for_empty_reversed_or_out_of_bounds_ranges() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo");
        assert!(screen.rows_with_scrollback_window(1, 1, true).is_empty());
        assert!(screen.rows_with_scrollback_window(2, 1, true).is_empty());
        assert!(
            screen
                .rows_with_scrollback_window(usize::MAX, usize::MAX, true)
                .is_empty()
        );
    }

    #[test]
    fn retained_selection_window_is_empty_for_empty_reversed_or_out_of_bounds_ranges() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo");
        let anchor = (0, 0);
        let focus = (1, 2);
        assert!(
            screen
                .rows_with_scrollback_window_selection(1, 1, anchor, focus)
                .is_empty()
        );
        assert!(
            screen
                .rows_with_scrollback_window_selection(2, 1, anchor, focus)
                .is_empty()
        );
        assert!(
            screen
                .rows_with_scrollback_window_selection(usize::MAX, usize::MAX, anchor, focus,)
                .is_empty()
        );
    }

    #[test]
    fn retained_row_count_includes_a_blank_live_cursor_but_not_blank_padding() {
        let mut screen = TerminalScreen::new(4, 8);
        assert_eq!(screen.rows_with_scrollback_count(false), 0);
        assert_eq!(screen.rows_with_scrollback_count(true), 1);
        screen.advance(b"one\r\n");
        assert_eq!(screen.rows_with_scrollback_count(false), 1);
        assert_eq!(screen.rows_with_scrollback_count(true), 2);
        assert_eq!(
            screen.rows_with_scrollback_window(1, 2, true),
            vec![format!("\x1b[7m{TERMINAL_CURSOR_MARKER} \x1b[0m")]
        );
    }

    #[test]
    fn retained_origin_and_buffer_follow_the_core_screen_authority() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(screen.retained_buffer(), TerminalBuffer::Primary);
        assert_eq!(screen.retained_row_origin(), 0);
        assert_eq!(screen.screen.trim_to_cells(3 * 8), 1);
        assert_eq!(screen.retained_row_origin(), 1);

        screen.advance(b"\x1b[?1049h");
        assert_eq!(screen.retained_buffer(), TerminalBuffer::Alternate);
        assert_eq!(screen.retained_row_origin(), 0);
        screen.advance(b"\x1b[?1049l");
        assert_eq!(screen.retained_buffer(), TerminalBuffer::Primary);
        assert_eq!(screen.retained_row_origin(), 1);
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
    fn selection_extends_the_projection_into_blank_padding_below_the_cursor() {
        let screen = TerminalScreen::new(4, 6);
        let rows = screen.rows_with_scrollback_and_cursor_selection((0, 0), (2, 2));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], "\u{1b}[7m   \u{1b}[0m");
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
