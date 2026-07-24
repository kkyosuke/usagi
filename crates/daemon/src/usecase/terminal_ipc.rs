//! Concrete daemon-owned adapter from the shared IPC terminal vocabulary to
//! the generic terminal coordinator.

#![allow(
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)] // IPC actor signatures deliberately carry the complete fencing vocabulary.

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
    usecase::{
        client::{TerminalAction, TerminalGeometry, TerminalRequest},
        vt_screen::{COLS_MAX, ROWS_MAX},
    },
};

use crate::presentation::ipc::TerminalOwner;

use super::{
    generic_terminal::{
        GenericPtySpawner, GenericTerminalCoordinator, GenericTerminalError,
        TerminalProfileResolver, TerminalStore,
    },
    terminal::{Geometry, InputRequest, PtyWriter, RegistryError, SnapshotWire},
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
        P: PtyWriter,
    {
        let result = self.coordinator.exit(terminal, status, &mut self.store);
        if matches!(
            result,
            Ok(())
                | Err(GenericTerminalError::ReconcileRequired(
                    super::terminal::TerminalReconcileState::PersistAfterExit
                ))
        ) {
            self.pty.release(terminal);
        }
        result.map_err(map_error)
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
        wire: SnapshotWire,
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
                    .map_err(map_scope_failure)?;
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
                .map(|attached| json!(attached.into_frame(wire)))
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
                // Liveness only: an incremental poll must not pay for a
                // screen capture.
                let exited = self
                    .coordinator
                    .terminal_exit_status(&terminal)
                    .map_err(map_error)?
                    .is_some();
                Ok(json!({"output": output, "exited": exited}))
            }
            (TerminalAction::Resync, TerminalRequest::Resync { terminal }) => self
                .coordinator
                .terminal_snapshot(&terminal)
                .map(|snapshot| json!(snapshot.into_frame(wire)))
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
                    .resize(&terminal, geometry, &mut self.pty)
                    .map(|snapshot| json!(snapshot.into_frame(wire)))
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
    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        self.coordinator.completed_inventory(scope)
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

/// Validates a requested geometry before it reaches a PTY or a decoded grid.
///
/// The daemon now allocates one screen per terminal, so an absurd geometry is a
/// memory amplifier: dimensions are bounded by the checkpoint's `ROWS_MAX` /
/// `COLS_MAX` and rejected rather than silently clamped.
pub(super) fn geometry(value: TerminalGeometry) -> Result<Geometry, ProtocolError> {
    let bounded = value.cols > 0
        && value.rows > 0
        && u32::from(value.rows) <= ROWS_MAX
        && u32::from(value.cols) <= COLS_MAX;
    bounded
        .then_some(Geometry {
            cols: value.cols,
            rows: value.rows,
        })
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal geometry must be non-zero and within the supported bounds",
            )
        })
}
fn map_scope_failure(_: TerminalScopeResolveError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InvalidArgument,
        "requested terminal scope is not an available managed scope",
    )
}
fn map_error(error: GenericTerminalError) -> ProtocolError {
    let code = match error {
        GenericTerminalError::Terminal(RegistryError::ResyncRequired) => ErrorCode::ResyncRequired,
        GenericTerminalError::Terminal(RegistryError::PtyResizeFailed)
        | GenericTerminalError::SpawnFailed => ErrorCode::Unavailable,
        // The screen does not fit one frame: no partial screen is emitted and
        // the client keeps its current state until a retry succeeds.
        GenericTerminalError::Terminal(RegistryError::CheckpointUnavailable) => {
            ErrorCode::ResourceExhausted
        }
        GenericTerminalError::UnknownTerminal
        | GenericTerminalError::TerminalGenerationMismatch
        | GenericTerminalError::Terminal(_) => ErrorCode::StaleTarget,
        GenericTerminalError::ConcurrencyExhausted => ErrorCode::ResourceExhausted,
        GenericTerminalError::ReconcileRequired(_)
        | GenericTerminalError::Store
        | GenericTerminalError::InvalidSnapshot => ErrorCode::OwnershipUnknown,
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
        terminal::{PtyWriteError, SpawnFailure, TerminalReconcileState},
    };
    use std::{collections::BTreeMap, path::PathBuf};
    use usagi_core::domain::{
        id::{SessionId, WorkspaceId, WorktreeId},
        terminal_launch::{DurableTerminalLaunchSnapshot, TerminalLaunchScope, TerminalProfileId},
    };

    #[derive(Default)]
    struct Store {
        fail: bool,
    }
    impl TerminalStore for Store {
        fn save(
            &mut self,
            _: super::super::generic_terminal::TerminalStoreSnapshot,
        ) -> Result<(), ()> {
            if self.fail { Err(()) } else { Ok(()) }
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
                )
                .expect("test launch snapshot uses canonical literals"),
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
        released: Vec<TerminalRef>,
        resize_failure: bool,
        resize_started: Option<std::sync::mpsc::SyncSender<()>>,
        resize_continue: Option<std::sync::mpsc::Receiver<()>>,
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
            if let Some(started) = &self.resize_started {
                started.send(()).unwrap();
            }
            if let Some(resume) = &self.resize_continue {
                resume.recv().unwrap();
            }
            if self.resize_failure {
                Err(PtyWriteError { applied_prefix: 0 })
            } else {
                Ok(())
            }
        }

        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            self.writes.extend_from_slice(bytes);
            Ok(())
        }

        fn release(&mut self, terminal: &TerminalRef) -> bool {
            self.released.push(terminal.clone());
            true
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
        call_on_wire(
            runtime,
            connection,
            client,
            action,
            request,
            SnapshotWire::RawTail,
        )
    }
    fn call_on_wire(
        runtime: &mut GenericTerminalRuntime<Resolver, Store, Pty, Scope>,
        connection: ConnectionId,
        client: ClientId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> Value {
        runtime
            .request(
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
                wire,
            )
            .unwrap()
    }
    fn launched_runtime() -> (
        GenericTerminalRuntime<Resolver, Store, Pty, Scope>,
        TerminalRef,
    ) {
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
            Store::default(),
            Pty::default(),
            Scope {
                scope: scope.clone(),
                working_directory: PathBuf::from("/available-worktree"),
            },
        );
        let terminal = serde_json::from_value(
            call(
                &mut runtime,
                ConnectionId::new(),
                ClientId::new(),
                TerminalAction::Launch,
                TerminalRequest::Launch {
                    intent: usagi_core::usecase::client::TerminalLaunchIntent {
                        request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                            profile_id: TerminalProfileId::new("login-shell").unwrap(),
                            scope,
                        },
                        geometry: TerminalGeometry { cols: 80, rows: 24 },
                    },
                },
            )["terminal"]
                .clone(),
        )
        .unwrap();
        (runtime, terminal)
    }

    #[test]
    fn resize_rejects_each_forged_terminal_ref_field_before_pty_effect() {
        let (mut runtime, terminal) = launched_runtime();
        let mut forged = Vec::new();
        let mut reference = terminal.clone();
        reference.daemon_generation = DaemonGeneration::new();
        forged.push(("daemon_generation", reference));
        let mut reference = terminal.clone();
        reference.terminal_id = TerminalId::new();
        forged.push(("terminal_id", reference));
        let mut reference = terminal.clone();
        reference.workspace_id = WorkspaceId::new();
        forged.push(("workspace_id", reference));
        let mut reference = terminal.clone();
        reference.session_id = Some(SessionId::new());
        forged.push(("session_id", reference));
        let mut reference = terminal;
        reference.worktree_id = WorktreeId::new();
        forged.push(("worktree_id", reference));

        for (field, terminal) in forged {
            let error = runtime
                .request(
                    ConnectionId::new(),
                    ClientId::new(),
                    RequestId::new(),
                    TerminalAction::Resize,
                    serde_json::to_value(TerminalRequest::Resize {
                        terminal,
                        geometry: TerminalGeometry {
                            cols: 100,
                            rows: 40,
                        },
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::StaleTarget, "forged {field}");
        }
        assert!(runtime.pty.resized.is_empty());
    }

    #[test]
    fn resize_failure_does_not_commit_geometry() {
        let (mut runtime, terminal) = launched_runtime();
        let before = runtime.coordinator.terminal_snapshot(&terminal).unwrap();
        runtime.pty.resize_failure = true;

        let error = runtime
            .request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Resize,
                serde_json::to_value(TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry {
                        cols: 100,
                        rows: 40,
                    },
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(runtime.pty.resized.len(), 1);
        assert_eq!(
            runtime.coordinator.terminal_snapshot(&terminal).unwrap(),
            before
        );
    }

    #[test]
    fn resize_holds_the_actor_lock_across_effect_and_commit() {
        use std::{
            sync::{Arc, Mutex, mpsc},
            time::Duration,
        };

        let (mut runtime, terminal) = launched_runtime();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (continue_tx, continue_rx) = mpsc::sync_channel(0);
        runtime.pty.resize_started = Some(started_tx);
        runtime.pty.resize_continue = Some(continue_rx);
        let runtime = Arc::new(Mutex::new(runtime));
        let resize_runtime = Arc::clone(&runtime);
        let resize_terminal = terminal.clone();
        let resize = std::thread::spawn(move || {
            resize_runtime.lock().unwrap().request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Resize,
                serde_json::to_value(TerminalRequest::Resize {
                    terminal: resize_terminal,
                    geometry: TerminalGeometry {
                        cols: 100,
                        rows: 40,
                    },
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
        });
        started_rx.recv().unwrap();

        // A screen capture cannot interleave with the resize either: the attach
        // blocks until geometry, revision and screen are committed together.
        let attach_runtime = Arc::clone(&runtime);
        let attach_terminal = terminal.clone();
        let (attach_tx, attach_rx) = mpsc::sync_channel(0);
        let attach = std::thread::spawn(move || {
            let attached = attach_runtime.lock().unwrap().request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Attach,
                serde_json::to_value(TerminalRequest::Attach {
                    terminal: attach_terminal,
                })
                .unwrap(),
                SnapshotWire::ScreenCheckpoint,
            );
            attach_tx.send(attached).unwrap();
        });
        assert!(attach_rx.recv_timeout(Duration::from_millis(50)).is_err());

        let exit_runtime = Arc::clone(&runtime);
        let exit_terminal = terminal.clone();
        let (exit_tx, exit_rx) = mpsc::sync_channel(0);
        let exit = std::thread::spawn(move || {
            let result = exit_runtime.lock().unwrap().exit(&exit_terminal, 0);
            exit_tx.send(result).unwrap();
        });
        assert!(exit_rx.recv_timeout(Duration::from_millis(50)).is_err());

        continue_tx.send(()).unwrap();
        assert_eq!(
            resize.join().unwrap().unwrap()["geometry"],
            json!({"cols":100,"rows":40})
        );
        // The captured screen is the post-resize one, never a mix of the two.
        let attached = attach_rx.recv().unwrap().unwrap();
        assert_eq!(
            attached["snapshot"]["geometry"],
            json!({"cols":100,"rows":40})
        );
        assert_eq!(
            attached["snapshot"]["screen"]["geometry"],
            json!({"cols":100,"rows":40})
        );
        assert_eq!(attached["snapshot"]["revision"], 1);
        attach.join().unwrap();
        exit_rx.recv().unwrap().unwrap();
        exit.join().unwrap();
        let runtime = runtime.lock().unwrap();
        assert_eq!(runtime.pty.resized.len(), 1);
        assert_eq!(
            runtime
                .coordinator
                .terminal_snapshot(&terminal)
                .unwrap()
                .exited,
            Some(0)
        );
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
            Store::default(),
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
        assert!(runtime.exit(&terminal, 0).is_err());
        assert_eq!(
            runtime.pty.released.as_slice(),
            std::slice::from_ref(&terminal)
        );
        let late_resize = runtime
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Resize,
                serde_json::to_value(TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(late_resize.code, ErrorCode::StaleTarget);
        let late_input = runtime
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Input,
                serde_json::to_value(TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription,
                    input_seq: 1,
                    bytes: b"late\n".to_vec(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(late_input.code, ErrorCode::StaleTarget);
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
            Store::default(),
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
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(runtime.pty.spawned_directories.is_empty());

        let invalid_scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: available_worktree,
        };
        let mut invalid_directory = GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            Resolver,
            Store::default(),
            Pty::default(),
            Scope {
                scope: invalid_scope.clone(),
                working_directory: PathBuf::new(),
            },
        );
        assert_eq!(
            invalid_directory
                .request(
                    ConnectionId::new(),
                    ClientId::new(),
                    RequestId::new(),
                    TerminalAction::Launch,
                    serde_json::to_value(TerminalRequest::Launch {
                        intent: usagi_core::usecase::client::TerminalLaunchIntent {
                            request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                                scope: invalid_scope,
                            },
                            geometry: TerminalGeometry { cols: 80, rows: 24 },
                        },
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn attach_resync_and_resize_follow_the_negotiated_snapshot_revision() {
        let (mut runtime, terminal) = launched_runtime();
        let connection = ConnectionId::new();
        let client = ClientId::new();
        runtime
            .output(&terminal, b"\x1b[1mbold\x1b[0m plain\r\nsecond".to_vec())
            .unwrap();

        // Revision 1 keeps the raw tail and its `[base_offset, output_offset)`
        // window; no checkpoint is put on that connection's wire.
        let legacy = call_on_wire(
            &mut runtime,
            connection,
            client,
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
            SnapshotWire::RawTail,
        );
        let legacy_snapshot = &legacy["snapshot"];
        assert!(legacy_snapshot["replay"].is_array());
        assert!(legacy_snapshot["screen"].is_null());
        assert_eq!(
            legacy_snapshot["base_offset"].as_u64().unwrap()
                + legacy_snapshot["replay"].as_array().unwrap().len() as u64,
            legacy_snapshot["output_offset"].as_u64().unwrap()
        );

        // Revision 2 carries the semantic screen instead, with no tail.
        for (action, request) in [
            (
                TerminalAction::Attach,
                TerminalRequest::Attach {
                    terminal: terminal.clone(),
                },
            ),
            (
                TerminalAction::Resync,
                TerminalRequest::Resync {
                    terminal: terminal.clone(),
                },
            ),
            (
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry { cols: 40, rows: 12 },
                },
            ),
        ] {
            let response = call_on_wire(
                &mut runtime,
                connection,
                client,
                action,
                request,
                SnapshotWire::ScreenCheckpoint,
            );
            let snapshot = response.get("snapshot").unwrap_or(&response);
            assert!(snapshot["replay"].is_null(), "no raw tail on revision 2");
            assert_eq!(
                snapshot["screen"]["schema_version"].as_u64(),
                Some(u64::from(usagi_core::usecase::vt_screen::SCHEMA_VERSION))
            );
            assert_eq!(snapshot["base_offset"], snapshot["output_offset"]);
            // The envelope geometry and the screen it carries always agree.
            assert_eq!(
                snapshot["geometry"]["rows"].as_u64(),
                snapshot["screen"]["geometry"]["rows"].as_u64()
            );
            assert_eq!(
                snapshot["geometry"]["cols"].as_u64(),
                snapshot["screen"]["geometry"]["cols"].as_u64()
            );
        }

        // Resume stays incremental on both revisions: raw suffix plus liveness.
        let resumed = call_on_wire(
            &mut runtime,
            connection,
            client,
            TerminalAction::Resume,
            TerminalRequest::Resume {
                terminal,
                after_offset: 0,
            },
            SnapshotWire::ScreenCheckpoint,
        );
        assert!(resumed["output"].is_array());
        assert_eq!(resumed["exited"], false);
    }

    #[test]
    fn a_geometry_beyond_the_screen_bounds_is_rejected_before_any_effect() {
        let (mut runtime, terminal) = launched_runtime();
        for size in [
            TerminalGeometry { cols: 1, rows: 0 },
            TerminalGeometry { cols: 0, rows: 1 },
            TerminalGeometry {
                cols: 1,
                rows: u16::try_from(ROWS_MAX).unwrap() + 1,
            },
            TerminalGeometry {
                cols: u16::try_from(COLS_MAX).unwrap() + 1,
                rows: 1,
            },
        ] {
            assert_eq!(
                runtime
                    .request(
                        ConnectionId::new(),
                        ClientId::new(),
                        RequestId::new(),
                        TerminalAction::Resize,
                        serde_json::to_value(TerminalRequest::Resize {
                            terminal: terminal.clone(),
                            geometry: size,
                        })
                        .unwrap(),
                        SnapshotWire::RawTail,
                    )
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument,
                "geometry {size:?}"
            );
        }
        // The largest supported geometry is accepted.
        assert!(
            geometry(TerminalGeometry {
                cols: u16::try_from(COLS_MAX).unwrap(),
                rows: u16::try_from(ROWS_MAX).unwrap(),
            })
            .is_ok()
        );
        assert!(runtime.pty.resized.is_empty());
    }

    #[test]
    fn trimmed_generic_terminal_output_maps_to_a_resync_protocol_error() {
        let error = map_error(GenericTerminalError::Terminal(
            RegistryError::ResyncRequired,
        ));

        assert_eq!(error.code, ErrorCode::ResyncRequired);
    }

    #[test]
    fn malformed_requests_geometry_and_every_error_family_are_typed() {
        let (mut runtime, terminal) = launched_runtime();
        runtime.disconnect(ConnectionId::new());
        let (mut failing_exit, failing_terminal) = launched_runtime();
        failing_exit.store.fail = true;
        assert_eq!(
            failing_exit.exit(&failing_terminal, 0).unwrap_err().code,
            ErrorCode::OwnershipUnknown
        );
        assert_eq!(failing_exit.pty.released, vec![failing_terminal]);
        assert_eq!(
            map_scope_failure(TerminalScopeResolveError::Unavailable).code,
            ErrorCode::InvalidArgument
        );
        let restored = GenericTerminalRuntime::from_snapshot(
            DaemonGeneration::new(),
            Resolver,
            Store::default(),
            Pty::default(),
            Scope {
                scope: TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
                working_directory: PathBuf::from("/"),
            },
            super::super::generic_terminal::TerminalStoreSnapshot::default(),
        );
        assert!(restored.is_ok());
        let invalid = super::super::generic_terminal::TerminalStoreSnapshot {
            schema_version: 0,
            ..Default::default()
        };
        assert!(
            GenericTerminalRuntime::from_snapshot(
                DaemonGeneration::new(),
                Resolver,
                Store::default(),
                Pty::default(),
                Scope {
                    scope: TerminalLaunchScope {
                        workspace_id: WorkspaceId::new(),
                        session_id: None,
                        worktree_id: WorktreeId::new(),
                    },
                    working_directory: PathBuf::from("/"),
                },
                invalid,
            )
            .is_err()
        );
        let malformed = runtime
            .request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Attach,
                json!({"unknown": true}),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(malformed.code, ErrorCode::InvalidArgument);
        let mismatch = runtime
            .request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Launch,
                serde_json::to_value(TerminalRequest::Attach { terminal }).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::InvalidArgument);
        assert_eq!(
            geometry(TerminalGeometry { cols: 1, rows: 0 })
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );

        let errors = [
            GenericTerminalError::Terminal(RegistryError::CheckpointUnavailable),
            GenericTerminalError::Terminal(RegistryError::PtyResizeFailed),
            GenericTerminalError::SpawnFailed,
            GenericTerminalError::UnknownTerminal,
            GenericTerminalError::TerminalGenerationMismatch,
            GenericTerminalError::Terminal(RegistryError::Exited),
            GenericTerminalError::ConcurrencyExhausted,
            GenericTerminalError::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            GenericTerminalError::Store,
            GenericTerminalError::InvalidSnapshot,
            GenericTerminalError::Launch(TerminalLaunchValidationError::InvalidProgram),
            GenericTerminalError::ScopeMismatch,
            GenericTerminalError::TerminalAlreadyExists,
        ];
        let expected = [
            ErrorCode::ResourceExhausted,
            ErrorCode::Unavailable,
            ErrorCode::Unavailable,
            ErrorCode::StaleTarget,
            ErrorCode::StaleTarget,
            ErrorCode::StaleTarget,
            ErrorCode::ResourceExhausted,
            ErrorCode::OwnershipUnknown,
            ErrorCode::OwnershipUnknown,
            ErrorCode::OwnershipUnknown,
            ErrorCode::InvalidArgument,
            ErrorCode::InvalidArgument,
            ErrorCode::RevisionConflict,
        ];
        for (error, code) in errors.into_iter().zip(expected) {
            assert_eq!(map_error(error).code, code);
        }
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
            Store::default(),
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
        assert_eq!(
            call(
                &mut runtime,
                ConnectionId::new(),
                ClientId::new(),
                TerminalAction::Inventory,
                TerminalRequest::Inventory {
                    scope: scope.clone(),
                },
            )["terminals"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
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

        // Before exit the terminal is not a completed tombstone.
        assert!(TerminalOwner::completed_inventory(&runtime, &scope).is_empty());

        // After the terminal exits it is no longer attachable (`live == false`).
        runtime.exit(&terminal, 0).unwrap();
        let exited = TerminalOwner::inventory(&runtime, &scope);
        assert_eq!(exited.len(), 1);
        assert!(!exited[0].live);

        // The exited terminal now appears as a completed tombstone (#525) with
        // its exit status; a foreign scope still sees none.
        let completed = TerminalOwner::completed_inventory(&runtime, &scope);
        assert_eq!(completed.len(), 1);
        assert!(completed[0].terminal.fences(&terminal));
        assert_eq!(completed[0].kind, TerminalKind::Terminal);
        assert_eq!(completed[0].exit_status, 0);
        assert!(TerminalOwner::completed_inventory(&runtime, &foreign).is_empty());
    }
}
