//! Per-worktree marker recording that a **live agent pane** is currently open for
//! a session — the authoritative signal for "the live channel has a consumer".
//!
//! The MCP `session_prompt` tool's `auto` mode, and its `live`-channel delivery
//! confirmation, need to know whether a running usagi TUI actually has a live
//! agent pane for a worktree, because only such a TUI drains the live queue
//! ([`super::agent_live_prompt_store`]) and types it into the pane. The MCP server
//! runs in a separate `usagi mcp` process that shares no memory with the TUI, so
//! the two agree on a file path purely from the worktree directory (the addressing
//! is shared via [`super::worktree_keyed_store`], exactly like the phase and
//! prompt stores).
//!
//! Why a dedicated marker rather than reusing the agent-phase file
//! ([`super::agent_state_store`]): the phase file records the agent's *lifecycle
//! phase* (`ready` while idle, before its first turn), which is not the same as
//! "a live pane exists that can receive a live prompt" — and, crucially, it is
//! cleared only when a *running* TUI watcher observes the pane die. A TUI that
//! quits or crashes leaves a stale `ready`/`running` file behind, so reading it as
//! liveness makes `auto` pick the live channel (and the tool report `live`) when
//! nothing is draining the queue, stranding the prompt. This marker instead stamps
//! the **process id of the TUI** that owns the live pane: a reader treats the
//! marker as live only while that process is still alive, so a crashed TUI's marker
//! reads as dead (self-healing) even though its `Drop` never ran to clear it.
//!
//! Like the sibling stores, each file also records the worktree it belongs to, so
//! a hashed-name collision (or a stale file synced from another machine) is
//! detected and read as absent rather than misattributed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::json_file;
use crate::infrastructure::worktree_keyed_store::{dir, file_name, key, path_for};

/// Subdirectory of the data dir the live-pane markers live under.
const MARKER_SUBDIR: &str = "agent-live-panes";

/// On-disk shape of a worktree's live-pane marker.
#[derive(Serialize, Deserialize)]
struct MarkerFile {
    /// The worktree this marker belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The process id of the TUI that currently hosts the live agent pane. A
    /// reader treats the marker as live only while this process is still alive, so
    /// a marker left by a crashed TUI (whose `Drop` never cleared it) reads as
    /// dead rather than stranding prompts on a queue no one drains.
    pid: u32,
}

/// Record that the TUI process `pid` currently has a live agent pane for the
/// session rooted at `worktree`. Called by the terminal pool whenever it (re)computes
/// a session's watched handles and finds an agent pane present.
pub fn set(worktree: &Path, pid: u32) -> Result<()> {
    let key = key(worktree);
    let dir = dir(MARKER_SUBDIR)?;
    let path = dir.join(file_name(&key));
    json_file::write_atomic(&dir, &path, &MarkerFile { worktree: key, pid })
}

/// Whether a live agent pane currently exists for the session rooted at
/// `worktree`: a marker is present, stamped for this worktree, and the TUI process
/// it names is still alive per `pid_alive`. A marker naming a dead process is
/// stale (a TUI that crashed without clearing it) and is removed as a side effect,
/// so a later read need not re-check the same dead pid.
///
/// `pid_alive` is injected so the decision logic is unit-tested without a real
/// process table; the production caller passes
/// [`super::resource::process_alive`]. It is a plain `fn` pointer (not a generic
/// `impl Fn`) on purpose: a generic here would be monomorphized separately in the
/// binary crate that calls it (`main.rs`), and that binary-side instance — never
/// exercised by these lib tests — would show as uncovered under
/// `cargo llvm-cov --workspace` (the same double-build trap `scripts/coverage.sh`
/// documents). A single concrete function is compiled once and linked from both.
pub fn is_live(worktree: &Path, pid_alive: fn(u32) -> bool) -> bool {
    match read_marker(worktree) {
        // Ours and the owning TUI is still running: a live consumer exists.
        Some((_, pid)) if pid_alive(pid) => true,
        // Ours but the owning TUI is gone: the marker is stale. Drop it so the
        // stale pid is not re-checked, and report no live pane.
        Some((path, _)) => {
            let _ = std::fs::remove_file(&path);
            false
        }
        // No marker, unreadable, or one stamped for another worktree (hash
        // collision / synced stale file) — nothing of ours says a pane is live.
        None => false,
    }
}

