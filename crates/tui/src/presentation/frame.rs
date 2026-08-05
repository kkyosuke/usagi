//! Pure terminal frame grid and incremental diff renderer.
//!
//! Views produce ANSI-decorated strings, while this module turns them into a
//! fixed cell grid and a list of row/column spans.  It deliberately has no
//! terminal dependency: a later adapter owns cursor movement and writes.

use unicode_width::UnicodeWidthChar;

use std::hash::{DefaultHasher, Hash, Hasher};

const ESC: char = '\u{1b}';
const RESET: &str = "\u{1b}[0m";

/// Zero-width, renderer-only marker for a background terminal cursor.
///
/// Views put this immediately before the visual block caret.  It never reaches
/// the terminal; [`Frame::from_lines`] instead records the cell so the runtime
/// can place the physical cursor there for IME candidate windows.
pub const TERMINAL_CURSOR_MARKER: char = '\u{e0001}';

/// Zero-width renderer-only marker for the currently focused text control.
/// This wins over [`TERMINAL_CURSOR_MARKER`] when a modal is open above a live
/// terminal pane.
pub const INPUT_CURSOR_MARKER: char = '\u{e0002}';

/// One terminal cell in a [`Frame`].
///
/// A double-width glyph occupies a `Glyph` cell followed by a `Continuation`.
/// Keeping the continuation explicit prevents a diff from beginning or ending
/// in the middle of a wide glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// Nothing has been drawn at this column.
    Empty,
    /// A visible scalar, its display width, and compact frame-local references.
    Glyph {
        scalar: char,
        width: u8,
        text_start: u32,
        text_len: u32,
        style_id: u32,
    },
    /// The second column of the preceding double-width [`Cell::Glyph`].
    Continuation,
}

#[derive(Debug, Clone, Default)]
struct Row {
    /// Concatenated render text for the row. Glyph cells reference byte ranges
    /// in this buffer, so ordinary glyphs do not own individual `String`s.
    text: String,
    hash: u64,
}

impl Cell {
    fn width(&self) -> usize {
        match self {
            Self::Glyph { width, .. } => usize::from(*width),
            Self::Empty | Self::Continuation => 1,
        }
    }
}

/// A rectangular, display-column based terminal frame.
#[derive(Debug, Clone)]
pub struct Frame {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    rows: Vec<Row>,
    /// Canonical non-empty SGR states. ID zero always means the plain style.
    styles: Vec<String>,
    input_cursor: Option<(usize, usize)>,
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.input_cursor == other.input_cursor
            && (0..self.height).all(|row| {
                self.rows[row].hash == other.rows[row].hash
                    && (0..self.width).all(|column| cells_equal(self, other, row, column))
            })
    }
}

impl Eq for Frame {}

impl Frame {
    /// Builds a grid of `width` columns and `height` rows from view lines.
    ///
    /// ANSI escape sequences consume no columns.  A glyph which would extend
    /// beyond the right edge is omitted as a whole, never split across cells.
    #[must_use]
    pub fn from_lines<I, S>(width: usize, height: usize, lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut frame = Self {
            width,
            height,
            cells: vec![Cell::Empty; width.saturating_mul(height)],
            rows: vec![Row::default(); height],
            styles: Vec::new(),
            input_cursor: None,
        };
        for row in 0..height {
            frame.rows[row].hash = frame.row_hash(row);
        }
        for (row, line) in lines.into_iter().take(height).enumerate() {
            frame.set_line(row, line.as_ref());
        }
        frame
    }

