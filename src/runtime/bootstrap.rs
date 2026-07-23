//! Client-side daemon bootstrap shared by every entry surface.
//!
//! The daemon presentation remains the authority for lifecycle locking. This
//! adapter reuses an active endpoint or requests `daemon start` once when no
//! locator exists. An unreachable, already-published endpoint may be retired
//! only through an injected ownership proof; the connection error itself is
//! never authority to mutate lifecycle state.

use std::fmt;
use std::io;
use std::thread;
use std::time::Duration;

use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, BuildRolloverTrigger, build_artifact_decision,
    build_rollover_trigger,
};

// `daemon start` confirms the PID record before the subsequently published IPC
// endpoint becomes connectable. Leave room for that bounded publication on a
// cold or contended host instead of surfacing a transient unavailable state.
const READINESS_ATTEMPTS: usize = 40;
const READINESS_DELAY: Duration = Duration::from_millis(50);

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
    match connect() {
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
            start().map_err(BootstrapError::Start)?;
            let stream = wait_for_ready(&mut connect).map_err(BootstrapError::Readiness)?;
            require_expected_build(&stream, expected_build, &build_of)?;
            Ok(stream)
        }
        Err(error) if can_attempt_stale_recovery(error.kind()) => {
            match recover_stale().map_err(BootstrapError::Recovery)? {
                StaleRecovery::Recovered => {
                    start().map_err(BootstrapError::Start)?;
                    let stream = wait_for_ready(&mut connect).map_err(BootstrapError::Readiness)?;
                    require_expected_build(&stream, expected_build, &build_of)?;
                    Ok(stream)
                }
                StaleRecovery::OwnerActive => {
                    let stream = wait_for_ready(&mut connect).map_err(BootstrapError::Readiness)?;
                    require_expected_build(&stream, expected_build, &build_of)?;
                    Ok(stream)
                }
                StaleRecovery::NotProven => Err(BootstrapError::Connect(error)),
            }
        }
        Err(error) => Err(BootstrapError::Connect(error)),
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

/// A safe, classified bootstrap failure. No variant permits local lifecycle or
/// terminal fallback; callers render only its display message.
#[derive(Debug)]
pub(crate) enum BootstrapError {
    Connect(io::Error),
    Recovery(io::Error),
    Start(io::Error),
    Readiness(io::Error),
    UnknownBuildIdentity,
    ReplacementBuildMismatch,
    RolloverRequired(BuildRolloverTrigger),
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
        }
    }
}

impl std::error::Error for BootstrapError {}

#[coverage(off)]
fn require_expected_build<S, B>(
    stream: &S,
    expected_build: &BuildIdentity,
    build_of: &B,
) -> Result<(), BootstrapError>
where
    B: Fn(&S) -> &BuildIdentity,
{
    match build_artifact_decision(build_of(stream), expected_build, false) {
        BuildArtifactDecision::Reuse => Ok(()),
        BuildArtifactDecision::RolloverTrigger | BuildArtifactDecision::ForceReplace => {
            Err(BootstrapError::ReplacementBuildMismatch)
        }
        BuildArtifactDecision::Unknown => Err(BootstrapError::UnknownBuildIdentity),
    }
}

#[coverage(off)]
fn wait_for_ready<S, C>(connect: &mut C) -> io::Result<S>
where
    C: FnMut() -> io::Result<S>,
{
    let mut last_error = io::Error::other("daemon did not publish an endpoint");
    for _ in 0..READINESS_ATTEMPTS {
        match connect() {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error,
        }
        thread::sleep(READINESS_DELAY);
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("daemon did not become ready: {last_error}"),
    ))
}

#[cfg(test)]
#[coverage(off)]
mod tests {
    use super::{BootstrapError, StaleRecovery, connect_or_start};
    use std::cell::Cell;
    use std::io;
    use usagi_core::infrastructure::ipc::{BuildIdentity, build_rollover_trigger};

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

    fn lifecycle_error() -> io::Result<()> {
        Err(io::Error::other("lifecycle action failed"))
    }

    fn recovery_error() -> io::Result<StaleRecovery> {
        Err(io::Error::other("private cleanup detail"))
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
        assert!(matches!(error, BootstrapError::Start(_)));
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn unproven_stale_endpoint_is_not_started() {
        let starts = Cell::new(0);
        let recoveries = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
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
        assert!(matches!(error, BootstrapError::Connect(_)));
        assert_eq!(starts.get(), 0);
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
        assert!(matches!(error, BootstrapError::Connect(_)));
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
        assert!(matches!(error, BootstrapError::Recovery(_)));
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
        assert!(matches!(error, BootstrapError::Readiness(_)));
    }

    #[test]
    fn recovered_or_active_owner_requires_the_expected_build() {
        for recovery in [StaleRecovery::Recovered, StaleRecovery::OwnerActive] {
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
            assert!(matches!(error, BootstrapError::ReplacementBuildMismatch));
        }
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
        assert!(matches!(start_error, BootstrapError::Start(_)));

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
        assert!(matches!(readiness_error, BootstrapError::Readiness(_)));
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
        let BootstrapError::RolloverRequired(trigger) = error else {
            panic!("mismatch must return a typed trigger");
        };
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
        let BootstrapError::RolloverRequired(trigger) = error else {
            panic!("force must return a typed trigger");
        };
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
        assert!(matches!(unknown, BootstrapError::UnknownBuildIdentity));

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
        assert!(matches!(
            missing_target,
            BootstrapError::UnknownBuildIdentity
        ));

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
        assert!(matches!(
            unknown_after_start,
            BootstrapError::UnknownBuildIdentity
        ));
    }

    #[test]
    fn bootstrap_errors_render_only_safe_messages() {
        let errors = [
            (
                BootstrapError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
                "daemon endpoint is unavailable",
            ),
            (
                BootstrapError::Recovery(io::Error::other("private recovery detail")),
                "daemon endpoint could not be recovered",
            ),
            (
                BootstrapError::Start(io::Error::other("private start detail")),
                "daemon could not be started",
            ),
            (
                BootstrapError::Readiness(io::Error::from(io::ErrorKind::TimedOut)),
                "daemon did not become ready",
            ),
            (
                BootstrapError::UnknownBuildIdentity,
                "daemon build identity is unavailable",
            ),
            (
                BootstrapError::ReplacementBuildMismatch,
                "replacement daemon build does not match this client",
            ),
        ];
        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
        let trigger = build_rollover_trigger(&build("old"), &build("new"), "local", false).unwrap();
        assert!(
            BootstrapError::RolloverRequired(trigger)
                .to_string()
                .starts_with("daemon build rollover is required (operation build-rollover-v1-")
        );
    }
}
