//! Create a new project from the New Project screen.
//!
//! "Creating" a project means: clone the repository into a chosen location,
//! register the clone as a workspace, and capture its initial worktree state in
//! `<repo>/.usagi/state.json`. This is the work performed when the user submits
//! the New Project form.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::domain::repository::RepoUrl;
use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;
use crate::infrastructure::{git, gitignore};
use crate::usecase::{workspace, workspace_state};

/// Directory used under the home directory when no `workspace_root` is set.
const DEFAULT_ROOT_DIR: &str = "git";

/// The base directory new projects are cloned under by default.
///
/// Prefers the configured `workspace_root`; otherwise falls back to `~/git`.
pub fn default_location(storage: &Storage) -> Result<PathBuf> {
    if let Some(root) = storage.load_settings()?.workspace_root {
        return Ok(root);
    }
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(DEFAULT_ROOT_DIR))
}

/// Clone `url` into `<location>/<directory>`, register it as a workspace, and
/// sync its initial worktree state. Returns the registered workspace.
///
/// Fails before cloning if the destination already exists, so an existing
/// checkout is never disturbed.
pub fn create(
    storage: &Storage,
    url: &RepoUrl,
    location: &Path,
    directory: &str,
    branch: Option<&str>,
) -> Result<Workspace> {
    let dest = location.join(directory);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    fs::create_dir_all(location).context(format!("failed to create {}", location.display()))?;

    git::clone(url.as_str(), &dest, branch)?;
    let workspace = workspace::add(storage, directory, &dest)?;
    workspace_state::sync(&dest)?;
    ignore_usagi_dir(&dest)?;
    Ok(workspace)
}

/// Register an existing directory as a workspace under `name`.
///
/// Unlike [`create`], nothing is cloned: the directory is used as-is. When it is
/// a git repository its initial worktree state is synced; a plain directory is
/// still registered (the sync is simply skipped), so usagi can track folders
/// that are not yet under version control.
pub fn register_existing(storage: &Storage, path: &Path, name: &str) -> Result<Workspace> {
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let workspace = workspace::add(storage, name, path)?;
    if git::is_repository(path) {
        workspace_state::sync(path)?;
        ignore_usagi_dir(path)?;
    }
    Ok(workspace)
}

