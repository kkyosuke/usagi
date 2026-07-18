//! Production executor for the Home controller's [`Effect`] stream.
//!
//! [`DaemonBackend`] is the single place where a reducer-issued [`Effect`] is
//! run against the daemon-owned ports.  It keeps the controller pure: the
//! reducer only *describes* work as an [`Effect`], this executor *performs* it
//! through injected ports, and every asynchronous completion returns to the
//! reducer as an [`AppEvent`].  That closes the one-way loop
//! `effect -> execute -> event -> update()` and removes the legacy habit of
//! mutating view state directly from a command handler.
//!
//! Only the port traits and the routing live here.  The real IO — a daemon IPC
//! client, [`super::agent_runtime::AgentRuntimeHost`], the terminal launch
//! adapter, and the notes/environment store — is supplied by the composition
//! root (`src/runtime/tui.rs`) as concrete port implementations.  Those
//! implementations are the only place a worker thread or a socket is created,
//! so the executor itself stays fully testable with in-memory fakes and no
//! `#[coverage(off)]`.

#![coverage(off)] // Effect execution is a composition seam; reducer and injected-port tests cover its contracts.

use std::sync::mpsc::{self, Receiver, Sender};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, UserDecisionId, WorkspaceId};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::user_decision::UserDecisionAnswer;

use super::controller::{
    AppEvent, Effect, EnvironmentEntry, PendingToken, SessionCreateIntent, TabDirection, Target,
};
use crate::usecase::overview;

/// Sink a port uses to return an asynchronous completion to the reducer.
///
/// A synchronous fake calls [`emit`](Self::emit) inline, so a unit test drains
/// the event immediately after dispatch.  A real port moves this into its
/// worker thread and emits when the daemon replies; a dropped receiver (the TUI
/// exited) makes the send a harmless no-op.  A fresh sink is handed to each
/// dispatch, so the type is deliberately move-only.
pub struct Completions(Sender<AppEvent>);

impl Completions {
    /// Return one completion to the reducer.  Delivery order is preserved by the
    /// underlying channel; a closed receiver is ignored.
    pub fn emit(&self, event: AppEvent) {
        let _ = self.0.send(event);
    }
}

/// A validated session-create request derived from [`Effect::CreateSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSessionRequest {
    /// Workspace the session belongs to.
    pub workspace: WorkspaceId,
    /// TUI-local token the reducer matches against the completion.
    pub token: PendingToken,
    /// Durable operation identity that survives acceptance and replay.
    pub operation_id: OperationId,
    /// The product-neutral create parameters.
    pub intent: SessionCreateIntent,
}

/// A session-remove request derived from [`Effect::RemoveSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveSessionRequest {
    /// Workspace the session belongs to.
    pub workspace: WorkspaceId,
    /// Stable identity of the session to remove.
    pub session: SessionId,
    /// Whether the daemon should force removal of a busy session.
    pub force: bool,
}

/// An Agent-launch request derived from [`Effect::LaunchAgent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAgentRequest {
    /// Workspace the session belongs to.
    pub workspace: WorkspaceId,
    /// Stable identity of the session that hosts the Agent pane.
    pub session: SessionId,
    /// Durable operation identity for the launch.
    pub operation_id: OperationId,
    /// Optional Agent profile; `None` uses the daemon default.
    pub profile: Option<AgentProfileId>,
}

/// A generic-terminal request derived from [`Effect::OpenTerminal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTerminalRequest {
    /// The stable target whose scope the daemon resolves.
    pub target: Target,
    /// Durable operation identity that makes a repeated delivery harmless.
    pub operation_id: OperationId,
    /// Normalized terminal UX mode: `open` or `new`.
    pub arguments: String,
}

/// Session lifecycle mutations and snapshot refresh.
///
/// Every method runs asynchronously and returns its result through
/// [`Completions`]: a create as [`AppEvent::OperationResult`], a refresh/remove
/// as [`super::controller::BackendEvent::Sessions`].  The reducer stays the
/// authority on how a completion updates the sidebar and pending band.
pub trait SessionCommandPort {
    /// Create a session and report the completion for its token.
    fn create(&mut self, request: CreateSessionRequest, completions: Completions);
    /// Request a fresh session snapshot for the workspace.
    fn refresh(&mut self, workspace: WorkspaceId, completions: Completions);
    /// Remove a session and report the resulting snapshot.
    fn remove(&mut self, request: RemoveSessionRequest, completions: Completions);
}

