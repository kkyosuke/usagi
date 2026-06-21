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
//! name under `<data-dir>/agent-prompts/`. Each file also stores the worktree it
//! belongs to, so a hash collision (or a stale file from another machine syncing
//! the data dir) is detected and read as absent rather than misattributed —
//! and crucially left on disk for its rightful owner, never deleted on a take
//! by the wrong worktree.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::{json_file, storage};

/// Subdirectory of the data dir the queued-prompt files live under.
const PROMPT_SUBDIR: &str = "agent-prompts";

/// On-disk shape of a worktree's queued-prompt file.
#[derive(Serialize, Deserialize)]
struct PromptFile {
    /// The worktree this prompt belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The prompt to hand the session's agent on its next fresh launch.
    prompt: String,
}

/// The directory queued-prompt files live under: `<data-dir>/agent-prompts/`.
fn dir() -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(PROMPT_SUBDIR))
}

/// The file name a worktree's prompt is stored under: a stable hash of its
/// canonical path rendered as hex, so the writer and reader agree on it without
/// listing the directory. Pure given `canonical`.
fn file_name(canonical: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The key a worktree is stored under: its canonical path, falling back to the
/// path as given when it cannot be resolved, so the writer and reader still
/// derive the same name.
fn key(worktree: &Path) -> PathBuf {
    worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf())
}

/// Queue `prompt` for the agent of the session rooted at `worktree`, replacing
/// any prompt already queued there.
pub fn set(worktree: &Path, prompt: &str) -> Result<()> {
    let key = key(worktree);
    let dir = dir()?;
    let path = dir.join(file_name(&key));
    json_file::write_atomic(
        &dir,
        &path,
        &PromptFile {
            worktree: key,
            prompt: prompt.to_string(),
        },
    )
}

/// Take (read and remove) the prompt queued for the session rooted at
/// `worktree`, or `None` when none is queued (or the file belongs to a different
/// worktree). Removing it makes the prompt one-shot: a later launch that finds
/// nothing queued starts the agent without one.
pub fn take(worktree: &Path) -> Option<String> {
    let key = key(worktree);
    let path = dir().ok()?.join(file_name(&key));
    match json_file::read::<PromptFile>(&path) {
        // Ours: hand back the prompt and remove the file (one-shot delivery).
        Ok(Some(file)) if file.worktree.as_path() == key => {
            let _ = fs::remove_file(&path);
            Some(file.prompt)
        }
        // A parseable file stamped with a different worktree: a hash collision
        // or a leftover synced from another machine. It belongs to that
        // worktree, so leave it untouched for its rightful owner to take.
        Ok(Some(_)) => None,
        // Either nothing is queued, or the file is corrupt/unparseable. A
        // corrupt file can never be delivered to anyone, so clear it; a missing
        // file is a no-op to remove.
        Ok(None) => None,
        Err(_) => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn set_replaces_a_previously_queued_prompt() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), "first").unwrap();
            set(wt.path(), "second").unwrap();
            assert_eq!(take(wt.path()), Some("second".to_string()));
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
            let dir = dir().unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &PromptFile {
                    worktree: key(other.path()),
                    prompt: "not ours".to_string(),
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
            let dir = dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "not json at all").unwrap();
            // It reads as absent, and the unparseable file is cleared.
            assert_eq!(take(wt.path()), None);
            assert!(!path.exists());
        });
    }

    #[test]
    fn file_name_is_stable_and_hex() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = key(dir.path());
        let name = file_name(&canonical);
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(name, file_name(&canonical));
    }

    #[test]
    fn key_falls_back_to_the_given_path_when_unresolvable() {
        let missing = Path::new("/usagi/does/not/exist");
        assert_eq!(key(missing), missing.to_path_buf());
    }
}
