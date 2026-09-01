//! Merge-confirmed session cleanup queue.

use crate::presentation::theme::Role;
use crate::presentation::widgets::{self, modal};

const INNER_WIDTH: usize = 58;
const BODY_HEIGHT: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupEntry {
    pub label: String,
    pub merged_prs: usize,
    pub selected: bool,
    pub removing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupModal {
    pub entries: Vec<CleanupEntry>,
    pub cursor: usize,
    pub feedback: Option<String>,
}

fn body(state: &CleanupModal) -> Vec<String> {
    let mut lines = vec![modal::footer(
        "Space: select  ·  a: all  ·  Enter: clean up  ·  Esc",
    )];
    if state.entries.is_empty() {
        lines.push(modal::empty_notice("no merge-confirmed sessions are ready"));
    } else {
        let rows = state
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let marker = modal::selection_marker(index == state.cursor);
                let check = if entry.removing {
                    Role::Feature.style().bold().paint("[…]")
                } else if entry.selected {
                    Role::Danger.style().bold().paint("[x]")
                } else {
                    "[ ]".to_owned()
                };
                let count = if entry.merged_prs == 1 {
                    "1 merged PR".to_owned()
                } else {
                    format!("{} merged PRs", entry.merged_prs)
                };
                let label = widgets::clip_to_width(&entry.label, INNER_WIDTH.saturating_sub(25));
                modal::content_line(&format!("{marker} {check} {label}  {count}"), INNER_WIDTH)
            })
            .collect::<Vec<_>>();
        let (start, end) = modal::list_window(rows.len(), state.cursor, 9);
        lines.extend(modal::scroll_window(&rows, start, end));
    }
    lines.push(String::new());
    lines.push(state.feedback.as_ref().map_or_else(String::new, |message| {
        Role::Warning.style().paint(&format!("  {message}"))
    }));
    lines
}

#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    state: &CleanupModal,
) -> Vec<String> {
    modal::render_body_over(
        height,
        width,
        base,
        "Merged session cleanup",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state),
    )
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

    use super::*;
    use crate::presentation::widgets::strip_ansi;

    #[test]
    fn renders_selection_progress_and_empty_feedback() {
        let state = CleanupModal {
            entries: vec![
                CleanupEntry {
                    label: "review".to_owned(),
                    merged_prs: 1,
                    selected: true,
                    removing: false,
                },
                CleanupEntry {
                    label: "stack".to_owned(),
                    merged_prs: 2,
                    selected: true,
                    removing: true,
                },
                CleanupEntry {
                    label: "later".to_owned(),
                    merged_prs: 3,
                    selected: false,
                    removing: false,
                },
            ],
            cursor: 1,
            feedback: Some("waiting for the current removal".to_owned()),
        };
        let frame = render_over(24, 80, &vec![String::new(); 24], &state);
        let text = strip_ansi(&frame.join("\n"));
        assert_eq!(frame.len(), 24);
        assert!(text.contains("Merged session cleanup"));
        assert!(text.contains("1 merged PR"));
        assert!(text.contains("2 merged PRs"));
        assert!(text.contains("3 merged PRs"));
        assert!(text.contains("[ ]"));
        assert!(text.contains("[…]"));
        assert!(text.contains("waiting for the current removal"));

        let empty = CleanupModal {
            entries: Vec::new(),
            cursor: 0,
            feedback: None,
        };
        assert!(
            strip_ansi(&render_over(24, 80, &frame, &empty).join("\n"))
                .contains("no merge-confirmed sessions are ready")
        );
    }
}
