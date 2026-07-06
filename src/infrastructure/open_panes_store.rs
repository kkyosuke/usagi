//! Per-worktree storage of a session's open panes, so they can be restored on
//! the next startup.
//!
//! When usagi quits, each session's live panes (an agent, one or more terminals)
//! are dropped with the process. To bring them back on the next launch — an agent
//! picking its conversation back up via the CLI's resume flag — the home screen
//! snapshots, per worktree, which panes were open and writes it here on quit; on
//! startup it reads each session's snapshot and re-spawns the panes in the
//! background. Gated by [`Settings::restore_panes_enabled`].
//!
//! [`Settings::restore_panes_enabled`]: crate::domain::settings::Settings::restore_panes_enabled
//!
//! Like [`super::agent_prompt_store`], the file is addressed purely from the
//! worktree directory: its canonical form hashed to a stable, filesystem-safe
//! name under `<data-dir>/open-panes/` (the addressing is shared via
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also records the
//! worktree it belongs to, so a hashed-name collision (or a stale file synced
//! from another machine) is detected and read as absent rather than
//! misattributed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::settings::AgentCli;
use crate::infrastructure::worktree_keyed_store::{
    self, dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

/// Subdirectory of the data dir the open-pane snapshots live under.
const OPEN_PANES_SUBDIR: &str = "open-panes";

/// Which kind of pane a [`StoredPane`] records. Mirrors the home screen's
/// `PaneKind`, kept here as the infrastructure-layer DTO so the store does not
/// depend on the presentation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredPaneKind {
    /// An AI agent pane (its [`StoredPane::cli`] names which agent launched it).
    Agent,
    /// A plain interactive terminal pane.
    Terminal,
}

/// One open pane in a session, as persisted for restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPane {
    /// Whether the pane ran an agent or a plain shell.
    pub kind: StoredPaneKind,
    /// For an agent pane, which CLI it ran — so restore relaunches the same agent
    /// (and resume matches the tool that created the conversation). `None` for a
    /// terminal pane, which has no agent to resume.
    #[serde(default)]
    pub cli: Option<AgentCli>,
    /// Optional user-facing label override shown on the tab chip. `None` means
    /// use the kind-derived default (`agent`, `terminal 2`, ...).
    #[serde(default)]
    pub label: Option<String>,
}

/// A session's open-pane snapshot: the panes in tab order plus which one was
/// active. The `worktree` it belongs to is recorded for collision detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenPanes {
    /// The worktree this snapshot belongs to. A read whose recorded worktree
    /// differs from the one asked for is treated as absent (hash collision / a
    /// file synced from another machine), so it is never misattributed.
    pub worktree: PathBuf,
    /// The index into `panes` of the pane that was active (clamped on restore).
    pub active: usize,
    /// The session's panes, in tab order.
    pub panes: Vec<StoredPane>,
}

impl WorktreeStamped for OpenPanes {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

/// Persist the open panes of the session rooted at `worktree`, replacing any
/// previous snapshot. Saving an empty `panes` list clears the snapshot instead,
/// so a session left with no panes is not "restored" into an empty shell.
pub fn save(worktree: &Path, active: usize, panes: &[StoredPane]) -> Result<()> {
    if panes.is_empty() {
        clear(worktree);
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(OPEN_PANES_SUBDIR)?;
    let path = dir.join(file_name(&key));
    write_stamped(
        &dir,
        &path,
        &OpenPanes {
            worktree: key,
            active,
            panes: panes.to_vec(),
        },
    )
}

/// Read the open-pane snapshot of the session rooted at `worktree`, or `None`
/// when none is stored (or the file belongs to a different worktree). A corrupt
/// file reads as absent and is cleared.
pub fn load(worktree: &Path) -> Option<OpenPanes> {
    let key = key(worktree);
    let path = dir(OPEN_PANES_SUBDIR).ok()?.join(file_name(&key));
    read_ours::<OpenPanes>(&path, &key)
}

/// Remove the open-pane snapshot of the session rooted at `worktree`. A no-op
/// when none is stored.
pub fn clear(worktree: &Path) {
    worktree_keyed_store::clear(OPEN_PANES_SUBDIR, worktree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::json_file;
    use crate::infrastructure::storage;
    use std::fs;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body`.
    fn with_data_dir(body: impl FnOnce()) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body();
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    fn agent(cli: AgentCli) -> StoredPane {
        StoredPane {
            kind: StoredPaneKind::Agent,
            cli: Some(cli),
            label: None,
        }
    }

    fn terminal() -> StoredPane {
        StoredPane {
            kind: StoredPaneKind::Terminal,
            cli: None,
            label: None,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            assert_eq!(load(wt.path()), None);
            let panes = vec![agent(AgentCli::Claude), terminal()];
            save(wt.path(), 1, &panes).unwrap();
            let loaded = load(wt.path()).expect("snapshot present");
            assert_eq!(loaded.active, 1);
            assert_eq!(loaded.panes, panes);
            assert_eq!(loaded.worktree, key(wt.path()));
        });
    }

    #[test]
    fn save_replaces_a_previous_snapshot() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            save(wt.path(), 0, &[agent(AgentCli::Claude)]).unwrap();
            save(wt.path(), 0, &[terminal(), terminal()]).unwrap();
            let loaded = load(wt.path()).unwrap();
            assert_eq!(loaded.panes, vec![terminal(), terminal()]);
        });
    }

    #[test]
    fn saving_no_panes_clears_the_snapshot() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            save(wt.path(), 0, &[terminal()]).unwrap();
            assert!(load(wt.path()).is_some());
            // An empty save clears it rather than restoring into an empty shell.
            save(wt.path(), 0, &[]).unwrap();
            assert_eq!(load(wt.path()), None);
        });
    }

    #[test]
    fn clear_removes_the_snapshot_and_is_a_noop_when_absent() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            // Clearing a missing snapshot is harmless.
            clear(wt.path());
            save(wt.path(), 0, &[terminal()]).unwrap();
            clear(wt.path());
            assert_eq!(load(wt.path()), None);
        });
    }

    #[test]
    fn distinct_worktrees_store_independently() {
        with_data_dir(|| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            save(a.path(), 0, &[agent(AgentCli::Codex)]).unwrap();
            save(b.path(), 0, &[terminal()]).unwrap();
            assert_eq!(load(a.path()).unwrap().panes, vec![agent(AgentCli::Codex)]);
            assert_eq!(load(b.path()).unwrap().panes, vec![terminal()]);
        });
    }

    #[test]
    fn a_file_stored_for_another_worktree_reads_as_absent_and_is_preserved() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at wt's hashed name but stamped with a different
            // worktree, as a hash collision or a synced stale file would be.
            let dir = dir(OPEN_PANES_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &OpenPanes {
                    worktree: key(other.path()),
                    active: 0,
                    panes: vec![terminal()],
                },
            )
            .unwrap();
            // Not returned for wt, and left intact for its rightful owner.
            assert_eq!(load(wt.path()), None);
            assert!(path.exists());
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_and_is_cleared() {
        with_data_dir(|| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(OPEN_PANES_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "not json at all").unwrap();
            assert_eq!(load(wt.path()), None);
            assert!(!path.exists());
        });
    }
}
