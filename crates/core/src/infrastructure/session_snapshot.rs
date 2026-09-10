//! Typed, surface-neutral replies for managed-session observations.
//!
//! The daemon owns the values in these projections. Presentation surfaces
//! deserialize this shared vocabulary instead of reaching into untyped JSON by
//! field name.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
    #[serde(default)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListItem {
    pub session: ManagedSession,
    pub role_summary: Option<String>,
    pub runtime: Option<SessionRuntimeObservation>,
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

impl Serialize for SessionListItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut value = serde_json::to_value(&self.session).map_err(serde::ser::Error::custom)?;
        let object = value.as_object_mut().ok_or_else(|| {
            serde::ser::Error::custom("managed session must serialize as an object")
        })?;
        // Creator identity is authorization metadata and never enters the
        // client-safe observation reply.
        object.remove("creator_agent_id");
        // Preserve the established public wire shape: these optional values
        // are explicit nulls rather than absent fields in observed replies.
        object.insert(
            "parent_session_id".to_owned(),
            serde_json::to_value(self.session.parent_session_id)
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "role_summary".to_owned(),
            serde_json::to_value(&self.role_summary).map_err(serde::ser::Error::custom)?,
        );
        if let Some(runtime) = &self.runtime {
            let fields = serde_json::to_value(runtime).map_err(serde::ser::Error::custom)?;
            object.extend(
                fields
                    .as_object()
                    .ok_or_else(|| {
                        serde::ser::Error::custom(
                            "session runtime observation must serialize as an object",
                        )
                    })?
                    .clone(),
            );
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionListItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("session list item must be an object"))?;
        // A create/remove reply from an older or pre-observation path may omit
        // every runtime field. Once any runtime field is present, decode the
        // complete typed observation and reject partial or malformed metadata.
        let runtime = RUNTIME_FIELDS
            .iter()
            .any(|field| object.contains_key(*field))
            .then(|| {
                serde_json::from_value::<SessionRuntimeObservation>(value.clone())
                    .map_err(serde::de::Error::custom)
            })
            .transpose()?;
        let role_summary = object
            .get("role_summary")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?
            .flatten();
        let session = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            session,
            role_summary,
            runtime,
        })
    }
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
        assert!(encoded["sessions"][0].get("creator_agent_id").is_none());
        assert_eq!(
            serde_json::from_value::<SessionListSnapshot>(encoded).unwrap(),
            snapshot
        );

        let mut runtime = runtime();
        runtime.agent_status = None;
        runtime.parent_session_name = None;
        let encoded = serde_json::to_value(SessionListItem {
            session: ManagedSession::new_creating(
                "root-child".to_owned(),
                OperationId::new(),
                Utc::now(),
            ),
            role_summary: None,
            runtime: Some(runtime),
        })
        .unwrap();
        for field in [
            "parent_session_id",
            "role_summary",
            "agent_status",
            "parent_session_name",
        ] {
            assert!(
                encoded[field].is_null(),
                "{field} must remain explicit null"
            );
        }
    }

    #[test]
    fn list_snapshot_accepts_absent_runtime_but_rejects_partial_or_malformed_metadata() {
        let session =
            ManagedSession::new_creating("child".to_owned(), OperationId::new(), Utc::now());
        let mut value = serde_json::to_value(SessionListItem {
            session,
            role_summary: None,
            runtime: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::from_value::<SessionListItem>(value.clone())
                .unwrap()
                .runtime,
            None
        );
        value["agent_phase"] = serde_json::json!("unknown");
        assert!(serde_json::from_value::<SessionListItem>(value).is_err());

        let session =
            ManagedSession::new_creating("child".to_owned(), OperationId::new(), Utc::now());
        let mut partial = serde_json::to_value(SessionListItem {
            session,
            role_summary: None,
            runtime: None,
        })
        .unwrap();
        partial["agent_phase"] = serde_json::json!("running");
        assert!(serde_json::from_value::<SessionListItem>(partial).is_err());
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
        assert!(encoded["sessions"][0]["role_id"].is_null());
        assert!(encoded["sessions"][0]["role_summary"].is_null());
        assert!(encoded["sessions"][0]["parent_session_id"].is_null());
        assert_eq!(
            serde_json::from_value::<SessionStatusSnapshot>(encoded).unwrap(),
            snapshot
        );
    }
}
