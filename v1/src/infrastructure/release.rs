//! Fetch the project's published release tags from its git remote.
//!
//! Shells out to `git ls-remote --tags` so usagi needs no HTTP dependency and
//! the user's existing git authentication / proxy configuration is respected.
//! This is a thin, network-touching IO wrapper (excluded from coverage); the
//! pure parsing and "is a newer version available" decision live in
//! [`crate::usecase::update_check`].

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::infrastructure::process::{self, Limits, Outcome};

/// The longest the update check waits on the remote before giving up. An
/// unreachable remote, a stalled proxy, or a credential prompt must not hang
/// usagi: past this the git child is killed and the check reports "no update
/// information".
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the wait loop re-polls the git child.
const FETCH_POLL: Duration = Duration::from_millis(50);
const FETCH_TERMINATE_GRACE: Duration = Duration::from_millis(250);
const FETCH_REAP_GRACE: Duration = Duration::from_millis(250);
const FETCH_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Run `git ls-remote --tags --refs <repo_url>` and return its stdout.
///
/// `--refs` filters out peeled tag entries (`^{}`). Returns `None` when git is
/// missing, the remote could not be reached, git exits non-zero, or the fetch
/// exceeds [`FETCH_TIMEOUT`] — the caller treats any failure as "no update
/// information", so a missing or slow network never surfaces an error or hangs.
pub fn fetch_tags(repo_url: &str) -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(["ls-remote", "--tags", "--refs", repo_url])
        .stdin(Stdio::null())
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
        );
    let Outcome::Exited(output) = process::run(
        command,
        None,
        Limits {
            timeout: FETCH_TIMEOUT,
            terminate_grace: FETCH_TERMINATE_GRACE,
            reap_grace: FETCH_REAP_GRACE,
            poll_interval: FETCH_POLL,
            stdout_cap: FETCH_MAX_BYTES,
            stderr_cap: 0,
        },
    )
    .ok()?
    else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let stdout = output.stdout.ok()?;
    (!stdout.truncated).then(|| String::from_utf8_lossy(&stdout.bytes).into_owned())
}