    /// Number of display columns.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Number of display rows.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// The cell at `row`, `column`, if it belongs to this frame.
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        (row < self.height && column < self.width).then(|| &self.cells[row * self.width + column])
    }

    /// The requested OS text-input cursor cell, if this frame has an active
    /// editable control.
    #[must_use]
    pub const fn input_cursor(&self) -> Option<(usize, usize)> {
        self.input_cursor
    }

    fn set_line(&mut self, row: usize, line: &str) {
        if self.width == 0 {
            return;
        }
        let mut column = 0;
        let mut pending_ansi = String::new();
        let mut active_style = String::new();
        let mut last_glyph = None;
        let mut row_text = String::with_capacity(line.len());
        let mut chars = line.char_indices().peekable();
        while let Some((byte_index, character)) = chars.next() {
            if character == INPUT_CURSOR_MARKER {
                // A focused form/modal must take precedence over a live
                // terminal cursor that can still be present in its background.
                self.input_cursor = Some((row, column));
                continue;
            }
            if character == TERMINAL_CURSOR_MARKER {
                if self.input_cursor.is_none() {
                    self.input_cursor = Some((row, column));
                }
                continue;
            }
            if character == ESC {
                let (sequence, consumed_bytes) = ansi_sequence(&line[byte_index..]);
                pending_ansi.push_str(sequence);
                update_active_style(&mut active_style, sequence);
                let sequence_end = byte_index + consumed_bytes;
                while chars
                    .peek()
                    .is_some_and(|(next_index, _)| *next_index < sequence_end)
                {
                    let _ = chars.next();
                }
                continue;
            }

            let glyph_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if glyph_width == 0 {
                if let Some(last_glyph) = last_glyph
                    && let Cell::Glyph { text_len, .. } = &mut self.cells[last_glyph]
                {
                    row_text.push(character);
                    *text_len = text_len
                        .checked_add(
                            u32::try_from(character.len_utf8())
                                .expect("UTF-8 scalar length fits u32"),
                        )
                        .expect("frame row text length fits u32");
                } else {
                    pending_ansi.push(character);
                }
                continue;
            }
            if glyph_width > self.width.saturating_sub(column) {
                break;
            }
            let cell_index = row * self.width + column;
            let text_start = u32::try_from(row_text.len()).expect("frame row text offset fits u32");
            row_text.push_str(&pending_ansi);
            pending_ansi.clear();
            row_text.push(character);
            let text_len =
                u32::try_from(row_text.len()).expect("frame row text length fits u32") - text_start;
            let style_id = self.intern_style(&active_style);
            self.cells[cell_index] = Cell::Glyph {
                scalar: character,
                width: u8::try_from(glyph_width).expect("unicode display width fits in u8"),
                text_start,
                text_len,
                style_id,
            };
            for offset in 1..glyph_width {
                self.cells[cell_index + offset] = Cell::Continuation;
            }
            last_glyph = Some(cell_index);
            column += glyph_width;
        }
        if let Some(last_glyph) = last_glyph.filter(|_| !pending_ansi.is_empty())
            && let Cell::Glyph { text_len, .. } = &mut self.cells[last_glyph]
        {
            row_text.push_str(&pending_ansi);
            *text_len = text_len
                .checked_add(u32::try_from(pending_ansi.len()).expect("ANSI text length fits u32"))
                .expect("frame row text length fits u32");
        }
        self.rows[row].text = row_text;
        self.rows[row].hash = self.row_hash(row);
    }

    fn intern_style(&mut self, style: &str) -> u32 {
        if style.is_empty() {
            return 0;
        }
        if let Some(index) = self.styles.iter().position(|candidate| candidate == style) {
            return u32::try_from(index + 1).expect("frame style count fits u32");
        }
        self.styles.push(style.to_owned());
        u32::try_from(self.styles.len()).expect("frame style count fits u32")
    }

    fn style(&self, style_id: u32) -> &str {
        if style_id == 0 {
            ""
        } else {
            &self.styles[usize::try_from(style_id - 1).expect("style ID fits usize")]
        }
    }

    fn glyph_text(&self, row: usize, cell: &Cell) -> &str {
        let Cell::Glyph {
            text_start,
            text_len,
            ..
        } = cell
        else {
            return "";
        };
        let start = usize::try_from(*text_start).expect("text offset fits usize");
        let end = start + usize::try_from(*text_len).expect("text length fits usize");
        &self.rows[row].text[start..end]
    }

    fn row_hash(&self, row: usize) -> u64 {
        let mut hasher = DefaultHasher::new();
        for column in 0..self.width {
            match self.cell(row, column).expect("row cell is inside frame") {
                Cell::Empty => 0_u8.hash(&mut hasher),
                Cell::Continuation => 1_u8.hash(&mut hasher),
                cell @ Cell::Glyph {
                    scalar,
                    width,
                    style_id,
                    ..
                } => {
                    2_u8.hash(&mut hasher);
                    scalar.hash(&mut hasher);
                    width.hash(&mut hasher);
                    self.glyph_text(row, cell).hash(&mut hasher);
                    self.style(*style_id).hash(&mut hasher);
                }
            }
        }
        hasher.finish()
    }

    fn glyph_start(&self, row: usize, column: usize) -> usize {
        let mut column = column;
        while column > 0 && matches!(self.cell(row, column), Some(Cell::Continuation)) {
            column -= 1;
        }
        column
    }

    fn glyph_end(&self, row: usize, column: usize) -> usize {
        let start = self.glyph_start(row, column);
        start + self.cell(row, start).map_or(1, Cell::width)
    }

    fn span_text(&self, row: usize, start: usize, end: usize) -> String {
        // The terminal keeps SGR state across cursor moves and across our
        // incremental writes. A diff span has no reliable knowledge of the
        // previous physical style at its cursor position, so make every span
        // self-contained. Without this, replacing a coloured cell with plain
        // text can leave the old foreground colour on screen.
        let mut text = RESET.to_owned();
        // 差分 span は色付き run の途中から始まることがある。その glyph 自身には
        // SGR 開始列がなくても `style` には現在の属性が残っているので、span の先頭で
        // 再出力する。これをしないと、後から追記された入力文字だけが terminal の
        // reset 後に白く描画される。
        if let Some(cell @ Cell::Glyph { style_id, .. }) = self.cell(row, start)
            && let glyph = self.glyph_text(row, cell)
            && let style = self.style(*style_id)
            && !style.is_empty()
            && !glyph.starts_with(ESC)
        {
            text.push_str(style);
        }
        for column in start..end {
            match self.cell(row, column).expect("span is inside frame") {
                Cell::Empty => text.push(' '),
                cell @ Cell::Glyph { .. } => text.push_str(self.glyph_text(row, cell)),
                Cell::Continuation => {}
            }
        }
        if !text.ends_with(RESET) {
            text.push_str(RESET);
        }
        text
    }

    #[cfg(test)]
    fn resident_payload_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Cell>()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + self
                .rows
                .iter()
                .map(|row| row.text.capacity())
                .sum::<usize>()
            + self.styles.capacity() * std::mem::size_of::<String>()
            + self.styles.iter().map(String::capacity).sum::<usize>()
    }
}

