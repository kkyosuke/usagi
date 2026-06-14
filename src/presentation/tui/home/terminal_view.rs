//! A plain, terminal-independent snapshot of an embedded shell's screen grid.
//!
//! The `terminal` command runs a live shell whose output is parsed into a
//! [`vt100::Screen`] (see [`crate::infrastructure::pty`]). That screen is owned
//! by a background thread and changes constantly, so the render loop snapshots
//! it into this owned, immutable [`TerminalView`] — one string per grid row plus
//! the cursor position — which the right pane then draws like any other data.
//!
//! Keeping the snapshot pure (no PTY, no terminal IO) makes the grid-to-text
//! conversion directly testable: a test drives a [`vt100::Parser`] with bytes
//! and asserts the resulting rows and cursor.

/// An owned snapshot of an embedded terminal's visible screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalView {
    /// One string per grid row, each laid out to the screen's column width.
    rows: Vec<String>,
    /// The cursor's `(row, col)` position, or `None` when it is hidden.
    cursor: Option<(u16, u16)>,
}

impl TerminalView {
    /// Build a snapshot from a parsed terminal `screen`: each grid row becomes a
    /// string of its cells' contents (blank cells render as spaces, and the
    /// trailing cell of a wide character is skipped so widths line up).
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        let mut out = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                match screen.cell(row, col) {
                    // The second half of a wide glyph is already covered by the
                    // wide cell itself, so it contributes nothing here.
                    Some(cell) if cell.is_wide_continuation() => {}
                    Some(cell) if cell.has_contents() => line.push_str(cell.contents()),
                    _ => line.push(' '),
                }
            }
            out.push(line);
        }
        let cursor = if screen.hide_cursor() {
            None
        } else {
            Some(screen.cursor_position())
        };
        Self { rows: out, cursor }
    }

    /// The screen's rows, top to bottom.
    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// The cursor's `(row, col)` position, or `None` when hidden.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    /// Build a view directly from rows and a cursor, for tests of the screens
    /// that render a [`TerminalView`].
    #[cfg(test)]
    pub fn from_rows(rows: Vec<String>, cursor: Option<(u16, u16)>) -> Self {
        Self { rows, cursor }
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
    fn from_screen_lays_out_text_and_pads_blank_cells() {
        let parser = parsed(2, 6, b"hi");
        let view = TerminalView::from_screen(parser.screen());
        assert_eq!(view.rows().len(), 2);
        // "hi" then blanks to the column width; the empty row is all spaces.
        assert_eq!(view.rows()[0], "hi    ");
        assert_eq!(view.rows()[1], "      ");
    }

    #[test]
    fn from_screen_reports_the_cursor_position() {
        // After writing "hi" the cursor sits on row 0, column 2.
        let parser = parsed(2, 6, b"hi");
        let view = TerminalView::from_screen(parser.screen());
        assert_eq!(view.cursor(), Some((0, 2)));
    }

    #[test]
    fn from_screen_omits_a_hidden_cursor() {
        // CSI ?25l hides the cursor.
        let parser = parsed(1, 4, b"\x1b[?25l");
        let view = TerminalView::from_screen(parser.screen());
        assert_eq!(view.cursor(), None);
    }

    #[test]
    fn from_screen_keeps_wide_glyphs_to_their_width() {
        // A full-width character occupies two columns; its continuation cell is
        // skipped, so the row stays exactly `cols` display columns wide.
        let parser = parsed(1, 4, "あ".as_bytes());
        let view = TerminalView::from_screen(parser.screen());
        assert_eq!(view.rows()[0], "あ  ");
        assert_eq!(console::measure_text_width(&view.rows()[0]), 4);
    }

    #[test]
    fn from_rows_builds_a_view_for_rendering_tests() {
        let view = TerminalView::from_rows(vec!["$ ls".to_string()], Some((0, 4)));
        assert_eq!(view.rows(), ["$ ls"]);
        assert_eq!(view.cursor(), Some((0, 4)));
    }
}
