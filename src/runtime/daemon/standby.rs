//! standby generation の IPC・custody・昇格。

use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::workspace_state;

use super::{
    ACCEPT_ERROR_BACKOFF, AcceptedStream, ActiveOwner, AdmissionGate, AdmissionLease, Arc,
    BTreeSet, BuildIdentity, CLIENT_RETIREMENT_POLL, CapacityRefusalLog, ClientError,
    ClientWorkers, ClientWorkspace, DEFAULT_GENERATION_LIMIT, DaemonProcessObservation,
    DaemonRecord, DaemonRecordStore, DeadlineUnixStream, Duration,
    ESTABLISHED_RESPONSE_WRITE_DEADLINE_MS, EndpointCleanup, EndpointLocator, ErrorLog,
    EstablishedResponseWriter, ExactProcessControl, FORCED_SHUTDOWN_KILL_GRACE,
    FORCED_SHUTDOWN_POLL, FORCED_SHUTDOWN_TERM_GRACE, FsRecordFile, GenerationRegistry,
    GenerationRegistryFile, GenerationRole, Instant, LeaseClass, LivenessProbe, Mutex,
    OwnedRuntime, PRE_HANDSHAKE_CONNECTION_LIMIT, PRE_HANDSHAKE_DEADLINE, Path, PathBuf,
    PreHandshakeAdmission, PreHandshakeDeadlineStream, PreHandshakePermit, ProcessIdentity,
    ProcessObservation, RefCell, RegistryDocument, RetainedGenerationControl, RetiringReader,
    RoutingLedger, RuntimeHydration, STANDBY_CUSTODY_TICK, SecureUnixListener,
    ShutdownAcceptedStreamOnDrop, ShutdownOnWorkerPanic, ShutdownPipe, ShutdownRequest,
    StandbyAuthority, StandbyCustody, StandbyEndpoint, StandbyProbe, SystemClock,
    admissible_active, bind_ipc_listener, classify_request, client_connection_capacity_available,
    client_connection_limit, connect_generation, current_build, evaluate_custody,
    lifecycle_state_initialized, own_process_identity, prepare_standby, process_start_identity,
    read_registry_document, release_authority, retain_client_worker, route_cache,
    signal_exact_process, spawn_ipc_server,
};

/// The generations `generations.json` still lists, plus this one.
///
/// This process is registering itself as it starts, so its own entry may not be
/// durable yet; including it keeps the reclaim from ever considering the
/// hydrating generation's own claims. An absent registry is a daemon that has
/// never rolled over, which is a readable answer, not an unknown one.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=restart_hydrates_file_snapshot_before_dispatch_admission_and_preserves_ledger
pub(super) fn registered_generations(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
) -> Option<std::collections::BTreeSet<usagi_core::domain::id::DaemonGeneration>> {
    let document = read_registry_document(data_dir).ok()?;
    let mut registered = std::collections::BTreeSet::from([generation]);
    registered.extend(
        document
            .into_iter()
            .flat_map(|document| document.generations)
            .map(|entry| entry.generation),
    );
    Some(registered)
}

/// Exact process control for all non-retired generations in this data home.
///
/// `daemon.json` follows the active generation across a handoff, but a draining
/// predecessor legitimately remains alive with its PTYs and the singleton lock.
/// Cold lifecycle transitions therefore use the generation registry rather
/// than assuming the lifecycle record names every process that must stop.
pub(super) struct RegistryGenerationControl {
    pub(super) data_dir: PathBuf,
    /// How long SIGTERM is given to drain a generation before the transition
    /// escalates. Injected so a test can prove the escalation without waiting
    /// out the production grace.
    pub(super) term_grace: Duration,
    /// How long SIGKILL is given before the transition reports failure.
    pub(super) kill_grace: Duration,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-08-31 tests=forced_transition_stops_a_live_draining_generation_before_stale_cleanup,a_forced_transition_escalates_to_sigkill_when_sigterm_is_ignored
impl RegistryGenerationControl {
    pub(super) fn production(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            term_grace: FORCED_SHUTDOWN_TERM_GRACE,
            kill_grace: FORCED_SHUTDOWN_KILL_GRACE,
        }
    }

    pub(super) fn retained(&self) -> std::io::Result<Vec<ProcessIdentity>> {
        Ok(read_registry_document(&self.data_dir)
            .map_err(std::io::Error::other)?
            .into_iter()
            .flat_map(|document| document.generations)
            .filter(|entry| entry.role != GenerationRole::Retired)
            .map(|entry| entry.process)
            .collect())
    }

