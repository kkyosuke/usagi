//! Create and remove sessions: parallel working trees under
//! `.usagi/sessions/<name>/`.
//!
//! The workspace root need not itself be a git repository. The root is walked
//! recursively: every git repository found gets a fresh `git worktree` (on a new
//! branch named after the session) at its mirrored location under
//! `.usagi/sessions/<name>/`, while non-git files and directories are copied
//! there. This supports a single repository, or a tree containing several — e.g.
//!
//! ```text
//! /root            (not a repo)
//! ├── app-a/  =git → worktree
//! ├── be/          (plain dir → recurse)
//! │   └── be1/=git → worktree
//! └── README.md   → copied
//! ```
//!
//! This module owns the session lifecycle and state recording. The recursive
//! mirroring and repository discovery live in [`tree`]; reconciling the on-disk
//! tree with `state.json` lives in [`reconcile`].

mod reconcile;
mod tree;

pub use reconcile::reconcile;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::domain::workspace_state::SessionRecord;
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::usecase::workspace_state;

/// The outcome of creating a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    /// The session name (also the new branch name in every repository).
    pub name: String,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
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

    // Sync the on-disk tree with the recorded sessions first: a leftover
    // directory `state.json` does not know about is force-removed, so a stale
    // directory of the same name never blocks a fresh session.
    reconcile(workspace_root)?;

    let dest_root = workspace_root.join(".usagi").join("sessions").join(name);
    if dest_root.exists() {
        bail!("session \"{name}\" already exists");
    }

    // A session creates a branch named after it in every source repository.
    // If a repo already has branches nested under `<name>/` (e.g. `test/foo`),
    // git cannot create the `<name>` branch and fails partway with a cryptic
    // `cannot lock ref` error. Refuse up front with a clear, actionable message
    // before touching any repository.
    for repo in tree::source_repos(workspace_root) {
        if let Some(conflict) = git::branch_namespace_conflict(&repo, name) {
            bail!(
                "session \"{name}\" conflicts with the existing branch \"{conflict}\": \
                 a branch named \"{name}\" cannot be created alongside branches under \
                 \"{name}/\". Choose a different session name."
            );
        }
    }

    let mut worktrees = Vec::new();
    if tree::is_repo_root(workspace_root) {
        // The whole workspace is one repository: a single worktree at the root.
        let parent = dest_root
            .parent()
            .expect("dest_root always has a .usagi/sessions parent");
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        let base = tree::base_ref(workspace_root);
        git::add_worktree(workspace_root, &dest_root, name, base.as_deref())?;
        worktrees.push(dest_root.clone());
    } else {
        fs::create_dir_all(&dest_root)
            .context(format!("failed to create {}", dest_root.display()))?;
        tree::build_dir(workspace_root, &dest_root, name, &mut worktrees)?;
    }

    record(workspace_root, name, &dest_root, &worktrees)?;

    Ok(CreatedSession {
        name: name.to_string(),
        root: dest_root,
        worktrees,
    })
}

