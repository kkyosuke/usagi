//! Client-side daemon bootstrap shared by every entry surface.
//!
//! The daemon presentation remains the authority for lifecycle locking. This
//! adapter reuses an active endpoint or requests `daemon start` once when no
//! locator exists. An unreachable, already-published endpoint may be retired
//! only through an injected ownership proof; the connection error itself is
//! never authority to mutate lifecycle state.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, BuildRolloverTrigger, ProtocolError,
    build_artifact_decision, build_rollover_trigger, is_workspace_mismatch,
};
use usagi_core::usecase::client::ClientError;

// `daemon start` confirms the PID record before the subsequently published IPC
// endpoint becomes connectable. Leave room for that bounded publication on a
// cold or contended host instead of surfacing a transient unavailable state.
const READINESS_ATTEMPTS: u32 = 40;

/// The longest one cold start spends waiting for the freshly spawned daemon to
/// publish its endpoint. Callers that serialize bootstrap across processes size
/// their bounded wait against this, so a concurrent honest cold start is waited
/// out rather than mistaken for a wedged holder.
pub(crate) const READINESS_CEILING: Duration = READINESS_DELAY.saturating_mul(READINESS_ATTEMPTS);
const READINESS_DELAY: Duration = Duration::from_millis(50);

// Keep endpoint ownership in the generic composition function while the
// retry/error decision below has one type-erased, coverage-visible instance.
macro_rules! wait_for_stream {
    ($connect:expr) => {{
        let mut ready = None;
        wait_for_ready(&mut || {
            ready = Some($connect()?);
            Ok(())
        })?;
        ready.expect("a successful readiness probe stores its endpoint")
    }};
}