    pub(super) fn exactly_alive(process: &ProcessIdentity) -> std::io::Result<bool> {
        if process.start_identity.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "retained daemon generation has no process identity",
            ));
        }
        match process_start_identity(process.pid) {
            Ok(identity) => Ok(identity == process.start_identity),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(super) fn record(process: &ProcessIdentity) -> DaemonRecord {
        DaemonRecord {
            pid: process.pid,
            process_start_identity: Some(process.start_identity.clone()),
            started_at: chrono::Utc::now(),
        }
    }

    /// Signal every live retained generation once with `signal`, then wait up to
    /// `grace` for them to go away. Reports whether none is left.
    ///
    /// Running out of grace is not an error here: the caller decides whether it
    /// escalates to a stronger signal or is the final failure. Only a *refused*
    /// signal against a still-live process is an error, because that one says
    /// the transition cannot proceed at all.
    pub(super) fn signal_until_gone(
        &self,
        signal: libc::c_int,
        grace: Duration,
    ) -> std::io::Result<bool> {
        let deadline = Instant::now() + grace;
        let mut signalled = BTreeSet::new();
        loop {
            let mut live = false;
            // Re-read on every pass so a handoff that was already committing
            // while shutdown began cannot leave its newly retained successor
            // outside the fixed snapshot we signal.
            for process in self.retained()? {
                if !Self::exactly_alive(&process)? {
                    continue;
                }
                live = true;
                let identity = (process.pid, process.start_identity.clone());
                if signalled.insert(identity)
                    && let Err(error) = signal_exact_process(&Self::record(&process), signal)
                    && Self::exactly_alive(&process)?
                {
                    return Err(error);
                }
            }
            if !live {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(FORCED_SHUTDOWN_POLL);
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-08-31 tests=forced_transition_stops_a_live_draining_generation_before_stale_cleanup
impl RetainedGenerationControl for RegistryGenerationControl {
    fn has_live(&self) -> std::io::Result<bool> {
        for process in self.retained()? {
            if Self::exactly_alive(&process)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn shutdown_all(&self) -> std::io::Result<()> {
        // SIGTERM asks, SIGKILL insists. `--force` is the operator's escape
        // hatch, so it has to end with the generation actually gone: reporting
        // a timeout while leaving the process running is the one outcome that
        // strands the operator, because every later lifecycle command then
        // refuses on that same still-live generation. Escalation keeps the
        // exact process-identity fence, so a recycled pid is never what gets
        // killed.
        if self.signal_until_gone(libc::SIGTERM, self.term_grace)? {
            return Ok(());
        }
        if self.signal_until_gone(libc::SIGKILL, self.kill_grace)? {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "retained daemon generations survived SIGKILL; a daemon pid is wedged \
             in the kernel and the host has to be checked",
        ))
    }
}

/// The generations whose endpoints pre-bind reclamation must leave alone.
///
/// The sweep cannot tell a live standby's socket from a crashed generation's
/// leftover, because a standby holds no lock to be excluded by. This is the
/// durable answer it uses instead: a generation the registry still retains and
/// whose recorded process the OS proves is exactly the process recorded. An
/// unreadable registry names nothing, which is the safe direction for a *sweep*
/// — it reclaims what it can prove is residue and the registry's own recovery
/// still fails the authority closed.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_collection_pass_removes_the_shard_of_a_generation_nothing_retains
pub(super) fn live_generation_endpoints(data_dir: &Path) -> BTreeSet<String> {
    let Ok(Some(document)) = read_registry_document(data_dir) else {
        return BTreeSet::new();
    };
    document
        .generations
        .iter()
        .filter(|entry| {
            entry.role != usagi_daemon::usecase::generation::GenerationRole::Retired
                && observe_generation_process(&entry.process)
                    == ProcessObservation::VerifiedAlive(entry.process.clone())
        })
        .map(|entry| entry.generation.as_str())
        .collect()
}

/// The two intentionally distinct shutdown domains owned by a standby process.
///
/// `replacement` stops only the readiness accept loop so promotion can reuse its
/// listener. `process` wakes the lifecycle owner, which releases registry custody
/// and exits. Keeping them in one named value prevents two same-typed requests
/// from being swapped at the accept/client/custody composition seams.
#[derive(Clone)]
pub(super) struct StandbyShutdownDomains {
    pub(super) process: Arc<ShutdownRequest>,
    pub(super) replacement: Arc<ShutdownRequest>,
}

impl StandbyShutdownDomains {
    pub(super) fn new(process: Arc<ShutdownRequest>) -> Self {
        Self {
            process,
            replacement: Arc::new(ShutdownRequest::new()),
        }
    }

    pub(super) fn request_process(&self) {
        self.process.request();
    }

    pub(super) fn request_replacement(&self) {
        self.replacement.request();
    }

    pub(super) fn process_panic_guard(&self) -> ShutdownOnWorkerPanic {
        ShutdownOnWorkerPanic {
            shutdown: Arc::clone(&self.process),
        }
    }

    /// Arms the standby accept loop and its internal wake pipe as one lifetime.
    pub(super) fn accept_lifetime(
        &self,
        create_wake: fn(&Arc<ShutdownRequest>) -> std::io::Result<ShutdownPipe>,
    ) -> std::io::Result<StandbyAcceptLifetime> {
        match create_wake(&self.replacement) {
            Ok(wake) => Ok(StandbyAcceptLifetime {
                shutdown: self.clone(),
                wake,
                completed: false,
            }),
            Err(error) => {
                self.request_process();
                Err(error)
            }
        }
    }
}

/// Couples standby accept completion with the wake pipe that could obscure it.
///
/// The guard decides whether the worker completed before its `wake` field drops.
/// Since [`ShutdownPipe::drop`] requests the replacement domain, keeping both in
/// this owner makes it impossible for field destruction to disguise an
/// unexpected accept-loop return as a planned promotion.
pub(super) struct StandbyAcceptLifetime {
    pub(super) shutdown: StandbyShutdownDomains,
    pub(super) wake: ShutdownPipe,
    pub(super) completed: bool,
}

impl StandbyAcceptLifetime {
    /// Completes only an explicitly requested replacement, and only when called
    /// after every accepted client has been retired.
    pub(super) fn finish_planned(&mut self) {
        self.completed = self.shutdown.replacement.is_requested();
    }
}

impl Drop for StandbyAcceptLifetime {
    fn drop(&mut self) {
        if !self.completed {
            self.shutdown.request_process();
        }
    }
}

/// The private endpoint a standby generation binds, and nothing else.
///
/// Everything the active [`IpcReady`] does that a standby must not do is simply
/// absent here: no locator publication, no runtime store reconcile or save, no
/// PTY / supervisor / PR / teardown worker, no spawn. What remains is a socket
/// that completes a readiness handshake and refuses every request through the
/// role admission fence.
pub(super) struct StandbyIpc<'a> {
    pub(super) data_dir: &'a Path,
    /// The workspace this process would take authority over. A standby reads
    /// that workspace's durable state to hydrate; it never adopts a new one.
    pub(super) workspace_root: PathBuf,
    pub(super) build: BuildIdentity,
    pub(super) pid: u32,
    pub(super) shutdown: StandbyShutdownDomains,
    pub(super) worker: Arc<Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>>,
    pub(super) listener: Arc<Mutex<Option<SecureUnixListener>>>,
    pub(super) cleanup: RefCell<Option<EndpointCleanup>>,
    /// The admission fence this process answers requests through. It is created
    /// at bind time in the `standby` role and never activated here: promoting it
    /// is the handoff's job, in a process that has taken the authority.
    pub(super) gate: RefCell<Option<AdmissionGate>>,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl<'a> StandbyIpc<'a> {
    /// Bind the standby endpoint seam for one `serve --standby` process.
    pub(super) fn new(
        data_dir: &'a Path,
        workspace_root: PathBuf,
        pid: u32,
        shutdown: Arc<ShutdownRequest>,
    ) -> Self {
        Self {
            data_dir,
            workspace_root,
            build: current_build(),
            pid,
            shutdown: StandbyShutdownDomains::new(shutdown),
            worker: Arc::new(Mutex::new(None)),
            listener: Arc::new(Mutex::new(None)),
            cleanup: RefCell::new(None),
            gate: RefCell::new(None),
        }
    }

    /// The generation and endpoint this process bound, read from the retained
    /// cleanup token so the registry entry and the accepting socket can only
    /// ever be the same generation.
    pub(super) fn bound_endpoint(&self) -> Option<EndpointLocator> {
        self.cleanup
            .borrow()
            .as_ref()
            .map(|cleanup| cleanup.locator().clone())
    }

    /// Read the durable runtime state without touching it.
    ///
    /// This is the whole of a standby's hydrate in this build: one read of the
    /// lifecycle store, which yields the workspace root the active generation
    /// took authority over and the state revision that read was sealed at. No
    /// reconcile, no save, no legacy migration — every one of those is a write,
    /// and the active generation is the only writer.
    ///
    /// An uninitialized store is refused rather than initialized: the process
    /// that initializes it is by definition the one that owns it.
    pub(super) fn hydrate(&self) -> std::io::Result<(PathBuf, u64)> {
        let store = usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(
            &standby_workspace_state_dir(&self.data_dir.join("daemon"), &self.workspace_root)?,
        );
        let (root, state) = store
            .load_with_workspace()
            .map_err(|error| std::io::Error::other(format!("{error:#}")))?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "durable runtime state is not initialized; a standby hydrates it read-only",
                )
            })?;
        Ok((root, state.state_revision))
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl StandbyEndpoint for StandbyIpc<'_> {
    fn bind(&self) -> std::io::Result<()> {
        // Hydrate first: a standby that cannot read the state it would serve has
        // nothing to prove by binding, and refusing here leaves no socket for a
        // rollback to reclaim.
        let (workspace_root, revision) = self.hydrate()?;
        let (listener, wire) = bind_ipc_listener(self.data_dir)?;
        let generation = usagi_core::domain::id::DaemonGeneration::parse(&wire.0)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let cleanup = listener.cleanup_handle();
        let gate = AdmissionGate::new(
            generation,
            usagi_daemon::usecase::generation::GenerationRole::Standby,
        );
        let protocol = usagi_daemon::presentation::ipc::standby_server_protocol(
            wire,
            generation.as_str(),
            self.build.clone(),
            // The standby asserts its *own* process, which is the only process it
            // can speak for. It is not the data directory's owner record and is
            // never written down; owner binding requires the `active` role, so no
            // client can mistake this for authority.
            DaemonRecord::identified(self.pid, process_start_identity(self.pid)?),
            paths::wire_workspace_root(&workspace_root),
        );
        let worker =
            spawn_standby_ipc_server(listener, protocol, gate.clone(), self.shutdown.clone());
        match worker {
            Ok(worker) => {
                *self.cleanup.borrow_mut() = Some(cleanup);
                *self.gate.borrow_mut() = Some(gate);
                *self
                    .worker
                    .lock()
                    .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))? =
                    Some(worker);
                ErrorLog::record(&format!(
                    "daemon standby hydrated read-only at runtime state revision {revision}"
                ));
                Ok(())
            }
            // The listener is dropped with the failure, and its Drop retires the
            // socket it bound. Nothing durable was written.
            Err(error) => Err(error),
        }
    }

    fn retire(&self) -> std::io::Result<()> {
        self.shutdown.request_process();
        self.shutdown.request_replacement();
        // Closing the two lease classes is what makes "stopped admitting"
        // observable to a request already in flight, rather than only to the next
        // connection.
        if let Some(gate) = self.gate.borrow().as_ref() {
            gate.close(LeaseClass::ActiveControl);
            gate.close(LeaseClass::OwnerTerminal);
        }
        let joined = match self
            .worker
            .lock()
            .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))?
            .take()
        {
            Some(worker) => worker
                .join()
                .map(|listener| {
                    if let Ok(mut retained) = self.listener.lock() {
                        *retained = Some(listener);
                    }
                })
                .map_err(|_| std::io::Error::other("daemon standby accept loop panicked")),
            None => Ok(()),
        };
        let cleanup = match self.cleanup.borrow().as_ref() {
            // A standby never published `current.json`, so this only ever removes
            // its own socket: the token refuses to touch a locator that names
            // another generation.
            Some(cleanup) => cleanup.retire(),
            None => Ok(()),
        };
        if cleanup.is_ok() {
            if let Ok(mut listener) = self.listener.lock() {
                listener.take();
            }
            self.cleanup.borrow_mut().take();
        }
        joined.and(cleanup)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl Drop for StandbyIpc<'_> {
    fn drop(&mut self) {
        // A panic unwinds past the state machine's own stand-down, and a socket
        // this process bound is a socket only this process can prove it owns.
        // Retirement is idempotent, so the ordinary path is unaffected.
        //
        // The guard matters because the composition root binds this seam for
        // *both* roles: an active `serve` never binds it, and dropping it must
        // not then request that process's shutdown.
        if self.cleanup.borrow().is_some() {
            let _ = StandbyEndpoint::retire(self);
        }
    }
}

