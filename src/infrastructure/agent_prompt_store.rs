//! Per-worktree storage of a prompt queued for a session's agent.
//!
//! The MCP `session_prompt` tool runs in the `usagi mcp` process, which is
//! separate from a running TUI: it cannot reach into the home screen and drive a
//! pane directly. Instead it *queues* the prompt here, keyed by the session's
//! worktree, and the home screen delivers it the next time it freshly launches
//! that session's agent pane — the agent opens with the queued prompt as its
//! first message (see [`crate::domain::agent::Agent::launch_command`]).
//!
//! Like [`super::agent_state_store`], the writer (the MCP process) and the reader
//! (the TUI) never share memory, so they agree on a file path purely from the
//! worktree directory: its canonical form hashed to a stable, filesystem-safe
//! name under `<data-dir>/agent-prompts/` (the addressing is shared with the
//! phase store in [`crate::infrastructure::worktree_keyed_store`]). Each file
//! also stores the worktree it belongs to, so a hash collision (or a stale file
//! from another machine syncing the data dir) is detected and read as absent
//! rather than misattributed — and crucially left on disk for its rightful
//! owner, never deleted on a take by the wrong worktree.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::store_lock::{self, StoreLock};
use crate::infrastructure::worktree_keyed_store::{
    self, dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

/// Subdirectory of the data dir the queued-prompt files live under.
const PROMPT_SUBDIR: &str = "agent-prompts";
const RETRY_BASE: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
pub const MAX_PROMPT_RETRY_ATTEMPTS: u32 = 5;

/// On-disk shape of a worktree's queued-prompt file.
#[derive(Serialize, Deserialize)]
struct PromptFile {
    /// The worktree this prompt belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The prompt to hand the session's agent on its next fresh launch.
    prompt: String,
    /// Retry state for background autostart. Missing on v1 files.
    #[serde(default)]
    retry: Option<RetryState>,
}

impl WorktreeStamped for PromptFile {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryState {
    pub attempts: u32,
    pub next_retry_unix_secs: u64,
    pub last_error: String,
    #[serde(default)]
    pub dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeReady {
    Ready(String),
    Waiting(RetryState),
    Dead(RetryState),
    Empty,
}

fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn retry_delay(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(10);
    RETRY_BASE.saturating_mul(1u32 << shift).min(RETRY_MAX)
}

fn prompt_dir_for_take_ready() -> Result<PathBuf> {
    #[cfg(test)]
    if std::env::var_os("USAGI_TEST_AGENT_PROMPT_DIR_ERROR").is_some() {
        anyhow::bail!("forced prompt dir error");
    }
    dir(PROMPT_SUBDIR)
}

pub fn retry_state_after_failure(
    previous: Option<&RetryState>,
    error: &str,
    now: SystemTime,
) -> RetryState {
    let attempts = previous.map_or(1, |retry| retry.attempts.saturating_add(1));
    let dead = attempts >= MAX_PROMPT_RETRY_ATTEMPTS;
    let next_retry_unix_secs = unix_secs(now + retry_delay(attempts));
    RetryState {
        attempts,
        next_retry_unix_secs,
        last_error: error.to_string(),
        dead,
    }
}

/// Queue `prompt` for the agent of the session rooted at `worktree`, replacing
/// any prompt already queued there.
pub fn set(worktree: &Path, prompt: &str) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    // Hold the store lock across the write so a concurrent `take` (in the TUI
    // process) cannot read an old prompt and then delete the file this write
    // just renamed into place — which would silently drop the queued prompt.
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    write_stamped(
        &dir,
        &path,
        &PromptFile {
            worktree: key,
            prompt: prompt.to_string(),
            retry: None,
        },
    )
}

/// Take a queued prompt for background autostart only when its retry backoff has
/// elapsed. Manual launches still use [`take`] and can deliver a prompt even
/// while autostart is backing off.
pub fn take_ready(worktree: &Path, now: SystemTime) -> TakeReady {
    let key = key(worktree);
    let dir = match prompt_dir_for_take_ready() {
        Ok(dir) => dir,
        Err(_) => return TakeReady::Empty,
    };
    let _lock = match StoreLock::acquire(&dir) {
        Ok(lock) => lock,
        Err(_) => return TakeReady::Empty,
    };
    let path = dir.join(file_name(&key));
    let Some(file) = read_ours::<PromptFile>(&path, &key) else {
        return TakeReady::Empty;
    };
    if let Some(retry) = file.retry.as_ref().filter(|retry| retry.dead) {
        return TakeReady::Dead(retry.clone());
    }
    if let Some(retry) = file
        .retry
        .as_ref()
        .filter(|retry| retry.next_retry_unix_secs > unix_secs(now))
    {
        return TakeReady::Waiting(retry.clone());
    }
    let _ = fs::remove_file(&path);
    TakeReady::Ready(file.prompt)
}

/// Requeue a prompt that autostart failed to spawn, recording exponential
/// backoff state so the home loop does not retry every tick forever.
pub fn requeue_after_failure(
    worktree: &Path,
    prompt: &str,
    error: &str,
    now: SystemTime,
) -> Result<RetryState> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let previous = read_ours::<PromptFile>(&path, &key).and_then(|file| file.retry);
    let retry = retry_state_after_failure(previous.as_ref(), error, now);
    let file = PromptFile {
        worktree: key,
        prompt: prompt.to_string(),
        retry: Some(retry.clone()),
    };
    write_stamped(&dir, &path, &file)?;
    Ok(retry)
}

/// Take (read and remove) the prompt queued for the session rooted at
/// `worktree`, or `None` when none is queued (or the file belongs to a different
/// worktree). Removing it makes the prompt one-shot: a later launch that finds
/// nothing queued starts the agent without one.
pub fn take(worktree: &Path) -> Option<String> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR).ok()?;
    // Serialise the read-then-remove against `set` (see there): without the lock
    // a `set` landing between the read below and the remove would have its file
    // deleted and its prompt never delivered. If the lock cannot be taken we
    // return without removing anything, leaving the prompt queued for a later
    // launch rather than risking a lost or misattributed delivery.
    let _lock = StoreLock::acquire(&dir).ok()?;
    let path = dir.join(file_name(&key));
    match read_ours::<PromptFile>(&path, &key) {
        Some(file) => {
            let _ = fs::remove_file(&path);
            Some(file.prompt)
        }
        None => None,
    }
}