/// Read this worktree's marker, returning its file path and recorded pid when the
/// file is present and stamped for this worktree, else `None` (absent, unreadable,
/// or a collision stamped for a different worktree — left untouched for its owner).
fn read_marker(worktree: &Path) -> Option<(PathBuf, u32)> {
    let key = key(worktree);
    let path = path_for(MARKER_SUBDIR, worktree).ok()?;
    let marker: MarkerFile = json_file::read(&path).ok()??;
    (marker.worktree.as_path() == key).then_some((path, marker.pid))
}

/// Forget any live-pane marker for `worktree` (best-effort), called when the pool
/// loses the session's agent pane (it closed, the session was removed, or the TUI
/// is tearing down) so `auto` mode stops resolving to the live channel.
pub fn clear(worktree: &Path) {
    if let Ok(path) = path_for(MARKER_SUBDIR, worktree) {
        let _ = std::fs::remove_file(path);
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

    #[test]
    fn absent_marker_reads_as_not_live() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing recorded, and the predicate is never consulted.
            assert!(!is_live(wt.path(), |_| panic!("pid check not expected")));
        });
    }

    #[test]
    fn a_marker_for_a_live_process_reads_as_live() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), 4321).unwrap();
            assert!(is_live(wt.path(), |pid| pid == 4321));
        });
    }

    #[test]
    fn a_marker_for_a_dead_process_reads_as_not_live_and_is_removed() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), 4321).unwrap();
            let path = path_for(MARKER_SUBDIR, wt.path()).unwrap();
            assert!(path.exists());
            // The owning TUI is gone, so the marker is stale: not live, and cleared.
            assert!(!is_live(wt.path(), |_| false));
            assert!(!path.exists());
            // A later read finds nothing (and never re-checks the dead pid).
            assert!(!is_live(wt.path(), |_| panic!("pid check not expected")));
        });
    }

    #[test]
    fn set_overwrites_the_recorded_pid() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), 1).unwrap();
            set(wt.path(), 2).unwrap();
            // The newest pid wins: the marker is read as live only when pid 2 (not
            // the overwritten pid 1) is the process reported alive.
            assert!(is_live(wt.path(), |pid| pid == 2));
        });
    }

    #[test]
    fn clear_forgets_the_marker() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            set(wt.path(), 7).unwrap();
            clear(wt.path());
            assert!(!is_live(wt.path(), |_| panic!("pid check not expected")));
            // Clearing again (nothing recorded) is a harmless no-op.
            clear(wt.path());
        });
    }

    #[test]
    fn distinct_worktrees_are_tracked_independently() {
        with_data_dir(|| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            set(a.path(), 100).unwrap();
            // Only `a` has a marker; `b` reads as not live without consulting the pid.
            assert!(is_live(a.path(), |pid| pid == 100));
            assert!(!is_live(b.path(), |_| true));
        });
    }

    #[test]
    fn a_marker_stamped_for_another_worktree_reads_as_absent_and_is_preserved() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at wt's hashed name but stamped with a different
            // worktree, as a hash collision or a synced stale file would be.
            let dir = dir(MARKER_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &MarkerFile {
                    worktree: key(other.path()),
                    pid: 55,
                },
            )
            .unwrap();
            // Not read as ours, the pid is never consulted, and the file is left
            // intact for its rightful owner.
            assert!(!is_live(wt.path(), |_| panic!("pid check not expected")));
            assert!(path.exists());
        });
    }

    #[test]
    fn a_corrupt_marker_reads_as_absent() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(MARKER_SUBDIR).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            std::fs::write(&path, "not json at all").unwrap();
            assert!(!is_live(wt.path(), |_| panic!("pid check not expected")));
        });
    }
}