/// Serve a standby's private endpoint.
///
/// The loop is deliberately not [`start_ipc_accept_loop`]: that one owns a
/// session runtime, a terminal runtime, an Agent runtime, a supervisor and a
/// PR projector, and a standby owns none of them. Every admitted connection here
/// gets a handshake and then a typed refusal.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
pub(super) fn spawn_standby_ipc_server(
    listener: SecureUnixListener,
    protocol: usagi_core::infrastructure::ipc::ServerProtocol,
    gate: AdmissionGate,
    shutdown: StandbyShutdownDomains,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    let connection_limit = client_connection_limit();
    std::thread::Builder::new()
        .name("usagi-ipc-standby".to_string())
        .spawn(move || {
            let workers = Arc::new(ClientWorkers::new());
            let pre_handshake = PreHandshakeAdmission::new(PRE_HANDSHAKE_CONNECTION_LIMIT);
            let mut capacity_log = CapacityRefusalLog::default();
            let mut lifetime = match shutdown.accept_lifetime(ShutdownPipe::mirroring) {
                Ok(lifetime) => lifetime,
                Err(error) => {
                    ErrorLog::record(&format!("daemon standby accept wait unavailable: {error}"));
                    return listener;
                }
            };
            while !shutdown.replacement.is_requested() {
                if !lifetime.wake.wait_for_listener(listener.readiness_fd()) {
                    break;
                }
                while !shutdown.replacement.is_requested() {
                    match listener.accept() {
                        Ok(stream) => {
                            if shutdown.replacement.is_requested() {
                                break;
                            }
                            let capacity_available = client_connection_capacity_available(
                                &workers,
                                connection_limit,
                            );
                            if capacity_log.should_record(capacity_available) {
                                ErrorLog::record(
                                    "daemon standby connection refused: client capacity exhausted",
                                );
                            }
                            if !capacity_available {
                                drop(stream);
                                continue;
                            }
                            let Some(pre_handshake_permit) = pre_handshake.try_admit() else {
                                ErrorLog::record(
                                    "daemon standby pre-handshake connection refused: capacity exhausted",
                                );
                                drop(stream);
                                continue;
                            };
                            let unblock = match stream.try_clone() {
                                Ok(stream) => AcceptedStream::new(stream),
                                Err(error) => {
                                    ErrorLog::record(&format!(
                                        "daemon standby connection refused: accepted stream could not be duplicated: {error}"
                                    ));
                                    continue;
                                }
                            };
                            match spawn_standby_client_worker(
                                stream,
                                unblock.clone(),
                                protocol.clone(),
                                gate.clone(),
                                pre_handshake_permit,
                                shutdown.clone(),
                            ) {
                                Ok(handle) => {
                                    retain_client_worker(&workers, Ok(unblock), handle);
                                }
                                Err(error) => ErrorLog::record(&format!(
                                    "daemon standby client worker unavailable: {error}"
                                )),
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
                    }
                }
            }
            let report = workers.retire();
            if !report.is_clean() {
                ErrorLog::record(&format!(
                    "daemon standby shutdown retired with client worker failures: {report:?}"
                ));
            }
            // Promotion and retirement request this private domain before they
            // join us. Mark the exit planned only after every retained client is
            // retired; an unwind at any earlier point must wake the process owner.
            lifetime.finish_planned();
            listener
        })
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
pub(super) fn spawn_standby_client_worker(
    stream: std::os::unix::net::UnixStream,
    completion: AcceptedStream,
    protocol: usagi_core::infrastructure::ipc::ServerProtocol,
    gate: AdmissionGate,
    pre_handshake_permit: PreHandshakePermit,
    shutdown: StandbyShutdownDomains,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-ipc-standby-client".to_string())
        .spawn(move || {
            let _panic = shutdown.process_panic_guard();
            let retirement = completion.retirement();
            let _completion = ShutdownAcceptedStreamOnDrop(Some(completion));
            if stream.set_nonblocking(false).is_err() {
                return;
            }
            let Ok(writer) = stream.try_clone() else {
                return;
            };
            let deadline = Instant::now() + PRE_HANDSHAKE_DEADLINE;
            let mut reader = PreHandshakeDeadlineStream::new(stream, deadline);
            let mut writer = PreHandshakeDeadlineStream::new(writer, deadline);
            let admitted = usagi_daemon::presentation::ipc::handshake_admitted(
                &mut reader,
                &mut writer,
                &protocol,
            );
            drop(pre_handshake_permit);
            let admitted = match admitted {
                Ok(Some(admitted)) => admitted,
                Ok(None) => return,
                Err(error) => {
                    let reason = if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        "deadline exceeded"
                    } else {
                        "invalid or incomplete hello"
                    };
                    ErrorLog::record(&format!(
                        "daemon standby pre-handshake connection refused: {reason}"
                    ));
                    return;
                }
            };
            if reader.clear_deadlines().is_err() || writer.clear_deadlines().is_err() {
                ErrorLog::record(
                    "daemon standby admitted connection closed: pre-handshake deadline could not be cleared",
                );
                return;
            }
            // Same reason as the active worker: a standby is retired by the same
            // barrier, and `shutdown(2)` can fail to return this parked read.
            let mut reader =
                RetiringReader::new(reader.into_inner(), retirement, CLIENT_RETIREMENT_POLL);
            let mut writer = EstablishedResponseWriter::new(
                SystemClock::new(),
                DeadlineUnixStream(writer.into_inner()),
                ESTABLISHED_RESPONSE_WRITE_DEADLINE_MS,
            );
            let _ = usagi_daemon::presentation::ipc::handle_admitted_connection_with(
                &mut reader,
                &mut writer,
                admitted,
                &mut |request_id, body, hello| {
                    standby_reply(&gate, request_id, &body, hello)
                },
            );
        })
}

/// The serving generation's authority over one client connection.
///
/// Both halves of it were already implemented and had no production caller. This
/// is where the shipping active daemon acquires them:
///
/// | half | what it decides |
/// |---|---|
/// | [`AdmissionGate`] | may this request produce an effect on this generation *right now* |
/// | [`RoutingLedger`] | may a rollover leave this generation draining — can every live client still address it |
///
/// Neither changes what a single active generation does: the gate opens both
/// lease classes for the `active` role, so every request that this build
/// dispatched before is still dispatched, and the ledger only records. What they
/// add is the *ability* to stop: a generation whose role moves to `draining`
/// refuses control and new spawns from the next request onwards while its owned
/// terminals keep being served, and the barrier a handoff waits on is the leases
/// the gate has already issued.
///
/// It is shared by every connection thread of one generation, so it is `Sync` and
/// both halves are internally locked.
pub(super) struct GenerationFence {
    pub(super) gate: AdmissionGate,
    pub(super) ledger: Arc<RoutingLedger>,
}

impl usagi_daemon::presentation::ipc::ConnectionFence for GenerationFence {
    fn admitted(
        &self,
        connection: usagi_core::domain::id::ConnectionId,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
    ) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
        self.ledger.admit(connection, hello);
        if self.gate.role() != GenerationRole::Active
            && !usagi_core::infrastructure::ipc::supports_owner_generation_routing(
                &hello.capabilities,
            )
        {
            self.ledger.disconnect(&connection);
            return Err(usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::GenerationRolledOver,
                "generation committed a rollover while this connection was waiting",
            ));
        }
        Ok(())
    }

    fn admit(
        &self,
        body: &serde_json::Value,
    ) -> Result<Option<AdmissionLease>, usagi_core::infrastructure::ipc::ProtocolError> {
        // The stance is `Own` because this process is the one that holds the data
        // directory's runtime state. Which *exact* record a ref names is the
        // terminal runtime's answer, not the fence's
        // (`usagi_daemon::usecase::authority::fence`).
        let (class, owner) = classify_request(body, OwnedRuntime::Own);
        self.gate.admit(class, owner).map_err(|refusal| {
            // The same code and the same meaning as a standby's refusal
            // ([`standby_reply`]): "this generation may not do this; re-resolve
            // the authority", with zero effect.
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::GenerationRolledOver,
                refusal.to_string(),
            )
        })
    }

    fn disconnected(&self, connection: usagi_core::domain::id::ConnectionId) {
        self.ledger.disconnect(&connection);
    }
}

