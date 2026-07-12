//! Per-worktree storage of a prompt queued for a session's agent.
//!
//! The MCP `session_prompt` tool runs in the `usagi mcp` process, which is
//! separate from a running TUI: it cannot reach into the home screen and drive a
//! pane directly. Instead it *queues* the prompt here, keyed by the session's
//! worktree. Explicit queue requests remain opening messages for the next fresh
//! agent launch (see [`crate::domain::agent::Agent::launch_command`]); an `auto`
//! request that fell back here only because the TUI was absent may instead opt
//! into delivery to an eligible existing agent when the TUI resumes.
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

use anyhow::{anyhow, Result};
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
    /// The queued launch-channel prompt. `reuse_live_agent` below decides
    /// whether it requires a fresh launch or may use an existing agent.
    prompt: String,
    /// Retry state for background autostart. Missing on v1 files.
    #[serde(default)]
    retry: Option<RetryState>,
    /// Whether an autostart pass may deliver this launch prompt to an eligible
    /// existing agent. Only `session_prompt(mode=auto)` falling back to the launch
    /// channel opts in; explicit `mode=queue` remains fresh-only.
    #[serde(default)]
    reuse_live_agent: bool,
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

/// One launch prompt removed from the durable store for an in-flight delivery.
/// Callers that may fail or cancel before delivery keep this whole value and
/// restore it with [`requeue_taken`], rather than losing retry/policy metadata by
/// carrying only the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenPrompt {
    pub prompt: String,
    pub retry: Option<RetryState>,
    pub reuse_live_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeReady {
    Ready {
        prompt: String,
        retry: Option<RetryState>,
        reuse_live_agent: bool,
    },
    /// A prompt exists and is ready, but its contract requires a fresh launch.
    /// Returned only by [`take_ready_for_live_agent`] and left on disk.
    FreshLaunch,
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
    set_with_live_handoff(worktree, prompt, false)
}

/// Queue `prompt`, optionally allowing an eligible existing agent to consume
/// it. Explicit queue callers use [`set`] (fresh-only); `session_prompt(auto)`
/// uses this opt-in when it chose the launch channel only because no TUI marker
/// was present.
pub fn set_with_live_handoff(worktree: &Path, prompt: &str, reuse_live_agent: bool) -> Result<()> {
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
            reuse_live_agent,
        },
    )?;
    // Publish the launch transaction only after the prompt record is durable.
    // session_prompt saves its SessionAgent override before calling this API, so
    // the request pins the authoritative CLI/model generation it belongs to.
    crate::infrastructure::agent_start_store::publish(worktree, prompt, reuse_live_agent)?;
    Ok(())
}

/// Take a queued prompt for background autostart only when its retry backoff has
/// elapsed. Manual launches still use [`take`] and can deliver a prompt even
/// while autostart is backing off.
pub fn take_ready(worktree: &Path, now: SystemTime) -> TakeReady {
    take_ready_inner(worktree, now, false).unwrap_or(TakeReady::Empty)
}

/// Strict unattended take used by restore ownership decisions. Store/path/lock
/// failures are surfaced so restore can fail closed instead of fresh-spawning
/// past a prompt it could not inspect.
pub fn take_ready_strict(worktree: &Path, now: SystemTime) -> Result<TakeReady> {
    take_ready_inner(worktree, now, false)
}

/// Take a ready launch prompt only when its producer allowed delivery to an
/// already-live eligible agent. A fresh-only prompt is reported without removing it,
/// preserving explicit `mode=queue` and pinned CLI/model launch semantics.
pub fn take_ready_for_live_agent(worktree: &Path, now: SystemTime) -> TakeReady {
    take_ready_for_live_agent_strict(worktree, now).unwrap_or(TakeReady::Empty)
}

/// Strict existing-agent take for ordering-sensitive watcher delivery. A store
/// inspection failure must keep the newer live queue untouched rather than
/// pretending the older launch channel is empty.
pub fn take_ready_for_live_agent_strict(worktree: &Path, now: SystemTime) -> Result<TakeReady> {
    take_ready_inner(worktree, now, true)
}

