//! Create a session: a parallel working tree under `.usagi/worktree/<name>/`.
//!
//! The workspace root need not itself be a git repository. The root is walked
//! recursively: every git repository found gets a fresh `git worktree` (on a new
//! branch named after the session) at its mirrored location under
//! `.usagi/worktree/<name>/`, while non-git files and directories are copied
//! there. This supports a single repository, or a tree containing several — e.g.
//!
//! ```text
//! /root            (not a repo)
//! ├── app-a/  =git → worktree
//! ├── be/          (plain dir → recurse)
//! │   └── be1/=git → worktree
//! └── README.md   → copied
//! ```

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::domain::workspace_state::{SessionRecord, WorkspaceState};
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Names never descended into or copied while building a session: usagi's own
/// data directory (which holds the session tree itself) and any `.git`.
const SKIP: &[&str] = &[".git", ".usagi"];

/// The outcome of creating a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    /// The session name (also the new branch name in every repository).
    pub name: String,
    /// Root of the session tree: `<workspace>/.usagi/worktree/<name>`.
    pub root: PathBuf,
    /// The mirrored path of every repository that received a new worktree.
    pub worktrees: Vec<PathBuf>,
}

/// Create session `name` under `workspace_root`.
///
/// Fails if the name is empty or contains path separators, or if the session
/// already exists. Any git error (e.g. the branch already exists in a repo) is
/// surfaced.
pub fn create(workspace_root: &Path, name: &str) -> Result<CreatedSession> {
    let name = name.trim();
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("session name must not contain path separators");
    }

    let dest_root = workspace_root.join(".usagi").join("worktree").join(name);
    if dest_root.exists() {
        bail!("session \"{name}\" already exists");
    }

    let mut worktrees = Vec::new();
    if is_repo_root(workspace_root) {
        // The whole workspace is one repository: a single worktree at the root.
        let parent = dest_root
            .parent()
            .expect("dest_root always has a .usagi/worktree parent");
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        git::add_worktree(workspace_root, &dest_root, name)?;
        worktrees.push(dest_root.clone());
    } else {
        fs::create_dir_all(&dest_root)
            .context(format!("failed to create {}", dest_root.display()))?;
        build_dir(workspace_root, &dest_root, name, &mut worktrees)?;
    }

    record(workspace_root, name, &dest_root, &worktrees)?;

    Ok(CreatedSession {
        name: name.to_string(),
        root: dest_root,
        worktrees,
    })
}

/// Append the session to `<workspace>/.usagi/state.json`, creating the state
/// (with a git-derived default branch) when none exists yet. This is what lets
/// a multi-repo, non-git root still track its sessions.
fn record(workspace_root: &Path, name: &str, root: &Path, worktrees: &[PathBuf]) -> Result<()> {
    let store = WorkspaceStore::new(workspace_root);
    let mut state = store
        .load()?
        .unwrap_or_else(|| WorkspaceState::new(git::default_branch(workspace_root), Vec::new()));

    let now = Utc::now();
    state.sessions.push(SessionRecord {
        name: name.to_string(),
        root: root.to_path_buf(),
        worktrees: worktrees.to_vec(),
        created_at: now,
    });
    state.updated_at = now;
    store.save(&state)
}

