//! bootstrap broker の起動・endpoint 公開・idle 監視と、broker 越しの client 要求。

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};
use usagi_core::infrastructure::daemon::Sleeper as _;
use usagi_core::infrastructure::paths;

use crate::runtime::bootstrap;

use super::{
    ATTEMPTED_REPLACEMENTS, Arc, BROKER_IDLE_POLL, BROKER_IDLE_TIMEOUT, BROKER_OK, BROKER_PING,
    BROKER_READINESS_ATTEMPTS, BROKER_REQUEST_TIMEOUT, BROKER_START, BROKER_STOP,
    BuildArtifactDecision, BuildIdentity, CliDaemonCommand, ClientError, ClientWorkspace, Condvar,
    Deserialize, Duration, ErrorLog, Instant, IpcClient, Mutex, Path, PathBuf,
    PrivateLockModePolicy, PrivateLockWait, Read, RealSleeper, Serialize, Write,
    bootstrap_serve_command, broker_may_retire, build_artifact_decision, build_rollover_trigger,
    cold_start_workspace, current_build, current_daemon_is_reachable, ensure_private_dir_all,
    lock_private_exclusive, opened_workspace, reap_child, recover_stale_client_endpoint,
    reused_build_mismatch_record, run_lifecycle, run_lifecycle_with, runtime_channel,
    serve_bootstrap_broker, should_attempt_automatic_replacement,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BootstrapBrokerAddress {
    pub(super) socket: PathBuf,
    pub(super) lock: PathBuf,
    pub(super) record: PathBuf,
}

/// Exact process identity published by a bootstrap broker for fixture and
/// operator cleanup. The identity fences PID reuse; the private, digest-scoped
/// path fences the workspace and executable this broker serves.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BootstrapBrokerRecord {
    pub(super) pid: u32,
    pub(super) process_start_identity: String,
}

