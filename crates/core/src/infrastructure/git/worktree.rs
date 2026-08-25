//! The worktree lifecycle: add, remove, and list a repository's worktrees.
//!
//! A session's parallel working tree is a git worktree on its own branch. These
//! build and tear that down. All operations go through the injected
//! [`GitRunner`], so the branching on git's stderr (an already-removed worktree,
//! a failed add) is exercised in unit tests without a real repository.

use std::collections::BTreeSet;
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
    match std::fs::symlink_metadata(dest) {
        Ok(_) => bail!("git worktree destination is already occupied"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let revision = base.unwrap_or("HEAD");
    let commit_expression = format!("{revision}^{{commit}}");
    let resolved = runner.run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &commit_expression,
        ],
    )?;
    if !resolved.success {
        bail!(
            "git worktree base resolution failed: {}",
            resolved.stderr.trim()
        );
    }
    let commit = resolved.stdout.trim();
    if commit.is_empty()
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        || commit.len() < 40
    {
        bail!("git worktree base resolution returned an invalid object id");
    }
    reject_checkout_filters(runner, repo, commit)?;

    // Branch creation is an independent atomic effect. Success proves that this
    // invocation owns the branch, and the exact inspected commit prevents a
    // mutable base ref from changing between policy validation and checkout.
    let output = runner.run(repo, &["branch", "--", branch, commit])?;
    if !output.success {
        bail!(
            "git worktree branch creation failed: {}",
            output.stderr.trim()
        );
    }

    // Create only worktree metadata. Materialisation is deliberately separate,
    // so no checkout filter can run before its effective driver is disabled.
    let output = match runner.run(
        repo,
        &["worktree", "add", "--no-checkout", "--", dest, branch],
    ) {
        Ok(output) => output,
        Err(error) => {
            return Err(compensate_failed_add(
                runner,
                repo,
                Path::new(dest),
                branch,
                commit,
                false,
                &error.to_string(),
            ));
        }
    };
    if !output.success {
        return Err(compensate_failed_add(
            runner,
            repo,
            Path::new(dest),
            branch,
            commit,
            false,
            output.stderr.trim(),
        ));
    }

    materialize_worktree(runner, repo, Path::new(dest), branch, commit)
}

fn materialize_worktree(
    runner: &dyn GitRunner,
    repo: &Path,
    destination: &Path,
    branch: &str,
    commit: &str,
) -> Result<()> {
    let drivers = match configured_filter_drivers(runner, destination) {
        Ok(drivers) => drivers,
        Err(error) => {
            return Err(compensate_failed_add(
                runner,
                repo,
                destination,
                branch,
                commit,
                true,
                &error.to_string(),
            ));
        }
    };
    let mut checkout_args = Vec::with_capacity(drivers.len() * 6 + 4);
    for driver in drivers {
        checkout_args.extend([
            "-c".to_owned(),
            format!("filter.{driver}.smudge="),
            "-c".to_owned(),
            format!("filter.{driver}.process="),
            "-c".to_owned(),
            format!("filter.{driver}.required=false"),
        ]);
    }
    checkout_args.extend(
        ["read-tree", "--reset", "-u", "HEAD"]
            .into_iter()
            .map(str::to_owned),
    );
    let checkout_refs = checkout_args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = match runner.run(destination, &checkout_refs) {
        Ok(output) => output,
        Err(error) => {
            return Err(compensate_failed_add(
                runner,
                repo,
                destination,
                branch,
                commit,
                true,
                &error.to_string(),
            ));
        }
    };
    if !output.success {
        return Err(compensate_failed_add(
            runner,
            repo,
            destination,
            branch,
            commit,
            true,
            output.stderr.trim(),
        ));
    }
    Ok(())
}

