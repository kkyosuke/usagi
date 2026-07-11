//! Per-workspace storage of the engagement the home screen was at when usagi
//! quit, so the next launch can drop the user back where they left off.
//!
//! Where [`super::open_panes_store`] records *which panes* each session had open
//! (so they re-spawn on the next launch), this records *where the user was* — the
//! session they were on and how deeply they were engaged with it (選択 / 集中 /
//! 没入). On quit the home screen writes it; on startup it reads the snapshot and,
//! after the panes are restored, moves the cursor to that session, focuses it, or
//! re-attaches its pane. Gated by the same [`Settings::restore_panes_enabled`]
//! setting as the pane restore, since the two together are one "restore my
//! session state" feature.
//!
//! [`Settings::restore_panes_enabled`]: crate::domain::settings::Settings::restore_panes_enabled
//!
//! Unlike the pane snapshot — keyed per *worktree* — this is one fact per
//! *workspace*, so it is addressed from the workspace root directory: its
//! canonical form hashed to a stable, filesystem-safe name under
//! `<data-dir>/resume-focus/` (the addressing is shared via
//! [`crate::infrastructure::worktree_keyed_store`]). The file also records the
//! workspace it belongs to, so a hashed-name collision (or a stale file synced
//! from another machine) is detected and read as absent rather than
//! misattributed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::worktree_keyed_store::{
    dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

/// Subdirectory of the data dir the resume-focus snapshots live under.
const RESUME_FOCUS_SUBDIR: &str = "resume-focus";

/// How deeply the user was engaged with the recorded session at quit — the home
/// screen's engagement ladder. The infrastructure-layer DTO mirroring the
/// presentation `Mode`, kept here so the store does not depend on the
/// presentation layer (as [`super::open_panes_store::StoredPaneKind`] mirrors the
/// pane kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredEngagement {
    /// 選択 (Overview): the session picker, with the cursor on the session. The
    /// `switch` alias reads snapshots written before the mode was renamed.
    #[serde(alias = "switch")]
    Overview,
    /// 集中 (Closeup): the session's right-pane action surface. The `focus` alias
    /// reads snapshots written before the mode was renamed.
    #[serde(alias = "focus")]
    Closeup,
    /// 没入 (Attached): an embedded pane was live and driven.
    Attached,
}

/// A workspace's resume-focus snapshot: the session the user was on and how
/// deeply they were engaged with it. The `workspace` it belongs to is recorded
/// for collision detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeFocus {
    /// The workspace this snapshot belongs to. A read whose recorded workspace
    /// differs from the one asked for is treated as absent (hash collision / a
    /// file synced from another machine), so it is never misattributed.
    pub workspace: PathBuf,
    /// The session the cursor was on — a branch name, or the root row's name for
    /// the workspace root. Resolved back to a row on restore; a name that no
    /// longer exists (the session was removed) restores nothing.
    pub session: String,
    /// How deeply the session was engaged with at quit.
    pub engagement: StoredEngagement,
}

impl WorktreeStamped for ResumeFocus {
    fn stamped(&self) -> &Path {
        &self.workspace
    }
}

/// Persist the resume focus for the workspace rooted at `workspace`, replacing
/// any previous snapshot.
pub fn save(workspace: &Path, session: &str, engagement: StoredEngagement) -> Result<()> {
    let key = key(workspace);
    let dir = dir(RESUME_FOCUS_SUBDIR)?;
    let path = dir.join(file_name(&key));
    write_stamped(
        &dir,
        &path,
        &ResumeFocus {
            workspace: key,
            session: session.to_string(),
            engagement,
        },
    )
}

/// Read the resume-focus snapshot for the workspace rooted at `workspace`, or
/// `None` when none is stored (or the file belongs to a different workspace). A
/// corrupt file reads as absent and is cleared.
pub fn load(workspace: &Path) -> Option<ResumeFocus> {
    let key = key(workspace);
    let path = dir(RESUME_FOCUS_SUBDIR).ok()?.join(file_name(&key));
    read_ours::<ResumeFocus>(&path, &key)
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

    #[test]
    fn save_then_load_round_trips() {
        with_data_dir(|| {
            let ws = tempfile::tempdir().unwrap();
            assert_eq!(load(ws.path()), None);
            save(ws.path(), "feature", StoredEngagement::Attached).unwrap();
            let loaded = load(ws.path()).expect("snapshot present");
            assert_eq!(loaded.session, "feature");
            assert_eq!(loaded.engagement, StoredEngagement::Attached);
            assert_eq!(loaded.workspace, key(ws.path()));
        });
    }

    #[test]
    fn save_replaces_a_previous_snapshot() {
        with_data_dir(|| {
            let ws = tempfile::tempdir().unwrap();
            save(ws.path(), "feature", StoredEngagement::Overview).unwrap();
            save(ws.path(), "root", StoredEngagement::Closeup).unwrap();
            let loaded = load(ws.path()).unwrap();
            assert_eq!(loaded.session, "root");
            assert_eq!(loaded.engagement, StoredEngagement::Closeup);
        });
    }

    #[test]
    fn distinct_workspaces_store_independently() {
        with_data_dir(|| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            save(a.path(), "one", StoredEngagement::Overview).unwrap();
            save(b.path(), "two", StoredEngagement::Attached).unwrap();
            assert_eq!(load(a.path()).unwrap().session, "one");
            assert_eq!(load(b.path()).unwrap().session, "two");
        });
    }

    #[test]
    fn a_file_stored_for_another_workspace_reads_as_absent_and_is_preserved() {
        with_data_dir(|| {
            let ws = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at ws's hashed name but stamped with a different
            // workspace, as a hash collision or a synced stale file would be.
            let dir = dir(RESUME_FOCUS_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(ws.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &ResumeFocus {
                    workspace: key(other.path()),
                    session: "feature".to_string(),
                    engagement: StoredEngagement::Overview,
                },
            )
            .unwrap();
            // Not returned for ws, and left intact for its rightful owner.
            assert_eq!(load(ws.path()), None);
            assert!(path.exists());
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_and_is_cleared() {
        with_data_dir(|| {
            let ws = tempfile::tempdir().unwrap();
            let dir = dir(RESUME_FOCUS_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(ws.path())));
            fs::write(&path, "not json at all").unwrap();
            assert_eq!(load(ws.path()), None);
            assert!(!path.exists());
        });
    }

    #[test]
    fn legacy_switch_focus_engagements_deserialize_via_aliases() {
        // Snapshots written before the 選択/集中 rename stored the engagement as
        // `"switch"` / `"focus"`; the serde aliases keep those readable so a user
        // upgrading is dropped back where they left off rather than at the base.
        let overview: StoredEngagement = serde_json::from_str("\"switch\"").unwrap();
        assert_eq!(overview, StoredEngagement::Overview);
        let closeup: StoredEngagement = serde_json::from_str("\"focus\"").unwrap();
        assert_eq!(closeup, StoredEngagement::Closeup);
        // The current names round-trip too.
        assert_eq!(
            serde_json::to_string(&StoredEngagement::Overview).unwrap(),
            "\"overview\""
        );
        assert_eq!(
            serde_json::to_string(&StoredEngagement::Closeup).unwrap(),
            "\"closeup\""
        );
    }
}