pub(super) fn bootstrap_broker_address(
    data_dir: &Path,
    workspace: &Path,
    exe: &Path,
) -> BootstrapBrokerAddress {
    let mut digest = Sha256::new();
    for component in [
        b"usagi-bootstrap-broker-v1".as_slice(),
        workspace.as_os_str().as_encoded_bytes(),
        exe.as_os_str().as_encoded_bytes(),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component);
    }
    let digest = digest.finalize();
    let mut key = String::with_capacity(32);
    for byte in &digest[..16] {
        write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let daemon_dir = data_dir.join("daemon");
    BootstrapBrokerAddress {
        socket: daemon_dir.join(format!("bootstrap-broker-{key}.sock")),
        lock: daemon_dir.join(format!("bootstrap-broker-{key}.lock")),
        record: daemon_dir.join(format!("bootstrap-broker-{key}.json")),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn request_bootstrap_broker(
    address: &BootstrapBrokerAddress,
    request: u8,
) -> std::io::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(&address.socket)?;
    let timeout = if request == BROKER_START {
        Duration::from_secs(6)
    } else {
        BROKER_REQUEST_TIMEOUT
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&[request])?;
    let mut reply = [0_u8; 1];
    stream.read_exact(&mut reply)?;
    (reply[0] == BROKER_OK)
        .then_some(())
        .ok_or_else(|| std::io::Error::other("daemon bootstrap broker refused the request"))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn request_broker_start(
    data_dir: &Path,
    workspace: &Path,
    exe: &Path,
) -> std::io::Result<()> {
    let exe = exe.canonicalize()?;
    let address = bootstrap_broker_address(data_dir, workspace, &exe);
    request_bootstrap_broker(&address, BROKER_START)?;
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if current_daemon_is_reachable(data_dir) {
            return Ok(());
        }
        RealSleeper.sleep();
    }
    Err(std::io::Error::other(
        "daemon bootstrap broker started no reachable daemon",
    ))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn spawn_bootstrap_broker(
    exe: &Path,
    data_dir: &Path,
    workspace: &Path,
) -> std::io::Result<()> {
    let workspace = paths::canonical_workspace_root(workspace)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let exe = exe.canonicalize()?;
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    // A daemon launched by this broker reaches here while the broker is still
    // waiting for that daemon's readiness. Requiring a ping reply would make
    // both processes wait on each other. The identity-scoped socket path and a
    // successful connect are sufficient to prove that this broker is present.
    if std::os::unix::net::UnixStream::connect(&address.socket).is_ok() {
        return Ok(());
    }
    let mut command = std::process::Command::new(&exe);
    command
        .args(["daemon", "bootstrap-broker"])
        .current_dir(&workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn()?;
    reap_child(child);
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if request_bootstrap_broker(&address, BROKER_PING).is_ok() {
            return Ok(());
        }
        RealSleeper.sleep();
    }
    Err(std::io::Error::other(
        "daemon bootstrap broker did not become ready",
    ))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn launch_broker_daemon(
    exe: &Path,
    workspace: &Path,
    data_dir: &Path,
) -> std::io::Result<()> {
    if current_daemon_is_reachable(data_dir) {
        return Ok(());
    }
    let child = bootstrap_serve_command(exe, workspace).spawn()?;
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if current_daemon_is_reachable(data_dir) {
            reap_child(child);
            return Ok(());
        }
        RealSleeper.sleep();
    }
    reap_child(child);
    Err(std::io::Error::other(
        "brokered daemon did not become ready",
    ))
}

/// What answering one broker request decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BrokerOutcome {
    /// Whether the peer is told the request succeeded.
    pub(super) accepted: bool,
    /// Whether the broker closes its endpoint after replying.
    pub(super) retire: bool,
}

impl BrokerOutcome {
    pub(super) const fn served(accepted: bool) -> Self {
        Self {
            accepted,
            retire: false,
        }
    }

    pub(super) const RETIRE: Self = Self {
        accepted: true,
        retire: true,
    };
}

pub(super) fn handle_bootstrap_broker_request(
    request: u8,
    launch: impl FnOnce() -> std::io::Result<()>,
    daemon_live: impl FnOnce() -> bool,
) -> BrokerOutcome {
    match request {
        BROKER_PING => BrokerOutcome::served(true),
        BROKER_START => BrokerOutcome::served(launch().is_ok()),
        // Retiring is acknowledged before it happens: the peer asked for the
        // endpoint to go away, so its disappearance is the success case.
        //
        // A reachable daemon vetoes it. Both senders decide to retire from
        // outside this loop — `usagi daemon stop` after it stopped the daemon,
        // the idle watch after it found none — and in between either decision
        // and this point a `BROKER_START` can have put one back. Retiring then
        // would leave a live daemon with no broker to outlive it, which is the
        // one state the broker exists to prevent. Re-reading the endpoint here
        // is the only place both senders pass through.
        BROKER_STOP if daemon_live() => BrokerOutcome::served(false),
        BROKER_STOP => BrokerOutcome::RETIRE,
        _ => BrokerOutcome::served(false),
    }
}

/// Whether a broker's published endpoint is still a socket on disk.
///
/// A broker whose endpoint was removed underneath it is *unreachable* rather
/// than idle: no client can connect to it, and neither can the retirement
/// request its own idle watch sends. Presence is enough to tell the two apart,
/// because the per-address instance lock already keeps a second broker from
/// publishing over this one.
pub(super) fn broker_endpoint_present(socket: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;

    std::fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket())
}

/// How long a broker tolerates being unused before retiring itself.
#[derive(Debug, Clone, Copy)]
pub(super) struct BrokerIdlePolicy {
    pub(super) timeout: Duration,
    pub(super) poll: Duration,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive
impl BrokerIdlePolicy {
    pub(super) const fn production() -> Self {
        Self {
            timeout: BROKER_IDLE_TIMEOUT,
            poll: BROKER_IDLE_POLL,
        }
    }
}

/// The last time a broker answered a request, shared with its idle watch.
pub(super) struct BrokerActivity {
    pub(super) state: Mutex<BrokerActivityState>,
    pub(super) signal: Condvar,
}

pub(super) struct BrokerActivityState {
    pub(super) last: Instant,
    pub(super) stopped: bool,
}

impl BrokerActivity {
    pub(super) fn started() -> Self {
        Self {
            state: Mutex::new(BrokerActivityState {
                last: Instant::now(),
                stopped: false,
            }),
            signal: Condvar::new(),
        }
    }

    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive
    pub(super) fn touch(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last = Instant::now();
        }
    }

