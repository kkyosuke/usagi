//! Concrete daemon-owned adapter from the shared IPC terminal vocabulary to
//! the generic terminal coordinator.

#![allow(
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)] // IPC actor signatures deliberately carry the complete fencing vocabulary.
#![coverage(off)] // This injected composition boundary is covered by the fake-PTY contract test.

use serde_json::{Value, json};
use usagi_core::{
    domain::id::{
        ClientId, CompletionFence, ConnectionId, DaemonGeneration, OperationId, RequestId,
        TerminalId, TerminalRef,
    },
    infrastructure::ipc::{ErrorCode, ProtocolError},
    usecase::client::{TerminalAction, TerminalGeometry, TerminalRequest},
};

use crate::presentation::ipc::TerminalOwner;

use super::{
    generic_terminal::{
        GenericPtySpawner, GenericTerminalCoordinator, GenericTerminalError,
        TerminalProfileResolver, TerminalStore,
    },
    terminal::{Geometry, InputRequest, PtyWriter, RegistryError},
};

/// Injected process boundary used by the runtime.  It is intentionally the
/// only component allowed to interact with a PTY master.
pub trait TerminalPty: GenericPtySpawner + PtyWriter {}
impl<T: GenericPtySpawner + PtyWriter> TerminalPty for T {}

/// Single-owner runtime used by the IPC server.  It contains no client-side
/// process fallback path.
pub struct GenericTerminalRuntime<R, S, P> {
    generation: DaemonGeneration,
    coordinator: GenericTerminalCoordinator,
    resolver: R,
    store: S,
    pty: P,
}

impl<R, S, P> GenericTerminalRuntime<R, S, P> {
    pub fn new(generation: DaemonGeneration, resolver: R, store: S, pty: P) -> Self {
        Self {
            generation,
            coordinator: GenericTerminalCoordinator::new(16, 64 * 1024, 64),
            resolver,
            store,
            pty,
        }
    }
    pub fn output(
        &mut self,
        terminal: &TerminalRef,
        bytes: Vec<u8>,
    ) -> Result<Value, ProtocolError> {
        self.coordinator
            .output(terminal, bytes)
            .map(|output| json!({"event":"output", "output": output}))
            .map_err(map_error)
    }
    pub fn exit(&mut self, terminal: &TerminalRef, status: i32) -> Result<(), ProtocolError>
    where
        S: TerminalStore,
    {
        self.coordinator
            .exit(terminal, status, &mut self.store)
            .map_err(map_error)
    }
}