// The unit suite exercises every action, readiness, recovery, and build-fence
// transition. LLVM nevertheless counts the separately generated production
// `IpcClient` instantiation as uncovered for branches exercised by the fake
// endpoint instantiations.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=runtime::bootstrap::tests
pub(crate) fn connect_or_start<S, C, L, K, B>(
    mut connect: C,
    mut start: L,
    mut recover_stale: K,
    expected_build: &BuildIdentity,
    channel: &str,
    force_replacement: bool,
    build_of: B,
) -> Result<S, BootstrapError>
where
    C: FnMut() -> io::Result<S>,
    L: FnMut() -> io::Result<()>,
    K: FnMut() -> io::Result<StaleRecovery>,
    B: Fn(&S) -> &BuildIdentity,
{
    let result = connect();
    if let Err(error) = &result
        && let Some(refusal) = workspace_refusal(error)
    {
        // The running daemon serves another workspace. That is a definitive
        // answer about *this* endpoint, so it must not be read as an unreachable
        // one: starting, recovering, or replacing a daemon would neither fix the
        // mismatch nor be safe for the workspace that daemon already owns.
        return Err(BootstrapError::WorkspaceMismatch(refusal));
    }
    match result {
        Ok(stream) => {
            match build_artifact_decision(build_of(&stream), expected_build, force_replacement) {
                BuildArtifactDecision::Reuse => Ok(stream),
                BuildArtifactDecision::ForceReplace | BuildArtifactDecision::RolloverTrigger => {
                    let trigger = build_rollover_trigger(
                        build_of(&stream),
                        expected_build,
                        channel,
                        force_replacement,
                    )
                    .ok_or(BootstrapError::UnknownBuildIdentity)?;
                    Err(BootstrapError::RolloverRequired(trigger))
                }
                BuildArtifactDecision::Unknown => Err(BootstrapError::UnknownBuildIdentity),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            start().map_err(start_error)?;
            let stream = wait_for_stream!(connect);
            require_expected_build(build_of(&stream), expected_build)?;
            Ok(stream)
        }
        Err(error) if can_attempt_stale_recovery(error.kind()) => {
            match recover_stale().map_err(BootstrapError::Recovery)? {
                StaleRecovery::Recovered => {
                    start().map_err(start_error)?;
                    let stream = wait_for_stream!(connect);
                    require_expected_build(build_of(&stream), expected_build)?;
                    Ok(stream)
                }
                StaleRecovery::OwnerActive => {
                    let stream = wait_for_stream!(connect);
                    require_expected_build(build_of(&stream), expected_build)?;
                    Ok(stream)
                }
                StaleRecovery::NotProven => Err(BootstrapError::Connect(error)),
            }
        }
        Err(error) => Err(BootstrapError::Connect(error)),
    }
}

/// Performs an explicitly selected cold replacement and reconnects only after
/// the replacement endpoint advertises the expected artifact.
///
/// The composition root is responsible for limiting this destructive fallback
/// to a runtime channel where losing process-owned terminals is acceptable.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=runtime::bootstrap::tests
pub(crate) fn restart_and_connect<S, C, R, B>(
    mut connect: C,
    mut restart: R,
    expected_build: &BuildIdentity,
    build_of: B,
) -> Result<S, BootstrapError>
where
    C: FnMut() -> io::Result<S>,
    R: FnMut() -> io::Result<()>,
    B: Fn(&S) -> &BuildIdentity,
{
    restart().map_err(restart_error)?;
    let stream = wait_for_stream!(connect);
    require_expected_build(build_of(&stream), expected_build)?;
    Ok(stream)
}

/// What development's build-mismatch ladder settled on.
#[derive(Debug)]
pub(crate) enum DevelopmentConnection<S> {
    /// The daemon now advertises this client's exact artifact.
    Replaced(S),
    /// The mismatch stands and the reachable daemon is reused as it is, because
    /// resolving it would have destroyed live runtime — or because this process
    /// already spent its one attempt against that artifact. `reason` is the
    /// non-sensitive explanation for the log.
    Reused { stream: S, reason: String },
}

/// Development's build-mismatch ladder: ask for one *planned* replacement, then
/// keep the mismatched daemon rather than destroy what it owns.
///
/// The replacement is planned, never forced. The daemon's own census then
/// decides between a cold transition (it owns nothing live) and a seamless
/// rollover that keeps the old PTY masters alive, and refuses only when neither
/// is safe. A refusal — or a replacement that still does not advertise this
/// artifact, which is exactly what a rebuilt on-disk executable produces — falls
/// back to reusing the reachable daemon.
///
/// Development prefers that over both alternatives on purpose. A forced cold
/// transition kills the Agent conversations another client is holding (the very
/// reason [`crate::runtime::daemon`]'s planned replacement guard exists), and a
/// hard refusal would wedge every control request of a client whose own build no
/// longer exists on disk.
// Every rung is exercised through the fake endpoint below. LLVM nevertheless
// counts the separately generated production `IpcClient` instantiation as
// uncovered, exactly as it does for the two functions above.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=runtime::bootstrap::tests
pub(crate) fn replace_or_reuse<S, C, R, B>(
    mut connect: C,
    restart: R,
    expected_build: &BuildIdentity,
    build_of: B,
    may_attempt: bool,
) -> Result<DevelopmentConnection<S>, BootstrapError>
where
    C: FnMut() -> io::Result<S>,
    R: FnMut() -> io::Result<()>,
    B: Fn(&S) -> &BuildIdentity,
{
    let reason = if may_attempt {
        match restart_and_connect(&mut connect, restart, expected_build, &build_of) {
            Ok(stream) => return Ok(DevelopmentConnection::Replaced(stream)),
            // A daemon serving another workspace is a definitive answer about
            // this endpoint, not a build mismatch to work around: there is
            // nothing here this client may reuse.
            Err(BootstrapError::WorkspaceMismatch(refusal)) => {
                return Err(BootstrapError::WorkspaceMismatch(refusal));
            }
            Err(error) => error.to_string(),
        }
    } else {
        "this daemon build was already asked to be replaced".to_owned()
    };
    let stream = connect().map_err(|error| match workspace_refusal(&error) {
        Some(refusal) => BootstrapError::WorkspaceMismatch(refusal),
        None => BootstrapError::Connect(error),
    })?;
    Ok(DevelopmentConnection::Reused { stream, reason })
}

/// The daemon artifacts one action of this process has already been spent on.
///
/// Two actions are once-per-artifact, and each keeps its own set: asking for a
/// replacement, and recording a reuse in the log. A client's own artifact is a
/// compile-time constant, but the executable a replacement launches is whatever
/// is on disk when it runs. The two differ as soon as somebody rebuilds while
/// this process keeps running, so the replacement cannot bring the daemon to
/// *this* artifact and asking again would only churn one generation per
/// bootstrap — several per second across the render and pump lanes. The first
/// observation of a daemon artifact is therefore worth one attempt (and one log
/// entry); every later observation of the same artifact is silent reuse.
pub(crate) struct OncePerArtifact(Mutex<BTreeSet<String>>);

impl OncePerArtifact {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(BTreeSet::new()))
    }

    /// Whether this process may still spend its one action on `artifact`. A
    /// poisoned lock is read through: the set only bounds churn, and losing it
    /// must not fail a bootstrap.
    pub(crate) fn claim(&self, artifact: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(artifact.to_owned())
    }
}

