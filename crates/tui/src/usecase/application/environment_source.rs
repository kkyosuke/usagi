//! Shared multiline environment-source editor used by Config and Home overlays.

use usagi_core::domain::settings::{EnvBindings, is_valid_env_name, validate_env_limits};

/// Terminal-independent editing state for `NAME=value` source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentSourceEditor {
    value: String,
    cursor: usize,
    save_focused: bool,
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
    }

    pub fn focus_source(&mut self) {
        self.save_focused = false;
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
    }

    pub fn move_edge(&mut self, end: bool) {
        if self.save_focused {
            return;
        }
        self.cursor = if end { self.value.len() } else { 0 };
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
        assert_eq!(editor.value(), "A=1");
    }
}
