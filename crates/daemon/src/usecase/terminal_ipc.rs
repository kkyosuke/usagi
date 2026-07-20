//! Concrete daemon-owned adapter from the shared IPC terminal vocabulary to
//! the generic terminal coordinator.

#![allow(
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)] // IPC actor signatures deliberately carry the complete fencing vocabulary.
#![coverage(off)] // This injected composition boundary is covered by the fake-PTY contract test.

use std::path::PathBuf;

use serde_json::{Value, json};
use usagi_core::{
    domain::{
        id::{
            ClientId, CompletionFence, ConnectionId, DaemonGeneration, OperationId, RequestId,
            TerminalId, TerminalRef,
        },
        terminal_launch::{
            DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
            TerminalLaunchScope, TerminalLaunchValidationError,
        },
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

/// Authoritative checkout returned only for an available managed-session
/// scope. The client never supplies its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTerminalScope {
    pub scope: TerminalLaunchScope,
    pub working_directory: PathBuf,
}

/// Safe failure returned when the managed-session owner cannot authorize a
/// requested terminal scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalScopeResolveError {
    Unavailable,
}

/// Resolves a complete generic-terminal scope through the managed-session
/// owner. A mismatched, unavailable, or workspace-root scope is rejected
/// before profile resolution and PTY spawn.
pub trait TerminalScopeResolver {
    fn resolve_available_scope(
        &self,
        scope: &TerminalLaunchScope,
    ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError>;
}

/// Applies the authoritative worktree path after a trusted profile resolves
/// program and environment. Reconstructing the durable snapshot makes the
/// request scope and spawned cwd one atomic launch boundary.
struct ScopedProfileResolver<'a, R> {
    profile: &'a mut R,
    working_directory: PathBuf,
}
impl<R: TerminalProfileResolver> TerminalProfileResolver for ScopedProfileResolver<'_, R> {
    fn resolve(
        &mut self,
        request: &TerminalLaunchRequest,
    ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
        let resolved = self.profile.resolve(request)?;
        let snapshot = DurableTerminalLaunchSnapshot::new(
            resolved.snapshot.request,
            resolved.snapshot.profile_revision,
            resolved.snapshot.program,
            resolved.snapshot.arguments,
            self.working_directory.clone(),
            resolved.snapshot.environment_allowlist,
        )?;
        ResolvedTerminalLaunch::new(snapshot, resolved.environment)
    }
}

/// Single-owner runtime used by the IPC server.  It contains no client-side
/// process fallback path.
pub struct GenericTerminalRuntime<R, S, P, Q> {
    generation: DaemonGeneration,
    coordinator: GenericTerminalCoordinator,
    resolver: R,
    store: S,
    pty: P,
    scope: Q,
}