fn take_ready_inner(
    worktree: &Path,
    now: SystemTime,
    require_live_handoff: bool,
) -> Result<TakeReady> {
    let durable = crate::infrastructure::agent_start_store::read(worktree);
    let key = key(worktree);
    let dir = prompt_dir_for_take_ready()?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    if !path.exists() {
        return Ok(TakeReady::Empty);
    }
    let Some(file) = read_ours::<PromptFile>(&path, &key) else {
        return Err(anyhow!(
            "queued prompt file {} is unreadable or belongs to another worktree",
            path.display()
        ));
    };
    // Delivery policy outranks retry state for an existing agent. A fresh-only
    // record belongs to another channel entirely, so neither its backoff nor its
    // dead-letter state may head-of-line block newer explicit live work.
    if require_live_handoff && !file.reuse_live_agent {
        return Ok(TakeReady::FreshLaunch);
    }
    if let Some(retry) = file.retry.as_ref().filter(|retry| retry.dead) {
        return Ok(TakeReady::Dead(retry.clone()));
    }
    if let Some(retry) = file
        .retry
        .as_ref()
        .filter(|retry| retry.next_retry_unix_secs > unix_secs(now))
    {
        return Ok(TakeReady::Waiting(retry.clone()));
    }
    let claimed_id = if let Some(request) = durable {
        match crate::infrastructure::agent_start_store::claim(
            worktree,
            &format!("tui:{}", std::process::id()),
            now,
        )? {
            Some(claimed) => Some(claimed.id),
            None => {
                return Ok(TakeReady::Waiting(RetryState {
                    attempts: request.attempts,
                    next_retry_unix_secs: unix_secs(now) + 1,
                    last_error: "durable start request is claimed by another consumer".to_string(),
                    dead: false,
                }));
            }
        }
    } else {
        None
    };
    fs::remove_file(&path)?;
    if let Some(id) = claimed_id {
        let _ = crate::infrastructure::agent_start_store::clear(worktree, id);
    }
    Ok(TakeReady::Ready {
        prompt: file.prompt,
        retry: file.retry,
        reuse_live_agent: file.reuse_live_agent,
    })
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
    let existing = read_ours::<PromptFile>(&path, &key);
    let previous = existing.as_ref().and_then(|file| file.retry.as_ref());
    let reuse_live_agent = existing.as_ref().is_some_and(|file| file.reuse_live_agent);
    let retry = retry_state_after_failure(previous, error, now);
    let file = PromptFile {
        worktree: key,
        prompt: prompt.to_string(),
        retry: Some(retry.clone()),
        reuse_live_agent,
    };
    write_stamped(&dir, &path, &file)?;
    Ok(retry)
}

/// Restore a taken prompt at the front of the launch queue after its delivery
/// failed, preserving both its retry history and any prompt queued concurrently
/// after the take.
///
/// The read/merge/write is one store-lock transaction. `previous_retry` belongs
/// to the prompt returned by [`take_ready`]; when absent, retry metadata already
/// on a concurrently queued file is the fallback baseline. The resulting retry
/// state applies to the merged oldest-first work and enforces the same bounded
/// backoff/dead-letter policy as background spawn failures.
pub fn requeue_front_after_failure(
    worktree: &Path,
    prompt: &str,
    previous_retry: Option<&RetryState>,
    reuse_live_agent: bool,
    error: &str,
    now: SystemTime,
) -> Result<RetryState> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let existing = read_ours::<PromptFile>(&path, &key);
    // A concurrent fresh-only prompt may carry pinned CLI/model semantics. Once
    // two records are merged into one oldest-first message, live handoff is safe
    // only when both records opted in.
    let reuse_live_agent =
        reuse_live_agent && existing.as_ref().is_none_or(|file| file.reuse_live_agent);
    let retry = retry_state_after_failure(
        previous_retry.or_else(|| existing.as_ref().and_then(|file| file.retry.as_ref())),
        error,
        now,
    );
    let merged = match existing {
        Some(file) if !file.prompt.is_empty() && !prompt.is_empty() => {
            format!("{prompt}\n\n{}", file.prompt)
        }
        Some(file) if prompt.is_empty() => file.prompt,
        _ => prompt.to_string(),
    };
    write_stamped(
        &dir,
        &path,
        &PromptFile {
            worktree: key,
            prompt: merged.clone(),
            retry: Some(retry.clone()),
            reuse_live_agent,
        },
    )?;
    Ok(retry)
}

