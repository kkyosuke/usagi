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

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::domain::workspace_state::SessionRecord;
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::usecase::{settings, workspace_state};

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
        let base = base_ref(workspace_root);
        git::add_worktree(workspace_root, &dest_root, name, base.as_deref())?;
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
/// when none exists yet. This is what lets a multi-repo, non-git root still
/// track its sessions. Each worktree's git status is captured at record time.
fn record(workspace_root: &Path, name: &str, root: &Path, worktrees: &[PathBuf]) -> Result<()> {
    let store = WorkspaceStore::new(workspace_root);
    let mut state = store.load()?.unwrap_or_default();

    let worktree_states = worktrees
        .iter()
        .map(|path| workspace_state::inspect_worktree(path))
        .collect();

    let now = Utc::now();
    state.sessions.push(SessionRecord {
        name: name.to_string(),
        root: root.to_path_buf(),
        worktrees: worktree_states,
        created_at: now,
    });
    state.updated_at = now;
    store.save(&state)
}

/// The result of attempting to remove a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalOutcome {
    /// `true` when the session was removed; `false` when blocked by `dirty`.
    pub removed: bool,
    /// Worktrees with uncommitted changes that blocked a non-forced removal.
    /// Empty when the session was removed.
    pub dirty: Vec<PathBuf>,
}

/// Remove session `name` under `workspace_root`: delete every repository's
/// worktree and session branch, drop any copied files, and forget it in
/// `state.json`.
///
/// Without `force`, a session whose worktrees have uncommitted changes is left
/// untouched and the dirty worktrees are returned for the caller to warn about.
/// With `force`, those changes are discarded.
pub fn remove(workspace_root: &Path, name: &str, force: bool) -> Result<RemovalOutcome> {
    let store = WorkspaceStore::new(workspace_root);
    let mut state = store
        .load()?
        .ok_or_else(|| anyhow!("no sessions recorded for this workspace"))?;
    let index = state
        .sessions
        .iter()
        .position(|s| s.name == name)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))?;
    let session = state.sessions[index].clone();

    // Refuse to discard uncommitted work unless forced.
    let dirty: Vec<PathBuf> = session
        .worktrees
        .iter()
        .filter(|wt| git::has_uncommitted_changes(&wt.path))
        .map(|wt| wt.path.clone())
        .collect();
    if !dirty.is_empty() && !force {
        return Ok(RemovalOutcome {
            removed: false,
            dirty,
        });
    }

    // Remove each repository's worktree and its now-orphaned session branch.
    for wt in &session.worktrees {
        let source = git::primary_worktree(&wt.path)?;
        git::remove_worktree(&source, &wt.path, force)?;
        // The branch may already be gone (e.g. a partial earlier removal).
        let _ = git::delete_branch(&source, name);
    }

    // Drop any copied files and now-empty directories left in the tree.
    if session.root.exists() {
        fs::remove_dir_all(&session.root)
            .context(format!("failed to remove {}", session.root.display()))?;
    }

    state.sessions.remove(index);
    state.updated_at = Utc::now();
    store.save(&state)?;

    Ok(RemovalOutcome {
        removed: true,
        dirty: Vec::new(),
    })
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
                let base = base_ref(&from);
                git::add_worktree(&from, &to, branch, base.as_deref())?;
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

