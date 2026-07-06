//! Per-worktree storage of the running agent's lifecycle phase.
//!
//! When usagi launches an agent CLI it wires in lifecycle hooks (see
//! [`crate::domain::settings::AgentCli::launch_command`]). Each hook runs
//! `usagi agent-phase <phase>`, which records the new
//! [`AgentPhase`] for the worktree the agent is running in. The home screen's
//! session watcher ([`crate::presentation::tui::home::terminal::pool`]) reads it
//! back each tick to drive the running / waiting indicator.
//!
//! The writer (a hook process) and the reader (the watcher) never share memory,
//! so they agree on a file path purely from the worktree directory: the path's
//! canonical form hashed to a stable, filesystem-safe name under
//! `<data-dir>/agent-state/` (the addressing is shared with the prompt store in
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also stores the
//! worktree it belongs to, so a hash collision (or a stale file from another
//! machine syncing the data dir) is detected and ignored rather than
//! misattributed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::agent_phase::AgentPhase;
use crate::infrastructure::worktree_keyed_store::{
    self, dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

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

impl WorktreeStamped for PhaseFile {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

/// Record the agent `phase` for the session rooted at `worktree`.
pub fn write(worktree: &Path, phase: AgentPhase) -> Result<()> {
    let key = key(worktree);
    let dir = dir(STATE_SUBDIR)?;
    let path = dir.join(file_name(&key));
    write_stamped(
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
///
/// Canonicalizes `worktree` once (the previous implementation did so twice). For
/// the home screen's per-tick polling prefer [`PhaseReader`], which additionally
/// caches by the file's mtime so an unchanged file is not re-read or re-parsed.
pub fn read(worktree: &Path) -> Option<AgentPhase> {
    let key = key(worktree);
    let path = dir(STATE_SUBDIR).ok()?.join(file_name(&key));
    read_phase_file(&path, &key)
}

/// Read and validate the phase file at `path`, where `key` is the canonical
/// worktree it must belong to. `None` when the file is absent/unreadable or was
/// recorded for a different worktree (a hashed-name collision or stale file).
fn read_phase_file(path: &Path, key: &Path) -> Option<AgentPhase> {
    read_ours::<PhaseFile>(path, key).map(|file| file.phase)
}

/// The file's last-modified time, or `None` when it is absent or unstattable —
/// the change signal [`PhaseReader`] compares to decide whether to re-read.
fn current_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
}

/// A stateful reader of phase files with an mtime cache, for the home screen's
/// session watcher which polls every session every tick (see
/// [`crate::presentation::tui::home::terminal::pool`]).
///
/// Each call stats the file for its mtime and returns the cached parse while the
/// file is unchanged, so an idle session costs one `stat` per tick instead of a
/// full read + JSON parse. The resolved file path (and the worktree's canonical
/// form) is cached too, so the worktree is canonicalized only the first time it
/// is seen rather than on every tick.
#[derive(Default)]
pub struct PhaseReader {
    cache: RefCell<HashMap<PathBuf, Cached>>,
}

/// A cached phase-file read: where the file is, the worktree it must belong to,
/// the mtime it was last read at (`None` when the file was absent), and the
/// phase that yielded.
struct Cached {
    path: PathBuf,
    key: PathBuf,
    mtime: Option<SystemTime>,
    phase: Option<AgentPhase>,
}

impl PhaseReader {
    /// A reader with an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded phase for `worktree`, served from the cache while the phase
    /// file's mtime is unchanged since the last read.
    pub fn read(&self, worktree: &Path) -> Option<AgentPhase> {
        let mut cache = self.cache.borrow_mut();
        // A worktree already cached reuses its resolved path/key — avoiding both
        // the canonicalising syscall and a clone of the two — and is served
        // straight back while the phase file's mtime is unchanged; only a changed
        // mtime re-reads the file and refreshes the entry in place.
        if let Some(cached) = cache.get_mut(worktree) {
            let mtime = current_mtime(&cached.path);
            if cached.mtime != mtime {
                cached.phase = read_phase_file(&cached.path, &cached.key);
                cached.mtime = mtime;
            }
            return cached.phase;
        }
        // First sighting: resolve the file path once, read it, and cache the lot.
        let key = key(worktree);
        let path = dir(STATE_SUBDIR).ok()?.join(file_name(&key));
        let mtime = current_mtime(&path);
        let phase = read_phase_file(&path, &key);
        cache.insert(
            worktree.to_path_buf(),
            Cached {
                path,
                key,
                mtime,
                phase,
            },
        );
        phase
    }

    /// Drop every cached entry whose worktree `keep` rejects. The session
    /// watcher calls this each tick with its live sessions, so a cache entry
    /// left behind by a removed session is released rather than accumulating —
    /// otherwise the cache grows unbounded across a long run that creates and
    /// removes many sessions ([`clear`] only deletes the on-disk file, not this
    /// in-memory entry).
    pub fn retain(&self, keep: impl Fn(&Path) -> bool) {
        self.cache.borrow_mut().retain(|path, _| keep(path));
    }
}

/// Forget any recorded phase for `worktree` (best-effort), so a session freshly
/// spawned there does not inherit a previous run's phase.
pub fn clear(worktree: &Path) {
    worktree_keyed_store::clear(STATE_SUBDIR, worktree);
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

/// Extract the file path a tool is about to touch from a Claude Code
/// `PreToolUse` hook payload: `tool_input.file_path` (the field Read / Edit /
/// Write carry). Returns `None` when the payload is not JSON, carries no
/// `tool_input`, or the tool has no `file_path` (e.g. `Bash`, `Grep`), so the
/// caller treats those calls as nothing to guard. Used by
/// [`crate::presentation::cli::guard_workspace`] to confine the agent to its
/// session worktree.
pub fn tool_path_from_hook_json(raw: &str) -> Option<PathBuf> {
    /// The nested fields usagi reads from a `PreToolUse` payload.
    #[derive(Deserialize)]
    struct HookInput {
        tool_input: Option<ToolInput>,
    }
    #[derive(Deserialize)]
    struct ToolInput {
        file_path: Option<PathBuf>,
    }
    serde_json::from_str::<HookInput>(raw)
        .ok()?
        .tool_input?
        .file_path
}

/// Extract the tool name a Claude Code `PreToolUse` hook is about to run:
/// `tool_name` (e.g. `"Edit"`, `"Write"`, `"Bash"`). Returns `None` when the
/// payload is not JSON or carries no `tool_name`. Used by
/// [`crate::presentation::cli::guard_workspace`]'s root mode to deny
/// file-writing tools regardless of their target path.
pub fn tool_name_from_hook_json(raw: &str) -> Option<String> {
    /// The single field usagi reads from a `PreToolUse` payload here.
    #[derive(Deserialize)]
    struct HookInput {
        tool_name: Option<String>,
    }
    serde_json::from_str::<HookInput>(raw).ok()?.tool_name
}

/// Extract the shell command a `Bash` tool call is about to run from a Claude
/// Code `PreToolUse` hook payload: `tool_input.command`. Returns `None` when the
/// payload is not JSON, carries no `tool_input`, or the tool has no `command`
/// (every non-`Bash` tool). Used by
/// [`crate::presentation::cli::guard_workspace`]'s root mode to inspect the git
/// subcommand a `Bash` call would run.
pub fn bash_command_from_hook_json(raw: &str) -> Option<String> {
    /// The nested fields usagi reads from a `Bash` `PreToolUse` payload.
    #[derive(Deserialize)]
    struct HookInput {
        tool_input: Option<ToolInput>,
    }
    #[derive(Deserialize)]
    struct ToolInput {
        command: Option<String>,
    }
    serde_json::from_str::<HookInput>(raw)
        .ok()?
        .tool_input?
        .command
}

/// Extract the `source` of a Claude Code `SessionStart` hook from its JSON
/// payload: `"startup"`, `"resume"`, `"clear"`, or `"compact"`. Returns `None`
/// when the payload is not JSON or carries no `source` (every non-`SessionStart`
/// hook).
///
/// usagi cares about `"compact"`: `SessionStart` fires not only when a session
/// begins but also after the context is compacted, which auto-compaction can do
/// **mid-turn** — the agent keeps working afterwards without a fresh
/// `UserPromptSubmit`. Treating that as the usual `SessionStart` → ready would
/// reset a still-working session to idle until its next `Stop`, leaving it stuck
/// showing ready while it works (see [`crate::presentation::cli::agent_phase`]).
pub fn session_start_source_from_hook_json(raw: &str) -> Option<String> {
    /// The single field usagi reads from a `SessionStart` payload.
    #[derive(Deserialize)]
    struct HookInput {
        source: Option<String>,
    }
    serde_json::from_str::<HookInput>(raw).ok()?.source
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::json_file;
    use crate::infrastructure::storage;

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
    fn phase_reader_serves_and_refreshes_across_an_mtime_change() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let reader = PhaseReader::new();

            // Absent file: reads as None and caches that absence.
            assert_eq!(reader.read(wt.path()), None);

            // Recording a phase creates the file, so its mtime now differs from
            // the cached "absent" state and the reader re-reads the new value.
            write(wt.path(), AgentPhase::Running).unwrap();
            assert_eq!(reader.read(wt.path()), Some(AgentPhase::Running));

            // A second read with the file unchanged is served from the cache.
            assert_eq!(reader.read(wt.path()), Some(AgentPhase::Running));
        });
    }

    #[test]
    fn phase_reader_ignores_a_file_recorded_for_another_worktree() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let dir = dir(STATE_SUBDIR).unwrap();
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
            assert_eq!(PhaseReader::new().read(wt.path()), None);
        });
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
    fn retain_drops_cache_entries_the_predicate_rejects() {
        with_data_dir(|_| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            write(a.path(), AgentPhase::Running).unwrap();
            write(b.path(), AgentPhase::Waiting).unwrap();
            let reader = PhaseReader::new();
            // Populate the cache with both worktrees.
            assert_eq!(reader.read(a.path()), Some(AgentPhase::Running));
            assert_eq!(reader.read(b.path()), Some(AgentPhase::Waiting));

            // Keep only `a`, dropping `b` — exercises the predicate returning
            // both true and false. Both worktrees still read correctly after:
            // `a` from the retained entry, `b` re-resolved from disk.
            let keep_a = a.path().to_path_buf();
            reader.retain(|path| path == keep_a);
            assert_eq!(reader.read(a.path()), Some(AgentPhase::Running));
            assert_eq!(reader.read(b.path()), Some(AgentPhase::Waiting));

            // Retaining nothing empties the cache; a later read re-resolves from
            // disk, so the value is unchanged.
            reader.retain(|_| false);
            assert_eq!(reader.read(a.path()), Some(AgentPhase::Running));
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
            let dir = dir(STATE_SUBDIR).unwrap();
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

    #[test]
    fn tool_path_reads_the_file_path_from_tool_input() {
        let json = r#"{"cwd":"/repo/wt","tool_name":"Edit","tool_input":{"file_path":"/repo/src/main.rs"}}"#;
        assert_eq!(
            tool_path_from_hook_json(json),
            Some(PathBuf::from("/repo/src/main.rs"))
        );
    }

    #[test]
    fn tool_path_is_none_when_the_tool_has_no_file_path_or_on_garbage() {
        // A Bash call carries a `command`, not a `file_path`: nothing to guard.
        assert_eq!(
            tool_path_from_hook_json(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#),
            None
        );
        // No tool_input at all.
        assert_eq!(tool_path_from_hook_json(r#"{"cwd":"/repo/wt"}"#), None);
        // Not JSON at all.
        assert_eq!(tool_path_from_hook_json("not json"), None);
    }

    #[test]
    fn tool_name_reads_the_tool_name_field() {
        let json =
            r#"{"cwd":"/repo","tool_name":"Edit","tool_input":{"file_path":"/repo/src/main.rs"}}"#;
        assert_eq!(tool_name_from_hook_json(json), Some("Edit".to_string()));
    }

    #[test]
    fn tool_name_is_none_without_a_tool_name_or_on_garbage() {
        assert_eq!(tool_name_from_hook_json(r#"{"cwd":"/repo"}"#), None);
        assert_eq!(tool_name_from_hook_json("not json"), None);
    }

    #[test]
    fn bash_command_reads_the_command_from_tool_input() {
        let json = r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
        assert_eq!(
            bash_command_from_hook_json(json),
            Some("git status".to_string())
        );
    }

    #[test]
    fn bash_command_is_none_when_the_tool_has_no_command_or_on_garbage() {
        // An Edit call carries a `file_path`, not a `command`.
        assert_eq!(
            bash_command_from_hook_json(r#"{"tool_name":"Edit","tool_input":{"file_path":"/x"}}"#),
            None
        );
        // No tool_input at all.
        assert_eq!(bash_command_from_hook_json(r#"{"cwd":"/repo"}"#), None);
        // Not JSON at all.
        assert_eq!(bash_command_from_hook_json("not json"), None);
    }

    #[test]
    fn session_start_source_reads_the_source_field() {
        // A SessionStart payload carries the source that started/resumed it.
        let json = r#"{"cwd":"/repo/wt","hook_event_name":"SessionStart","source":"compact"}"#;
        assert_eq!(
            session_start_source_from_hook_json(json),
            Some("compact".to_string())
        );
        assert_eq!(
            session_start_source_from_hook_json(r#"{"source":"startup"}"#),
            Some("startup".to_string())
        );
    }

    #[test]
    fn session_start_source_is_none_without_a_source_or_on_garbage() {
        // Hooks other than SessionStart carry no source.
        assert_eq!(
            session_start_source_from_hook_json(r#"{"cwd":"/repo/wt","hook_event_name":"Stop"}"#),
            None
        );
        // Not JSON at all.
        assert_eq!(session_start_source_from_hook_json("not json"), None);
    }
}
