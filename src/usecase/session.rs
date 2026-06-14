//! Creating and listing sessions.
//!
//! `session new <name>` reproduces the workspace root under
//! `.usagi/worktree/<name>/`: the root is walked recursively, every git
//! repository becomes a `git worktree` on the new branch `<name>`, and every
//! other file or directory is copied. The resulting [`Session`] is persisted in
//! the workspace's `state.json` so it survives restarts and a later `usagi
//! status` sync (see [`crate::usecase::workspace_state::sync`]).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::domain::session::{Session, SessionRepo};
use crate::domain::workspace_state::WorkspaceState;
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Directory, relative to the workspace root, under which session worktrees are
/// collected. It is `.gitignore`d, so the worktrees never pollute commits.
const WORKTREE_SUBDIR: &str = ".usagi/worktree";

/// Entries never descended into or copied: git's own metadata and usagi's data
/// directory (which holds the session worktrees themselves).
const SKIP: &[&str] = &[".git", ".usagi"];

/// Create a session named `name` under `workspace_root`, building a worktree for
/// every nested git repository and copying everything else.
///
/// Fails if the name is invalid or a session with that name already exists.
pub fn create(workspace_root: &Path, name: &str) -> Result<Session> {
    validate_name(name)?;

    let session_root = workspace_root.join(WORKTREE_SUBDIR).join(name);
    if session_root.exists() {
        bail!("session '{name}' already exists");
    }

    let mut repos = Vec::new();
    if git::is_repository_root(workspace_root) {
        // The whole workspace is a single repository: one worktree for the root.
        create_parent(&session_root)?;
        git::add_worktree(workspace_root, &session_root, name)?;
        repos.push(SessionRepo {
            relative: PathBuf::new(),
            path: session_root.clone(),
            branch: name.to_string(),
        });
    } else {
        // A plain (possibly multi-repo) root: walk it and reproduce its tree.
        fs::create_dir_all(&session_root)
            .with_context(|| format!("failed to create {}", session_root.display()))?;
        walk(
            workspace_root,
            workspace_root,
            &session_root,
            name,
            &mut repos,
        )?;
    }

    let session = Session::new(name, &session_root, repos);
    persist(workspace_root, &session)?;
    Ok(session)
}

/// List the sessions recorded for `workspace_root` (empty if none / no state).
pub fn list(workspace_root: &Path) -> Result<Vec<Session>> {
    Ok(WorkspaceStore::new(workspace_root)
        .load()?
        .map(|state| state.sessions)
        .unwrap_or_default())
}

/// Reject names that are empty or would escape the worktree directory.
fn validate_name(name: &str) -> Result<()> {
    let invalid =
        name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']);
    if invalid {
        bail!("invalid session name '{name}'");
    }
    Ok(())
}

/// Recursively reproduce the directory `dir` (absolute) into the session.
///
/// `workspace_root` anchors relative-path computation and `session_root` is the
/// destination tree. Git repository roots become worktrees (and are not
/// descended into); plain directories are recreated and recursed; files are
/// copied. Entries are visited in sorted order for deterministic output.
fn walk(
    dir: &Path,
    workspace_root: &Path,
    session_root: &Path,
    branch: &str,
    repos: &mut Vec<SessionRepo>,
) -> Result<()> {
    // Inner IO errors (read / create / copy) are surfaced as-is rather than
    // wrapped: they are not reachable from a well-formed workspace, so adding
    // per-call context here would only be dead, untested code.
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();

    for src in entries {
        if src
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| SKIP.contains(&n))
        {
            continue;
        }

        let rel = src
            .strip_prefix(workspace_root)
            .expect("entry is under the workspace root")
            .to_path_buf();
        let dest = session_root.join(&rel);

        if git::is_repository_root(&src) {
            create_parent(&dest)?;
            git::add_worktree(&src, &dest, branch)?;
            repos.push(SessionRepo {
                relative: rel,
                path: dest,
                branch: branch.to_string(),
            });
        } else if src.is_dir() {
            fs::create_dir_all(&dest)?;
            walk(&src, workspace_root, session_root, branch, repos)?;
        } else {
            create_parent(&dest)?;
            fs::copy(&src, &dest)?;
        }
    }
    Ok(())
}

/// Ensure the parent directory of `path` exists.
fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