/// Recursively mirror `src` into the already-created `dest`: a git repository
/// becomes a new worktree, a plain directory is recreated and descended into,
/// and a plain file is copied.
fn build_dir(src: &Path, dest: &Path, branch: &str, worktrees: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(src)
        .context(format!("failed to read {}", src.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .context(format!("failed to read {}", src.display()))?;
    // A stable order keeps the created tree (and tests) deterministic.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        if SKIP.iter().any(|s| OsStr::new(s) == name) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let file_type = entry
            .file_type()
            .context(format!("failed to inspect {}", from.display()))?;

        if file_type.is_dir() {
            if is_repo_root(&from) {
                git::add_worktree(&from, &to, branch)?;
                worktrees.push(to);
            } else {
                fs::create_dir_all(&to).context(format!("failed to create {}", to.display()))?;
                build_dir(&from, &to, branch, worktrees)?;
            }
        } else {
            fs::copy(&from, &to).context(format!("failed to copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// A directory is a repository root when it directly contains a `.git` entry —
/// a directory for a normal clone, or a file for a linked worktree.
fn is_repo_root(path: &Path) -> bool {
    path.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;

    /// Initialise a throwaway git repo with one commit on `main`.
    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                git_cmd(dir).args(args).status().unwrap().success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(dir.join("code.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// The branch checked out in the worktree at `dir`.
    fn head_branch(dir: &Path) -> String {
        let out = git_cmd(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn rejects_an_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = create(dir.path(), "   ").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_a_name_with_path_separators() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["a/b", "a\\b", ".", ".."] {
            let err = create(dir.path(), bad).unwrap_err();
            assert!(err.to_string().contains("must not contain path separators"));
        }
    }

    #[test]
    fn single_repo_root_gets_one_worktree() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let created = create(root.path(), "feature-x").unwrap();

        let wt = root.path().join(".usagi/worktree/feature-x");
        assert_eq!(created.root, wt);
        assert_eq!(created.worktrees, vec![wt.clone()]);
        // The new worktree is on the session branch and carries the repo files.
        assert_eq!(head_branch(&wt), "feature-x");
        assert!(wt.join("code.txt").is_file());
        // The session is recorded in state.json.
        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].name, "feature-x");
        assert_eq!(state.sessions[0].root, wt);
    }

    #[test]
    fn non_git_root_recurses_over_repos_and_copies_files() {
        let root = tempfile::tempdir().unwrap();
        // Two top-level repos, a plain nested dir holding a third repo, and a
        // loose file at the root — mirroring the multi-repo example.
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("app-b"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        // A pre-existing .usagi dir must be skipped, not copied into the session.
        fs::create_dir_all(root.path().join(".usagi")).unwrap();
        fs::write(root.path().join(".usagi/marker"), "x").unwrap();

        let created = create(root.path(), "wip").unwrap();

        let base = root.path().join(".usagi/worktree/wip");
        // Every repository became a worktree on the session branch.
        for repo in ["app-a", "app-b", "be/be1"] {
            let wt = base.join(repo);
            assert!(wt.is_dir(), "{repo} worktree missing");
            assert_eq!(head_branch(&wt), "wip");
            assert!(created.worktrees.contains(&wt));
        }
        assert_eq!(created.worktrees.len(), 3);
        // The loose file was copied; usagi's own data dir was not.
        assert_eq!(fs::read_to_string(base.join("README.md")).unwrap(), "hi");
        assert!(!base.join(".usagi").exists());
        // The session is recorded even though the root is not a git repository.
        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].worktrees.len(), 3);
    }

    #[test]
    fn records_multiple_sessions_in_order() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        create(root.path(), "first").unwrap();
        // The second create loads the existing state and appends to it.
        create(root.path(), "second").unwrap();

        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        let names: Vec<&str> = state.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn rejects_a_duplicate_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "dup").unwrap();

        let err = create(root.path(), "dup").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn surfaces_a_git_error_when_the_branch_exists() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // Pre-create the branch so `git worktree add -b` fails.
        assert!(git_cmd(root.path())
            .args(["branch", "taken"])
            .status()
            .unwrap()
            .success());

        let err = create(root.path(), "taken").unwrap_err();
        assert!(err.to_string().contains("git worktree add failed"));
    }

    #[test]
    fn fails_when_the_session_directory_cannot_be_created() {
        let root = tempfile::tempdir().unwrap();
        // A non-repo root whose `.usagi` is a *file* makes create_dir_all fail.
        fs::write(root.path().join(".usagi"), "not a dir").unwrap();

        let err = create(root.path(), "x").unwrap_err();
        assert!(err.to_string().contains("failed to create"));
    }
}