/// The one answer a standby has for a post-handshake request.
///
/// The role admission fence decides it, which is what
/// `daemon.generation-handoff.v1` claims this peer does. Control, spawn and
/// terminal IO are refused by the fence itself; a read the fence admits is still
/// refused here, because this build's standby holds no runtime state to read —
/// the owner shard it would read is not wired yet.
///
/// The classification is
/// [`classify_request`](usagi_daemon::usecase::authority::fence::classify_request),
/// the same one the active generation's fence reads, under this role's honest
/// stance: a standby owns nothing, so every request that names a runtime names
/// another generation's ([`OwnedRuntime::Nothing`]).
///
/// A fence refusal is reported as `generation_rolled_over`, which is the same
/// code the draining generation's fence reports for the same decision
/// (`crates/daemon/tests/generation_authority.rs`). Both mean "this generation
/// may not do this; re-resolve the authority", and both are effect zero, so the
/// two roles stay one contract for a client rather than two.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
pub(super) fn standby_reply(
    gate: &AdmissionGate,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{
        Envelope, EnvelopeKind, ErrorCode, ProtocolError, ResponseOutcome,
    };
    let (class, owner) = classify_request(body, OwnedRuntime::Nothing);
    let error = match gate.admit(class, owner) {
        Ok(lease) => {
            drop(lease);
            ProtocolError::new(
                ErrorCode::Unavailable,
                "standby generation serves no runtime state",
            )
        }
        Err(refusal) => ProtocolError::new(ErrorCode::GenerationRolledOver, refusal.to_string()),
    };
    Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: EnvelopeKind::Response {
            request_id,
            outcome: ResponseOutcome::Error(error),
            body: serde_json::Value::Null,
        },
    }
}

