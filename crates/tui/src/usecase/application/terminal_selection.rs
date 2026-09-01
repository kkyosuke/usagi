//! Pure terminal-output selection and clipboard boundary.
//!
//! A selection snapshots the visible, ANSI-free terminal grid at its anchor.
//! This deliberately makes an in-progress drag stable when PTY output arrives,
//! the terminal reconnects, or scrollback advances: copy can never silently
//! return characters from a newer screen.

use unicode_width::UnicodeWidthChar;
use usagi_core::usecase::vt_screen::RetainedRowMotion;

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
    soft_wraps: Vec<bool>,
    /// Current retained-row position for each snapshotted source row. VT
    /// scroll-region operations update this map while `anchor` / `focus` stay
    /// fixed against `viewport`, so copy remains immutable and the highlight
    /// follows the same cells through an Agent redraw.
    visual_rows: Vec<Option<usize>>,
}

impl TerminalSelection {
    /// Starts a selection from the current visible terminal grid.
    #[must_use]
    pub fn begin(viewport: Vec<String>, anchor: TerminalPoint) -> Self {
        Self::begin_with_wraps(viewport, Vec::new(), anchor)
    }

    /// Starts a selection with one terminal auto-wrap marker per physical row.
    /// Missing markers are treated as hard line boundaries.
    #[must_use]
    pub fn begin_with_wraps(
        viewport: Vec<String>,
        soft_wraps: Vec<bool>,
        anchor: TerminalPoint,
    ) -> Self {
        let mapped_rows = viewport.len().max(anchor.row.saturating_add(1));
        Self {
            anchor,
            focus: anchor,
            viewport,
            soft_wraps,
            visual_rows: (0..mapped_rows).map(Some).collect(),
        }
    }

    /// The immutable drag origin.
    #[must_use]
    pub const fn anchor(&self) -> TerminalPoint {
        self.anchor
    }

    /// The current drag endpoint.
    #[must_use]
    pub const fn focus(&self) -> TerminalPoint {
        self.focus
    }

    /// Extends the selection without reading the live terminal again.
    pub fn extend(&mut self, focus: TerminalPoint) {
        while self.visual_rows.len() <= focus.row {
            self.visual_rows.push(Some(self.visual_rows.len()));
        }
        self.focus = focus;
    }

    /// Extends from a pointer position in the current retained coordinate
    /// space, translating it back to the immutable snapshot row when prior VT
    /// output moved that row.
    pub fn extend_visual(&mut self, focus: TerminalPoint) {
        let source_row = self
            .visual_rows
            .iter()
            .position(|row| *row == Some(focus.row))
            .unwrap_or(focus.row);
        self.extend(TerminalPoint {
            row: source_row,
            column: focus.column,
        });
    }

    /// Moves the visual row map through one VT retained-coordinate change.
    /// The snapshotted copy coordinates are deliberately untouched.
    pub fn apply_retained_row_motion(&mut self, motion: RetainedRowMotion) -> bool {
        let (up, top, bottom, count) = match motion {
            RetainedRowMotion::Reset { .. } => return self.invalidate_visual_rows(),
            RetainedRowMotion::Up {
                top, bottom, count, ..
            } => (true, top, bottom, count),
            RetainedRowMotion::Down {
                top, bottom, count, ..
            } => (false, top, bottom, count),
        };
        let count = count.min(bottom.saturating_sub(top).saturating_add(1));
        if count == 0 {
            return false;
        }
        let mut changed = false;
        for visual_row in &mut self.visual_rows {
            let Some(row) = *visual_row else {
                continue;
            };
            if row < top || row > bottom {
                continue;
            }
            let next = if up {
                (row >= top.saturating_add(count)).then(|| row - count)
            } else {
                let first_dropped = bottom.saturating_add(1).saturating_sub(count);
                (row < first_dropped).then(|| row.saturating_add(count))
            };
            changed |= next != *visual_row;
            *visual_row = next;
        }
        changed
    }

    /// Drop only the live highlight mapping after a screen replacement. The
    /// immutable snapshot remains available to the native copy shortcut.
    pub fn invalidate_visual_rows(&mut self) -> bool {
        let changed = self.visual_rows.iter().any(Option::is_some);
        self.visual_rows.fill(None);
        changed
    }

    /// Last current row touched by the selected source range.
    #[must_use]
    pub fn visual_tail(&self) -> Option<usize> {
        let (start, end) = ordered(self.anchor, self.focus);
        (start.row..=end.row)
            .filter_map(|source| self.visual_rows.get(source).copied().flatten())
            .max()
    }

    /// Inclusive selected columns for one current retained row.
    #[must_use]
    pub fn visual_columns_at(&self, current_row: usize) -> Option<(usize, usize)> {
        let (start, end) = ordered(self.anchor, self.focus);
        (start.row..=end.row).find_map(|source| {
            (self.visual_rows.get(source).copied().flatten() == Some(current_row)).then_some({
                (
                    if source == start.row { start.column } else { 0 },
                    if source == end.row {
                        end.column
                    } else {
                        usize::MAX
                    },
                )
            })
        })
    }

