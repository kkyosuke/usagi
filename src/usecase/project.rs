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
use crate::infrastructure::git;
use crate::infrastructure::storage::Storage;
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
    Ok(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command;

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
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
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
}