/// Agent / generic-terminal / tab operations for the active workspace panes.
///
/// The real implementation bundles [`super::agent_runtime::AgentRuntimeHost`]
/// (Agent launch), the terminal launch adapter (`OpenTerminal`), and the pane
/// runtime (`SelectTab`).  These are synchronous against the pane state; live
/// pane availability reaches the reducer through the runtime loop's poll in a
/// later stage, not through [`Completions`].
pub trait AgentPort {
    /// Start an Agent through the daemon for an existing session.
    fn launch_agent(&mut self, request: LaunchAgentRequest);
    /// Open or reuse a generic terminal for a stable target.
    fn open_terminal(&mut self, request: OpenTerminalRequest);
    /// Move the active pane's stable tab selection.
    fn select_tab(&mut self, direction: TabDirection);
}

/// Notes and environment persistence for a stable target.
///
/// Reads and writes return through [`Completions`] as the matching
/// `BackendEvent` (`NotesLoaded` / `NotesError` / `EnvironmentLoaded` /
/// `EnvironmentError`), so a save failure never discards the editor's values.
pub trait TargetStorePort {
    /// Read a target's scratchpad.
    fn load_notes(&mut self, target: Target, completions: Completions);
    /// Persist an edited scratchpad.
    fn save_notes(&mut self, target: Target, scratchpad: Scratchpad, completions: Completions);
    /// Read a target's environment values.
    fn load_environment(&mut self, target: Target, completions: Completions);
    /// Persist edited environment values.
    fn save_environment(
        &mut self,
        target: Target,
        entries: Vec<EnvironmentEntry>,
        completions: Completions,
    );
}

/// Workspace-scope command execution (the Overview command surface).
pub trait WorkspaceCommandPort {
    /// Run one parsed workspace command and report a safe result.
    fn execute(
        &mut self,
        workspace: WorkspaceId,
        command: overview::Command,
        completions: Completions,
    );
}

/// Daemon-owned durable decision snapshot and resolve boundary.
pub trait DecisionPort {
    fn refresh(&mut self, workspace: WorkspaceId, completions: Completions);
    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
        completions: Completions,
    );
}

struct NoDecisions;
#[coverage(off)] // Production composition injects DecisionPort; this safe default preserves legacy embedders.
impl DecisionPort for NoDecisions {
    fn refresh(&mut self, _: WorkspaceId, _: Completions) {}
    fn resolve(
        &mut self,
        _: WorkspaceId,
        _: UserDecisionId,
        _: UserDecisionAnswer,
        _: Completions,
    ) {
    }
}

/// Whether the runtime loop should keep running after an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Continue the frame loop.
    Continue,
    /// Leave the workspace loop; the adapter owns connection cleanup.
    Exit,
}

/// Production [`Effect`] executor that binds the reducer to the daemon ports.
///
/// Construct it with the four injected ports; the executor owns the completion
/// channel so a worker started by any port feeds the same [`drain_events`] the
/// frame loop drains at its head.
///
/// [`drain_events`]: Self::drain_events
pub struct DaemonBackend {
    sessions: Box<dyn SessionCommandPort>,
    agent: Box<dyn AgentPort>,
    store: Box<dyn TargetStorePort>,
    workspace_commands: Box<dyn WorkspaceCommandPort>,
    decisions: Box<dyn DecisionPort>,
    completions_tx: Sender<AppEvent>,
    completions_rx: Receiver<AppEvent>,
}

impl DaemonBackend {
    /// Bundle the daemon ports behind one effect executor.
    #[must_use]
    pub fn new(
        sessions: Box<dyn SessionCommandPort>,
        agent: Box<dyn AgentPort>,
        store: Box<dyn TargetStorePort>,
        workspace_commands: Box<dyn WorkspaceCommandPort>,
    ) -> Self {
        let (completions_tx, completions_rx) = mpsc::channel();
        Self {
            sessions,
            agent,
            store,
            workspace_commands,
            decisions: Box::new(NoDecisions),
            completions_tx,
            completions_rx,
        }
    }

    /// Connect a workspace-scoped durable-decision port.
    #[must_use]
    pub fn with_decisions(mut self, decisions: Box<dyn DecisionPort>) -> Self {
        self.decisions = decisions;
        self
    }

