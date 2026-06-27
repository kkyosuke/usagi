//! Per-worktree storage of the pull requests discovered for a session.
//!
//! usagi does not query GitHub for a session's PRs. Instead the TUI scans the
//! embedded agent's terminal output for pull-request URLs (see
//! [`crate::presentation::tui::home::terminal::link::pr_links`]) and records them
//! here, keyed by the session's worktree. The next workspace sync reads them back
//! and folds them into the worktree's [`PrLink`] list so the sidebar shows the
//! `#<number>` badges and a click reopens them — and, because they are persisted,
//! the badges survive a restart even though the agent only prints each URL once.
//!
//! A session may open several PRs (one per repository it touches, or several over
//! its life), so the store **accumulates** distinct URLs across calls rather than
//! replacing: [`add`] merges newly seen PRs into the recorded list (dropping
//! duplicate URLs), and [`get`] returns the whole list. The read is not one-shot —
//! the list stays so the badges keep showing across syncs.
//!
//! Like [`super::agent_prompt_store`], the writer and reader may be different
//! processes that never share memory, so they agree on a file path purely from
//! the worktree directory: its canonical form hashed to a stable name under
//! `<data-dir>/pr-links/` (the addressing is shared in
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also stores the
//! worktree it belongs to, so a hash collision (or a stale file synced from
//! another machine) is detected and read as absent rather than misattributed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::workspace_state::PrLink;
use crate::infrastructure::json_file;
use crate::infrastructure::store_lock::StoreLock;
use crate::infrastructure::worktree_keyed_store::{dir, file_name, key};

/// Subdirectory of the data dir the PR-link files live under.
const PR_SUBDIR: &str = "pr-links";

/// On-disk shape of a worktree's PR-link file.
#[derive(Serialize, Deserialize)]
struct PrLinkFile {
    /// The worktree these PRs belong to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The pull requests discovered for the worktree, in the order first seen.
    #[serde(default)]
    prs: Vec<PrLink>,
}

/// Merge `prs` into the pull requests recorded for the session rooted at
/// `worktree`, keeping the existing ones and appending any whose `url` is not
/// already recorded (so a PR seen again is not duplicated). New PRs land at the
/// end, after the ones already stored.
pub fn add(worktree: &Path, prs: &[PrLink]) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PR_SUBDIR)?;
    // Hold the store lock across read-modify-write so two processes adding PRs for
    // the same worktree at once cannot clobber each other's additions.
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let mut recorded = read_ours(&path, &key);
    for pr in prs {
        if !recorded.iter().any(|p| p.url == pr.url) {
            recorded.push(pr.clone());
        }
    }
    json_file::write_atomic(
        &dir,
        &path,
        &PrLinkFile {
            worktree: key,
            prs: recorded,
        },
    )
}

/// The pull requests recorded for the session rooted at `worktree`, or an empty
/// list when none are recorded (or the file belongs to a different worktree, or is
/// corrupt). The read leaves the file in place — the list persists across syncs.
pub fn get(worktree: &Path) -> Vec<PrLink> {
    let key = key(worktree);
    // An unresolvable data dir yields the empty list (via `unwrap_or_default`),
    // same as a missing file.
    dir(PR_SUBDIR)
        .map(|dir| read_ours(&dir.join(file_name(&key)), &key))
        .unwrap_or_default()
}

/// Read the PR list from `path`, but only when the file is stamped with `key` (our
/// worktree). A file stamped with a different worktree (a hash collision, or a
/// stale file synced from another machine), a missing file, or a corrupt one all
/// read as an empty list — a collision victim is left untouched for its rightful
/// owner, and a corrupt file is simply overwritten by the next [`add`].
fn read_ours(path: &Path, key: &Path) -> Vec<PrLink> {
    match json_file::read::<PrLinkFile>(path) {
        Ok(Some(file)) if file.worktree.as_path() == key => file.prs,
        _ => Vec::new(),
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
    fn add_then_get_round_trips_and_is_not_one_shot() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing recorded yet.
            assert_eq!(get(wt.path()), Vec::new());
            // Record PRs, then read them — repeatedly: the read does not clear them.
            add(wt.path(), &[pr(412)]).unwrap();
            assert_eq!(get(wt.path()), vec![pr(412)]);
            assert_eq!(get(wt.path()), vec![pr(412)]);
        });
    }

    #[test]
    fn add_accumulates_distinct_prs_and_drops_duplicate_urls() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            // Two PRs added across separate calls accumulate, in order seen.
            add(wt.path(), &[pr(1)]).unwrap();
            add(wt.path(), &[pr(2)]).unwrap();
            assert_eq!(get(wt.path()), vec![pr(1), pr(2)]);
            // Re-adding an already-recorded URL (and a new one) keeps the list
            // de-duplicated, appending only the new PR.
            add(wt.path(), &[pr(1), pr(3)]).unwrap();
            assert_eq!(get(wt.path()), vec![pr(1), pr(2), pr(3)]);
        });
    }

    #[test]
    fn distinct_worktrees_record_independently() {
        with_data_dir(|| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            add(a.path(), &[pr(10)]).unwrap();
            add(b.path(), &[pr(20)]).unwrap();
            assert_eq!(get(a.path()), vec![pr(10)]);
            assert_eq!(get(b.path()), vec![pr(20)]);
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
                    prs: vec![pr(99)],
                },
            )
            .unwrap();
            // It is not returned for wt, but the file is left intact for `other`.
            assert_eq!(get(wt.path()), Vec::new());
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
            assert_eq!(get(wt.path()), Vec::new());
        });
    }
}
