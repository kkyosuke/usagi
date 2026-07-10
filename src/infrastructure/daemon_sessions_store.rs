//! The daemon's snapshot of the sessions it monitors, under
//! `<data-dir>/daemon/sessions.json`.
//!
//! The daemon rebuilds this each tick from the shared stores (see
//! [`crate::usecase::daemon::monitor_tick`]) so a separate `usagi daemon status`
//! process — with no shared memory — can report what the daemon currently sees.
//! It is a rebuildable view, not a source of truth: an absent or stale file just
//! reads as "no sessions", and the daemon overwrites it on its next change.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::daemon::SessionSnapshot;
use crate::infrastructure::json_file;

/// File name of the monitored-sessions snapshot.
const SESSIONS_FILE: &str = "sessions.json";

/// The snapshot payload written to disk, borrowing the slice so the caller never
/// clones it into an owned wrapper just to serialize.
#[derive(Serialize)]
struct SessionsRef<'a> {
    sessions: &'a [SessionSnapshot],
}

/// The snapshot payload as read back.
#[derive(Deserialize)]
struct SessionsOwned {
    sessions: Vec<SessionSnapshot>,
}

/// Read the monitored-sessions snapshot under `dir`, returning an empty list when
/// the daemon has not written one yet.
pub fn read(dir: &Path) -> Result<Vec<SessionSnapshot>> {
    Ok(
        json_file::read_versioned::<SessionsOwned>(&dir.join(SESSIONS_FILE))?
            .map(|f| f.sessions)
            .unwrap_or_default(),
    )
}

/// Write (or replace) the monitored-sessions snapshot under `dir`, creating `dir`
/// if needed.
pub fn write(dir: &Path, sessions: &[SessionSnapshot]) -> Result<()> {
    json_file::write_versioned(dir, &dir.join(SESSIONS_FILE), &SessionsRef { sessions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::daemon::SessionActivity;
    use std::path::PathBuf;

    fn snapshot(name: &str, activity: Option<SessionActivity>) -> SessionSnapshot {
        SessionSnapshot {
            workspace: PathBuf::from("/repo"),
            name: name.to_string(),
            activity,
        }
    }

    #[test]
    fn read_is_empty_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("daemon");
        let sessions = vec![
            snapshot("work-a", Some(SessionActivity::Waiting)),
            snapshot("fix-b", None),
        ];
        write(&dir, &sessions).unwrap();
        assert_eq!(read(&dir).unwrap(), sessions);
    }

    #[test]
    fn write_replaces_the_earlier_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, &[snapshot("a", Some(SessionActivity::Running))]).unwrap();
        write(dir, &[snapshot("b", Some(SessionActivity::Done))]).unwrap();
        assert_eq!(
            read(dir).unwrap(),
            vec![snapshot("b", Some(SessionActivity::Done))]
        );
    }
}
