//! Client-side daemon bootstrap shared by every entry surface.
//!
//! The daemon presentation remains the authority for lifecycle locking.  This
//! adapter only reuses an active endpoint or requests `daemon start` once when
//! no locator exists, then waits for that endpoint to become connectable.

use std::fmt;
use std::io;
use std::thread;
use std::time::Duration;

use usagi_core::infrastructure::ipc::BuildIdentity;

// `daemon start` confirms the PID record before the subsequently published IPC
// endpoint becomes connectable. Leave room for that bounded publication on a
// cold or contended host instead of surfacing a transient unavailable state.
const READINESS_ATTEMPTS: usize = 40;
const READINESS_DELAY: Duration = Duration::from_millis(50);

#[coverage(off)]
pub(crate) fn connect_or_start<S, C, L, R, B>(
    mut connect: C,
    mut start: L,
    mut restart: R,
    expected_build: &BuildIdentity,
    force_restart: bool,
    build_of: B,
) -> Result<S, BootstrapError>
where
    C: FnMut() -> io::Result<S>,
    L: FnMut() -> io::Result<()>,
    R: FnMut() -> io::Result<()>,
    B: Fn(&S) -> &BuildIdentity,
{
    match connect() {
        Ok(stream) => match build_status(build_of(&stream), expected_build) {
            Ok(true) if !force_restart => Ok(stream),
            Ok(_) => {
                restart().map_err(BootstrapError::Restart)?;
                let stream = wait_for_ready(&mut connect).map_err(BootstrapError::Readiness)?;
                require_expected_build(&stream, expected_build, &build_of)?;
                Ok(stream)
            }
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            start().map_err(BootstrapError::Start)?;
            let stream = wait_for_ready(&mut connect).map_err(BootstrapError::Readiness)?;
            require_expected_build(&stream, expected_build, &build_of)?;
            Ok(stream)
        }
        Err(error) => Err(BootstrapError::Connect(error)),
    }
}

/// A safe, classified bootstrap failure. No variant permits local lifecycle or
/// terminal fallback; callers render only its display message.
#[derive(Debug)]
pub(crate) enum BootstrapError {
    Connect(io::Error),
    Start(io::Error),
    Restart(io::Error),
    Readiness(io::Error),
    UnknownBuildIdentity,
    ReplacementBuildMismatch,
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => {
                let _ = error.kind();
                f.write_str("daemon endpoint is unavailable")
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
    if build_status(build_of(stream), expected_build)? {
        Ok(())
    } else {
        Err(BootstrapError::ReplacementBuildMismatch)
    }
}

#[coverage(off)]
fn build_status(actual: &BuildIdentity, expected: &BuildIdentity) -> Result<bool, BootstrapError> {
    if actual.version.is_empty() || actual.target.is_empty() {
        return Err(BootstrapError::UnknownBuildIdentity);
    }
    Ok(actual == expected)
}

/// Only an interactive debug `cargo run` forces a same-build rollover. Test
/// harnesses and directly executed debug binaries still use their isolated
/// development channel, but reuse a matching daemon like release binaries.
#[must_use]
#[coverage(off)]
pub(crate) const fn should_force_restart(debug_build: bool, cargo_run_parent: bool) -> bool {
    debug_build && cargo_run_parent
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
    use super::{BootstrapError, connect_or_start};
    use std::cell::Cell;
    use std::io;
    use usagi_core::infrastructure::ipc::BuildIdentity;

    #[derive(Debug)]
    struct Endpoint {
        name: &'static str,
        build: BuildIdentity,
    }

    fn build(version: &str) -> BuildIdentity {
        BuildIdentity {
            version: version.into(),
            commit: "unknown".into(),
            target: "test".into(),
        }
    }

    fn endpoint(name: &'static str, version: &str) -> Endpoint {
        Endpoint {
            name,
            build: build(version),
        }
    }

    #[test]
    fn reuses_a_connectable_endpoint_without_starting() {
        let starts = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || Ok(endpoint("connected", "current")),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap();
        assert_eq!(stream.name, "connected");
        assert_eq!(starts.get(), 0);
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
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
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
                Err(io::Error::other("start failed"))
            },
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(error, BootstrapError::Start(_)));
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn does_not_start_when_an_existing_endpoint_is_unhealthy() {
        let starts = Cell::new(0);
        let expected = build("current");
        let error = connect_or_start(
            || Err::<Endpoint, _>(io::Error::from(io::ErrorKind::ConnectionRefused)),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(error, BootstrapError::Connect(_)));
        assert_eq!(starts.get(), 0);
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
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(error, BootstrapError::Readiness(_)));
    }

    #[test]
    fn old_build_restarts_once_then_requires_the_replacement_build() {
        let connects = Cell::new(0);
        let restarts = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || {
                let call = connects.get();
                connects.set(call + 1);
                Ok(endpoint(
                    if call == 0 { "old" } else { "new" },
                    if call == 0 { "old" } else { "current" },
                ))
            },
            || Ok(()),
            || {
                restarts.set(restarts.get() + 1);
                Ok(())
            },
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap();
        assert_eq!(stream.name, "new");
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn development_channel_restarts_a_matching_daemon() {
        let restarts = Cell::new(0);
        let expected = build("current");
        let stream = connect_or_start(
            || Ok(endpoint("new", "current")),
            || Ok(()),
            || {
                restarts.set(restarts.get() + 1);
                Ok(())
            },
            &expected,
            true,
            |stream| &stream.build,
        )
        .unwrap();
        assert_eq!(stream.name, "new");
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn rejects_unknown_or_wrong_replacement_builds() {
        let expected = build("current");
        let unknown = connect_or_start(
            || Ok(endpoint("unknown", "")),
            || Ok(()),
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(unknown, BootstrapError::UnknownBuildIdentity));

        let missing_target = connect_or_start(
            || {
                Ok(Endpoint {
                    name: "unknown-target",
                    build: BuildIdentity {
                        version: "current".into(),
                        commit: "unknown".into(),
                        target: String::new(),
                    },
                })
            },
            || Ok(()),
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
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
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(
            unknown_after_start,
            BootstrapError::UnknownBuildIdentity
        ));

        let mismatch = connect_or_start(
            || Ok(endpoint("old", "old")),
            || Ok(()),
            || Ok(()),
            &expected,
            false,
            |stream| &stream.build,
        )
        .unwrap_err();
        assert!(matches!(mismatch, BootstrapError::ReplacementBuildMismatch));
    }

    #[test]
    fn only_a_debug_cargo_run_forces_a_same_build_restart() {
        assert!(super::should_force_restart(true, true));
        assert!(!super::should_force_restart(true, false));
        assert!(!super::should_force_restart(false, true));
    }

    #[test]
    fn bootstrap_errors_render_only_safe_messages() {
        let errors = [
            (
                BootstrapError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
                "daemon endpoint is unavailable",
            ),
            (
                BootstrapError::Start(io::Error::other("private start detail")),
                "daemon could not be started",
            ),
            (
                BootstrapError::Restart(io::Error::other("private restart detail")),
                "daemon generation could not be restarted",
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
    }
}
