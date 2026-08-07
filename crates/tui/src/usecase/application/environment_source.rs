//! Shared multiline environment-source editor used by Config and Home overlays.

use unicode_width::UnicodeWidthChar;
use usagi_core::domain::settings::{EnvBindings, is_valid_env_name, validate_env_limits};

/// Terminal-independent editing state for `NAME=value` source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentSourceEditor {
    value: String,
    cursor: usize,
    save_focused: bool,
    /// Terminal display column retained while moving through shorter lines.
    vertical_column: Option<usize>,
}

impl EnvironmentSourceEditor {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self {
            value,
            cursor,
            save_focused: false,
            vertical_column: None,
        }
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub const fn is_save_focused(&self) -> bool {
        self.save_focused
    }

    pub fn replace(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
        self.save_focused = false;
        self.vertical_column = None;
    }

    pub fn focus_source(&mut self) {
        self.save_focused = false;
        self.vertical_column = None;
    }

    pub fn toggle_save_focus(&mut self, enabled: bool) {
        if enabled {
            self.save_focused = !self.save_focused;
        }
    }

    pub fn insert(&mut self, text: &str) {
        if self.save_focused {
            return;
        }
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.vertical_column = None;
    }

    pub fn paste(&mut self, text: &str) {
        self.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    pub fn backspace(&mut self) {
        if self.save_focused || self.cursor == 0 {
            return;
        }
        let previous = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.value.drain(previous..self.cursor);
        self.cursor = previous;
        self.vertical_column = None;
    }

    pub fn delete_forward(&mut self) {
        if self.save_focused || self.cursor == self.value.len() {
            return;
        }
        let next = self.value[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |character| self.cursor + character.len_utf8());
        self.value.drain(self.cursor..next);
        self.vertical_column = None;
    }

    pub fn move_cursor(&mut self, forward: bool) {
        if self.save_focused {
            return;
        }
        if forward {
            if let Some(character) = self.value[self.cursor..].chars().next() {
                self.cursor += character.len_utf8();
            }
        } else if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
        self.vertical_column = None;
    }

    /// Move to the previous or next source line while retaining the terminal
    /// display column across shorter intermediate lines.
    pub fn move_vertical(&mut self, down: bool) {
        if self.save_focused {
            return;
        }
        let line_start = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let line_end = self.value[self.cursor..]
            .find('\n')
            .map_or(self.value.len(), |offset| self.cursor + offset);
        let column = display_width(&self.value[line_start..self.cursor]);
        let preferred = self.vertical_column.unwrap_or(column);
        let target = if down {
            if line_end == self.value.len() {
                return;
            }
            let start = line_end + 1;
            let end = self.value[start..]
                .find('\n')
                .map_or(self.value.len(), |offset| start + offset);
            (start, end)
        } else {
            if line_start == 0 {
                return;
            }
            let end = line_start - 1;
            let start = self.value[..end].rfind('\n').map_or(0, |index| index + 1);
            (start, end)
        };
        self.cursor =
            target.0 + byte_offset_at_display_column(&self.value[target.0..target.1], preferred);
        self.vertical_column = Some(preferred);
    }

    pub fn move_edge(&mut self, end: bool) {
        if self.save_focused {
            return;
        }
        self.cursor = if end { self.value.len() } else { 0 };
        self.vertical_column = None;
    }

    /// Parse the current source into validated bindings.
    ///
    /// # Errors
    ///
    /// Returns a display-safe validation message for the first invalid line or
    /// when the resulting bindings exceed the settings limits.
    pub fn parse(&self) -> Result<EnvBindings, String> {
        parse_environment_source(&self.value)
    }
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn byte_offset_at_display_column(value: &str, column: usize) -> usize {
    let mut width = 0;
    for (index, character) in value.char_indices() {
        if width >= column {
            return index;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width.saturating_add(character_width) > column {
            return index;
        }
        width += character_width;
    }
    value.len()
}

/// Parse and validate newline-delimited `NAME=value` source.
///
/// # Errors
///
/// Returns a display-safe validation message for the first invalid line or
/// when the resulting bindings exceed the settings limits.
pub fn parse_environment_source(source: &str) -> Result<EnvBindings, String> {
    let mut bindings = EnvBindings::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected NAME=value", index + 1));
        };
        let name = name.trim();
        let value = value.trim();
        if !is_valid_env_name(name) {
            return Err(format!("line {}: invalid variable name", index + 1));
        }
        if value.is_empty() {
            return Err(format!("line {}: remove the line to unset it", index + 1));
        }
        if value.contains('\0') {
            return Err(format!("line {}: values cannot contain NUL", index + 1));
        }
        bindings.insert(name.to_owned(), value.to_owned());
    }
    validate_env_limits(&bindings).map_err(|error| error.to_string())?;
    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::EnvironmentSourceEditor;

    #[test]
    fn editing_is_unicode_safe_and_shared_validation_runs() {
        let mut editor = EnvironmentSourceEditor::new("A=é");
        editor.backspace();
        editor.insert("1\nB=2");
        editor.delete_forward();
        assert_eq!(editor.parse().unwrap().len(), 2);
    }

    #[test]
    fn save_focus_blocks_source_edits() {
        let mut editor = EnvironmentSourceEditor::new("A=1");
        editor.toggle_save_focus(true);
        editor.insert("x");
        editor.backspace();
        editor.move_vertical(false);
        editor.move_vertical(true);
        assert_eq!(editor.value(), "A=1");
    }

    #[test]
    fn vertical_movement_is_unicode_safe_and_retains_the_preferred_column() {
        let mut editor = EnvironmentSourceEditor::new("ABCD\né\nWXYZ");

        editor.move_vertical(false);
        assert_eq!(editor.cursor(), "ABCD\né".len());
        editor.move_vertical(false);
        assert_eq!(editor.cursor(), "ABCD".len());
        editor.move_vertical(false);
        assert_eq!(editor.cursor(), "ABCD".len());

        editor.move_vertical(true);
        assert_eq!(editor.cursor(), "ABCD\né".len());
        editor.move_vertical(true);
        assert_eq!(editor.cursor(), "ABCD\né\nWXYZ".len());
        editor.move_vertical(true);
        assert_eq!(editor.cursor(), "ABCD\né\nWXYZ".len());

        editor.move_vertical(false);
        editor.move_cursor(false);
        editor.move_vertical(false);
        assert_eq!(
            editor.cursor(),
            0,
            "horizontal movement resets the preferred column"
        );
    }

    #[test]
    fn vertical_movement_retains_the_terminal_column_across_wide_characters() {
        let mut editor = EnvironmentSourceEditor::new("あX\nABCD");

        editor.move_vertical(false);
        editor.move_cursor(false);
        assert_eq!(editor.cursor(), "あ".len());

        editor.move_vertical(true);
        assert_eq!(
            editor.cursor(),
            "あX\nAB".len(),
            "a width-two character must map to two ASCII columns"
        );
        editor.move_vertical(false);
        assert_eq!(editor.cursor(), "あ".len());

        let mut editor = EnvironmentSourceEditor::new("A\nあX");
        editor.move_vertical(false);
        editor.move_cursor(false);
        editor.move_cursor(true);
        editor.move_vertical(true);
        assert_eq!(
            editor.cursor(),
            "A\n".len(),
            "a caret cannot split a width-two terminal cell"
        );
        editor.move_vertical(false);
        assert_eq!(editor.cursor(), "A".len());
    }
}
