//! Per-worktree storage of the running agent's lifecycle phase.
//!
//! When usagi launches an agent CLI it wires in lifecycle hooks (see
//! [`crate::domain::settings::AgentCli::launch_command`]). Each hook runs
//! `usagi agent-phase <phase>`, which records the new
//! [`AgentPhase`] for the worktree the agent is running in. The home screen's
//! session watcher ([`crate::presentation::tui::home::terminal_pool`]) reads it
//! back each tick to drive the running / waiting indicator.
//!
//! The writer (a hook process) and the reader (the watcher) never share memory,
//! so they agree on a file path purely from the worktree directory: the path's
//! canonical form hashed to a stable, filesystem-safe name under
//! `<data-dir>/agent-state/`. Each file also stores the worktree it belongs to,
//! so a hash collision (or a stale file from another machine syncing the data
//! dir) is detected and ignored rather than misattributed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::agent_phase::AgentPhase;
use crate::infrastructure::{json_file, storage};

/// Subdirectory of the data dir the phase files live under.
const STATE_SUBDIR: &str = "agent-state";

/// On-disk shape of a worktree's phase file.
#[derive(Serialize, Deserialize)]
struct PhaseFile {
    /// The worktree this phase belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The last phase the agent's hooks reported for that worktree.
    phase: AgentPhase,
}

/// The directory phase files live under: `<data-dir>/agent-state/`.
fn dir() -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(STATE_SUBDIR))
}

/// The file name a worktree's phase is stored under: a stable hash of its
/// canonical path rendered as hex, so the writer and reader agree on it without
/// listing the directory. Pure given `canonical`.
fn file_name(canonical: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The key a worktree is stored under: its canonical path, falling back to the
/// path as given when it cannot be resolved (e.g. it no longer exists), so the
/// writer and reader still derive the same name.
fn key(worktree: &Path) -> PathBuf {
    worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf())
}

/// The full path of `worktree`'s phase file.
fn path_for(worktree: &Path) -> Result<PathBuf> {
    Ok(dir()?.join(file_name(&key(worktree))))
}

/// Record the agent `phase` for the session rooted at `worktree`.
pub fn write(worktree: &Path, phase: AgentPhase) -> Result<()> {
    let key = key(worktree);
    let dir = dir()?;
    let path = dir.join(file_name(&key));
    json_file::write_atomic(
        &dir,
        &path,
        &PhaseFile {
            worktree: key,
            phase,
        },
    )
}

/// Read the recorded phase for the session rooted at `worktree`, or `None` when
/// none has been recorded (or the file belongs to a different worktree).
pub fn read(worktree: &Path) -> Option<AgentPhase> {
    let path = path_for(worktree).ok()?;
    let file: PhaseFile = json_file::read(&path).ok()??;
    (file.worktree == key(worktree)).then_some(file.phase)
}

/// Forget any recorded phase for `worktree` (best-effort), so a session freshly
/// spawned there does not inherit a previous run's phase.
pub fn clear(worktree: &Path) {
    if let Ok(path) = path_for(worktree) {
        let _ = std::fs::remove_file(path);
    }
}

/// Extract the worktree directory from a Claude Code hook's JSON payload: its
/// `cwd` field, which is the directory the agent was launched in. Returns `None`
/// when the payload is not JSON or carries no `cwd`, so the caller can fall back.
pub fn worktree_from_hook_json(raw: &str) -> Option<PathBuf> {
    /// The single field usagi reads from a hook payload.
    #[derive(Deserialize)]
    struct HookInput {
        cwd: Option<PathBuf>,
    }
    serde_json::from_str::<HookInput>(raw).ok()?.cwd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body` with it. The
    /// override is cleared afterwards, matching the suite's "unset by default"
    /// baseline (see [`crate::infrastructure::storage`]'s own tests).
    fn with_data_dir(body: impl FnOnce(&Path)) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body(dir.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn write_then_read_round_trips_the_phase() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            assert_eq!(read(wt.path()), None);
            write(wt.path(), AgentPhase::Running).unwrap();
            assert_eq!(read(wt.path()), Some(AgentPhase::Running));
            // A later write overwrites the previous phase.
            write(wt.path(), AgentPhase::Waiting).unwrap();
            assert_eq!(read(wt.path()), Some(AgentPhase::Waiting));
        });
    }

    #[test]
    fn clear_forgets_a_recorded_phase() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            write(wt.path(), AgentPhase::Waiting).unwrap();
            clear(wt.path());
            assert_eq!(read(wt.path()), None);
            // Clearing again (nothing there) is a harmless no-op.
            clear(wt.path());
        });
    }

    #[test]
    fn distinct_worktrees_are_tracked_independently() {
        with_data_dir(|_| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            write(a.path(), AgentPhase::Running).unwrap();
            write(b.path(), AgentPhase::Waiting).unwrap();
            assert_eq!(read(a.path()), Some(AgentPhase::Running));
            assert_eq!(read(b.path()), Some(AgentPhase::Waiting));
        });
    }

    #[test]
    fn a_file_recorded_for_another_worktree_reads_as_absent() {
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
                &PhaseFile {
                    worktree: key(other.path()),
                    phase: AgentPhase::Waiting,
                },
            )
            .unwrap();
            assert_eq!(read(wt.path()), None);
        });
    }

    #[test]
    fn file_name_is_stable_and_hex() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = key(dir.path());
        let name = file_name(&canonical);
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        // Same input → same name; the writer and reader rely on this.
        assert_eq!(name, file_name(&canonical));
    }

    #[test]
    fn key_falls_back_to_the_given_path_when_unresolvable() {
        // A path that does not exist cannot be canonicalized, so it is returned
        // verbatim — the writer and reader still derive a matching name.
        let missing = Path::new("/usagi/does/not/exist");
        assert_eq!(key(missing), missing.to_path_buf());
    }

    #[test]
    fn worktree_from_hook_json_reads_the_cwd() {
        let json = r#"{"session_id":"x","cwd":"/repo/wt","hook_event_name":"Stop"}"#;
        assert_eq!(
            worktree_from_hook_json(json),
            Some(PathBuf::from("/repo/wt"))
        );
    }

    #[test]
    fn worktree_from_hook_json_is_none_without_a_cwd_or_on_garbage() {
        // Valid JSON but no cwd field.
        assert_eq!(worktree_from_hook_json(r#"{"session_id":"x"}"#), None);
        // Not JSON at all.
        assert_eq!(worktree_from_hook_json("not json"), None);
    }
}
