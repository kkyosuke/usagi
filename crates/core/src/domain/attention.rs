//! Read-only, cross-workspace attention summary for the human control plane.
use super::id::{SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};

/// A reason to visit a workspace; idle processes alone are not human blockers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Decision,
    Review,
    Blocked,
    System,
    Running,
}

impl AttentionKind {
    #[must_use]
    pub const fn needs_action(self) -> bool {
        matches!(self, Self::Decision | Self::Review | Self::Blocked)
    }
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Decision => "Your decision",
            Self::Review => "Review ready",
            Self::Blocked => "Stopped / failed",
            Self::System => "System wait",
            Self::Running => "Running",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// Stable within the workspace, independent of display order and text.
    pub key: String,
    pub session: Option<SessionId>,
    pub label: String,
    pub kind: AttentionKind,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAttention {
    pub workspace: WorkspaceId,
    pub items: Vec<AttentionItem>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attention_kinds_distinguish_human_work_from_background_progress() {
        for (kind, action, label) in [
            (AttentionKind::Decision, true, "Your decision"),
            (AttentionKind::Review, true, "Review ready"),
            (AttentionKind::Blocked, true, "Stopped / failed"),
            (AttentionKind::System, false, "System wait"),
            (AttentionKind::Running, false, "Running"),
        ] {
            assert_eq!(kind.needs_action(), action);
            assert_eq!(kind.label(), label);
        }
    }
}