/// A standby's participation in the durable generation registry.
///
/// It is the composition of the registry document, the data directory's owner
/// record, and a read-only handshake against this process's own private
/// endpoint. The pure decisions it drives —
/// [`admissible_active`] and [`prepare_standby`] — never touch the current
/// locator, which is what keeps every client pointed at the active generation
/// throughout.
pub(super) struct StandbyRegistryAuthority<'a> {
    pub(super) data_dir: &'a Path,
    pub(super) endpoint: &'a StandbyIpc<'a>,
    pub(super) build: BuildIdentity,
    pub(super) pid: u32,
    pub(super) registered: RefCell<Option<usagi_core::domain::id::DaemonGeneration>>,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl<'a> StandbyRegistryAuthority<'a> {
    /// Bind the standby registry seam against the endpoint that process bound.
    pub(super) fn new(data_dir: &'a Path, endpoint: &'a StandbyIpc<'a>, pid: u32) -> Self {
        Self {
            data_dir,
            build: current_build(),
            pid,
            endpoint,
            registered: RefCell::new(None),
        }
    }

    pub(super) fn registry(&self) -> std::io::Result<GenerationRegistry> {
        Ok(GenerationRegistry::new(
            GenerationRegistryFile::new(self.data_dir)?,
            DEFAULT_GENERATION_LIMIT,
        ))
    }

    /// The registry document, as a reader that must not become a writer sees it.
    pub(super) fn document(&self) -> std::io::Result<Option<RegistryDocument>> {
        read_registry_document(self.data_dir).map_err(std::io::Error::other)
    }

    /// The live registered active generation of this data directory, proved from
    /// the registry document and the owner record together. Reads only.
    pub(super) fn active_generation(
        &self,
    ) -> std::io::Result<usagi_core::domain::id::DaemonGeneration> {
        let record = DaemonRecordStore::new(FsRecordFile {
            path: self.data_dir.join("daemon").join("daemon.json"),
        })
        .load()?;
        let observation = record
            .as_ref()
            .map_or(DaemonProcessObservation::Unknown, |record| {
                LivenessProbe::observe(&ExactProcessControl, record)
            });
        let document = self.document()?;
        Ok(admissible_active(
            document.as_ref(),
            &ActiveOwner {
                record: record.as_ref(),
                observation,
            },
        )?)
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl StandbyAuthority for StandbyRegistryAuthority<'_> {
    fn preflight(&self) -> std::io::Result<()> {
        self.active_generation().map(|_| ())
    }

    fn admit(&self) -> std::io::Result<()> {
        let bound = self.endpoint.bound_endpoint().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "standby endpoint must be bound before registering",
            )
        })?;
        let generation = usagi_core::domain::id::DaemonGeneration::parse(&bound.generation.0)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bound endpoint does not name a canonical daemon generation",
                )
            })?;
        // Re-proved immediately before the compare-and-swap: the owner could have
        // died while this process was binding, and a standby beside a dead active
        // is a successor with nothing to succeed.
        let active = self.active_generation()?;
        let process = own_process_identity(self.pid)?;
        prepare_standby(
            &self.registry()?,
            &UnixStandbyProbe {
                data_dir: self.data_dir,
                build: self.build.clone(),
            },
            generation,
            &bound.endpoint,
            &process,
            &self.build,
        )
        .map_err(std::io::Error::other)?;
        *self.registered.borrow_mut() = Some(generation);
        ErrorLog::record(&format!(
            "daemon standby {generation} verified for active generation {active}"
        ));
        // Supervision starts once there is an entry to supervise, so a refused
        // admission never leaves a thread watching for one.
        start_standby_custody_worker(
            self.data_dir.to_path_buf(),
            generation,
            process,
            self.endpoint.hydrate()?.0,
            self.build.clone(),
            Arc::clone(&self.endpoint.worker),
            self.endpoint.shutdown.clone(),
        )
    }

    fn release(&self) -> std::io::Result<()> {
        let Some(generation) = *self.registered.borrow() else {
            return Ok(());
        };
        release_authority(&self.registry()?, generation).map_err(std::io::Error::other)
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_registers_beside_the_active_generation_without_publishing_a_locator
impl Drop for StandbyRegistryAuthority<'_> {
    fn drop(&mut self) {
        // Dropped before the endpoint it registered (declaration order in the
        // composition root is what fixes that), so an unwind gives up the entry
        // that names the socket before the socket goes.
        if self.registered.borrow().is_some() {
            let _ = StandbyAuthority::release(self);
        }
    }
}

