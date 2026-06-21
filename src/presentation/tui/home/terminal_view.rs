//! A terminal-independent snapshot of an embedded shell's screen grid.
//!
//! The `terminal` command runs a live shell whose output is parsed into a
//! [`vt100::Screen`] (see [`crate::infrastructure::pty`]). That screen is owned
//! by a background thread and changes constantly, so the render loop snapshots
//! it into this owned, immutable [`TerminalView`] — one string per grid row plus
//! the cursor position — which the right pane then draws like any other data.
//!
//! Each row carries the shell's own colours and text attributes as embedded
//! ANSI (SGR) escape sequences, so `vim`, `ls --color`, `claude`, and the like
//! render the same in the pane as in a standalone terminal. A run of cells that
//! share a style emits one escape sequence, cells in the terminal's default
//! style emit none at all, and every styled row is reset at its end so colours
//! never bleed into the rest of the frame. The escapes have zero display width
//! ([`console::measure_text_width`] skips them), so the layout still lines up.
//!
//! Keeping the snapshot pure (no PTY, no terminal IO) makes the grid-to-text
//! conversion directly testable: a test drives a [`vt100::Parser`] with bytes
//! and asserts the resulting rows and cursor.

use std::sync::Arc;

use super::terminal_link;
use super::terminal_selection::{Cell, Selection};

/// An owned snapshot of an embedded terminal's visible screen.
///
/// The 切替 preview re-snapshots the same backgrounded session every frame and
/// caches the result; the rows are wrapped in an [`Arc`] so that cache (and any
/// other) `clone` is a refcount bump rather than a deep copy of the whole grid's
/// text — the view is immutable once built, so sharing it is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalView {
    /// One string per grid row, each laid out to the screen's column width.
    rows: Arc<Vec<String>>,
    /// The cursor's `(row, col)` position, or `None` when it is hidden.
    cursor: Option<(u16, u16)>,
}