    /// Stop the idle watch and let it be joined without waiting out a poll.
    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive
    pub(super) fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.signal.notify_all();
    }

    /// Wait for the next idle poll unless shutdown was already requested.
    ///
    /// The predicate is checked while holding the same mutex that [`Self::stop`]
    /// updates. This closes the stop-before-wait window: a notification may be
    /// coalesced or arrive before this method locks, but the state transition
    /// itself cannot be missed.
    pub(super) fn wait_for_poll(&self, poll: Duration) -> Option<Duration> {
        let state = self.state.lock().ok()?;
        let (state, _) = self
            .signal
            .wait_timeout_while(state, poll, |state| !state.stopped)
            .ok()?;
        (!state.stopped).then(|| state.last.elapsed())
    }
}

/// Watch an idle broker and ask it to retire once nothing needs it.
///
/// The retirement is delivered as an ordinary [`BROKER_STOP`] request rather
/// than by killing the loop from outside: the accept loop stays blocked (so a
/// cold start pays no polling latency), and the endpoint is torn down by the
/// same path an operator's `usagi daemon stop` takes.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive,an_unreachable_broker_endpoint_is_not_mistaken_for_an_idle_one
pub(super) fn spawn_broker_idle_watch(
    activity: &Arc<BrokerActivity>,
    address: BootstrapBrokerAddress,
    data_dir: &Path,
    idle: BrokerIdlePolicy,
) -> std::thread::JoinHandle<()> {
    let activity = Arc::clone(activity);
    let data_dir = data_dir.to_path_buf();
    std::thread::spawn(move || {
        loop {
            let Some(idle_for) = activity.wait_for_poll(idle.poll) else {
                return;
            };
            // An endpoint that is gone can never be reached again — not by a
            // client, and not by the retirement request below. Leaving the
            // broker running then means blocking in `accept` forever on a
            // socket nobody can connect to, which is how brokers outlived by
            // weeks the temporary homes that owned them. Exiting is the whole
            // cleanup: the directory that held the socket and the record file
            // is already gone.
            if !broker_endpoint_present(&address.socket) {
                std::process::exit(0);
            }
            let daemon_live = current_daemon_is_reachable(&data_dir);
            if broker_may_retire(idle_for, idle.timeout, daemon_live)
                && request_bootstrap_broker(&address, BROKER_STOP).is_ok()
            {
                return;
            }
            // Either the broker is still needed, or a daemon appeared between
            // the probe and the request and vetoed the retirement. Keep
            // watching. Returning on a *refused* retirement left a broker that
            // could never retire afterwards, however long the daemon it exists
            // to outlive had been gone.
        }
    })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn run_broker_lifecycle_command(
    command: &CliDaemonCommand,
) -> Option<std::io::Result<()>> {
    if command == &CliDaemonCommand::BootstrapBroker {
        return Some((|| {
            let data_dir =
                paths::data_dir().map_err(|error| std::io::Error::other(format!("{error:#}")))?;
            serve_bootstrap_broker(
                &data_dir,
                &std::env::current_dir()?,
                &std::env::current_exe()?,
                BrokerIdlePolicy::production(),
            )
        })());
    }
    None
}

/// Ask the broker for `workspace` and `exe` to close its endpoint.
///
/// Best effort by construction: there may be no broker (nothing started one, or
/// it already retired), and a daemon that is going away anyway must not fail a
/// stop because a helper could not be reached.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=planned_stop_retires_generation_endpoint_and_allows_safe_autostart
pub(super) fn retire_bootstrap_broker(data_dir: &Path, workspace: &Path, exe: &Path) {
    let Ok(exe) = exe.canonicalize() else {
        return;
    };
    let Ok(workspace) = paths::canonical_workspace_root(workspace) else {
        return;
    };
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    let _ = request_bootstrap_broker(&address, BROKER_STOP);
}

/// Establishes a bootstrapped, build-fenced daemon connection. `connect` builds
/// one authenticated session over any stream type — the exact-owner-verified
/// [`LaneClient`] the terminal lanes use, or the per-request one
/// [`policy_client`] builds — so every surface shares the identical cold-start,
/// stale-recovery, and development rollover handling. Both are deadline-armed:
/// no caller can ask this for an unbounded socket.
///
/// Entering the bootstrap section is itself bounded
/// ([`acquire_bootstrap_lock`]), so a peer that is holding it cannot stall this
/// caller indefinitely.
// LLVM counts the deadline-stream instantiation as uncovered for branches the
// UnixStream instantiation already exercises through the integration suite.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=cli_tui_pty
#[allow(clippy::too_many_lines)] // Lock, broker, lifecycle start, and rollover share one workspace snapshot.
pub(super) fn bootstrap_client<S: Read + Write>(
    workspace: &ClientWorkspace,
    connect: impl Fn(&Path, &BuildIdentity) -> std::io::Result<IpcClient<S>>,
) -> Result<IpcClient<S>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let opened = opened_workspace();
    let ambient_cwd = std::env::current_dir().ok();
    let launch_workspace = || {
        cold_start_workspace(
            &data_dir.join("daemon"),
            workspace,
            opened.as_deref(),
            ambient_cwd.as_deref(),
        )
        .map_err(ClientError::Protocol)
    };
    let exe =
        std::env::current_exe().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let expected_build = current_build();
    let _bootstrap_lock =
        match acquire_bootstrap_lock_io_within(&data_dir, PrivateLockWait::BOOTSTRAP) {
            Ok(lock) => lock,
            Err(lock_error) if lock_error.kind() == std::io::ErrorKind::PermissionDenied => {
                let broker_workspace = launch_workspace()?;
                if request_broker_start(&data_dir, &broker_workspace, &exe).is_err() {
                    return Err(map_bootstrap_lock_error(&lock_error));
                }
                for _ in 0..40 {
                    if let Ok(client) = connect(&data_dir, &expected_build) {
                        return match build_artifact_decision(
                            client.server_build(),
                            &expected_build,
                            false,
                        ) {
                            BuildArtifactDecision::Reuse => Ok(client),
                            BuildArtifactDecision::ForceReplace
                            | BuildArtifactDecision::RolloverTrigger
                                if !should_attempt_automatic_replacement(paths::runtime_mode()) =>
                            {
                                Ok(client)
                            }
                            BuildArtifactDecision::ForceReplace
                            | BuildArtifactDecision::RolloverTrigger => {
                                Err(ClientError::RolloverRequired(
                                    build_rollover_trigger(
                                        client.server_build(),
                                        &expected_build,
                                        runtime_channel(),
                                        false,
                                    )
                                    .ok_or(ClientError::BuildIdentityUnavailable)?,
                                ))
                            }
                            BuildArtifactDecision::Unknown => {
                                Err(ClientError::BuildIdentityUnavailable)
                            }
                        };
                    }
                    RealSleeper.sleep();
                }
                return Err(ClientError::Unavailable(
                    "daemon bootstrap broker started no reachable daemon".into(),
                ));
            }
            Err(lock_error) => return Err(map_bootstrap_lock_error(&lock_error)),
        };
    let channel = runtime_channel();
    let connection = bootstrap::connect_or_start(
        || connect(&data_dir, &expected_build),
        || {
            let workspace = launch_workspace().map_err(std::io::Error::other)?;
            run_lifecycle(&exe, "start", &workspace)
        },
        || recover_stale_client_endpoint(&data_dir),
        &expected_build,
        channel,
        false,
        IpcClient::server_build,
    );
    let connection = match connection {
        Err(bootstrap::BootstrapError::RolloverRequired(trigger)) => {
            // Keyed by the artifact this daemon advertises, so a client whose own
            // build no longer exists on disk asks once instead of once per lane.
            let may_attempt = should_attempt_automatic_replacement(paths::runtime_mode())
                && ATTEMPTED_REPLACEMENTS.claim(&trigger.running_artifact);
            match bootstrap::replace_or_reuse(
                || connect(&data_dir, &expected_build),
                // Planned, never forced. A rebuild is not a reason to destroy the
                // Agent conversations this daemon owns for another client: its
                // census picks a cold transition only when nothing is live, and a
                // seamless rollover keeps the old PTY masters alive otherwise
                // (#507 / #559).
                || {
                    let workspace = launch_workspace().map_err(std::io::Error::other)?;
                    run_lifecycle_with(&exe, &["daemon", "restart"], "restart", &workspace)
                },
                &expected_build,
                IpcClient::server_build,
                may_attempt,
            ) {
                Ok(bootstrap::BuildMismatchConnection::Replaced(stream)) => Ok(stream),
                Ok(bootstrap::BuildMismatchConnection::Reused { stream, reason }) => {
                    if let Some(entry) = reused_build_mismatch_record(&trigger, &reason) {
                        ErrorLog::record(&entry);
                    }
                    Ok(stream)
                }
                Err(error) => Err(error),
            }
        }
        other => other,
    };
    connection.map_err(|error| match error {
        bootstrap::BootstrapError::RolloverRequired(trigger) => {
            ClientError::RolloverRequired(trigger)
        }
        bootstrap::BootstrapError::UnknownBuildIdentity => ClientError::BuildIdentityUnavailable,
        // Keep the daemon's typed refusal (code, error id, message) so every
        // surface renders "this is another workspace's daemon" instead of an
        // unavailable transport.
        bootstrap::BootstrapError::WorkspaceMismatch(refusal) => ClientError::Protocol(refusal),
        other => ClientError::Lifecycle(other.to_string()),
    })
}

