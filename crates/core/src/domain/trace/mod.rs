//! Operation trace events — the activity log.
//!
//! Beyond the error log ([`crate::infrastructure::error_log`]), which records
//! only failures, usagi can record a structured **trace** of the operations a
//! user (or an agent) drives — CLI commands, TUI key presses, session
//! create / remove, and MCP tool calls — so a whole session's activity can be
//! analysed after the fact. Each operation is a [`TraceEvent`]: a plain entity
//! with no IO, meant to be serialized one-per-line as JSON (JSONL) by a future
//! log store next to the daily error log under `<data dir>/logs/`.
//!
//! Tracing is opt-in (off by default) so the hot paths it sits on — every TUI
//! key press, every MCP call — cost nothing unless explicitly enabled.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which surface produced a trace event. Serialized lowercase
/// (`"cli"` / `"tui"` / `"session"` / `"mcp"`) so a JSONL line groups cleanly by
/// `category` when analysed (e.g. with `jq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceCategory {
    /// A `usagi <subcommand>` invocation (recorded in `main`).
    Cli,
    /// A key press handled by the home (workspace) screen's event loop.
    Tui,
    /// A session lifecycle operation (create / remove).
    Session,
    /// An MCP tool call dispatched by the `usagi mcp` server.
    Mcp,
}

/// A single recorded operation: when it happened, which surface produced it, the
/// `action` (e.g. a command name, `"key"`, `"create"`, or an MCP tool name), and
/// an optional `detail` (the command outcome, the pressed key, the session
/// name…). The shape is deliberately flat so a JSONL line is easy to filter and
/// aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    /// When the operation was recorded.
    pub recorded_at: DateTime<Utc>,
    /// The surface that produced the event.
    pub category: TraceCategory,
    /// The operation name within the category.
    pub action: String,
    /// Free-form specifics for the action (the outcome, the key, a name…).
    /// Omitted from the JSON line when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl TraceEvent {
    /// Record `action` in `category` as having happened now, with no detail.
    #[must_use]
    pub fn now(category: TraceCategory, action: impl Into<String>) -> Self {
        Self {
            recorded_at: Utc::now(),
            category,
            action: action.into(),
            detail: None,
        }
    }

    /// Attach `detail` (builder style), e.g. the command outcome or pressed key.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests;