/// The real readiness handshake: connect to this generation's own private
/// endpoint by name and complete one hello.
///
/// It is deliberately the same endpoint resolution a client uses for a
/// non-current generation, so readiness proves the socket a rollover would
/// actually name rather than a path this process remembers.
pub(super) struct UnixStandbyProbe<'a> {
    pub(super) data_dir: &'a Path,
    pub(super) build: BuildIdentity,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_standby_stands_down_with_its_incumbent_so_the_next_start_succeeds
impl StandbyProbe for UnixStandbyProbe<'_> {
    fn hello(
        &self,
        endpoint: &str,
    ) -> std::io::Result<usagi_core::infrastructure::ipc::ServerHello> {
        use usagi_core::infrastructure::ipc::{
            Bootstrap, ClientHello, ClientId, DEFAULT_MAX_FRAME_BYTES, ProtocolRange,
            TERMINAL_CHECKPOINT_REVISION, TERMINAL_WIRE_GENERATION, read_json_frame,
            write_json_frame,
        };
        let generation = usagi_core::domain::id::DaemonGeneration::parse(
            endpoint
                .strip_prefix("generations/")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default(),
        )
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "standby endpoint does not name a canonical daemon generation",
            )
        })?;
        let mut stream = connect_generation(
            self.data_dir,
            &usagi_core::infrastructure::owner_routing::TrustedEndpoint {
                generation,
                // Only the endpoint spelling is used by the connect; the role is
                // carried for the caller's own bookkeeping.
                role: usagi_core::infrastructure::ipc::GenerationRole::Standby,
                endpoint: endpoint.to_owned(),
            },
        )?;
        // One bootstrap frame out, one in, then the connection is dropped. There
        // is deliberately no request path here: a readiness probe that could
        // mutate its peer would not be a proof of readiness.
        write_json_frame(
            &mut stream,
            &Bootstrap::ClientHello(ClientHello {
                client_id: ClientId(format!("standby-readiness-{}", std::process::id())),
                connection_nonce: format!("{}", std::process::id()),
                expected_daemon_generation: None,
                supported_protocols: vec![ProtocolRange {
                    generation: TERMINAL_WIRE_GENERATION,
                    min_revision: 0,
                    max_revision: TERMINAL_CHECKPOINT_REVISION,
                }],
                capabilities: Vec::new(),
                required_capabilities: Vec::new(),
                build: self.build.clone(),
                workspace: Some(ClientWorkspace::Unbound),
            }),
            DEFAULT_MAX_FRAME_BYTES,
        )?;
        match read_json_frame::<Bootstrap>(&mut stream, DEFAULT_MAX_FRAME_BYTES)? {
            Some(Bootstrap::ServerHello(hello)) => Ok(hello),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("standby endpoint did not complete a handshake: {other:?}"),
            )),
        }
    }
}

