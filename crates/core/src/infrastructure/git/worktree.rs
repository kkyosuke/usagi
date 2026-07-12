//! The worktree lifecycle: add, remove, and list a repository's worktrees.
//!
//! A session's parallel working tree is a git worktree on its own branch. These
//! build and tear that down. All operations go through the injected
//! [`GitRunner`], so the branching on git's stderr (an already-removed worktree,
//! a failed add) is exercised in unit tests without a real repository.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::runner::GitRunner;

/// One entry of `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// The checked-out commit, if reported.
    pub head: Option<String>,
    /// The checked-out branch (with `refs/heads/` stripped), or `None` for a
    /// detached HEAD.
    pub branch: Option<String>,
}

/// Create a worktree at `dest` on a new branch `branch`, optionally based on
/// `base` (a ref to branch from; git's default when `None`).
///
/// # Errors
///
/// Returns an error when the path is not valid UTF-8, the `git` process cannot be
/// spawned, or `git worktree add` exits non-zero.
pub fn add_worktree(
    runner: &dyn GitRunner,
    repo: &Path,
    dest: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<()> {
    let dest = dest.to_str().context("worktree path is not valid UTF-8")?;
    // `--` keeps a leading-`-` path or base from being parsed as an option.
    let mut args = vec!["worktree", "add", "-b", branch, "--", dest];
    if let Some(base) = base {
        args.push(base);
    }
    let output = runner.run(repo, &args)?;
    if !output.success {
        bail!("git worktree add failed: {}", output.stderr.trim());
    }
    Ok(())
}

/// Remove the worktree at `worktree` (with `--force` when `force`).
///
/// A path git does not recognise as a worktree is already in the desired end
/// state — a session whose worktree was never built, or a repeated removal — so
/// it is treated as a no-op rather than an error, letting callers finish cleaning
/// up the rest of a session.
///
/// # Errors
///
/// Returns an error when the path is not valid UTF-8, the `git` process cannot be
/// spawned, or `git worktree remove` fails for any reason other than the path not
/// being a worktree.
pub fn remove_worktree(
    runner: &dyn GitRunner,
    repo: &Path,
    worktree: &Path,
    force: bool,
) -> Result<()> {
    let path = worktree
        .to_str()
        .context("worktree path is not valid UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.extend(["--", path]);
    let output = runner.run(repo, &args)?;
    if output.success || output.stderr.contains("is not a working tree") {
        return Ok(());
    }
    bail!("git worktree remove failed: {}", output.stderr.trim());
}

/// List the repository's worktrees.
///
/// # Errors
///
/// Returns an error when the `git` process cannot be spawned or
/// `git worktree list` exits non-zero.
pub fn list_worktrees(runner: &dyn GitRunner, repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let output = runner.run(repo, &["worktree", "list", "--porcelain"])?;
    if !output.success {
        bail!("git worktree list failed: {}", output.stderr.trim());
    }
    Ok(parse_porcelain(&output.stdout))
}

/// Parse the `git worktree list --porcelain` output: a blank-line-separated block
/// per worktree, each with a `worktree <path>` line and optional `HEAD <sha>` /
/// `branch <ref>` (absent when the worktree is on a detached HEAD).
fn parse_porcelain(text: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                head: None,
                branch: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ")
            && let Some(wt) = current.as_mut()
        {
            wt.head = Some(head.to_owned());
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(wt) = current.as_mut()
        {
            wt.branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_owned(),
            );
        }
    }
    if let Some(done) = current.take() {
        out.push(done);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{WorktreeInfo, add_worktree, list_worktrees, remove_worktree};
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use std::path::{Path, PathBuf};

    #[test]
    fn add_worktree_builds_the_expected_command_with_a_base() {
        let git = FakeGit::new(vec![ok("")]);
        add_worktree(
            &git,
            Path::new("/repo"),
            Path::new("/repo/.usagi/sessions/x"),
            "usagi/x",
            Some("main"),
        )
        .unwrap();
        assert_eq!(
            git.calls.borrow()[0],
            vec![
                "worktree",
                "add",
                "-b",
                "usagi/x",
                "--",
                "/repo/.usagi/sessions/x",
                "main"
            ]
        );
    }

    #[test]
    fn add_worktree_omits_the_base_when_none_and_reports_failure() {
        let git = FakeGit::new(vec![ok("")]);
        add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None).unwrap();
        assert_eq!(
            git.calls.borrow()[0],
            vec!["worktree", "add", "-b", "b", "--", "/dest"]
        );

        let bad = FakeGit::new(vec![fail("fatal: branch 'b' already exists")]);
        let err = add_worktree(&bad, Path::new("/repo"), Path::new("/dest"), "b", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("git worktree add failed"));
        assert!(err.contains("already exists"));
    }

    #[test]
    fn remove_worktree_passes_force_and_succeeds() {
        let git = FakeGit::new(vec![ok("")]);
        remove_worktree(&git, Path::new("/repo"), Path::new("/dest"), true).unwrap();
        assert_eq!(
            git.calls.borrow()[0],
            vec!["worktree", "remove", "--force", "--", "/dest"]
        );
    }

    #[test]
    fn remove_worktree_treats_a_missing_worktree_as_a_noop() {
        let git = FakeGit::new(vec![fail("fatal: '/dest' is not a working tree")]);
        // No `--force` when false, and the "not a working tree" error is swallowed.
        remove_worktree(&git, Path::new("/repo"), Path::new("/dest"), false).unwrap();
        assert_eq!(
            git.calls.borrow()[0],
            vec!["worktree", "remove", "--", "/dest"]
        );
    }

    #[test]
    fn remove_worktree_surfaces_other_failures() {
        let git = FakeGit::new(vec![fail(
            "fatal: '/dest' contains modified or untracked files",
        )]);
        let err = remove_worktree(&git, Path::new("/repo"), Path::new("/dest"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("git worktree remove failed"));
        assert!(err.contains("modified or untracked"));
    }

    #[test]
    fn list_worktrees_parses_porcelain_including_a_detached_head() {
        let porcelain = "\
worktree /repo
HEAD abc123
branch refs/heads/main

worktree /repo/.usagi/sessions/x
HEAD def456
branch refs/heads/usagi/x

worktree /repo/detached
HEAD 999aaa
detached
";
        let git = FakeGit::new(vec![ok(porcelain)]);
        let list = list_worktrees(&git, Path::new("/repo")).unwrap();
        assert_eq!(
            list,
            vec![
                WorktreeInfo {
                    path: PathBuf::from("/repo"),
                    head: Some("abc123".to_string()),
                    branch: Some("main".to_string()),
                },
                WorktreeInfo {
                    path: PathBuf::from("/repo/.usagi/sessions/x"),
                    head: Some("def456".to_string()),
                    branch: Some("usagi/x".to_string()),
                },
                WorktreeInfo {
                    path: PathBuf::from("/repo/detached"),
                    head: Some("999aaa".to_string()),
                    branch: None,
                },
            ]
        );
    }

    #[test]
    fn list_worktrees_reports_failure() {
        let git = FakeGit::new(vec![fail("fatal: not a git repository")]);
        assert!(list_worktrees(&git, Path::new("/repo")).is_err());
    }
}
