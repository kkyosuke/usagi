//! Per-worktree queue of prompts to inject into a session's *already running*
//! agent, drained live by the TUI while it runs.
//!
//! This is the live counterpart of [`super::agent_prompt_store`]. Both are fed by
//! the MCP `session_prompt` tool, which routes to one or the other by its `mode`:
//! where that store hands a prompt to the agent on its *next fresh launch* (the
//! launch channel), this one delivers to an agent pane that is *already open* (the
//! live channel). `session_prompt` appends a prompt here, and the running home
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
use crate::infrastructure::store_lock::{self, StoreLock};
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
    // (another live `session_prompt`) or a `take_all` (the TUI draining) cannot
    // race and drop a queued prompt.
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
    // The file existed a moment ago; drain it under the lock. A missing data dir
    // or a contended lock yields nothing, leaving anything queued for a later tick.
    drain(worktree).unwrap_or_default()
}

/// Read-and-remove the queued prompts under the store lock, or `None` when the
/// data dir or lock is unavailable. Split from [`take_all`] so those two early
/// exits collapse onto single `?` lines rather than never-taken `return` arms.
fn drain(worktree: &Path) -> Option<Vec<String>> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR).ok()?;
    // Serialise the read-then-remove against `append` (see there): without the
    // lock an `append` landing between the read below and the remove would have
    // its file deleted and its prompt never delivered. If the lock cannot be
    // taken, leave everything queued for a later drain rather than risk loss.
    let _lock = StoreLock::acquire(&dir).ok()?;
    let path = dir.join(file_name(&key));
    Some(match json_file::read::<LivePromptFile>(&path) {
        // Ours: hand back the queued prompts and remove the file (one-shot).
        Ok(Some(file)) if file.worktree.as_path() == key => {
            let _ = fs::remove_file(&path);
            file.prompts
        }
        // A file stamped for another worktree (hash collision) — leave it for its
        // rightful owner — or nothing parseable there (a race removed it between
        // the fast-path check and here). Either way there is nothing to hand back
        // and nothing of ours to remove.
        Ok(Some(_)) | Ok(None) => Vec::new(),
        // Corrupt / unparseable: it can never be delivered, so clear it.
        Err(_) => {
            let _ = fs::remove_file(&path);
            Vec::new()
        }
    })
}

/// Put `prompts` back at the **front** of the live queue for the session rooted
/// at `worktree`, ahead of anything queued since they were taken.
///
/// [`take_all`] drains the whole queue in one shot before the caller delivers
/// it; if delivery then fails partway (e.g. a PTY write to a wedged pane), the
/// undelivered tail would otherwise be lost even though the caller was told the
/// prompts were queued. This returns those prompts to the store in their
/// original order so a later tick retries them. Because a concurrent [`append`]
/// may have landed between the drain and here, the returned prompts are placed
/// *before* the newer ones, preserving overall send order. An empty slice is a
/// no-op (the file is left untouched). Best-effort like the rest of the store: a
/// missing data dir or a contended lock drops the prompts rather than blocking
/// the caller, matching [`take_all`]'s own "leave it for a later tick" stance.
pub fn requeue(worktree: &Path, prompts: &[String]) -> Result<()> {
    if prompts.is_empty() {
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    // Hold the store lock across the read-modify-write for the same reason
    // [`append`] does: a concurrent append/drain must not race and drop a prompt.
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    // Anything appended since the drain (stamped as ours); a file for a different
    // worktree or a corrupt one is treated as absent so we never adopt another
    // session's prompts — the same stamp check `append` / `drain` apply.
    let existing = match json_file::read::<LivePromptFile>(&path) {
        Ok(Some(file)) if file.worktree.as_path() == key => file.prompts,
        _ => Vec::new(),
    };
    let mut merged = prompts.to_vec();
    merged.extend(existing);
    json_file::write_atomic(
        &dir,
        &path,
        &LivePromptFile {
            worktree: key,
            prompts: merged,
        },
    )
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

/// Whether any prompt is currently queued for *some* worktree's live agent — a
/// cheap directory listing, so the home screen's autostart pass can skip the
/// per-session lookup on the common tick where nothing is queued.
///
/// The mirror of [`super::agent_prompt_store::any_queued`] for the live channel:
/// autostart consults both, since a pane-less session may have a prompt stranded
/// in *either* store. `session_prompt`'s explicit `mode="live"` always appends here
/// without checking for a live pane, so a prompt sent that way to a session with no
/// live agent pane waits here until one opens. (`auto` no longer strands prompts
/// here — it routes live only when the pid-stamped live-pane marker confirms a
/// running consumer, see [`super::agent_live_pane_store`].) Deliberately coarse —
/// it does not read or validate the files (a
/// [`take_all`] still confirms each queue belongs to the worktree it is keyed
/// under); it only reports whether the directory holds anything besides the
/// shared [`StoreLock`] file. A missing directory reads as empty.
pub fn any_queued() -> bool {
    dir(PROMPT_SUBDIR)
        .ok()
        .and_then(|dir| fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            // The store lock lives alongside the prompt files; it is not a queued
            // prompt, so a directory holding only the lock reads as empty.
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name().to_str() != Some(store_lock::LOCK_FILE_NAME)
        })
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

    #[test]
    fn requeue_restores_undelivered_prompts_for_a_later_take() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // A batch is drained, but delivery fails after the first prompt, so the
            // undelivered tail is put back — and a later drain hands it back in order.
            append(wt.path(), "first").unwrap();
            append(wt.path(), "second").unwrap();
            append(wt.path(), "third").unwrap();
            let taken = take_all(wt.path());
            assert_eq!(taken, vec!["first", "second", "third"]);
            // "first" delivered; "second"/"third" undelivered and returned.
            requeue(wt.path(), &taken[1..]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["second", "third"]);
        });
    }

    #[test]
    fn requeue_places_undelivered_prompts_before_ones_appended_since() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // A prompt is taken then fails to deliver; meanwhile a new send arrives.
            append(wt.path(), "taken").unwrap();
            let taken = take_all(wt.path());
            assert_eq!(taken, vec!["taken"]);
            append(wt.path(), "arrived-since").unwrap();
            // Requeuing puts the retried prompt ahead of the newer one, so overall
            // send order (taken before arrived-since) is preserved.
            requeue(wt.path(), &taken).unwrap();
            assert_eq!(take_all(wt.path()), vec!["taken", "arrived-since"]);
        });
    }

    #[test]
    fn requeue_of_nothing_is_a_no_op() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "kept").unwrap();
            // An empty requeue leaves the existing queue untouched and writes nothing.
            requeue(wt.path(), &[]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["kept"]);
        });
    }

    #[test]
    fn any_queued_reports_whether_a_live_prompt_is_waiting() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing queued (the directory may not even exist yet).
            assert!(!any_queued());
            // A queued prompt makes it report true.
            append(wt.path(), "queued").unwrap();
            assert!(any_queued());
            // Draining it (the file removed) makes it report empty again, even
            // though the shared store-lock file remains in the directory.
            let _ = take_all(wt.path());
            assert!(StoreLock::path(&dir(PROMPT_SUBDIR).unwrap()).exists());
            assert!(!any_queued());
        });
    }

    #[test]
    fn requeue_does_not_adopt_another_worktrees_prompts() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // A colliding file stamped for another worktree sits at wt's name.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["theirs".to_string()],
                },
            )
            .unwrap();
            // Requeuing as wt restamps the file as ours with only our prompt; the
            // other worktree's prompt is never adopted.
            requeue(wt.path(), &["ours".to_string()]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["ours"]);
        });
    }
}