/// Result of the composition root's lock- and identity-fenced stale-owner
/// proof. `NotProven` is intentionally distinct from an error: live, replaced,
/// or identity-unknown owners remain untouched and preserve the original
/// connection failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StaleRecovery {
    Recovered,
    /// The singleton fence is held by a live or starting owner. Preserve all
    /// state and only wait for that owner to make its endpoint connectable.
    OwnerActive,
    NotProven,
}

fn can_attempt_stale_recovery(kind: io::ErrorKind) -> bool {
    kind == io::ErrorKind::ConnectionRefused
}

/// The typed workspace-fence refusal carried by a connect failure, if that is
/// what it was. The composition root wraps client errors as `io::Error::other`,
/// so the classification reads the original typed error instead of a message.
fn workspace_refusal(error: &io::Error) -> Option<ProtocolError> {
    match error.get_ref()?.downcast_ref::<ClientError>()? {
        ClientError::Protocol(refusal) if is_workspace_mismatch(refusal) => Some(refusal.clone()),
        _ => None,
    }
}

fn start_error(error: io::Error) -> BootstrapError {
    workspace_refusal(&error).map_or(
        BootstrapError::Start(error),
        BootstrapError::WorkspaceMismatch,
    )
}

fn restart_error(error: io::Error) -> BootstrapError {
    workspace_refusal(&error).map_or(
        BootstrapError::Restart(error),
        BootstrapError::WorkspaceMismatch,
    )
}

/// A safe, classified bootstrap failure. No variant permits local lifecycle or
/// terminal fallback; callers render only its display message.
#[derive(Debug)]
pub(crate) enum BootstrapError {
    Connect(io::Error),
    Recovery(io::Error),
    Start(io::Error),
    Restart(io::Error),
    Readiness(io::Error),
    UnknownBuildIdentity,
    ReplacementBuildMismatch,
    RolloverRequired(BuildRolloverTrigger),
    /// The reachable daemon owns a different workspace. Callers surface the
    /// daemon's own refusal so the user learns which workspace is served instead
    /// of an unavailable endpoint.
    WorkspaceMismatch(ProtocolError),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => {
                let _ = error.kind();
                f.write_str("daemon endpoint is unavailable")
            }
            Self::Recovery(error) => {
                let _ = error.kind();
                f.write_str("daemon endpoint could not be recovered")
            }
            Self::Start(error) => {
                let _ = error.kind();
                f.write_str("daemon could not be started")
            }
            Self::Restart(error) => {
                let _ = error.kind();
                f.write_str("daemon generation could not be restarted")
            }
            Self::Readiness(error) => {
                let _ = error.kind();
                f.write_str("daemon did not become ready")
            }
            Self::UnknownBuildIdentity => f.write_str("daemon build identity is unavailable"),
            Self::ReplacementBuildMismatch => {
                f.write_str("replacement daemon build does not match this client")
            }
            Self::RolloverRequired(trigger) => write!(
                f,
                "daemon build rollover is required (operation {})",
                trigger.operation_id.0
            ),
            Self::WorkspaceMismatch(refusal) => f.write_str(&refusal.message),
        }
    }
}