/// A changed, contiguous range of cells in one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Zero-based terminal row.
    pub row: usize,
    /// Zero-based terminal column.
    pub column: usize,
    /// ANSI-preserving text to write at `row`, `column`.
    pub text: String,
}

/// The pure output consumed by a real-terminal adapter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameDiff {
    /// Clear the complete surface before applying [`Self::spans`].
    pub clear_surface: bool,
    /// Changed row/column spans, in terminal order.
    pub spans: Vec<Span>,
    /// Physical cursor location for the active text input, independent of
    /// which spans happened to change in this frame.
    pub input_cursor: Option<(usize, usize)>,
}

/// Retains the previous frame and creates pure incremental diffs.
#[derive(Debug, Default)]
pub struct FrameRenderer {
    previous: Option<Frame>,
    reset_pending: bool,
}

impl FrameRenderer {
    /// Creates a renderer without a base frame. Its first render clears and
    /// paints the entire supplied frame.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            previous: None,
            reset_pending: false,
        }
    }

    /// Invalidates the surface while preserving no terminal-specific state.
    /// The next [`Self::render`] clears the surface and repaints every row.
    pub fn reset_surface(&mut self) {
        self.reset_pending = true;
    }

    /// Diffs `next` against the previous frame and remembers it as the base.
    /// A changed geometry is a resize: it discards the base and returns a full
    /// surface clear followed by complete-row spans.
    #[must_use]
    pub fn render(&mut self, next: Frame) -> FrameDiff {
        let full_repaint = self.reset_pending
            || self.previous.as_ref().is_none_or(|previous| {
                previous.width != next.width || previous.height != next.height
            });
        self.reset_pending = false;

        let spans = if full_repaint {
            full_spans(&next)
        } else {
            // A missing base always sets `full_repaint`; `unwrap_or` keeps the
            // state transition total if that invariant changes later.
            diff_spans(self.previous.as_ref().unwrap_or(&next), &next)
        };
        let input_cursor = next.input_cursor();
        self.previous = Some(next);
        FrameDiff {
            clear_surface: full_repaint,
            spans,
            input_cursor,
        }
    }
}

