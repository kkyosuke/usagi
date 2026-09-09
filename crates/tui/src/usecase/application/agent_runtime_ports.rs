//! Daemon Agent and terminal boundaries used by the workspace runtime.
//!
//! Launch, restore, and terminal streaming are application capabilities.  This
//! module keeps their contracts out of the rendering module and lets the
//! composition root provide independent clients for each blocking lane.

use std::sync::{Mutex, MutexGuard, PoisonError};

use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentResumeRelation, AgentResumeTarget,
};
use usagi_core::domain::id::{
    AgentContinuationRef, OperationId, SessionId, TerminalRef, WorkspaceId,
};
use usagi_core::domain::supervisor::SupervisorRunId;
use usagi_core::domain::terminal_launch::TerminalInventoryEntry;

use super::pane_runtime::Geometry;
use super::terminal_session::{
    TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome, TerminalInputResolution,
    TerminalSubscription,
};

/// One daemon-authoritative Agent launch admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaneAdmission {
    /// Fully fenced terminal identity.
    pub terminal: TerminalRef,
    /// Provider-neutral continuation identity, when available.
    pub continuation: Option<AgentContinuationRef>,
    /// Run identity returned only for a goal-driven root launch.
    pub supervisor_run_id: Option<SupervisorRunId>,
}

/// One accepted exact-target Agent resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactAgentResume {
    /// Replacement runtime's fully fenced terminal.
    pub terminal: TerminalRef,
    /// Lineage continued by the replacement.
    pub continuation: Option<AgentContinuationRef>,
    /// Source-to-replacement relation proving what was replaced.
    pub relation: Option<AgentResumeRelation>,
}

/// Composition-level bundle for one workspace's Agent and terminal client.
///
/// Feature consumers do not receive this aggregate directly: pane launch uses
/// [`PaneLaunchCommandPort`] and a live terminal uses `TerminalStreamPort`.
/// Restore alone needs Agent inventory and terminal inventory from the same
/// connection so it can bracket a coherent snapshot.
pub trait AgentCommandPort: Send {
    /// Launches one Agent under the caller's durable operation.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon failure.
    fn launch(
        &mut self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String>;

    /// Launches a goal-driven workspace root Agent.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon failure.
    fn launch_goal(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _profile: Option<AgentProfileId>,
        _goal: &str,
    ) -> Result<AgentPaneAdmission, String> {
        Err("goal-driven Agent launch is unavailable".to_owned())
    }

    /// Resumes retained metadata without attaching to its old PTY.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon refuses the resume.
    fn resume(
        &mut self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation_id: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent resume is unavailable.".to_owned())
    }

    /// Returns the safe exact-target inventory for one workspace.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the inventory cannot be read.
    fn resume_inventory(&mut self, _workspace: WorkspaceId) -> Result<AgentInventory, String> {
        Err("Agent resume inventory is unavailable.".to_owned())
    }

    /// Resumes only the daemon-issued target selected by the caller.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the exact relation cannot be proven.
    fn resume_exact(
        &mut self,
        _target: AgentResumeTarget,
        _operation_id: OperationId,
    ) -> Result<ExactAgentResume, String> {
        Err("Exact Agent resume is unavailable.".to_owned())
    }

    /// Opens a daemon-owned login shell for a workspace or session scope.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe launch failure.
    fn launch_terminal(
        &mut self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is unavailable".to_owned())
    }

    /// Applies a visible pane viewport to one daemon terminal.
    ///
    /// # Errors
    ///
    /// Returns a safe transport or ownership failure.
    fn resize_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Attaches and returns retained replay and cursor state.
    ///
    /// # Errors
    ///
    /// Returns a safe transport or ownership failure.
    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Fetches output produced after `after_offset`.
    ///
    /// # Errors
    ///
    /// Returns a safe transport or ownership failure.
    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Returns the current shared terminal transport epoch.
    fn terminal_connection_epoch(&self) -> Option<u64> {
        None
    }

    /// Sends input fenced by subscription, sequence, and operation.
    ///
    /// # Errors
    ///
    /// Returns a safe transport or ownership failure.
    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        _bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Reads the recorded final of one durable input operation.
    ///
    /// # Errors
    ///
    /// Returns a safe transport or ownership failure.
    fn terminal_input_outcome(
        &mut self,
        _terminal: &TerminalRef,
        _operation: OperationId,
        _input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Releases a subscription without stopping its process.
    fn detach_terminal(&mut self, _terminal: &TerminalRef, _subscription: TerminalSubscription) {}

    /// Declares detached terminals whose exit still needs observation.
    fn watch_background_terminals(&mut self, _terminals: &[TerminalRef]) {}

    /// Drains background terminals observed as no longer live.
    fn take_exited_background_terminals(&mut self, _limit: usize) -> Vec<TerminalRef> {
        Vec::new()
    }

    /// Lists daemon-owned runtimes so live panes can be restored.
    ///
    /// # Errors
    ///
    /// Returns a safe transport failure.
    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        Err(TerminalError::Unavailable)
    }
}

/// Narrow terminal-stream capability consumed by live pane sessions.
///
/// The production connection can implement the wider [`AgentCommandPort`],
/// while stream coordination remains unable to launch or resume an Agent.
pub trait TerminalCommandPort {
    /// Returns the current shared terminal transport epoch.
    fn connection_epoch(&self) -> Option<u64>;

