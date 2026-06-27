//! Per-worktree storage of the pull request discovered for a session.
//!
//! usagi does not query GitHub for a session's PR. Instead the TUI scans the
//! embedded agent's terminal output for a pull-request URL (see
//! [`crate::presentation::tui::home::terminal::link::pr_link`]) and records it
//! here, keyed by the session's worktree. The next workspace sync reads it back
//! and folds it into the worktree's [`PrLink`] so the sidebar shows `#<number>`
//! and a click reopens it — and, because it is persisted, the badge survives a
//! restart even though the agent only prints the URL once.
//!
//! Like [`super::agent_prompt_store`], the writer and reader may be different
//! processes that never share memory, so they agree on a file path purely from
//! the worktree directory: its canonical form hashed to a stable name under
//! `<data-dir>/pr-links/` (the addressing is shared in
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also stores the
//! worktree it belongs to, so a hash collision (or a stale file synced from
//! another machine) is detected and read as absent rather than misattributed.
//!
//! Unlike the prompt store, a read here is **not** one-shot: the PR stays until a
//! newer one replaces it, so the badge keeps showing across syncs.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::workspace_state::PrLink;
use crate::infrastructure::json_file;
use crate::infrastructure::worktree_keyed_store::{dir, file_name, key};

/// Subdirectory of the data dir the PR-link files live under.
const PR_SUBDIR: &str = "pr-links";

/// On-disk shape of a worktree's PR-link file.
#[derive(Serialize, Deserialize)]
struct PrLinkFile {
    /// The worktree this PR belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The pull request discovered for the worktree.
    pr: PrLink,
}

/// Record `pr` as the pull request for the session rooted at `worktree`,
/// replacing any previously recorded one.
pub fn set(worktree: &Path, pr: &PrLink) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PR_SUBDIR)?;
    let path = dir.join(file_name(&key));
    json_file::write_atomic(
        &dir,
        &path,
        &PrLinkFile {
            worktree: key,
            pr: pr.clone(),
        },
    )
}

/// The pull request recorded for the session rooted at `worktree`, or `None` when
/// none is recorded (or the file belongs to a different worktree, or is corrupt).
/// The read leaves the file in place — the PR persists until a newer one is set.
pub fn get(worktree: &Path) -> Option<PrLink> {
    let key = key(worktree);
    let dir = dir(PR_SUBDIR).ok()?;
    let path = dir.join(file_name(&key));
    match json_file::read::<PrLinkFile>(&path) {
        // Ours: hand back the recorded PR.
        Ok(Some(file)) if file.worktree.as_path() == key => Some(file.pr),
        // A file stamped with a different worktree (hash collision / synced stale
        // file), nothing recorded, or a corrupt file: read as absent. Unlike the
        // prompt store we never remove on read, so a collision victim is left for
        // its rightful owner and a corrupt file is simply overwritten by the next
        // `set`.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::storage;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body` with it.
    fn with_data_dir(body: impl FnOnce()) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body();
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    fn pr(number: u32) -> PrLink {
        PrLink {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    #[test]
    fn set_then_get_round_trips_and_is_not_one_shot() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing recorded yet.
            assert_eq!(get(wt.path()), None);
            // Record a PR, then read it — repeatedly: the read does not clear it.
            set(wt.path(), &pr(412)).unwrap();
            assert_eq!(get(wt.path()), Some(pr(412)));
            assert_eq!(get(wt.path()), Some(pr(412)));
        });
    }

    #[test]
    fn set_replaces_a_previously_recorded_pr() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), &pr(1)).unwrap();
            set(wt.path(), &pr(2)).unwrap();
            assert_eq!(get(wt.path()), Some(pr(2)));
        });
    }

    #[test]
    fn distinct_worktrees_record_independently() {
        with_data_dir(|| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            set(a.path(), &pr(10)).unwrap();
            set(b.path(), &pr(20)).unwrap();
            assert_eq!(get(a.path()), Some(pr(10)));
            assert_eq!(get(b.path()), Some(pr(20)));
        });
    }

    #[test]
    fn a_file_recorded_for_another_worktree_reads_as_absent_and_is_preserved() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at wt's hashed name but stamped with a different
            // worktree, as a hash collision or a synced stale file would be.
            let dir = dir(PR_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &PrLinkFile {
                    worktree: key(other.path()),
                    pr: pr(99),
                },
            )
            .unwrap();
            // It is not returned for wt, but the file is left intact for `other`.
            assert_eq!(get(wt.path()), None);
            assert!(path.exists());
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(PR_SUBDIR).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            std::fs::write(&path, "not json at all").unwrap();
            assert_eq!(get(wt.path()), None);
        });
    }
}
