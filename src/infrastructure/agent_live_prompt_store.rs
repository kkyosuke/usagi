//! Per-worktree queue of prompts to inject into a session's *already running*
//! agent, drained live by the TUI while it runs.
//!
//! This is the live counterpart of [`super::agent_prompt_store`]. Where that
//! store hands a prompt to the agent on its *next fresh launch* (the MCP
//! `session_prompt` tool), this one delivers to an agent pane that is *already
//! open*: the MCP `session_send` tool appends a prompt here, and the running home
//! screen's terminal-pool watcher drains it every tick and types it — followed by
//! a submit — straight into that session's live agent pane (see
//! [`crate::presentation::tui::home::terminal::pool`]). Nothing else can reach a
//! running pane across the process boundary: the MCP server runs in the separate
//! `usagi mcp` process, which shares no memory with the TUI, so the two agree on
//! a file path purely from the worktree directory.
//!
//! Unlike the one-shot launch prompt, several sends can pile up before the
//! watcher next drains them, so this file holds a **list** appended to in order
//! and taken all at once. Delivery is best-effort: if no live agent pane exists
//! (the session is not open in a running TUI), the queued prompts simply wait
//! here until one does — or are discarded with the session (see [`clear`]).
//!
//! Like [`super::agent_prompt_store`], the file is addressed purely from the
//! worktree directory: its canonical form hashed to a stable, filesystem-safe
//! name under `<data-dir>/agent-live-prompts/` (the addressing is shared via
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also records the
//! worktree it belongs to, so a hashed-name collision (or a stale file synced
//! from another machine) is detected and read as absent rather than
//! misattributed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::json_file;
use crate::infrastructure::store_lock::StoreLock;
use crate::infrastructure::worktree_keyed_store::{dir, file_name, key, path_for};

/// Subdirectory of the data dir the live-prompt files live under.
const PROMPT_SUBDIR: &str = "agent-live-prompts";

/// On-disk shape of a worktree's live-prompt queue.
#[derive(Serialize, Deserialize)]
struct LivePromptFile {
    /// The worktree this queue belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The prompts awaiting delivery to the session's live agent, in send order.
    prompts: Vec<String>,
}

/// Append `prompt` to the live queue for the session rooted at `worktree`, to be
/// delivered to its already-running agent pane. Preserves any prompts already
/// queued (unlike [`super::agent_prompt_store::set`], which replaces), so several
/// sends before the watcher next drains are all delivered in order.
pub fn append(worktree: &Path, prompt: &str) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    // Hold the store lock across the read-modify-write so a concurrent `append`
    // (another `session_send`) or a `take_all` (the TUI draining) cannot race and
    // drop a queued prompt.
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    // Start from the queue already stored for this worktree; a file stamped with
    // a different worktree (hash collision) or a corrupt one is treated as absent
    // and the queue starts fresh — the take side checks the stamp, so this can
    // never misdeliver another worktree's prompts.
    let mut prompts = match json_file::read::<LivePromptFile>(&path) {
        Ok(Some(file)) if file.worktree.as_path() == key => file.prompts,
        _ => Vec::new(),
    };
    prompts.push(prompt.to_string());
    json_file::write_atomic(
        &dir,
        &path,
        &LivePromptFile {
            worktree: key,
            prompts,
        },
    )
}

/// Take (read and remove) every prompt queued for the session rooted at
/// `worktree`, in send order, or an empty vector when none is queued (or the
/// file belongs to a different worktree). Removing the file makes delivery
/// one-shot: a later drain that finds nothing returns empty.
///
/// The common case each watcher tick is an empty queue, so this first checks the
/// file's existence cheaply and returns without taking the store lock when there
/// is nothing to drain.
pub fn take_all(worktree: &Path) -> Vec<String> {
    // Fast path: no file means nothing queued. Skips the lock (and its own lock
    // file creation) on the overwhelmingly common empty tick.
    match path_for(PROMPT_SUBDIR, worktree) {
        Ok(path) if path.exists() => {}
        _ => return Vec::new(),
    }
    let key = key(worktree);
    let dir = match dir(PROMPT_SUBDIR) {
        Ok(dir) => dir,
        Err(_) => return Vec::new(),
    };
    // Serialise the read-then-remove against `append` (see there): without the
    // lock an `append` landing between the read below and the remove would have
    // its file deleted and its prompt never delivered. If the lock cannot be
    // taken, leave everything queued for a later drain rather than risk loss.
    let _lock = match StoreLock::acquire(&dir) {
        Ok(lock) => lock,
        Err(_) => return Vec::new(),
    };
    let path = dir.join(file_name(&key));
    match json_file::read::<LivePromptFile>(&path) {
        // Ours: hand back the queued prompts and remove the file (one-shot).
        Ok(Some(file)) if file.worktree.as_path() == key => {
            let _ = fs::remove_file(&path);
            file.prompts
        }
        // A parseable file stamped with a different worktree: leave it untouched
        // for its rightful owner.
        Ok(Some(_)) => Vec::new(),
        // Nothing queued.
        Ok(None) => Vec::new(),
        // Corrupt / unparseable: it can never be delivered, so clear it.
        Err(_) => {
            let _ = fs::remove_file(&path);
            Vec::new()
        }
    }
}

