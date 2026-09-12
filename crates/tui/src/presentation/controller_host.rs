//! Terminal-loop adapter for controller effects whose stateful host stays in
//! the loop.
//!
//! [`ControllerHost`] is handed to the production backend factory as a port. It
//! holds no policy: every call enqueues exactly one [`ControllerHostAction`] for
//! the terminal loop to drain, so the loop keeps ownership of the state those
//! actions need while [`DaemonBackend`](crate::usecase::application::daemon_backend::DaemonBackend)
//! remains the sole dispatcher of controller effects.

use std::sync::mpsc::{self, Receiver, Sender};

use usagi_core::domain::id::WorkspaceId;

use crate::usecase::application::controller::{
    AppEvent, BackendEvent, Notice, OperationResult, Target,
};
use crate::usecase::application::daemon_backend::{
    AgentPort as BackendAgentPort, Completions, CreateSessionRequest, LaunchAgentRequest,
    OpenTerminalRequest, RemoveSessionRequest, ReopenAgentRequest, ResumeAgentRequest,
    SessionCommandPort as BackendSessionCommandPort, SleepSessionRequest,
};

/// Actions whose stateful host remains in the terminal loop while
/// [`DaemonBackend`] is the sole controller-effect dispatcher.
pub enum ControllerHostAction {
    Create(CreateSessionRequest, Completions),
    Refresh(WorkspaceId, Completions),
    Remove(RemoveSessionRequest, Completions),
    Sleep(SleepSessionRequest, Completions),
    LaunchAgent(LaunchAgentRequest),
    ResumeAgent(ResumeAgentRequest),
    ReopenAgent(ReopenAgentRequest),
    OpenTerminal(OpenTerminalRequest),
    OpenExternalTerminal(Target),
    SelectTab(crate::usecase::application::controller::TabDirection),
}

/// Cloneable adapter handed to the production backend factory. It contains no
/// policy: each port call enqueues exactly one action for the terminal host.
#[derive(Clone)]
pub struct ControllerHost(Sender<ControllerHostAction>);

impl ControllerHost {
    /// Create the host adapter and the terminal loop's action receiver.
    #[must_use]
    pub fn channel() -> (Self, Receiver<ControllerHostAction>) {
        let (sender, receiver) = mpsc::channel();
        (Self(sender), receiver)
    }
}

impl BackendSessionCommandPort for ControllerHost {
    fn create(&mut self, request: CreateSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Create(request, completions))) = self
            .0
            .send(ControllerHostAction::Create(request, completions))
        {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: request.token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("session command host is unavailable")),
            }));
        }
    }

    fn refresh(&mut self, workspace: WorkspaceId, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Refresh(_, completions))) = self
            .0
            .send(ControllerHostAction::Refresh(workspace, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }

    fn remove(&mut self, request: RemoveSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Remove(_, completions))) = self
            .0
            .send(ControllerHostAction::Remove(request, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }

    fn sleep(&mut self, request: SleepSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Sleep(_, completions))) = self
            .0
            .send(ControllerHostAction::Sleep(request, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }
}

impl BackendAgentPort for ControllerHost {
    fn launch_agent(&mut self, request: LaunchAgentRequest) {
        let _ = self.0.send(ControllerHostAction::LaunchAgent(request));
    }

    fn resume_agent(&mut self, request: ResumeAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ResumeAgent(request));
    }

    fn reopen_agent(&mut self, request: ReopenAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ReopenAgent(request));
    }

    fn open_terminal(&mut self, request: OpenTerminalRequest) {
        let _ = self.0.send(ControllerHostAction::OpenTerminal(request));
    }

    fn open_external_terminal(&mut self, target: Target) {
        let _ = self
            .0
            .send(ControllerHostAction::OpenExternalTerminal(target));
    }

    fn select_tab(&mut self, direction: crate::usecase::application::controller::TabDirection) {
        let _ = self.0.send(ControllerHostAction::SelectTab(direction));
    }
}
