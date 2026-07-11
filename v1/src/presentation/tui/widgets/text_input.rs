//! A reusable, terminal-independent single-line text input buffer.
//!
//! [`TextInput`] owns the typed text and a caret position, and implements the
//! editing every input field on every screen wants: insert at the caret, delete
//! either side of it, and move it (←/→ a character, Home/End to the edges). The
//! caret is a byte offset kept on a `char` boundary, so editing is correct for
//! multi-byte text (e.g. Japanese) — moving and deleting step whole characters,
//! never half of one.
//!
//! Keeping it free of any terminal IO makes it directly testable and lets every
//! screen share one editing behaviour instead of re-implementing append/pop. The
//! renderer reads [`TextInput::before`] / [`TextInput::after`] to split the line
//! and draw a caret where editing happens.

use console::Key;

/// A single line of editable text with a caret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    /// The typed text.
    value: String,
    /// Caret position as a byte offset into `value`, always on a `char`
    /// boundary in `0..=value.len()`.
    cursor: usize,
}

impl TextInput {
    /// An empty input with the caret at the start.
    pub fn new() -> Self {
        Self::default()
    }

    /// An input pre-filled with `value`, the caret placed at the end (ready to
    /// keep typing).
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    /// The typed text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The caret position as a byte offset on a `char` boundary, so the renderer
    /// can split the line and draw the caret where editing happens.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether nothing has been typed.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The text before the caret (the renderer pairs it with [`Self::after`] to
    /// place a caret glyph between them).
    pub fn before(&self) -> &str {
        &self.value[..self.cursor]
    }

    /// The text from the caret to the end of the line.
    pub fn after(&self) -> &str {
        &self.value[self.cursor..]
    }