/// Discard any prompts queued for `worktree` (best-effort), so a session removed
/// before its agent drained them — and later recreated at the same path — does
/// not inherit prompts sent to the previous session. Called from session removal
/// (see [`crate::usecase::session::remove`]); a no-op when nothing is queued.
pub fn clear(worktree: &Path) {
    if let Ok(path) = path_for(PROMPT_SUBDIR, worktree) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::storage;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body` with it.
    fn with_data_dir(body: impl FnOnce(&Path)) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body(dir.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn append_then_take_all_round_trips_in_order_and_is_one_shot() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing queued yet.
            assert!(take_all(wt.path()).is_empty());
            // Several sends pile up and are drained together, in send order.
            append(wt.path(), "first").unwrap();
            append(wt.path(), "second").unwrap();
            assert_eq!(take_all(wt.path()), vec!["first", "second"]);
            // Draining again finds nothing: delivery is one-shot.
            assert!(take_all(wt.path()).is_empty());
        });
    }

    #[test]
    fn append_and_take_all_create_the_shared_store_lock() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "queued").unwrap();
            // The per-store lock file lives alongside the prompt files, so a
            // concurrent append/drain in another process serialises behind it.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            assert!(StoreLock::path(&dir).exists());
            assert_eq!(take_all(wt.path()), vec!["queued"]);
        });
    }

    #[test]
    fn clear_discards_queued_prompts_without_delivering_them() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "for the old session").unwrap();
            clear(wt.path());
            // Gone: a session recreated at the same path does not inherit them.
            assert!(take_all(wt.path()).is_empty());
            // Clearing again (nothing queued) is a harmless no-op.
            clear(wt.path());
        });
    }

    #[test]
    fn distinct_worktrees_queue_independently() {
        with_data_dir(|_| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            append(a.path(), "for a").unwrap();
            append(b.path(), "for b").unwrap();
            assert_eq!(take_all(a.path()), vec!["for a"]);
            assert_eq!(take_all(b.path()), vec!["for b"]);
        });
    }

    #[test]
    fn a_file_queued_for_another_worktree_reads_as_absent_and_is_preserved() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at wt's hashed name but stamped with a different
            // worktree, as a hash collision or a synced stale file would be.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["not ours".to_string()],
                },
            )
            .unwrap();
            // Not returned for wt, and left intact for its rightful owner.
            assert!(take_all(wt.path()).is_empty());
            assert!(path.exists());
            let still: LivePromptFile = json_file::read(&path).unwrap().unwrap();
            assert_eq!(still.worktree, key(other.path()));
            assert_eq!(still.prompts, vec!["not ours".to_string()]);
        });
    }

    #[test]
    fn append_over_another_worktrees_file_starts_a_fresh_queue_stamped_as_ours() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            // A colliding file stamped for another worktree sits at wt's name.
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["theirs".to_string()],
                },
            )
            .unwrap();
            // Appending as wt does not adopt their prompts; it restamps the file
            // as ours with only our prompt, so we can never read theirs back.
            append(wt.path(), "ours").unwrap();
            assert_eq!(take_all(wt.path()), vec!["ours"]);
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_and_is_cleared() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "not json at all").unwrap();
            assert!(take_all(wt.path()).is_empty());
            assert!(!path.exists());
        });
    }

    #[test]
    fn append_after_a_corrupt_file_starts_a_fresh_queue() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "}{ not json").unwrap();
            // Append treats the unreadable file as an empty queue and overwrites it.
            append(wt.path(), "recovered").unwrap();
            assert_eq!(take_all(wt.path()), vec!["recovered"]);
        });
    }
}
