//! Durable snapshot plus append-only journal for supervisor runs.
//!
//! The journal is appended and fsynced before its derived snapshot is atomically
//! replaced.  On restart a snapshot is replayed from the journal; a torn final
//! JSONL record is ignored because it was never a durable, complete event.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::domain::supervisor::{
    SupervisorEvent, SupervisorRun, SupervisorRunId, SupervisorRunQuery, reduce,
};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const SNAPSHOT_SUFFIX: &str = ".snapshot.json";
const JOURNAL_SUFFIX: &str = ".events.jsonl";

/// Cursor used to page a run's event history without exposing payload bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct EventCursor {
    pub next_sequence: u64,
}
/// Redaction-safe journal result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EventQuery {
    pub sequence: u64,
    pub event_id: crate::domain::id::OperationId,
    pub payload_digest: String,
    pub source: crate::domain::supervisor::SupervisorEventSource,
}

/// A daemon-owned durable supervisor store rooted at its state directory.
pub struct SupervisorStore {
    dir: PathBuf,
}
impl SupervisorStore {
    #[must_use]
    pub fn new(daemon_state_dir: &Path) -> Self {
        Self {
            dir: daemon_state_dir.join("supervisor-runs"),
        }
    }
    #[must_use]
    pub fn snapshot_path(&self, id: SupervisorRunId) -> PathBuf {
        self.dir.join(format!("{id}{SNAPSHOT_SUFFIX}"))
    }
    #[must_use]
    pub fn journal_path(&self, id: SupervisorRunId) -> PathBuf {
        self.dir.join(format!("{id}{JOURNAL_SUFFIX}"))
    }
    /// Creates the initial atomically-written snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or snapshot cannot be written.
    pub fn initialize(&self, run: &SupervisorRun) -> Result<()> {
        json_file::write_atomic(&self.dir, &self.snapshot_path(run.supervisor_run_id), run)
    }
    /// Loads and reconstructs a run, replaying complete events not yet reflected
    /// by the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when a snapshot or a non-final journal record is corrupt.
    pub fn load(&self, id: SupervisorRunId) -> Result<Option<SupervisorRun>> {
        let Some(mut run) = json_file::read(&self.snapshot_path(id))? else {
            return Ok(None);
        };
        for event in self.read_journal(id)? {
            reduce(&mut run, &event).map_err(anyhow::Error::msg)?;
        }
        Ok(Some(run))
    }
    /// Appends an event under the cross-process store lock, requiring exact
    /// sequence CAS. Duplicate event IDs are safely returned as a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale revision, reducer rejection, or durable IO
    /// failure. The snapshot is unchanged when the event cannot be accepted.
    pub fn apply(
        &self,
        id: SupervisorRunId,
        expected_revision: u64,
        event: &SupervisorEvent,
    ) -> Result<SupervisorRun> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut run = self
            .load(id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist"))?;
        if run.applied_events.contains(&event.event_id) {
            return Ok(run);
        }
        if run.state_revision != expected_revision {
            bail!(
                "stale supervisor state revision: expected {expected_revision}, got {}",
                run.state_revision
            );
        }
        reduce(&mut run, event).map_err(anyhow::Error::msg)?;
        self.append(id, event)?;
        json_file::write_atomic(&self.dir, &self.snapshot_path(id), &run)?;
        Ok(run)
    }
    /// Returns the redaction-safe aggregate projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable state cannot be read or replayed.
    pub fn query(&self, id: SupervisorRunId) -> Result<Option<SupervisorRunQuery>> {
        Ok(self.load(id)?.map(|run| run.query()))
    }
    /// Returns every durable aggregate, including journal records committed
    /// after its latest snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or an aggregate is corrupt.
    pub fn runs(&self) -> Result<Vec<SupervisorRun>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("failed to list supervisor runs"),
        };
        let mut runs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(SNAPSHOT_SUFFIX))
            {
                continue;
            }
            let snapshot: SupervisorRun = json_file::read(&path)?
                .ok_or_else(|| anyhow::anyhow!("supervisor snapshot disappeared"))?;
            if let Some(run) = self.load(snapshot.supervisor_run_id)? {
                runs.push(run);
            }
        }
        runs.sort_by_key(|run| (run.created_at, run.supervisor_run_id));
        Ok(runs)
    }
    /// Lists event metadata from `cursor`, and the next cursor if more history
    /// was returned. Event kinds and instruction bodies are intentionally absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the event journal cannot be read.
    pub fn events(
        &self,
        id: SupervisorRunId,
        cursor: EventCursor,
        limit: usize,
    ) -> Result<(Vec<EventQuery>, EventCursor)> {
        let events = self.read_journal(id)?;
        let selected: Vec<_> = events
            .into_iter()
            .filter(|event| event.sequence >= cursor.next_sequence)
            .take(limit)
            .map(|event| EventQuery {
                sequence: event.sequence,
                event_id: event.event_id,
                payload_digest: event.payload_digest,
                source: event.source,
            })
            .collect();
        let next_sequence = selected
            .last()
            .map_or(cursor.next_sequence, |event| event.sequence + 1);
        Ok((selected, EventCursor { next_sequence }))
    }
    fn append(&self, id: SupervisorRunId, event: &SupervisorEvent) -> Result<()> {
        fs::create_dir_all(&self.dir).context("failed to create supervisor state directory")?;
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path(id))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }
    fn read_journal(&self, id: SupervisorRunId) -> Result<Vec<SupervisorEvent>> {
        let path = self.journal_path(id);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error).context("failed to open supervisor event journal"),
        };
        let mut result = vec![];
        let lines: Vec<_> = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<_>>()?;
        for (index, line) in lines.iter().enumerate() {
            match serde_json::from_str(line) {
                Ok(event) => result.push(event),
                // A crash may leave only the final non-fsynced JSONL bytes.
                Err(_) if index + 1 == lines.len() => break,
                Err(error) => return Err(error).context("corrupt supervisor event journal"),
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::OperationId;
    use crate::domain::supervisor::{
        SupervisorEventKind, SupervisorEventSource, SupervisorRunState,
    };
    use chrono::{DateTime, TimeZone, Utc};
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn event(sequence: u64) -> SupervisorEvent {
        SupervisorEvent {
            sequence,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now(),
            payload_digest: "digest".into(),
            source: SupervisorEventSource::Admission,
            kind: SupervisorEventKind::SetRunState {
                state: SupervisorRunState::Running,
                terminal_reason: None,
            },
        }
    }
    #[test]
    fn replays_a_journal_after_snapshot_and_fences_stale_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let id = run.supervisor_run_id;
        store.initialize(&run).unwrap();
        let first = event(1);
        let saved = store.apply(id, 0, &first).unwrap();
        assert_eq!(saved.state_revision, 1);
        assert!(
            store
                .apply(id, 0, &event(2))
                .unwrap_err()
                .to_string()
                .starts_with("stale supervisor state revision"),
        );
        assert_eq!(store.apply(id, 1, &first).unwrap().state_revision, 1);
        let (events, cursor) = store
            .events(id, EventCursor { next_sequence: 1 }, 10)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(cursor.next_sequence, 2);
        assert_eq!(
            store.load(id).unwrap().unwrap().query().state,
            SupervisorRunState::Running
        );
    }

    #[test]
    fn query_runs_and_corrupt_journal_paths_are_observable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let missing = SupervisorRunId::new();
        assert!(store.query(missing).unwrap().is_none());
        assert!(store.runs().unwrap().is_empty());
        assert!(
            store
                .apply(missing, 0, &event(1))
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );

        let run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let id = run.supervisor_run_id;
        store.initialize(&run).unwrap();
        let another = SupervisorRun::new(
            "caller".into(),
            "another".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        store.initialize(&another).unwrap();
        assert_eq!(store.query(id).unwrap().unwrap().supervisor_run_id, id);
        fs::write(store.dir.join("ignored.txt"), "ignored").unwrap();
        assert_eq!(store.runs().unwrap().len(), 2);

        fs::write(store.journal_path(id), "{broken\n{also-broken\n").unwrap();
        assert!(
            store
                .load(id)
                .unwrap_err()
                .to_string()
                .contains("corrupt supervisor event journal")
        );
        fs::write(store.journal_path(id), "{final-torn\n").unwrap();
        assert_eq!(store.load(id).unwrap().unwrap().state_revision, 0);

        fs::remove_file(store.journal_path(id)).unwrap();
        fs::create_dir(store.journal_path(id)).unwrap();
        assert!(store.load(id).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_dir(store.journal_path(id)).unwrap();
            symlink(store.journal_path(id), store.journal_path(id)).unwrap();
            assert!(
                store
                    .load(id)
                    .unwrap_err()
                    .to_string()
                    .contains("failed to open supervisor event journal")
            );
        }
    }

    #[test]
    fn store_io_failures_do_not_masquerade_as_applied_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let id = run.supervisor_run_id;
        store.initialize(&run).unwrap();
        fs::create_dir(store.journal_path(id)).unwrap();
        assert!(store.apply(id, 0, &event(1)).is_err());
        assert!(store.load(id).is_err());

        let blocked = tempfile::tempdir().unwrap();
        fs::write(blocked.path().join("supervisor-runs"), "not a directory").unwrap();
        assert!(SupervisorStore::new(blocked.path()).runs().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn disappearing_snapshot_is_reported_deterministically() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        fs::create_dir_all(&store.dir).unwrap();
        let name = format!("{}{}", SupervisorRunId::new(), SNAPSHOT_SUFFIX);
        symlink(store.dir.join("missing-target"), store.dir.join(name)).unwrap();
        assert!(
            store
                .runs()
                .unwrap_err()
                .to_string()
                .contains("snapshot disappeared")
        );
    }
}