    /// Returns selected text, joined by newlines. Endpoints are inclusive so a
    /// click selects the cell under the pointer (or the nearest valid cell).
    #[must_use]
    pub fn text(&self) -> String {
        let (start, end) = ordered(self.anchor, self.focus);
        let rows = (start.row..=end.row)
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
            .collect::<Vec<_>>();
        let mut text = String::new();
        for (offset, row) in rows.iter().enumerate() {
            if offset > 0
                && !self
                    .soft_wraps
                    .get(start.row + offset - 1)
                    .copied()
                    .unwrap_or(false)
            {
                text.push('\n');
            }
            text.push_str(row);
        }
        text
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
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;
    use usagi_core::usecase::vt_screen::{ActiveBuffer, RetainedRowMotion};

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
    fn omits_only_auto_wrap_boundaries_from_copied_text() {
        let mut selection = TerminalSelection::begin_with_wraps(
            vec!["wrapped ".into(), "text    ".into(), "next    ".into()],
            vec![true, false, false],
            TerminalPoint { row: 0, column: 0 },
        );
        selection.extend(TerminalPoint { row: 2, column: 3 });
        assert_eq!(selection.text(), "wrapped text    \nnext");
    }

    #[test]
    fn out_of_range_points_are_safe() {
        let mut selection =
            TerminalSelection::begin(vec!["ok".into()], TerminalPoint { row: 0, column: 99 });
        selection.extend(TerminalPoint { row: 3, column: 0 });
        assert_eq!(selection.text(), "");
    }

    #[test]
    fn visual_rows_follow_agent_scroll_without_changing_copied_text() {
        let mut selection = TerminalSelection::begin(
            vec![
                "header".into(),
                "first".into(),
                "second".into(),
                "composer".into(),
            ],
            TerminalPoint { row: 2, column: 0 },
        );
        selection.extend(TerminalPoint { row: 2, column: 5 });
        assert_eq!(selection.text(), "second");

        assert!(selection.apply_retained_row_motion(RetainedRowMotion::Up {
            buffer: ActiveBuffer::Alternate,
            top: 1,
            bottom: 2,
            count: 1,
        }));

        assert_eq!(selection.text(), "second");
        assert_eq!(selection.visual_tail(), Some(1));
        assert_eq!(selection.visual_columns_at(1), Some((0, 5)));
        assert_eq!(selection.visual_columns_at(2), None);
    }

    #[test]
    fn visual_rows_drop_only_the_part_that_leaves_a_scroll_region() {
        let mut selection = TerminalSelection::begin(
            vec![
                "header".into(),
                "one".into(),
                "two".into(),
                "composer".into(),
            ],
            TerminalPoint { row: 1, column: 1 },
        );
        selection.extend(TerminalPoint { row: 2, column: 1 });

        selection.apply_retained_row_motion(RetainedRowMotion::Up {
            buffer: ActiveBuffer::Alternate,
            top: 1,
            bottom: 2,
            count: 1,
        });

        assert_eq!(selection.visual_columns_at(1), Some((0, 1)));
        assert_eq!(selection.visual_columns_at(2), None);
        assert_eq!(selection.text(), "ne\ntw");
    }

    #[test]
    fn reverse_scroll_moves_surviving_highlights_down() {
        let mut selection = TerminalSelection::begin(
            vec![
                "header".into(),
                "one".into(),
                "two".into(),
                "composer".into(),
            ],
            TerminalPoint { row: 1, column: 0 },
        );
        selection.extend(TerminalPoint { row: 1, column: 2 });

        selection.apply_retained_row_motion(RetainedRowMotion::Down {
            buffer: ActiveBuffer::Alternate,
            top: 1,
            bottom: 2,
            count: 1,
        });

        assert_eq!(selection.visual_columns_at(1), None);
        assert_eq!(selection.visual_columns_at(2), Some((0, 2)));
    }

    #[test]
    fn screen_replacement_drops_only_the_visual_mapping() {
        let mut selection =
            TerminalSelection::begin(vec!["selected".into()], TerminalPoint { row: 0, column: 0 });
        selection.extend(TerminalPoint { row: 0, column: 7 });

        assert!(
            selection.apply_retained_row_motion(RetainedRowMotion::Reset {
                buffer: ActiveBuffer::Primary,
            })
        );

        assert_eq!(selection.text(), "selected");
        assert_eq!(selection.visual_tail(), None);
        assert_eq!(selection.visual_columns_at(0), None);
        assert!(!selection.invalidate_visual_rows());
    }

    #[derive(Default)]
    struct FakeClipboard {
        written: Option<String>,
        error: Option<String>,
    }

    impl ClipboardPort for FakeClipboard {
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
