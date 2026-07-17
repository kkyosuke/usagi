//! A minimal terminal screen grid.
//!
//! The daemon owns the PTY and streams **raw** output bytes; this type turns
//! that byte stream into a fixed `rows × cols` character grid the Home right
//! pane can render.  It is a deliberately small VT interpreter: it covers what a
//! login shell prompt and everyday commands such as `ls` emit — printable text,
//! `CR` / `LF` / `BS` / `HT`, line wrap and scroll, cursor moves, line/display
//! erase and SGR styling — and silently ignores window-title (OSC) sequences.
//! It is pure and holds no IO, so it is exercised entirely by unit tests.
//!
//! Alternate screen buffers used by full-screen terminal applications are
//! supported. Scrollback is retained locally with a bounded history for the
//! pane viewport.

use unicode_width::UnicodeWidthChar;

// Kept in sync with `presentation::frame::TERMINAL_CURSOR_MARKER`.  This
// use-case module deliberately does not depend on presentation, while the
// renderer consumes the marker before writing terminal output.
const TERMINAL_CURSOR_MARKER: char = '\u{e0001}';

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

/// One terminal cell and the SGR state that was active when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Cell {
    ch: char,
    style: String,
    continuation: bool,
}

/// The primary screen state saved while a full-screen application owns the
/// alternate buffer. Parser state itself remains shared: `DECSET` / `DECRST`
/// are consumed as one continuous byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScreenBuffer {
    grid: Vec<Vec<Cell>>,
    scrollback: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    style: String,
    saved_cursor: Option<(usize, usize)>,
    scroll_top: usize,
    scroll_bottom: usize,
}

impl Cell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            style: String::new(),
            continuation: false,
        }
    }
}

