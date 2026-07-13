//! Daemon-authoritative Agent launch adapter for Closeup panes.
//!
//! This adapter translates the controller's product-neutral launch effect into
//! one daemon request, records a pending Agent tab before transport work, and
//! applies only a fenced daemon completion to [`PaneRuntime`].  It owns no PTY
//! or process-spawn capability.

use std::collections::HashMap;

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
            DaemonReply::Ok(value) => {
                let terminal = value
                    .get("terminal")
                    .cloned()
                    .ok_or_else(|| "daemon did not accept agent launch".to_owned())
                    .and_then(|terminal| {
                        serde_json::from_value(terminal)
                            .map_err(|_| "daemon final had an invalid terminal".to_owned())
                    })?;
                (value.get("completed").and_then(serde_json::Value::as_bool) == Some(true))
                    .then_some(AgentLaunchEvent::Succeeded {
                        operation,
                        terminal,
                    })
                    .ok_or_else(|| "daemon did not complete agent launch".to_owned())
            }
        }
    }
}

/// Executes Agent effects and joins their final events to the existing pane
/// runtime. `P` is also the terminal attach transport, keeping launch and
/// attach daemon-authoritative through the same injected client boundary.
pub struct AgentLaunchAdapter<P> {
    port: P,
    /// An accepted operation is durable daemon-owned work.  Re-sending it on
    /// duplicate controller delivery could create ambiguous ownership, so the
    /// TUI records the producer-issued ID and waits for replay instead.
    submitted: HashMap<OperationId, AgentLaunchIntent>,
}

impl<P> AgentLaunchAdapter<P> {
    #[must_use]
    pub fn new(port: P) -> Self {
        Self {
            port,
            submitted: HashMap::new(),
        }
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
        let intent = AgentLaunchIntent {
            workspace,
            session,
            profile,
        };
        if self.submitted.contains_key(&operation_id) {
            return;
        }
        self.submitted.insert(operation_id, intent.clone());
        runtime.dispatch(
            &mut self.port,
            PaneEvent::Request {
                operation: operation_id,
                target: super::controller::Target::Session(session),
                kind: PaneKind::Agent,
            },
        );
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
            } => {
                let valid = self.submitted.get(&operation).is_some_and(|intent| {
                    terminal.workspace_id == intent.workspace
                        && terminal.session_id == Some(intent.session)
                });
                if valid {
                    runtime.dispatch(
                        &mut self.port,
                        PaneEvent::Succeeded {
                            operation,
                            terminal,
                        },
                    );
                    self.submitted.remove(&operation);
                }
            }
            AgentLaunchEvent::Failed { operation, message } => {
                if self.submitted.remove(&operation).is_some() {
                    runtime.dispatch(&mut self.port, PaneEvent::Failed { operation, message });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{self, Cursor, Read, Write};

    use super::*;
    use crate::usecase::application::{
        controller::Target,
        pane::{PaneSelection, PaneTab},
        pane_runtime::{Geometry, TerminalError, TerminalInventory, TerminalSnapshot},
    };
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };
    use usagi_core::infrastructure::ipc::{
        Bootstrap, BuildIdentity, ConnectionId, Envelope, EnvelopeKind, ErrorCode, GenerationRole,
        ProtocolError, ProtocolLimits, ProtocolVersion, RequestId, ResponseOutcome, ServerHello,
        write_json_frame,
    };

    struct Scripted {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Scripted {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn ipc_client(outcome: ResponseOutcome) -> IpcClient<Scripted> {
        let protocol = ProtocolVersion {
            generation: 1,
            revision: 1,
        };
        let generation = usagi_core::infrastructure::ipc::DaemonGeneration("daemon".into());
        let hello = Bootstrap::ServerHello(ServerHello {
            connection_nonce: "nonce".into(),
            connection_id: ConnectionId("connection".into()),
            daemon_generation: generation.clone(),
            generation_role: GenerationRole::Active,
            protocol,
            capabilities: vec![],
            build: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
            },
            limits: ProtocolLimits::default(),
        });
        let reply = Envelope {
            protocol,
            daemon_generation: generation,
            kind: EnvelopeKind::Response {
                request_id: RequestId("1".into()),
                outcome,
                body: serde_json::json!({}),
            },
        };
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello, 1_048_576).unwrap();
        write_json_frame(&mut input, &reply, 1_048_576).unwrap();
        IpcClient::connect(
            Scripted {
                input: Cursor::new(input),
                output: vec![],
            },
            "client".into(),
            "nonce".into(),
            usagi_core::usecase::client::ClientPolicy::tui(),
        )
        .unwrap()
    }

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
            Err(TerminalError::Unavailable)
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

    #[test]
    fn duplicate_effect_never_resends_a_durable_operation() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let port = FakePort {
            events: VecDeque::from([Ok(AgentLaunchEvent::Accepted { operation })]),
            ..FakePort::default()
        };
        let mut adapter = AgentLaunchAdapter::new(port);
        let mut runtime = PaneRuntime::new(super::super::pane::PaneState::new(
            PaneSelection::Target(Target::Session(session)),
        ));
        let effect = Effect::LaunchAgent {
            workspace,
            session,
            operation_id: operation,
            profile: None,
        };

        adapter.dispatch(&mut runtime, effect.clone());
        adapter.dispatch(&mut runtime, effect);

        assert_eq!(adapter.port().launches.len(), 1);
        assert_eq!(runtime.pane().tabs().len(), 1);
        assert!(matches!(runtime.pane().tabs()[0], PaneTab::Pending(_)));
    }