impl<R: TerminalProfileResolver, S: TerminalStore, P: TerminalPty> TerminalOwner
    for GenericTerminalRuntime<R, S, P>
{
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        payload: Value,
    ) -> Result<Value, ProtocolError> {
        let request: TerminalRequest = serde_json::from_value(payload).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid terminal request vocabulary",
            )
        })?;
        match (action, request) {
            (TerminalAction::Launch, TerminalRequest::Launch { intent }) => {
                let Some(session_id) = intent.request.scope.session_id else {
                    return Err(ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "workspace-root terminal launch is not yet bound to a durable session fence",
                    ));
                };
                let terminal = TerminalRef {
                    daemon_generation: self.generation,
                    terminal_id: TerminalId::new(),
                    workspace_id: intent.request.scope.workspace_id,
                    session_id: Some(session_id),
                    worktree_id: intent.request.scope.worktree_id,
                };
                let fence = CompletionFence {
                    workspace_id: terminal.workspace_id,
                    session_id,
                    operation_id: OperationId::new(),
                    owner_daemon_generation: terminal.daemon_generation,
                    execution_attempt: 1,
                    lifecycle_attempt: 1,
                    expected_revision: 0,
                };
                let geometry = geometry(intent.geometry)?;
                self.coordinator
                    .launch(
                        &intent.request,
                        terminal.clone(),
                        fence,
                        geometry,
                        &mut self.resolver,
                        &mut self.store,
                        &mut self.pty,
                    )
                    .map_err(map_error)?;
                Ok(json!({"terminal": terminal}))
            }
            (TerminalAction::Inventory, TerminalRequest::Inventory { scope }) => {
                Ok(json!({"terminals": self.coordinator.inventory(&scope.scope)}))
            }
            (TerminalAction::Attach, TerminalRequest::Attach { terminal }) => self
                .coordinator
                .attach(&terminal, connection)
                .map(|attached| json!(attached))
                .map_err(map_error),
            (
                TerminalAction::Resume,
                TerminalRequest::Resume {
                    terminal,
                    after_offset,
                },
            ) => self
                .coordinator
                .replay_from(&terminal, after_offset)
                .map(|output| json!({"output": output}))
                .map_err(map_error),
            (TerminalAction::Resync, TerminalRequest::Resync { terminal }) => self
                .coordinator
                .terminal_snapshot(&terminal)
                .map(|snapshot| json!(snapshot))
                .map_err(map_error),
            (
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal,
                    geometry: size,
                },
            ) => self
                .coordinator
                .resize(&terminal, geometry(size)?)
                .map(|snapshot| json!(snapshot))
                .map_err(map_error),
            (
                TerminalAction::Detach,
                TerminalRequest::Detach {
                    terminal,
                    subscription,
                },
            ) => {
                self.coordinator
                    .detach(&terminal, subscription, connection)
                    .map_err(map_error)?;
                Ok(json!({}))
            }
            (
                TerminalAction::Input,
                TerminalRequest::Input {
                    terminal,
                    subscription,
                    input_seq,
                    bytes,
                },
            ) => self
                .input(
                    &terminal,
                    InputRequest {
                        subscription,
                        connection,
                        client,
                        request: request_id,
                        input_seq,
                    },
                    &bytes,
                )
                .map(|ack| json!({"ack": ack}))
                .map_err(map_error),
            _ => Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal action does not match its payload",
            )),
        }
    }
    fn disconnect(&mut self, connection: ConnectionId) {
        self.coordinator.disconnect(connection);
    }
}

impl<R: TerminalProfileResolver, S: TerminalStore, P: TerminalPty> GenericTerminalRuntime<R, S, P> {
    fn input(
        &mut self,
        terminal: &TerminalRef,
        input: InputRequest,
        bytes: &[u8],
    ) -> Result<super::terminal::InputAck, GenericTerminalError> {
        self.pty.select_terminal(terminal);
        self.coordinator
            .input(terminal, input, bytes, &mut self.pty)
    }
}