fn full_spans(frame: &Frame) -> Vec<Span> {
    (0..frame.height)
        .map(|row| Span {
            row,
            column: 0,
            text: frame.span_text(row, 0, frame.width),
        })
        .collect()
}

fn diff_spans(previous: &Frame, next: &Frame) -> Vec<Span> {
    let mut spans = Vec::new();
    for row in 0..next.height {
        if previous.rows[row].hash == next.rows[row].hash {
            continue;
        }
        let mut changed = (0..next.width)
            .map(|column| !cells_equal(previous, next, row, column))
            .collect::<Vec<_>>();
        expand_wide_glyph_changes(&mut changed, previous, next, row);
        let mut column = 0;
        while column < next.width {
            if !changed[column] {
                column += 1;
                continue;
            }
            let start = column;
            while column < next.width && changed[column] {
                column += 1;
            }
            spans.push(Span {
                row,
                column: start,
                text: next.span_text(row, start, column),
            });
        }
    }
    spans
}

fn cells_equal(previous: &Frame, next: &Frame, row: usize, column: usize) -> bool {
    match (previous.cell(row, column), next.cell(row, column)) {
        (Some(Cell::Empty), Some(Cell::Empty))
        | (Some(Cell::Continuation), Some(Cell::Continuation)) => true,
        (
            Some(
                previous_cell @ Cell::Glyph {
                    scalar: previous_scalar,
                    width: previous_width,
                    style_id: previous_style,
                    ..
                },
            ),
            Some(
                next_cell @ Cell::Glyph {
                    scalar: next_scalar,
                    width: next_width,
                    style_id: next_style,
                    ..
                },
            ),
        ) => {
            previous_scalar == next_scalar
                && previous_width == next_width
                && previous.glyph_text(row, previous_cell) == next.glyph_text(row, next_cell)
                && previous.style(*previous_style) == next.style(*next_style)
        }
        _ => false,
    }
}

fn expand_wide_glyph_changes(changed: &mut [bool], previous: &Frame, next: &Frame, row: usize) {
    loop {
        let mut expanded = false;
        for column in 0..changed.len() {
            if !changed[column] {
                continue;
            }
            for frame in [previous, next] {
                let start = frame.glyph_start(row, column);
                let end = frame.glyph_end(row, column).min(changed.len());
                for cell in &mut changed[start..end] {
                    if !*cell {
                        *cell = true;
                        expanded = true;
                    }
                }
            }
        }
        if !expanded {
            return;
        }
    }
}

fn ansi_sequence(text: &str) -> (&str, usize) {
    let mut chars = text.char_indices();
    let (_, first) = chars.next().expect("ANSI parser starts at ESC");
    let Some((_, second)) = chars.next() else {
        return (&text[..first.len_utf8()], first.len_utf8());
    };
    if second != '[' {
        return (&text[..first.len_utf8()], first.len_utf8());
    }
    for (index, character) in chars {
        if ('\u{40}'..='\u{7e}').contains(&character) {
            let consumed = index + character.len_utf8();
            return (&text[..consumed], consumed);
        }
    }
    (text, text.len())
}