    #[test]
    fn final_for_another_session_never_attaches_the_pending_agent_tab() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
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
                operation,
                terminal: terminal(workspace, SessionId::new()),
            },
        );

        assert!(adapter.port().attachments.is_empty());
        assert!(matches!(runtime.pane().tabs()[0], PaneTab::Pending(_)));
    }

    #[test]
    fn ipc_port_accepts_only_the_matching_operation_and_safe_errors() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let intent = AgentLaunchIntent {
            workspace,
            session,
            profile: None,
        };
        let mut accepted = ipc_client(ResponseOutcome::Accepted {
            operation_id: usagi_core::infrastructure::ipc::OperationId(operation.as_str()),
            operation_revision: 1,
        });
        assert_eq!(
            accepted.launch(operation, intent.clone()),
            Ok(AgentLaunchEvent::Accepted { operation })
        );

        let mut mismatched = ipc_client(ResponseOutcome::Accepted {
            operation_id: usagi_core::infrastructure::ipc::OperationId("other".into()),
            operation_revision: 1,
        });
        assert_eq!(
            mismatched.launch(operation, intent.clone()),
            Err("daemon returned a mismatched operation".to_owned())
        );
        let mut not_accepted = ipc_client(ResponseOutcome::Ok);
        assert_eq!(
            not_accepted.launch(operation, intent.clone()),
            Err("daemon did not accept agent launch".to_owned())
        );
        let mut unavailable = ipc_client(ResponseOutcome::Error(ProtocolError::new(
            ErrorCode::Unavailable,
            "private detail",
        )));
        assert_eq!(
            unavailable.launch(operation, intent),
            Err("daemon unavailable".to_owned())
        );
    }

    #[test]
    fn scripted_transport_records_client_bytes() {
        let mut transport = Scripted {
            input: Cursor::new(vec![]),
            output: vec![],
        };
        transport.write_all(b"request").unwrap();
        transport.flush().unwrap();
        assert_eq!(transport.output, b"request");
    }

    #[test]
    fn adapter_ignores_other_effects_and_projects_safe_failures() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let port = FakePort {
            events: VecDeque::from([Err("private daemon detail".to_owned())]),
            ..FakePort::default()
        };
        let mut adapter = AgentLaunchAdapter::new(port);
        let mut runtime = PaneRuntime::new(super::super::pane::PaneState::new(
            PaneSelection::Target(Target::Session(session)),
        ));
        adapter.dispatch(
            &mut runtime,
            Effect::OpenTerminal {
                target: Target::Session(session),
                arguments: "open".to_owned(),
            },
        );
        assert!(adapter.port().launches.is_empty());
        adapter.dispatch(
            &mut runtime,
            Effect::LaunchAgent {
                workspace,
                session,
                operation_id: operation,
                profile: None,
            },
        );
        assert_eq!(
            runtime.pane().error(),
            Some("daemon unavailable; reconnect to continue")
        );
        adapter.apply(
            &mut runtime,
            AgentLaunchEvent::Failed {
                operation,
                message: "safe failure".to_owned(),
            },
        );
        assert_eq!(
            runtime.pane().error(),
            Some("daemon unavailable; reconnect to continue")
        );

        let terminal = terminal(workspace, session);
        assert!(adapter.port_mut().inventory().unwrap().is_empty());
        assert_eq!(
            adapter.port_mut().resync(&terminal),
            Err(TerminalError::Unavailable)
        );
        adapter.port_mut().input(&terminal, b"x").unwrap();
        adapter
            .port_mut()
            .resize(&terminal, Geometry { cols: 80, rows: 24 })
            .unwrap();
        adapter.port_mut().detach(&terminal);
    }
}
