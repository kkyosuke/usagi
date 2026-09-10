//! Typed, surface-neutral replies for managed-session observations.
//!
//! The daemon owns the values in these projections. Presentation surfaces
//! deserialize this shared vocabulary instead of reaching into untyped JSON by
//! field name.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentStatus, ProviderResumeProjection, ProviderResumeReason};
use crate::domain::id::{OperationId, SessionId, WorkspaceId, WorktreeId};
use crate::domain::role::RoleId;
use crate::domain::session_lifecycle::{
    AgentPhase, DeletePlan, Failure, ManagedSession, SessionLifecycle, SetupPlan,
};

/// Daemon-authoritative runtime and organization metadata shared by session
/// list, overview, and status replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeObservation {
    pub agent_phase: AgentPhase,
    pub agent_resumable: bool,
    pub agent_resume_reason: ProviderResumeReason,
    pub agent_status: Option<AgentStatus>,
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

/// Client-safe lifecycle fields for one managed session.
///
/// The durable creator identity is deliberately absent from this type, making
/// it impossible for observation serialization to publish authorization data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListLifecycle {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub name: String,
    #[serde(default)]
    pub parent_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<RoleId>,
    pub lifecycle: SessionLifecycle,
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub changed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_plan: Option<SetupPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_plan: Option<DeletePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

impl From<ManagedSession> for SessionListLifecycle {
    fn from(session: ManagedSession) -> Self {
        Self {
            session_id: session.session_id,
            worktree_id: session.worktree_id,
            name: session.name,
            parent_session_id: session.parent_session_id,
            role_id: session.role_id,
            lifecycle: session.lifecycle,
            attempt: session.attempt,
            operation_id: session.operation_id,
            changed_at: session.changed_at,
            setup_plan: session.setup_plan,
            delete_plan: session.delete_plan,
            failure: session.failure,
        }
    }
}

impl From<SessionListLifecycle> for ManagedSession {
    fn from(session: SessionListLifecycle) -> Self {
        Self {
            session_id: session.session_id,
            worktree_id: session.worktree_id,
            name: session.name,
            parent_session_id: session.parent_session_id,
            creator_agent_id: None,
            role_id: session.role_id,
            lifecycle: session.lifecycle,
            attempt: session.attempt,
            operation_id: session.operation_id,
            changed_at: session.changed_at,
            setup_plan: session.setup_plan,
            delete_plan: session.delete_plan,
            failure: session.failure,
        }
    }
}

/// Empty legacy runtime projection. The list-item decoder admits this only
/// when no runtime field is present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AbsentSessionRuntimeObservation {}

/// Runtime metadata is either completely absent (legacy mutation reply) or a
/// complete daemon observation. The untagged wire form remains flat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SessionRuntimeProjection {
    Observed(SessionRuntimeObservation),
    Absent(AbsentSessionRuntimeObservation),
}

impl SessionRuntimeProjection {
    #[must_use]
    pub const fn as_ref(&self) -> Option<&SessionRuntimeObservation> {
        match self {
            Self::Observed(observation) => Some(observation),
            Self::Absent(_) => None,
        }
    }
}

impl From<Option<SessionRuntimeObservation>> for SessionRuntimeProjection {
    fn from(observation: Option<SessionRuntimeObservation>) -> Self {
        match observation {
            Some(observation) => Self::Observed(observation),
            None => Self::Absent(AbsentSessionRuntimeObservation {}),
        }
    }
}

/// One managed lifecycle row together with its current display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionListItem {
    #[serde(flatten)]
    pub session: SessionListLifecycle,
    #[serde(default)]
    pub role_summary: Option<String>,
    #[serde(flatten)]
    pub runtime: SessionRuntimeProjection,
}

const RUNTIME_FIELDS: [&str; 7] = [
    "agent_phase",
    "agent_resumable",
    "agent_resume_reason",
    "agent_status",
    "parent_session_name",
    "organization_depth",
    "organization_path",
];