/// Append `session` to the workspace's persisted state, creating the state file
/// if this is the first session.
fn persist(workspace_root: &Path, session: &Session) -> Result<()> {
    let store = WorkspaceStore::new(workspace_root);
    let mut state = store.load()?.unwrap_or_else(|| {
        let default_branch = if git::is_repository(workspace_root) {
            git::default_branch(workspace_root)
        } else {
            String::new()
        };
        WorkspaceState::new(default_branch, Vec::new())
    });
    state.sessions.push(session.clone());
    store.save(&state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;

    /// Initialise a throwaway git repo with one commit on `main`.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(
                git_cmd(dir).args(args).status().unwrap().success(),
                "git {args:?} failed"
            );
        };
        fs::create_dir_all(dir).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(dir.join("file.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// The branch checked out in a worktree directory.
    fn branch_of(dir: &Path) -> String {
        let out = git_cmd(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn create_on_a_single_repo_root_makes_one_worktree() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let session = create(root.path(), "feature-x").unwrap();

        assert_eq!(session.name, "feature-x");
        assert_eq!(session.repos.len(), 1);
        let repo = &session.repos[0];
        assert_eq!(repo.relative, PathBuf::new());
        assert_eq!(repo.branch, "feature-x");
        assert!(repo.path.join("file.txt").is_file());
        assert_eq!(branch_of(&repo.path), "feature-x");

        // The session is persisted and listable.
        let listed = list(root.path()).unwrap();
        assert_eq!(listed, vec![session]);
    }

    #[test]
    fn create_walks_a_non_git_root_recursively() {
        let root = tempfile::tempdir().unwrap();
        // Two top-level repos, a nested repo under a plain dir, and a loose file.
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("app-b"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        // A plain (non-repo) directory with a file is copied wholesale.
        fs::create_dir_all(root.path().join("docs")).unwrap();
        fs::write(root.path().join("docs/guide.md"), "g").unwrap();

        let session = create(root.path(), "feat").unwrap();

        let mut rels: Vec<_> = session
            .repos
            .iter()
            .map(|r| r.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        rels.sort();
        assert_eq!(rels, vec!["app-a", "app-b", "be/be1"]);

        let session_root = root.path().join(".usagi/worktree/feat");
        // Worktrees exist on the new branch.
        assert_eq!(branch_of(&session_root.join("app-a")), "feat");
        assert_eq!(branch_of(&session_root.join("be/be1")), "feat");
        // Loose files and plain directories are copied, not worktreed.
        assert_eq!(
            fs::read_to_string(session_root.join("README.md")).unwrap(),
            "hi"
        );
        assert_eq!(
            fs::read_to_string(session_root.join("docs/guide.md")).unwrap(),
            "g"
        );
    }

    #[test]
    fn create_rejects_a_duplicate_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "dup").unwrap();
        let err = create(root.path(), "dup").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn create_rejects_invalid_names() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            let err = create(root.path(), bad).unwrap_err();
            assert!(err.to_string().contains("invalid session name"));
        }
    }

    #[test]
    fn list_is_empty_without_any_state() {
        let root = tempfile::tempdir().unwrap();
        assert!(list(root.path()).unwrap().is_empty());
    }

    #[test]
    fn persist_records_default_branch_for_a_non_git_root() {
        let root = tempfile::tempdir().unwrap();
        // Non-git root with a single nested repo: persisting starts a fresh state
        // file whose default branch is blank (the root is not a repository).
        init_repo(&root.path().join("only"));
        create(root.path(), "s1").unwrap();

        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        assert_eq!(state.default_branch, "");
        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn create_on_a_repo_root_reports_a_worktree_dir_failure() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // A file where `.usagi/` should be makes the worktree parent
        // un-creatable, exercising the create_parent error path.
        fs::write(root.path().join(".usagi"), "not a dir").unwrap();

        let err = create(root.path(), "feat").unwrap_err();
        assert!(err.to_string().contains("failed to create"));
    }

    #[test]
    fn create_on_a_plain_root_reports_a_session_dir_failure() {
        let root = tempfile::tempdir().unwrap();
        // Non-git root; a file at `.usagi` blocks creating the session root.
        fs::write(root.path().join(".usagi"), "not a dir").unwrap();

        let err = create(root.path(), "feat").unwrap_err();
        assert!(err.to_string().contains("failed to create"));
    }

    #[test]
    fn create_propagates_a_walk_failure() {
        let root = tempfile::tempdir().unwrap();
        // A nested repo that already has the target branch makes `git worktree
        // add` fail, so walking the root surfaces the error.
        let app = root.path().join("app");
        init_repo(&app);
        assert!(git_cmd(&app)
            .args(["branch", "feat"])
            .status()
            .unwrap()
            .success());

        let err = create(root.path(), "feat").unwrap_err();
        assert!(err.to_string().contains("git worktree add failed"));
    }

    #[test]
    fn create_parent_is_a_noop_at_the_filesystem_root() {
        // The root has no parent, so create_parent does nothing and succeeds.
        create_parent(Path::new("/")).unwrap();
    }
}