fn geometry(value: TerminalGeometry) -> Result<Geometry, ProtocolError> {
    (value.cols > 0 && value.rows > 0)
        .then_some(Geometry {
            cols: value.cols,
            rows: value.rows,
        })
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal geometry must be non-zero",
            )
        })
}
fn map_error(error: GenericTerminalError) -> ProtocolError {
    let code = match error {
        GenericTerminalError::Terminal(RegistryError::ResyncRequired) => ErrorCode::ResyncRequired,
        GenericTerminalError::UnknownTerminal
        | GenericTerminalError::TerminalGenerationMismatch
        | GenericTerminalError::Terminal(_) => ErrorCode::StaleTarget,
        GenericTerminalError::ConcurrencyExhausted => ErrorCode::ResourceExhausted,
        GenericTerminalError::ReconcileRequired(_) | GenericTerminalError::Store => {
            ErrorCode::OwnershipUnknown
        }
        GenericTerminalError::SpawnFailed => ErrorCode::Unavailable,
        GenericTerminalError::Launch(_) | GenericTerminalError::ScopeMismatch => {
            ErrorCode::InvalidArgument
        }
        GenericTerminalError::TerminalAlreadyExists => ErrorCode::RevisionConflict,
    };
    ProtocolError::new(
        code,
        "daemon terminal request could not be completed safely",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::{
        generation::ProcessIdentity,
        terminal::{PtyWriteError, SpawnFailure},
    };
    use std::{collections::BTreeMap, path::PathBuf};
    use usagi_core::domain::{
        id::{SessionId, WorkspaceId, WorktreeId},
        terminal_launch::{DurableTerminalLaunchSnapshot, TerminalLaunchScope, TerminalProfileId},
    };

    #[derive(Default)]
    struct Store;
    impl TerminalStore for Store {
        type Error = ();
        fn save(
            &mut self,
            _: super::super::generic_terminal::TerminalStoreSnapshot,
        ) -> Result<(), ()> {
            Ok(())
        }
    }
    struct Resolver;
    impl TerminalProfileResolver for Resolver {
        fn resolve(
            &mut self,
            request: &usagi_core::domain::terminal_launch::TerminalLaunchRequest,
        ) -> Result<
            usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
            usagi_core::domain::terminal_launch::TerminalLaunchValidationError,
        > {
            usagi_core::domain::terminal_launch::ResolvedTerminalLaunch::new(
                DurableTerminalLaunchSnapshot::new(
                    request.clone(),
                    1,
                    "/bin/sh",
                    vec![],
                    PathBuf::from("/"),
                    [],
                )?,
                BTreeMap::new(),
            )
        }
    }
    #[derive(Default)]
    struct Pty {
        writes: Vec<u8>,
    }
    impl GenericPtySpawner for Pty {
        fn spawn(
            &mut self,
            _: &usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
            _: &TerminalRef,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            Ok(ProcessIdentity {
                pid: 7,
                start_identity: "fake".into(),
                process_group: 7,
            })
        }
    }
    impl PtyWriter for Pty {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            self.writes.extend_from_slice(bytes);
            Ok(())
        }
    }
    fn call(
        runtime: &mut GenericTerminalRuntime<Resolver, Store, Pty>,
        connection: ConnectionId,
        client: ClientId,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> Value {
        runtime
            .request(
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
            )
            .unwrap()
    }
    #[test]
    fn fake_pty_covers_launch_attach_output_input_detach_reattach_and_exit() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let mut runtime =
            GenericTerminalRuntime::new(DaemonGeneration::new(), Resolver, Store, Pty::default());
        let connection = ConnectionId::new();
        let client = ClientId::new();
        let intent = usagi_core::usecase::client::TerminalLaunchIntent {
            request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope: TerminalLaunchScope {
                    workspace_id: workspace,
                    session_id: Some(session),
                    worktree_id: worktree,
                },
            },
            geometry: TerminalGeometry { cols: 80, rows: 24 },
        };
        let launched = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Launch,
            TerminalRequest::Launch { intent },
        );
        let terminal: TerminalRef = serde_json::from_value(launched["terminal"].clone()).unwrap();
        let attached = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
        );
        let subscription = attached["subscription"].as_u64().unwrap();
        runtime.output(&terminal, b"ready\n".to_vec()).unwrap();
        assert_eq!(
            call(
                &mut runtime,
                connection,
                client,
                TerminalAction::Input,
                TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription,
                    input_seq: 0,
                    bytes: b"echo ok\n".to_vec()
                }
            )["ack"],
            "Written"
        );
        call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Detach,
            TerminalRequest::Detach {
                terminal: terminal.clone(),
                subscription,
            },
        );
        assert_eq!(
            call(
                &mut runtime,
                connection,
                client,
                TerminalAction::Attach,
                TerminalRequest::Attach {
                    terminal: terminal.clone()
                }
            )["snapshot"]["output_offset"],
            6
        );
        runtime.exit(&terminal, 0).unwrap();
        assert_eq!(
            call(
                &mut runtime,
                connection,
                client,
                TerminalAction::Resync,
                TerminalRequest::Resync { terminal }
            )["exited"],
            0
        );
        assert_eq!(runtime.pty.writes, b"echo ok\n");
    }

    #[test]
    fn trimmed_generic_terminal_output_maps_to_a_resync_protocol_error() {
        let error = map_error(GenericTerminalError::Terminal(
            RegistryError::ResyncRequired,
        ));

        assert_eq!(error.code, ErrorCode::ResyncRequired);
    }
}
