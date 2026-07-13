//! Pure terminal frame grid and incremental diff renderer.
//!
//! Views produce ANSI-decorated strings, while this module turns them into a
//! fixed cell grid and a list of row/column spans.  It deliberately has no
//! terminal dependency: a later adapter owns cursor movement and writes.

use unicode_width::UnicodeWidthChar;

/// One terminal cell in a [`Frame`].
///
/// A double-width glyph occupies a `Glyph` cell followed by a `Continuation`.
/// Keeping the continuation explicit prevents a diff from beginning or ending
/// in the middle of a wide glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// Nothing has been drawn at this column.
    Empty,
    /// A visible scalar and any ANSI escape sequences immediately preceding it.
    Glyph { text: String, width: u8 },
    /// The second column of the preceding double-width [`Cell::Glyph`].
    Continuation,
}

impl Cell {
    #[coverage(off)]
    fn width(&self) -> usize {
        match self {
            Self::Glyph { width, .. } => usize::from(*width),
            Self::Empty | Self::Continuation => 1,
        }
    }
}

/// A rectangular, display-column based terminal frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl Frame {
    /// Builds a grid of `width` columns and `height` rows from view lines.
    ///
    /// ANSI escape sequences consume no columns.  A glyph which would extend
    /// beyond the right edge is omitted as a whole, never split across cells.
    #[must_use]
    #[coverage(off)]
    pub fn from_lines<I, S>(width: usize, height: usize, lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut frame = Self {
            width,
            height,
            cells: vec![Cell::Empty; width.saturating_mul(height)],
        };
        for (row, line) in lines.into_iter().take(height).enumerate() {
            frame.set_line(row, line.as_ref());
        }
        frame
    }

    /// Number of display columns.
    #[must_use]
    #[coverage(off)]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Number of display rows.
    #[must_use]
    #[coverage(off)]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// The cell at `row`, `column`, if it belongs to this frame.
    #[must_use]
    #[coverage(off)]
    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        (row < self.height && column < self.width).then(|| &self.cells[row * self.width + column])
    }

    #[coverage(off)]
    fn set_line(&mut self, row: usize, line: &str) {
        if self.width == 0 {
            return;
        }
        let mut column = 0;
        let mut pending_ansi = String::new();
        let mut last_glyph = None;
        let chars = line.chars().collect::<Vec<_>>();
        let mut index = 0;
        while index < chars.len() {
            if chars[index] == '\u{1b}' {
                let (sequence, consumed) = ansi_sequence(&chars[index..]);
                pending_ansi.push_str(&sequence);
                index += consumed;
                continue;
            }

            let glyph = chars[index];
            let glyph_width = UnicodeWidthChar::width(glyph).unwrap_or(0);
            index += 1;
            if glyph_width == 0 {
                if let Some(last_glyph) = last_glyph
                    && let Cell::Glyph { text, .. } = &mut self.cells[last_glyph]
                {
                    text.push(glyph);
                } else {
                    pending_ansi.push(glyph);
                }
                continue;
            }
            if glyph_width > self.width.saturating_sub(column) {
                break;
            }
            let cell_index = row * self.width + column;
            let mut text = std::mem::take(&mut pending_ansi);
            text.push(glyph);
            self.cells[cell_index] = Cell::Glyph {
                text,
                width: u8::try_from(glyph_width).expect("unicode display width fits in u8"),
            };
            for offset in 1..glyph_width {
                self.cells[cell_index + offset] = Cell::Continuation;
            }
            last_glyph = Some(cell_index);
            column += glyph_width;
        }
        if let Some(last_glyph) = last_glyph.filter(|_| !pending_ansi.is_empty())
            && let Cell::Glyph { text, .. } = &mut self.cells[last_glyph]
        {
            text.push_str(&pending_ansi);
        }
    }

    #[coverage(off)]
    fn glyph_start(&self, row: usize, column: usize) -> usize {
        let mut column = column;
        while column > 0 && matches!(self.cell(row, column), Some(Cell::Continuation)) {
            column -= 1;
        }
        column
    }

    #[coverage(off)]
    fn glyph_end(&self, row: usize, column: usize) -> usize {
        let start = self.glyph_start(row, column);
        start + self.cell(row, start).map_or(1, Cell::width)
    }

    #[coverage(off)]
    fn span_text(&self, row: usize, start: usize, end: usize) -> String {
        let mut text = String::new();
        for column in start..end {
            match self.cell(row, column).expect("span is inside frame") {
                Cell::Empty => text.push(' '),
                Cell::Glyph { text: glyph, .. } => text.push_str(glyph),
                Cell::Continuation => {}
            }
        }
        text
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
    #[coverage(off)]
    pub const fn new() -> Self {
        Self {
            previous: None,
            reset_pending: false,
        }
    }

    /// Invalidates the surface while preserving no terminal-specific state.
    /// The next [`Self::render`] clears the surface and repaints every row.
    #[coverage(off)]
    pub fn reset_surface(&mut self) {
        self.reset_pending = true;
    }

    /// Diffs `next` against the previous frame and remembers it as the base.
    /// A changed geometry is a resize: it discards the base and returns a full
    /// surface clear followed by complete-row spans.
    #[must_use]
    #[coverage(off)]
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
        self.previous = Some(next);
        FrameDiff {
            clear_surface: full_repaint,
            spans,
        }
    }
}

