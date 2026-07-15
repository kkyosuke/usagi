//! A minimal terminal screen grid.
//!
//! The daemon owns the PTY and streams **raw** output bytes; this type turns
//! that byte stream into a fixed `rows × cols` character grid the Home right
//! pane can render.  It is a deliberately small VT interpreter: it covers what a
//! login shell prompt and everyday commands such as `ls` emit — printable text,
//! `CR` / `LF` / `BS` / `HT`, line wrap and scroll, cursor moves, line/display
//! erase — and silently ignores styling (SGR) and window-title (OSC) sequences.
//! It is pure and holds no IO, so it is exercised entirely by unit tests.
//!
//! Out of scope on purpose: double-width (CJK) cells are stored as a single
//! grid column, alternate screen buffers, and scrollback beyond the viewport.

/// Escape-sequence parser position.  Only these five states are reachable; any
/// byte that does not belong to the active state returns the parser to
/// [`Phase::Ground`] without emitting output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Printable text and C0 control bytes are interpreted directly.
    Ground,
    /// The previous byte was `ESC`; the next byte selects the sequence kind.
    Escape,
    /// Collecting a `CSI` (`ESC [`) parameter/intermediate run until its final.
    Csi,
    /// Swallowing an `OSC` (`ESC ]`) string until `BEL` or `ESC`.
    Osc,
    /// Swallowing the single byte that follows a charset-select (`ESC (`/`)`).
    Charset,
}

/// A fixed-size character grid updated from a raw terminal byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalScreen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    cursor_row: usize,
    cursor_col: usize,
    phase: Phase,
    /// Collected `CSI` parameter/intermediate bytes (without the leading `ESC [`).
    params: String,
    /// Partially received UTF-8 bytes awaiting their continuation bytes.
    utf8_pending: Vec<u8>,
    /// The total length of the multibyte sequence currently being assembled.
    utf8_needed: usize,
}

impl TerminalScreen {
    /// Creates a blank screen.  `rows` and `cols` are clamped to at least one so
    /// the grid always has a valid cursor cell.
    #[must_use]
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            grid: vec![vec![' '; cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            phase: Phase::Ground,
            params: String::new(),
            utf8_pending: Vec::new(),
            utf8_needed: 0,
        }
    }