/// Driver names present in the new worktree's complete effective Git config.
fn configured_filter_drivers(runner: &dyn GitRunner, repo: &Path) -> Result<BTreeSet<String>> {
    let output = runner.run(repo, &["config", "--null", "--name-only", "--list"])?;
    if !output.success {
        bail!(
            "could not inspect checkout filter policy: {}",
            output.stderr.trim()
        );
    }
    Ok(output
        .stdout
        .split('\0')
        .filter_map(|key| {
            let (section, key) = key.split_once('.')?;
            if !section.eq_ignore_ascii_case("filter") {
                return None;
            }
            let (driver, _) = key.rsplit_once('.')?;
            (!driver.is_empty()).then(|| driver.to_owned())
        })
        .collect())
}

/// Roll back only the branch and registered worktree this invocation created.
fn compensate_failed_add(
    runner: &dyn GitRunner,
    repo: &Path,
    destination: &Path,
    branch: &str,
    commit: &str,
    registration_succeeded: bool,
    failure: &str,
) -> anyhow::Error {
    let mut cleanup = Vec::new();
    let registered = registration_succeeded
        || match list_worktrees(runner, repo) {
            Ok(worktrees) => worktrees.iter().any(|worktree| {
                worktree.path == destination
                    && worktree.branch.as_deref() == Some(branch)
                    && worktree.head.as_deref() == Some(commit)
            }),
            Err(error) => {
                cleanup.push(error.to_string());
                false
            }
        };
    if registered && let Err(error) = remove_worktree(runner, repo, destination, true) {
        cleanup.push(error.to_string());
    }
    if let Err(error) = delete_branch(runner, repo, branch, true) {
        cleanup.push(error.to_string());
    }
    let cleanup = if cleanup.is_empty() {
        String::new()
    } else {
        format!("; compensation failed: {}", cleanup.join("; "))
    };
    anyhow::anyhow!("git worktree add failed: {failure}{cleanup}")
}

fn reject_checkout_filters(runner: &dyn GitRunner, repo: &Path, commit: &str) -> Result<()> {
    let tree = runner.run(repo, &["ls-tree", "-rz", "--name-only", commit])?;
    if !tree.success {
        bail!("git worktree attribute scan failed: {}", tree.stderr.trim());
    }
    for path in tree.stdout.split('\0').filter(|path| {
        Path::new(path)
            .file_name()
            .is_some_and(|name| name == ".gitattributes")
    }) {
        let object = format!("{commit}:{path}");
        let attributes = runner.run(repo, &["cat-file", "blob", &object])?;
        if !attributes.success {
            bail!(
                "git worktree attribute scan failed: {}",
                attributes.stderr.trim()
            );
        }
        if attributes.stdout.lines().any(line_enables_checkout_filter) {
            bail!("git worktree checkout refused: tracked {path} configures an executable filter");
        }
    }
    Ok(())
}

