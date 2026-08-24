//! Durable snapshot plus append-only journal for supervisor runs.
//!
//! The journal is appended and fsynced before its derived snapshot is atomically
//! replaced. A replay checkpoint seeks past history already reflected by the
//! snapshot while the full journal remains available to the event-history API.
//! On restart, a torn final JSONL record is ignored because it was never a
//! durable, complete event.

#[cfg(test)]
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::domain::supervisor::{
    SupervisorEvent, SupervisorRun, SupervisorRunId, SupervisorRunQuery, reduce,
};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const SNAPSHOT_SUFFIX: &str = ".snapshot.json";
const JOURNAL_SUFFIX: &str = ".events.jsonl";
const CHECKPOINT_SUFFIX: &str = ".replay.json";

/// How many finished supervisor runs are kept on disk.
///
/// One run is a snapshot, a journal and a checkpoint, and nothing removed them.
/// Every `supervisor_list` reads *all* of them and replays each journal past its
/// snapshot, so on a long-lived daemon a listing gets slower with every run ever
/// started and the state directory grows without limit. A finished run is
/// history: this keeps the recent ones for inspection and drops the rest.
const RUN_RETENTION: usize = 128;

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
struct ReplayCheckpoint {
    snapshot_revision: u64,
    journal_offset: u64,
}

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
    #[cfg(test)]
    journal_bytes_read: Cell<u64>,
}
impl SupervisorStore {
    #[must_use]
    pub fn new(daemon_state_dir: &Path) -> Self {
        Self {
            dir: daemon_state_dir.join("supervisor-runs"),
            #[cfg(test)]
            journal_bytes_read: Cell::new(0),
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
    fn checkpoint_path(&self, id: SupervisorRunId) -> PathBuf {
        self.dir.join(format!("{id}{CHECKPOINT_SUFFIX}"))
    }
    /// Creates the initial atomically-written snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or snapshot cannot be written.
    pub fn initialize(&self, run: &SupervisorRun) -> Result<()> {
        json_file::write_atomic(&self.dir, &self.snapshot_path(run.supervisor_run_id), run)?;
        self.write_checkpoint(run.supervisor_run_id, run.state_revision, 0)?;
        // Starting a run is the moment the directory grows, so it is also where
        // the bound is charged. Pruning is best effort: a run that failed to be
        // removed is retried at the next start, and refusing to start a new run
        // because old history could not be deleted would be the worse failure.
        let _ = self.prune_finished_runs();
        Ok(())
    }

    /// Delete the finished runs past [`RUN_RETENTION`], oldest first.
    ///
    /// Only terminal runs are eligible: a run still planning, running, verifying
    /// or waiting on a person is live state, whatever its age. Each removal takes
    /// the snapshot, journal and checkpoint together, and the snapshot goes last
    /// so an interrupted prune leaves a run that [`Self::runs`] can still read
    /// and that the next prune will finish.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory cannot be listed or an
    /// aggregate cannot be read.
    pub fn prune_finished_runs(&self) -> Result<usize> {
        let mut finished: Vec<(DateTime<Utc>, SupervisorRunId)> = self
            .runs()?
            .into_iter()
            .filter(|run| run.state.terminal())
            .map(|run| {
                (
                    run.terminal_at.unwrap_or(run.updated_at),
                    run.supervisor_run_id,
                )
            })
            .collect();
        let Some(over) = finished.len().checked_sub(RUN_RETENTION) else {
            return Ok(0);
        };
        if over == 0 {
            return Ok(0);
        }
        finished.sort_by_key(|(finished_at, id)| (*finished_at, *id));
        let mut removed = 0;
        for (_, id) in finished.into_iter().take(over) {
            for path in [
                self.journal_path(id),
                self.checkpoint_path(id),
                self.snapshot_path(id),
            ] {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).context(format!("failed to remove {}", path.display()));
                    }
                }
            }
            removed += 1;
        }
        Ok(removed)
    }
    /// Loads and reconstructs a run, replaying complete events not yet reflected
    /// by the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when a snapshot or a non-final journal record is corrupt.
    pub fn load(&self, id: SupervisorRunId) -> Result<Option<SupervisorRun>> {
        let Some(run) = json_file::read(&self.snapshot_path(id))? else {
            return Ok(None);
        };
        self.replay_snapshot(run).map(Some)
    }
    fn replay_snapshot(&self, mut run: SupervisorRun) -> Result<SupervisorRun> {
        let snapshot_revision = run.state_revision;
        let checkpoint: Option<ReplayCheckpoint> =
            json_file::read(&self.checkpoint_path(run.supervisor_run_id))?;
        let checkpoint_matches =
            checkpoint.is_some_and(|checkpoint| checkpoint.snapshot_revision == snapshot_revision);
        let offset = checkpoint
            .filter(|checkpoint| checkpoint.snapshot_revision == snapshot_revision)
            .map_or(0, |checkpoint| checkpoint.journal_offset);
        let (events, journal_end) = self.read_journal_from(run.supervisor_run_id, offset)?;
        let journal_was_fully_reflected = events
            .last()
            .is_none_or(|event| event.sequence <= snapshot_revision);
        for event in events {
            reduce(&mut run, &event).map_err(anyhow::Error::msg)?;
        }
        // Older stores have no checkpoint. Once their snapshot covers the
        // journal, build the replay cursor so subsequent loads seek directly to
        // events appended after that snapshot.
        if !checkpoint_matches && journal_was_fully_reflected {
            self.write_checkpoint(run.supervisor_run_id, snapshot_revision, journal_end)?;
        }
        Ok(run)
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
            self.checkpoint_current_journal(id, run.state_revision)?;
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
        self.checkpoint_current_journal(id, run.state_revision)?;
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
            runs.push(self.replay_snapshot(snapshot)?);
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
    fn checkpoint_current_journal(
        &self,
        id: SupervisorRunId,
        snapshot_revision: u64,
    ) -> Result<()> {
        let journal_offset = match fs::metadata(self.journal_path(id)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).context("failed to inspect supervisor event journal"),
        };
        self.write_checkpoint(id, snapshot_revision, journal_offset)
    }
    fn write_checkpoint(
        &self,
        id: SupervisorRunId,
        snapshot_revision: u64,
        journal_offset: u64,
    ) -> Result<()> {
        json_file::write_atomic(
            &self.dir,
            &self.checkpoint_path(id),
            &ReplayCheckpoint {
                snapshot_revision,
                journal_offset,
            },
        )
    }
    fn read_journal(&self, id: SupervisorRunId) -> Result<Vec<SupervisorEvent>> {
        self.read_journal_from(id, 0).map(|(events, _)| events)
    }
    fn read_journal_from(
        &self,
        id: SupervisorRunId,
        offset: u64,
    ) -> Result<(Vec<SupervisorEvent>, u64)> {
        let path = self.journal_path(id);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], 0)),
            Err(error) => return Err(error).context("failed to open supervisor event journal"),
        };
        let journal_end = file.metadata()?.len();
        if offset > journal_end {
            bail!("supervisor replay checkpoint is beyond the event journal");
        }
        if offset > 0 {
            file.seek(SeekFrom::Start(offset - 1))?;
            let mut preceding = [0];
            file.read_exact(&mut preceding)?;
            if preceding != *b"\n" {
                bail!("supervisor replay checkpoint is not at an event boundary");
            }
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut result = vec![];
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            #[cfg(test)]
            self.journal_bytes_read
                .set(self.journal_bytes_read.get() + bytes as u64);
            lines.push(line);
        }
        for (index, line) in lines.iter().enumerate() {
            match serde_json::from_str(line.trim_end_matches('\n')) {
                Ok(event) => result.push(event),
                // A crash may leave only the final non-fsynced JSONL bytes.
                Err(_) if index + 1 == lines.len() => break,
                Err(error) => return Err(error).context("corrupt supervisor event journal"),
            }
        }
        Ok((result, journal_end))
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
    fn load_reads_only_events_after_the_snapshot_checkpoint() {
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
        let mut saved = run;
        for sequence in 1..=20 {
            saved = store
                .apply(id, saved.state_revision, &event(sequence))
                .unwrap();
        }
        let pending = event(21);
        store.append(id, &pending).unwrap();
        let pending_bytes = serde_json::to_vec(&pending).unwrap().len() as u64 + 1;
        let full_journal_bytes = fs::metadata(store.journal_path(id)).unwrap().len();

        store.journal_bytes_read.set(0);
        let loaded = store.load(id).unwrap().unwrap();

        assert_eq!(loaded.state_revision, 21);
        assert_eq!(store.journal_bytes_read.get(), pending_bytes);
        assert!(pending_bytes < full_journal_bytes);
    }

    #[test]
    fn replay_checkpoint_boundaries_fail_closed_and_missing_journals_use_zero() {
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
        store
            .checkpoint_current_journal(id, run.state_revision)
            .unwrap();

        store.append(id, &event(1)).unwrap();
        store.write_checkpoint(id, run.state_revision, 1).unwrap();
        assert!(
            store
                .load(id)
                .unwrap_err()
                .to_string()
                .contains("not at an event boundary")
        );
        let journal_end = fs::metadata(store.journal_path(id)).unwrap().len();
        store
            .write_checkpoint(id, run.state_revision, journal_end + 1)
            .unwrap();
        assert!(
            store
                .load(id)
                .unwrap_err()
                .to_string()
                .contains("beyond the event journal")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(store.journal_path(id)).unwrap();
            symlink(store.journal_path(id), store.journal_path(id)).unwrap();
            assert!(
                store
                    .checkpoint_current_journal(id, run.state_revision)
                    .unwrap_err()
                    .to_string()
                    .contains("failed to inspect supervisor event journal")
            );
        }
    }

    #[test]
    fn load_migrates_a_fully_reflected_legacy_journal_once() {
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
        store.apply(id, 0, &event(1)).unwrap();
        fs::remove_file(store.checkpoint_path(id)).unwrap();

        store.journal_bytes_read.set(0);
        store.load(id).unwrap();
        assert!(store.journal_bytes_read.get() > 0);
        store.journal_bytes_read.set(0);
        store.load(id).unwrap();
        assert_eq!(store.journal_bytes_read.get(), 0);
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

    /// Every `supervisor_list` reads all snapshots and replays each journal past
    /// it, so runs that are never removed make listing slower with every run
    /// ever started and grow the state directory without limit.
    #[test]
    fn finished_runs_are_pruned_while_live_ones_are_kept_whatever_their_age() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());

        // The oldest run of all, still waiting on a person. Age is not what
        // makes a run eligible; being finished is.
        let live = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let live_id = live.supervisor_run_id;
        store.initialize(&live).unwrap();

        let mut finished = Vec::new();
        for index in 0..(RUN_RETENTION + 20) {
            let mut run = SupervisorRun::new(
                "caller".into(),
                "task".into(),
                "input".into(),
                "policy".into(),
                now() + chrono::Duration::seconds(i64::try_from(index).unwrap() + 1),
            );
            run.state = SupervisorRunState::Succeeded;
            run.terminal_at = Some(run.created_at);
            finished.push(run.supervisor_run_id);
            store.initialize(&run).unwrap();
        }

        let kept = store.runs().unwrap();
        let kept_finished = kept.iter().filter(|run| run.state.terminal()).count();
        assert!(
            kept_finished <= RUN_RETENTION,
            "supervisor history grew past its bound: {kept_finished}"
        );
        assert!(
            kept.iter().any(|run| run.supervisor_run_id == live_id),
            "a live run was pruned as history"
        );

        // A pruned run leaves nothing behind: snapshot, journal and checkpoint
        // all go, or the directory would keep growing anyway.
        let surviving: Vec<_> = kept.iter().map(|run| run.supervisor_run_id).collect();
        for id in finished {
            if !surviving.contains(&id) {
                assert!(!store.snapshot_path(id).exists(), "snapshot survived");
                assert!(!store.journal_path(id).exists(), "journal survived");
                assert!(!store.checkpoint_path(id).exists(), "checkpoint survived");
            }
        }
    }

    /// Below the bound nothing is removed, so an ordinary daemon never loses a
    /// run it might be asked about.
    #[test]
    fn pruning_removes_nothing_while_the_history_is_within_its_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        for index in 0..3 {
            let mut run = SupervisorRun::new(
                "caller".into(),
                "task".into(),
                "input".into(),
                "policy".into(),
                now() + chrono::Duration::seconds(index),
            );
            run.state = SupervisorRunState::Failed;
            store.initialize(&run).unwrap();
        }
        assert_eq!(store.prune_finished_runs().unwrap(), 0);
        assert_eq!(store.runs().unwrap().len(), 3);
    }

    /// A prune that cannot delete must say so rather than report a removal it
    /// did not make: the next start would otherwise never retry it.
    #[test]
    fn a_prune_that_cannot_remove_a_file_reports_the_failure() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        // Started live so `initialize` prunes nothing, then finished in place.
        // Creating them finished instead would let each `initialize` prune the
        // previous one and the store would never exceed its bound.
        let mut runs = Vec::new();
        for index in 0..(RUN_RETENTION + 5) {
            let run = SupervisorRun::new(
                "caller".into(),
                "task".into(),
                "input".into(),
                "policy".into(),
                now() + chrono::Duration::seconds(i64::try_from(index).unwrap()),
            );
            store.initialize(&run).unwrap();
            runs.push(run);
        }
        for mut run in runs {
            run.state = SupervisorRunState::Succeeded;
            run.terminal_at = Some(run.created_at);
            json_file::write_atomic(
                &store.dir,
                &store.snapshot_path(run.supervisor_run_id),
                &run,
            )
            .unwrap();
        }

        // A read-only parent directory is what makes `remove_file` fail while
        // the entries themselves are still present and listable.
        let mode = fs::metadata(&store.dir).unwrap().permissions().mode();
        fs::set_permissions(&store.dir, fs::Permissions::from_mode(0o555)).unwrap();
        let refused = store.prune_finished_runs();
        fs::set_permissions(&store.dir, fs::Permissions::from_mode(mode)).unwrap();

        let error = refused.expect_err("a prune that could delete nothing reported success");
        assert!(
            format!("{error:#}").contains("failed to remove"),
            "{error:#}"
        );
    }
}
