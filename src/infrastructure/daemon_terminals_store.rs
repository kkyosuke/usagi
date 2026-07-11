//! Persisted daemon-owned terminal records, under
//! `<data-dir>/daemon/terminals.json`.
//!
//! The live PTY handles themselves cannot be serialized, but their terminal id,
//! worktree, and shell pid can. A daemon that restarts after an abnormal exit
//! reads this file, keeps only pids that are still alive, and can then recover
//! ownership enough to avoid id reuse and to clean up those process groups on a
//! later deliberate `daemon stop`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::daemon_ipc::TerminalId;
use crate::infrastructure::json_file;
use crate::usecase::daemon_ipc::PersistedTerminal;

/// File name of the terminal registry snapshot.
const TERMINALS_FILE: &str = "terminals.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonTerminalRecord {
    pub terminal: TerminalId,
    pub worktree: PathBuf,
    pub pid: u32,
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Serialize)]
struct TerminalsRef<'a> {
    terminals: &'a [DaemonTerminalRecord],
}

#[derive(Deserialize)]
struct TerminalsOwned {
    terminals: Vec<DaemonTerminalRecord>,
}

impl From<DaemonTerminalRecord> for PersistedTerminal {
    fn from(record: DaemonTerminalRecord) -> Self {
        Self {
            terminal: record.terminal,
            worktree: record.worktree,
            pid: record.pid,
        }
    }
}

impl From<&PersistedTerminal> for DaemonTerminalRecord {
    fn from(record: &PersistedTerminal) -> Self {
        Self {
            terminal: record.terminal,
            worktree: record.worktree.clone(),
            pid: record.pid,
            adopted: true,
        }
    }
}

/// Read persisted terminal records, returning an empty list before the daemon
/// has spawned any terminal.
pub fn read(dir: &Path) -> Result<Vec<DaemonTerminalRecord>> {
    Ok(read_if_present(dir)?.unwrap_or_default())
}

/// Read the persisted terminal registry without collapsing a missing file into
/// an empty registry. That distinction lets callers tell a known-empty registry
/// from an old daemon that never persisted ownership at all.
pub fn read_if_present(dir: &Path) -> Result<Option<Vec<DaemonTerminalRecord>>> {
    Ok(
        json_file::read_versioned::<TerminalsOwned>(&dir.join(TERMINALS_FILE))?
            .map(|file| file.terminals),
    )
}

/// Replace the terminal registry snapshot.
pub fn write(dir: &Path, terminals: &[DaemonTerminalRecord]) -> Result<()> {
    json_file::write_versioned(dir, &dir.join(TERMINALS_FILE), &TerminalsRef { terminals })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: TerminalId, adopted: bool) -> DaemonTerminalRecord {
        DaemonTerminalRecord {
            terminal: id,
            worktree: PathBuf::from(format!("/repo/{id}")),
            pid: id as u32 + 100,
            adopted,
        }
    }

    #[test]
    fn read_is_empty_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read(tmp.path()).unwrap().is_empty());
        assert_eq!(read_if_present(tmp.path()).unwrap(), None);
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("daemon");
        let terminals = vec![record(1, false), record(2, true)];
        write(&dir, &terminals).unwrap();
        assert_eq!(read(&dir).unwrap(), terminals);
        assert_eq!(read_if_present(&dir).unwrap(), Some(terminals));
    }

    #[test]
    fn write_replaces_the_earlier_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), &[record(1, false)]).unwrap();
        write(tmp.path(), &[record(2, true)]).unwrap();
        assert_eq!(read(tmp.path()).unwrap(), vec![record(2, true)]);
    }

    #[test]
    fn adopted_defaults_to_false_for_old_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(TERMINALS_FILE);
        crate::infrastructure::json_file::write_text_atomic(
            &path,
            r#"{"version":1,"terminals":[{"terminal":7,"worktree":"/repo","pid":77}]}"#,
        )
        .unwrap();
        assert_eq!(
            read(tmp.path()).unwrap(),
            vec![DaemonTerminalRecord {
                terminal: 7,
                worktree: PathBuf::from("/repo"),
                pid: 77,
                adopted: false,
            }]
        );
    }

    #[test]
    fn owned_record_converts_to_persisted_terminal() {
        let record = record(3, true);
        let persisted = PersistedTerminal::from(record);
        assert_eq!(persisted.terminal, 3);
        assert_eq!(persisted.worktree, PathBuf::from("/repo/3"));
        assert_eq!(persisted.pid, 103);
    }

    #[test]
    fn persisted_terminal_converts_to_adopted_record() {
        let persisted = PersistedTerminal {
            terminal: 4,
            worktree: PathBuf::from("/repo/4"),
            pid: 104,
        };
        assert_eq!(
            DaemonTerminalRecord::from(&persisted),
            DaemonTerminalRecord {
                terminal: 4,
                worktree: PathBuf::from("/repo/4"),
                pid: 104,
                adopted: true,
            }
        );
    }
}