    /// Applies the requested viewport.
    ///
    /// # Errors
    ///
    /// Returns a transport, ownership, or availability failure.
    fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError>;

    /// Attaches one terminal stream.
    ///
    /// # Errors
    ///
    /// Returns a transport, ownership, or availability failure.
    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError>;

    /// Reads output after the supplied offset.
    ///
    /// # Errors
    ///
    /// Returns a transport, ownership, or availability failure.
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError>;

    /// Writes fenced input to the terminal.
    ///
    /// # Errors
    ///
    /// Returns a transport, ownership, or availability failure.
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError>;

    /// Reads the final resolution of one input operation.
    ///
    /// # Errors
    ///
    /// Returns a transport, ownership, or availability failure.
    fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError>;

    /// Releases one stream subscription without stopping its process.
    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription);
}

impl<T: AgentCommandPort + ?Sized> TerminalCommandPort for T {
    fn connection_epoch(&self) -> Option<u64> {
        self.terminal_connection_epoch()
    }

    fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        self.resize_terminal(terminal, geometry)
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.attach_terminal(terminal, geometry)
    }

    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.poll_terminal(terminal, after_offset)
    }

    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.input_terminal(terminal, subscription, input_seq, operation, bytes)
    }

    fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        self.terminal_input_outcome(terminal, operation, input_len)
    }

    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
        self.detach_terminal(terminal, subscription);
    }
}

/// Shared boundary for a single pane launch request.
pub trait PaneLaunchCommandPort: Send + Sync {
    /// Launches one Agent under the controller's durable operation.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon failure.
    fn launch(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String>;

    /// Launches one goal-driven root Agent.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon failure.
    fn launch_goal(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _profile: Option<AgentProfileId>,
        _goal: &str,
    ) -> Result<AgentPaneAdmission, String> {
        Err("goal-driven Agent launch is unavailable".to_owned())
    }

    /// Resumes one session Agent.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon refuses the resume.
    fn resume(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        operation: OperationId,
    ) -> Result<AgentPaneAdmission, String>;

    /// Resumes one exact daemon-issued target.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the exact relation cannot be proven.
    fn resume_exact(
        &self,
        target: AgentResumeTarget,
        operation: OperationId,
    ) -> Result<ExactAgentResume, String>;

    /// Opens a daemon-owned shell.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon failure.
    fn launch_terminal(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        geometry: Geometry,
        arguments: &str,
        operation: OperationId,
    ) -> Result<TerminalRef, String>;
}

/// Serializes one dedicated Agent client behind the shared launch contract.
pub struct SerializedPaneLaunchPort(Mutex<Box<dyn AgentCommandPort>>);

impl SerializedPaneLaunchPort {
    /// Binds a dedicated launch client for one workspace.
    #[must_use]
    pub fn new(port: Box<dyn AgentCommandPort>) -> Self {
        Self(Mutex::new(port))
    }

    fn client(&self) -> MutexGuard<'_, Box<dyn AgentCommandPort>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PaneLaunchCommandPort for SerializedPaneLaunchPort {
    fn launch(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        self.client().launch(operation, workspace, session, profile)
    }

    fn launch_goal(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        profile: Option<AgentProfileId>,
        goal: &str,
    ) -> Result<AgentPaneAdmission, String> {
        self.client()
            .launch_goal(operation, workspace, profile, goal)
    }

    fn resume(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        self.client().resume(workspace, session, operation)
    }

    fn resume_exact(
        &self,
        target: AgentResumeTarget,
        operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        self.client().resume_exact(target, operation)
    }

    fn launch_terminal(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        geometry: Geometry,
        arguments: &str,
        operation: OperationId,
    ) -> Result<TerminalRef, String> {
        self.client()
            .launch_terminal(workspace, session, geometry, arguments, operation)
    }
}

/// Creates a fresh daemon Agent client for every workspace entry.
pub trait AgentCommandPortFactory {
    /// Builds one workspace-scoped client.
    fn create(&mut self) -> Box<dyn AgentCommandPort>;
}