/// Enters the cross-process bootstrap section under a bounded wait.
///
/// The section serializes `connect_or_start` so two clients cannot cold-start
/// two daemons for one data directory. Because the data directory is shared by
/// every usagi process on the machine, the wait is bounded
/// ([`PrivateLockWait::BOOTSTRAP`]) and contention is reported as
/// [`ClientError::BootstrapContended`] rather than folded into "the daemon is
/// unavailable": the daemon may be perfectly healthy and the caller should
/// simply try again, which is exactly what the TUI's reattach backoff does.
pub(super) fn acquire_bootstrap_lock(data_dir: &Path) -> Result<std::fs::File, ClientError> {
    acquire_bootstrap_lock_within(data_dir, PrivateLockWait::BOOTSTRAP)
}

pub(super) fn acquire_bootstrap_lock_within(
    data_dir: &Path,
    wait: PrivateLockWait,
) -> Result<std::fs::File, ClientError> {
    acquire_bootstrap_lock_io_within(data_dir, wait)
        .map_err(|error| map_bootstrap_lock_error(&error))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=active_owner_is_waited_for_without_starting_a_duplicate
pub(super) fn acquire_bootstrap_lock_io_within(
    data_dir: &Path,
    wait: PrivateLockWait,
) -> std::io::Result<std::fs::File> {
    (|| {
        ensure_private_dir_all(data_dir)?;
        // `open_private_lock` runs `ensure_private_dir` on the lock's parent, so
        // creating (and directory-locking) `daemon/` here as well would double
        // the setup locking every bootstrap performs on the shared data dir.
        let path = data_dir.join("daemon").join("bootstrap.lock");
        lock_private_exclusive(
            &path,
            "bootstrap lock",
            PrivateLockModePolicy::OwnerLegacy0644,
            wait,
        )
    })()
}

pub(super) fn map_bootstrap_lock_error(error: &std::io::Error) -> ClientError {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        ClientError::BootstrapContended
    } else {
        ClientError::Unavailable(error.to_string())
    }
}
