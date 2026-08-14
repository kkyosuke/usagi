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
            CompletionFence, ConnectionId, DaemonGeneration, OperationId, TerminalId, TerminalRef,
        },
        terminal_launch::{
            DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
            TerminalLaunchScope, TerminalLaunchValidationError,
        },
    },
    infrastructure::ipc::{ErrorCode, ProtocolError},
    usecase::{
        client::{TerminalGeometry, TerminalRequest},
        vt_screen::{COLS_MAX, ROWS_MAX},
    },
};

use super::{
    generic_terminal::{
        GenericPtySpawner, GenericTerminalCoordinator, GenericTerminalError,
        TerminalProfileResolver, TerminalStore,
    },
    terminal::{Geometry, InputRequest, PtyWriter, RegistryError},
    terminal_owner::{
        TerminalOwner as TerminalOwnerPort, TerminalRequestContext, TerminalResponse,
    },
    terminal_retention_ipc::SharedTerminalRetention,
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

/// How many generic terminals one daemon admits at a time.
///
/// It is also the generic terminal capacity pool's global limit, which every
/// retained generation shares and which is never implicitly summed with the Agent
/// pool ([`crate::usecase::resources::allocator::CapacityPolicy`]).
pub const GENERIC_TERMINAL_LIMIT: usize = 16;

impl<R, S, P, Q> GenericTerminalRuntime<R, S, P, Q> {
    pub fn new(generation: DaemonGeneration, resolver: R, store: S, pty: P, scope: Q) -> Self {
        Self {
            generation,
            coordinator: GenericTerminalCoordinator::new(GENERIC_TERMINAL_LIMIT, 64 * 1024, 64),
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
        Self::from_snapshot_with_retention(
            generation,
            resolver,
            store,
            pty,
            scope,
            snapshot,
            SharedTerminalRetention::new(),
        )
    }

    /// Restores a runtime bound to the daemon-wide retention authority, so
    /// generic terminals and Agent runtimes share one aggregate budget (#526).
    pub fn from_snapshot_with_retention(
        generation: DaemonGeneration,
        resolver: R,
        store: S,
        pty: P,
        scope: Q,
        snapshot: super::generic_terminal::TerminalStoreSnapshot,
        retention: SharedTerminalRetention,
    ) -> Result<Self, GenericTerminalError> {
        Ok(Self {
            generation,
            coordinator: GenericTerminalCoordinator::from_snapshot_with_retention(
                GENERIC_TERMINAL_LIMIT,
                64 * 1024,
                64,
                snapshot,
                retention,
            )?,
            resolver,
            store,
            pty,
            scope,
        })
    }

    /// Runs one bounded retention collection pass and applies its decisions to
    /// this owner's records and journals. The composition root drives it
    /// periodically so a daemon whose terminals are idle still ages its finals
    /// out of the budget.
    pub fn collect_retention_garbage(&mut self) -> usize
    where
        S: TerminalStore,
    {
        self.coordinator.retention().collect();
        self.coordinator.collect_garbage(&mut self.store)
    }

    /// The resource ids this owner still answers for.
    ///
    /// Durable state of a generation that is gone may only be collected once
    /// nothing retains its records any more, and this is the live half of that
    /// question ([`crate::usecase::resources::durable::ShardedRuntimeState::collect`]).
    #[must_use]
    pub fn retained_resources(&self) -> std::collections::BTreeSet<String> {
        self.coordinator
            .snapshot()
            .records
            .iter()
            .map(|record| record.terminal.terminal_id.as_str())
            .collect()
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
    TerminalOwnerPort for GenericTerminalRuntime<R, S, P, Q>
{
    fn handle(
        &mut self,
        context: TerminalRequestContext,
        request: TerminalRequest,
    ) -> Result<TerminalResponse, ProtocolError> {
        let TerminalRequestContext {
            connection,
            client,
            request: request_id,
        } = context;
        match request {
            TerminalRequest::Launch { intent } => {
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
                // The producer's own launch identity, when the client carries one
                // (#518). Keying the durable record on it is what makes a lost
                // response, a reconnect, or a restart replay this terminal instead
                // of spawning a second one for the same intent.
                let digest = intent.canonical_digest();
                if let Some(producer) = intent.launch_operation
                    && let Some(recorded) = self.coordinator.launch_by_operation(&producer)
                {
                    if recorded.launch_digest.as_deref() != Some(digest.as_str()) {
                        return Err(ProtocolError::new(
                            ErrorCode::IdempotencyConflict,
                            "launch operation id was accepted for a different intent",
                        ));
                    }
                    return Ok(TerminalResponse::Launch {
                        terminal: recorded.terminal.clone(),
                        launch_operation: producer,
                        replayed: true,
                    });
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
                    operation_id: intent.launch_operation.unwrap_or_else(OperationId::new),
                    owner_daemon_generation: terminal.daemon_generation,
                    execution_attempt: 1,
                    lifecycle_attempt: 1,
                    expected_revision: 0,
                };
                let launch_operation = fence.operation_id;
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
                // The accepted response echoes the producer's own id, so a client
                // that lost the first answer can resolve it without guessing.
                Ok(TerminalResponse::Launch {
                    terminal,
                    launch_operation,
                    replayed: false,
                })
            }
            TerminalRequest::Inventory { scope } => Ok(TerminalResponse::Inventory(
                self.coordinator.inventory(&scope),
            )),
            TerminalRequest::Attach { terminal } => self
                .coordinator
                .attach_for_client(&terminal, connection, client)
                .map(TerminalResponse::Attached)
                .map_err(map_error),
            TerminalRequest::Resume {
                terminal,
                after_offset,
            } => {
                let output = self
                    .coordinator
                    .replay_from(&terminal, after_offset, Some(&client))
                    .map_err(map_error)?;
                // Liveness only: an incremental poll must not pay for a
                // screen capture.
                let exited = self
                    .coordinator
                    .terminal_exit_status(&terminal)
                    .map_err(map_error)?
                    .is_some();
                Ok(TerminalResponse::Resumed { output, exited })
            }
            TerminalRequest::Resync { terminal } => self
                .coordinator
                .terminal_snapshot(&terminal)
                .map(TerminalResponse::Snapshot)
                .map_err(map_error),
            TerminalRequest::Resize {
                terminal,
                geometry: size,
            } => {
                let geometry = geometry(size)?;
                self.coordinator
                    .resize(&terminal, geometry, Some(&client), &mut self.pty)
                    .map(TerminalResponse::Snapshot)
                    .map_err(map_error)
            }
            TerminalRequest::Detach {
                terminal,
                subscription,
            } => {
                self.coordinator
                    .detach(&terminal, subscription, connection, &mut self.pty)
                    .map_err(map_error)?;
                Ok(TerminalResponse::Detached)
            }
            TerminalRequest::Input {
                terminal,
                subscription,
                input_seq,
                input_operation,
                bytes,
            } => self
                .input(
                    &terminal,
                    InputRequest {
                        subscription,
                        connection,
                        client,
                        request: request_id,
                        input_seq,
                        operation: input_operation,
                    },
                    &bytes,
                )
                .map(TerminalResponse::Input)
                .map_err(map_error),
            TerminalRequest::InputOutcome {
                terminal,
                input_operation,
            } => self
                .coordinator
                .input_outcome(&terminal, client, input_operation)
                .map(TerminalResponse::InputOutcome)
                .map_err(map_error),
            TerminalRequest::CompletedInventory { scope } => Ok(
                TerminalResponse::CompletedInventory(self.coordinator.completed_inventory(&scope)),
            ),
            TerminalRequest::Observe { .. } | TerminalRequest::Dismiss { .. } => {
                Err(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "terminal visibility request requires the shared owner",
                ))
            }
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
        self.coordinator.disconnect(connection, &mut self.pty);
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
        // One durable operation identity presented for different bytes or
        // another terminal: nothing was written and nothing is replayed.
        GenericTerminalError::Terminal(RegistryError::IdempotencyConflict) => {
            ErrorCode::IdempotencyConflict
        }
        GenericTerminalError::Terminal(RegistryError::IdempotencyExpired) => {
            ErrorCode::IdempotencyExpired
        }
        GenericTerminalError::Terminal(RegistryError::SequenceGap) => ErrorCode::SequenceGap,
        GenericTerminalError::UnknownTerminal
        | GenericTerminalError::TerminalGenerationMismatch
        | GenericTerminalError::Terminal(_) => ErrorCode::StaleTarget,
        // A launch whose worst-case final does not fit the aggregate retention
        // budget is refused before spawn, like any other exhausted capacity.
        GenericTerminalError::ConcurrencyExhausted
        | GenericTerminalError::RetentionExhausted(_) => ErrorCode::ResourceExhausted,
        // Retention collected this terminal's final. The client is told the
        // history expired rather than being handed another terminal's.
        GenericTerminalError::FinalEvicted(_) => ErrorCode::NotFound,
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
    use crate::usecase::terminal::SnapshotWire;
    use crate::usecase::terminal_owner::JsonTerminalOwner as TerminalOwner;
    use crate::usecase::{
        generation::ProcessIdentity,
        terminal::{PtyWriteError, SpawnFailure, TerminalReconcileState},
    };
    use std::{collections::BTreeMap, path::PathBuf};
    use usagi_core::domain::{
        id::{ClientId, RequestId, SessionId, WorkspaceId, WorktreeId},
        terminal_launch::{DurableTerminalLaunchSnapshot, TerminalLaunchScope, TerminalProfileId},
    };
    use usagi_core::usecase::client::TerminalAction;

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
                        launch_operation: None,
                    },
                },
            )["terminal"]
                .clone(),
        )
        .unwrap();
        (runtime, terminal)
    }

    /// A runtime whose scope resolver admits exactly `scope`.
    fn runtime_for(
        scope: TerminalLaunchScope,
    ) -> GenericTerminalRuntime<Resolver, Store, Pty, Scope> {
        GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            Resolver,
            Store::default(),
            Pty::default(),
            Scope {
                scope: scope.clone(),
                working_directory: PathBuf::from("/available-worktree"),
            },
        )
    }

    fn launch_request(
        scope: &TerminalLaunchScope,
        operation: Option<OperationId>,
        cols: u16,
    ) -> TerminalRequest {
        TerminalRequest::Launch {
            intent: usagi_core::usecase::client::TerminalLaunchIntent {
                request: TerminalLaunchRequest {
                    profile_id: TerminalProfileId::new("login-shell").unwrap(),
                    scope: scope.clone(),
                },
                geometry: TerminalGeometry { cols, rows: 24 },
                launch_operation: operation,
            },
        }
    }

    fn scope_of(session: Option<SessionId>) -> TerminalLaunchScope {
        TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: session,
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn a_repeated_producer_launch_replays_one_terminal_and_a_changed_intent_conflicts() {
        let scope = scope_of(Some(SessionId::new()));
        let mut runtime = runtime_for(scope.clone());
        let producer = OperationId::new();
        let connection = ConnectionId::new();
        let client = ClientId::new();

        let first = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Launch,
            launch_request(&scope, Some(producer), 80),
        );
        assert_eq!(first["launch_operation"], json!(producer));
        assert_eq!(first["replayed"], json!(false));

        // The response was lost and the client reconnected: the identical intent
        // answers with the same terminal instead of spawning a second one.
        let replay = call(
            &mut runtime,
            ConnectionId::new(),
            client,
            TerminalAction::Launch,
            launch_request(&scope, Some(producer), 80),
        );
        assert_eq!(replay["terminal"], first["terminal"]);
        assert_eq!(replay["launch_operation"], json!(producer));
        assert_eq!(replay["replayed"], json!(true));

        let inventory = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Inventory,
            TerminalRequest::Inventory {
                scope: scope.clone(),
            },
        );
        assert_eq!(
            inventory["terminals"].as_array().unwrap().len(),
            1,
            "one producer operation owns exactly one terminal"
        );

        // The same id with another geometry is a different request.
        let conflict = runtime
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Launch,
                serde_json::to_value(launch_request(&scope, Some(producer), 120)).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(conflict.code, ErrorCode::IdempotencyConflict);
        let after = call(
            &mut runtime,
            connection,
            client,
            TerminalAction::Inventory,
            TerminalRequest::Inventory { scope },
        );
        assert_eq!(
            after["terminals"], inventory["terminals"],
            "a conflict changes neither the terminal nor the inventory"
        );
    }

    #[test]
    fn a_launch_without_a_producer_id_keeps_its_server_issued_identity() {
        let scope = scope_of(None);
        let mut runtime = runtime_for(scope.clone());
        let body = call(
            &mut runtime,
            ConnectionId::new(),
            ClientId::new(),
            TerminalAction::Launch,
            launch_request(&scope, None, 80),
        );
        let issued: OperationId = serde_json::from_value(body["launch_operation"].clone()).unwrap();
        assert_eq!(body["replayed"], json!(false));
        // A peer that predates the producer id still gets one durable identity
        // back, and it is the daemon's own.
        assert_ne!(issued.as_str(), String::new());
    }

    #[test]
    fn a_hydrated_record_without_a_canonical_digest_never_proves_a_replay() {
        let scope = scope_of(Some(SessionId::new()));
        let generation = DaemonGeneration::new();
        let producer = OperationId::new();
        let terminal = TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: scope.workspace_id,
            session_id: scope.session_id,
            worktree_id: scope.worktree_id,
        };
        let record = super::super::generic_terminal::DurableTerminalRecord {
            terminal: terminal.clone(),
            operation: CompletionFence {
                workspace_id: terminal.workspace_id,
                session_id: terminal.session_id,
                operation_id: producer,
                owner_daemon_generation: generation,
                execution_attempt: 1,
                lifecycle_attempt: 1,
                expected_revision: 0,
            },
            launch: DurableTerminalLaunchSnapshot::new(
                TerminalLaunchRequest {
                    profile_id: TerminalProfileId::new("login-shell").unwrap(),
                    scope: scope.clone(),
                },
                1,
                "/bin/sh",
                vec![],
                PathBuf::from("/"),
                [],
            )
            .unwrap(),
            // A restart moves an unterminated record to `identity_unknown`; that
            // is the shape a hydrating owner actually holds.
            state: super::super::terminal::TerminalRuntimeState::ReconcileRequired(
                super::super::terminal::TerminalReconcileState::IdentityUnknown,
            ),
            process: None,
            launch_digest: None,
        };
        let mut runtime = GenericTerminalRuntime::from_snapshot(
            generation,
            Resolver,
            Store::default(),
            Pty::default(),
            Scope {
                scope: scope.clone(),
                working_directory: PathBuf::from("/available-worktree"),
            },
            super::super::generic_terminal::TerminalStoreSnapshot {
                schema_version:
                    super::super::generic_terminal::TerminalStoreSnapshot::SCHEMA_VERSION,
                records: vec![record],
            },
        )
        .unwrap();

        let refusal = runtime
            .request(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Launch,
                serde_json::to_value(launch_request(&scope, Some(producer), 80)).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(
            refusal.code,
            ErrorCode::IdempotencyConflict,
            "a legacy record cannot prove the intents match, so it refuses instead of guessing"
        );
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
    fn two_windows_on_one_terminal_are_answered_with_the_shared_viewport() {
        let (mut runtime, terminal) = launched_runtime();
        // Two windows of the same workspace: separate lanes, separate client
        // incarnations, one daemon terminal.
        let (wide_connection, wide_client) = (ConnectionId::new(), ClientId::new());
        let (narrow_connection, narrow_client) = (ConnectionId::new(), ClientId::new());
        let attach = |runtime: &mut _, connection, client| {
            call(
                runtime,
                connection,
                client,
                TerminalAction::Attach,
                TerminalRequest::Attach {
                    terminal: terminal.clone(),
                },
            )
        };
        let resize = |runtime: &mut _, connection, client, cols, rows| {
            call(
                runtime,
                connection,
                client,
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry { cols, rows },
                },
            )
        };

        attach(&mut runtime, wide_connection, wide_client);
        let wide = resize(&mut runtime, wide_connection, wide_client, 100, 40);
        assert_eq!(wide["geometry"], json!({ "cols": 100, "rows": 40 }));

        // The second window is smaller, so the terminal takes its size and the
        // large window is told so by its own next resize answer.
        attach(&mut runtime, narrow_connection, narrow_client);
        let narrow = resize(&mut runtime, narrow_connection, narrow_client, 40, 10);
        assert_eq!(narrow["geometry"], json!({ "cols": 40, "rows": 10 }));

        // Until the large window takes a fresh screen, its incremental poll is
        // refused: its screen is still 100 columns wide, and the output after it
        // was produced for a 40 column grid.
        let resumed = runtime
            .request(
                wide_connection,
                wide_client,
                RequestId::new(),
                TerminalAction::Resume,
                serde_json::to_value(TerminalRequest::Resume {
                    terminal: terminal.clone(),
                    after_offset: 0,
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(resumed.code, ErrorCode::ResyncRequired);

        let reattached = attach(&mut runtime, wide_connection, wide_client);
        assert_eq!(
            reattached["snapshot"]["geometry"],
            json!({ "cols": 40, "rows": 10 })
        );
        assert!(
            runtime
                .request(
                    wide_connection,
                    wide_client,
                    RequestId::new(),
                    TerminalAction::Resume,
                    serde_json::to_value(TerminalRequest::Resume {
                        terminal: terminal.clone(),
                        after_offset: 0,
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .is_ok()
        );
        assert_eq!(
            runtime.pty.resized,
            vec![
                Geometry {
                    cols: 100,
                    rows: 40
                },
                Geometry { cols: 40, rows: 10 }
            ]
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
            launch_operation: None,
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
                    input_operation: None,
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
                    input_operation: None,
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
                        launch_operation: None,
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
                            launch_operation: None,
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
        TerminalOwner::disconnect(&mut runtime, ConnectionId::new());
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
            GenericTerminalError::Terminal(RegistryError::IdempotencyExpired),
            GenericTerminalError::Terminal(RegistryError::SequenceGap),
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
            GenericTerminalError::RetentionExhausted(
                usagi_core::domain::terminal_retention::AdmissionRejection {
                    scope: usagi_core::domain::terminal_retention::RetentionScope::Daemon,
                    dimension: usagi_core::domain::terminal_retention::RetentionDimension::Count,
                },
            ),
            GenericTerminalError::FinalEvicted(
                usagi_core::domain::terminal_retention::EvictionReason::Pressure,
            ),
        ];
        let expected = [
            ErrorCode::ResourceExhausted,
            ErrorCode::Unavailable,
            ErrorCode::IdempotencyExpired,
            ErrorCode::SequenceGap,
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
            // An unreservable launch is exhausted capacity; a collected final
            // is expired history, not a stale or unknown terminal.
            ErrorCode::ResourceExhausted,
            ErrorCode::NotFound,
        ];
        for (error, code) in errors.into_iter().zip(expected) {
            assert_eq!(map_error(error).code, code);
        }
    }

    #[test]
    fn the_periodic_collector_ages_an_idle_owners_finals_out_of_the_budget() {
        use crate::usecase::terminal_retention_ipc::tests::{manual_retention, small_budget};

        let (retention, clock) = manual_retention();
        assert_eq!(retention.budget(), small_budget());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        let mut runtime = GenericTerminalRuntime::from_snapshot_with_retention(
            DaemonGeneration::new(),
            Resolver,
            Store::default(),
            Pty::default(),
            Scope {
                scope: scope.clone(),
                working_directory: PathBuf::from("/available-worktree"),
            },
            super::super::generic_terminal::TerminalStoreSnapshot::default(),
            retention.clone(),
        )
        .unwrap();
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
                            scope,
                        },
                        geometry: TerminalGeometry { cols: 80, rows: 24 },
                        launch_operation: None,
                    },
                },
            )["terminal"]
                .clone(),
        )
        .unwrap();
        runtime.exit(&terminal, 0).unwrap();
        // Nothing is due yet, so an idle tick collects nothing.
        assert_eq!(runtime.collect_retention_garbage(), 0);
        // While the record is retained, the durable state of a retired generation
        // that holds it may not be collected either (#562).
        assert_eq!(
            runtime.retained_resources(),
            std::iter::once(terminal.terminal_id.as_str()).collect()
        );
        clock.advance(1000);
        assert_eq!(runtime.collect_retention_garbage(), 1);
        assert!(retention.lookup(&terminal).marker().is_some());
        assert!(runtime.retained_resources().is_empty());
    }

    /// The wire contract of #519: one operation identity, one PTY write, and a
    /// read-only query that answers the same final on a different connection —
    /// including after the terminal has exited, when the write path is closed.
    #[test]
    fn durable_input_operations_replay_and_resolve_across_connections() {
        let (mut runtime, terminal) = launched_runtime();
        let client = ClientId::new();
        let first = ConnectionId::new();
        let subscription = call(
            &mut runtime,
            first,
            client,
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
        )["subscription"]
            .as_u64()
            .unwrap();
        let operation = OperationId::new();
        let input = |subscription: u64, input_seq: u64, input_operation: Option<OperationId>| {
            TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq,
                input_operation,
                bytes: b"ls\r".to_vec(),
            }
        };
        assert_eq!(
            call(
                &mut runtime,
                first,
                client,
                TerminalAction::Input,
                input(subscription, 0, Some(operation)),
            )["ack"],
            "Written"
        );

        // The connection dies before the client sees the acknowledgement.
        TerminalOwner::disconnect(&mut runtime, first);
        let second = ConnectionId::new();
        let fresh = call(
            &mut runtime,
            second,
            client,
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
        )["subscription"]
            .as_u64()
            .unwrap();

        // The resend on the fresh connection and subscription replays the
        // recorded final; the epoch-local sequence restarted at zero.
        assert_eq!(
            call(
                &mut runtime,
                second,
                client,
                TerminalAction::Input,
                input(fresh, 0, Some(operation)),
            )["ack"],
            serde_json::json!({ "Cached": "Written" })
        );
        // A genuinely new operation on the fresh connection is written, at
        // sequence zero: the epoch-local ledger restarted with the connection
        // even though the client incarnation (and its operation ledger) did not.
        assert_eq!(
            call(
                &mut runtime,
                second,
                client,
                TerminalAction::Input,
                TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription: fresh,
                    input_seq: 0,
                    input_operation: Some(OperationId::new()),
                    bytes: b"\r".to_vec(),
                },
            )["ack"],
            "Written"
        );
        let resolved = call(
            &mut runtime,
            second,
            client,
            TerminalAction::InputOutcome,
            TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: operation,
            },
        );
        assert_eq!(resolved["outcome"], "final");
        assert_eq!(resolved["ack"], "Written");
        assert_eq!(runtime.pty.writes, b"ls\r\r");

        // An operation the daemon never recorded is unknown, not an error and
        // not a success.
        assert_eq!(
            call(
                &mut runtime,
                second,
                client,
                TerminalAction::InputOutcome,
                TerminalRequest::InputOutcome {
                    terminal: terminal.clone(),
                    input_operation: OperationId::new(),
                },
            ),
            serde_json::json!({ "outcome": "unknown" })
        );

        // Reusing the identity for different bytes is a conflict with no write.
        let conflict = runtime
            .request(
                second,
                client,
                RequestId::new(),
                TerminalAction::Input,
                serde_json::to_value(TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription: fresh,
                    input_seq: 0,
                    input_operation: Some(operation),
                    bytes: b"rm -rf\r".to_vec(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap_err();
        assert_eq!(conflict.code, ErrorCode::IdempotencyConflict);
        assert_eq!(
            conflict.side_effect,
            usagi_core::infrastructure::ipc::SideEffect::None
        );
        assert_eq!(runtime.pty.writes, b"ls\r\r");

        // After exit the write path is closed, but the resolution query still
        // returns the final recorded before it.
        runtime.exit(&terminal, 0).unwrap();
        let after_exit = call(
            &mut runtime,
            second,
            client,
            TerminalAction::InputOutcome,
            TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: operation,
            },
        );
        assert_eq!(after_exit["ack"], "Written");
        // Another client's query for the same identity is unknown: the ledger is
        // scoped to the client incarnation that issued it.
        assert_eq!(
            call(
                &mut runtime,
                second,
                ClientId::new(),
                TerminalAction::InputOutcome,
                TerminalRequest::InputOutcome {
                    terminal,
                    input_operation: operation,
                },
            )["outcome"],
            "unknown"
        );
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
                        launch_operation: None,
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

    #[test]
    fn typed_generic_port_handles_completed_inventory_and_refuses_visibility_commands() {
        let scope = TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let mut runtime = runtime_for(scope.clone());
        let context = TerminalRequestContext {
            connection: ConnectionId::new(),
            client: ClientId::new(),
            request: RequestId::new(),
        };
        assert_eq!(
            TerminalOwnerPort::handle(
                &mut runtime,
                context,
                TerminalRequest::CompletedInventory { scope },
            )
            .unwrap(),
            TerminalResponse::CompletedInventory(Vec::new())
        );

        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        assert_eq!(
            TerminalOwnerPort::handle(
                &mut runtime,
                context,
                TerminalRequest::Observe {
                    terminal,
                    expected_revision: 0,
                },
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgument
        );
    }
}
