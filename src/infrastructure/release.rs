//! Fetch the project's published release tags from its git remote.
//!
//! Shells out to `git ls-remote --tags` so usagi needs no HTTP dependency and
//! the user's existing git authentication / proxy configuration is respected.
//! This is a thin, network-touching IO wrapper (excluded from coverage); the
//! pure parsing and "is a newer version available" decision live in
//! [`crate::usecase::update_check`].

use std::process::Command;

/// Run `git ls-remote --tags --refs <repo_url>` and return its stdout.
///
/// `--refs` filters out peeled tag entries (`^{}`). Returns `None` when git is
/// missing, the remote could not be reached, or git exits non-zero — the caller
/// treats any failure as "no update information", so a missing network never
/// surfaces an error.
pub fn fetch_tags(repo_url: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", repo_url])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