/// Start the only standby custody supervisor.
///
/// A standby holds neither the instance lock nor a lifecycle record, so the
/// active daemon's two custody invariants do not exist for it. Its registry entry
/// is the whole of its authority: recovery that fails an abandoned handoff closed
/// retires every generation, and the standby it retired must exit rather than
/// keep a socket a future rollover might trust.
#[allow(clippy::too_many_arguments)] // Promotion carries the exact process, endpoint, runtime root, and both shutdown domains.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_stands_down_with_its_incumbent_so_the_next_start_succeeds
pub(super) fn start_standby_custody_worker(
    data_dir: PathBuf,
    generation: usagi_core::domain::id::DaemonGeneration,
    process: ProcessIdentity,
    workspace_root: PathBuf,
    build: BuildIdentity,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>>,
    shutdown: StandbyShutdownDomains,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-daemon-standby-custody".to_string())
        .spawn(move || {
            let _panic = shutdown.process_panic_guard();
            let mut promoted = false;
            while !shutdown.process.is_requested() {
                // An unreadable registry is uncertainty, not a loss: it never
                // terminates a standby that may still hold its entry.
                if let Ok(Some(document)) = read_registry_document(&data_dir) {
                    if !promoted && document.role(generation) == Some(GenerationRole::Active) {
                        match promote_standby_generation(
                            &data_dir,
                            &workspace_root,
                            generation,
                            &process,
                            &build,
                            &worker,
                            &shutdown,
                        ) {
                            Ok(()) => promoted = true,
                            Err(error) => {
                                ErrorLog::record(&format!(
                                    "daemon standby promotion failed: {error}"
                                ));
                                shutdown.request_process();
                                return;
                            }
                        }
                    }
                    if let StandbyCustody::Lost(loss) = evaluate_custody(
                        &document,
                        generation,
                        &process,
                        &mut observe_generation_process,
                    ) {
                        ErrorLog::record(&format!(
                            "daemon standby custody lost ({}); shutting down",
                            loss.reason()
                        ));
                        shutdown.request_process();
                        return;
                    }
                }
                if shutdown.process.wait_for_tick(STANDBY_CUSTODY_TICK) {
                    break;
                }
            }
        })
        .map(|_| ())
}

