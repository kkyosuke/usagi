//! Mirroring a workspace into a session tree and discovering its repositories.
//!
//! A session mirrors the workspace under `.usagi/sessions/<name>/`: every git
//! repository found becomes a fresh `git worktree` (on a new branch named after
//! the session), while non-git files and directories are copied. The same
//! recursive walk that builds the tree ([`build_dir`]) is reused to discover the
//! source repositories a session spans ([`source_repos`]).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::infrastructure::git;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::usecase::settings;

/// Names never descended into or copied while building a session: usagi's own
/// data directory (which holds the session tree itself) and any `.git`.
const SKIP: &[&str] = &[".git", STATE_DIR];

/// A directory is a repository root when it directly contains a `.git` entry —
/// a directory for a normal clone, or a file for a linked worktree.
pub(super) fn is_repo_root(path: &Path) -> bool {
    path.join(".git").exists()
}

/// A directory is a *linked worktree* when its `.git` is a file (a `gitdir:`
/// pointer) rather than a directory. Such directories are existing worktrees
/// managed elsewhere — `git worktree` checkouts like `.claude/worktrees/*` or a
/// `.workspace` — and must never be mirrored, branched, or descended into when
/// building a session.
pub(super) fn is_linked_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
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
pub(super) fn base_ref(repo: &Path) -> Option<String> {
    let local = settings::load_local(repo).unwrap_or_default();
    git::resolve_base_ref(repo, local.branch_source(), local.default_branch())
}

/// Recursively mirror `src` into the already-created `dest`: a git repository
/// becomes a new worktree, a plain directory is recreated and descended into,
/// and a plain file is copied. Existing linked worktrees
/// ([`is_linked_worktree`]) are skipped entirely.
pub(super) fn build_dir(
    src: &Path,
    dest: &Path,
    branch: &str,
    worktrees: &mut Vec<PathBuf>,
) -> Result<()> {
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
        // An existing linked worktree is not a source repository: skip it
        // outright rather than branch from it or descend into it.
        if is_linked_worktree(&from) {
            continue;
        }
        let to = dest.join(&name);
        let file_type = entry
            .file_type()
            .context(format!("failed to inspect {}", from.display()))?;

        if file_type.is_dir() {
            if is_repo_root(&from) {
                let base = base_ref(&from);
                git::add_worktree(&from, &to, branch, base.as_deref())?;
                git::init_submodules(&to)?;
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

/// The source git repositories a session under `workspace_root` spans: the root
/// itself when it is a repository, otherwise every repository reached by the
/// same recursive walk [`build_dir`] uses.
pub(super) fn source_repos(workspace_root: &Path) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    if is_repo_root(workspace_root) {
        repos.push(workspace_root.to_path_buf());
    } else {
        collect_repos(workspace_root, &mut repos);
    }
    repos
}

/// Append every repository root reachable under `dir` to `repos`, recursing into
/// plain directories and skipping [`SKIP`] entries, symlinks, and existing
/// linked worktrees ([`is_linked_worktree`]). Best-effort: unreadable
/// directories and entries are silently skipped.
fn collect_repos(dir: &Path, repos: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if SKIP.iter().any(|s| OsStr::new(s) == entry.file_name()) {
            continue;
        }
        // Use the entry's own type, which does *not* follow symlinks, rather than
        // `path.is_dir()`, which does: a directory symlink pointing back at an
        // ancestor would otherwise make this recurse forever (stack overflow,
        // hanging `session create` / `reconcile`). `build_dir` likewise skips
        // symlinks (it copies them as plain files), so both walks agree that a
        // symlink is never a source repository.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        // An existing linked worktree is not a source repository.
        if is_linked_worktree(&path) {
            continue;
        }
        if is_repo_root(&path) {
            repos.push(path);
        } else {
            collect_repos(&path, repos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory symlink that points back at an ancestor must not be followed:
    /// `source_repos` skips it (so it never recurses forever) and still finds the
    /// real repositories beside it.
    #[cfg(unix)]
    #[test]
    fn source_repos_skips_directory_symlinks_and_does_not_recurse_into_cycles() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // A real repository (a directory holding a `.git` directory) to discover.
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        // A plain subdirectory the walk recurses into, holding a directory symlink
        // that points back at the workspace root — a cycle the old `path.is_dir()`
        // check would have followed until the stack overflowed.
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        symlink(root, sub.join("loop")).unwrap();

        // Terminates (no infinite recursion) and finds only the real repo.
        assert_eq!(source_repos(root), vec![repo]);
    }
}