/// The ref a new session worktree in `repo` should branch from, per the repo's
/// project-local settings: the chosen
/// [`default_branch`](crate::domain::settings::LocalSettings::default_branch)
/// (or the detected default when unset) resolved through the
/// [`BranchSource`](crate::domain::settings::BranchSource).
///
/// Reading the local settings is best-effort: a missing or unreadable file
/// resolves to the defaults (detected branch, [`BranchSource::Remote`]). `None`
/// means "branch from the current HEAD" — either the chosen ref does not exist,
/// or the resolution fell through.
fn base_ref(repo: &Path) -> Option<String> {
    let local = settings::load_local(repo).unwrap_or_default();
    git::resolve_base_ref(repo, local.branch_source(), local.default_branch())
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

    /// The full HEAD commit at the worktree `dir`.
    fn head_commit(dir: &Path) -> String {
        let out = git_cmd(dir).args(["rev-parse", "HEAD"]).output().unwrap();
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
    fn branches_from_remote_by_default_and_from_local_when_configured() {
        use crate::domain::settings::{BranchSource, LocalSettings};

        // A repo whose local `main` is one commit ahead of `origin/main`, so the
        // two refs resolve to different commits and the chosen base is provable.
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let root = tmp.path().join("work");
        let run = |dir: &Path, args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };

        run(
            tmp.path(),
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        );
        init_repo(&root);
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        run(&root, &["push", "-q", "-u", "origin", "main"]);
        run(&root, &["remote", "set-head", "origin", "main"]);
        let remote_commit = head_commit(&root); // origin/main == first commit
                                                // Advance local main ahead of the remote.
        fs::write(root.join("code.txt"), "second").unwrap();
        run(&root, &["commit", "-aqm", "second"]);
        let local_commit = head_commit(&root);
        assert_ne!(remote_commit, local_commit);

        // Default (no local settings): session branches from origin/main.
        let created = create(&root, "from-remote").unwrap();
        assert_eq!(head_commit(&created.root), remote_commit);

        // Configured Local: session branches from the local default branch.
        settings::save_local(
            &root,
            &LocalSettings {
                default_branch_source: Some(BranchSource::Local),
                ..Default::default()
            },
        )
        .unwrap();
        let created = create(&root, "from-local").unwrap();
        assert_eq!(head_commit(&created.root), local_commit);
    }

    #[test]
    fn branches_from_a_configured_specific_branch() {
        use crate::domain::settings::LocalSettings;

        // A repo whose `develop` branch sits at a different commit than `main`,
        // so the chosen base is provable from the resulting HEAD.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let main_commit = head_commit(root.path());
        let run = |args: &[&str]| {
            assert!(git_cmd(root.path()).args(args).status().unwrap().success());
        };
        run(&["checkout", "-q", "-b", "develop"]);
        fs::write(root.path().join("code.txt"), "on develop").unwrap();
        run(&["commit", "-aqm", "develop work"]);
        let develop_commit = head_commit(root.path());
        run(&["checkout", "-q", "main"]);
        assert_ne!(main_commit, develop_commit);

        // Configure the session base to the `develop` branch (local form).
        settings::save_local(
            root.path(),
            &LocalSettings {
                default_branch_source: Some(crate::domain::settings::BranchSource::Local),
                default_branch: Some("develop".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let created = create(root.path(), "from-develop").unwrap();
        assert_eq!(head_commit(&created.root), develop_commit);
    }

    #[test]
    fn fails_when_the_session_directory_cannot_be_created() {
        let root = tempfile::tempdir().unwrap();
        // A non-repo root whose `.usagi` is a *file* makes create_dir_all fail.
        fs::write(root.path().join(".usagi"), "not a dir").unwrap();

        let err = create(root.path(), "x").unwrap_err();
        assert!(err.to_string().contains("failed to create"));
    }

    // --- remove ------------------------------------------------------------

    fn sessions_of(root: &Path) -> Vec<String> {
        WorkspaceStore::new(root)
            .load()
            .unwrap()
            .map(|s| s.sessions.into_iter().map(|r| r.name).collect())
            .unwrap_or_default()
    }

    #[test]
    fn remove_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = remove(root.path(), "x", false).unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = remove(root.path(), "absent", false).unwrap_err();
        assert!(err.to_string().contains("no such session"));
    }

    #[test]
    fn remove_deletes_a_clean_single_repo_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "feature").unwrap();
        assert!(created.root.exists());

        let outcome = remove(root.path(), "feature", false).unwrap();
        assert!(outcome.removed);
        assert!(outcome.dirty.is_empty());
        // The worktree directory and the state record are both gone.
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
        // The branch was deleted in the source repo.
        assert!(!git_cmd(root.path())
            .args(["rev-parse", "--verify", "--quiet", "feature"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn remove_cleans_a_multi_repo_session_including_copied_files() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        let created = create(root.path(), "wip").unwrap();
        assert!(created.root.join("README.md").exists());

        let outcome = remove(root.path(), "wip", false).unwrap();
        assert!(outcome.removed);
        // The whole session tree (worktrees + copied files) is gone.
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn remove_warns_on_uncommitted_changes_and_forces_through() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "dirty").unwrap();
        // Make the worktree dirty.
        fs::write(created.root.join("scratch.txt"), "wip").unwrap();

        // Without force: blocked, nothing removed, the dirty worktree reported.
        let outcome = remove(root.path(), "dirty", false).unwrap();
        assert!(!outcome.removed);
        assert_eq!(outcome.dirty, vec![created.root.clone()]);
        assert!(created.root.exists());
        assert_eq!(sessions_of(root.path()), vec!["dirty".to_string()]);

        // With force: removed despite the changes.
        let outcome = remove(root.path(), "dirty", true).unwrap();
        assert!(outcome.removed);
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }
}