impl<R, S, P, Q> GenericTerminalRuntime<R, S, P, Q> {
    pub fn new(generation: DaemonGeneration, resolver: R, store: S, pty: P, scope: Q) -> Self {
        Self {
            generation,
            coordinator: GenericTerminalCoordinator::new(16, 64 * 1024, 64),
            resolver,
            store,
            pty,
            scope,
        }
    }
    pub fn from_snapshot(
        generation: DaemonGeneration,
        resolver: R,
        store: S,
        pty: P,
        scope: Q,
        snapshot: super::generic_terminal::TerminalStoreSnapshot,
    ) -> Result<Self, GenericTerminalError> {
        Ok(Self {
            generation,
            coordinator: GenericTerminalCoordinator::from_snapshot(16, 64 * 1024, 64, snapshot)?,
            resolver,
            store,
            pty,
            scope,
        })
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

impl<R: TerminalProfileResolver, S: TerminalStore, P: TerminalPty, Q: TerminalScopeResolver>
    TerminalOwner for GenericTerminalRuntime<R, S, P, Q>
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
                // The scope's session is optional: `Some` is a managed session
                // and `None` is the workspace root. Either way the daemon owner
                // resolves the authoritative checkout path; the client never
                // supplies it, so a root launch cannot escape the trusted root.
                let resolved_scope = self
                    .scope
                    .resolve_available_scope(&intent.request.scope)
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "requested terminal scope is not an available managed scope",
                        )
                    })?;
                if resolved_scope.scope != intent.request.scope {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "requested terminal scope did not match the resolved scope",
                    ));
                }
                let terminal = TerminalRef {
                    daemon_generation: self.generation,
                    terminal_id: TerminalId::new(),
                    workspace_id: intent.request.scope.workspace_id,
                    session_id: intent.request.scope.session_id,
                    worktree_id: intent.request.scope.worktree_id,
                };
                let fence = CompletionFence {
                    workspace_id: terminal.workspace_id,
                    session_id: terminal.session_id,
                    operation_id: OperationId::new(),
                    owner_daemon_generation: terminal.daemon_generation,
                    execution_attempt: 1,
                    lifecycle_attempt: 1,
                    expected_revision: 0,
                };
                let geometry = geometry(intent.geometry)?;
                let mut resolver = ScopedProfileResolver {
                    profile: &mut self.resolver,
                    working_directory: resolved_scope.working_directory,
                };
                self.coordinator
                    .launch(
                        &intent.request,
                        terminal.clone(),
                        fence,
                        geometry,
                        &mut resolver,
                        &mut self.store,
                        &mut self.pty,
                    )
                    .map_err(map_error)?;
                Ok(json!({"terminal": terminal}))
            }
            (TerminalAction::Inventory, TerminalRequest::Inventory { scope }) => {
                Ok(json!({"terminals": self.coordinator.inventory(&scope)}))
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
            ) => {
                let output = self
                    .coordinator
                    .replay_from(&terminal, after_offset)
                    .map_err(map_error)?;
                let exited = self
                    .coordinator
                    .terminal_snapshot(&terminal)
                    .map_err(map_error)?
                    .exited
                    .is_some();
                Ok(json!({"output": output, "exited": exited}))
            }
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
            ) => {
                let geometry = geometry(size)?;
                self.coordinator
                    .ensure_running(&terminal)
                    .map_err(map_error)?;
                self.pty.resize(&terminal, geometry).map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "terminal resize failed")
                })?;
                self.coordinator
                    .resize(&terminal, geometry)
                    .map(|snapshot| json!(snapshot))
                    .map_err(map_error)
            }
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
    fn inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        self.coordinator.inventory(scope)
    }
    fn disconnect(&mut self, connection: ConnectionId) {
        self.coordinator.disconnect(connection);
    }
}