/// The local branch names that already exist across every source repository a
/// session under `workspace_root` would span, de-duplicated and sorted.
///
/// A new session cuts a `<name>` branch in each of these repos, so this is the
/// set its name must avoid — both as an exact duplicate and as a namespace
/// clash (a branch under `<name>/`). The TUI reads it once when the inline
/// create input opens to validate the typed name live (see
/// [`git::branch_namespace_conflict`]). Best-effort: a non-git or unreadable
/// repo simply contributes no names.
pub fn existing_branch_names(workspace_root: &Path) -> Vec<String> {
    use std::collections::BTreeSet;
    tree::source_repos(workspace_root)
        .iter()
        .flat_map(|repo| git::local_branches(repo))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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
    // Sync the on-disk tree with the recorded sessions: any session directory
    // `state.json` does not know about is force-removed regardless of
    // uncommitted changes (the recorded `name` itself keeps its dirty guard).
    reconcile(workspace_root)?;

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

/// Resolve the workspace root from a working directory that may sit inside a
/// session tree.
///
/// A session is mirrored at `<workspace>/.usagi/sessions/<name>/...`. When a
/// process runs from within such a tree (e.g. an agent's `usagi mcp` server),
/// its data stores still belong to the *workspace* — issues live at
/// `<workspace>/.usagi/issues/`, not in a throwaway copy under the session that
/// `usagi clean` later deletes. So we strip everything from the
/// `.usagi/sessions` segment onward and return the workspace root. A path that
/// is not inside a session tree is returned unchanged.
pub fn workspace_root(start: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = start.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() == OsStr::new(".usagi")
            && components
                .peek()
                .is_some_and(|next| next.as_os_str() == OsStr::new("sessions"))
        {
            return prefix;
        }
        prefix.push(component);
    }
    start.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::usecase::settings;

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

        let wt = root.path().join(".usagi/sessions/feature-x");
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

        let base = root.path().join(".usagi/sessions/wip");
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

    /// Add a linked worktree of `repo` at `dest` on a throwaway branch; its
    /// `.git` is a file pointer, marking it as an existing worktree to skip.
    fn add_linked_worktree(repo: &Path, dest: &Path, branch: &str) {
        assert!(git_cmd(repo)
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                dest.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success());
        assert!(dest.join(".git").is_file());
    }

    #[test]
    fn create_skips_existing_linked_worktrees() {
        let root = tempfile::tempdir().unwrap();
        // A real repo at the root is mirrored, but a linked worktree sitting
        // alongside it (e.g. a `.workspace` or `.claude/worktrees/*`) is left
        // untouched: not branched, not copied into the session.
        init_repo(&root.path().join("app"));
        add_linked_worktree(
            &root.path().join("app"),
            &root.path().join(".workspace"),
            "wt",
        );

        let created = create(root.path(), "wip").unwrap();

        let base = root.path().join(".usagi/sessions/wip");
        assert_eq!(created.worktrees, vec![base.join("app")]);
        assert!(!base.join(".workspace").exists());
    }

    #[test]
    fn source_repos_skips_linked_worktrees() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app"));
        add_linked_worktree(
            &root.path().join("app"),
            &root.path().join(".workspace"),
            "wt",
        );

        // Only the real repository is a source repo; the linked worktree is not.
        let repos = tree::source_repos(root.path());
        assert_eq!(repos, vec![root.path().join("app")]);
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
    fn existing_branch_names_unions_local_branches_across_repos() {
        // A multi-repo workspace: each repo's local branches are unioned, sorted
        // and de-duplicated; remote-tracking refs are excluded.
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        let run = |dir: &Path, args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };
        run(&root.path().join("app-a"), &["branch", "test/x"]);
        run(&root.path().join("be/be1"), &["branch", "feature"]);

        let names = existing_branch_names(root.path());
        // Both repos start on `main` (deduped) plus each one's extra branch.
        assert_eq!(
            names,
            vec![
                "feature".to_string(),
                "main".to_string(),
                "test/x".to_string()
            ]
        );

        // A non-git, empty root contributes nothing.
        let empty = tempfile::tempdir().unwrap();
        assert!(existing_branch_names(empty.path()).is_empty());
    }

    #[test]
    fn rejects_a_name_that_clashes_with_a_branch_namespace() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // Pre-create branches under `test/`, mirroring a repo that already has
        // `test/home-ui-e2e` etc. A plain `test` branch then cannot be created.
        for branch in ["test/home-ui-e2e", "test/tui-e2e-pty"] {
            assert!(git_cmd(root.path())
                .args(["branch", branch])
                .status()
                .unwrap()
                .success());
        }

        let err = create(root.path(), "test").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("conflicts with the existing branch"), "{msg}");
        assert!(msg.contains("test/home-ui-e2e"), "{msg}");
        // Nothing was created on the failed attempt.
        assert!(!root.path().join(".usagi/sessions/test").exists());
        assert!(sessions_of(root.path()).is_empty());
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

    /// Forget session `name` in `state.json` while leaving its on-disk directory
    /// in place — the exact "stray" state reconcile is meant to clean up.
    fn drop_record(root: &Path, name: &str) {
        let store = WorkspaceStore::new(root);
        let mut state = store.load().unwrap().unwrap();
        state.sessions.retain(|s| s.name != name);
        store.save(&state).unwrap();
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        git_cmd(repo)
            .args(["rev-parse", "--verify", "--quiet", branch])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn reconcile_is_a_noop_without_a_sessions_directory() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No `.usagi/sessions/` exists yet, so there is nothing to reconcile.
        assert!(reconcile(root.path()).unwrap().is_empty());
    }

    #[test]
    fn reconcile_force_removes_an_untracked_session_and_keeps_tracked_ones() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let kept = create(root.path(), "keep").unwrap();
        let stray = create(root.path(), "stray").unwrap();
        // Forget "stray" in state.json while its worktree stays on disk.
        drop_record(root.path(), "stray");

        let removed = reconcile(root.path()).unwrap();

        // The stray worktree directory and its branch are gone...
        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        assert!(!branch_exists(root.path(), "stray"));
        // ...while the tracked session and its branch survive untouched.
        assert!(kept.root.exists());
        assert_eq!(head_branch(&kept.root), "keep");
        assert!(branch_exists(root.path(), "keep"));
        assert_eq!(sessions_of(root.path()), vec!["keep".to_string()]);
    }

    #[test]
    fn reconcile_force_removes_a_dirty_untracked_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let stray = create(root.path(), "stray").unwrap();
        // Uncommitted work must not stop the sync.
        fs::write(stray.root.join("scratch.txt"), "wip").unwrap();
        drop_record(root.path(), "stray");

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        assert!(!branch_exists(root.path(), "stray"));
    }

    #[test]
    fn reconcile_ignores_loose_files_under_the_sessions_dir() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "keep").unwrap();
        // A loose *file* (not a directory) is not a session: leave it be.
        let loose = root.path().join(".usagi/sessions/NOTES.txt");
        fs::write(&loose, "scratch").unwrap();

        let removed = reconcile(root.path()).unwrap();

        assert!(removed.is_empty());
        assert!(loose.is_file());
    }

    #[test]
    fn reconcile_removes_a_stray_when_no_state_exists() {
        let root = tempfile::tempdir().unwrap();
        // A non-git root with a leftover session directory but no state.json.
        let ghost = root.path().join(".usagi/sessions/ghost");
        fs::create_dir_all(&ghost).unwrap();
        fs::write(ghost.join("leftover.txt"), "x").unwrap();

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![ghost.clone()]);
        assert!(!ghost.exists());
    }

    #[test]
    fn reconcile_removes_a_stray_across_a_multi_repo_workspace() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        let stray = create(root.path(), "wip").unwrap();
        drop_record(root.path(), "wip");

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        // The session branch is gone from every source repository.
        assert!(!branch_exists(&root.path().join("app-a"), "wip"));
        assert!(!branch_exists(&root.path().join("be/be1"), "wip"));
    }

    #[test]
    fn create_clears_a_stale_directory_of_the_same_name() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "dup").unwrap();
        // Forget the record but leave the worktree behind, as a crash might.
        drop_record(root.path(), "dup");

        // Re-creating "dup" succeeds: reconcile clears the stale tree first.
        let recreated = create(root.path(), "dup").unwrap();

        assert!(recreated.root.exists());
        assert_eq!(head_branch(&recreated.root), "dup");
        assert_eq!(sessions_of(root.path()), vec!["dup".to_string()]);
    }

    #[test]
    fn remove_also_prunes_other_strays() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let a = create(root.path(), "a").unwrap();
        let b = create(root.path(), "b").unwrap();
        // "b" becomes a stray; removing "a" should sync it away as well.
        drop_record(root.path(), "b");

        let outcome = remove(root.path(), "a", false).unwrap();

        assert!(outcome.removed);
        assert!(!a.root.exists());
        assert!(!b.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn workspace_root_strips_a_session_subtree() {
        // A cwd inside a session resolves back to the workspace root.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp")),
            PathBuf::from("/repo")
        );
        // ...including a subdirectory deeper within the session.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp/crate/src")),
            PathBuf::from("/repo")
        );
        // A doubly nested copy stops at the first session segment.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp/.usagi/issues")),
            PathBuf::from("/repo")
        );
    }

    #[test]
    fn workspace_root_leaves_a_plain_path_unchanged() {
        // Not inside a session tree: returned as-is.
        assert_eq!(workspace_root(Path::new("/repo")), PathBuf::from("/repo"));
        // A bare `.usagi` without a `sessions` child is not a session tree.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/issues")),
            PathBuf::from("/repo/.usagi/issues")
        );
    }
}
