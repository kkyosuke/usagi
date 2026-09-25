//! single-instance lock と workspace fence、custody 監視。

use std::panic::{self, AssertUnwindSafe};

use usagi_core::infrastructure::paths;

use crate::runtime::bootstrap;

#[cfg(test)]
use super::{FAIL_PRIVATE_LOCK_AFTER_CREATE, PRIVATE_LOCK_AFTER_FLOCK_BARRIER};

use super::{
    AdmissionGate, AdmissionLease, AppInfo, Arc, BackgroundWorker, BuildArtifactDecision,
    BuildRolloverTrigger, CUSTODY_TICK, CliDaemonCommand, ClientError, ClientPolicy,
    ClientWorkspace, ConnectionCleanup, ConnectionId, Custody, CustodyProbe,
    DaemonProcessObservation, DaemonRecord, Duration, ErrorLog, FileExt, FsRecordFile,
    GenerationRole, InstanceLock, Instant, LivenessProbe, Mutex, NodeIdentity, Path, PathBuf,
    ProcessIdentitySource, RefCell, ShutdownRequest, SystemClock, WORKSPACE_ADOPTION_PATIENCE,
    WorkspaceFence, WorkspaceFenceFactory, WorkspaceFenceOutcome, Write, build_artifact_decision,
    build_rollover_trigger, connect_client, current_build, deadline_transport, ensure_private_dir,
    ensure_private_dir_all, install_panic_logger, node_identity, private_lock_error,
    process_start_identity, read_owner_hint, run_inner, runtime_channel, write_owner_hint,
};

/// Couples the presentation-owned connection ID with its authenticated peer
/// PID at admission, then removes both through the same guaranteed disconnect
/// callback. This registers even an idle connection, so MCP PID retention is a
/// census of admitted sockets rather than only sockets that issued a request.
pub(super) struct CensusConnectionFence<'a> {
    pub(super) inner: &'a dyn usagi_daemon::presentation::ipc::ConnectionFence,
    pub(super) cleanup: ConnectionCleanup,
    pub(super) peer_pid: u32,
}

