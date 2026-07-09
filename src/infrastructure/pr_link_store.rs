//! Per-worktree storage of the pull requests discovered for a session.
//!
//! usagi does not query GitHub for a session's PRs. Instead the TUI scans live
//! embedded terminal output for pull-request URLs (see
//! [`crate::presentation::tui::home::terminal::link::pr_links`]) and records them
//! here, keyed by the session's worktree. The next workspace sync reads them back
//! and folds them into the worktree's [`PrLink`] list so the sidebar shows the
//! `#<number>` badges and a click reopens them — and, because they are persisted,
//! the badges survive a restart even though a command may print each URL only
//! once.
//!
//! A session may open several PRs (one per repository it touches, or several over
//! its life), so the store **accumulates** distinct PRs across calls rather than
//! replacing: [`add`] merges newly seen PRs into the recorded list (de-duplicated
//! by [`PrLink::pr_key`], so the plain and `/files` forms of one PR are one entry,
//! and a later title backfills an untitled entry), [`set`] overwrites the list
//! (used to write back resolved titles), and [`get`] returns the whole list. The
//! read is not one-shot — the list stays so the badges keep showing across syncs.
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
use crate::infrastructure::store_lock::StoreLock;
use crate::infrastructure::worktree_keyed_store::{
    self, dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

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

impl WorktreeStamped for PrLinkFile {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

/// Merge `prs` into the pull requests recorded for the session rooted at
/// `worktree`, keeping the existing ones and appending any whose
/// [`pr_key`](PrLink::pr_key) is not already recorded (so a PR seen again — even
/// under its `/files` deep link — is not duplicated). A PR already recorded but
/// still untitled adopts an incoming title, so a title resolved after the URL was
/// first stored is not lost. New PRs land at the end, after the ones already
/// stored.
pub fn add(worktree: &Path, prs: &[PrLink]) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PR_SUBDIR)?;
    // Hold the store lock across read-modify-write so two processes adding PRs for
    // the same worktree at once cannot clobber each other's additions.
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let mut recorded = read_prs_ours(&path, &key);
    for pr in prs {
        match recorded.iter_mut().find(|p| p.pr_key() == pr.pr_key()) {
            Some(existing) if existing.title.is_none() => existing.title = pr.title.clone(),
            Some(_) => {}
            None => recorded.push(pr.clone()),
        }
    }
    write_stamped(
        &dir,
        &path,
        &PrLinkFile {
            worktree: key,
            prs: recorded,
        },
    )
}

/// Overwrite the pull requests recorded for the session rooted at `worktree` with
/// `prs`. Unlike [`add`] this replaces the list wholesale — the terminal pool
/// uses it to write back a list whose missing titles it has just resolved (see
/// [`crate::infrastructure::pr_title`]), keeping the same set of PRs but with
/// their titles filled in.
pub fn set(worktree: &Path, prs: &[PrLink]) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PR_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    write_stamped(
        &dir,
        &path,
        &PrLinkFile {
            worktree: key,
            prs: prs.to_vec(),
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
        .map(|dir| read_prs_ours(&dir.join(file_name(&key)), &key))
        .unwrap_or_default()
}

/// Forget any PRs recorded for `worktree` (best-effort), so a session removed
/// and later recreated at the same path does not inherit the previous session's
/// PR badges. Called from session removal (see
/// [`crate::usecase::session::remove`]); a no-op when nothing is recorded.
pub fn clear(worktree: &Path) {
    worktree_keyed_store::clear(PR_SUBDIR, worktree);
}

/// Read the PR list from `path`, but only when the file is stamped with `key` (our
/// worktree). A file stamped with a different worktree (a hash collision, or a
/// stale file synced from another machine), a missing file, or a corrupt one all
/// read as an empty list.
fn read_prs_ours(path: &Path, key: &Path) -> Vec<PrLink> {
    read_ours::<PrLinkFile>(path, key)
        .map(|file| file.prs)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::json_file;
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
        PrLink::new(number, format!("https://github.com/o/r/pull/{number}"))
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
    fn add_dedups_the_files_url_and_backfills_a_title() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            add(wt.path(), &[pr(5)]).unwrap();
            // The same PR under its `/files` deep link is not a new entry.
            let files = PrLink::new(5, "https://github.com/o/r/pull/5/files");
            add(wt.path(), std::slice::from_ref(&files)).unwrap();
            assert_eq!(get(wt.path()), vec![pr(5)]);
            // Re-adding it with a title now resolved upgrades the stored entry in
            // place — keeping its canonical URL but adopting the title.
            let mut titled = pr(5);
            titled.title = Some("Fix the thing".to_string());
            add(wt.path(), std::slice::from_ref(&titled)).unwrap();
            let stored = get(wt.path());
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].url, "https://github.com/o/r/pull/5");
            assert_eq!(stored[0].title.as_deref(), Some("Fix the thing"));
            // An already-titled entry is not clobbered by a later untitled add.
            add(wt.path(), &[pr(5)]).unwrap();
            assert_eq!(get(wt.path())[0].title.as_deref(), Some("Fix the thing"));
        });
    }

    #[test]
    fn set_overwrites_the_recorded_list() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            add(wt.path(), &[pr(1), pr(2)]).unwrap();
            let mut titled = pr(1);
            titled.title = Some("t".to_string());
            // `set` replaces the whole list — here dropping #2 and titling #1.
            set(wt.path(), std::slice::from_ref(&titled)).unwrap();
            assert_eq!(get(wt.path()), vec![titled]);
        });
    }

    #[test]
    fn clear_forgets_recorded_prs() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            add(wt.path(), &[pr(1), pr(2)]).unwrap();
            clear(wt.path());
            // The PRs are gone: a session recreated at the same path starts fresh.
            assert_eq!(get(wt.path()), Vec::new());
            // Clearing again (nothing there) is a harmless no-op.
            clear(wt.path());
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