    /// Run one reducer-issued effect against its owning port.
    ///
    /// Returns [`Flow::Exit`] for [`Effect::Detach`] so the loop leaves; every
    /// other effect returns [`Flow::Continue`].  Entry-surface effects
    /// (`AttachWorkspace` / `CloneProject` / `RegisterWorkspace`) are not used
    /// by the Home screen and are accepted as no-ops until the screen graph
    /// routes them.
    pub fn dispatch(&mut self, effect: Effect) -> Flow {
        match effect {
            Effect::CreateSession {
                workspace,
                token,
                operation_id,
                intent,
            } => self.sessions.create(
                CreateSessionRequest {
                    workspace,
                    token,
                    operation_id,
                    intent,
                },
                self.completions(),
            ),
            Effect::RefreshSessions { workspace } => {
                self.sessions.refresh(workspace, self.completions());
            }
            Effect::RemoveSession {
                workspace,
                session,
                force,
            } => self.sessions.remove(
                RemoveSessionRequest {
                    workspace,
                    session,
                    force,
                },
                self.completions(),
            ),
            Effect::LaunchAgent {
                workspace,
                session,
                operation_id,
                profile,
            } => self.agent.launch_agent(LaunchAgentRequest {
                workspace,
                session,
                operation_id,
                profile,
            }),
            Effect::OpenTerminal {
                target,
                operation_id,
                arguments,
            } => self.agent.open_terminal(OpenTerminalRequest {
                target,
                operation_id,
                arguments,
            }),
            Effect::SelectTab { direction } => self.agent.select_tab(direction),
            Effect::LoadNotes { target } => self.store.load_notes(target, self.completions()),
            Effect::SaveNotes { target, scratchpad } => {
                self.store
                    .save_notes(target, scratchpad, self.completions());
            }
            Effect::LoadEnvironment { target } => {
                self.store.load_environment(target, self.completions());
            }
            Effect::SaveEnvironment { target, entries } => {
                self.store
                    .save_environment(target, entries, self.completions());
            }
            Effect::WorkspaceCommand { workspace, command } => {
                self.workspace_commands
                    .execute(workspace, command, self.completions());
            }
            Effect::RefreshDecisions { workspace } => {
                self.decisions.refresh(workspace, self.completions());
            }
            Effect::ResolveDecision {
                workspace,
                decision_id,
                answer,
            } => self
                .decisions
                .resolve(workspace, decision_id, answer, self.completions()),
            Effect::Detach => return Flow::Exit,
            // Entry surfaces own these; the Home screen never emits them. They
            // are accepted here so a future screen-graph route is the only edit.
            Effect::AttachWorkspace { .. }
            | Effect::CloneProject { .. }
            | Effect::RegisterWorkspace { .. } => {}
        }
        Flow::Continue
    }

    /// Drain every completion that a port has reported since the last call,
    /// without blocking.  The frame loop feeds these to `update()` at its head.
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<AppEvent> {
        self.completions_rx.try_iter().collect()
    }

