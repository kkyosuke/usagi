//! Cloning a repository into a fresh working directory.
//!
//! `usagi` starts a brand-new project by cloning a repository into a child
//! directory of a chosen location. Like the rest of this module the operation
//! goes through the injected [`GitRunner`], so the branching on git's exit
//! status is exercised in unit tests without a real network or repository.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::runner::GitRunner;

/// Clone `repository` into a new `directory` under `parent`, optionally checking
/// out `branch` instead of the repository's default.
///
/// Returns the created working directory (`parent/directory`).
///
/// # Errors
///
/// Returns an error when `directory` is not valid UTF-8, the `git` process
/// cannot be spawned, or `git clone` exits non-zero (its stderr is surfaced).
pub fn clone(
    runner: &dyn GitRunner,
    parent: &Path,
    repository: &str,
    directory: &str,
    branch: Option<&str>,
) -> Result<PathBuf> {
    let mut args = vec!["clone"];
    if let Some(branch) = branch {
        args.push("--branch");
        args.push(branch);
    }
    // `--` keeps a leading-`-` repository or directory from being read as an option.
    args.extend(["--", repository, directory]);
    let output = runner.run(parent, &args)?;
    if !output.success {
        bail!("git clone failed: {}", output.stderr.trim());
    }
    Ok(parent.join(directory))
}

#[cfg(test)]
mod tests {
    use super::clone;
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use std::path::PathBuf;

    #[test]
    fn clone_passes_repository_and_directory_and_returns_the_destination() {
        let git = FakeGit::new(vec![ok("")]);
        let destination = clone(
            &git,
            &PathBuf::from("/projects"),
            "https://example.com/owner/repo.git",
            "repo",
            None,
        )
        .unwrap();
        assert_eq!(destination, PathBuf::from("/projects/repo"));
        assert_eq!(
            git.calls.borrow()[0],
            vec!["clone", "--", "https://example.com/owner/repo.git", "repo"]
        );
    }

    #[test]
    fn clone_forwards_the_requested_branch() {
        let git = FakeGit::new(vec![ok("")]);
        clone(
            &git,
            &PathBuf::from("/projects"),
            "git@example.com:owner/repo.git",
            "repo",
            Some("develop"),
        )
        .unwrap();
        assert_eq!(
            git.calls.borrow()[0],
            vec![
                "clone",
                "--branch",
                "develop",
                "--",
                "git@example.com:owner/repo.git",
                "repo",
            ]
        );
    }

    #[test]
    fn clone_reports_gits_stderr_on_failure() {
        let git = FakeGit::new(vec![fail("fatal: repository not found")]);
        let error = clone(
            &git,
            &PathBuf::from("/projects"),
            "https://example.com/missing.git",
            "missing",
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("repository not found"));
    }
}
