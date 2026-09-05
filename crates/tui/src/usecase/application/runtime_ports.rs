//! Runtime boundaries used by the workspace application loop.
//!
//! These contracts describe application effects and observations.  They live
//! beside the controller instead of beside rendering so infrastructure and
//! composition adapters do not depend on a presentation module merely to
//! implement IO.

use std::collections::BTreeMap;
use std::path::Path;

use usagi_core::domain::agent::{AgentWorkspaceObservation, ProviderResumeProjection};
use usagi_core::domain::id::{SessionId, UserDecisionId, WorkspaceId};
use usagi_core::domain::session::SessionRecord;
use usagi_core::domain::session_lifecycle::SessionLifecycleProjection;
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::env::EnvScope;

use super::controller::{BackendEvent, EnvironmentEntry, SessionRoleProjection};
use crate::usecase::overview::SessionCommand;

/// Platform-native terminal launch boundary.
pub trait ExternalTerminalPort: Send {
    /// Opens a native terminal rooted at `directory`.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe platform launch failure.
    fn open(&mut self, directory: &Path) -> Result<(), String>;
}

/// Daemon-authoritative durable decision boundary.
pub trait DecisionCommandPort: Send {
    /// Fetches the authoritative pending snapshot for one workspace.
    fn refresh(&mut self, workspace: WorkspaceId) -> BackendEvent;

    /// Submits one already validated answer.
    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    ) -> BackendEvent;
}

/// Durable per-target environment boundary.
pub trait EnvironmentStorePort: Send {
    /// Reads `scope`'s bindings and inherited global bindings.
    fn load(&mut self, scope: EnvScope) -> BackendEvent;

    /// Replaces the complete stored entry set for `scope`.
    fn save(&mut self, scope: EnvScope, entries: Vec<EnvironmentEntry>) -> BackendEvent;
}

/// Best-effort desktop notification boundary.
pub trait DesktopNotificationPort {
    /// Announces a newly observed decision without making delivery required.
    fn notify(&mut self, title: &str, body: &str);
}

/// Read-only daemon lane used to observe other projects in the Garden.
pub trait GardenInventoryPort: Send {
    /// Returns the safe Agent observation for `workspace`.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon is unavailable or refuses the
    /// workspace.
    fn inventory(&mut self, workspace: WorkspaceId) -> Result<AgentWorkspaceObservation, String>;
}

/// Dedicated restore-client connection lifecycle.
pub trait RestoreConnectionPort: Send {
    /// Drains the newest strictly monotonic reconnect epoch.
    fn take_reconnected_epoch(&mut self) -> Option<u64>;
}

/// Overview session commands owned by the daemon lifecycle runner.
pub trait SessionCommandPort: Send + Sync {
    /// Executes one parsed command against the selected workspace/session.
    ///
    /// # Errors
    ///
    /// Returns a safe message when the daemon cannot accept the request.
    fn execute(
        &self,
        _workspace: &Workspace,
        _selected: Option<&SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session command port is not implemented".to_owned())
    }
}

/// Safe result of one daemon-owned session command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCommandResult {
    /// Message for the Overview modal.
    pub message: String,
    /// Authoritative sidebar rows, when refreshed.
    pub sessions: Option<Vec<SessionRecord>>,
    /// Stable daemon identities aligned with `sessions`.
    pub session_ids: Option<Vec<SessionId>>,
    /// Safe provider resume state keyed by stable session identity.
    pub agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
    /// Safe lifecycle state keyed by stable session identity.
    pub session_lifecycles: Option<BTreeMap<SessionId, SessionLifecycleProjection>>,
    /// Safe role projection keyed by stable session identity.
    pub session_roles: Option<BTreeMap<SessionId, SessionRoleProjection>>,
    /// Monotonic daemon lifecycle revision.
    pub revision: Option<u64>,
}

impl SessionCommandResult {
    /// Creates a result carrying only a user-facing message.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sessions: None,
            session_ids: None,
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        }
    }
}

/// Resident session-inventory observation lane.
pub trait SessionRefreshPort: Send {
    /// Requests an immediate out-of-cadence observation without blocking.
    fn wake(&mut self) {}

    /// Drains the newest completed snapshot.
    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        None
    }
}

/// Creates a fresh session command port for each workspace launch.
pub trait SessionCommandPortFactory {
    /// Builds a workspace-scoped command port.
    fn create(&mut self) -> Box<dyn SessionCommandPort>;
}

/// Read-only worktree-name scan used by the create-session collision hint.
pub trait SessionWorktreeScanPort {
    /// Returns directory names directly under `<workspace>/.usagi/sessions`.
    fn scan(&mut self, workspace: &Path) -> Vec<String>;
}
