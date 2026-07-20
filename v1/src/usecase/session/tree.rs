//! Mirroring a workspace into a session tree and discovering its repositories.
//!
//! A session mirrors the workspace under `.usagi/sessions/<name>/`: every git
//! repository found becomes a fresh `git worktree` (on a new branch
//! `usagi/<name>`, the session name under the `usagi/` namespace), while non-git
//! regular files and directories are copied; symlinks and special files are
//! rejected. The same
//! recursive walk that builds the tree ([`build_dir`]) is reused to discover the
//! source repositories a session spans ([`source_repos`]).

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
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
    fs::symlink_metadata(path.join(".git"))
        .map(|metadata| metadata.is_dir() || metadata.is_file())
        .unwrap_or(false)
}

/// A directory is a *linked worktree* when its `.git` is a file (a `gitdir:`
/// pointer) rather than a directory. Such directories are existing worktrees
/// managed elsewhere — `git worktree` checkouts like `.claude/worktrees/*` or a
/// `.workspace` — and must never be mirrored, branched, or descended into when
/// building a session.
pub(super) fn is_linked_worktree(path: &Path) -> bool {
    fs::symlink_metadata(path.join(".git"))
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// A non-Git entry that cannot be mirrored without crossing the source or
/// destination tree boundary.
#[derive(Debug)]
pub(super) enum TreeEntryError {
    Symlink(PathBuf),
    SpecialFile(PathBuf),
    SourceEscape { path: PathBuf, root: PathBuf },
    DestinationEscape { path: PathBuf, root: PathBuf },
}

impl fmt::Display for TreeEntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink(path) => write!(
                f,
                "refusing to copy symlink in non-Git workspace: {}",
                path.display()
            ),
            Self::SpecialFile(path) => write!(
                f,
                "refusing to copy special file in non-Git workspace: {}",
                path.display()
            ),
            Self::SourceEscape { path, root } => write!(
                f,
                "source entry {} escapes workspace root {}",
                path.display(),
                root.display()
            ),
            Self::DestinationEscape { path, root } => write!(
                f,
                "destination entry {} escapes session root {}",
                path.display(),
                root.display()
            ),
        }
    }
}

impl std::error::Error for TreeEntryError {}

/// Validate every entry the non-Git mirror will consume before creating any
/// destination entry. Paths are derived relative to `src`, and symlinks and
/// special files are rejected rather than followed or opened.
pub(super) fn validate_copy_tree(src: &Path, dest: &Path) -> Result<()> {
    let source_root = fs::canonicalize(src)
        .with_context(|| format!("failed to resolve source root {}", src.display()))?;
    validate_dir(src, src, &source_root, dest)
}

fn validate_dir(
    dir: &Path,
    lexical_root: &Path,
    source_root: &Path,
    dest_root: &Path,
) -> Result<()> {
    let canonical_dir =
        fs::canonicalize(dir).with_context(|| format!("failed to resolve {}", dir.display()))?;
    if !canonical_dir.starts_with(source_root) {
        return Err(TreeEntryError::SourceEscape {
            path: canonical_dir,
            root: source_root.to_path_buf(),
        }
        .into());
    }

    for entry in sorted_entries(dir)? {
        let name = entry.file_name();
        if SKIP.iter().any(|s| OsStr::new(s) == name) {
            continue;
        }
        let path = entry.path();
        let relative =
            path.strip_prefix(lexical_root)
                .map_err(|_| TreeEntryError::SourceEscape {
                    path: path.clone(),
                    root: source_root.to_path_buf(),
                })?;
        let destination = dest_root.join(relative);
        if !destination.starts_with(dest_root) {
            return Err(TreeEntryError::DestinationEscape {
                path: destination,
                root: dest_root.to_path_buf(),
            }
            .into());
        }

        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(TreeEntryError::Symlink(path).into());
        }
        if file_type.is_dir() {
            if !is_linked_worktree(&path) && !is_repo_root(&path) {
                validate_dir(&path, lexical_root, source_root, dest_root)?;
            }
        } else if !file_type.is_file() {
            return Err(TreeEntryError::SpecialFile(path).into());
        }
    }
    Ok(())
}