impl std::error::Error for BootstrapError {}

fn require_expected_build(
    actual_build: &BuildIdentity,
    expected_build: &BuildIdentity,
) -> Result<(), BootstrapError> {
    match build_artifact_decision(actual_build, expected_build, false) {
        BuildArtifactDecision::Reuse => Ok(()),
        BuildArtifactDecision::RolloverTrigger | BuildArtifactDecision::ForceReplace => {
            Err(BootstrapError::ReplacementBuildMismatch)
        }
        BuildArtifactDecision::Unknown => Err(BootstrapError::UnknownBuildIdentity),
    }
}

fn wait_for_ready(connect: &mut dyn FnMut() -> io::Result<()>) -> Result<(), BootstrapError> {
    let mut last_error = io::Error::other("daemon did not publish an endpoint");
    for _ in 0..READINESS_ATTEMPTS {
        match connect() {
            Ok(()) => return Ok(()),
            // A workspace refusal is the endpoint's final answer, not a
            // publication delay: waiting cannot change it, so report it at once
            // instead of spending the readiness budget on it.
            Err(error) => match workspace_refusal(&error) {
                Some(refusal) => return Err(BootstrapError::WorkspaceMismatch(refusal)),
                None => last_error = error,
            },
        }
        thread::sleep(READINESS_DELAY);
    }
    Err(BootstrapError::Readiness(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("daemon did not become ready: {last_error}"),
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapError, DevelopmentConnection, StaleRecovery, connect_or_start, replace_or_reuse,
        restart_and_connect, workspace_refusal,
    };
    use std::cell::Cell;
    use std::io;
    use usagi_core::infrastructure::ipc::{BuildIdentity, build_rollover_trigger};
    use usagi_core::usecase::client::ClientError;

    #[derive(Debug)]
    struct Endpoint {
        name: &'static str,
        build: BuildIdentity,
    }

    fn build(version: &str) -> BuildIdentity {
        let source = if version == "current" {
            "a"
        } else if version == "old" {
            "b"
        } else {
            "c"
        }
        .repeat(64);
        usagi_core::infrastructure::ipc::build_identity(version, "test", "test", "debug", &source)
    }

    fn endpoint(name: &'static str, version: &str) -> Endpoint {
        Endpoint {
            name,
            build: build(version),
        }
    }

    fn endpoint_build(stream: &Endpoint) -> &BuildIdentity {
        &stream.build
    }

    fn rollover(
        error: BootstrapError,
    ) -> Option<usagi_core::infrastructure::ipc::BuildRolloverTrigger> {
        match error {
            BootstrapError::RolloverRequired(trigger) => Some(trigger),
            _ => None,
        }
    }

    fn workspace_mismatch(
        error: BootstrapError,
    ) -> Option<usagi_core::infrastructure::ipc::ProtocolError> {
        match error {
            BootstrapError::WorkspaceMismatch(refusal) => Some(refusal),
            _ => None,
        }
    }

    fn lifecycle_error() -> io::Result<()> {
        Err(io::Error::other("lifecycle action failed"))
    }

    fn recovery_error() -> io::Result<StaleRecovery> {
        Err(io::Error::other("private cleanup detail"))
    }

    fn assert_same_variant(actual: &BootstrapError, expected: &BootstrapError) {
        assert_eq!(
            std::mem::discriminant(actual),
            std::mem::discriminant(expected)
        );
    }

    fn assert_recovery_requires_expected_build(recovery: StaleRecovery) {
        let connects = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || {
                let call = connects.get();
                connects.set(call + 1);
                if call == 0 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(endpoint("wrong-owner", "old"))
                }
            },
            || Ok(()),
            || Ok(recovery),
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(&error, &BootstrapError::ReplacementBuildMismatch);
    }

    fn assert_safe_message(error: &BootstrapError, expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn workspace_refusal_ignores_untyped_and_non_protocol_errors() {
        assert_eq!(
            workspace_refusal(&io::Error::from(io::ErrorKind::ConnectionRefused)),
            None
        );
        assert_eq!(workspace_refusal(&io::Error::other("untyped")), None);
        assert_eq!(
            workspace_refusal(&io::Error::other(ClientError::BuildIdentityUnavailable)),
            None
        );
    }

    #[test]
    fn reuses_a_connectable_endpoint_without_starting() {
        let expected = build("current");
        let stream = connect_or_start(
            || Ok(endpoint("connected", "current")),
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "connected");
    }

    #[test]
    fn absent_endpoint_starts_once_and_waits_for_readiness() {
        let calls = Cell::new(0);
        let starts = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || {
                let call = calls.get();
                calls.set(call + 1);
                if call < 2 {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else {
                    Ok(endpoint("ready", "current"))
                }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "ready");
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn propagates_start_failure_without_retrying_or_falling_back() {
        let starts = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::NotFound)),
            || {
                starts.set(starts.get() + 1);
                lifecycle_error()
            },
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &error,
            &BootstrapError::Start(io::Error::other("expected variant")),
        );
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn unproven_stale_endpoint_is_not_started() {
        let recoveries = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            lifecycle_error,
            || {
                recoveries.set(recoveries.get() + 1);
                Ok(StaleRecovery::NotProven)
            },
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &error,
            &BootstrapError::Connect(io::Error::other("expected variant")),
        );
        assert_eq!(recoveries.get(), 1);
    }

    #[test]
    fn proven_stale_endpoint_is_recovered_then_started_once() {
        let connects = Cell::new(0);
        let starts = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || {
                let call = connects.get();
                connects.set(call + 1);
                if call == 0 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(endpoint("replacement", "current"))
                }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            || Ok(StaleRecovery::Recovered),
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "replacement");
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn active_owner_is_waited_for_without_starting_a_duplicate() {
        let connects = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || {
                let call = connects.get();
                connects.set(call + 1);
                if call == 0 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(endpoint("owner", "current"))
                }
            },
            lifecycle_error,
            || Ok(StaleRecovery::OwnerActive),
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "owner");
    }

    #[test]
    fn unsafe_locator_failure_never_attempts_stale_recovery() {
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::PermissionDenied)),
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &error,
            &BootstrapError::Connect(io::Error::other("expected variant")),
        );
    }

    #[test]
    fn recovery_failure_is_classified_without_starting() {
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &error,
            &BootstrapError::Recovery(io::Error::other("expected variant")),
        );
        assert_eq!(error.to_string(), "daemon endpoint could not be recovered");
    }

    #[test]
    fn reports_a_timeout_when_started_daemon_never_becomes_ready() {
        let calls = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || {
                let call = calls.get();
                calls.set(call + 1);
                Err::<Endpoint, _>(io::Error::from(if call == 0 {
                    io::ErrorKind::NotFound
                } else {
                    io::ErrorKind::ConnectionRefused
                }))
            },
            || Ok(()),
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &error,
            &BootstrapError::Readiness(io::Error::other("expected variant")),
        );
    }

    #[test]
    fn recovered_or_active_owner_requires_the_expected_build() {
        assert_recovery_requires_expected_build(StaleRecovery::Recovered);
        assert_recovery_requires_expected_build(StaleRecovery::OwnerActive);
    }

    #[test]
    fn recovered_owner_propagates_start_and_readiness_failures() {
        let expected = build("current");
        let start_error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            lifecycle_error,
            || Ok(StaleRecovery::Recovered),
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &start_error,
            &BootstrapError::Start(io::Error::other("expected variant")),
        );

        let readiness_error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            || Ok(()),
            || Ok(StaleRecovery::Recovered),
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &readiness_error,
            &BootstrapError::Readiness(io::Error::other("expected variant")),
        );
    }

    #[test]
    fn old_build_returns_one_effect_free_rollover_trigger() {
        let expected = build("current");
        let error = connect_or_start(
            || Ok(endpoint("old", "old")),
            lifecycle_error,
            recovery_error,
            &expected,
            "development",
            false,
            endpoint_build,
        )
        .unwrap_err();
        let trigger = rollover(error).unwrap();
        assert!(rollover(BootstrapError::UnknownBuildIdentity).is_none());
        assert_eq!(trigger.channel, "development");
        assert!(!trigger.forced);
        assert_eq!(
            trigger.running_artifact,
            format!("usagi-artifact-v1:debug:test:{}", "b".repeat(64))
        );
        assert_eq!(
            trigger.expected_artifact,
            format!("usagi-artifact-v1:debug:test:{}", "a".repeat(64))
        );
    }

    #[test]
    fn explicit_force_replacement_triggers_but_plain_reconnect_reuses() {
        let expected = build("current");
        let error = connect_or_start(
            || Ok(endpoint("new", "current")),
            lifecycle_error,
            recovery_error,
            &expected,
            "development",
            true,
            endpoint_build,
        )
        .unwrap_err();
        let trigger = rollover(error).unwrap();
        assert!(trigger.forced);

        let stream = connect_or_start(
            || Ok(endpoint("same", "current")),
            lifecycle_error,
            recovery_error,
            &expected,
            "development",
            false,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "same");
    }

    #[test]
    fn selected_cold_restart_requires_the_expected_replacement_build() {
        let expected = build("current");
        let restarts = Cell::new(0);
        let connect = || Ok(endpoint("replacement", "current"));
        let stream = restart_and_connect(
            connect,
            || {
                restarts.set(restarts.get() + 1);
                Ok(())
            },
            &expected,
            endpoint_build,
        )
        .unwrap();
        assert_eq!(stream.name, "replacement");
        assert_eq!(restarts.get(), 1);

        let restart_error =
            restart_and_connect(connect, lifecycle_error, &expected, endpoint_build).unwrap_err();
        assert_same_variant(
            &restart_error,
            &BootstrapError::Restart(io::Error::other("expected variant")),
        );

        let mismatch = restart_and_connect(
            || Ok(endpoint("wrong", "old")),
            || Ok(()),
            &expected,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(&mismatch, &BootstrapError::ReplacementBuildMismatch);
    }

    #[test]
    fn rejects_unknown_or_wrong_replacement_builds() {
        let expected = build("current");
        let unknown = connect_or_start(
            || Ok(endpoint("unknown", "")),
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(&unknown, &BootstrapError::UnknownBuildIdentity);

        let missing_target = connect_or_start(
            || {
                Ok(Endpoint {
                    name: "unknown-target",
                    build: BuildIdentity {
                        version: "current".into(),
                        commit: "test".into(),
                        target: String::new(),
                        artifact: format!("usagi-artifact-v1:debug:test:{}", "a".repeat(64)),
                    },
                })
            },
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(&missing_target, &BootstrapError::UnknownBuildIdentity);

        let calls = Cell::new(0);
        let unknown_after_start = connect_or_start(
            || {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else {
                    Ok(endpoint("unknown-after-start", ""))
                }
            },
            || Ok(()),
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(&unknown_after_start, &BootstrapError::UnknownBuildIdentity);
    }

    #[test]
    fn a_daemon_serving_another_workspace_is_neither_started_recovered_nor_replaced() {
        use usagi_core::infrastructure::ipc::{ClientWorkspace, workspace_admission};
        use usagi_core::usecase::client::ClientError;

        let refusal = workspace_admission(
            Some(&ClientWorkspace::Bound {
                root: "/workspace/other".into(),
            }),
            "/workspace/root",
        )
        .unwrap_err();
        let refused =
            || Err::<Endpoint, _>(io::Error::other(ClientError::Protocol(refusal.clone())));
        let expected = build("current");

        let error = connect_or_start(
            refused,
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();

        // The endpoint answered, so it is neither absent nor stale: replacing or
        // restarting a daemon that legitimately owns another workspace would be
        // both useless and destructive.
        let surfaced = workspace_mismatch(error).unwrap();
        assert!(workspace_mismatch(BootstrapError::UnknownBuildIdentity).is_none());
        assert_eq!(surfaced, refusal);
        // The caller renders the daemon's own message, which names the workspace
        // that is served.
        assert_eq!(
            BootstrapError::WorkspaceMismatch(refusal.clone()).to_string(),
            refusal.message
        );

        // A cold-start preflight runs only after endpoint absence is known. Its
        // typed refusal must survive the lifecycle callback's io boundary.
        let preflight = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::NotFound)),
            || Err(io::Error::other(ClientError::Protocol(refusal.clone()))),
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_eq!(workspace_mismatch(preflight).unwrap(), refusal);

        // An explicit cold replacement is refused for the same reason.
        let restart_error =
            restart_and_connect(refused, || Ok(()), &expected, endpoint_build).unwrap_err();
        assert!(workspace_mismatch(restart_error).is_some());
        let reconnect = || Ok(endpoint("unused", "current"));
        assert_eq!(reconnect().unwrap().name, "unused");
        let restart_preflight = restart_and_connect(
            reconnect,
            || Err(io::Error::other(ClientError::Protocol(refusal.clone()))),
            &expected,
            endpoint_build,
        )
        .unwrap_err();
        assert_eq!(workspace_mismatch(restart_preflight).unwrap(), refusal);

        // Only this refusal is reclassified. Every other typed client failure
        // keeps its existing connect handling, so the fence cannot swallow an
        // unrelated protocol error.
        let unrelated = connect_or_start(
            || {
                Err::<Endpoint, _>(io::Error::other(ClientError::Protocol(
                    usagi_core::infrastructure::ipc::ProtocolError::new(
                        usagi_core::infrastructure::ipc::ErrorCode::Busy,
                        "daemon is busy",
                    ),
                )))
            },
            lifecycle_error,
            recovery_error,
            &expected,
            "local",
            false,
            endpoint_build,
        )
        .unwrap_err();
        assert_same_variant(
            &unrelated,
            &BootstrapError::Connect(io::Error::other("expected variant")),
        );
    }

    /// The endpoint a development ladder settled on, and whether the mismatch
    /// survived it.
    fn settled(outcome: DevelopmentConnection<Endpoint>) -> (&'static str, Option<String>) {
        match outcome {
            DevelopmentConnection::Replaced(stream) => (stream.name, None),
            DevelopmentConnection::Reused { stream, reason } => (stream.name, Some(reason)),
        }
    }

    #[test]
    fn a_planned_replacement_that_reaches_this_build_is_adopted() {
        let restarts = Cell::new(0);
        let expected = build("current");
        let outcome = replace_or_reuse(
            || Ok(endpoint("replacement", "current")),
            counted_restart(&restarts),
            &expected,
            endpoint_build,
            true,
        )
        .unwrap();
        assert_eq!(settled(outcome), ("replacement", None));
        assert_eq!(restarts.get(), 1);
    }

    /// A successful replacement that records each attempt. Shared by the cases
    /// that expect an attempt and the case that expects none, so one code site
    /// answers "how many times was the daemon asked to replace itself".
    fn counted_restart(restarts: &Cell<u32>) -> impl FnMut() -> io::Result<()> + '_ {
        move || {
            restarts.set(restarts.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn a_refused_replacement_reuses_the_daemon_instead_of_destroying_its_runtime() {
        let expected = build("current");
        let outcome = replace_or_reuse(
            || Ok(endpoint("live-owner", "old")),
            // What the daemon's live-runtime census answers while it still owns
            // another client's Agent: the transition is refused, effect zero.
            lifecycle_error,
            &expected,
            endpoint_build,
            true,
        )
        .unwrap();
        let (name, reason) = settled(outcome);
        assert_eq!(name, "live-owner");
        assert_eq!(
            reason.as_deref(),
            Some("daemon generation could not be restarted")
        );
    }

    #[test]
    fn a_replacement_that_still_advertises_another_build_is_reused_not_retried() {
        let restarts = Cell::new(0);
        let expected = build("current");
        // What a rebuilt on-disk executable produces: the replacement succeeds and
        // is somebody else's artifact again, so this client can never reach its
        // own build by asking for another one.
        let outcome = replace_or_reuse(
            || Ok(endpoint("newer", "old")),
            counted_restart(&restarts),
            &expected,
            endpoint_build,
            true,
        )
        .unwrap();
        let (name, reason) = settled(outcome);
        assert_eq!(name, "newer");
        assert_eq!(
            reason.as_deref(),
            Some("replacement daemon build does not match this client")
        );
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn an_already_attempted_artifact_is_reused_without_asking_again() {
        let expected = build("current");
        let restarts = Cell::new(0);
        let outcome = replace_or_reuse(
            || Ok(endpoint("standing-mismatch", "old")),
            counted_restart(&restarts),
            &expected,
            endpoint_build,
            false,
        )
        .unwrap();
        let (name, reason) = settled(outcome);
        assert_eq!(name, "standing-mismatch");
        assert_eq!(
            reason.as_deref(),
            Some("this daemon build was already asked to be replaced")
        );
        // Every later lane of this process reuses in place: nothing is restarted,
        // so a mismatch cannot churn one generation per bootstrap.
        assert_eq!(restarts.get(), 0);
    }

    #[test]
    fn one_replacement_attempt_is_claimed_per_daemon_artifact() {
        let attempts = super::OncePerArtifact::new();
        assert!(attempts.claim("usagi-artifact-v1:debug:test:old"));
        assert!(!attempts.claim("usagi-artifact-v1:debug:test:old"));
        // A daemon that changed artifact again is a new observation, and worth the
        // one attempt that may reach this client's build.
        assert!(attempts.claim("usagi-artifact-v1:debug:test:newer"));
    }

    #[test]
    fn a_reused_endpoint_keeps_the_workspace_fence_and_connect_failures_typed() {
        use usagi_core::infrastructure::ipc::{ClientWorkspace, workspace_admission};

        let expected = build("current");
        let refusal = workspace_admission(
            Some(&ClientWorkspace::Bound {
                root: "/workspace/other".into(),
            }),
            "/workspace/root",
        )
        .unwrap_err();
        let refused =
            || Err::<Endpoint, _>(io::Error::other(ClientError::Protocol(refusal.clone())));

        // Another workspace's daemon is a definitive answer about the endpoint,
        // so it is neither replaced nor reused — on either rung of the ladder.
        let attempted =
            replace_or_reuse(refused, || Ok(()), &expected, endpoint_build, true).unwrap_err();
        assert_eq!(workspace_mismatch(attempted).as_ref(), Some(&refusal));
        let without_attempt =
            replace_or_reuse(refused, lifecycle_error, &expected, endpoint_build, false)
                .unwrap_err();
        assert_eq!(workspace_mismatch(without_attempt).as_ref(), Some(&refusal));

        // An endpoint that stopped answering keeps its transport classification
        // rather than being reported as a build decision.
        let unreachable = replace_or_reuse(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            lifecycle_error,
            &expected,
            endpoint_build,
            false,
        )
        .unwrap_err();
        assert_same_variant(
            &unreachable,
            &BootstrapError::Connect(io::Error::other("expected variant")),
        );
    }

    #[test]
    fn bootstrap_errors_render_only_safe_messages() {
        assert_safe_message(
            &BootstrapError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
            "daemon endpoint is unavailable",
        );
        assert_safe_message(
            &BootstrapError::Recovery(io::Error::other("private recovery detail")),
            "daemon endpoint could not be recovered",
        );
        assert_safe_message(
            &BootstrapError::Start(io::Error::other("private start detail")),
            "daemon could not be started",
        );
        assert_safe_message(
            &BootstrapError::Restart(io::Error::other("private restart detail")),
            "daemon generation could not be restarted",
        );
        assert_safe_message(
            &BootstrapError::Readiness(io::Error::from(io::ErrorKind::TimedOut)),
            "daemon did not become ready",
        );
        assert_safe_message(
            &BootstrapError::UnknownBuildIdentity,
            "daemon build identity is unavailable",
        );
        assert_safe_message(
            &BootstrapError::ReplacementBuildMismatch,
            "replacement daemon build does not match this client",
        );
        let trigger = build_rollover_trigger(&build("old"), &build("new"), "local", false).unwrap();
        assert!(
            BootstrapError::RolloverRequired(trigger)
                .to_string()
                .starts_with("daemon build rollover is required (operation build-rollover-v1-")
        );
    }
}
