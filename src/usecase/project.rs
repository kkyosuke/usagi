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

/// The `.gitignore` block usagi maintains: ignore everything under `.usagi/`
/// *except* the shared `issues/` directory, while still keeping the derived
/// `index.json` cache out of git.
///
/// Task issues are meant to be committed and shared with the team, so they are
/// re-included; the machine-local state (`state.json`, `settings.json`,
/// `history.json`, `worktree/`) and the rebuildable issue index stay ignored.
const USAGI_IGNORE_BLOCK: &[&str] = &[".usagi/*", "!.usagi/issues/", ".usagi/issues/index.json"];

/// Whether `line` is one of the gitignore entries usagi manages (including the
/// legacy `.usagi/` form), so it can be normalized to the current block.
fn is_usagi_ignore_line(line: &str) -> bool {
    matches!(
        line.trim(),
        ".usagi"
            | ".usagi/"
            | ".usagi/*"
            | "!.usagi/issues"
            | "!.usagi/issues/"
            | ".usagi/issues/index.json"
    )
}

/// Ensure the repository's `.gitignore` ignores usagi's per-project metadata
/// while keeping the shared `.usagi/issues/` directory tracked.
///
/// Idempotent: if the current [`USAGI_IGNORE_BLOCK`] is already present it does
/// nothing. Otherwise it strips any usagi-managed lines (including a legacy
/// `.usagi/` entry, which would wrongly hide the issues directory) and appends
/// the current block, preserving all other content and creating the file when
/// absent.
fn ignore_usagi_dir(repo: &Path) -> Result<()> {
    let gitignore = repo.join(".gitignore");

    let existing = match fs::read_to_string(&gitignore) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context(format!("failed to read {}", gitignore.display())),
    };

    // Already normalized: every managed line is present and nothing else needs
    // changing.
    let present: Vec<&str> = existing
        .lines()
        .filter(|l| is_usagi_ignore_line(l))
        .map(str::trim)
        .collect();
    if present == USAGI_IGNORE_BLOCK {
        return Ok(());
    }

    // Drop any managed lines (e.g. a legacy `.usagi/`) and keep the rest.
    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|l| !is_usagi_ignore_line(l))
        .collect();
    // Trim trailing blank lines so the appended block sits flush.
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }

    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    for entry in USAGI_IGNORE_BLOCK {
        out.push_str(entry);
        out.push('\n');
    }
    fs::write(&gitignore, out).context(format!("failed to write {}", gitignore.display()))
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
        // The clone's .gitignore is created with the usagi metadata block,
        // which keeps the shared issues directory tracked.
        assert_eq!(
            std::fs::read_to_string(dest.join(".gitignore")).unwrap(),
            ".usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n"
        );
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
        // ...and its .gitignore is updated to exclude the metadata directory.
        assert!(std::fs::read_to_string(repo.join(".gitignore"))
            .unwrap()
            .lines()
            .any(|l| l.trim() == ".usagi/*"));
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
        // Not a git repo: registered, but no state file or .gitignore is written.
        assert!(!plain.join(".usagi/state.json").exists());
        assert!(!plain.join(".gitignore").exists());
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

    /// The canonical block usagi appends, as it appears in the file.
    const IGNORE_BLOCK: &str = ".usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n";

    #[test]
    fn ignore_usagi_dir_appends_to_existing_gitignore_preserving_content() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Pre-existing content without a trailing newline.
        std::fs::write(repo.join(".gitignore"), "target\n/build").unwrap();

        ignore_usagi_dir(repo).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            format!("target\n/build\n{IGNORE_BLOCK}")
        );
    }

    #[test]
    fn ignore_usagi_dir_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join(".gitignore"),
            format!("node_modules\n{IGNORE_BLOCK}"),
        )
        .unwrap();

        ignore_usagi_dir(repo).unwrap();

        // Already normalized: left untouched, no duplicate entries.
        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            format!("node_modules\n{IGNORE_BLOCK}")
        );
    }

    #[test]
    fn ignore_usagi_dir_migrates_a_legacy_bare_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // A legacy `.usagi/` (or slashless `.usagi`) hid the issues directory;
        // it must be replaced by the selective block so issues are tracked.
        for legacy in [".usagi\n", ".usagi/\n", "node_modules\n.usagi/\n\n"] {
            std::fs::write(repo.join(".gitignore"), legacy).unwrap();
            ignore_usagi_dir(repo).unwrap();
            let result = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
            assert!(result.contains(".usagi/*\n"));
            assert!(result.contains("!.usagi/issues/\n"));
            // No bare legacy entry survives.
            assert!(!result
                .lines()
                .any(|l| matches!(l.trim(), ".usagi" | ".usagi/")));
        }
    }

    #[test]
    fn ignore_usagi_dir_reports_a_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where .gitignore is expected fails to read with an error
        // other than NotFound, exercising that arm.
        std::fs::create_dir(tmp.path().join(".gitignore")).unwrap();
        assert!(ignore_usagi_dir(tmp.path()).is_err());
    }

    #[test]
    fn ignore_usagi_dir_reports_a_write_error() {
        let tmp = tempfile::tempdir().unwrap();
        // The repo directory does not exist, so the read returns NotFound (empty)
        // and the subsequent write fails, exercising the write error arm.
        let missing = tmp.path().join("nope");
        assert!(ignore_usagi_dir(&missing).is_err());
    }
}