impl TerminalView {
    /// Build a snapshot from a parsed terminal `screen`. See
    /// [`from_screen_with_selection`](Self::from_screen_with_selection); this is
    /// the no-selection, no-hover case.
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        Self::from_screen_with_selection(screen, None, None)
    }

    /// Build a snapshot from a parsed terminal `screen`: each grid row becomes a
    /// string of its cells' contents, carrying the cells' colours and text
    /// attributes as embedded ANSI escapes (blank cells render as spaces, and
    /// the trailing cell of a wide character is skipped so widths line up).
    ///
    /// Cells within `selection` are drawn inverted (their `inverse` attribute is
    /// flipped), so a mouse drag over the pane shows what it has picked out — see
    /// [`terminal_selection`](super::terminal_selection).
    ///
    /// Cells that sit on an `http(s)` URL (see [`terminal_link::link_cells`]) are
    /// drawn underlined, marking them as clickable without disturbing their own
    /// colours. The one link under the pointer — `hover`, the mouse's cell (see
    /// [`terminal_link::link_cells_at`]) — is additionally recoloured a light
    /// blue, so a link lights up as the pointer reaches it the way a browser's
    /// does.
    ///
    /// This detects the URL cells from `screen` on every call; the hot render
    /// loop instead caches them across frames and uses
    /// [`from_screen_with_links`](Self::from_screen_with_links).
    pub fn from_screen_with_selection(
        screen: &vt100::Screen,
        selection: Option<&Selection>,
        hover: Option<Cell>,
    ) -> Self {
        let links = terminal_link::link_cells(screen);
        Self::from_screen_with_links(screen, selection, hover, &links)
    }

    /// Like [`from_screen_with_selection`](Self::from_screen_with_selection),
    /// but with the screen's URL cells (`links`) supplied by the caller. The
    /// render loop computes [`terminal_link::link_cells`] only when the screen's
    /// generation changes and reuses the result on hover-only / throttled frames,
    /// so a frame that merely moved the pointer skips the O(all cells) re-scan.
    pub fn from_screen_with_links(
        screen: &vt100::Screen,
        selection: Option<&Selection>,
        hover: Option<Cell>,
        links: &std::collections::HashSet<Cell>,
    ) -> Self {
        let (rows, cols) = screen.size();
        // Just the hovered link's cells (empty when the pointer is off any link),
        // so only it is recoloured while every link stays underlined.
        let hovered = hover
            .map(|cell| terminal_link::link_cells_at(screen, cell))
            .unwrap_or_default();
        let mut out = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            // At least one byte per column; escapes grow it from there. Sizing up
            // front avoids the reallocations a default-empty `String` would make
            // while filling a full-width row.
            let mut line = String::with_capacity(cols as usize);
            // The style currently selected on `line`; we only emit a new escape
            // when a cell's style differs, and start from the terminal default.
            let mut active = CellStyle::default();
            for col in 0..cols {
                let cell = screen.cell(row, col);
                // The second half of a wide glyph is already covered by the wide
                // cell itself, so it contributes nothing here.
                if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                    continue;
                }
                let mut style = cell.map(CellStyle::of).unwrap_or_default();
                // A cell on a URL is underlined so the link reads as clickable
                // while keeping its own colour; only the link under the pointer is
                // recoloured light blue, lighting it up on hover. Applied before
                // the selection flip so a drag still inverts (and stays visible)
                // over a link.
                if links.contains(&Cell::new(row, col)) {
                    style.underline = true;
                    if hovered.contains(&Cell::new(row, col)) {
                        style.fg = LINK_COLOR;
                    }
                }
                // A selected cell is inverted so the drag is visible; flipping
                // (rather than forcing) keeps already-inverse text readable.
                if selection.is_some_and(|s| s.contains(row, col)) {
                    style.inverse = !style.inverse;
                }
                if style != active {
                    style.write_sgr(&mut line);
                    active = style;
                }
                match cell {
                    Some(cell) if cell.has_contents() => line.push_str(cell.contents()),
                    // A blank cell — or, defensively, an out-of-grid one — is a
                    // single space at the cell's (already-selected) style.
                    _ => line.push(' '),
                }
            }
            // Reset at the row's end so an open colour never leaks past it.
            if active != CellStyle::default() {
                line.push_str(SGR_RESET);
            }
            out.push(line);
        }
        let cursor = if screen.hide_cursor() {
            None
        } else {
            Some(screen.cursor_position())
        };
        Self {
            rows: Arc::new(out),
            cursor,
        }
    }

    /// The screen's rows, top to bottom.
    pub fn rows(&self) -> &[String] {
        self.rows.as_slice()
    }

    /// The cursor's `(row, col)` position, or `None` when hidden.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    /// Build a view directly from rows and a cursor, for tests of the screens
    /// that render a [`TerminalView`].
    #[cfg(test)]
    pub fn from_rows(rows: Vec<String>, cursor: Option<(u16, u16)>) -> Self {
        Self {
            rows: Arc::new(rows),
            cursor,
        }
    }
}

/// The ANSI escape that clears all colours and attributes back to default.
const SGR_RESET: &str = "\x1b[0m";

/// The colour the *hovered* link is drawn in — a light blue ("水色") which lights
/// the underlined URL under the pointer up as clickable. A 24-bit RGB value so it
/// reads the same regardless of the user's terminal palette.
const LINK_COLOR: vt100::Color = vt100::Color::Rgb(102, 178, 255);

/// The drawable style of one screen cell: its colours and text attributes,
/// distilled from a [`vt100::Cell`] into something comparable and renderable.
///
/// Consecutive cells that compare equal share a single escape sequence, and the
/// all-default style (the [`Default`](Default) value) renders to nothing — so a
/// plain shell screen stays plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    /// Read the style of a single grid `cell`.
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// Write the SGR escape selecting this style directly into `out`. It always
    /// begins by resetting (`\x1b[0`), so the sequence fully describes the style
    /// regardless of what preceded it. Writing in place avoids the two temporary
    /// `String`s a `format!`-based builder would allocate per style change.
    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.dim {
            out.push_str(";2");
        }
        if self.italic {
            out.push_str(";3");
        }
        if self.underline {
            out.push_str(";4");
        }
        if self.inverse {
            out.push_str(";7");
        }
        push_color(out, self.fg, false);
        push_color(out, self.bg, true);
        out.push('m');
    }
}