/// A fixed-size character grid updated from a raw terminal byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalScreen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<Cell>>,
    /// Rows pushed off the visible grid. Keeping this at the terminal decoder
    /// layer preserves the exact terminal semantics for both agent and shell
    /// panes while the view chooses which rows to project.
    scrollback: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    phase: Phase,
    /// Collected `CSI` parameter/intermediate bytes (without the leading `ESC [`).
    params: String,
    /// Partially received UTF-8 bytes awaiting their continuation bytes.
    utf8_pending: Vec<u8>,
    /// The total length of the multibyte sequence currently being assembled.
    utf8_needed: usize,
    /// The complete SGR state to apply to subsequently printed cells.
    style: String,
    /// Cursor position saved by DECSC (`ESC 7`) or SCP (`CSI s`).
    saved_cursor: Option<(usize, usize)>,
    /// Inclusive DECSTBM scroll region. Codex reserves its composer outside
    /// this region and scrolls only the transcript above it.
    scroll_top: usize,
    scroll_bottom: usize,
    /// The primary screen while a full-screen program (for example Codex)
    /// renders into the active alternate buffer.
    primary_screen: Option<Box<ScreenBuffer>>,
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
            grid: vec![vec![Cell::blank(); cols]; rows],
            scrollback: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            phase: Phase::Ground,
            params: String::new(),
            utf8_pending: Vec::new(),
            utf8_needed: 0,
            style: String::new(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            primary_screen: None,
        }
    }

    /// Applies a chunk of raw PTY output.  Chunks may split a multibyte
    /// character; the trailing bytes are buffered until the next call.
    pub fn advance(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed(byte);
        }
    }

    /// Changes the visible width without replaying historical control bytes.
    ///
    /// Historical PTY output can contain cursor moves addressed to a prior
    /// width (notably shell right prompts). Replaying those bytes at a smaller
    /// width duplicates rows. Resize the decoded cells instead: cells outside
    /// the new viewport are clipped and existing history keeps its row count.
    #[coverage(off)] // Geometry changes are exercised through TerminalSession.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (self.rows, self.cols) == (rows, cols) {
            return;
        }

        let old_rows = self.rows;
        self.grid = resize_grid(std::mem::take(&mut self.grid), rows, cols);
        for row in &mut self.scrollback {
            resize_row(row, cols);
        }
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.saved_cursor = self
            .saved_cursor
            .map(|(row, col)| (row.min(rows - 1), col.min(cols - 1)));
        self.scroll_top = self.scroll_top.min(rows - 1);
        self.scroll_bottom = if self.scroll_bottom + 1 == old_rows {
            rows - 1
        } else {
            self.scroll_bottom.min(rows - 1)
        };
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
            self.scroll_bottom = rows - 1;
        }
        if let Some(primary) = &mut self.primary_screen {
            resize_buffer(primary, rows, cols, old_rows);
        }
    }

    /// Renders the grid as one `String` per row with trailing blanks trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| render_row(row, None, ""))
            .collect()
    }

    /// Renders retained scrollback followed by the visible terminal grid.
    #[must_use]
    pub fn rows_with_scrollback(&self) -> Vec<String> {
        let mut rows: Vec<_> = self
            .scrollback
            .iter()
            .map(|row| render_row(row, None, ""))
            .chain(self.grid.iter().map(|row| render_row(row, None, "")))
            .collect();
        // The visible grid is fixed-height, but its unused tail is not terminal
        // content. Dropping it lets the live viewport stay anchored to the last
        // meaningful output instead of a screenful of padding.
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// Renders retained scrollback and the visible grid with the current PTY
    /// cursor as an inverted cell.
    #[must_use]
    #[coverage(off)] // Iterator closure instrumentation is emitted twice by coverage builds.
    pub fn rows_with_scrollback_and_cursor(&self) -> Vec<String> {
        let mut rows: Vec<_> = self
            .scrollback
            .iter()
            .map(|row| render_row(row, None, ""))
            .chain(self.grid.iter().enumerate().map(|(row, cells)| {
                let cursor = (row == self.cursor_row).then_some(self.cursor_col);
                render_row(cells, cursor, &self.style)
            }))
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// Renders scrollback and the visible grid with a cell-precise selection.
    #[must_use]
    #[coverage(off)]
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
        let mut rows: Vec<_> = self
            .scrollback
            .iter()
            .enumerate()
            .map(|(row, cells)| {
                render_row_selected(cells, None, "", selection_for(row, first, last))
            })
            .chain(self.grid.iter().enumerate().map(|(index, cells)| {
                let row = self.scrollback.len() + index;
                let cursor = (index == self.cursor_row).then_some(self.cursor_col);
                render_row_selected(cells, cursor, &self.style, selection_for(row, first, last))
            }))
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// Renders the grid with the current PTY cursor as an inverted cell.
    #[must_use]
    pub fn rows_with_cursor(&self) -> Vec<String> {
        self.grid
            .iter()
            .enumerate()
            .map(|(row, cells)| {
                let cursor = (row == self.cursor_row).then_some(self.cursor_col);
                render_row(cells, cursor, &self.style)
            })
            .collect()
    }

    /// Returns the complete visible grid without trimming trailing spaces.
    ///
    /// Rendering uses [`Self::rows`] because blank padding is not interesting
    /// on screen. Copying, however, must preserve a selected space at the end
    /// of a line, so selection works from this unstyled grid instead.
    #[must_use]
    pub fn cells(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.continuation)
                    .map(|cell| cell.ch)
                    .collect()
            })
            .collect()
    }

    /// Returns retained scrollback followed by the complete visible grid.
    #[must_use]
    #[coverage(off)]
    pub fn cells_with_scrollback(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .scrollback
            .iter()
            .chain(&self.grid)
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.continuation)
                    .map(|cell| cell.ch)
                    .collect()
            })
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
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
            b'7' => {
                self.save_cursor();
                self.phase = Phase::Ground;
            }
            b'8' => {
                self.restore_cursor();
                self.phase = Phase::Ground;
            }
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
            'r' => self.set_scroll_region(),
            'S' => self.scroll_up(self.param(0, 1)),
            'T' => self.scroll_down(self.param(0, 1)),
            'm' => self.sgr(),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'h' => self.set_private_modes(),
            'l' => self.reset_private_modes(),
            // Unhandled finals leave the grid unchanged.
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
            self.grid[row][col] = Cell::blank();
        }
    }

    fn erase_display(&mut self) {
        match self.param(0, 0) {
            1 => {
                for row in 0..self.cursor_row {
                    self.blank_row(row);
                }
                for col in 0..=self.cursor_col.min(self.cols - 1) {
                    self.grid[self.cursor_row][col] = Cell::blank();
                }
            }
            2 => {
                for row in 0..self.rows {
                    self.blank_row(row);
                }
            }
            _ => {
                for col in self.cursor_col..self.cols {
                    self.grid[self.cursor_row][col] = Cell::blank();
                }
                for row in (self.cursor_row + 1)..self.rows {
                    self.blank_row(row);
                }
            }
        }
    }

    fn blank_row(&mut self, row: usize) {
        self.grid[row].fill(Cell::blank());
    }

    fn print(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if self.cursor_col >= self.cols || self.cursor_col + width > self.cols {
            self.cursor_col = 0;
            self.line_feed();
        }
        self.grid[self.cursor_row][self.cursor_col] = Cell {
            ch,
            style: self.style.clone(),
            continuation: false,
        };
        for column in 1..width {
            self.grid[self.cursor_row][self.cursor_col + column] = Cell {
                ch: '\0',
                style: self.style.clone(),
                continuation: true,
            };
        }
        self.cursor_col += width;
    }

    fn line_feed(&mut self) {
        if self.cursor_row >= self.scroll_bottom {
            self.scroll_region_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    fn set_scroll_region(&mut self) {
        let top = self.param(0, 1).saturating_sub(1).min(self.rows - 1);
        let bottom = self
            .param(1, self.rows)
            .saturating_sub(1)
            .min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn scroll_region_up(&mut self, count: usize) {
        for _ in 0..count.min(self.scroll_bottom - self.scroll_top + 1) {
            let row = self.grid.remove(self.scroll_top);
            // Mirror v1's vt100 policy: a region anchored at row zero is
            // transcript history; a lower region is a transient full-screen UI.
            if self.primary_screen.is_none() && self.scroll_top == 0 {
                self.scrollback.push(row);
                if self.scrollback.len() > 10_000 {
                    self.scrollback.remove(0);
                }
            }
            self.grid
                .insert(self.scroll_bottom, vec![Cell::blank(); self.cols]);
        }
    }

    /// Applies CSI SU inside the active DECSTBM scroll region. On the primary
    /// screen, rows leaving a top-anchored region are shell history; on the
    /// alternate screen they are transient app frames.
    fn scroll_up(&mut self, count: usize) {
        self.scroll_region_up(count);
    }

    /// Applies CSI SD inside the active DECSTBM scroll region. Reverse
    /// scrolling never invents local history.
    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count.min(self.scroll_bottom - self.scroll_top + 1) {
            self.grid.remove(self.scroll_bottom);
            self.grid
                .insert(self.scroll_top, vec![Cell::blank(); self.cols]);
        }
    }

    fn tab(&mut self) {
        let next = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next.min(self.cols - 1);
    }

    fn sgr(&mut self) {
        // `CSI m` is reset. Any sequence containing `0` also starts a fresh
        // state, so a later repaint can faithfully reconstruct the style from
        // the beginning of a row rather than relying on terminal history.
        let reset = self.params.is_empty()
            || self
                .params
                .split(';')
                .any(|parameter| parameter.is_empty() || parameter == "0");
        if reset {
            self.style.clear();
        }
        if !self.params.is_empty() && self.params != "0" {
            self.style.push_str("\u{1b}[");
            self.style.push_str(&self.params);
            self.style.push('m');
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.cursor_row = row;
            self.cursor_col = col;
        }
    }

    /// Applies the DEC private modes that change the visible buffer.  Codex's
    /// ratatui renderer uses 1049; accepting the older 47/1047 variants keeps
    /// the pane compatible with other full-screen agents as well.
    fn set_private_modes(&mut self) {
        let modes = self.private_modes();
        if modes.iter().any(|mode| matches!(mode, 47 | 1047 | 1049)) {
            self.enter_alternate_screen();
        }
    }

    fn reset_private_modes(&mut self) {
        let modes = self.private_modes();
        if modes.iter().any(|mode| matches!(mode, 47 | 1047 | 1049)) {
            self.leave_alternate_screen();
        }
    }

    fn private_modes(&self) -> Vec<usize> {
        self.params
            .strip_prefix('?')
            .map_or_else(Vec::new, |values| {
                values
                    .split(';')
                    .filter_map(|value| value.parse::<usize>().ok())
                    .collect()
            })
    }

    fn enter_alternate_screen(&mut self) {
        if self.primary_screen.is_some() {
            return;
        }
        self.primary_screen = Some(Box::new(ScreenBuffer {
            grid: std::mem::take(&mut self.grid),
            scrollback: std::mem::take(&mut self.scrollback),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            style: std::mem::take(&mut self.style),
            saved_cursor: self.saved_cursor.take(),
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
        }));
        self.grid = vec![vec![Cell::blank(); self.cols]; self.rows];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    fn leave_alternate_screen(&mut self) {
        let Some(primary) = self.primary_screen.take() else {
            return;
        };
        self.grid = primary.grid;
        self.scrollback = primary.scrollback;
        self.cursor_row = primary.cursor_row;
        self.cursor_col = primary.cursor_col;
        self.style = primary.style;
        self.saved_cursor = primary.saved_cursor;
        self.scroll_top = primary.scroll_top;
        self.scroll_bottom = primary.scroll_bottom;
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

#[coverage(off)] // Helper for the geometry adapter above.
fn resize_row(row: &mut Vec<Cell>, cols: usize) {
    row.truncate(cols);
    row.resize(cols, Cell::blank());
    if row.last().is_some_and(|cell| cell.continuation) {
        *row.last_mut().expect("terminal rows are never empty") = Cell::blank();
    }
}

#[coverage(off)] // Helper for the geometry adapter above.
fn resize_grid(mut grid: Vec<Vec<Cell>>, rows: usize, cols: usize) -> Vec<Vec<Cell>> {
    grid.truncate(rows);
    for row in &mut grid {
        resize_row(row, cols);
    }
    grid.resize_with(rows, || vec![Cell::blank(); cols]);
    grid
}

#[coverage(off)] // Helper for the geometry adapter above.
fn resize_buffer(buffer: &mut ScreenBuffer, rows: usize, cols: usize, old_rows: usize) {
    buffer.grid = resize_grid(std::mem::take(&mut buffer.grid), rows, cols);
    for row in &mut buffer.scrollback {
        resize_row(row, cols);
    }
    buffer.cursor_row = buffer.cursor_row.min(rows - 1);
    buffer.cursor_col = buffer.cursor_col.min(cols - 1);
    buffer.saved_cursor = buffer
        .saved_cursor
        .map(|(row, col)| (row.min(rows - 1), col.min(cols - 1)));
    buffer.scroll_top = buffer.scroll_top.min(rows - 1);
    buffer.scroll_bottom = if buffer.scroll_bottom + 1 == old_rows {
        rows - 1
    } else {
        buffer.scroll_bottom.min(rows - 1)
    };
    if buffer.scroll_top >= buffer.scroll_bottom {
        buffer.scroll_top = 0;
        buffer.scroll_bottom = rows - 1;
    }
}

fn render_row(row: &[Cell], cursor: Option<usize>, cursor_style: &str) -> String {
    render_row_selected(row, cursor, cursor_style, None)
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
) -> String {
    let cursor = cursor.filter(|column| *column < row.len());
    let last = row
        .iter()
        .rposition(|cell| cell.ch != ' ' && !cell.continuation)
        .into_iter()
        .chain(cursor)
        .max();
    let Some(last) = last else {
        return String::new();
    };
    let mut rendered = String::new();
    let mut active = String::new();
    for (column, cell) in row[..=last].iter().enumerate() {
        if cell.continuation {
            continue;
        }
        let width = if row.get(column + 1).is_some_and(|next| next.continuation) {
            2
        } else {
            1
        };
        let selected = selection
            .is_some_and(|(start, end)| column <= end && column.saturating_add(width) > start);
        let mut style = if cursor == Some(column) {
            let base = if cell.style.is_empty() {
                cursor_style
            } else {
                cell.style.as_str()
            };
            format!("{base}\u{1b}[7m")
        } else {
            cell.style.clone()
        };
        if selected {
            style.push_str("\u{1b}[7m");
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
        rendered.push(cell.ch);
    }
    if !active.is_empty() {
        rendered.push_str("\u{1b}[0m");
    }
    rendered
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
    fn resize_clips_scrollback_without_creating_additional_rows() {
        let mut screen = TerminalScreen::new(2, 10);
        screen.advance(b"first-row\r\nsecond-row\r\nthird-row");
        assert_eq!(
            screen.rows_with_scrollback(),
            vec!["first-row", "second-row", "third-row"]
        );

        screen.resize(2, 5);
        assert_eq!(
            screen.rows_with_scrollback(),
            vec!["first", "secon", "third"]
        );
    }

    #[test]
    fn cells_keep_trailing_spaces_for_copying() {
        let mut screen = TerminalScreen::new(1, 5);
        screen.advance(b"a b");
        assert_eq!(screen.rows(), vec!["a b"]);
        assert_eq!(screen.cells(), vec!["a b  "]);
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
    fn wide_characters_occupy_terminal_columns_and_selection_marks_their_cell() {
        let mut screen = TerminalScreen::new(2, 4);
        screen.advance("AあB".as_bytes());
        assert_eq!(screen.rows(), vec!["AあB", ""]);
        assert_eq!(screen.cells(), vec!["AあB", "    "]);
        assert_eq!(screen.cursor(), (0, 4));
        assert_eq!(
            screen.rows_with_scrollback_and_cursor_selection((0, 1), (0, 2)),
            vec!["A\u{1b}[7mあ\u{1b}[0mB"]
        );
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
    fn scrollback_retains_rows_pushed_off_the_visible_grid() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo\r\nthree");
        assert_eq!(screen.rows(), vec!["two", "three"]);
        assert_eq!(screen.rows_with_scrollback(), vec!["one", "two", "three"]);
        assert_eq!(
            screen.cells_with_scrollback(),
            vec!["one     ", "two     ", "three   "]
        );
    }

    #[test]
    fn csi_scroll_commands_keep_the_latest_full_screen_agent_frame_visible() {
        let mut screen = TerminalScreen::new(3, 12);
        screen.advance(b"\x1b[?1049hone\r\ntwo\r\nthree");
        screen.advance(b"\x1b[1S\x1b[3;1Hreply");
        assert_eq!(screen.rows_with_scrollback(), vec!["two", "three", "reply"]);

        screen.advance(b"\x1b[1T");
        assert_eq!(screen.rows_with_scrollback(), vec!["", "two", "three"]);
    }

    #[test]
    fn codex_scroll_region_keeps_the_composer_and_latest_reply_on_screen() {
        let mut screen = TerminalScreen::new(4, 16);
        screen.advance(b"\x1b[?1049hheader\x1b[2;3r\x1b[2;1Hone\r\ntwo\r\nreply");

        assert_eq!(screen.rows(), vec!["header", "two", "reply", ""]);
        assert_eq!(
            screen.rows_with_scrollback(),
            vec!["header", "two", "reply"]
        );
    }

    #[test]
    fn csi_scroll_commands_respect_the_codex_scroll_region() {
        let mut screen = TerminalScreen::new(4, 16);
        screen.advance(b"\x1b[?1049hheader\x1b[4;1Hcomposer\x1b[2;3r\x1b[2;1Hone\r\ntwo");

        screen.advance(b"\x1b[1S");
        assert_eq!(screen.rows(), vec!["header", "two", "", "composer"]);

        screen.advance(b"\x1b[1T");
        assert_eq!(screen.rows(), vec!["header", "", "two", "composer"]);
    }

    #[test]
    fn invalid_scroll_region_resets_to_the_full_screen() {
        let mut screen = TerminalScreen::new(3, 8);
        screen.advance(b"\x1b[2;3r\x1b[3;2r");

        assert_eq!((screen.scroll_top, screen.scroll_bottom), (0, 2));
    }

    #[test]
    fn scrollback_omits_unused_visible_rows_and_is_bounded() {
        let mut screen = TerminalScreen::new(2, 8);
        screen.advance(b"one\r\ntwo\r\n");
        assert_eq!(screen.rows_with_scrollback(), vec!["one", "two"]);

        for _ in 0..10_001 {
            screen.advance(b"x\r\n");
        }
        assert_eq!(screen.scrollback.len(), 10_000);
    }

    #[test]
    fn bell_del_and_other_controls_are_ignored() {
        assert_eq!(screen_after(1, 10, b"a\x07\x7f\x01b"), vec!["ab"]);
    }

    #[test]
    fn sgr_colors_and_attributes_are_preserved_in_rendered_rows() {
        assert_eq!(
            screen_after(1, 10, b"\x1b[31mred\x1b[0m"),
            vec!["\x1b[31mred\x1b[0m"]
        );
        assert_eq!(
            screen_after(1, 10, b"\x1b[1;38;5;208mhi\x1b[0mok"),
            vec!["\x1b[1;38;5;208mhi\x1b[0mok"]
        );
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
    fn saved_cursor_restores_the_input_position_after_a_right_prompt() {
        let mut screen = TerminalScreen::new(1, 30);
        screen.advance(b"left\x1b[s\x1b[25G\x1b[36mright\x1b[0m\x1b[u input");
        assert_eq!(screen.cursor(), (0, 10));
        assert_eq!(
            screen.rows(),
            vec!["left input              \x1b[36mright\x1b[0m"]
        );

        screen.advance(b"\x1b7\x1b[1G$ \x1b8!");
        assert_eq!(
            screen.rows()[0],
            "$ ft input!             \x1b[36mright\x1b[0m"
        );
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
    fn alternate_screen_keeps_full_screen_agent_output_visible_and_restores_the_shell() {
        let mut screen = TerminalScreen::new(2, 12);
        screen.advance(b"$ shell");

        // An unmatched restore and unknown CSI are harmless before an agent
        // takes over the pane.
        screen.advance(b"\x1b[?1049l\x1b[?9999z");
        assert_eq!(screen.rows(), vec!["$ shell", ""]);

        // Codex uses DECSET 1049 before its ratatui frame. Its output must be
        // rendered from the alternate buffer while it is active, rather than
        // leaving the pane on the underlying shell frame.
        screen.advance(b"\x1b[?1049h\x1b[2J\x1b[Hcodex reply");
        assert_eq!(screen.rows(), vec!["codex reply", ""]);

        // A repeated DECSET must not replace the saved primary screen.
        screen.advance(b"\x1b[?1049h");
        assert_eq!(screen.rows(), vec!["codex reply", ""]);

        // A normal exit restores the prior shell viewport exactly.
        screen.advance(b"\x1b[?1049l");
        assert_eq!(screen.rows(), vec!["$ shell", ""]);
    }

    #[test]
    fn alternate_screen_scroll_does_not_mix_old_frames_into_agent_output() {
        let mut screen = TerminalScreen::new(2, 12);
        screen.advance(b"shell\r\n");
        screen.advance(b"\x1b[?1049hfirst\r\nsecond\r\nthird");

        assert_eq!(screen.rows(), vec!["second", "third"]);
        assert_eq!(screen.rows_with_scrollback(), vec!["second", "third"]);

        // A full-screen redraw replaces the grid, not a synthetic history of
        // the preceding frame.
        screen.advance(b"\x1b[2J\x1b[Hreply");
        assert_eq!(screen.rows_with_scrollback(), vec!["reply"]);
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
