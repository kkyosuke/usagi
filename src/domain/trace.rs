//! Operation trace events.
//!
//! Beyond the error log ([`crate::infrastructure::error_log`]), which records
//! only failures, usagi can record a structured **trace** of the operations a
//! user (or an agent) drives — CLI commands, TUI key presses, session
//! create / remove, and MCP tool calls — so a whole session's activity can be
//! analysed after the fact. Each operation is a [`TraceEvent`]: a plain entity
//! with no IO, serialized one-per-line as JSON (JSONL) by
//! [`crate::infrastructure::trace_log`], next to the daily error log under
//! `<data dir>/logs/`.
//!
//! Tracing is opt-in (off by default) so the hot paths it sits on — every TUI
//! key press, every MCP call — cost nothing unless explicitly enabled; see
//! [`crate::infrastructure::trace_log::is_enabled`].

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
    pub fn now(category: TraceCategory, action: impl Into<String>) -> Self {
        Self {
            recorded_at: Utc::now(),
            category,
            action: action.into(),
            detail: None,
        }
    }

    /// Attach `detail` (builder style), e.g. the command outcome or pressed key.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_stamps_the_current_time_with_no_detail() {
        let before = Utc::now();
        let event = TraceEvent::now(TraceCategory::Cli, "doctor");
        let after = Utc::now();
        assert_eq!(event.category, TraceCategory::Cli);
        assert_eq!(event.action, "doctor");
        assert_eq!(event.detail, None);
        assert!(event.recorded_at >= before && event.recorded_at <= after);
    }

    #[test]
    fn with_detail_attaches_the_specifics() {
        let event = TraceEvent::now(TraceCategory::Session, "create").with_detail("feature-x");
        assert_eq!(event.detail.as_deref(), Some("feature-x"));
    }

    #[test]
    fn round_trips_through_json() {
        let event = TraceEvent::now(TraceCategory::Mcp, "issue_create").with_detail("ok");
        let line = serde_json::to_string(&event).unwrap();
        let parsed: TraceEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn category_serializes_lowercase() {
        let event = TraceEvent::now(TraceCategory::Tui, "key");
        let value: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["category"], "tui");
    }

    #[test]
    fn absent_detail_is_omitted_from_the_json_line() {
        let event = TraceEvent::now(TraceCategory::Cli, "status");
        let line = serde_json::to_string(&event).unwrap();
        assert!(!line.contains("detail"), "{line}");
    }
}