impl usagi_daemon::presentation::ipc::ConnectionFence for CensusConnectionFence<'_> {
    fn admitted(
        &self,
        connection: ConnectionId,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
    ) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
        self.inner.admitted(connection, hello)?;
        self.cleanup.connected(connection, self.peer_pid);
        Ok(())
    }

    fn admit(
        &self,
        body: &serde_json::Value,
    ) -> Result<Option<AdmissionLease>, usagi_core::infrastructure::ipc::ProtocolError> {
        self.inner.admit(body)
    }

    fn disconnected(&self, connection: ConnectionId) {
        self.inner.disconnected(connection);
        self.cleanup.disconnected(connection);
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_collection_pass_removes_the_shard_of_a_generation_nothing_retains
pub(super) fn start_custody_worker(
    probe: FsCustodyProbe,
    owner: DaemonRecord,
    data_dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_custody_worker(probe, owner, data_dir, gate, shutdown, CUSTODY_TICK)
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=a_collection_pass_removes_the_shard_of_a_generation_nothing_retains
pub(super) fn spawn_custody_worker<P>(
    probe: P,
    owner: DaemonRecord,
    data_dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    P: CustodyProbe + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-daemon-custody".to_string())
        .spawn(move || {
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::Custody);
            while !shutdown.is_requested() {
                // After a handoff this process deliberately no longer owns the
                // lifecycle record. Its authority is the draining registry
                // entry and the exact PTYs it still owns, so losing active
                // custody must not tear those PTYs down.
                if gate.role() != GenerationRole::Active {
                    if shutdown.wait_for_tick(tick) {
                        break;
                    }
                    continue;
                }
                match usagi_daemon::usecase::custody::evaluate(&probe, &owner) {
                    Ok(Custody::Lost(loss)) => {
                        // The error log lives inside the data directory. Record
                        // the reason only while that directory still exists: a
                        // daemon exiting because its tree was deleted must not
                        // re-create the tree it is releasing.
                        if data_dir.exists() {
                            ErrorLog::record(&format!(
                                "daemon custody lost ({}); shutting down",
                                loss.reason()
                            ));
                        }
                        // Request the same graceful shutdown a SIGTERM does, so
                        // endpoint retirement and record clearing stay on one path.
                        shutdown.request();
                        break;
                    }
                    // An undecidable observation is not a loss: keep serving and
                    // re-evaluate on the next tick.
                    Ok(Custody::Held) | Err(_) => {}
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

/// Real filesystem observations behind [`usagi_daemon::usecase::custody`].
///
/// `locked` is observed through the descriptor the single-instance lock holds,
/// so replacing the pathname afterwards cannot forge the identity it is
/// compared against.
pub(super) struct FsCustodyProbe {
    pub(super) locked: Option<NodeIdentity>,
    pub(super) lock_path: PathBuf,
    pub(super) record: FsRecordFile,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_collection_pass_removes_the_shard_of_a_generation_nothing_retains
impl CustodyProbe for FsCustodyProbe {
    fn locked_inode(&self) -> std::io::Result<NodeIdentity> {
        self.locked.ok_or_else(|| {
            std::io::Error::other("daemon instance lock identity was never observed")
        })
    }

    fn lock_pathname(&self) -> std::io::Result<Option<NodeIdentity>> {
        match std::fs::symlink_metadata(&self.lock_path) {
            Ok(metadata) => Ok(Some(node_identity(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn owner_record(&self) -> std::io::Result<Option<DaemonRecord>> {
        // Read without taking `record.lock`: records commit by rename, so a
        // reader never observes a torn file, and locking would re-create a
        // directory this daemon may already have lost.
        self.record
            .read_unlocked()?
            .map(|contents| {
                serde_json::from_str(&contents)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })
            .transpose()
    }
}

#[cfg(test)]
pub(super) struct PrivateLockAfterFlockBarrier {
    pub(super) path: PathBuf,
    pub(super) acquired: Arc<std::sync::Barrier>,
    pub(super) replaced: Arc<std::sync::Barrier>,
}

#[cfg(test)]
pub(super) fn take_private_lock_create_failpoint(path: &Path) -> bool {
    FAIL_PRIVATE_LOCK_AFTER_CREATE.with(|failpoint| {
        if failpoint.borrow().as_deref() == Some(path) {
            failpoint.borrow_mut().take();
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
pub(super) fn install_private_lock_after_flock_barrier(
    path: &Path,
    acquired: Arc<std::sync::Barrier>,
    replaced: Arc<std::sync::Barrier>,
) {
    PRIVATE_LOCK_AFTER_FLOCK_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PrivateLockAfterFlockBarrier {
            path: path.to_path_buf(),
            acquired,
            replaced,
        });
    });
}

#[cfg(test)]
pub(super) fn wait_private_lock_after_flock_barrier(path: &Path) {
    let barrier = PRIVATE_LOCK_AFTER_FLOCK_BARRIER.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        matches.then(|| slot.borrow_mut().take().expect("barrier was present"))
    });
    if let Some(barrier) = barrier {
        barrier.acquired.wait();
        barrier.replaced.wait();
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PrivateLockModePolicy {
    CrashResidue,
    OwnerLegacy0644,
}

pub(super) fn verify_private_lock_metadata(
    metadata: &std::fs::Metadata,
    label: &str,
    mode_policy: Option<PrivateLockModePolicy>,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode() & 0o7777;
    let mode_is_safe = match mode_policy {
        None => mode == 0o600,
        Some(PrivateLockModePolicy::CrashResidue) => mode & !0o600 == 0,
        Some(PrivateLockModePolicy::OwnerLegacy0644) => mode & !0o600 == 0 || mode == 0o644,
    };
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || !mode_is_safe
    {
        return Err(private_lock_error(
            label,
            "is not an exact private single-link regular owner file",
        ));
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=private_lock_refuses_unsafe_metadata_and_paths
pub(super) fn open_private_lock(
    path: &Path,
    label: &str,
    mode_policy: PrivateLockModePolicy,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} path has no parent"),
        )
    })?;
    ensure_private_dir(parent)?;
    let open = |create_new| {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        if create_new {
            options.create_new(true);
        }
        options.open(path)
    };

    let (file, created) = match open(true) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => match open(false) {
            Ok(file) => (file, false),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // A creator killed after create_new but before fd-fchmod can
                // leave an owner-only directory containing a mode-000 lock.
                // Validate that residue before path chmod, then require the
                // O_NOFOLLOW reopen to resolve to the exact inode inspected.
                let before = std::fs::symlink_metadata(path)?;
                verify_private_lock_metadata(&before, label, Some(mode_policy))?;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                let file = open(false)?;
                let after = file.metadata()?;
                if before.dev() != after.dev() || before.ino() != after.ino() {
                    return Err(private_lock_error(
                        label,
                        "changed while repairing its mode",
                    ));
                }
                (file, false)
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };

    #[cfg(not(test))]
    let _ = created;
    #[cfg(test)]
    if created && take_private_lock_create_failpoint(path) {
        return Err(std::io::Error::other(format!(
            "injected {label} failure after create_new"
        )));
    }

    // Reject links/non-owner nodes before chmod so widening a hostile inode is
    // impossible. fd-fchmod then repairs both umask-reduced creation and safe
    // legacy modes without reopening the pathname.
    verify_private_lock_metadata(&file.metadata()?, label, Some(mode_policy))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    verify_private_lock_metadata(&file.metadata()?, label, None)?;
    Ok(file)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=private_lock_refuses_unsafe_metadata_and_paths
pub(super) fn verify_private_lock_path(
    path: &Path,
    file: &std::fs::File,
    label: &str,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let descriptor = file.metadata()?;
    verify_private_lock_metadata(&descriptor, label, None)?;
    let pathname = std::fs::symlink_metadata(path)?;
    verify_private_lock_metadata(&pathname, label, None)?;
    if descriptor.dev() != pathname.dev() || descriptor.ino() != pathname.ino() {
        return Err(private_lock_error(
            label,
            "pathname does not name the locked inode",
        ));
    }
    let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if descriptor_flags & libc::FD_CLOEXEC == 0 {
        return Err(private_lock_error(label, "descriptor is not close-on-exec"));
    }
    Ok(())
}

/// How long a caller waits for a contended private lock before giving up.
///
/// Every cross-process section this module takes is bounded, because these
/// locks are held on a machine-wide data directory: a blocking `flock` here lets
/// any other usagi process — an MCP server, a CLI invocation, a rollover — stall
/// an interactive surface for as long as it likes, and a holder killed while
/// wedged would stall it forever. Each bound is sized against what its own
/// section can legitimately take.
#[derive(Clone, Copy)]
pub(super) struct PrivateLockWait {
    pub(super) limit: Duration,
    pub(super) poll: Duration,
}

impl PrivateLockWait {
    pub(super) const POLL: Duration = Duration::from_millis(20);

    /// A section that only reads or rewrites one small record file. The same
    /// two seconds the instance lock and the workspace fence already wait.
    pub(super) const RECORD: Self = Self {
        limit: Duration::from_secs(2),
        poll: Self::POLL,
    };

    /// The bootstrap section, held across one `connect_or_start`. Its worst case
    /// is a cold start: spawning the lifecycle child, then
    /// [`bootstrap::READINESS_CEILING`] of endpoint polling. The budget is that
    /// ceiling plus a spawn margin, so a concurrent honest cold start is waited
    /// out while a wedged holder still returns a typed answer.
    pub(super) const BOOTSTRAP: Self = Self {
        limit: bootstrap::READINESS_CEILING.saturating_add(Duration::from_secs(3)),
        poll: Self::POLL,
    };

    /// Explicit lifecycle custody can span standby verification and, after the
    /// authority commit, successor serving hydration. Both stages have their
    /// own 30-second window; the margin covers process launch and final output.
    pub(super) const LIFECYCLE: Self = Self {
        limit: Duration::from_secs(65),
        poll: Self::POLL,
    };
}

pub(super) fn lock_private_exclusive(
    path: &Path,
    label: &str,
    mode_policy: PrivateLockModePolicy,
    wait: PrivateLockWait,
) -> std::io::Result<std::fs::File> {
    let file = open_private_lock(path, label, mode_policy)?;
    let deadline = Instant::now() + wait.limit;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(wait.poll),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("{label} is held by another process"),
                ));
            }
        }
    }
    #[cfg(test)]
    wait_private_lock_after_flock_barrier(path);
    verify_private_lock_path(path, &file, label)?;
    Ok(file)
}

pub(super) struct ExactProcessControl;

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
impl ProcessIdentitySource for ExactProcessControl {
    fn process_start_identity(&self, pid: u32) -> std::io::Result<String> {
        process_start_identity(pid)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
impl LivenessProbe for ExactProcessControl {
    fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation {
        let Some(expected) = record
            .process_start_identity
            .as_deref()
            .filter(|identity| !identity.is_empty())
        else {
            return DaemonProcessObservation::Unknown;
        };
        match process_start_identity(record.pid) {
            Ok(actual) if actual == expected => DaemonProcessObservation::Exact,
            Ok(_) => DaemonProcessObservation::IdentityMismatch,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DaemonProcessObservation::Gone
            }
            Err(_) => DaemonProcessObservation::Unknown,
        }
    }
}

pub(super) struct FileInstanceLock {
    pub(super) path: PathBuf,
    pub(super) held: RefCell<Option<std::fs::File>>,
}

/// The descriptor identity behind the singleton guard. Normally this is the
/// data-home lock itself. When the selected workspace is the data home's parent
/// (the default home-directory case), the workspace fence and singleton lock
/// are the same inode and one held descriptor supplies both invariants.
pub(super) trait InstanceLockCustody {
    fn locked_inode(&self) -> Option<NodeIdentity>;
}

impl FileInstanceLock {
    /// Identity of the inode this process locked, read from the held descriptor
    /// rather than the pathname, so a later replacement of the pathname cannot
    /// forge the identity custody supervision compares against.
    ///
    /// `None` means this lock was never acquired (or its descriptor cannot be
    /// inspected), which leaves custody undecidable instead of lost.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_live_registered_authority_is_repaired_rather_than_displaced
    pub(super) fn locked_inode(&self) -> Option<NodeIdentity> {
        let held = self.held.borrow();
        let metadata = held.as_ref()?.metadata().ok()?;
        Some(node_identity(&metadata))
    }
}

impl InstanceLockCustody for FileInstanceLock {
    fn locked_inode(&self) -> Option<NodeIdentity> {
        Self::locked_inode(self)
    }
}

/// The workspace-scoped fence: an exclusive `flock` on
/// `<workspace>/.usagi/daemon/daemon.lock`.
///
/// The node is outside every runtime-mode child directory, so `production`,
/// `development`, and `local` — and any `$USAGI_HOME` — converge on one inode.
/// Path spelling cannot split it either, because `flock` excludes per inode and
/// the workspace root is canonicalized before it is spelled.
///
/// After acquiring, the owner writes its pid line into the node. That line is the
/// only cross-mode discovery channel there is: a refused daemon reads a different
/// data directory's `daemon.json` than the owner writes, so without the hint it
/// could not name the process holding the workspace.
pub(super) struct FileWorkspaceFence {
    pub(super) path: PathBuf,
    pub(super) workspace: PathBuf,
    pub(super) pid: u32,
    /// How long to wait for a departing owner before refusing. A start can wait
    /// for the previous daemon to exit; an adoption happens inside a client's
    /// handshake, which has its own deadline, so it refuses quickly instead.
    pub(super) patience: Duration,
    /// The locked descriptor, or `None` before acquisition and after release.
    ///
    /// A `Mutex` rather than a `RefCell` because the startup fence outlives the
    /// thread that took it: the tenant sweep is what gives that workspace back
    /// once this generation has handed off ([`FileWorkspaceFence::release`]),
    /// and it runs on its own thread.
    pub(super) held: Mutex<Option<std::fs::File>>,
}

impl FileWorkspaceFence {
    /// Give the workspace back by dropping the locked descriptor.
    ///
    /// `flock` is released with the last descriptor naming the node, so this is
    /// the whole of it. The owner pid line stays behind as the fence node's
    /// contents; the next owner truncates and rewrites it when it acquires.
    ///
    /// Returns whether this call was the one that released it, so a repeated
    /// sweep neither re-reports nor fails.
    pub(super) fn release(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some()
    }
}

/// Startup's singleton guard. Distinct nodes use the ordinary data-home lock;
/// a path alias to the workspace fence reuses its already-acquired descriptor.
/// Opening the aliased path a second time would make `flock` contend with this
/// process's own descriptor on Unix and reject every start from `$HOME`.
pub(super) enum ProcessInstanceLock<'a> {
    Independent(FileInstanceLock),
    WorkspaceAlias {
        path: PathBuf,
        workspace: &'a FileWorkspaceFence,
    },
}

pub(super) fn process_instance_lock(
    path: PathBuf,
    workspace: &FileWorkspaceFence,
) -> ProcessInstanceLock<'_> {
    if lock_paths_alias(&path, &workspace.path) {
        ProcessInstanceLock::WorkspaceAlias { path, workspace }
    } else {
        ProcessInstanceLock::Independent(FileInstanceLock {
            path,
            held: RefCell::new(None),
        })
    }
}

pub(super) fn lock_paths_alias(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if left.file_name() != right.file_name() {
        return false;
    }
    match (
        left.parent().and_then(|parent| parent.canonicalize().ok()),
        right.parent().and_then(|parent| parent.canonicalize().ok()),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, _) | (_, None) => false,
    }
}

impl ProcessInstanceLock<'_> {
    /// Whether the singleton guard is this process's workspace fence under
    /// another spelling.
    ///
    /// One held descriptor then supplies both invariants, so releasing the
    /// fence would release the single-instance guard with it and let a second
    /// daemon start on this data directory. Such a fence is never given back
    /// while the process lives, whatever this generation's authority is.
    pub(super) const fn aliases_workspace_fence(&self) -> bool {
        matches!(self, Self::WorkspaceAlias { .. })
    }
}

impl InstanceLockCustody for ProcessInstanceLock<'_> {
    fn locked_inode(&self) -> Option<NodeIdentity> {
        match self {
            Self::Independent(lock) => lock.locked_inode(),
            Self::WorkspaceAlias { workspace, .. } => {
                let held = workspace
                    .held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let metadata = held.as_ref()?.metadata().ok()?;
                Some(node_identity(&metadata))
            }
        }
    }
}

impl InstanceLock for ProcessInstanceLock<'_> {
    fn acquire(&self) -> std::io::Result<bool> {
        match self {
            Self::Independent(lock) => lock.acquire(),
            Self::WorkspaceAlias { path, workspace } => {
                let held = workspace
                    .held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let file = held.as_ref().ok_or_else(|| {
                    std::io::Error::other(
                        "daemon workspace fence must be acquired before its aliased instance lock",
                    )
                })?;
                verify_private_lock_path(&workspace.path, file, "aliased daemon instance lock")?;
                verify_private_lock_path(path, file, "aliased daemon instance lock")?;
                Ok(true)
            }
        }
    }
}

/// Builds a [`FileWorkspaceFence`] for any workspace this daemon adopts.
///
/// The daemon serves one workspace per tenant and holds one fence per tenant, so
/// the fence stops being a single start-up value and becomes something the
/// registry asks for by root.
pub(super) struct FileWorkspaceFences {
    pub(super) pid: u32,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=workspace_fence_refuses_through_a_symlinked_or_relative_spelling
impl WorkspaceFenceFactory for FileWorkspaceFences {
    fn fence_for(&self, workspace_root: &Path) -> Box<dyn WorkspaceFence + Send> {
        Box::new(FileWorkspaceFence {
            path: paths::workspace_fence_path(workspace_root),
            workspace: workspace_root.to_path_buf(),
            pid: self.pid,
            patience: WORKSPACE_ADOPTION_PATIENCE,
            held: Mutex::new(None),
        })
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=workspace_fence_refuses_through_a_symlinked_or_relative_spelling
impl WorkspaceFence for FileWorkspaceFence {
    fn acquire(&self) -> std::io::Result<WorkspaceFenceOutcome> {
        const POLL: Duration = Duration::from_millis(20);
        // `<workspace>/.usagi` is user-visible project metadata, so it keeps
        // ordinary directory permissions; only the `daemon/` child holding the
        // fence is private, which `open_private_lock` establishes.
        std::fs::create_dir_all(self.workspace.join(paths::STATE_DIR))?;
        let file = open_private_lock(
            &self.path,
            "daemon workspace fence",
            PrivateLockModePolicy::OwnerLegacy0644,
        )?;
        let deadline = Instant::now() + self.patience;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    #[cfg(test)]
                    wait_private_lock_after_flock_barrier(&self.path);
                    verify_private_lock_path(&self.path, &file, "daemon workspace fence")?;
                    // Publish the owner hint immediately, so the window in which a
                    // refused start could read the previous owner's line is only
                    // as long as this write.
                    write_owner_hint(&file, self.pid)?;
                    *self
                        .held
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
                    return Ok(WorkspaceFenceOutcome::Acquired);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                Err(_) => {
                    return Ok(WorkspaceFenceOutcome::Held {
                        workspace: self.workspace.display().to_string(),
                        // The hint is diagnostic only. An empty or garbled line
                        // (a holder killed before publishing) yields no pid, and a
                        // holder mid-write can still show the departed owner's.
                        // Neither changes the refusal, which the `flock` decides.
                        owner: read_owner_hint(&file),
                    });
                }
            }
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_second_active_daemon_is_refused_without_disturbing_the_registry
impl InstanceLock for FileInstanceLock {
    fn acquire(&self) -> std::io::Result<bool> {
        const TIMEOUT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(20);
        if let Some(parent) = self.path.parent() {
            ensure_private_dir(parent)?;
        }
        let file = open_private_lock(
            &self.path,
            "daemon instance lock",
            PrivateLockModePolicy::OwnerLegacy0644,
        )?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    #[cfg(test)]
                    wait_private_lock_after_flock_barrier(&self.path);
                    verify_private_lock_path(&self.path, &file, "daemon instance lock")?;
                    *self.held.borrow_mut() = Some(file);
                    return Ok(true);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                Err(_) => return Ok(false),
            }
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=daemon_stop_waits_for_lifecycle_custody
pub(super) fn run_with_lifecycle_custody(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
    lifecycle_custody: Option<&std::fs::File>,
) -> std::io::Result<()> {
    install_panic_logger();
    match panic::catch_unwind(AssertUnwindSafe(|| {
        run_inner(out, command, info, operation, lifecycle_custody.is_some())
    })) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            ErrorLog::record(&format!("daemon failed: {error}"));
            Err(error)
        }
        // `install_panic_logger` has already recorded the payload, location,
        // and backtrace. Convert the unwind to an ordinary process error so
        // callers do not continue after a failed daemon startup or serve loop.
        Err(_) => Err(std::io::Error::other(
            "daemon panicked; see the error log for details",
        )),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
pub(super) fn request_replacement_while_locked(
    policy: ClientPolicy,
) -> Result<BuildRolloverTrigger, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let expected_build = current_build();
    // Replacing the running artifact is a lifecycle observation, not workspace
    // work: it reads the daemon's advertised build and sends no request, so it
    // stays usable from outside the daemon's workspace.
    let clock = SystemClock::new();
    let client = connect_client(
        &data_dir,
        policy,
        expected_build.clone(),
        ClientWorkspace::Unbound,
        |stream| deadline_transport(clock, stream, policy.timeout_ms),
    )
    .map_err(|_| ClientError::Unavailable("daemon endpoint is unavailable".into()))?;
    let actual_build = client.server_build();
    match build_artifact_decision(actual_build, &expected_build, true) {
        BuildArtifactDecision::ForceReplace | BuildArtifactDecision::RolloverTrigger => {
            build_rollover_trigger(actual_build, &expected_build, runtime_channel(), true)
                .ok_or(ClientError::BuildIdentityUnavailable)
        }
        BuildArtifactDecision::Unknown => Err(ClientError::BuildIdentityUnavailable),
        BuildArtifactDecision::Reuse => Err(ClientError::Lifecycle(
            "daemon replacement trigger could not be created".into(),
        )),
    }
}

/// Serializes explicit stop/restart with managed update's final observation,
/// replacement, and serving verification. It is separate from the client
/// bootstrap lock because a bootstrap owner launches a lifecycle subprocess
/// while continuing to hold its own lock.
pub(super) fn acquire_lifecycle_lock_io_within(
    data_dir: &Path,
    wait: PrivateLockWait,
) -> std::io::Result<std::fs::File> {
    (|| {
        ensure_private_dir_all(data_dir)?;
        let path = data_dir.join("daemon").join("lifecycle.lock");
        lock_private_exclusive(
            &path,
            "daemon lifecycle lock",
            PrivateLockModePolicy::CrashResidue,
            wait,
        )
    })()
}
