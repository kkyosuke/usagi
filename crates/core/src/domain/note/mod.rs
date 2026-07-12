//! A session's note scratchpad: a free-form note plus a lightweight checklist
//! and an append-only decision log.
//!
//! Unlike an [`Issue`](crate::domain::issue::Issue) in the git-tracked store,
//! everything here is throwaway, machine-local scratch space for the current
//! session's work — display / UX only, never affecting the session's identity or
//! branches. The three sections travel together as one [`Scratchpad`]:
//!
//! - a free-form multi-line `note`,
//! - `todos` — a checklist of [`SessionTodo`] items,
//! - `decisions` — an append-only log of [`SessionDecision`] entries where an
//!   agent records *why* it chose an approach.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `true` when a boolean is its `false` default, so a not-yet-done todo omits the
/// `done` key from `state.json`.
// serde's `skip_serializing_if` hands the predicate `&field`, so the reference is
// required by that contract despite `bool` being trivially copyable.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// One entry in a session's lightweight checklist.
///
/// A throwaway, machine-local reminder for the current session's work — distinct
/// from a git-tracked issue in the store (which has status / priority /
/// dependencies).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionTodo {
    /// The todo text. Trimmed and required non-empty by the usecase that writes it.
    pub text: String,
    /// Whether the todo is checked off. `false` (the default) is omitted from
    /// `state.json`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
}

impl SessionTodo {
    /// A fresh, unchecked todo with the given text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            done: false,
        }
    }
}

/// One append-only entry in a session's decision log: an agent records *why* it
/// chose an approach, timestamped, so a coordinator can follow a session's
/// reasoning without replaying its whole transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDecision {
    /// When the decision was recorded (UTC). Supplied by the caller so the
    /// usecase stays clock-free and testable.
    pub at: DateTime<Utc>,
    /// What was decided and why. Trimmed and required non-empty by the usecase.
    pub text: String,
}

impl SessionDecision {
    /// A decision recorded `at` the given time with the given text.
    #[must_use]
    pub fn new(at: DateTime<Utc>, text: impl Into<String>) -> Self {
        Self {
            at,
            text: text.into(),
        }
    }
}

/// A session's note scratchpad: the free-form note, the todo checklist, and the
/// decision log, travelling together. Every section is optional/empty by default
/// and omitted from `state.json` when so, keeping an untouched session lean.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Scratchpad {
    /// A free-form, multi-line note. `None` (the default) means none written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The todo checklist. Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<SessionTodo>,
    /// The append-only decision log. Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<SessionDecision>,
}

impl Scratchpad {
    /// Whether nothing has been written: no note, no todos, no decisions. Used to
    /// omit the whole scratchpad from a session record that never used it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.note.is_none() && self.todos.is_empty() && self.decisions.is_empty()
    }

    /// The free-form note, or `None` when none has been written.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The todo checklist (empty when none have been added).
    #[must_use]
    pub fn todos(&self) -> &[SessionTodo] {
        &self.todos
    }

    /// The decision log (empty when none have been recorded).
    #[must_use]
    pub fn decisions(&self) -> &[SessionDecision] {
        &self.decisions
    }
}

#[cfg(test)]
mod tests;
