//! Pure terminal-output selection and clipboard boundary.
//!
//! A selection snapshots the visible, ANSI-free terminal grid at its anchor.
//! This deliberately makes an in-progress drag stable when PTY output arrives,
//! the terminal reconnects, or scrollback advances: copy can never silently
//! return characters from a newer screen.

use unicode_width::UnicodeWidthChar;

/// A location in the visible terminal viewport, measured in terminal columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TerminalPoint {
    pub row: usize,
    pub column: usize,
}

/// A terminal viewport selection. `anchor` is fixed; `focus` changes while a
/// mouse drag or keyboard extension is in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSelection {
    anchor: TerminalPoint,
    focus: TerminalPoint,
    viewport: Vec<String>,
}

impl TerminalSelection {
    /// Starts a selection from the current visible terminal grid.
    #[must_use]
    pub fn begin(viewport: Vec<String>, anchor: TerminalPoint) -> Self {
        Self {
            anchor,
            focus: anchor,
            viewport,
        }
    }

    /// The immutable drag origin.
    #[must_use]
    #[coverage(off)]
    pub const fn anchor(&self) -> TerminalPoint {
        self.anchor
    }

    /// The current drag endpoint.
    #[must_use]
    #[coverage(off)]
    pub const fn focus(&self) -> TerminalPoint {
        self.focus
    }

    /// Extends the selection without reading the live terminal again.
    pub fn extend(&mut self, focus: TerminalPoint) {
        self.focus = focus;
    }

    /// Returns selected text, joined by newlines. Endpoints are inclusive so a
    /// click selects the cell under the pointer (or the nearest valid cell).
    #[must_use]
    pub fn text(&self) -> String {
        let (start, end) = ordered(self.anchor, self.focus);
        (start.row..=end.row)
            .filter_map(|row| self.viewport.get(row).map(|line| (row, line)))
            .map(|(row, line)| {
                let first = if row == start.row { start.column } else { 0 };
                let last = if row == end.row {
                    end.column
                } else {
                    usize::MAX
                };
                extract_columns(line, first, last)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// OS-specific clipboard adapter. Presentation/application code only depends on
/// this small boundary; real process or platform APIs stay in the composition
/// root.
pub trait ClipboardPort {
    /// Replaces the OS clipboard with `text`.
    ///
    /// # Errors
    ///
    /// Returns an adapter-safe message when the clipboard is unavailable.
    fn write_text(&mut self, text: &str) -> Result<(), String>;
}

/// Copies a finished selection through the injected OS boundary.  Empty
/// selections are intentionally rejected so a stale keyboard shortcut cannot
/// erase the user's clipboard.
///
/// # Errors
///
/// Returns an error for an empty selection or when the clipboard adapter fails.
pub fn copy<P: ClipboardPort>(port: &mut P, selection: &TerminalSelection) -> Result<(), String> {
    let text = selection.text();
    if text.is_empty() {
        return Err("no terminal text is selected".to_owned());
    }
    port.write_text(&text)
}

fn ordered(a: TerminalPoint, b: TerminalPoint) -> (TerminalPoint, TerminalPoint) {
    if a <= b { (a, b) } else { (b, a) }
}

fn extract_columns(line: &str, from: usize, to: usize) -> String {
    let mut result = String::new();
    let mut column: usize = 0;
    for character in line.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        let end = column.saturating_add(width);
        if end > from && column <= to {
            result.push(character);
        }
        if column > to {
            break;
        }
        column = end;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_multiple_lines_and_preserves_selected_spaces() {
        let selection = TerminalSelection::begin(
            vec!["hello  ".into(), "  world".into()],
            TerminalPoint { row: 0, column: 3 },
        );
        let mut selection = selection;
        selection.extend(TerminalPoint { row: 1, column: 3 });
        assert_eq!(selection.text(), "lo  \n  wo");
    }

    #[test]
    fn accepts_reverse_drag_and_cjk_display_columns() {
        let mut selection =
            TerminalSelection::begin(vec!["AあB".into()], TerminalPoint { row: 0, column: 3 });
        selection.extend(TerminalPoint { row: 0, column: 1 });
        assert_eq!(selection.text(), "あB");
    }

    #[test]
    fn snapshots_the_viewport_before_output_changes() {
        let mut selection =
            TerminalSelection::begin(vec!["before".into()], TerminalPoint { row: 0, column: 0 });
        selection.extend(TerminalPoint { row: 0, column: 5 });
        assert_eq!(selection.text(), "before");
    }

    #[test]
    fn out_of_range_points_are_safe() {
        let mut selection =
            TerminalSelection::begin(vec!["ok".into()], TerminalPoint { row: 0, column: 99 });
        selection.extend(TerminalPoint { row: 3, column: 0 });
        assert_eq!(selection.text(), "");
    }

    #[derive(Default)]
    struct FakeClipboard {
        written: Option<String>,
        error: Option<String>,
    }

    impl ClipboardPort for FakeClipboard {
        #[coverage(off)]
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            self.written = Some(text.to_owned());
            Ok(())
        }
    }

    #[test]
    fn copies_only_non_empty_selection_through_the_port() {
        let mut selection =
            TerminalSelection::begin(vec!["copy".into()], TerminalPoint { row: 0, column: 0 });
        selection.extend(TerminalPoint { row: 0, column: 3 });
        let mut clipboard = FakeClipboard::default();
        copy(&mut clipboard, &selection).unwrap();
        assert_eq!(clipboard.written.as_deref(), Some("copy"));
    }

    #[test]
    fn does_not_clear_clipboard_for_an_empty_selection() {
        let selection =
            TerminalSelection::begin(vec!["copy".into()], TerminalPoint { row: 0, column: 9 });
        let mut clipboard = FakeClipboard::default();
        assert_eq!(
            copy(&mut clipboard, &selection),
            Err("no terminal text is selected".into())
        );
        assert_eq!(clipboard.written, None);
    }
}