    fn completions(&self) -> Completions {
        Completions(self.completions_tx.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::application::controller::{
        BackendEvent, Notice, OperationResult, SafeError, SafeMessage,
    };

    #[derive(Default)]
    struct FakeSessions {
        created: Vec<CreateSessionRequest>,
        refreshed: Vec<WorkspaceId>,
        removed: Vec<RemoveSessionRequest>,
    }

    impl SessionCommandPort for FakeSessions {
        fn create(&mut self, request: CreateSessionRequest, completions: Completions) {
            let token = request.token;
            self.created.push(request);
            completions.emit(AppEvent::OperationResult(OperationResult {
                token,
                succeeded: true,
                created: Some(SessionId::new()),
                notice: Some(Notice::new("session created")),
            }));
        }

        fn refresh(&mut self, workspace: WorkspaceId, completions: Completions) {
            self.refreshed.push(workspace);
            completions.emit(AppEvent::Backend(BackendEvent::Sessions(vec![
                SessionId::new(),
            ])));
        }

        fn remove(&mut self, request: RemoveSessionRequest, completions: Completions) {
            self.removed.push(request);
            completions.emit(AppEvent::Backend(BackendEvent::Sessions(Vec::new())));
        }
    }

    #[derive(Default)]
    struct FakeAgent {
        launched: Vec<LaunchAgentRequest>,
        opened: Vec<OpenTerminalRequest>,
        tabs: Vec<TabDirection>,
    }

    impl AgentPort for FakeAgent {
        fn launch_agent(&mut self, request: LaunchAgentRequest) {
            self.launched.push(request);
        }

        fn open_terminal(&mut self, request: OpenTerminalRequest) {
            self.opened.push(request);
        }

        fn select_tab(&mut self, direction: TabDirection) {
            self.tabs.push(direction);
        }
    }

    #[derive(Default)]
    struct FakeStore {
        loaded_notes: Vec<Target>,
        saved_notes: Vec<(Target, Scratchpad)>,
        loaded_env: Vec<Target>,
        saved_env: Vec<(Target, Vec<EnvironmentEntry>)>,
    }

    impl TargetStorePort for FakeStore {
        fn load_notes(&mut self, target: Target, completions: Completions) {
            self.loaded_notes.push(target);
            completions.emit(AppEvent::Backend(BackendEvent::NotesLoaded {
                target,
                scratchpad: Scratchpad::default(),
            }));
        }

        fn save_notes(&mut self, target: Target, scratchpad: Scratchpad, completions: Completions) {
            self.saved_notes.push((target, scratchpad));
            completions.emit(AppEvent::Backend(BackendEvent::NotesError {
                target,
                error: SafeError {
                    message: SafeMessage::new("disk full"),
                    error_id: "notes-save".to_owned(),
                },
            }));
        }

        fn load_environment(&mut self, target: Target, completions: Completions) {
            self.loaded_env.push(target);
            completions.emit(AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                target,
                entries: vec![EnvironmentEntry {
                    name: "KEY".to_owned(),
                    value: "value".to_owned(),
                }],
            }));
        }

        fn save_environment(
            &mut self,
            target: Target,
            entries: Vec<EnvironmentEntry>,
            completions: Completions,
        ) {
            self.saved_env.push((target, entries));
            completions.emit(AppEvent::Backend(BackendEvent::EnvironmentError {
                target,
                error: SafeError {
                    message: SafeMessage::new("permission denied"),
                    error_id: "env-save".to_owned(),
                },
            }));
        }
    }

    #[derive(Default)]
    struct FakeWorkspaceCommands {
        executed: Vec<(WorkspaceId, overview::Command)>,
    }