    /// Applies a chunk of raw PTY output.  Chunks may split a multibyte
    /// character; the trailing bytes are buffered until the next call.
    pub fn advance(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed(byte);
        }
    }

    /// Renders the grid as one `String` per row with trailing blanks trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| {
                let mut line: String = row.iter().collect();
                let trimmed = line.trim_end().len();
                line.truncate(trimmed);
                line
            })
            .collect()
    }

    /// The zero-based cursor position, clamped inside the grid.
    #[must_use]
    pub const fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    fn feed(&mut self, byte: u8) {
        match self.phase {
            Phase::Ground => self.ground(byte),
            Phase::Escape => self.escape(byte),
            Phase::Csi => self.csi(byte),
            Phase::Osc => self.osc(byte),
            Phase::Charset => self.phase = Phase::Ground,
        }
    }

    fn ground(&mut self, byte: u8) {
        if !self.utf8_pending.is_empty() {
            if byte & 0xC0 == 0x80 {
                self.utf8_pending.push(byte);
                if self.utf8_pending.len() >= self.utf8_needed {
                    self.flush_utf8();
                }
                return;
            }
            // An unexpected byte aborts the incomplete sequence; the byte is then
            // interpreted fresh below rather than being lost.
            self.utf8_pending.clear();
            self.utf8_needed = 0;
        }
        match byte {
            0x1b => self.phase = Phase::Escape,
            b'\r' => self.cursor_col = 0,
            b'\n' => self.line_feed(),
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            b'\t' => self.tab(),
            0x20..=0x7e => self.print(byte as char),
            // BEL, DEL and other C0 controls have no grid effect here.
            0x00..=0x1f | 0x7f => {}
            _ => {
                let needed = utf8_len(byte);
                if needed > 1 {
                    self.utf8_needed = needed;
                    self.utf8_pending.push(byte);
                }
                // A stray continuation/invalid lead byte is dropped.
            }
        }
    }

    fn flush_utf8(&mut self) {
        if let Ok(text) = std::str::from_utf8(&self.utf8_pending)
            && let Some(ch) = text.chars().next()
        {
            self.print(ch);
        }
        self.utf8_pending.clear();
        self.utf8_needed = 0;
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.params.clear();
                self.phase = Phase::Csi;
            }
            b']' => self.phase = Phase::Osc,
            b'(' | b')' => self.phase = Phase::Charset,
            // Single-byte escapes (e.g. `ESC =`, `ESC c`) are ignored.
            _ => self.phase = Phase::Ground,
        }
    }

    fn csi(&mut self, byte: u8) {
        match byte {
            0x20..=0x3f => self.params.push(byte as char),
            0x40..=0x7e => {
                self.dispatch_csi(byte as char);
                self.phase = Phase::Ground;
            }
            _ => self.phase = Phase::Ground,
        }
    }

    fn osc(&mut self, byte: u8) {
        // Terminated by BEL, or by `ESC` (a lone ESC or the start of the `ESC \`
        // string terminator).  Routing ESC back through the escape parser lets
        // the trailing `\` be swallowed instead of printed.
        if byte == 0x07 {
            self.phase = Phase::Ground;
        } else if byte == 0x1b {
            self.phase = Phase::Escape;
        }
    }

    fn dispatch_csi(&mut self, final_byte: char) {
        match final_byte {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(self.param(0, 1)),
            'B' => self.cursor_row = (self.cursor_row + self.param(0, 1)).min(self.rows - 1),
            'C' => self.cursor_col = (self.cursor_col + self.param(0, 1)).min(self.cols - 1),
            'D' => self.cursor_col = self.cursor_col.saturating_sub(self.param(0, 1)),
            'G' => self.cursor_col = self.param(0, 1).saturating_sub(1).min(self.cols - 1),
            'd' => self.cursor_row = self.param(0, 1).saturating_sub(1).min(self.rows - 1),
            'H' | 'f' => {
                self.cursor_row = self.param(0, 1).saturating_sub(1).min(self.rows - 1);
                self.cursor_col = self.param(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            'K' => self.erase_line(),
            'J' => self.erase_display(),
            // Styling (`m`) and unhandled finals leave the grid unchanged.
            _ => {}
        }
    }

    fn erase_line(&mut self) {
        let row = self.cursor_row;
        let (start, end) = match self.param(0, 0) {
            1 => (0, self.cursor_col + 1),
            2 => (0, self.cols),
            _ => (self.cursor_col, self.cols),
        };
        for col in start..end.min(self.cols) {
            self.grid[row][col] = ' ';
        }
    }

    fn erase_display(&mut self) {
        match self.param(0, 0) {
            1 => {
                for row in 0..self.cursor_row {
                    self.blank_row(row);
                }
                for col in 0..=self.cursor_col.min(self.cols - 1) {
                    self.grid[self.cursor_row][col] = ' ';
                }
            }
            2 => {
                for row in 0..self.rows {
                    self.blank_row(row);
                }
            }
            _ => {
                for col in self.cursor_col..self.cols {
                    self.grid[self.cursor_row][col] = ' ';
                }
                for row in (self.cursor_row + 1)..self.rows {
                    self.blank_row(row);
                }
            }
        }
    }

    fn blank_row(&mut self, row: usize) {
        self.grid[row].fill(' ');
    }

    fn print(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.line_feed();
        }
        self.grid[self.cursor_row][self.cursor_col] = ch;
        self.cursor_col += 1;
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 >= self.rows {
            self.grid.remove(0);
            self.grid.push(vec![' '; self.cols]);
        } else {
            self.cursor_row += 1;
        }
    }

    fn tab(&mut self) {
        let next = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next.min(self.cols - 1);
    }

    /// Reads the `idx`-th `;`-separated numeric CSI parameter, or `default` when
    /// it is absent or not a number (e.g. a private `?` marker).
    fn param(&self, idx: usize, default: usize) -> usize {
        self.params
            .split(';')
            .nth(idx)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
    }
}