/// Replace the readiness-only standby accept loop with the full active runtime
/// on the same bound socket and generation after the durable handoff commits.
#[allow(clippy::too_many_arguments)] // Each handoff fence is passed explicitly; bundling would hide identity or listener ownership.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_standby_stands_down_with_its_incumbent_so_the_next_start_succeeds
pub(super) fn promote_standby_generation(
    data_dir: &Path,
    workspace_root: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    process: &ProcessIdentity,
    build: &BuildIdentity,
    worker: &Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>,
    shutdown: &StandbyShutdownDomains,
) -> std::io::Result<()> {
    shutdown.request_replacement();
    let standby = worker
        .lock()
        .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))?
        .take()
        .ok_or_else(|| std::io::Error::other("standby accept loop is unavailable"))?;
    let listener = standby
        .join()
        .map_err(|_| std::io::Error::other("daemon standby accept loop panicked"))?;

    let record = DaemonRecord::identified(process.pid, process.start_identity.clone());
    DaemonRecordStore::new(FsRecordFile {
        path: data_dir.join("daemon/daemon.json"),
    })
    .save(&record)?;
    let wire = usagi_core::infrastructure::ipc::DaemonGeneration(generation.as_str());
    let active = spawn_ipc_server(
        listener,
        &wire,
        data_dir,
        workspace_root,
        build,
        record,
        None,
        RuntimeHydration::AgentResumeHistory,
        Arc::clone(&shutdown.process),
    )?;
    *worker
        .lock()
        .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))? = Some(active);
    Ok(())
}

/// Whether a recorded generation process is still exactly the process recorded.
///
/// An identity that does not match is `Unknown` rather than `Gone`: the PID is
/// live, so nothing about the recorded owner has been proved either way. Only an
/// absent process is `Gone`, and only `Gone` lets recovery retire an authority.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
pub(super) fn observe_generation_process(process: &ProcessIdentity) -> ProcessObservation {
    if process.start_identity.is_empty() {
        return ProcessObservation::Unknown;
    }
    match process_start_identity(process.pid) {
        // The PID names a live process: either the recorded owner, or a different
        // incarnation that reused the PID — which proves nothing about the owner.
        Ok(identity) => {
            if identity == process.start_identity {
                ProcessObservation::VerifiedAlive(process.clone())
            } else {
                ProcessObservation::Unknown
            }
        }
        // Only an absent process is proof the owner is gone. An unreadable
        // process table is uncertainty, and uncertainty never retires anything.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProcessObservation::Gone,
        Err(_) => ProcessObservation::Unknown,
    }
}

pub(super) struct LaunchedStandby {
    pub(super) child: std::process::Child,
    pub(super) record: DaemonRecord,
}

/// The state subtree a standby uses as its initial workspace, without creating
/// one.
///
/// A standby hydrates read-only — every write belongs to the active generation —
/// so it never adopts the command's current directory. If that directory is
/// already inside an adopted workspace, that workspace remains the preferred
/// initial tenant. Otherwise a deterministic existing subtree is used: daemon
/// replacement is machine-wide and must not depend on which unrelated directory
/// the operator happened to run `daemon restart` from.
pub(super) fn standby_workspace_state_dir(
    daemon_dir: &Path,
    workspace_root: &Path,
) -> std::io::Result<PathBuf> {
    let adopted = workspace_state::adopted(daemon_dir)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let initialized = adopted
        .iter()
        .filter_map(|state| match lifecycle_state_initialized(state.dir()) {
            Ok(true) => Some(Ok(state)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    initialized
        .iter()
        .filter(|state| workspace_root.starts_with(state.root()))
        .max_by_key(|state| state.root().components().count())
        .or_else(|| initialized.first())
        .map(|state| state.dir().to_path_buf())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "durable runtime state is not initialized; a standby hydrates it read-only",
            )
        })
}

/// Every generation a scope inventory must be asked, active first.
///
/// A scope query has more than one answer while a generation is draining, and
/// taking only the active one's would read the draining generation's terminals
/// as absent. Absence is what collects a tab, so the fan-out is what keeps a
/// terminal whose owner is merely busy from being reaped.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
pub(crate) fn trusted_generations()
-> Result<Vec<usagi_core::infrastructure::owner_routing::TrustedEndpoint>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let mut cache = route_cache(&data_dir)
        .lock()
        .map_err(|_| ClientError::Unavailable("generation routing cache is poisoned".into()))?;
    cache
        .every_generation()
        .map_err(|error| error.to_client_error())
}