/// Put a prompt taken for delivery back in front of the launch queue.
///
/// A pending pane launch removes the one-shot prompt before the PTY exists so it
/// can submit it immediately after spawn. If the later spawn/cancel path proves
/// the prompt was not delivered, this restores it. When another prompt was queued after the
/// take, the restored prompt is prepended so retry order stays "old work first,
/// newly queued work next" rather than losing either side.
pub fn requeue_front(worktree: &Path, prompt: &str) -> Result<()> {
    if prompt.is_empty() {
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let merged = match read_ours::<PromptFile>(&path, &key) {
        Some(file) if !file.prompt.is_empty() => format!("{prompt}\n\n{}", file.prompt),
        _ => prompt.to_string(),
    };
    write_stamped(
        &dir,
        &path,
        &PromptFile {
            worktree: key,
            prompt: merged,
            retry: None,
        },
    )
}

/// Discard any prompt queued for `worktree` (best-effort), so a session removed
/// before its agent ever launched — and later recreated at the same path — does
/// not inherit a prompt queued for the previous session. Called from session
/// removal (see [`crate::usecase::session::remove`]); a no-op when nothing is
/// queued. Unlike [`take`] this does not hand the prompt back: it is being
/// thrown away with the session, not delivered.
pub fn clear(worktree: &Path) {
    worktree_keyed_store::clear(PROMPT_SUBDIR, worktree);
}

/// Whether any prompt is currently queued for *some* worktree — a cheap
/// directory listing, so the home screen's autostart pass can skip the
/// per-session lookup entirely on the common tick where nothing is queued.
///
/// This is deliberately coarse: it does not read or validate the files (a
/// [`take`] still confirms each queued prompt belongs to the worktree it is
/// keyed under). It only reports whether the queue directory holds anything
/// besides the shared [`StoreLock`] file, so a `false` lets the caller return
/// without hashing every session's worktree path. A missing directory (nothing
/// ever queued) reads as empty.
pub fn any_queued() -> bool {
    // A missing / unresolvable data dir (or an unreadable directory) means nothing
    // is queued; both failures collapse to an empty iterator rather than an early
    // return, so the common empty case is one cheap listing.
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
    use crate::infrastructure::json_file;
    use crate::infrastructure::storage;
    use std::time::{Duration, UNIX_EPOCH};

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
    fn set_then_take_round_trips_and_is_one_shot() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing queued yet.
            assert_eq!(take(wt.path()), None);
            // Queue a prompt, then take it once.
            set(wt.path(), "implement issue #50").unwrap();
            assert_eq!(take(wt.path()), Some("implement issue #50".to_string()));
            // Taking again finds nothing: the prompt is one-shot.
            assert_eq!(take(wt.path()), None);
        });
    }

    #[test]
    fn take_ready_respects_backoff_and_then_delivers_once() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued").unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(1_000);
            let retry = requeue_after_failure(wt.path(), "queued", "spawn failed", now).unwrap();
            assert_eq!(retry.attempts, 1);
            assert!(matches!(
                take_ready(wt.path(), now + Duration::from_secs(29)),
                TakeReady::Waiting(_)
            ));
            assert_eq!(
                take_ready(wt.path(), now + Duration::from_secs(30)),
                TakeReady::Ready("queued".to_string())
            );
            assert_eq!(
                take_ready(wt.path(), now + Duration::from_secs(31)),
                TakeReady::Empty
            );
        });
    }

    #[test]
    fn take_ready_reports_dead_letter_and_lock_failure_as_empty() {
        with_data_dir(|data_dir| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(2_000);
            let mut retry = None;
            for i in 0..MAX_PROMPT_RETRY_ATTEMPTS {
                retry = Some(
                    requeue_after_failure(wt.path(), "queued", &format!("err{i}"), now).unwrap(),
                );
            }
            assert!(retry.unwrap().dead);
            assert!(matches!(take_ready(wt.path(), now), TakeReady::Dead(_)));

            let store_dir = data_dir.join(PROMPT_SUBDIR);
            let _ = fs::remove_dir_all(&store_dir);
            fs::write(&store_dir, "not a directory").unwrap();
            assert_eq!(take_ready(wt.path(), now), TakeReady::Empty);
        });
    }

    #[test]
    fn take_ready_without_a_data_dir_is_empty() {
        let _guard = crate::test_support::process_env_guard();
        std::env::remove_var(storage::DATA_DIR_ENV);
        std::env::remove_var("HOME");
        assert_eq!(
            take_ready(Path::new("/tmp/usagi-no-home"), UNIX_EPOCH),
            TakeReady::Empty
        );
    }

    #[test]
    fn take_ready_returns_empty_when_prompt_dir_cannot_be_resolved() {
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR", "1");
        assert_eq!(
            take_ready(Path::new("/tmp/usagi-prompt-dir-error"), UNIX_EPOCH),
            TakeReady::Empty
        );
        std::env::remove_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR");
    }

    #[test]
    fn retry_state_exponentially_backs_off_and_dead_letters() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let mut retry = retry_state_after_failure(None, "err1", now);
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.next_retry_unix_secs, 10_030);
        assert!(!retry.dead);
        retry = retry_state_after_failure(Some(&retry), "err2", now);
        assert_eq!(retry.attempts, 2);
        assert_eq!(retry.next_retry_unix_secs, 10_060);
        for i in 3..=MAX_PROMPT_RETRY_ATTEMPTS {
            retry = retry_state_after_failure(Some(&retry), &format!("err{i}"), now);
        }
        assert_eq!(retry.attempts, MAX_PROMPT_RETRY_ATTEMPTS);
        assert!(retry.dead);
    }

    #[test]
    fn set_and_take_create_the_shared_store_lock() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued").unwrap();
            // The per-store lock file lives alongside the prompt files, so a
            // concurrent set/take in another process serialises behind it
            // (see StoreLock) and a queued prompt can never be lost to a race.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            assert!(StoreLock::path(&dir).exists());
            // Delivery still works with the lock held across read-then-remove.
            assert_eq!(take(wt.path()), Some("queued".to_string()));
            assert_eq!(take(wt.path()), None);
        });
    }

    #[test]
    fn set_replaces_a_previously_queued_prompt() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "first").unwrap();
            set(wt.path(), "second").unwrap();
            assert_eq!(take(wt.path()), Some("second".to_string()));
        });
    }

    #[test]
    fn requeue_front_restores_a_taken_prompt() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "first").unwrap();
            assert_eq!(take(wt.path()), Some("first".to_string()));
            requeue_front(wt.path(), "first").unwrap();
            assert_eq!(take(wt.path()), Some("first".to_string()));
        });
    }

    #[test]
    fn requeue_front_with_an_empty_prompt_is_a_noop() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued").unwrap();
            requeue_front(wt.path(), "").unwrap();
            assert_eq!(take(wt.path()), Some("queued".to_string()));
        });
    }

    #[test]
    fn requeue_front_preserves_a_prompt_queued_after_the_take() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "first").unwrap();
            assert_eq!(take(wt.path()), Some("first".to_string()));
            set(wt.path(), "second").unwrap();
            requeue_front(wt.path(), "first").unwrap();
            assert_eq!(
                take(wt.path()),
                Some("first\n\nsecond".to_string()),
                "retry sees the failed delivery before later queued work",
            );
        });
    }

    #[test]
    fn clear_discards_a_queued_prompt_without_delivering_it() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued for the old session").unwrap();
            clear(wt.path());
            // The prompt is gone: a session recreated at the same path does not
            // inherit it.
            assert_eq!(take(wt.path()), None);
            // Clearing again (nothing queued) is a harmless no-op.
            clear(wt.path());
        });
    }

    #[test]
    fn any_queued_reports_whether_a_prompt_is_waiting() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // No directory / no files yet: nothing is queued.
            assert!(!any_queued());
            // Queue one prompt: now something is waiting.
            set(wt.path(), "implement issue #98").unwrap();
            assert!(any_queued());
            // The store lock created alongside it does not count as a prompt, so
            // taking the only queued prompt leaves the directory reading as empty.
            assert_eq!(take(wt.path()), Some("implement issue #98".to_string()));
            assert!(StoreLock::path(&dir(PROMPT_SUBDIR).unwrap()).exists());
            assert!(!any_queued());
        });
    }

    #[test]
    fn distinct_worktrees_queue_independently() {
        with_data_dir(|_| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            set(a.path(), "for a").unwrap();
            set(b.path(), "for b").unwrap();
            assert_eq!(take(a.path()), Some("for a".to_string()));
            assert_eq!(take(b.path()), Some("for b".to_string()));
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
                &PromptFile {
                    worktree: key(other.path()),
                    prompt: "not ours".to_string(),
                    retry: None,
                },
            )
            .unwrap();
            // It is not returned for wt, but the file is left intact: it belongs
            // to `other`, which must still be able to take its own prompt.
            assert_eq!(take(wt.path()), None);
            assert!(path.exists());
            let still: PromptFile = json_file::read(&path).unwrap().unwrap();
            assert_eq!(still.worktree, key(other.path()));
            assert_eq!(still.prompt, "not ours");
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_and_is_cleared() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Write garbage at wt's hashed name so it cannot be parsed.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "not json at all").unwrap();
            // It reads as absent, and the unparseable file is cleared.
            assert_eq!(take(wt.path()), None);
            assert!(!path.exists());
        });
    }
}
