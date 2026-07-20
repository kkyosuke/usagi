//! The `SessionRecord` entity: a session created under
//! `<workspace>/.usagi/sessions/<name>/`.
//!
//! A session is a parallel working tree on its own branch. This is the slim
//! *identity* of a session — its name, where it lives, who created it, and when
//! it was last active. The richer state a session accretes (per-worktree git
//! status, PR links, agent CLI overrides, the note scratchpad) is modelled by
//! separate domain entities (e.g. [`crate::domain::pullrequest`],
//! [`crate::domain::note`]) and joined in at the store / usecase layer, so this
//! record stays a stable, dependency-light core.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::note::Scratchpad;
use crate::domain::pullrequest::PrLink;

/// Who launched a session — a person operating the TUI, or an agent driving the
/// MCP server — recorded once when the session is created so a later reader can
/// tell an automated session apart from a hand-made one.
///
/// [`Human`](Self::Human) and [`Mcp`](Self::Mcp) are the two real origins.
/// [`Unknown`](Self::Unknown) is only the degraded reading of a session recorded
/// by an *older* usagi that predates this field (the key is absent), so such a
/// record still loads; an unrecognised stored token degrades to it as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Created interactively by a person from the TUI home screen.
    Human,
    /// Created by an agent through the MCP server.
    Mcp,
    /// Origin not recorded (a session from a `state.json` written before usagi
    /// tracked this, or an unrecognised stored token). Never written for a
    /// session usagi creates itself. `#[serde(other)]` makes it the catch-all, so
    /// it must stay the last variant.
    #[default]
    #[serde(other)]
    Unknown,
}

impl SessionOrigin {
    /// The lowercase token used in `state.json` and MCP tool output
    /// (`"unknown"` / `"human"` / `"mcp"`), matching the `snake_case` serde rename
    /// so the string form has one source of truth.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionOrigin::Unknown => "unknown",
            SessionOrigin::Human => "human",
            SessionOrigin::Mcp => "mcp",
        }
    }

    /// Whether the origin was not recorded (the pre-field default). Used to omit
    /// the field from `state.json` so an untracked session stays lean.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        matches!(self, SessionOrigin::Unknown)
    }
}

impl std::fmt::Display for SessionOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The slim identity of a session created under `.usagi/sessions/<name>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session name (also the branch name created in every repository). This is
    /// the session's identity: commands target it, so it never changes once
    /// created.
    pub name: String,
    /// An optional sidebar label that overrides [`name`](Self::name) in the home
    /// screen's session list, without touching the branch / identity. `None`
    /// (the default, omitted from the file) shows the `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Who launched the session, recorded once at creation and never changed.
    /// Defaults to [`SessionOrigin::Unknown`] for a record written before usagi
    /// tracked this, and that default is omitted from `state.json`.
    #[serde(default, skip_serializing_if = "SessionOrigin::is_unknown")]
    pub origin: SessionOrigin,
    /// The name of the session this one was started from — the parent session the
    /// agent was in when it created this one through the MCP server. `None` when
    /// there is no parent (a human-cut session, or one made at the workspace
    /// root). Recorded once at creation; omitted from the file when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<String>,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
    pub root: PathBuf,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last *touched*: switched to, or observed producing
    /// terminal/agent activity. `None` (the default, omitted from older files)
    /// means it has never been touched since creation, so callers fall back to
    /// [`created_at`](Self::created_at) via
    /// [`last_active_or_created`](Self::last_active_or_created).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<DateTime<Utc>>,
    /// The session's note scratchpad — free-form note, todo checklist, and
    /// decision log (see [`Scratchpad`]). Display / UX only; empty (the default)
    /// is omitted from `state.json`.
    #[serde(default, skip_serializing_if = "Scratchpad::is_empty")]
    pub notes: Scratchpad,
    /// Pull requests discovered for this session — harvested from its panes'
    /// output and persisted so the sidebar keeps showing the `#<number>` badges
    /// across restarts. Empty (the default) is omitted from `state.json`. Rolled
    /// up to the session level via [`PrLink::aggregate`] (per-worktree attribution
    /// arrives with the git layer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrLink>,
    /// The session's environment variables, edited through the Overview `env`
    /// command and persisted so they survive restarts. A stable `name -> value`
    /// map (sorted by name); empty (the default) is omitted from `state.json`.
    /// Display / configuration only — this never changes the session's identity
    /// or branches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
}

impl SessionRecord {
    /// The label shown in the sidebar: the custom
    /// [`display_name`](Self::display_name) when set, otherwise the session
    /// [`name`](Self::name).
    #[must_use]
    pub fn display_label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The reference time for the freshness ("heat") dot: the persisted
    /// [`last_active`](Self::last_active), or [`created_at`](Self::created_at)
    /// when the session has never been touched.
    #[must_use]
    pub fn last_active_or_created(&self) -> DateTime<Utc> {
        self.last_active.unwrap_or(self.created_at)
    }
}

#[cfg(test)]
mod tests;