/// Append the SGR parameters for a foreground (`background = false`) or
/// background (`background = true`) `color`. The default colour adds nothing,
/// since the leading reset already restores it.
fn push_color(params: &mut String, color: vt100::Color, background: bool) {
    use std::fmt::Write as _;
    match color {
        vt100::Color::Default => {}
        // The 16 named colours have compact codes (30–37 / 90–97, +10 for the
        // background); everything else uses the 256-colour selector.
        vt100::Color::Idx(n @ 0..=7) => {
            let base = if background { 40 } else { 30 };
            let _ = write!(params, ";{}", base + n as u16);
        }
        vt100::Color::Idx(n @ 8..=15) => {
            let base = if background { 100 } else { 90 };
            let _ = write!(params, ";{}", base + (n as u16 - 8));
        }
        vt100::Color::Idx(n) => {
            let selector = if background { 48 } else { 38 };
            let _ = write!(params, ";{selector};5;{n}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let selector = if background { 48 } else { 38 };
            let _ = write!(params, ";{selector};2;{r};{g};{b}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::home::terminal_selection::Cell;

    /// A parser sized `rows`×`cols`, fed `bytes`, returned for inspection.
    fn parsed(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn from_screen_with_selection_inverts_selected_cells() {
        // Select the first two columns of "abcd"; they pick up the inverse (7)
        // attribute while the rest of the row stays plain.
        let parser = parsed(1, 4, b"abcd");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 1));
        let view = TerminalView::from_screen_with_selection(parser.screen(), Some(&sel), None);
        let row = &view.rows()[0];
        // The selected run opens with an inverse escape and the cells after it
        // reset back to plain before the row ends.
        assert!(row.contains("\x1b[0;7mab"));
        assert!(row.contains("\x1b[0mcd"));
    }

    #[test]
    fn from_screen_with_selection_flips_already_inverse_text() {
        // Text drawn with SGR 7 (inverse) becomes non-inverse where selected, so
        // the highlight is still visible against it.
        let parser = parsed(1, 2, b"\x1b[7mAB");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 0));
        let view = TerminalView::from_screen_with_selection(parser.screen(), Some(&sel), None);
        let row = &view.rows()[0];
        // Column 0 (selected) drops the inverse, so it renders plain (no escape);
        // column 1 keeps inverse (SGR 7), and the row resets at its end.
        assert_eq!(row, "A\x1b[0;7mB\x1b[0m");
    }

    #[test]
    fn from_screen_underlines_a_link_without_recolouring_it() {
        // With no hover, the URL cells are only underlined (SGR 4) and keep their
        // own colour — no light-blue override — so an idle screen stays readable.
        let parser = parsed(1, 30, b"x https://example.com y");
        let view = TerminalView::from_screen(parser.screen());
        let row = &view.rows()[0];
        // Underlined, but with no foreground colour (no `38;2;...`).
        assert!(row.contains("\x1b[0;4mhttps://example.com"));
        assert!(!row.contains("38;2;102;178;255"));
        // The leading "x " has no escape before it, and the link resets before
        // the trailing " y" so the underline never bleeds onto it.
        assert!(row.starts_with("x \x1b[0;4m"));
        assert!(row.contains("\x1b[0m y"));
    }

    #[test]
    fn from_screen_recolours_only_the_hovered_link() {
        // Hovering the link recolours it light blue (RGB 102;178;255) on top of
        // the underline, lighting it up as clickable under the pointer.
        let parser = parsed(1, 30, b"x https://example.com y");
        let hover = Some(Cell::new(0, 5)); // somewhere on the URL
        let view = TerminalView::from_screen_with_selection(parser.screen(), None, hover);
        let row = &view.rows()[0];
        assert!(row.contains("\x1b[0;4;38;2;102;178;255mhttps://example.com"));
        assert!(row.contains("\x1b[0m y"));
    }

    #[test]
    fn from_screen_leaves_an_unhovered_link_uncoloured() {
        // Hovering off the link (on the trailing blank padding) underlines it but
        // adds no colour.
        let parser = parsed(1, 30, b"x https://example.com y");
        let hover = Some(Cell::new(0, 29));
        let view = TerminalView::from_screen_with_selection(parser.screen(), None, hover);
        let row = &view.rows()[0];
        assert!(row.contains("\x1b[0;4mhttps://example.com"));
        assert!(!row.contains("38;2;102;178;255"));
    }

    #[test]
    fn from_screen_inverts_a_selected_hovered_link_on_top_of_its_style() {
        // A drag over the hovered link keeps the underline + link colour and adds
        // the inverse (7) so the selection is still visible over it.
        let parser = parsed(1, 20, b"https://x.io");
        let mut sel = Selection::new(Cell::new(0, 0));
        sel.extend(Cell::new(0, 0));
        let hover = Some(Cell::new(0, 0));
        let view = TerminalView::from_screen_with_selection(parser.screen(), Some(&sel), hover);
        let row = &view.rows()[0];
        // Column 0 ("h") is a link cell, hovered, and selected: underline +
        // inverse + link colour.
        assert!(row.starts_with("\x1b[0;4;7;38;2;102;178;255mh"));
    }

    #[test]
    fn from_screen_with_links_matches_self_detecting_path() {
        // Supplying the precomputed link cells (as the cached render loop does)
        // yields exactly what `from_screen_with_selection` produces by detecting
        // them itself, so the cache cannot drift from the on-the-fly path.
        let parser = parsed(1, 30, b"x https://example.com y");
        let screen = parser.screen();
        let hover = Some(Cell::new(0, 5));
        let links = terminal_link::link_cells(screen);
        let cached = TerminalView::from_screen_with_links(screen, None, hover, &links);
        let detected = TerminalView::from_screen_with_selection(screen, None, hover);
        assert_eq!(cached, detected);
    }

    #[test]
    fn from_screen_without_selection_matches_the_plain_snapshot() {
        // Passing no selection is identical to `from_screen`.
        let parser = parsed(1, 4, b"abcd");
        let plain = TerminalView::from_screen(parser.screen());
        let none = TerminalView::from_screen_with_selection(parser.screen(), None, None);
        assert_eq!(plain, none);
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

    #[test]
    fn from_screen_leaves_default_styled_text_plain() {
        // No colours or attributes: the row carries no escape sequences at all.
        let parser = parsed(1, 4, b"hi");
        let view = TerminalView::from_screen(parser.screen());
        assert_eq!(view.rows()[0], "hi  ");
        assert!(!view.rows()[0].contains('\x1b'));
    }

    #[test]
    fn from_screen_carries_a_named_foreground_colour() {
        // CSI 31m selects red (palette index 1) → SGR `31`. The trailing blank
        // cells revert to the default, so the colour is reset before the row's
        // end and never leaks past it.
        let parser = parsed(1, 4, b"\x1b[31mhi");
        let view = TerminalView::from_screen(parser.screen());
        let row = &view.rows()[0];
        assert!(row.contains("\x1b[0;31mhi"));
        assert!(row.contains(SGR_RESET));
        // The escapes have no display width, so the row stays four columns wide.
        assert_eq!(console::measure_text_width(row), 4);
    }

    #[test]
    fn from_screen_resets_a_row_coloured_to_its_final_cell() {
        // The whole 2-column row is red, so the style is still active at the end
        // and the row closes with an explicit reset.
        let parser = parsed(1, 2, b"\x1b[31mab");
        let view = TerminalView::from_screen(parser.screen());
        assert!(view.rows()[0].ends_with(SGR_RESET));
    }

    #[test]
    fn from_screen_carries_text_attributes_and_background() {
        // Bold + underline on a green background.
        let parser = parsed(1, 2, b"\x1b[1;4;42mX");
        let view = TerminalView::from_screen(parser.screen());
        let row = &view.rows()[0];
        assert!(row.contains("\x1b[0;1;4;42m"));
        assert!(row.contains('X'));
    }

    #[test]
    fn from_screen_carries_bright_dim_italic_inverse() {
        // Dim + italic + inverse with a bright-cyan foreground (index 14 → 96).
        let parser = parsed(1, 2, b"\x1b[2;3;7;96mZ");
        let view = TerminalView::from_screen(parser.screen());
        assert!(view.rows()[0].contains("\x1b[0;2;3;7;96m"));
    }

    #[test]
    fn from_screen_carries_256_and_rgb_colours() {
        // A 256-palette foreground and a 24-bit RGB background.
        let parser = parsed(1, 2, b"\x1b[38;5;200;48;2;10;20;30mQ");
        let view = TerminalView::from_screen(parser.screen());
        assert!(view.rows()[0].contains("\x1b[0;38;5;200;48;2;10;20;30m"));
    }

    #[test]
    fn from_screen_resets_when_style_returns_to_default() {
        // "a" is red, then SGR 0 clears it before "b": the reset appears inline.
        let parser = parsed(1, 4, b"\x1b[31ma\x1b[0mb");
        let view = TerminalView::from_screen(parser.screen());
        let row = &view.rows()[0];
        // Red is selected, then cleared back to the default mid-row.
        assert!(row.contains("\x1b[0;31ma\x1b[0m"));
        assert!(row.contains('b'));
    }
}