/// Take (read and remove) the prompt queued for the session rooted at
/// `worktree`, or `None` when none is queued (or the file belongs to a different
/// worktree). Removing it makes the prompt one-shot: a later launch that finds
/// nothing queued starts the agent without one.
pub fn take(worktree: &Path) -> Option<String> {
    take_with_state(worktree).map(|taken| taken.prompt)
}

/// Take a queued prompt for a manual/fresh launch, including the metadata that
/// must be restored if the launch is canceled or fails. Unlike [`take_ready`],
/// an explicit manual launch intentionally bypasses automatic retry backoff.
pub fn take_with_state(worktree: &Path) -> Option<TakenPrompt> {
    if crate::infrastructure::agent_start_store::read(worktree).is_some()
        && crate::infrastructure::agent_start_store::claim(
            worktree,
            &format!("tui:{}", std::process::id()),
            SystemTime::now(),
        )
        .ok()?
        .is_none()
    {
        return None;
    }
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
            if let Some(request) = crate::infrastructure::agent_start_store::read(worktree) {
                let _ = crate::infrastructure::agent_start_store::clear(worktree, request.id);
            }
            Some(TakenPrompt {
                prompt: file.prompt,
                retry: file.retry,
                reuse_live_agent: file.reuse_live_agent,
            })
        }
        None => None,
    }
}

/// Whether `worktree` currently owns a durable launch prompt. Unlike
/// [`any_queued`], this validates the stamped worktree under the store lock and
/// is therefore suitable for deciding whether a restore fallback must yield to
/// the queued launch owner.
pub fn has_queued(worktree: &Path) -> Result<bool> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    if !path.exists() {
        return Ok(false);
    }
    read_ours::<PromptFile>(&path, &key)
        .map(|_| true)
        .ok_or_else(|| {
            anyhow!(
                "queued prompt file {} is unreadable or belongs to another worktree",
                path.display()
            )
        })
}

/// Restore a state-carrying prompt removed by [`take_with_state`] without
/// counting a new failure.
pub fn requeue_taken(worktree: &Path, taken: TakenPrompt) -> Result<()> {
    requeue_front_with_state(worktree, &taken.prompt, taken.retry, taken.reuse_live_agent)
}

/// Put a prompt taken for delivery back in front of the launch queue.
///
/// A pending pane launch removes the one-shot prompt before the PTY exists so it
/// can submit it immediately after spawn. If the later spawn/cancel path proves
/// the prompt was not delivered, this restores it. When another prompt was queued after the
/// take, the restored prompt is prepended so retry order stays "old work first,
/// newly queued work next" rather than losing either side.
pub fn requeue_front(worktree: &Path, prompt: &str) -> Result<()> {
    requeue_front_with_live_handoff(worktree, prompt, false)
}

/// Put a taken prompt back at the front while preserving whether it may be
/// handed to an eligible live agent.
pub fn requeue_front_with_live_handoff(
    worktree: &Path,
    prompt: &str,
    reuse_live_agent: bool,
) -> Result<()> {
    requeue_front_with_state(worktree, prompt, None, reuse_live_agent)
}

