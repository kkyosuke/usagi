//! The injected seam for shelling out to `git`.

use std::path::Path;

use anyhow::Result;

/// The captured result of one `git` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// Whether git exited with a zero status.
    pub success: bool,
    /// Captured stdout (callers trim / parse as needed).
    pub stdout: String,
    /// Captured stderr (used for the message on failure, and to branch on
    /// specific git errors such as "is not a working tree").
    pub stderr: String,
}

/// Runs `git` commands scoped to a repository. Implemented for real by the
/// composition root (spawning the `git` binary) and by a fake in tests.
pub trait GitRunner {
    /// Run `git -C <repo> <args>` and capture its output.
    ///
    /// # Errors
    ///
    /// Returns an error only when the `git` process could not be spawned; a
    /// non-zero git exit is reported through [`GitOutput::success`] (`false`),
    /// not as an `Err`.
    fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOutput>;
}
