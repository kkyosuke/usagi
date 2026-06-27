//! Fetch the project's published release tags from its git remote.
//!
//! Shells out to `git ls-remote --tags` so usagi needs no HTTP dependency and
//! the user's existing git authentication / proxy configuration is respected.
//! This is a thin, network-touching IO wrapper (excluded from coverage); the
//! pure parsing and "is a newer version available" decision live in
//! [`crate::usecase::update_check`].

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The longest the update check waits on the remote before giving up. An
/// unreachable remote, a stalled proxy, or a credential prompt must not hang
/// usagi: past this the git child is killed and the check reports "no update
/// information".
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the wait loop re-polls the git child.
const FETCH_POLL: Duration = Duration::from_millis(50);

/// Run `git ls-remote --tags --refs <repo_url>` and return its stdout.
///
/// `--refs` filters out peeled tag entries (`^{}`). Returns `None` when git is
/// missing, the remote could not be reached, git exits non-zero, or the fetch
/// exceeds [`FETCH_TIMEOUT`] — the caller treats any failure as "no update
/// information", so a missing or slow network never surfaces an error or hangs.
pub fn fetch_tags(repo_url: &str) -> Option<String> {
    let mut child = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", repo_url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Never block on an interactive credential prompt in this headless check.
        .env("GIT_TERMINAL_PROMPT", "0")
        // Abort an HTTP transfer that stalls below 1 byte/s for the window, so a
        // half-open connection is bounded even if the kill below races it.
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
        .env(
            "GIT_HTTP_LOW_SPEED_TIME",
            FETCH_TIMEOUT.as_secs().to_string(),
        )
        .spawn()
        .ok()?;

    // Drain stdout on a thread so a large tag list cannot deadlock on a full pipe
    // while the main thread bounds the wall-clock wait. Deliver the result over a
    // channel rather than joining the thread directly: killing the git child does
    // not close a stdout write-end that a daemonized grandchild (an ssh
    // ControlMaster, a credential-cache daemon) may have inherited, so
    // `read_to_end` could block forever. An unbounded `join` would then hang; a
    // bounded `recv` instead lets the check give up and leak at most one detached
    // thread, never freezing the caller.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + FETCH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(FETCH_POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    // Collect the drained stdout, but never wait past the timeout: once the child
    // has exited normally its buffered output is already readable, so `recv`
    // returns promptly; only the pathological grandchild-holds-the-pipe case waits
    // the full window and then gives up (the reader thread exits on its own when
    // the fd is finally closed).
    let bytes = rx.recv_timeout(FETCH_TIMEOUT).ok()?;
    if !status?.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
