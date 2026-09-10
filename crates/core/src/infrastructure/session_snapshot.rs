//! Typed, surface-neutral replies for managed-session observations.
//!
//! The daemon owns the values in these projections. Presentation surfaces
//! deserialize this shared vocabulary instead of reaching into untyped JSON by
//! field name.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentStatus, ProviderResumeProjection, ProviderResumeReason};
use crate::domain::id::{SessionId, WorkspaceId, WorktreeId};
use crate::domain::role::RoleId;
use crate::domain::session_lifecycle::{AgentPhase, ManagedSession, SessionLifecycle};

/// Daemon-authoritative runtime and organization metadata shared by session
/// list, overview, and status replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeObservation {
    pub agent_phase: AgentPhase,
    pub agent_resumable: bool,
    pub agent_resume_reason: ProviderResumeReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_name: Option<String>,
    pub organization_depth: usize,
    pub organization_path: Vec<String>,
}

impl SessionRuntimeObservation {
    /// Convert the runtime observation into the ID-free projection consumed by
    /// TUI state.
    #[must_use]
    pub const fn provider_resume_projection(&self) -> ProviderResumeProjection {
        ProviderResumeProjection {
            interrupted: matches!(
                self.agent_phase,
                AgentPhase::Interrupted | AgentPhase::Sleeping
            ),
            resumable: self.agent_resumable,
            reason: self.agent_resume_reason,
        }
    }
}

/// One managed lifecycle row together with its current display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListItem {
    #[serde(flatten)]
    pub session: ManagedSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_summary: Option<String>,
    #[serde(flatten)]
    pub runtime: Option<SessionRuntimeObservation>,
}

/// Typed reply for the lightweight session list and overview actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListSnapshot {
    pub workspace_id: WorkspaceId,
    pub root_worktree_id: WorktreeId,
    pub revision: u64,
    pub sessions: Vec<SessionListItem>,
}

/// Git state for the single checkout owned by a managed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorktreeStatus {
    pub path: PathBuf,
    pub branch: String,
    pub status: String,
    pub dirty: bool,
    pub merged: bool,
}

/// One detailed status row together with current runtime metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatusItem {
    pub name: String,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<RoleId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_summary: Option<String>,
    pub lifecycle: SessionLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub worktrees: Vec<SessionWorktreeStatus>,
    #[serde(flatten)]
    pub runtime: SessionRuntimeObservation,
}

/// Typed reply for the Git-enriched session status action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatusSnapshot {
    pub workspace_id: WorkspaceId,
    pub revision: u64,
    pub sessions: Vec<SessionStatusItem>,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::domain::id::OperationId;

    fn runtime() -> SessionRuntimeObservation {
        SessionRuntimeObservation {
            agent_phase: AgentPhase::Interrupted,
            agent_resumable: true,
            agent_resume_reason: ProviderResumeReason::ExplicitResumeAvailable,
            agent_status: Some(AgentStatus::Idle),
            parent_session_name: Some("parent".to_owned()),
            organization_depth: 2,
            organization_path: vec![
                "Director".to_owned(),
                "parent".to_owned(),
                "child".to_owned(),
            ],
        }
    }

    #[test]
    fn list_snapshot_round_trips_the_shared_wire_shape() {
        let snapshot = SessionListSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: WorktreeId::new(),
            revision: 7,
            sessions: vec![SessionListItem {
                session: ManagedSession::new_creating(
                    "child".to_owned(),
                    OperationId::new(),
                    Utc::now(),
                ),
                role_summary: Some("Review changes".to_owned()),
                runtime: Some(runtime()),
            }],
        };

        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["sessions"][0]["agent_phase"], "interrupted");
        assert_eq!(encoded["sessions"][0]["organization_depth"], 2);
        assert_eq!(
            serde_json::from_value::<SessionListSnapshot>(encoded).unwrap(),
            snapshot
        );
    }

    #[test]
    fn runtime_observation_projects_provider_resume_without_wire_reparsing() {
        assert_eq!(
            runtime().provider_resume_projection(),
            ProviderResumeProjection {
                interrupted: true,
                resumable: true,
                reason: ProviderResumeReason::ExplicitResumeAvailable,
            }
        );
        let mut active = runtime();
        active.agent_phase = AgentPhase::Running;
        assert!(!active.provider_resume_projection().interrupted);
    }

    #[test]
    fn status_snapshot_round_trips_worktree_and_optional_metadata() {
        let snapshot = SessionStatusSnapshot {
            workspace_id: WorkspaceId::new(),
            revision: 9,
            sessions: vec![SessionStatusItem {
                name: "child".to_owned(),
                session_id: SessionId::new(),
                role_id: None,
                role_summary: None,
                lifecycle: SessionLifecycle::Available,
                parent_session_id: None,
                worktrees: vec![SessionWorktreeStatus {
                    path: PathBuf::from("/work/child"),
                    branch: "usagi/child".to_owned(),
                    status: "local".to_owned(),
                    dirty: false,
                    merged: false,
                }],
                runtime: runtime(),
            }],
        };

        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded["sessions"][0].get("role_id").is_none());
        assert_eq!(
            serde_json::from_value::<SessionStatusSnapshot>(encoded).unwrap(),
            snapshot
        );
    }
}