/// Ensure `.usagi/` governs its own ignore rules so usagi's per-project metadata
/// stays out of git while the shared `issues/` directory remains tracked.
///
/// Writes `<repo>/.usagi/.gitignore` and removes any usagi entries a previous
/// version left in the repository-root `.gitignore`. The byte-level editing
/// lives in [`crate::infrastructure::gitignore`].
fn ignore_usagi_dir(repo: &Path) -> Result<()> {
    gitignore::write_usagi_gitignore(repo)?;
    gitignore::strip_legacy_root_entries(repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command;
    use crate::infrastructure::gitignore::USAGI_GITIGNORE;

    /// Create a throwaway source repository with one commit on `main`.
    fn init_source(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                test_command(dir).args(args).status().unwrap().success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn default_location_prefers_configured_root() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));
        let mut settings = storage.load_settings().unwrap();
        settings.workspace_root = Some(PathBuf::from("/custom/root"));
        storage.save_settings(&settings).unwrap();

        assert_eq!(
            default_location(&storage).unwrap(),
            PathBuf::from("/custom/root")
        );
    }

    #[test]
    fn default_location_falls_back_to_home_git() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));
        // No workspace_root configured: the fallback ends in `git`.
        assert!(default_location(&storage).unwrap().ends_with("git"));
    }

    #[test]
    fn create_clones_registers_and_syncs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        init_source(&src);
        let storage = Storage::new(tmp.path().join("usagi"));
        let location = tmp.path().join("workspaces");
        let url = RepoUrl::parse(src.to_str().unwrap()).unwrap();

        let workspace = create(&storage, &url, &location, "repo", None).unwrap();

        let dest = location.join("repo");
        assert_eq!(workspace.name, "repo");
        assert_eq!(workspace.path, dest);
        assert!(dest.join(".git").is_dir());
        assert!(dest.join(".usagi/state.json").is_file());
        // The clone gets a self-contained .usagi/.gitignore that keeps the
        // shared issues directory tracked, and its root .gitignore is left
        // untouched (the source repo had none, so none is created).
        assert_eq!(
            std::fs::read_to_string(dest.join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert!(!dest.join(".gitignore").exists());
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn register_existing_registers_a_git_repo_and_syncs_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_source(&repo);
        let storage = Storage::new(tmp.path().join("usagi"));

        let workspace = register_existing(&storage, &repo, "repo").unwrap();

        assert_eq!(workspace.name, "repo");
        assert_eq!(workspace.path, repo);
        // A git repo gets its worktree state captured.
        assert!(repo.join(".usagi/state.json").is_file());
        // ...and a .usagi/.gitignore that excludes the metadata directory.
        assert_eq!(
            std::fs::read_to_string(repo.join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn register_existing_registers_a_plain_dir_without_syncing() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));

        let workspace = register_existing(&storage, &plain, "plain").unwrap();

        assert_eq!(workspace.name, "plain");
        // Not a git repo: registered, but no state file or .usagi/.gitignore is
        // written.
        assert!(!plain.join(".usagi/state.json").exists());
        assert!(!plain.join(".usagi/.gitignore").exists());
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn register_existing_fails_for_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));
        let missing = tmp.path().join("nope");

        let err = register_existing(&storage, &missing, "nope").unwrap_err();
        assert!(err.to_string().contains("is not a directory"));
    }

    #[test]
    fn create_fails_when_destination_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));
        let location = tmp.path().to_path_buf();
        std::fs::create_dir_all(location.join("repo")).unwrap();
        let url = RepoUrl::parse("https://github.com/owner/repo.git").unwrap();

        let err = create(&storage, &url, &location, "repo", None).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn ignore_usagi_dir_writes_a_self_contained_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        ignore_usagi_dir(repo).unwrap();

        // The rules live inside .usagi/; the repo root is left clean.
        assert_eq!(
            std::fs::read_to_string(repo.join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert!(!repo.join(".gitignore").exists());
    }

    #[test]
    fn ignore_usagi_dir_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        ignore_usagi_dir(repo).unwrap();
        // A second run finds the file already current and leaves it untouched.
        ignore_usagi_dir(repo).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
    }

    #[test]
    fn ignore_usagi_dir_migrates_legacy_root_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Earlier versions wrote usagi entries into the repo-root .gitignore.
        // They are stripped (a bare `.usagi/` would otherwise hide the whole
        // directory), while unrelated content and a clean ending are preserved.
        let block = "node_modules\n.usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n";
        for root in [block, "node_modules\n.usagi/\n\n"] {
            std::fs::write(repo.join(".gitignore"), root).unwrap();

            ignore_usagi_dir(repo).unwrap();

            assert_eq!(
                std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
                "node_modules\n"
            );
            assert_eq!(
                std::fs::read_to_string(repo.join(".usagi/.gitignore")).unwrap(),
                USAGI_GITIGNORE
            );
        }
    }

    #[test]
    fn ignore_usagi_dir_keeps_an_unrelated_root_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join(".gitignore"), "target\n/build\n").unwrap();

        ignore_usagi_dir(repo).unwrap();

        // No usagi lines to strip: the root file is left untouched.
        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            "target\n/build\n"
        );
    }

    #[test]
    fn ignore_usagi_dir_reports_a_root_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the root .gitignore is expected fails to read with an
        // error other than NotFound, exercising that arm.
        std::fs::create_dir(tmp.path().join(".gitignore")).unwrap();
        assert!(ignore_usagi_dir(tmp.path()).is_err());
    }

    #[test]
    fn ignore_usagi_dir_reports_a_create_dir_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // A file occupying the .usagi path makes create_dir_all fail, which
        // propagates out of ignore_usagi_dir.
        std::fs::write(repo.join(".usagi"), "x").unwrap();
        assert!(ignore_usagi_dir(repo).is_err());
    }
}