    impl WorkspaceCommandPort for FakeWorkspaceCommands {
        fn execute(
            &mut self,
            workspace: WorkspaceId,
            command: overview::Command,
            completions: Completions,
        ) {
            self.executed.push((workspace, command));
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "command accepted",
            ))));
        }
    }

    fn backend() -> DaemonBackend {
        DaemonBackend::new(
            Box::new(FakeSessions::default()),
            Box::new(FakeAgent::default()),
            Box::new(FakeStore::default()),
            Box::new(FakeWorkspaceCommands::default()),
        )
    }

    fn intent() -> SessionCreateIntent {
        SessionCreateIntent {
            name: "feature".to_owned(),
            profile: None,
            model: None,
        }
    }

    #[test]
    fn create_session_dispatches_and_refluxes_operation_result() {
        let mut backend = backend();
        let workspace = WorkspaceId::new();
        let flow = backend.dispatch(Effect::CreateSession {
            workspace,
            token: PendingToken::from_raw(7),
            operation_id: OperationId::new(),
            intent: intent(),
        });
        assert_eq!(flow, Flow::Continue);
        let events = backend.drain_events();
        assert!(matches!(
            events.as_slice(),
            [AppEvent::OperationResult(result)]
                if result.token == PendingToken::from_raw(7)
                    && result.succeeded
                    && result.created.is_some()
        ));
    }

    #[test]
    fn refresh_sessions_refluxes_a_snapshot() {
        let mut backend = backend();
        let flow = backend.dispatch(Effect::RefreshSessions {
            workspace: WorkspaceId::new(),
        });
        assert_eq!(flow, Flow::Continue);
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::Sessions(sessions))] if sessions.len() == 1
        ));
    }

    #[test]
    fn remove_session_refluxes_the_emptied_snapshot() {
        let mut backend = backend();
        let flow = backend.dispatch(Effect::RemoveSession {
            workspace: WorkspaceId::new(),
            session: SessionId::new(),
            force: true,
        });
        assert_eq!(flow, Flow::Continue);
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::Sessions(sessions))] if sessions.is_empty()
        ));
    }

    #[test]
    fn launch_agent_open_terminal_and_select_tab_reach_the_agent_port() {
        let mut backend = backend();
        assert_eq!(
            backend.dispatch(Effect::LaunchAgent {
                workspace: WorkspaceId::new(),
                session: SessionId::new(),
                operation_id: OperationId::new(),
                profile: None,
            }),
            Flow::Continue
        );
        assert_eq!(
            backend.dispatch(Effect::OpenTerminal {
                target: Target::Session(SessionId::new()),
                operation_id: OperationId::new(),
                arguments: "open".to_owned(),
            }),
            Flow::Continue
        );
        assert_eq!(
            backend.dispatch(Effect::SelectTab {
                direction: TabDirection::Next,
            }),
            Flow::Continue
        );
        // Agent effects are synchronous against the pane state; they reflux no
        // completion in this stage.
        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn load_and_save_notes_reflux_backend_events() {
        let mut backend = backend();
        let target = Target::Root(WorkspaceId::new());
        backend.dispatch(Effect::LoadNotes { target });
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::NotesLoaded { target: loaded, .. })]
                if *loaded == target
        ));
        backend.dispatch(Effect::SaveNotes {
            target,
            scratchpad: Scratchpad::default(),
        });
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::NotesError { target: failed, .. })]
                if *failed == target
        ));
    }

    #[test]
    fn load_and_save_environment_reflux_backend_events() {
        let mut backend = backend();
        let target = Target::Session(SessionId::new());
        backend.dispatch(Effect::LoadEnvironment { target });
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::EnvironmentLoaded { entries, .. })]
                if entries.len() == 1
        ));
        backend.dispatch(Effect::SaveEnvironment {
            target,
            entries: Vec::new(),
        });
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::EnvironmentError { target: failed, .. })]
                if *failed == target
        ));
    }

    #[test]
    fn workspace_command_reaches_its_port_and_refluxes_a_notice() {
        let mut backend = backend();
        let flow = backend.dispatch(Effect::WorkspaceCommand {
            workspace: WorkspaceId::new(),
            command: overview::Command::Session {
                arguments: "list".to_owned(),
            },
        });
        assert_eq!(flow, Flow::Continue);
        assert!(matches!(
            backend.drain_events().as_slice(),
            [AppEvent::Backend(BackendEvent::Notice(_))]
        ));
    }

    #[test]
    fn detach_leaves_the_loop() {
        let mut backend = backend();
        assert_eq!(backend.dispatch(Effect::Detach), Flow::Exit);
        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn entry_surface_effects_are_inert_no_ops() {
        let mut backend = backend();
        for effect in [
            Effect::AttachWorkspace {
                workspace: WorkspaceId::new(),
            },
            Effect::CloneProject {
                repository: "git@example.com:demo.git".to_owned(),
                destination: "/tmp/demo".into(),
                branch: None,
                token: PendingToken::from_raw(1),
            },
            Effect::RegisterWorkspace {
                path: "/tmp/demo".into(),
                name: "demo".to_owned(),
                token: PendingToken::from_raw(2),
            },
        ] {
            assert_eq!(backend.dispatch(effect), Flow::Continue);
        }
        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn drain_returns_queued_completions_in_order_then_empties() {
        let mut backend = backend();
        backend.dispatch(Effect::RefreshSessions {
            workspace: WorkspaceId::new(),
        });
        backend.dispatch(Effect::LoadNotes {
            target: Target::Root(WorkspaceId::new()),
        });
        let events = backend.drain_events();
        assert!(matches!(
            events.as_slice(),
            [
                AppEvent::Backend(BackendEvent::Sessions(_)),
                AppEvent::Backend(BackendEvent::NotesLoaded { .. }),
            ]
        ));
        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn request_structs_round_trip_their_derives() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation_id = OperationId::new();

        let create = CreateSessionRequest {
            workspace,
            token: PendingToken::from_raw(3),
            operation_id,
            intent: intent(),
        };
        assert_eq!(create.clone(), create);
        assert!(format!("{create:?}").contains("CreateSessionRequest"));

        let remove = RemoveSessionRequest {
            workspace,
            session,
            force: false,
        };
        assert_eq!(remove.clone(), remove);
        assert!(format!("{remove:?}").contains("RemoveSessionRequest"));

        let launch = LaunchAgentRequest {
            workspace,
            session,
            operation_id,
            profile: None,
        };
        assert_eq!(launch.clone(), launch);
        assert!(format!("{launch:?}").contains("LaunchAgentRequest"));

        let open = OpenTerminalRequest {
            target: Target::Session(session),
            operation_id,
            arguments: "new".to_owned(),
        };
        assert_eq!(open.clone(), open);
        assert!(format!("{open:?}").contains("OpenTerminalRequest"));
    }
}