/// Put a taken prompt back at the front without counting another failure,
/// preserving its retry state and live-handoff contract. This is used when a
/// prepared launch is canceled or cannot reserve a slot before delivery begins.
pub fn requeue_front_with_state(
    worktree: &Path,
    prompt: &str,
    retry: Option<RetryState>,
    reuse_live_agent: bool,
) -> Result<()> {
    if prompt.is_empty() {
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    let path = dir.join(file_name(&key));
    let existing = read_ours::<PromptFile>(&path, &key);
    let reuse_live_agent =
        reuse_live_agent && existing.as_ref().is_none_or(|file| file.reuse_live_agent);
    let retry = retry.or_else(|| existing.as_ref().and_then(|file| file.retry.clone()));
    let merged = match existing {
        Some(file) if !file.prompt.is_empty() => format!("{prompt}\n\n{}", file.prompt),
        _ => prompt.to_string(),
    };
    write_stamped(
        &dir,
        &path,
        &PromptFile {
            worktree: key,
            prompt: merged.clone(),
            retry,
            reuse_live_agent,
        },
    )?;
    crate::infrastructure::agent_start_store::publish(worktree, &merged, reuse_live_agent)?;
    Ok(())
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

    fn ready_parts(taken: TakeReady) -> Option<(String, Option<RetryState>, bool)> {
        match taken {
            TakeReady::Ready {
                prompt,
                retry,
                reuse_live_agent,
            } => Some((prompt, retry, reuse_live_agent)),
            _ => None,
        }
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
            let now = SystemTime::now();
            let retry = requeue_after_failure(wt.path(), "queued", "spawn failed", now).unwrap();
            assert_eq!(retry.attempts, 1);
            assert!(matches!(
                take_ready(wt.path(), now + Duration::from_secs(29)),
                TakeReady::Waiting(_)
            ));
            assert_eq!(
                take_ready(wt.path(), now + Duration::from_secs(30)),
                TakeReady::Ready {
                    prompt: "queued".to_string(),
                    retry: Some(retry),
                    reuse_live_agent: false,
                }
            );
            assert_eq!(
                take_ready(wt.path(), now + Duration::from_secs(31)),
                TakeReady::Empty
            );
        });
    }

    #[test]
    fn durable_claim_blocks_a_second_tui_consumer() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued").unwrap();
            let now = SystemTime::now();
            crate::infrastructure::agent_start_store::claim(wt.path(), "daemon", now)
                .unwrap()
                .unwrap();
            assert!(matches!(take_ready(wt.path(), now), TakeReady::Waiting(_)));
            assert_eq!(take_with_state(wt.path()), None);
        });
    }

    #[test]
    fn durable_claim_errors_are_surfaced_without_taking_the_prompt() {
        with_data_dir(|data| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "queued").unwrap();
            let start_dir = data.join("agent-start-requests");
            let lock = start_dir.join(crate::infrastructure::store_lock::LOCK_FILE_NAME);
            fs::remove_file(&lock).unwrap();
            fs::create_dir(&lock).unwrap();
            assert!(take_ready_strict(wt.path(), SystemTime::now()).is_err());
            assert!(has_queued(wt.path()).unwrap());
        });
    }

    #[test]
    fn publish_paths_surface_prompt_write_failures() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let store_dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&store_dir).unwrap();
            let path = store_dir.join(file_name(&key(wt.path())));
            fs::create_dir(&path).unwrap();
            assert!(set(wt.path(), "queued").is_err());
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            assert!(requeue_front_with_state(wt.path(), "queued", None, false).is_err());
        });
    }

    #[test]
    fn only_opted_in_launch_prompts_can_be_taken_for_a_live_agent() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(1_000);

            set(wt.path(), "fresh only").unwrap();
            assert_eq!(
                take_ready_for_live_agent(wt.path(), now),
                TakeReady::FreshLaunch
            );
            assert_eq!(take(wt.path()).as_deref(), Some("fresh only"));

            set(wt.path(), "fresh backing off").unwrap();
            requeue_after_failure(wt.path(), "fresh backing off", "spawn failed", now).unwrap();
            assert_eq!(
                take_ready_for_live_agent(wt.path(), now),
                TakeReady::FreshLaunch,
                "fresh-only retry state must not block the independent live channel",
            );
            for attempt in 1..MAX_PROMPT_RETRY_ATTEMPTS {
                requeue_after_failure(
                    wt.path(),
                    "fresh backing off",
                    &format!("spawn failed {attempt}"),
                    now,
                )
                .unwrap();
            }
            assert_eq!(
                take_ready_for_live_agent(wt.path(), now),
                TakeReady::FreshLaunch,
                "fresh-only dead-letter must not block the independent live channel",
            );
            assert_eq!(take(wt.path()).as_deref(), Some("fresh backing off"));

            set_with_live_handoff(wt.path(), "reuse idle", true).unwrap();
            assert_eq!(
                take_ready_for_live_agent(wt.path(), now),
                TakeReady::Ready {
                    prompt: "reuse idle".to_string(),
                    retry: None,
                    reuse_live_agent: true,
                }
            );
            assert_eq!(take(wt.path()), None);
        });
    }

    #[test]
    fn legacy_prompt_without_a_handoff_policy_remains_fresh_launch_only() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let key = key(wt.path());
            let store_dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&store_dir).unwrap();
            let path = store_dir.join(file_name(&key));
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "worktree": key,
                    "prompt": "legacy fresh work",
                    "retry": null,
                }))
                .unwrap(),
            )
            .unwrap();

            assert_eq!(
                take_ready_for_live_agent(wt.path(), UNIX_EPOCH),
                TakeReady::FreshLaunch
            );
            assert!(
                path.exists(),
                "live handoff must not consume the legacy file"
            );
            assert_eq!(take(wt.path()).as_deref(), Some("legacy fresh work"));
        });
    }

    #[test]
    fn repeated_take_then_failure_preserves_retry_history_until_dead_letter() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set_with_live_handoff(wt.path(), "retry me", true).unwrap();
            let mut now = UNIX_EPOCH + Duration::from_secs(2_000);

            for expected_attempt in 1..=MAX_PROMPT_RETRY_ATTEMPTS {
                let (prompt, retry, reuse_live_agent) =
                    ready_parts(take_ready(wt.path(), now)).unwrap();
                let state = requeue_front_after_failure(
                    wt.path(),
                    &prompt,
                    retry.as_ref(),
                    reuse_live_agent,
                    "spawn failed",
                    now,
                )
                .unwrap();
                assert_eq!(state.attempts, expected_attempt);
                assert_eq!(state.dead, expected_attempt == MAX_PROMPT_RETRY_ATTEMPTS);
                now = UNIX_EPOCH + Duration::from_secs(state.next_retry_unix_secs);
            }

            assert!(matches!(take_ready(wt.path(), now), TakeReady::Dead(_)));
        });
    }

    #[test]
    fn requeue_merge_requires_every_prompt_to_allow_live_handoff() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set_with_live_handoff(wt.path(), "auto fallback", true).unwrap();
            let (prompt, retry, reuse_live_agent) =
                ready_parts(take_ready_for_live_agent(wt.path(), UNIX_EPOCH)).unwrap();

            // An explicit queue request lands after the take but before the
            // failed delivery is restored.
            set(wt.path(), "fresh pinned work").unwrap();
            requeue_front_after_failure(
                wt.path(),
                &prompt,
                retry.as_ref(),
                reuse_live_agent,
                "input failed",
                UNIX_EPOCH,
            )
            .unwrap();

            assert!(matches!(
                take_ready_for_live_agent(
                    wt.path(),
                    UNIX_EPOCH + Duration::from_secs(RETRY_BASE.as_secs())
                ),
                TakeReady::FreshLaunch
            ));
            assert_eq!(
                take(wt.path()).as_deref(),
                Some("auto fallback\n\nfresh pinned work")
            );
        });
    }

    #[test]
    fn failure_restore_handles_empty_old_work_and_surfaces_write_errors() {
        with_data_dir(|_| {
            assert_eq!(ready_parts(TakeReady::Empty), None);

            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "concurrent fresh work").unwrap();
            requeue_front_after_failure(wt.path(), "", None, true, "old input failed", UNIX_EPOCH)
                .unwrap();
            assert_eq!(take(wt.path()).as_deref(), Some("concurrent fresh work"));

            // Leave a directory at the addressed file path: the store lock is
            // still usable, but the atomic rename cannot replace a directory.
            // The error must reach the caller so it can log/escalate rather than
            // claim that the undelivered prompt was restored.
            let broken = tempfile::tempdir().unwrap();
            let store_dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&store_dir).unwrap();
            let prompt_path = store_dir.join(file_name(&key(broken.path())));
            fs::create_dir(&prompt_path).unwrap();
            assert!(requeue_front_after_failure(
                broken.path(),
                "undelivered",
                None,
                true,
                "input failed",
                UNIX_EPOCH,
            )
            .is_err());
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
    fn strict_take_surfaces_store_errors_instead_of_reporting_empty() {
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR", "1");
        let error = take_ready_strict(Path::new("/tmp/usagi-prompt-dir-error-strict"), UNIX_EPOCH)
            .unwrap_err()
            .to_string();
        std::env::remove_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR");

        assert!(error.contains("forced prompt dir error"));
    }

    #[test]
    fn strict_take_and_has_queued_reject_corrupt_prompt_metadata() {
        with_data_dir(|_| {
            let take_wt = tempfile::tempdir().unwrap();
            assert!(!has_queued(take_wt.path()).unwrap());

            set(take_wt.path(), "must remain attributable").unwrap();
            let take_path = dir(PROMPT_SUBDIR)
                .unwrap()
                .join(file_name(&key(take_wt.path())));
            fs::write(&take_path, "not json").unwrap();

            let take_error = take_ready_strict(take_wt.path(), UNIX_EPOCH)
                .unwrap_err()
                .to_string();
            assert!(take_error.contains("unreadable or belongs to another worktree"));

            let inspect_wt = tempfile::tempdir().unwrap();
            set(inspect_wt.path(), "inspect without consuming").unwrap();
            let inspect_path = dir(PROMPT_SUBDIR)
                .unwrap()
                .join(file_name(&key(inspect_wt.path())));
            fs::write(inspect_path, "not json").unwrap();
            let inspect_error = has_queued(inspect_wt.path()).unwrap_err().to_string();
            assert!(inspect_error.contains("unreadable or belongs to another worktree"));
        });
    }

    #[test]
    fn strict_live_handoff_surfaces_store_errors_instead_of_reporting_empty() {
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR", "1");
        let error = take_ready_for_live_agent_strict(
            Path::new("/tmp/usagi-live-handoff-dir-error-strict"),
            UNIX_EPOCH,
        )
        .unwrap_err()
        .to_string();
        std::env::remove_var("USAGI_TEST_AGENT_PROMPT_DIR_ERROR");

        assert!(error.contains("forced prompt dir error"));
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
    fn state_carrying_manual_take_restores_retry_and_handoff_policy() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(4_000);
            set_with_live_handoff(wt.path(), "manual retry", true).unwrap();
            let expected_retry =
                requeue_after_failure(wt.path(), "manual retry", "spawn failed", now).unwrap();

            let taken = take_with_state(wt.path()).unwrap();
            assert_eq!(taken.prompt, "manual retry");
            assert_eq!(taken.retry.as_ref(), Some(&expected_retry));
            assert!(taken.reuse_live_agent);
            requeue_taken(wt.path(), taken).unwrap();

            assert_eq!(
                take_ready_for_live_agent(
                    wt.path(),
                    UNIX_EPOCH + Duration::from_secs(expected_retry.next_retry_unix_secs)
                ),
                TakeReady::Ready {
                    prompt: "manual retry".to_string(),
                    retry: Some(expected_retry),
                    reuse_live_agent: true,
                }
            );
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
                    reuse_live_agent: false,
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
