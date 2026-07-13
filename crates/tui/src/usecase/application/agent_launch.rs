//! Daemon-authoritative Agent launch adapter for Closeup panes.
//!
//! This adapter translates the controller's product-neutral launch effect into
//! one daemon request, records a pending Agent tab before transport work, and
//! applies only a fenced daemon completion to [`PaneRuntime`].  It owns no PTY
//! or process-spawn capability.

use usagi_core::{
    domain::id::{OperationId, TerminalRef},
    usecase::client::{AgentLaunchIntent, DaemonClient, DaemonReply, DaemonRequest, IpcClient},
};

use super::{
    controller::Effect,
    pane::{PaneEvent, PaneKind},
    pane_runtime::{PaneRuntime, TerminalPort},
};

/// Safe projection of a daemon Agent-launch lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLaunchEvent {
    /// The durable operation was admitted. The pending pane remains visible.
    Accepted { operation: OperationId },
    /// Only this fenced terminal reference may become an attached Agent pane.
    Succeeded {
        operation: OperationId,
        terminal: TerminalRef,
    },
    /// A presentation-safe daemon failure. Raw adapter/process details never
    /// cross this boundary.
    Failed {
        operation: OperationId,
        message: String,
    },
}

/// The daemon launch surface required by the TUI.
pub trait AgentLaunchPort {
    /// Submit a durable launch intent exactly once.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon admission or transport failure.
    fn launch(
        &mut self,
        operation: OperationId,
        intent: AgentLaunchIntent,
    ) -> Result<AgentLaunchEvent, String>;
}

/// IPC transport implementation. An accepted reply deliberately leaves the
/// pane pending; a later subscribed/replayed [`AgentLaunchEvent::Succeeded`]
/// is the only way it becomes attachable.
impl<S: std::io::Read + std::io::Write> AgentLaunchPort for IpcClient<S> {
    fn launch(
        &mut self,
        operation: OperationId,
        intent: AgentLaunchIntent,
    ) -> Result<AgentLaunchEvent, String> {
        let expected = operation.as_str();
        match self
            .request(DaemonRequest::Agent {
                operation_id: expected.clone(),
                intent,
            })
            .map_err(|_| "daemon unavailable".to_owned())?
        {
            DaemonReply::Accepted {
                operation_id,
                revision: _,
            } if operation_id == expected => Ok(AgentLaunchEvent::Accepted { operation }),
            DaemonReply::Accepted { .. } => {
                Err("daemon returned a mismatched operation".to_owned())
            }
            DaemonReply::Ok(_) => Err("daemon did not accept agent launch".to_owned()),
        }
    }
}

/// Executes Agent effects and joins their final events to the existing pane
/// runtime. `P` is also the terminal attach transport, keeping launch and
/// attach daemon-authoritative through the same injected client boundary.
pub struct AgentLaunchAdapter<P> {
    port: P,
}

impl<P> AgentLaunchAdapter<P> {
    #[must_use]
    pub fn new(port: P) -> Self {
        Self { port }
    }

    #[must_use]
    pub fn port(&self) -> &P {
        &self.port
    }

    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }
}

impl<P: AgentLaunchPort + TerminalPort> AgentLaunchAdapter<P> {
    /// Dispatch one controller effect when it is an Agent launch. Other
    /// effects belong to their existing runners and are intentionally ignored.
    pub fn dispatch(&mut self, runtime: &mut PaneRuntime, effect: Effect) {
        let Effect::LaunchAgent {
            workspace,
            session,
            operation_id,
            profile,
        } = effect
        else {
            return;
        };
        runtime.dispatch(
            &mut self.port,
            PaneEvent::Request {
                operation: operation_id,
                target: super::controller::Target::Session(session),
                kind: PaneKind::Agent,
            },
        );
        let intent = AgentLaunchIntent {
            workspace,
            session,
            profile,
        };
        match self.port.launch(operation_id, intent) {
            Ok(event) => self.apply(runtime, event),
            Err(_) => runtime.dispatch(
                &mut self.port,
                PaneEvent::Failed {
                    operation: operation_id,
                    message: "daemon unavailable; reconnect to continue".to_owned(),
                },
            ),
        }
    }