impl<R: TerminalProfileResolver, S: TerminalStore, P: TerminalPty, Q>
    GenericTerminalRuntime<R, S, P, Q>
{
    fn input(
        &mut self,
        terminal: &TerminalRef,
        input: InputRequest,
        bytes: &[u8],
    ) -> Result<super::terminal::InputAck, GenericTerminalError> {
        self.coordinator.ensure_running(terminal)?;
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
        GenericTerminalError::ReconcileRequired(_)
        | GenericTerminalError::Store
        | GenericTerminalError::InvalidSnapshot => ErrorCode::OwnershipUnknown,
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
        spawned_directories: Vec<PathBuf>,
        spawned_geometry: Option<Geometry>,
        resized: Vec<Geometry>,
    }
    impl GenericPtySpawner for Pty {
        fn spawn(
            &mut self,
            launch: &usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
            _: &TerminalRef,
            geometry: Geometry,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.spawned_directories
                .push(launch.snapshot.working_directory.clone());
            self.spawned_geometry = Some(geometry);
            Ok(ProcessIdentity {
                pid: 7,
                start_identity: "fake".into(),
                process_group: 7,
            })
        }
    }
    impl PtyWriter for Pty {
        fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
            self.resized.push(geometry);
            Ok(())
        }

        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            self.writes.extend_from_slice(bytes);
            Ok(())
        }
    }
    struct Scope {
        scope: TerminalLaunchScope,
        working_directory: PathBuf,
    }
    impl TerminalScopeResolver for Scope {
        fn resolve_available_scope(
            &self,
            _: &TerminalLaunchScope,
        ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
            Ok(ResolvedTerminalScope {
                scope: self.scope.clone(),
                working_directory: self.working_directory.clone(),
            })
        }
    }
    fn call(
        runtime: &mut GenericTerminalRuntime<Resolver, Store, Pty, Scope>,
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
        let working_directory = PathBuf::from("/available-worktree");
        let mut runtime = GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            Resolver,
            Store,
            Pty::default(),
            Scope {
                scope: TerminalLaunchScope {
                    workspace_id: workspace,
                    session_id: Some(session),
                    worktree_id: worktree,
                },
                working_directory: working_directory.clone(),
            },
        );
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
            geometry: TerminalGeometry { cols: 43, rows: 17 },
        };
        let launched = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Launch,
            TerminalRequest::Launch { intent },
        );
        let terminal: TerminalRef = serde_json::from_value(launched["terminal"].clone()).unwrap();
        assert_eq!(runtime.pty.spawned_directories, [working_directory]);
        assert_eq!(
            runtime.pty.spawned_geometry,
            Some(Geometry { cols: 43, rows: 17 })
        );
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
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry { cols: 92, rows: 31 },
                }
            )["geometry"],
            serde_json::json!({"cols": 92, "rows": 31})
        );
        assert_eq!(runtime.pty.resized, vec![Geometry { cols: 92, rows: 31 }]);
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
                TerminalAction::Resume,
                TerminalRequest::Resume {
                    terminal: terminal.clone(),
                    after_offset: 6,
                }
            )["exited"],
            true
        );
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
    fn rejects_a_scope_that_is_not_the_available_managed_session_before_spawn() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let available_worktree = WorktreeId::new();
        let mut runtime = GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            Resolver,
            Store,
            Pty::default(),
            Scope {
                scope: TerminalLaunchScope {
                    workspace_id: workspace,
                    session_id: Some(session),
                    worktree_id: available_worktree,
                },
                working_directory: PathBuf::from("/available-worktree"),
            },
        );
        let error = runtime
            .request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Launch,
                serde_json::to_value(TerminalRequest::Launch {
                    intent: usagi_core::usecase::client::TerminalLaunchIntent {
                        request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                            profile_id: TerminalProfileId::new("login-shell").unwrap(),
                            scope: TerminalLaunchScope {
                                workspace_id: workspace,
                                session_id: Some(session),
                                worktree_id: WorktreeId::new(),
                            },
                        },
                        geometry: TerminalGeometry { cols: 80, rows: 24 },
                    },
                })
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(runtime.pty.spawned_directories.is_empty());
    }

    #[test]
    fn trimmed_generic_terminal_output_maps_to_a_resync_protocol_error() {
        let error = map_error(GenericTerminalError::Terminal(
            RegistryError::ResyncRequired,
        ));

        assert_eq!(error.code, ErrorCode::ResyncRequired);
    }

    #[test]
    fn inventory_lists_only_in_scope_terminals_and_marks_live_until_exit() {
        use usagi_core::domain::terminal_launch::TerminalKind;

        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        let mut runtime = GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            Resolver,
            Store,
            Pty::default(),
            Scope {
                scope: scope.clone(),
                working_directory: PathBuf::from("/available-worktree"),
            },
        );
        let terminal: TerminalRef = serde_json::from_value(
            call(
                &mut runtime,
                ConnectionId::new(),
                ClientId::new(),
                TerminalAction::Launch,
                TerminalRequest::Launch {
                    intent: usagi_core::usecase::client::TerminalLaunchIntent {
                        request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                            profile_id: TerminalProfileId::new("login-shell").unwrap(),
                            scope: scope.clone(),
                        },
                        geometry: TerminalGeometry { cols: 80, rows: 24 },
                    },
                },
            )["terminal"]
                .clone(),
        )
        .unwrap();

        let live = TerminalOwner::inventory(&runtime, &scope);
        assert_eq!(live.len(), 1);
        assert!(live[0].terminal.fences(&terminal));
        assert_eq!(live[0].kind, TerminalKind::Terminal);
        assert!(live[0].live);

        // A different scope (foreign session) sees nothing.
        let foreign = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(SessionId::new()),
            worktree_id: worktree,
        };
        assert!(TerminalOwner::inventory(&runtime, &foreign).is_empty());

        // After the terminal exits it is no longer attachable (`live == false`).
        runtime.exit(&terminal, 0).unwrap();
        let exited = TerminalOwner::inventory(&runtime, &scope);
        assert_eq!(exited.len(), 1);
        assert!(!exited[0].live);
    }
}