impl<'de> Deserialize<'de> for SessionListItem {
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=core expires=2027-01-31 tests=list_snapshot_accepts_absent_runtime_but_rejects_partial_or_malformed_metadata
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        decode_session_list_item(serde_json::Value::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

fn decode_session_list_item(value: serde_json::Value) -> Result<SessionListItem, &'static str> {
    let object = value
        .as_object()
        .ok_or("session list item must be an object")?;
    // A create/remove reply from an older or pre-observation path may omit
    // every runtime field. Once any runtime field is present, decode the
    // complete typed observation and reject partial or malformed metadata.
    let observed_fields = RUNTIME_FIELDS
        .iter()
        .filter(|field| object.contains_key(**field))
        .count();
    let runtime = if observed_fields == RUNTIME_FIELDS.len() {
        Some(
            serde_json::from_value::<SessionRuntimeObservation>(value.clone())
                .map_err(|_| "session runtime observation is malformed")?,
        )
    } else if observed_fields == 0 {
        None
    } else {
        return Err("session runtime observation is incomplete");
    };
    let role_summary = object
        .get("role_summary")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| "session role summary is malformed")?
        .flatten();
    let session = serde_json::from_value(value).map_err(|_| "managed session is malformed")?;
    Ok(SessionListItem {
        session,
        role_summary,
        runtime: runtime.into(),
    })
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
    #[serde(default)]
    pub role_id: Option<RoleId>,
    #[serde(default)]
    pub role_summary: Option<String>,
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
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
    use crate::domain::id::{AgentId, OperationId};

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
        let mut session =
            ManagedSession::new_creating("child".to_owned(), OperationId::new(), Utc::now());
        session.creator_agent_id = Some(AgentId::new());
        let snapshot = SessionListSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: WorktreeId::new(),
            revision: 7,
            sessions: vec![SessionListItem {
                session: session.into(),
                role_summary: Some("Review changes".to_owned()),
                runtime: Some(runtime()).into(),
            }],
        };

        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["sessions"][0]["agent_phase"], "interrupted");
        assert_eq!(encoded["sessions"][0]["organization_depth"], 2);
        assert!(encoded["sessions"][0].get("creator_agent_id").is_none());
        let decoded = serde_json::from_value::<SessionListSnapshot>(encoded).unwrap();
        assert_eq!(decoded, snapshot);
        assert!(
            ManagedSession::from(decoded.sessions[0].session.clone())
                .creator_agent_id
                .is_none()
        );

        let mut runtime = runtime();
        runtime.agent_status = None;
        runtime.parent_session_name = None;
        let encoded = serde_json::to_value(SessionListItem {
            session: ManagedSession::new_creating(
                "root-child".to_owned(),
                OperationId::new(),
                Utc::now(),
            )
            .into(),
            role_summary: None,
            runtime: Some(runtime).into(),
        })
        .unwrap();
        for field in [
            "parent_session_id",
            "role_summary",
            "agent_status",
            "parent_session_name",
        ] {
            assert!(
                encoded
                    .as_object()
                    .is_some_and(|object| object.get(field) == Some(&serde_json::Value::Null)),
                "{field} must remain explicit null"
            );
        }
    }

    #[test]
    fn list_snapshot_accepts_absent_runtime_but_rejects_partial_or_malformed_metadata() {
        let session =
            ManagedSession::new_creating("child".to_owned(), OperationId::new(), Utc::now());
        let mut value = serde_json::to_value(SessionListItem {
            session: session.into(),
            role_summary: None,
            runtime: None.into(),
        })
        .unwrap();
        assert_eq!(
            serde_json::from_value::<SessionListItem>(value.clone())
                .unwrap()
                .runtime
                .as_ref(),
            None
        );
        value["agent_phase"] = serde_json::json!("unknown");
        assert!(serde_json::from_value::<SessionListItem>(value).is_err());

        let session =
            ManagedSession::new_creating("child".to_owned(), OperationId::new(), Utc::now());
        let mut partial = serde_json::to_value(SessionListItem {
            session: session.into(),
            role_summary: None,
            runtime: None.into(),
        })
        .unwrap();
        partial["agent_phase"] = serde_json::json!("running");
        assert!(serde_json::from_value::<SessionListItem>(partial).is_err());

        let mut missing_nullable = serde_json::to_value(SessionListItem {
            session: ManagedSession::adopt_available("child".to_owned(), Utc::now()).into(),
            role_summary: None,
            runtime: Some(runtime()).into(),
        })
        .unwrap();
        missing_nullable
            .as_object_mut()
            .unwrap()
            .remove("agent_status");
        assert!(serde_json::from_value::<SessionListItem>(missing_nullable).is_err());

        assert!(serde_json::from_value::<SessionListItem>(serde_json::Value::Null).is_err());
        let session = ManagedSession::adopt_available("child".to_owned(), Utc::now());
        let mut malformed_role = serde_json::to_value(SessionListItem {
            session: session.clone().into(),
            role_summary: None,
            runtime: None.into(),
        })
        .unwrap();
        malformed_role["role_summary"] = serde_json::json!(7);
        assert!(serde_json::from_value::<SessionListItem>(malformed_role).is_err());
        let mut malformed_session = serde_json::to_value(SessionListItem {
            session: session.into(),
            role_summary: None,
            runtime: None.into(),
        })
        .unwrap();
        malformed_session
            .as_object_mut()
            .unwrap()
            .remove("session_id");
        assert!(serde_json::from_value::<SessionListItem>(malformed_session).is_err());
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
        let item = encoded["sessions"][0].as_object().unwrap();
        for field in ["role_id", "role_summary", "parent_session_id"] {
            assert_eq!(item.get(field), Some(&serde_json::Value::Null));
        }
        assert_eq!(
            serde_json::from_value::<SessionStatusSnapshot>(encoded).unwrap(),
            snapshot
        );
    }
}
