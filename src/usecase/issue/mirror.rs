//! Mirror the workspace issue store into each session's working tree.
//!
//! Issues are a single store at the workspace root (see [`super`]): every usagi
//! command resolves there, so an update from one session is immediately visible
//! to all. But each session is a git worktree that *also* checks out the tracked
//! `.usagi/issues/` files, so without help those per-session copies would go
//! stale after a mutation — and later merging a session branch could revert the
//! workspace store to its old issues. After every create / update / delete we
//! therefore copy the root's markdown files into every session that already has
//! an issue store, so the worktree copies always match the workspace.
//!
//! This is **best-effort**: the workspace write is authoritative and has already
//! happened, so a failure to refresh a session (a busy file, a vanished
//! directory) must not fail the issue operation. Callers run it as
//! `let _ = mirror::to_sessions(root);`. The next mutation re-syncs anything a
//! transient failure left behind.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const STATE_DIR: &str = ".usagi";
const SESSIONS_DIR: &str = "sessions";
const ISSUES_DIR: &str = "issues";

/// Copy the workspace's issue markdown files into every session's
/// `.usagi/issues/` so each worktree copy matches the workspace store.
///
/// Sessions that do not check out an issue store (the `.usagi/issues/`
/// directory is absent) are left untouched — we never create one where the
/// worktree does not already have it.
pub fn to_sessions(workspace_root: &Path) -> Result<()> {
    let sessions = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    if !sessions.is_dir() {
        return Ok(());
    }
    let src = workspace_root.join(STATE_DIR).join(ISSUES_DIR);
    for entry in
        fs::read_dir(&sessions).context(format!("failed to read {}", sessions.display()))?
    {
        let dst = entry
            .context(format!("failed to read an entry in {}", sessions.display()))?
            .path()
            .join(STATE_DIR)
            .join(ISSUES_DIR);
        if dst.is_dir() {
            sync_dir(&src, &dst)?;
        }
    }
    Ok(())
}

/// Make `dst` hold exactly the same `*.md` issue files as `src`: delete any
/// markdown file in `dst` that is gone from `src` (handles deletes and slug
/// renames), then copy every source file over. The derived `index.json` cache
/// is left untouched — each store rebuilds its own on the next read.
fn sync_dir(src: &Path, dst: &Path) -> Result<()> {
    let wanted = issue_files(src)?;
    for existing in issue_files(dst)? {
        if !wanted.iter().any(|p| p.file_name() == existing.file_name()) {
            fs::remove_file(&existing)
                .context(format!("failed to remove {}", existing.display()))?;
        }
    }
    for file in &wanted {
        let name = file
            .file_name()
            .expect("an issue file path has a final component");
        let target = dst.join(name);
        fs::copy(file, &target).context(format!("failed to copy into {}", target.display()))?;
    }
    Ok(())
}

/// Paths of the `*.md` issue files in `dir` (the `index.json` cache and any
/// other non-markdown entries excluded). Empty when `dir` does not exist.
fn issue_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).context(format!("failed to read {}", dir.display()))? {
        let path = entry
            .context(format!("failed to read an entry in {}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create `dir/name` with `body`.
    fn write(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    /// The `*.md` file names in `dir`, sorted.
    fn md_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".md"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn to_sessions_is_a_noop_without_a_sessions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join(".usagi/issues"), "001-a.md", "a");
        // No `.usagi/sessions/` exists: nothing to mirror, and no error.
        to_sessions(root).unwrap();
    }

    #[test]
    fn to_sessions_syncs_each_session_that_has_an_issue_store() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let src = root.join(".usagi/issues");
        write(&src, "001-a.md", "a");
        write(&src, "002-b.md", "b");
        write(&src, "index.json", "{}"); // a cache, not an issue file

        // `foo` already tracks an issue store; it holds a now-shared file
        // (`001-a.md`) plus a stale one (`009-old.md`) and its own cache.
        let foo = root.join(".usagi/sessions/foo/.usagi/issues");
        write(&foo, "001-a.md", "stale a");
        write(&foo, "009-old.md", "gone");
        write(&foo, "index.json", "{old}");
        // `bar` is a session without an issue store: it must be left alone.
        fs::create_dir_all(root.join(".usagi/sessions/bar")).unwrap();

        to_sessions(root).unwrap();

        // `foo` now mirrors the workspace's markdown files exactly...
        assert_eq!(md_names(&foo), vec!["001-a.md", "002-b.md"]);
        assert_eq!(fs::read_to_string(foo.join("001-a.md")).unwrap(), "a");
        // ...the stale issue is gone, and the local cache is untouched.
        assert!(!foo.join("009-old.md").exists());
        assert_eq!(fs::read_to_string(foo.join("index.json")).unwrap(), "{old}");
        // `bar` got no issue store created under it.
        assert!(!root.join(".usagi/sessions/bar/.usagi/issues").exists());
    }

    #[test]
    fn issue_files_skips_missing_dirs_and_non_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory that does not exist yields no files.
        assert!(issue_files(&tmp.path().join("nope")).unwrap().is_empty());

        let dir = tmp.path().join("issues");
        write(&dir, "001-a.md", "a");
        write(&dir, "index.json", "{}");
        let names: Vec<String> = issue_files(&dir)
            .unwrap()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["001-a.md"]);
    }
}