#[coverage(off)]
fn full_spans(frame: &Frame) -> Vec<Span> {
    (0..frame.height)
        .map(|row| Span {
            row,
            column: 0,
            text: frame.span_text(row, 0, frame.width),
        })
        .collect()
}

#[coverage(off)]
fn diff_spans(previous: &Frame, next: &Frame) -> Vec<Span> {
    let mut spans = Vec::new();
    for row in 0..next.height {
        let mut changed = (0..next.width)
            .map(|column| previous.cell(row, column) != next.cell(row, column))
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

#[coverage(off)]
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

#[coverage(off)]
fn ansi_sequence(chars: &[char]) -> (String, usize) {
    if chars.len() < 2 || chars[1] != '[' {
        return (chars[0].to_string(), 1);
    }
    for (index, character) in chars.iter().enumerate().skip(2) {
        if ('\u{40}'..='\u{7e}').contains(character) {
            return (chars[..=index].iter().collect(), index + 1);
        }
    }
    (chars.iter().collect(), chars.len())
}

#[cfg(test)]
mod tests {
    use super::{Cell, Frame, FrameRenderer, Span};

    fn frame(width: usize, height: usize, lines: &[&str]) -> Frame {
        Frame::from_lines(width, height, lines)
    }

    #[test]
    fn golden_frame_uses_display_columns_and_never_splits_wide_glyphs() {
        let rendered = frame(5, 2, &["A\u{1b}[31mあ\u{1b}[0mB", "界x"]);
        assert_eq!(
            rendered.cell(0, 0),
            Some(&Cell::Glyph {
                text: "A".into(),
                width: 1
            })
        );
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
    fn frame_handles_empty_geometry_combining_marks_and_malformed_ansi() {
        let empty = frame(0, 2, &["ignored"]);
        assert_eq!(empty.width(), 0);
        assert_eq!(empty.height(), 2);
        assert_eq!(empty.cell(0, 0), None);

        let combining = frame(2, 1, &["e\u{301}x"]);
        assert!(matches!(
            combining.cell(0, 0),
            Some(Cell::Glyph { text, width: 1 }) if text == "e\u{301}"
        ));
        let leading_combining = frame(2, 1, &["\u{301}x"]);
        assert!(matches!(
            leading_combining.cell(0, 0),
            Some(Cell::Glyph { text, width: 1 }) if text == "\u{301}x"
        ));

        let malformed = frame(2, 1, &["\u{1b}X"]);
        assert!(matches!(
            malformed.cell(0, 0),
            Some(Cell::Glyph { text, width: 1 }) if text == "\u{1b}X"
        ));
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
                text: "Z".into(),
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
                text: "   ".into(),
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
                text: "語".into(),
            }]
        );
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
                text: "wide".into()
            }]
        );
    }
}
