//! Session-removal checklist modal.
//!
//! Stable selection and sequential dispatch belong to the controller. This
//! view receives only display labels and state flags, so a refresh cannot make
//! presentation-owned row indexes into deletion identities.

use crate::presentation::theme::Role;
use crate::presentation::widgets::{self, modal};

const INNER_WIDTH: usize = 52;
const BODY_HEIGHT: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveEntry {
    pub label: String,
    pub selected: bool,
    pub removing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveModal {
    pub entries: Vec<RemoveEntry>,
    pub cursor: usize,
    pub force: bool,
    pub feedback: Option<String>,
}

fn body(state: &RemoveModal) -> Vec<String> {
    let mut lines = vec![format!(
        "{}{}{}",
        modal::footer("Space: select  ·  "),
        Role::Danger.style().bold().paint("Enter: remove"),
        modal::footer("  ·  Esc: cancel"),
    )];
    if state.entries.is_empty() {
        lines.push(modal::empty_notice("no sessions can be removed"));
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
                let label = widgets::clip_to_width(&entry.label, INNER_WIDTH.saturating_sub(10));
                let label = if entry.selected {
                    Role::Danger.style().paint(&label)
                } else {
                    label
                };
                modal::content_line(&format!("{marker} {check} {label}"), INNER_WIDTH)
            })
            .collect::<Vec<_>>();
        let (start, end) = modal::list_window(rows.len(), state.cursor, 8);
        lines.extend(modal::scroll_window(&rows, start, end));
    }
    lines.push(String::new());
    lines.push(match &state.feedback {
        Some(message) => Role::Warning.style().paint(&format!("  {message}")),
        None if state.force => Role::Danger
            .style()
            .paint("  force removes dirty worktrees and unmerged branches"),
        None => String::new(),
    });
    lines
}

#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    state: &RemoveModal,
) -> Vec<String> {
    modal::render_body_over(
        height,
        width,
        base,
        "Remove sessions",
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
    fn renders_selection_progress_force_warning_and_empty_feedback() {
        let state = RemoveModal {
            entries: vec![
                RemoveEntry {
                    label: "review".to_owned(),
                    selected: true,
                    removing: false,
                },
                RemoveEntry {
                    label: "stack".to_owned(),
                    selected: true,
                    removing: true,
                },
                RemoveEntry {
                    label: "later".to_owned(),
                    selected: false,
                    removing: false,
                },
            ],
            cursor: 1,
            force: true,
            feedback: None,
        };
        let frame = render_over(24, 80, &vec![String::new(); 24], &state);
        let rendered = frame.join("\n");
        assert!(rendered.contains("\u{1b}[1;31mEnter: remove\u{1b}[0m"));
        let text = strip_ansi(&rendered);
        assert_eq!(frame.len(), 24);
        assert!(text.contains("Remove sessions"));
        assert!(text.contains("review"));
        assert!(text.contains("[…]"));
        assert!(text.contains("force removes dirty worktrees"));

        let empty = RemoveModal {
            entries: Vec::new(),
            cursor: 0,
            force: false,
            feedback: Some("removal complete".to_owned()),
        };
        let text = strip_ansi(&render_over(24, 80, &frame, &empty).join("\n"));
        assert!(text.contains("no sessions can be removed"));
        assert!(text.contains("removal complete"));

        let normal = RemoveModal {
            entries: vec![RemoveEntry {
                label: "safe".to_owned(),
                selected: false,
                removing: false,
            }],
            cursor: 0,
            force: false,
            feedback: None,
        };
        let text = strip_ansi(&render_over(24, 80, &frame, &normal).join("\n"));
        assert!(text.contains("safe"));
        assert!(!text.contains("force removes"));
    }
}