    /// Apply a replayed or subscribed daemon lifecycle event. Unknown,
    /// duplicate, and stale completions are harmless because `PaneState`
    /// accepts a success only for its matching pending operation.
    pub fn apply(&mut self, runtime: &mut PaneRuntime, event: AgentLaunchEvent) {
        match event {
            AgentLaunchEvent::Accepted { .. } => {}
            AgentLaunchEvent::Succeeded {
                operation,
                terminal,
            } => runtime.dispatch(
                &mut self.port,
                PaneEvent::Succeeded {
                    operation,
                    terminal,
                },
            ),
            AgentLaunchEvent::Failed { operation, message } => {
                runtime.dispatch(&mut self.port, PaneEvent::Failed { operation, message });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::usecase::application::{
        controller::Target,
        pane::{PaneSelection, PaneTab},
        pane_runtime::{Geometry, TerminalError, TerminalInventory, TerminalSnapshot},
    };
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };

    #[derive(Default)]
    struct FakePort {
        launches: Vec<(OperationId, AgentLaunchIntent)>,
        events: VecDeque<Result<AgentLaunchEvent, String>>,
        attachments: Vec<TerminalRef>,
    }

    impl AgentLaunchPort for FakePort {
        fn launch(
            &mut self,
            operation: OperationId,
            intent: AgentLaunchIntent,
        ) -> Result<AgentLaunchEvent, String> {
            self.launches.push((operation, intent));
            self.events.pop_front().expect("launch event")
        }
    }

    impl TerminalPort for FakePort {
        fn inventory(&mut self) -> Result<Vec<TerminalInventory>, TerminalError> {
            Ok(vec![])
        }
        fn attach(
            &mut self,
            terminal: &TerminalRef,
            _: Option<u64>,
        ) -> Result<TerminalSnapshot, TerminalError> {
            self.attachments.push(terminal.clone());
            Ok(TerminalSnapshot {
                terminal: terminal.clone(),
                output_offset: 0,
                geometry: Geometry { cols: 80, rows: 24 },
                replay: vec![],
                exited: false,
            })
        }
        fn resync(&mut self, _: &TerminalRef) -> Result<TerminalSnapshot, TerminalError> {
            unreachable!()
        }
        fn input(&mut self, _: &TerminalRef, _: &[u8]) -> Result<(), TerminalError> {
            Ok(())
        }
        fn resize(&mut self, _: &TerminalRef, _: Geometry) -> Result<(), TerminalError> {
            Ok(())
        }
        fn detach(&mut self, _: &TerminalRef) {}
    }

    fn terminal(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn launches_once_then_attaches_only_the_matching_fenced_completion() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let terminal = terminal(workspace, session);
        let port = FakePort {
            events: VecDeque::from([Ok(AgentLaunchEvent::Accepted { operation })]),
            ..FakePort::default()
        };
        let mut adapter = AgentLaunchAdapter::new(port);
        let mut runtime = PaneRuntime::new(super::super::pane::PaneState::new(
            PaneSelection::Target(Target::Session(session)),
        ));
        adapter.dispatch(
            &mut runtime,
            Effect::LaunchAgent {
                workspace,
                session,
                operation_id: operation,
                profile: None,
            },
        );
        adapter.apply(
            &mut runtime,
            AgentLaunchEvent::Succeeded {
                operation: OperationId::new(),
                terminal: terminal.clone(),
            },
        );
        assert!(adapter.port().attachments.is_empty());
        adapter.apply(
            &mut runtime,
            AgentLaunchEvent::Succeeded {
                operation,
                terminal: terminal.clone(),
            },
        );
        assert_eq!(adapter.port().attachments, vec![terminal]);
        assert!(
            matches!(&runtime.pane().tabs()[0], PaneTab::Live(live) if live.kind == PaneKind::Agent)
        );
    }
}