fn sorted_entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
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
/// and a plain file is copied. Symlinks and special files are rejected. Existing
/// linked worktrees ([`is_linked_worktree`]) are skipped entirely.
pub(super) fn build_dir(
    src: &Path,
    dest: &Path,
    branch: &str,
    base_commit: Option<&str>,
    worktrees: &mut Vec<PathBuf>,
) -> Result<()> {
    let source_root = fs::canonicalize(src)
        .with_context(|| format!("failed to resolve source root {}", src.display()))?;
    let destination_root = fs::canonicalize(dest)
        .with_context(|| format!("failed to resolve session root {}", dest.display()))?;
    build_dir_inner(
        src,
        dest,
        branch,
        base_commit,
        worktrees,
        &source_root,
        &destination_root,
    )
}

fn build_dir_inner(
    src: &Path,
    dest: &Path,
    branch: &str,
    base_commit: Option<&str>,
    worktrees: &mut Vec<PathBuf>,
    source_root: &Path,
    destination_root: &Path,
) -> Result<()> {
    let canonical_source =
        fs::canonicalize(src).with_context(|| format!("failed to resolve {}", src.display()))?;
    if !canonical_source.starts_with(source_root) {
        return Err(TreeEntryError::SourceEscape {
            path: canonical_source,
            root: source_root.to_path_buf(),
        }
        .into());
    }
    let canonical_destination =
        fs::canonicalize(dest).with_context(|| format!("failed to resolve {}", dest.display()))?;
    if !canonical_destination.starts_with(destination_root) {
        return Err(TreeEntryError::DestinationEscape {
            path: canonical_destination,
            root: destination_root.to_path_buf(),
        }
        .into());
    }

    for entry in sorted_entries(src)? {
        let name = entry.file_name();
        if SKIP.iter().any(|s| OsStr::new(s) == name) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        // Re-inspect immediately before each effect. The preflight validation
        // catches a stable bad tree before construction starts; this second
        // classification also refuses an entry replaced while the build runs.
        let file_type = fs::symlink_metadata(&from)
            .with_context(|| format!("failed to inspect {}", from.display()))?
            .file_type();

        if file_type.is_symlink() {
            return Err(TreeEntryError::Symlink(from).into());
        } else if file_type.is_dir() {
            // An existing linked worktree is not a source repository: skip it
            // outright rather than branch from it or descend into it.
            if is_linked_worktree(&from) {
                continue;
            }
            if is_repo_root(&from) {
                let configured_base = base_ref(&from);
                let base = base_commit.or(configured_base.as_deref());
                git::add_worktree(&from, &to, branch, base)?;
                worktrees.push(to.clone());
                git::init_submodules(&to)?;
            } else {
                fs::create_dir_all(&to).context(format!("failed to create {}", to.display()))?;
                build_dir_inner(
                    &from,
                    &to,
                    branch,
                    base_commit,
                    worktrees,
                    source_root,
                    destination_root,
                )?;
            }
        } else if file_type.is_file() {
            copy_regular_file(&from, &to)?;
        } else {
            return Err(TreeEntryError::SpecialFile(from).into());
        }
    }
    Ok(())
}

fn copy_regular_file(from: &Path, to: &Path) -> Result<()> {
    let mut source =
        open_regular_file(from).with_context(|| format!("failed to open {}", from.display()))?;
    if !source
        .metadata()
        .with_context(|| format!("failed to inspect {}", from.display()))?
        .is_file()
    {
        return Err(TreeEntryError::SpecialFile(from.to_path_buf()).into());
    }
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .with_context(|| format!("failed to create {}", to.display()))?;
    io::copy(&mut source, &mut destination)
        .with_context(|| format!("failed to copy {}", from.display()))?;
    let permissions = fs::symlink_metadata(from)
        .with_context(|| format!("failed to inspect {}", from.display()))?
        .permissions();
    fs::set_permissions(to, permissions)
        .with_context(|| format!("failed to set permissions on {}", to.display()))?;
    Ok(())
}

#[cfg(unix)]
fn open_regular_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
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
