//! Whether an agent's tool call reaches outside its session worktree.
//!
//! A usagi session worktree lives *inside* the main repository
//! (`<repo>/.usagi/sessions/<name>/`), so the repository root and its other
//! worktrees sit just above it on disk. An agent that edits `<repo>/src/...` or
//! `cd`s up into the repo is touching the wrong tree — a recurring foot-gun, see
//! [`crate::presentation::cli::guard_workspace`] for how this is wired into
//! Claude Code as a `PreToolUse` guard.
//!
//! This module is the pure decision: given the worktree and the path a tool
//! wants to touch, does that path escape the worktree? It resolves the path
//! *lexically* (folding `.` / `..` without touching the filesystem) so it works
//! for files that do not exist yet (a fresh `Write` target) and stays
//! deterministic under test.

use std::path::{Component, Path, PathBuf};

/// True when `target` resolves outside `worktree`. A relative `target` is taken
/// relative to `worktree` (the agent's cwd), so it always stays inside; an
/// absolute path, or a relative one that climbs out with `..`, escapes when its
/// normalized form is not under the worktree. Comparison is component-wise, so a
/// sibling sharing a name prefix (`…/sessions/work` vs `…/sessions/work2`) does
/// not count as inside.
pub fn escapes_worktree(worktree: &Path, target: &Path) -> bool {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        worktree.join(target)
    };
    !normalize(&absolute).starts_with(normalize(worktree))
}

/// Fold `.` and `..` out of `path` lexically, without consulting the filesystem.
/// `..` pops the last kept component (and is a no-op at the root), so the result
/// never climbs above the path's own root. Used instead of
/// [`std::fs::canonicalize`] so a not-yet-created file still resolves.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: &str = "/repo/.usagi/sessions/work";

    #[test]
    fn a_file_under_the_worktree_stays_inside() {
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work/src/main.rs")
        ));
    }

    #[test]
    fn the_worktree_itself_is_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new(WT)));
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_worktree_and_stays_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new("src/lib.rs")));
    }

    #[test]
    fn an_absolute_path_into_the_parent_repo_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/src/main.rs")
        ));
    }

    #[test]
    fn a_relative_path_climbing_out_with_dotdot_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("../../../src/main.rs")
        ));
    }

    #[test]
    fn dotdot_that_stays_inside_does_not_escape() {
        // `work/src/../Cargo.toml` folds back to `work/Cargo.toml` — still inside.
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("src/../Cargo.toml")
        ));
    }

    #[test]
    fn a_sibling_worktree_sharing_a_name_prefix_escapes() {
        // Component-wise containment: `work2` is not under `work` despite the
        // string prefix.
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work2/src/main.rs")
        ));
    }

    #[test]
    fn dotdot_at_the_root_does_not_climb_above_it() {
        // `/..` normalizes to `/`, which is not under the worktree, so it escapes
        // rather than panicking or wrapping.
        assert!(escapes_worktree(Path::new(WT), Path::new("/../etc/passwd")));
    }

    #[test]
    fn normalize_drops_a_leading_current_dir_component() {
        // A leading `.` is the one `CurDir` form `Path::components` preserves
        // (mid-path `.` are already folded away), so normalize it directly: it is
        // skipped, leaving just the real components.
        assert_eq!(normalize(Path::new("./a/b")), PathBuf::from("a/b"));
    }
}