/// The byte length of a UTF-8 sequence from its lead byte, or `1` when the byte
/// cannot begin a multibyte sequence.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xf0..=0xf7 => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_after(rows: usize, cols: usize, bytes: &[u8]) -> Vec<String> {
        let mut screen = TerminalScreen::new(rows, cols);
        screen.advance(bytes);
        screen.rows()
    }

    #[test]
    fn plain_text_writes_at_the_cursor_and_trims_trailing_blanks() {
        assert_eq!(screen_after(2, 10, b"hello"), vec!["hello", ""]);
    }

    #[test]
    fn geometry_is_clamped_to_a_valid_cell() {
        let screen = TerminalScreen::new(0, 0);
        assert_eq!(screen.rows(), vec![String::new()]);
        assert_eq!(screen.cursor(), (0, 0));
    }

    #[test]
    fn crlf_returns_to_column_zero_on_the_next_row() {
        assert_eq!(screen_after(3, 10, b"ab\r\ncd"), vec!["ab", "cd", ""]);
    }

    #[test]
    fn bare_line_feed_keeps_the_column() {
        // PTY output normally arrives as CRLF; a lone LF only drops a row.
        assert_eq!(screen_after(3, 6, b"ab\ncd"), vec!["ab", "  cd", ""]);
    }

    #[test]
    fn carriage_return_rewrites_the_current_line() {
        assert_eq!(screen_after(1, 10, b"abc\rX"), vec!["Xbc"]);
    }

    #[test]
    fn backspace_and_tab_reposition_the_cursor() {
        assert_eq!(screen_after(1, 20, b"abc\x08X"), vec!["abX"]);
        assert_eq!(screen_after(1, 20, b"a\tb"), vec!["a       b"]);
    }

    #[test]
    fn tab_is_clamped_to_the_last_column() {
        assert_eq!(screen_after(1, 4, b"a\tZ"), vec!["a  Z"]);
    }

    #[test]
    fn printing_past_the_width_wraps_to_the_next_row() {
        assert_eq!(screen_after(2, 3, b"abcd"), vec!["abc", "d"]);
    }

    #[test]
    fn writing_past_the_last_row_scrolls_up() {
        assert_eq!(screen_after(2, 3, b"one\r\ntwo\r\nend"), vec!["two", "end"]);
    }

    #[test]
    fn line_feed_on_the_last_row_scrolls_without_a_move() {
        let mut screen = TerminalScreen::new(2, 3);
        screen.advance(b"a\r\nb\r\nc");
        assert_eq!(screen.rows(), vec!["b", "c"]);
    }

    #[test]
    fn bell_del_and_other_controls_are_ignored() {
        assert_eq!(screen_after(1, 10, b"a\x07\x7f\x01b"), vec!["ab"]);
    }

    #[test]
    fn sgr_color_sequences_do_not_appear_on_screen() {
        assert_eq!(screen_after(1, 10, b"\x1b[31mred\x1b[0m"), vec!["red"]);
    }

    #[test]
    fn cursor_position_sequence_places_text_absolutely() {
        // CUP is 1-based; `ESC[2;3H` targets row 2 column 3 (zero-based 1,2).
        let mut screen = TerminalScreen::new(3, 6);
        screen.advance(b"\x1b[2;3HX");
        assert_eq!(screen.rows(), vec!["", "  X", ""]);
        assert_eq!(screen.cursor(), (1, 3));
    }

    #[test]
    fn cursor_position_defaults_to_home() {
        let mut screen = TerminalScreen::new(2, 4);
        screen.advance(b"ab\x1b[HZ");
        assert_eq!(screen.rows(), vec!["Zb", ""]);
    }

    #[test]
    fn relative_cursor_moves_are_clamped() {
        let mut screen = TerminalScreen::new(3, 6);
        // Down 5 (clamped to last row), forward 2, up 1, back 1.
        screen.advance(b"\x1b[5B\x1b[2C\x1b[1A\x1b[1D");
        assert_eq!(screen.cursor(), (1, 1));
        // Large moves saturate at the home cell rather than underflowing.
        screen.advance(b"\x1b[100D\x1b[100A");
        assert_eq!(screen.cursor(), (0, 0));
    }

    #[test]
    fn column_and_row_absolute_moves() {
        let mut screen = TerminalScreen::new(3, 8);
        screen.advance(b"\x1b[4GX"); // CHA: column 4 (zero-based 3)
        assert_eq!(screen.cursor(), (0, 4));
        screen.advance(b"\x1b[3dY"); // VPA: row 3 (zero-based 2)
        assert_eq!(screen.cursor(), (2, 5));
    }

    #[test]
    fn erase_line_variants_clear_the_expected_span() {
        assert_eq!(screen_after(1, 8, b"abcdef\r\x1b[K"), vec![""]);
        assert_eq!(screen_after(1, 8, b"abcdef\x1b[2K"), vec![""]);
        // EL 1 clears from start through the cursor inclusive.
        assert_eq!(
            screen_after(1, 8, b"abcdef\r\x1b[2C\x1b[1K"),
            vec!["   def"]
        );
        // EL 0 clears from the cursor to the end of the line.
        assert_eq!(screen_after(1, 8, b"abcdef\r\x1b[3C\x1b[0K"), vec!["abc"]);
    }

    #[test]
    fn erase_display_variants_clear_the_expected_region() {
        // ED 2 clears everything.
        assert_eq!(screen_after(2, 4, b"ab\r\ncd\x1b[2J"), vec!["", ""]);
        // ED 0 (default) clears from the cursor to the end of screen.
        let mut screen = TerminalScreen::new(3, 4);
        screen.advance(b"aa\r\nbb\r\ncc\x1b[2;2H\x1b[J");
        assert_eq!(screen.rows(), vec!["aa", "b", ""]);
        // ED 1 clears from the start of screen through the cursor inclusive.
        let mut screen = TerminalScreen::new(3, 4);
        screen.advance(b"aa\r\nbb\r\ncc\x1b[2;1H\x1b[1J");
        assert_eq!(screen.rows(), vec!["", " b", "cc"]);
    }

    #[test]
    fn osc_title_sequences_are_swallowed() {
        assert_eq!(screen_after(1, 12, b"\x1b]0;my title\x07$ "), vec!["$"]);
        // OSC terminated by ESC (start of ST) instead of BEL.
        assert_eq!(screen_after(1, 12, b"\x1b]0;t\x1b\\$"), vec!["$"]);
    }

    #[test]
    fn charset_select_swallows_its_argument() {
        assert_eq!(screen_after(1, 6, b"\x1b(BAB"), vec!["AB"]);
    }

    #[test]
    fn unknown_escape_is_ignored() {
        assert_eq!(screen_after(1, 6, b"\x1b=AB"), vec!["AB"]);
    }

    #[test]
    fn private_mode_and_incomplete_csi_are_ignored() {
        // `ESC[?25l` (hide cursor) and a stray CSI abort leave text intact.
        assert_eq!(screen_after(1, 6, b"\x1b[?25lAB"), vec!["AB"]);
        assert_eq!(screen_after(1, 6, b"\x1b[1\x00AB"), vec!["AB"]);
    }

    #[test]
    fn multibyte_utf8_is_assembled_across_chunk_boundaries() {
        let mut screen = TerminalScreen::new(1, 6);
        let star = "☆".as_bytes();
        screen.advance(&star[..1]);
        screen.advance(&star[1..]);
        assert_eq!(screen.rows(), vec!["☆"]);
    }

    #[test]
    fn invalid_utf8_lead_and_stray_continuation_are_dropped() {
        // A lead byte interrupted by ASCII drops the partial sequence.
        assert_eq!(screen_after(1, 6, b"\xe3A"), vec!["A"]);
        // A stray continuation byte in Ground is ignored.
        assert_eq!(screen_after(1, 6, b"\x80B"), vec!["B"]);
        // An invalid (overlong-range) lead byte is ignored.
        assert_eq!(screen_after(1, 6, b"\xffC"), vec!["C"]);
    }

    #[test]
    fn invalid_utf8_payload_is_discarded_when_complete() {
        // An overlong 3-byte sequence assembles fully but decodes as invalid.
        assert_eq!(screen_after(1, 6, b"\xe0\x80\x80Z"), vec!["Z"]);
    }

    #[test]
    fn empty_csi_parameters_fall_back_to_defaults() {
        // `ESC[;3H`: the missing first parameter defaults to row 1.
        let mut screen = TerminalScreen::new(3, 6);
        screen.advance(b"\x1b[;3HX");
        assert_eq!(screen.rows(), vec!["  X", "", ""]);
    }
}
