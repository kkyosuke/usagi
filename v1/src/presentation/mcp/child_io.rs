//! Bounded draining and waiting for the subprocess an MCP backend drives.
//!
//! The `usagi llm-mcp` backend (`OllamaBackend` at the composition root) runs
//! `ollama run` as a child process: it must drain the child's output without
//! letting a flood deadlock on a full pipe, and bound how long it waits before
//! giving up and killing a wedged run. Both pieces are pure decision logic over
//! injected IO, so they live here — generic over [`std::io::Read`] and a small
//! [`WaitableChild`] trait — and are unit-tested, while the real `Child` is
//! wrapped at the composition root (`main.rs`, coverage-excluded) the same way
//! the MCP backends themselves are.

use std::io::Read;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// Read up to `cap` bytes from `reader`, draining (and discarding) the rest so
/// the child never blocks on a full pipe. Returns the captured bytes and whether
/// the stream was longer than `cap`.
///
/// A read error is **propagated**, not swallowed: a stream that fails partway
/// must not be handed back as a short-but-complete result, or the caller would
/// treat a truncated-by-IO-error response as the child's full output. The
/// post-truncation drain stays best-effort (it only unblocks the pipe).
pub fn read_capped(reader: &mut impl Read, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    // Read one past the cap to detect truncation, then drain the remainder.
    reader.take(cap as u64 + 1).read_to_end(&mut buf)?;
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
        let _ = std::io::copy(reader, &mut std::io::sink());
    }
    Ok((buf, truncated))
}

/// The slice of [`std::process::Child`] that [`wait_with_timeout`] needs: poll
/// for exit, and (on timeout / error) kill and reap. Abstracting these three
/// calls keeps the wait loop unit-testable with a fake; the production wrapper
/// over a real `Child` lives at the composition root (`main.rs`), so the thin
/// delegation stays in the coverage-excluded binary while the loop logic here is
/// covered.
pub trait WaitableChild {
    /// Poll whether the child has exited, without blocking ([`std::process::Child::try_wait`]).
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
    /// Forcibly terminate the child ([`std::process::Child::kill`]).
    fn kill(&mut self) -> std::io::Result<()>;
    /// Block until the child exits, reaping it ([`std::process::Child::wait`]).
    fn wait(&mut self) -> std::io::Result<ExitStatus>;
}

/// Wait for `child` up to `timeout`, re-polling every `poll`, and return its exit
/// status — or `None` after killing and reaping it when the timeout elapses (or a
/// poll errors) first. Killing a wedged child keeps a stuck model or unreachable
/// server from blocking the MCP call forever.
pub fn wait_with_timeout(
    child: &mut impl WaitableChild,
    timeout: Duration,
    poll: Duration,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(poll),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_capped_returns_all_bytes_when_under_the_cap() {
        let mut reader = Cursor::new(b"hello".to_vec());
        let (buf, truncated) = read_capped(&mut reader, 64).unwrap();
        assert_eq!(buf, b"hello");
        assert!(!truncated);
    }

    #[test]
    fn read_capped_keeps_exactly_cap_bytes_without_flagging_truncation() {
        let mut reader = Cursor::new(b"hello".to_vec());
        let (buf, truncated) = read_capped(&mut reader, 5).unwrap();
        assert_eq!(buf, b"hello");
        assert!(!truncated);
    }

    #[test]
    fn read_capped_truncates_and_drains_the_rest_when_over_the_cap() {
        let mut reader = Cursor::new(b"hello world".to_vec());
        let (buf, truncated) = read_capped(&mut reader, 5).unwrap();
        assert_eq!(buf, b"hello");
        assert!(truncated);
        // The remainder was drained to the sink, so the reader is now at EOF.
        assert_eq!(reader.position(), 11);
    }

    #[test]
    fn read_capped_propagates_a_read_error_instead_of_a_short_buffer() {
        // A reader that errors mid-stream must surface the error, not be returned
        // as a complete-but-short output the caller mistakes for the full result.
        struct ErroringReader;
        impl Read for ErroringReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
        }
        let err = read_capped(&mut ErroringReader, 64).unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }

    /// A scripted [`WaitableChild`]: each `try_wait` pops the next queued result,
    /// and `kill` / `wait` record that they ran so the timeout / error path can be
    /// asserted.
    struct FakeChild {
        polls: std::collections::VecDeque<std::io::Result<Option<ExitStatus>>>,
        killed: bool,
        reaped: bool,
    }

    impl FakeChild {
        fn new(polls: Vec<std::io::Result<Option<ExitStatus>>>) -> Self {
            Self {
                polls: polls.into(),
                killed: false,
                reaped: false,
            }
        }
    }

    impl WaitableChild for FakeChild {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            self.polls
                .pop_front()
                .unwrap_or(Ok(None))
                .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed = true;
            Ok(())
        }
        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            self.reaped = true;
            Ok(exited(0))
        }
    }

    /// A real [`ExitStatus`] for the tests. Constructed from a raw wait status on
    /// Unix (where coverage runs); `0` means success.
    #[cfg(unix)]
    fn exited(raw: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(raw)
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_returns_the_status_when_the_child_finishes() {
        let mut child = FakeChild::new(vec![Ok(Some(exited(0)))]);
        let status =
            wait_with_timeout(&mut child, Duration::from_secs(60), Duration::ZERO).expect("status");
        assert!(status.success());
        assert!(!child.killed);
        assert!(!child.reaped);
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_polls_again_while_the_child_is_still_running() {
        // First poll: still running (before the deadline) → sleep and re-poll;
        // second poll: finished. A zero `poll` keeps the test instant.
        let mut child = FakeChild::new(vec![Ok(None), Ok(Some(exited(0)))]);
        let status =
            wait_with_timeout(&mut child, Duration::from_secs(60), Duration::ZERO).expect("status");
        assert!(status.success());
        assert!(child.polls.is_empty());
    }

    #[test]
    fn wait_with_timeout_kills_and_reaps_the_child_when_the_timeout_elapses() {
        // A zero timeout means the deadline is already past on the first `None`,
        // so the loop falls straight through to killing the wedged child.
        let mut child = FakeChild::new(vec![Ok(None)]);
        let status = wait_with_timeout(&mut child, Duration::ZERO, Duration::ZERO);
        assert!(status.is_none());
        assert!(child.killed);
        assert!(child.reaped);
    }

    #[test]
    fn wait_with_timeout_kills_and_reaps_the_child_when_a_poll_errors() {
        let mut child = FakeChild::new(vec![Err(std::io::Error::other("boom"))]);
        let status = wait_with_timeout(&mut child, Duration::from_secs(60), Duration::ZERO);
        assert!(status.is_none());
        assert!(child.killed);
        assert!(child.reaped);
    }
}
