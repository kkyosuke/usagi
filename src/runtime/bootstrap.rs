//! Client-side daemon bootstrap shared by every entry surface.
//!
//! The daemon presentation remains the authority for lifecycle locking.  This
//! adapter only reuses an active endpoint or requests `daemon start` once when
//! no locator exists, then waits for that endpoint to become connectable.

use std::io;
use std::thread;
use std::time::Duration;

const READINESS_ATTEMPTS: usize = 20;
const READINESS_DELAY: Duration = Duration::from_millis(50);

#[coverage(off)]
pub(crate) fn connect_or_start<S, C, L>(mut connect: C, mut start: L) -> io::Result<S>
where
    C: FnMut() -> io::Result<S>,
    L: FnMut() -> io::Result<()>,
{
    match connect() {
        Ok(stream) => Ok(stream),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            start()?;
            wait_for_ready(&mut connect)
        }
        Err(error) => Err(error),
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
    use super::connect_or_start;
    use std::cell::Cell;
    use std::io;

    #[test]
    fn reuses_a_connectable_endpoint_without_starting() {
        let starts = Cell::new(0);
        let stream = connect_or_start(
            || Ok("connected"),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(stream, "connected");
        assert_eq!(starts.get(), 0);
    }

    #[test]
    fn absent_endpoint_starts_once_and_waits_for_readiness() {
        let calls = Cell::new(0);
        let starts = Cell::new(0);
        let stream = connect_or_start(
            || {
                let call = calls.get();
                calls.set(call + 1);
                if call < 2 {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else {
                    Ok("ready")
                }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(stream, "ready");
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn propagates_start_failure_without_retrying_or_falling_back() {
        let starts = Cell::new(0);
        let error = connect_or_start::<(), _, _>(
            || Err(io::Error::from(io::ErrorKind::NotFound)),
            || {
                starts.set(starts.get() + 1);
                Err(io::Error::other("start failed"))
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "start failed");
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn does_not_start_when_an_existing_endpoint_is_unhealthy() {
        let starts = Cell::new(0);
        let error = connect_or_start::<(), _, _>(
            || Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(starts.get(), 0);
    }

    #[test]
    fn reports_a_timeout_when_started_daemon_never_becomes_ready() {
        let calls = Cell::new(0);
        let error = connect_or_start::<(), _, _>(
            || {
                let call = calls.get();
                calls.set(call + 1);
                Err(io::Error::from(if call == 0 {
                    io::ErrorKind::NotFound
                } else {
                    io::ErrorKind::ConnectionRefused
                }))
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("daemon did not become ready"));
    }
}
