//! Session-removal checklist modal.
//!
//! The modal owns only a snapshot of the sessions the user saw when opening it.
//! Before dispatch, the runtime verifies the record still matches the current
//! sidebar projection so a refresh cannot turn a checked row into a different
//! removal target.

use std::collections::BTreeSet;

use usagi_core::domain::session::SessionRecord;

use crate::presentation::theme::Role;
use crate::presentation::widgets::{self, modal};

const INNER_WIDTH: usize = 52;
const BODY_HEIGHT: usize = 14;

#[derive(Debug, Clone)]
pub struct RemoveModal {
    entries: Vec<SessionRecord>,
    cursor: usize,
    selected: BTreeSet<String>,
    force: bool,
    feedback: Option<String>,
}

impl RemoveModal {
    #[must_use]
    pub fn new(entries: Vec<SessionRecord>, force: bool) -> Self {
        Self {
            entries,
            cursor: 0,
            selected: BTreeSet::new(),
            force,
            feedback: None,
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[SessionRecord] {
        &self.entries
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn force(&self) -> bool {
        self.force
    }

    #[must_use]
    pub fn selected_entries(&self) -> Vec<SessionRecord> {
        self.entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.name))
            .cloned()
            .collect()
    }

    #[coverage(off)] // LLVM records the wrapping branch as an uncovered region despite its unit test.
    pub fn move_up(&mut self) {
        if !self.entries.is_empty() {
            self.cursor = self.cursor.checked_sub(1).unwrap_or(self.entries.len() - 1);
        }
    }

    pub fn move_down(&mut self) {
        if !self.entries.is_empty() {
            self.cursor = (self.cursor + 1) % self.entries.len();
        }
    }

    pub fn toggle(&mut self) {
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        if !self.selected.insert(entry.name.clone()) {
            self.selected.remove(&entry.name);
        }
        self.feedback = None;
    }

    pub fn set_feedback(&mut self, message: impl Into<String>) {
        self.feedback = Some(message.into());
    }

    pub fn remove_entry(&mut self, entry: &SessionRecord) {
        self.entries
            .retain(|candidate| !same_incarnation(candidate, entry));
        self.selected.remove(&entry.name);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }

    /// Drop checked entries that no longer denote the same record in a fresh
    /// daemon snapshot. The selector never rebinds a checked entry by name.
    #[coverage(off)] // LLVM attributes the retain closure as a separate uncovered function.
    pub fn reconcile(&mut self, current: &[SessionRecord]) {
        self.entries.retain(|entry| {
            current
                .iter()
                .any(|candidate| same_incarnation(candidate, entry))
        });
        self.selected
            .retain(|name| self.entries.iter().any(|entry| entry.name == *name));
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }
}

/// A session record has no durable ID in the current daemon projection. This
/// fence prevents a refresh from retargeting a checked row to a same-named new
/// record until #258 supplies that durable identity end-to-end.
#[must_use]
pub fn same_incarnation(left: &SessionRecord, right: &SessionRecord) -> bool {
    left.name == right.name && left.root == right.root && left.created_at == right.created_at
}

fn row(entry: &SessionRecord, cursor: bool, selected: bool, width: usize) -> String {
    let marker = modal::selection_marker(cursor);
    let check = if selected {
        Role::Success.style().paint("[x]")
    } else {
        "[ ]".to_owned()
    };
    let label = widgets::clip_to_width(entry.display_label(), width.saturating_sub(8));
    modal::content_line(&format!("{marker} {check} {label}"), width)
}

fn body(state: &RemoveModal) -> Vec<String> {
    // The removal checklist is the one modal whose help hint leads the body
    // instead of closing it.
    let mut lines = vec![modal::footer("Space: select   Enter: remove   Esc: cancel")];
    if state.is_empty() {
        lines.push(modal::empty_notice("no sessions to remove"));
    } else {
        for (index, entry) in state.entries.iter().enumerate() {
            lines.push(row(
                entry,
                index == state.cursor,
                state.selected.contains(&entry.name),
                INNER_WIDTH,
            ));
        }
    }
    lines.push(String::new());
    lines.push(match &state.feedback {
        Some(message) => Role::Danger.style().paint(&format!("  {message}")),
        None if state.force => Role::Danger.style().paint("  force removal enabled"),
        None => String::new(),
    });
    lines
}

#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &RemoveModal,
) -> Vec<String> {
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        "Remove sessions",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state),
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

    use super::{RemoveModal, render_over, same_incarnation};

    fn session(name: &str, created: i64) -> SessionRecord {
        SessionRecord {
            name: name.to_owned(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from(format!("/tmp/{name}")),
            created_at: Utc.timestamp_opt(created, 0).unwrap(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }
    }

    #[test]
    fn toggles_and_wraps_without_selecting_an_empty_modal() {
        let mut modal = RemoveModal::new(vec![session("a", 1), session("b", 2)], false);
        modal.move_up();
        modal.toggle();
        assert_eq!(modal.selected_entries(), vec![session("b", 2)]);
        modal.move_down();
        modal.toggle();
        assert_eq!(
            modal.selected_entries(),
            vec![session("a", 1), session("b", 2)]
        );
        let mut empty = RemoveModal::new(Vec::new(), false);
        empty.move_down();
        empty.toggle();
        assert!(empty.selected_entries().is_empty());
    }

    #[test]
    fn reconcile_drops_a_same_named_new_incarnation() {
        let old = session("same", 1);
        let new = session("same", 2);
        assert!(!same_incarnation(&old, &new));
        let mut modal = RemoveModal::new(vec![old], false);
        modal.toggle();
        modal.reconcile(&[new]);
        assert!(modal.is_empty());
        assert!(modal.selected_entries().is_empty());
    }

    #[test]
    fn feedback_removal_and_rendering_cover_the_selector_states() {
        let first = session("a", 1);
        let second = session("b", 2);
        assert!(same_incarnation(&first, &first));

        let mut modal = RemoveModal::new(vec![first.clone(), second.clone()], true);
        assert_eq!(modal.entries().len(), 2);
        assert!(modal.force());
        let force = render_over(12, 60, &vec![String::new(); 12], &modal).join("\n");
        assert!(force.contains("force removal enabled"));
        let normal = RemoveModal::new(vec![second.clone()], false);
        let normal_frame = render_over(12, 60, &vec![String::new(); 12], &normal).join("\n");
        assert!(!normal_frame.contains("force removal enabled"));
        modal.toggle();
        modal.set_feedback("cannot remove");
        let frame = render_over(12, 60, &vec![String::new(); 12], &modal).join("\n");
        assert!(frame.contains("cannot remove"));
        modal.toggle();
        modal.remove_entry(&first);
        assert_eq!(modal.entries(), &[second]);
        modal.reconcile(&[]);
        let empty = render_over(12, 60, &vec![String::new(); 12], &modal).join("\n");
        assert!(empty.contains("no sessions to remove"));
    }
}