fn line_enables_checkout_filter(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let Some(pattern) = fields.next() else {
        return false;
    };
    if pattern.starts_with('#') {
        return false;
    }
    fields.any(|attribute| {
        attribute == "filter"
            || attribute.starts_with("filter=")
            || attribute == "-filter"
            || attribute == "!filter"
    })
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

/// Delete the local branch `branch`.
///
/// A branch git does not know is already in the desired end state — a create
/// whose worktree add failed before branching, or a repeated deletion — so it is
/// treated as a no-op. When `force` is false Git refuses to delete a branch with
/// unmerged commits; compensating teardown passes true only for a branch that
/// never became user-owned work.
///
/// # Errors
///
/// Returns an error when the `git` process cannot be spawned, or `git branch -d`
/// / `git branch -D` fails for any reason other than the branch not existing.
pub fn delete_branch(runner: &dyn GitRunner, repo: &Path, branch: &str, force: bool) -> Result<()> {
    let delete_flag = if force { "-D" } else { "-d" };
    let output = runner.run(repo, &["branch", delete_flag, "--", branch])?;
    if output.success || output.stderr.contains("not found") {
        return Ok(());
    }
    bail!("git branch delete failed: {}", output.stderr.trim());
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
    use super::{
        WorktreeInfo, add_worktree, delete_branch, line_enables_checkout_filter, list_worktrees,
        remove_worktree,
    };
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use std::path::{Path, PathBuf};

    #[test]
    fn add_worktree_builds_the_expected_command_with_a_base() {
        let commit = "a".repeat(40);
        let git = FakeGit::new(vec![ok(&commit), ok(""), ok(""), ok(""), ok(""), ok("")]);
        add_worktree(
            &git,
            Path::new("/repo"),
            Path::new("/repo/.usagi/sessions/x"),
            "usagi/x",
            Some("main"),
        )
        .unwrap();
        assert_eq!(
            git.calls.borrow().as_slice(),
            &[
                vec!["rev-parse", "--verify", "--end-of-options", "main^{commit}"],
                vec!["ls-tree", "-rz", "--name-only", commit.as_str()],
                vec!["branch", "--", "usagi/x", commit.as_str()],
                vec![
                    "worktree",
                    "add",
                    "--no-checkout",
                    "--",
                    "/repo/.usagi/sessions/x",
                    "usagi/x"
                ],
                vec!["config", "--null", "--name-only", "--list"],
                vec!["read-tree", "--reset", "-u", "HEAD"]
            ]
        );
    }

    #[test]
    fn add_worktree_omits_the_base_when_none_and_reports_failure() {
        let commit = "b".repeat(40);
        let git = FakeGit::new(vec![ok(&commit), ok(""), ok(""), ok(""), ok(""), ok("")]);
        add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None).unwrap();
        assert_eq!(
            git.calls.borrow()[2],
            vec!["branch", "--", "b", commit.as_str()]
        );

        let failed_commit = "c".repeat(40);
        let bad = FakeGit::new(vec![
            ok(&failed_commit),
            ok(""),
            fail("branch already exists"),
        ]);
        let err = add_worktree(&bad, Path::new("/repo"), Path::new("/dest"), "b", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("branch creation failed"));
        assert!(err.contains("already exists"));
    }

    #[test]
    fn add_worktree_compensates_only_an_exact_partial_registration() {
        let commit = "e".repeat(40);
        let listing = format!("worktree /dest\nHEAD {commit}\nbranch refs/heads/b\n\n");
        let git = FakeGit::new(vec![
            ok(&commit),
            ok(""),
            ok(""),
            fail("checkout failed"),
            ok(&listing),
            ok(""),
            ok(""),
        ]);
        let error = add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("checkout failed"));
        assert_eq!(
            git.calls.borrow()[5],
            ["worktree", "remove", "--force", "--", "/dest"]
        );
        assert_eq!(git.calls.borrow()[6], ["branch", "-D", "--", "b"]);
    }

    #[test]
    fn materialization_disables_every_effective_filter_driver() {
        let commit = "f".repeat(40);
        let git = FakeGit::new(vec![
            ok(&commit),
            ok(""),
            ok(""),
            ok(""),
            ok(
                "Filter.pwn.smudge\0filter.pwn.process\0filter.lfs.clean\0filter.invalid\0filter..smudge\0",
            ),
            ok(""),
        ]);

        add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None).unwrap();

        assert_eq!(
            git.calls.borrow()[5],
            vec![
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.required=false",
                "-c",
                "filter.pwn.smudge=",
                "-c",
                "filter.pwn.process=",
                "-c",
                "filter.pwn.required=false",
                "read-tree",
                "--reset",
                "-u",
                "HEAD"
            ]
        );
    }

    #[test]
    fn filter_discovery_and_materialization_failures_are_compensated() {
        for (failure_at, expected) in [
            (4, "could not inspect checkout filter policy"),
            (5, "materialization failed"),
        ] {
            let commit = "a".repeat(40);
            let mut outputs = vec![ok(&commit), ok(""), ok(""), ok(""), ok(""), ok("")];
            outputs[failure_at] = fail(if failure_at == 4 {
                "config is invalid"
            } else {
                "materialization failed"
            });
            outputs.extend([ok(""), ok("")]);
            let git = FakeGit::new(outputs);

            let error = add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None)
                .unwrap_err()
                .to_string();

            assert!(error.contains(expected), "{error}");
            let calls = git.calls.borrow();
            assert_eq!(
                calls[calls.len() - 2],
                ["worktree", "remove", "--force", "--", "/dest"]
            );
            assert_eq!(calls[calls.len() - 1], ["branch", "-D", "--", "b"]);
        }
    }

    #[test]
    fn add_worktree_refuses_tracked_checkout_filters_before_materializing() {
        let commit = "d".repeat(40);
        let git = FakeGit::new(vec![
            ok(&commit),
            ok(".gitattributes\0src/.gitattributes\0src/lib.rs\0"),
            ok("*.md text\n"),
            ok("*.bin filter=owned\n"),
        ]);
        let error = add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("executable filter"));
        assert_eq!(
            git.calls.borrow().len(),
            4,
            "checkout must not have started"
        );
    }

    #[test]
    fn checkout_filter_attribute_detection_is_fail_closed() {
        assert!(line_enables_checkout_filter("*.bin filter=evil"));
        assert!(line_enables_checkout_filter("*.bin -filter"));
        assert!(line_enables_checkout_filter("[attr]binary filter"));
        assert!(!line_enables_checkout_filter("# *.bin filter=ignored"));
        assert!(!line_enables_checkout_filter("*.bin diff=custom"));
        assert!(!line_enables_checkout_filter(""));
    }

    #[test]
    fn add_worktree_refuses_every_untrusted_pre_checkout_state() {
        let occupied_root = tempfile::tempdir().unwrap();
        let occupied = occupied_root.path().join("occupied");
        std::fs::write(&occupied, "owned").unwrap();
        assert!(
            add_worktree(
                &FakeGit::new(vec![]),
                Path::new("/repo"),
                &occupied,
                "b",
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("occupied")
        );

        for (git, expected) in [
            (
                FakeGit::new(vec![fail("unknown revision")]),
                "base resolution failed",
            ),
            (FakeGit::new(vec![ok("not-an-object")]), "invalid object id"),
            (
                FakeGit::new(vec![ok(&"a".repeat(40)), fail("tree unreadable")]),
                "attribute scan failed",
            ),
            (
                FakeGit::new(vec![
                    ok(&"b".repeat(40)),
                    ok(".gitattributes\0"),
                    fail("blob unreadable"),
                ]),
                "attribute scan failed",
            ),
        ] {
            let error = add_worktree(&git, Path::new("/repo"), Path::new("/dest"), "b", None)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
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
    fn delete_branch_forces_the_deletion_and_swallows_an_unknown_branch() {
        let git = FakeGit::new(vec![ok("Deleted branch usagi/x")]);
        delete_branch(&git, Path::new("/repo"), "usagi/x", true).unwrap();
        assert_eq!(git.calls.borrow()[0], vec!["branch", "-D", "--", "usagi/x"]);

        let missing = FakeGit::new(vec![fail("error: branch 'usagi/x' not found.")]);
        delete_branch(&missing, Path::new("/repo"), "usagi/x", true).unwrap();
    }

    #[test]
    fn delete_branch_uses_safe_delete_and_surfaces_unmerged_work() {
        let git = FakeGit::new(vec![fail(
            "error: the branch 'usagi/x' is not fully merged",
        )]);
        let err = delete_branch(&git, Path::new("/repo"), "usagi/x", false)
            .unwrap_err()
            .to_string();
        assert_eq!(git.calls.borrow()[0], vec!["branch", "-d", "--", "usagi/x"]);
        assert!(err.contains("git branch delete failed"));
        assert!(err.contains("not fully merged"));
    }

    #[test]
    fn delete_branch_surfaces_other_failures() {
        let git = FakeGit::new(vec![fail(
            "error: cannot delete branch 'usagi/x' used by worktree",
        )]);
        let err = delete_branch(&git, Path::new("/repo"), "usagi/x", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("git branch delete failed"));
        assert!(err.contains("used by worktree"));
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
