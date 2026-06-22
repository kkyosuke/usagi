//! A reusable, terminal-independent multi-line text buffer.
//!
//! [`TextArea`] is the multi-line sibling of
//! [`TextInput`](super::text_input::TextInput): it owns the typed text as a list
//! of lines and a caret position `(row, col)`, and implements the editing a
//! multi-line field wants — insert at the caret, split a line on Enter, join
//! lines on delete, and move the caret a character / line at a time. The caret
//! column is a byte offset kept on a `char` boundary, so editing is correct for
//! multi-byte text (e.g. Japanese) — moving and deleting step whole characters,
//! never half of one.
//!
//! Keeping it free of any terminal IO makes it directly testable; the renderer
//! reads [`TextArea::lines`] and the caret to draw the buffer and place a caret
//! where editing happens.

/// A multi-line block of editable text with a caret. There is always at least
/// one line, so the caret's `row` always indexes a real line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextArea {
    /// The text, one entry per line (no trailing newlines). Never empty.
    lines: Vec<String>,
    /// Caret line index, always `< lines.len()`.
    row: usize,
    /// Caret byte offset within `lines[row]`, always on a `char` boundary in
    /// `0..=lines[row].len()`.
    col: usize,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    /// An empty area: a single blank line with the caret at its start.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    /// An area pre-filled with `text` (split on `\n`), the caret placed at the
    /// very end (ready to keep typing). Empty text yields a single blank line.
    pub fn from_text(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let row = lines.len() - 1;
        let col = lines[row].len();
        Self { lines, row, col }
    }

    /// The lines, for rendering.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The caret position as `(row, col)` — the line index and the byte offset
    /// (on a `char` boundary) within it, so the renderer can split the cursor
    /// line and draw the caret where editing happens.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// The whole text, lines re-joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Whether the area holds no text at all (a single blank line).
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Insert a character at the caret, advancing it past the inserted char.
    pub fn insert(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Split the current line at the caret, moving the tail onto a new line
    /// below and placing the caret at its start (the `Enter` key).
    pub fn newline(&mut self) {
        let tail = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// Delete the character before the caret. At the start of a line (but not
    /// the first), join it onto the end of the previous line, leaving the caret
    /// at the join. A no-op at the very start of the buffer.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let prev = self.prev_boundary();
            self.lines[self.row].replace_range(prev..self.col, "");
            self.col = prev;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&current);
        }
    }

    /// Delete the character at the caret (the `Del`/forward-delete key). At the
    /// end of a line (but not the last), pull the next line up onto it. A no-op
    /// at the very end of the buffer.
    pub fn delete_forward(&mut self) {
        if self.col < self.lines[self.row].len() {
            let next = self.next_boundary();
            self.lines[self.row].replace_range(self.col..next, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Move the caret one character left, wrapping to the end of the previous
    /// line at the start of a line.
    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col = self.prev_boundary();
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    /// Move the caret one character right, wrapping to the start of the next
    /// line at the end of a line.
    pub fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col = self.next_boundary();
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Move the caret up one line, keeping the column where the shorter line
    /// allows (clamped to a `char` boundary). A no-op on the first line.
    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.clamp_col();
        }
    }

    /// Move the caret down one line, keeping the column where the shorter line
    /// allows (clamped to a `char` boundary). A no-op on the last line.
    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.clamp_col();
        }
    }

    /// Move the caret to the start of the current line.
    pub fn move_home(&mut self) {
        self.col = 0;
    }

    /// Move the caret to the end of the current line.
    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Clamp the caret column to the current line, snapped back to the nearest
    /// `char` boundary at or before it — used after a vertical move onto a line
    /// shorter than (or splitting a multi-byte char at) the old column.
    fn clamp_col(&self) -> usize {
        let line = &self.lines[self.row];
        let mut col = self.col.min(line.len());
        // Floor to a char boundary so the caret never lands mid-character.
        while !line.is_char_boundary(col) {
            col -= 1;
        }
        col
    }

    /// Byte offset of the `char` boundary just before the caret on its line.
    fn prev_boundary(&self) -> usize {
        self.lines[self.row][..self.col]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// Byte offset of the `char` boundary just after the caret on its line.
    fn next_boundary(&self) -> usize {
        self.lines[self.row][self.col..]
            .chars()
            .next()
            .map_or(self.col, |c| self.col + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_area_is_one_blank_line_with_caret_at_start() {
        let area = TextArea::new();
        assert!(area.is_empty());
        assert_eq!(area.lines(), &[String::new()]);
        assert_eq!(area.cursor(), (0, 0));
        assert_eq!(area.text(), "");
        // `default()` delegates to `new()`.
        assert_eq!(TextArea::default(), area);
    }

    #[test]
    fn from_text_splits_on_newlines_and_parks_caret_at_the_end() {
        let area = TextArea::from_text("ab\ncd");
        assert_eq!(area.lines(), &["ab".to_string(), "cd".to_string()]);
        assert_eq!(area.cursor(), (1, 2));
        assert!(!area.is_empty());
        // Empty text is a single blank line.
        assert!(TextArea::from_text("").is_empty());
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut area = TextArea::new();
        area.insert('a');
        area.insert('c');
        area.move_left();
        area.insert('b');
        assert_eq!(area.text(), "abc");
        assert_eq!(area.cursor(), (0, 2));
    }

    #[test]
    fn newline_splits_the_line_at_the_caret() {
        let mut area = TextArea::from_text("abcd");
        area.move_home();
        area.move_right();
        area.move_right();
        area.newline();
        assert_eq!(area.lines(), &["ab".to_string(), "cd".to_string()]);
        assert_eq!(area.cursor(), (1, 0));
        assert_eq!(area.text(), "ab\ncd");
    }

    #[test]
    fn backspace_deletes_within_a_line_and_joins_at_the_start() {
        let mut area = TextArea::from_text("ab\ncd");
        // Caret at end of "cd": delete 'd'.
        area.backspace();
        assert_eq!(area.text(), "ab\nc");
        // Move to the start of the second line and backspace to join.
        area.move_home();
        area.backspace();
        assert_eq!(area.lines(), &["abc".to_string()]);
        assert_eq!(area.cursor(), (0, 2));
        // Backspace at the very start of the buffer is a no-op.
        area.move_home();
        area.backspace();
        assert_eq!(area.text(), "abc");
        assert_eq!(area.cursor(), (0, 0));
    }

    #[test]
    fn delete_forward_removes_at_the_caret_and_pulls_up_at_the_end() {
        let mut area = TextArea::from_text("ab\ncd");
        area.move_up(); // caret clamps to end of "ab" (col 2 == len)
        assert_eq!(area.cursor(), (0, 2));
        // At the end of line 0: delete_forward joins "cd" up.
        area.delete_forward();
        assert_eq!(area.lines(), &["abcd".to_string()]);
        // Now delete the char at the caret ('c').
        area.delete_forward();
        assert_eq!(area.text(), "abd");
        // At the very end of the buffer: a no-op.
        area.move_end();
        area.delete_forward();
        assert_eq!(area.text(), "abd");
    }

    #[test]
    fn horizontal_movement_wraps_across_lines() {
        let mut area = TextArea::from_text("ab\ncd");
        area.move_home(); // (1, 0)
        area.move_left(); // wraps to end of line 0
        assert_eq!(area.cursor(), (0, 2));
        area.move_right(); // wraps to start of line 1
        assert_eq!(area.cursor(), (1, 0));
        // Right at the very end is a no-op; left at the very start is a no-op.
        area.move_end();
        area.move_right();
        assert_eq!(area.cursor(), (1, 2));
        area.move_up();
        area.move_home();
        area.move_left();
        assert_eq!(area.cursor(), (0, 0));
    }

    #[test]
    fn vertical_movement_clamps_the_column() {
        let mut area = TextArea::from_text("long line\nx");
        area.move_up(); // onto "long line", keeping col 1 (length of "x")
        assert_eq!(area.cursor(), (0, 1));
        // Move to a far column, then down onto the short line: clamps to its end.
        area.move_end();
        area.move_down();
        assert_eq!(area.cursor(), (1, 1));
        // Up/down at the edges are no-ops.
        area.move_up();
        area.move_up();
        assert_eq!(area.cursor().0, 0);
        let mut single = TextArea::from_text("only");
        single.move_down();
        assert_eq!(single.cursor(), (0, 4));
    }

    #[test]
    fn editing_steps_whole_multibyte_characters() {
        // Japanese text: caret moves and deletes by whole characters.
        let mut area = TextArea::from_text("あい\nう");
        area.move_up(); // onto "あい", col clamped to the boundary at/under 3
        let (row, col) = area.cursor();
        assert_eq!(row, 0);
        // "う" is 3 bytes, so the old col was 3; "あい" has a boundary exactly at 3.
        assert_eq!(col, 3);
        area.insert('ん'); // between あ and い
        assert_eq!(area.lines()[0], "あんい");
        area.move_home();
        area.backspace(); // start of line, first line: no-op
        assert_eq!(area.lines()[0], "あんい");
        area.move_end();
        area.backspace(); // removes trailing い
        assert_eq!(area.lines()[0], "あん");
    }

    #[test]
    fn clamp_col_floors_into_a_multibyte_char_on_the_line_above() {
        // Column lands inside a multi-byte char on the shorter line above: it
        // floors back to that char's boundary rather than splitting it.
        let mut area = TextArea::from_text("あ\nxyz");
        area.move_end(); // (1, 3)
        area.move_up(); // onto "あ" (len 3): col 3 == len, so end of the line
        assert_eq!(area.cursor(), (0, 3));
        // From column 2 (mid-multibyte if naively applied) it floors to 0.
        area.move_down();
        area.move_home();
        area.move_right();
        area.move_right(); // (1, 2)
        area.move_up(); // onto "あ": col 2 floors to the boundary at 0
        assert_eq!(area.cursor(), (0, 0));
    }
}
