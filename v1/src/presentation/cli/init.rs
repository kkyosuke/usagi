//! `usagi init`: initialize a project from the current directory.
//!
//! Two modes, mirroring the New Project screen:
//!
//! - `usagi init` — register the current directory as a project as-is (like the
//!   screen's "Existing" mode).
//! - `usagi init --git <url>` — clone the repository into `<cwd>/<repo-name>`
//!   and register that directory (like the screen's "Clone" mode).
//!
//! Both reuse [`crate::usecase::project`], so the CLI and the TUI share one
//! initialization flow.

use std::env;
use std::path::Path;

use anyhow::{Context, Result};

use crate::domain::repository::RepoUrl;
use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;
use crate::usecase::project;

/// Entry point for `usagi init`.
///
/// With no `git` URL, registers the current directory. With one, clones the
/// repository into a directory named after it under the current directory.
pub fn run(git: Option<String>) -> Result<()> {
    let storage = Storage::open_default()?;
    let cwd = env::current_dir().context("could not determine the current directory")?;
    let workspace = init(&storage, &cwd, git.as_deref())?;
    println!(
        "registered '{}' at {}",
        workspace.name,
        workspace.path.display()
    );
    Ok(())
}

/// Initialize a project at `cwd`, optionally cloning `git` into it first.
///
/// Kept free of process globals (current dir, default storage) so it is
/// directly testable; [`run`] supplies those.
fn init(storage: &Storage, cwd: &Path, git: Option<&str>) -> Result<Workspace> {
    match git {
        // Clone mode: derive the directory name from the URL and clone into
        // `<cwd>/<name>`, registering the clone (default branch).
        Some(url) => {
            let url = RepoUrl::parse(url)?;
            let directory = url.directory_name();
            project::create(storage, &url, cwd, &directory, None)
        }
        // Register the current directory as-is.
        None => {
            let name = workspace_name(cwd)?;
            project::register_existing(storage, cwd, &name)
        }
    }
}

/// Derive a workspace name from a directory: its final path component.
fn workspace_name(path: &Path) -> Result<String> {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .context("could not derive a project name from the current directory")
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
    fn init_with_git_clones_into_a_named_directory_and_registers_it() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        init_source(&src);
        let storage = Storage::new(tmp.path().join("usagi"));
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let workspace = init(&storage, &cwd, Some(src.to_str().unwrap())).unwrap();

        // The clone lands in <cwd>/<repo-name> (the source dir is named "src").
        let dest = cwd.join("src");
        assert_eq!(workspace.name, "src");
        assert_eq!(workspace.path, dest);
        assert!(dest.join(".git").is_dir());
        assert!(dest.join(".usagi/state.json").is_file());
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn init_without_git_registers_the_current_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-app");
        init_source(&repo);
        let storage = Storage::new(tmp.path().join("usagi"));

        let workspace = init(&storage, &repo, None).unwrap();

        // The directory is registered as-is under its own name.
        assert_eq!(workspace.name, "my-app");
        assert_eq!(workspace.path, repo);
        assert!(repo.join(".usagi/state.json").is_file());
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn init_with_an_invalid_git_url_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("usagi"));
        let err = init(&storage, tmp.path(), Some("notaurl")).unwrap_err();
        assert!(err.to_string().contains("repository URL"));
    }

    #[test]
    fn workspace_name_uses_the_final_path_component() {
        assert_eq!(workspace_name(Path::new("/home/me/app")).unwrap(), "app");
    }

    #[test]
    fn workspace_name_fails_at_the_filesystem_root() {
        let err = workspace_name(Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("project name"));
    }

    #[test]
    fn run_registers_the_current_directory() {
        // `run` reads process globals, so point both the data dir and the
        // current directory at throwaway locations. The guard serializes this
        // against the other tests that mutate the cwd / $USAGI_HOME.
        let _guard = crate::test_support::process_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("project");
        init_source(&repo);

        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, &home);
        let original = env::current_dir().unwrap();
        env::set_current_dir(&repo).unwrap();
        let result = run(None);
        env::set_current_dir(original).unwrap();
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);

        assert!(result.is_ok());
        // The workspace was recorded in the temp data directory.
        let storage = Storage::new(&home);
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
    }
}