    /// Replace the whole value, placing the caret at the end. Used when the value
    /// is set from outside the keyboard (history recall, a derived suggestion, a
    /// path chosen in a picker).
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
    }

    /// Clear the text and reset the caret to the start.
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Insert a character at the caret, advancing the caret past it.
    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the caret, moving the caret back. No-op at the
    /// start of the line. Returns whether anything was deleted.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_boundary();
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// Delete the character at the caret (the `Del`/forward-delete key), leaving
    /// the caret in place. No-op at the end of the line. Returns whether anything
    /// was deleted.
    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.value.len() {
            return false;
        }
        let next = self.next_boundary();
        self.value.replace_range(self.cursor..next, "");
        true
    }

    /// Move the caret one character left.
    pub fn move_left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    /// Move the caret one character right.
    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary();
    }

    /// Move the caret to the start of the line.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the caret to the end of the line.
    pub fn move_end(&mut self) {
        self.cursor = self.value.len();
    }

    /// Apply one key's editing effect, returning whether the key was an editing
    /// key the buffer consumed.
    ///
    /// Consumed: a printable character (inserted at the caret), Backspace, Del,
    /// ←/→, Home/End. Everything else — Enter, Tab, Esc, control characters — is
    /// left for the caller to interpret, so each screen keeps its own meaning for
    /// those keys and only delegates the plain editing here.
    pub fn handle_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Char(c) if !c.is_control() => {
                self.insert(*c);
                true
            }
            Key::Backspace => {
                self.backspace();
                true
            }
            Key::Del => {
                self.delete_forward();
                true
            }
            Key::ArrowLeft => {
                self.move_left();
                true
            }
            Key::ArrowRight => {
                self.move_right();
                true
            }
            Key::Home => {
                self.move_home();
                true
            }
            Key::End => {
                self.move_end();
                true
            }
            _ => false,
        }
    }

    /// Byte offset of the `char` boundary just before the caret (or `0`).
    fn prev_boundary(&self) -> usize {
        self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// Byte offset of the `char` boundary just after the caret (or the end).
    fn next_boundary(&self) -> usize {
        self.value[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_input_is_empty_with_the_caret_at_the_start() {
        let input = TextInput::new();
        assert!(input.is_empty());
        assert_eq!(input.value(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn with_value_places_the_caret_at_the_end() {
        let input = TextInput::with_value("hi");
        assert_eq!(input.value(), "hi");
        assert_eq!(input.cursor(), 2);
        assert!(!input.is_empty());
    }

    #[test]
    fn typing_inserts_at_the_caret_and_advances_it() {
        let mut input = TextInput::new();
        input.insert('a');
        input.insert('c');
        // Move left and insert in the middle.
        input.move_left();
        input.insert('b');
        assert_eq!(input.value(), "abc");
        // Caret sits just after the inserted 'b'.
        assert_eq!(input.before(), "ab");
        assert_eq!(input.after(), "c");
    }

    #[test]
    fn backspace_deletes_before_the_caret() {
        let mut input = TextInput::with_value("abc");
        input.move_left(); // between 'b' and 'c'
        assert!(input.backspace()); // removes 'b'
        assert_eq!(input.value(), "ac");
        assert_eq!(input.before(), "a");
        assert_eq!(input.after(), "c");
    }

    #[test]
    fn backspace_at_the_start_is_a_noop() {
        let mut input = TextInput::with_value("a");
        input.move_home();
        assert!(!input.backspace());
        assert_eq!(input.value(), "a");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn delete_forward_removes_the_character_at_the_caret() {
        let mut input = TextInput::with_value("abc");
        input.move_home();
        assert!(input.delete_forward()); // removes 'a'
        assert_eq!(input.value(), "bc");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn delete_forward_at_the_end_is_a_noop() {
        let mut input = TextInput::with_value("ab");
        assert!(!input.delete_forward());
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn caret_movement_clamps_at_both_edges() {
        let mut input = TextInput::with_value("ab");
        input.move_right(); // already at end
        assert_eq!(input.cursor(), 2);
        input.move_home();
        input.move_left(); // already at start
        assert_eq!(input.cursor(), 0);
        input.move_end();
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn editing_steps_whole_multibyte_characters() {
        // Japanese text: each character is multiple bytes, so the caret must move
        // and delete by whole characters, never landing mid-character.
        let mut input = TextInput::with_value("あい");
        assert_eq!(input.cursor(), 6); // two 3-byte chars
        input.move_left();
        assert_eq!(input.cursor(), 3); // on the boundary between あ and い
        input.insert('ん');
        assert_eq!(input.value(), "あんい");
        input.move_home();
        assert!(!input.backspace()); // at start: no-op
        assert_eq!(input.value(), "あんい");
        input.move_end();
        assert!(input.backspace()); // removes trailing い
        assert_eq!(input.value(), "あん");
    }

    #[test]
    fn set_value_replaces_text_and_parks_the_caret_at_the_end() {
        let mut input = TextInput::with_value("old");
        input.move_home();
        input.set_value("brand new");
        assert_eq!(input.value(), "brand new");
        assert_eq!(input.cursor(), 9);
    }

    #[test]
    fn clear_empties_the_buffer_and_resets_the_caret() {
        let mut input = TextInput::with_value("text");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn handle_key_consumes_editing_keys() {
        let mut input = TextInput::new();
        assert!(input.handle_key(&Key::Char('x')));
        assert!(input.handle_key(&Key::Char('y'))); // "xy", caret at end
        assert!(input.handle_key(&Key::ArrowLeft)); // between x and y
        assert!(input.handle_key(&Key::Backspace)); // removes 'x' → "y", caret at start
        assert!(input.handle_key(&Key::End)); // caret after y
        assert!(input.handle_key(&Key::Home)); // caret before y
        assert!(input.handle_key(&Key::ArrowRight)); // caret after y
        assert!(input.handle_key(&Key::Backspace)); // removes 'y'
        assert!(input.handle_key(&Key::Del)); // empty: consumed, no-op
        assert_eq!(input.value(), "");
    }

    #[test]
    fn handle_key_leaves_non_editing_keys_to_the_caller() {
        let mut input = TextInput::with_value("a");
        // Enter, Tab, Escape, and control characters are not consumed, so the
        // caller keeps their screen-specific meaning.
        assert!(!input.handle_key(&Key::Enter));
        assert!(!input.handle_key(&Key::Tab));
        assert!(!input.handle_key(&Key::Escape));
        assert!(!input.handle_key(&Key::Char('\u{000f}'))); // Ctrl-O
        assert_eq!(input.value(), "a");
    }
}