/// Reflect an ANSI SGR sequence in the state used for frame diffing. The
/// renderer only needs this state for equality: output still preserves the
/// original escape placement in each glyph's `text`.
fn update_active_style(active_style: &mut String, sequence: &str) {
    if !sequence.starts_with("\u{1b}[") || !sequence.ends_with('m') {
        return;
    }
    let params = &sequence[2..sequence.len() - 1];
    if params.is_empty() || params.split(';').any(|param| param == "0") {
        active_style.clear();
    } else {
        active_style.push_str(sequence);
    }
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell as CounterCell;

    use super::{
        Cell, Frame, FrameRenderer, INPUT_CURSOR_MARKER, Span, TERMINAL_CURSOR_MARKER,
        update_active_style,
    };

    struct CountingAllocator;

    thread_local! {
        static COUNT_ALLOCATIONS: CounterCell<bool> = const { CounterCell::new(false) };
        static ALLOCATION_COUNT: CounterCell<usize> = const { CounterCell::new(0) };
    }

    #[global_allocator]
    static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: forwarding the unchanged allocation request to the system allocator.
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                record_allocation();
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: forwarding the unchanged allocation request to the system allocator.
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                record_allocation();
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: `pointer` and `layout` came from the system allocator above.
            unsafe { System.dealloc(pointer, layout) };
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: forwarding the unchanged reallocation request to the system allocator.
            let pointer = unsafe { System.realloc(pointer, layout, new_size) };
            if !pointer.is_null() {
                record_allocation();
            }
            pointer
        }
    }

    fn record_allocation() {
        COUNT_ALLOCATIONS.with(|enabled| {
            if enabled.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
            }
        });
    }

    fn count_allocations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATION_COUNT.with(|count| count.set(0));
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
        let value = operation();
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
        let count = ALLOCATION_COUNT.with(CounterCell::get);
        (value, count)
    }

    fn frame(width: usize, height: usize, lines: &[&str]) -> Frame {
        Frame::from_lines(width, height, lines)
    }

    fn cell_text(frame: &Frame, row: usize, column: usize) -> &str {
        let cell = frame.cell(row, column).expect("cell exists");
        frame.glyph_text(row, cell)
    }

    #[test]
    fn golden_frame_uses_display_columns_and_never_splits_wide_glyphs() {
        let rendered = frame(5, 2, &["A\u{1b}[31mあ\u{1b}[0mB", "界x"]);
        assert_eq!(rendered, frame(5, 2, &["A\u{1b}[31mあ\u{1b}[0mB", "界x"]));
        assert!(matches!(
            rendered.cell(0, 0),
            Some(Cell::Glyph {
                scalar: 'A',
                width: 1,
                ..
            })
        ));
        assert!(matches!(
            rendered.cell(0, 1),
            Some(Cell::Glyph { width: 2, .. })
        ));
        assert_eq!(rendered.cell(0, 2), Some(&Cell::Continuation));
        assert!(matches!(
            rendered.cell(1, 0),
            Some(Cell::Glyph { width: 2, .. })
        ));
        assert_eq!(rendered.cell(1, 1), Some(&Cell::Continuation));

        let clipped = frame(3, 1, &["aあb"]);
        assert!(matches!(
            clipped.cell(0, 1),
            Some(Cell::Glyph { width: 2, .. })
        ));
        assert_eq!(clipped.cell(0, 2), Some(&Cell::Continuation));
    }

    #[test]
    fn ansi_has_zero_width_and_ambiguous_characters_are_one_column() {
        let ansi = frame(2, 1, &["\u{1b}[1;31mab\u{1b}[0m"]);
        assert!(matches!(
            ansi.cell(0, 0),
            Some(Cell::Glyph { width: 1, .. })
        ));
        assert!(matches!(
            ansi.cell(0, 1),
            Some(Cell::Glyph { width: 1, .. })
        ));

        let ambiguous = frame(2, 1, &["Ωx"]);
        assert!(matches!(
            ambiguous.cell(0, 0),
            Some(Cell::Glyph { width: 1, .. })
        ));
        assert!(matches!(
            ambiguous.cell(0, 1),
            Some(Cell::Glyph { width: 1, .. })
        ));
    }

    #[test]
    fn input_cursor_marker_is_not_drawn_and_tracks_its_display_cell() {
        let rendered = frame(8, 2, &[&format!("aあ{INPUT_CURSOR_MARKER}b"), ""]);
        assert_eq!(rendered.input_cursor(), Some((0, 3)));
        assert_eq!(cell_text(&rendered, 0, 3), "b");

        let diff = FrameRenderer::new().render(rendered);
        assert_eq!(diff.input_cursor, Some((0, 3)));
        assert!(
            !diff
                .spans
                .iter()
                .any(|span| span.text.contains(INPUT_CURSOR_MARKER))
        );
    }

    #[test]
    fn focused_input_cursor_overrides_a_terminal_cursor_in_its_background() {
        let rendered = frame(
            8,
            2,
            &[
                &format!("{INPUT_CURSOR_MARKER}form"),
                &format!("{TERMINAL_CURSOR_MARKER}shell"),
            ],
        );
        assert_eq!(rendered.input_cursor(), Some((0, 0)));
        let terminal_only = frame(8, 1, &[&format!("x{TERMINAL_CURSOR_MARKER}y")]);
        assert_eq!(terminal_only.input_cursor(), Some((0, 1)));
        let styled = frame(2, 1, &["\u{1b}[31ma"]);
        assert!(styled.span_text(0, 0, 1).ends_with("\u{1b}[0m"));
    }

    #[test]
    fn frame_handles_empty_geometry_combining_marks_and_malformed_ansi() {
        let empty = frame(0, 2, &["ignored"]);
        assert_eq!(empty.width(), 0);
        assert_eq!(empty.height(), 2);
        assert_eq!(empty.cell(0, 0), None);

        let combining = frame(2, 1, &["e\u{301}x"]);
        assert!(matches!(
            combining.cell(0, 0),
            Some(Cell::Glyph { width: 1, .. })
        ));
        assert_eq!(cell_text(&combining, 0, 0), "e\u{301}");
        let leading_combining = frame(2, 1, &["\u{301}x"]);
        assert!(matches!(
            leading_combining.cell(0, 0),
            Some(Cell::Glyph { width: 1, .. })
        ));
        assert_eq!(cell_text(&leading_combining, 0, 0), "\u{301}x");

        let malformed = frame(2, 1, &["\u{1b}X"]);
        assert!(matches!(
            malformed.cell(0, 0),
            Some(Cell::Glyph { width: 1, .. })
        ));
        assert_eq!(cell_text(&malformed, 0, 0), "\u{1b}X");
        assert_eq!(malformed.glyph_text(0, &Cell::Empty), "");
        assert_eq!(frame(2, 1, &["\u{1b}"]).cell(0, 0), Some(&Cell::Empty));
        assert_eq!(frame(2, 1, &["\u{1b}[31"]).cell(0, 0), Some(&Cell::Empty));
    }

    #[test]
    fn identical_frames_emit_no_content_writes() {
        let mut renderer = FrameRenderer::new();
        let first = frame(4, 1, &["same"]);
        assert!(renderer.render(first.clone()).clear_surface);
        assert!(renderer.render(first).spans.is_empty());
    }

    #[test]
    fn one_changed_span_only_writes_its_row_and_columns() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(6, 2, &["abcdef", "second"]));
        let diff = renderer.render(frame(6, 2, &["abZdef", "second"]));
        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 2,
                text: "\u{1b}[0mZ\u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn shortening_writes_spaces_over_the_stale_suffix() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(6, 1, &["abcdef"]));
        let diff = renderer.render(frame(6, 1, &["abc"]));
        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 3,
                text: "\u{1b}[0m   \u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn a_diff_touching_wide_glyph_repaints_the_whole_glyph() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(4, 1, &["a界b"]));
        let diff = renderer.render(frame(4, 1, &["a語b"]));
        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 1,
                text: "\u{1b}[0m語\u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn changing_a_style_repaints_every_glyph_in_its_span() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(6, 1, &["  Open "]));

        let diff = renderer.render(frame(6, 1, &["  \u{1b}[1;36mOpen\u{1b}[0m "]));

        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 2,
                text: "\u{1b}[0m\u{1b}[1;36mOpen\u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn extending_a_styled_run_reopens_its_style_for_the_new_glyph() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(6, 1, &["\u{1b}[1;36mab\u{1b}[0m"]));

        let diff = renderer.render(frame(6, 1, &["\u{1b}[1;36mabc\u{1b}[0m"]));

        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 1,
                text: "\u{1b}[0m\u{1b}[1;36mbc\u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn sgr_style_state_ignores_non_sgr_sequences_accumulates_and_resets() {
        let mut style = String::new();
        update_active_style(&mut style, "\u{1b}[2J");
        assert!(style.is_empty());

        update_active_style(&mut style, "\u{1b}[1;36m");
        update_active_style(&mut style, "\u{1b}[4m");
        assert_eq!(style, "\u{1b}[1;36m\u{1b}[4m");

        update_active_style(&mut style, "\u{1b}[0m");
        assert!(style.is_empty());
        update_active_style(&mut style, "\u{1b}[m");
        assert!(style.is_empty());
    }

    #[test]
    fn reset_and_resize_clear_then_repaint_every_row() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(3, 2, &["one", "two"]));
        renderer.reset_surface();
        let reset = renderer.render(frame(3, 2, &["one", "two"]));
        assert!(reset.clear_surface);
        assert_eq!(reset.spans.len(), 2);

        let resized = renderer.render(frame(4, 1, &["wide"]));
        assert!(resized.clear_surface);
        assert_eq!(
            resized.spans,
            vec![Span {
                row: 0,
                column: 0,
                text: "\u{1b}[0mwide\u{1b}[0m".into()
            }]
        );
    }

    #[test]
    fn clearing_a_coloured_cell_resets_the_terminal_before_plain_text() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(4, 1, &["\u{1b}[1;32mok\u{1b}[0m"]));

        let diff = renderer.render(frame(4, 1, &["ok"]));

        assert_eq!(
            diff.spans,
            vec![Span {
                row: 0,
                column: 0,
                text: "\u{1b}[0mok\u{1b}[0m".into(),
            }]
        );
    }

    #[test]
    fn frame_allocations_scale_with_rows_and_style_runs_instead_of_glyphs() {
        let plain_120 = vec!["x".repeat(120); 40];
        let plain_240 = vec!["x".repeat(240); 40];
        let styled_120 = vec![format!("\u{1b}[31m{}\u{1b}[0m", "x".repeat(120)); 40];
        let styled_240 = vec![format!("\u{1b}[31m{}\u{1b}[0m", "x".repeat(240)); 40];

        let (_, plain_120_allocations) =
            count_allocations(|| Frame::from_lines(120, 40, &plain_120));
        let (_, plain_240_allocations) =
            count_allocations(|| Frame::from_lines(240, 40, &plain_240));
        let (_, styled_120_allocations) =
            count_allocations(|| Frame::from_lines(120, 40, &styled_120));
        let (_, styled_240_allocations) =
            count_allocations(|| Frame::from_lines(240, 40, &styled_240));

        assert!(
            plain_120_allocations <= 50,
            "plain: {plain_120_allocations}"
        );
        assert!(
            styled_120_allocations <= 140,
            "styled: {styled_120_allocations}"
        );
        assert!(plain_240_allocations <= plain_120_allocations + 2);
        assert!(styled_240_allocations <= styled_120_allocations + 2);
    }

    #[test]
    fn two_frame_resident_payload_is_less_than_half_the_per_cell_string_model() {
        #[allow(dead_code)]
        enum LegacyCell {
            Empty,
            Glyph {
                text: String,
                width: u8,
                style: String,
            },
            Continuation,
        }

        let lines = vec![format!("\u{1b}[31m{}\u{1b}[0m", "x".repeat(120)); 40];
        let previous = Frame::from_lines(120, 40, &lines);
        let next = Frame::from_lines(120, 40, &lines);
        let current_payload = previous.resident_payload_bytes() + next.resident_payload_bytes();
        let legacy_cell_storage = 2 * 120 * 40 * std::mem::size_of::<LegacyCell>();

        assert!(
            current_payload * 2 < legacy_cell_storage,
            "current pair: {current_payload} bytes, legacy cell storage: {legacy_cell_storage} bytes"
        );
    }

    #[test]
    fn canonical_style_equality_does_not_depend_on_frame_local_ids() {
        let mut renderer = FrameRenderer::new();
        let _ = renderer.render(frame(
            4,
            2,
            &["\u{1b}[31mred\u{1b}[0m", "\u{1b}[34mblue\u{1b}[0m"],
        ));

        let diff = renderer.render(frame(4, 2, &["new", "\u{1b}[34mblue\u{1b}[0m"]));

        assert_eq!(diff.spans.len(), 1);
        assert_eq!(diff.spans[0].row, 0);
    }
}
