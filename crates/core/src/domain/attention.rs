//! Read-only, cross-workspace attention summary for the human control plane.
use super::id::{SessionId, UserDecisionId, WorkspaceId};
use super::pr_inventory::PrIdentity;
use serde::{Deserialize, Serialize};

/// Bound the summary below the IPC frame ceiling even with multibyte labels.
pub const ATTENTION_ITEMS_MAX: usize = 512;

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionTarget {
    Result { primary: PrimaryResult },
    Decision { decision_id: UserDecisionId },
    PullRequest { identity: PrIdentity },
}

/// User-facing result attached to an explicitly completed agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryResult {
    Artifact(String),
    PullRequest(PrIdentity),
}

impl PrimaryResult {
    pub fn from_report(result: &super::agent::StructuredResult) -> Option<Self> {
        if let Some(path) = &result.artifact {
            return Self::valid_artifact(path).then(|| Self::Artifact(path.clone()));
        }
        result
            .pr
            .as_deref()
            .and_then(super::pr_inventory::canonicalize)
            .map(Self::PullRequest)
    }

    #[must_use]
    pub fn valid_artifact(path: &str) -> bool {
        path != ".index.json"
            && path != ".gitignore"
            && !path.is_empty()
            && path.len() <= 512
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && super::presentation_text::presentation_text_is_safe(path)
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
    /// Optional for older producers; new snapshots carry the exact human target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AttentionTarget>,
    /// A successful workspace read can still contain an unconfirmed PR status.
    #[serde(default)]
    pub status_unknown: bool,
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
    fn report_result_prefers_the_designated_file_and_validates_legacy_prs() {
        use super::super::agent::StructuredResult;
        let mut report = StructuredResult::default();
        assert_eq!(PrimaryResult::from_report(&report), None);
        report.pr = Some("https://github.com/example/repo/pull/2".into());
        assert!(matches!(
            PrimaryResult::from_report(&report),
            Some(PrimaryResult::PullRequest(_))
        ));
        for path in [
            "../escape",
            "/etc/passwd",
            "a//b",
            ".",
            ".index.json",
            ".gitignore",
            "",
            "a/./b",
            "a\nb",
        ] {
            assert!(!PrimaryResult::valid_artifact(path));
        }
        report.artifact = Some("docs/検証.md".into());
        assert_eq!(
            PrimaryResult::from_report(&report),
            Some(PrimaryResult::Artifact("docs/検証.md".into()))
        );
    }

    #[test]
    fn old_snapshots_remain_readable_and_exact_targets_round_trip() {
        let mut item: AttentionItem = serde_json::from_value(serde_json::json!({
            "key": "legacy", "session": null, "label": "task", "kind": "decision", "reason": "Choose"
        })).unwrap();
        assert!(item.target.is_none());
        assert!(!item.status_unknown);
        assert!(serde_json::to_value(&item).unwrap().get("target").is_none());
        for target in [
            AttentionTarget::Decision {
                decision_id: UserDecisionId::new(),
            },
            AttentionTarget::PullRequest {
                identity: super::super::pr_inventory::canonicalize(
                    "https://github.com/example/repo/pull/2",
                )
                .unwrap(),
            },
        ] {
            item.status_unknown = matches!(&target, AttentionTarget::PullRequest { .. });
            item.kind = if item.status_unknown {
                AttentionKind::System
            } else {
                AttentionKind::Decision
            };
            item.target = Some(target);
            let wire = serde_json::to_value(&item).unwrap();
            assert_eq!(serde_json::from_value::<AttentionItem>(wire).unwrap(), item);
        }
    }
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
