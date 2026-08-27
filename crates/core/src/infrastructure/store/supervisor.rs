//! Durable snapshot plus append-only journal for supervisor runs.
//!
//! The journal is appended and fsynced before its derived snapshot is atomically
//! replaced. A replay checkpoint seeks past history already reflected by the
//! snapshot while the full journal remains available to the event-history API.
//! On restart, a torn final JSONL record is ignored because it was never a
//! durable, complete event.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::domain::supervisor::{
    SupervisorEvent, SupervisorRun, SupervisorRunId, SupervisorRunQuery, SupervisorRunState, reduce,
};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const SNAPSHOT_SUFFIX: &str = ".snapshot.json";
const JOURNAL_SUFFIX: &str = ".events.jsonl";
const JOURNAL_INDEX_SUFFIX: &str = ".events.index.json";
const CHECKPOINT_SUFFIX: &str = ".replay.json";
const RUN_LIST_INDEX_FILE: &str = "runs.index.json";

/// Compact at the high watermark and keep this many newest events. The gap
/// avoids rewriting the journal for every subsequent append.
#[cfg(not(test))]
const JOURNAL_MAX_EVENTS: usize = 4_096;
#[cfg(test)]
const JOURNAL_MAX_EVENTS: usize = 64;
#[cfg(not(test))]
const JOURNAL_RETAIN_EVENTS: usize = 2_048;
#[cfg(test)]
const JOURNAL_RETAIN_EVENTS: usize = 32;

/// How many finished supervisor runs are kept on disk.
///
/// One run is a snapshot, a journal and a checkpoint, and nothing removed them.
/// Every `supervisor_list` reads *all* of them and replays each journal past its
/// snapshot, so on a long-lived daemon a listing gets slower with every run ever
/// started and the state directory grows without limit. A finished run is
/// history: this keeps the recent ones for inspection and drops the rest.
const RUN_RETENTION: usize = 128;
/// Keep list results comfortably below the 1 MiB daemon IPC frame after the
/// response envelope and protocol metadata are added.
pub const RUN_LIST_RESPONSE_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
struct ReplayCheckpoint {
    snapshot_revision: u64,
    journal_offset: u64,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
struct JournalIndexEntry {
    sequence: u64,
    offset: u64,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
struct JournalIndex {
    /// Exact current file length, including a crash-torn final record.
    journal_len: u64,
    /// End of the last complete indexed record.
    valid_len: u64,
    entries: Vec<JournalIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct RunListIndexEntry {
    supervisor_run_id: SupervisorRunId,
    root_caller_ref: String,
    created_at: DateTime<Utc>,
    state: SupervisorRunState,
    state_revision: u64,
}

impl From<&SupervisorRun> for RunListIndexEntry {
    fn from(run: &SupervisorRun) -> Self {
        Self {
            supervisor_run_id: run.supervisor_run_id,
            root_caller_ref: run.root_caller_ref.clone(),
            created_at: run.created_at,
            state: run.state,
            state_revision: run.state_revision,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
struct RunListIndex {
    entries: Vec<RunListIndexEntry>,
}

/// Bounded `supervisor_list` result. The cursor is an opaque position in the
/// durable run index rather than an offset into a fully hydrated result set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorRunPage {
    pub runs: Vec<SupervisorRunQuery>,
    pub next_cursor: Option<String>,
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
    run_list_index_trusted: Cell<bool>,
    #[cfg(test)]
    journal_bytes_read: Cell<u64>,
    #[cfg(test)]
    run_snapshots_read: Cell<u64>,
}
impl SupervisorStore {
    #[must_use]
    pub fn new(daemon_state_dir: &Path) -> Self {
        Self {
            dir: daemon_state_dir.join("supervisor-runs"),
            run_list_index_trusted: Cell::new(false),
            #[cfg(test)]
            journal_bytes_read: Cell::new(0),
            #[cfg(test)]
            run_snapshots_read: Cell::new(0),
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
    fn journal_index_path(&self, id: SupervisorRunId) -> PathBuf {
        self.dir.join(format!("{id}{JOURNAL_INDEX_SUFFIX}"))
    }
    fn checkpoint_path(&self, id: SupervisorRunId) -> PathBuf {
        self.dir.join(format!("{id}{CHECKPOINT_SUFFIX}"))
    }
    fn run_list_index_path(&self) -> PathBuf {
        self.dir.join(RUN_LIST_INDEX_FILE)
    }
    /// Creates the initial atomically-written snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or snapshot cannot be written.
    pub fn initialize(&self, run: &SupervisorRun) -> Result<()> {
        json_file::write_atomic(&self.dir, &self.snapshot_path(run.supervisor_run_id), run)?;
        self.write_checkpoint(run.supervisor_run_id, run.state_revision, 0)?;
        self.refresh_run_list_index(run);
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
        // Counting snapshots is a directory listing; deciding from `runs()`
        // would read and replay every aggregate on every single run start,
        // trading the unbounded listing this bound exists to fix for an
        // unbounded start. Below the bound there is nothing to remove whatever
        // those aggregates say, so the expensive read is not reached.
        if self.snapshot_count()? <= RUN_RETENTION {
            return Ok(0);
        }
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
        let mut removed_ids = Vec::new();
        for (_, id) in finished.into_iter().take(over) {
            for path in [
                self.journal_path(id),
                self.journal_index_path(id),
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
            removed_ids.push(id);
        }
        self.remove_from_run_list_index(&removed_ids);
        Ok(removed)
    }
    /// How many run snapshots the state directory holds, without reading any of
    /// them. A missing directory holds none.
    fn snapshot_count(&self) -> Result<usize> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("failed to list supervisor runs"),
        };
        let mut count = 0;
        for entry in entries {
            if entry?
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(SNAPSHOT_SUFFIX))
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Loads and reconstructs a run, replaying complete events not yet reflected
    /// by the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when a snapshot or a non-final journal record is corrupt.
    pub fn load(&self, id: SupervisorRunId) -> Result<Option<SupervisorRun>> {
        #[cfg(test)]
        self.run_snapshots_read
            .set(self.run_snapshots_read.get() + 1);
        let Some(run) = json_file::read(&self.snapshot_path(id))? else {
            return Ok(None);
        };
        Self::validate_snapshot(&run)?;
        self.replay_snapshot(run).map(Some)
    }

    fn validate_snapshot(run: &SupervisorRun) -> Result<()> {
        if !run.compaction_state_is_valid() {
            bail!("supervisor snapshot has an invalid compaction tombstone");
        }
        Ok(())
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
        match run.event_id_status(event.event_id) {
            crate::domain::supervisor::AppliedEventStatus::Recent => {
                self.checkpoint_current_journal(id, run.state_revision)?;
                return Ok(run);
            }
            crate::domain::supervisor::AppliedEventStatus::Expired => {
                bail!("supervisor event id is outside the retained idempotency window");
            }
            crate::domain::supervisor::AppliedEventStatus::Fresh => {}
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
        if let Some(retained) = self.compact_journal(id, run.state_revision)? {
            let retained_ids = retained
                .iter()
                .map(|event| event.event_id)
                .collect::<std::collections::BTreeSet<_>>();
            run.compact_applied_events(&retained_ids);
            json_file::write_atomic(&self.dir, &self.snapshot_path(id), &run)?;
        }
        self.checkpoint_current_journal(id, run.state_revision)?;
        self.refresh_run_list_index(&run);
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
            Self::validate_snapshot(&snapshot)?;
            runs.push(self.replay_snapshot(snapshot)?);
        }
        runs.sort_by_key(|run| (run.created_at, run.supervisor_run_id));
        Ok(runs)
    }

    /// Lists one owner/state-filtered page without hydrating aggregates outside
    /// that page, and refuses a single response above the serialized budget.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor, unreadable durable state, or a
    /// run whose safe projection alone exceeds the response byte budget.
    pub fn runs_page(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
        cursor: usize,
        limit: usize,
    ) -> Result<SupervisorRunPage> {
        self.runs_page_with_budget(caller, state, cursor, limit, RUN_LIST_RESPONSE_MAX_BYTES)
    }

    fn runs_page_with_budget(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
        cursor: usize,
        limit: usize,
        maximum_bytes: usize,
    ) -> Result<SupervisorRunPage> {
        if limit == 0 {
            bail!("supervisor list page limit must be positive");
        }
        let index = self.run_list_index()?;
        if cursor > index.entries.len() {
            bail!("supervisor list cursor is outside the retained run range");
        }
        let matches = |entry: &RunListIndexEntry| {
            entry.root_caller_ref == caller && state.is_none_or(|value| entry.state == value)
        };
        let mut page = SupervisorRunPage {
            runs: Vec::new(),
            next_cursor: None,
        };
        let mut positions = Vec::new();
        for (position, entry) in index.entries.iter().enumerate().skip(cursor) {
            if !matches(entry) {
                continue;
            }
            let run = self
                .load(entry.supervisor_run_id)?
                .ok_or_else(|| anyhow::anyhow!("supervisor indexed snapshot disappeared"))?;
            if RunListIndexEntry::from(&run) != *entry {
                // Another store instance may have advanced a snapshot. Rebuild
                // the disposable index once from authoritative aggregates.
                self.run_list_index_trusted.set(false);
                return self.runs_page_with_budget(caller, state, cursor, limit, maximum_bytes);
            }
            page.runs.push(run.query());
            positions.push(position);
            let more = index.entries[position + 1..].iter().any(&matches);
            page.next_cursor = more.then(|| (position + 1).to_string());
            if serde_json::to_vec(&page)?.len() > maximum_bytes {
                page.runs.pop();
                positions.pop();
                let mut resume_at = position;
                loop {
                    if page.runs.is_empty() {
                        bail!("supervisor list response capacity is exhausted by one run");
                    }
                    page.next_cursor = Some(resume_at.to_string());
                    if serde_json::to_vec(&page)?.len() <= maximum_bytes {
                        return Ok(page);
                    }
                    page.runs.pop();
                    resume_at = positions
                        .pop()
                        .context("supervisor list page position is missing")?;
                }
            }
            if page.runs.len() == limit || !more {
                return Ok(page);
            }
        }
        Ok(page)
    }

    fn run_list_index(&self) -> Result<RunListIndex> {
        if self.run_list_index_trusted.get()
            && let Ok(Some(index)) = json_file::read::<RunListIndex>(&self.run_list_index_path())
            && self.run_list_index_is_valid(&index)?
        {
            return Ok(index);
        }
        self.rebuild_run_list_index()
    }

    fn run_list_index_is_valid(&self, index: &RunListIndex) -> Result<bool> {
        if index.entries.len() != self.snapshot_count()? {
            return Ok(false);
        }
        let mut previous = None;
        for entry in &index.entries {
            let key = (entry.created_at, entry.supervisor_run_id);
            if previous.is_some_and(|value| value >= key)
                || !self.snapshot_path(entry.supervisor_run_id).is_file()
            {
                return Ok(false);
            }
            previous = Some(key);
        }
        Ok(true)
    }

    fn rebuild_run_list_index(&self) -> Result<RunListIndex> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RunListIndex::default());
            }
            Err(error) => return Err(error).context("failed to list supervisor runs"),
        };
        let mut index = RunListIndex::default();
        for entry in entries {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(SNAPSHOT_SUFFIX))
            {
                continue;
            }
            #[cfg(test)]
            self.run_snapshots_read
                .set(self.run_snapshots_read.get() + 1);
            let snapshot: SupervisorRun = json_file::read(&path)?
                .ok_or_else(|| anyhow::anyhow!("supervisor snapshot disappeared"))?;
            Self::validate_snapshot(&snapshot)?;
            index
                .entries
                .push(RunListIndexEntry::from(&self.replay_snapshot(snapshot)?));
        }
        index
            .entries
            .sort_by_key(|entry| (entry.created_at, entry.supervisor_run_id));
        if json_file::write_atomic(&self.dir, &self.run_list_index_path(), &index).is_ok() {
            self.run_list_index_trusted.set(true);
        } else {
            self.run_list_index_trusted.set(false);
        }
        Ok(index)
    }

    fn refresh_run_list_index(&self, run: &SupervisorRun) {
        if !self.run_list_index_trusted.get() {
            if self.snapshot_count().ok() != Some(1) {
                return;
            }
            let index = RunListIndex {
                entries: vec![RunListIndexEntry::from(run)],
            };
            if json_file::write_atomic(&self.dir, &self.run_list_index_path(), &index).is_ok() {
                self.run_list_index_trusted.set(true);
            }
            return;
        }
        let Ok(Some(mut index)) = json_file::read::<RunListIndex>(&self.run_list_index_path())
        else {
            self.run_list_index_trusted.set(false);
            return;
        };
        let replacement = RunListIndexEntry::from(run);
        if let Some(entry) = index
            .entries
            .iter_mut()
            .find(|entry| entry.supervisor_run_id == run.supervisor_run_id)
        {
            *entry = replacement;
        } else {
            index.entries.push(replacement);
        }
        index
            .entries
            .sort_by_key(|entry| (entry.created_at, entry.supervisor_run_id));
        if json_file::write_atomic(&self.dir, &self.run_list_index_path(), &index).is_err() {
            self.run_list_index_trusted.set(false);
        }
    }

    fn remove_from_run_list_index(&self, removed: &[SupervisorRunId]) {
        if removed.is_empty() || !self.run_list_index_trusted.get() {
            return;
        }
        let Ok(Some(mut index)) = json_file::read::<RunListIndex>(&self.run_list_index_path())
        else {
            self.run_list_index_trusted.set(false);
            return;
        };
        index
            .entries
            .retain(|entry| !removed.contains(&entry.supervisor_run_id));
        if json_file::write_atomic(&self.dir, &self.run_list_index_path(), &index).is_err() {
            self.run_list_index_trusted.set(false);
        }
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
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let index = self.journal_index(id)?;
        let Some(first) = index.entries.first() else {
            return Ok((Vec::new(), cursor));
        };
        if cursor.next_sequence < first.sequence {
            bail!(
                "supervisor event cursor expired: earliest retained sequence is {}",
                first.sequence
            );
        }
        let start = index
            .entries
            .partition_point(|entry| entry.sequence < cursor.next_sequence);
        let Some(entry) = index.entries.get(start) else {
            return Ok((Vec::new(), cursor));
        };
        let selected: Vec<_> = self
            .read_journal_page(id, entry.offset, limit)?
            .into_iter()
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
        let mut index = self.journal_index(id)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path(id))?;
        let mut offset = file.metadata()?.len();
        if index.valid_len < offset {
            file.set_len(index.valid_len)?;
            offset = index.valid_len;
            index.journal_len = offset;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        let journal_len = offset + u64::try_from(bytes.len())?;
        index.entries.push(JournalIndexEntry {
            sequence: event.sequence,
            offset,
        });
        index.journal_len = journal_len;
        index.valid_len = journal_len;
        self.write_journal_index(id, &index)?;
        Ok(())
    }

    fn compact_journal(
        &self,
        id: SupervisorRunId,
        snapshot_revision: u64,
    ) -> Result<Option<Vec<SupervisorEvent>>> {
        let index = self.journal_index(id)?;
        if index.entries.len() <= JOURNAL_MAX_EVENTS {
            return Ok(None);
        }
        let retained_start = index.entries.len() - JOURNAL_RETAIN_EVENTS;
        let retained_offset = index.entries[retained_start].offset;
        let retained = self.read_journal_page(id, retained_offset, JOURNAL_RETAIN_EVENTS)?;
        let text = retained
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n")
            + "\n";

        // Publish a checkpoint valid both before and after the atomic journal
        // replacement. A crash in this window replays duplicates from offset
        // zero against the already-current snapshot, which is effect-free.
        self.write_checkpoint(id, snapshot_revision, 0)?;
        json_file::write_text_atomic(&self.journal_path(id), &text)?;
        let compacted = Self::index_for_events(&retained)?;
        self.write_journal_index(id, &compacted)?;
        Ok(Some(retained))
    }

    fn index_for_events(events: &[SupervisorEvent]) -> Result<JournalIndex> {
        let mut offset = 0_u64;
        let mut entries = Vec::with_capacity(events.len());
        for event in events {
            entries.push(JournalIndexEntry {
                sequence: event.sequence,
                offset,
            });
            offset += u64::try_from(serde_json::to_vec(event)?.len() + 1)?;
        }
        Ok(JournalIndex {
            journal_len: offset,
            valid_len: offset,
            entries,
        })
    }

    fn journal_index(&self, id: SupervisorRunId) -> Result<JournalIndex> {
        let journal_len = match fs::metadata(self.journal_path(id)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JournalIndex::default());
            }
            Err(error) => return Err(error).context("failed to inspect supervisor event journal"),
        };
        // The index is a derived cache. A stale or corrupt copy must never make
        // the authoritative journal unreadable; rebuild it from the journal.
        if let Ok(Some(index)) = json_file::read::<JournalIndex>(&self.journal_index_path(id))
            && index.journal_len == journal_len
            && index.valid_len <= index.journal_len
            && index
                .entries
                .last()
                .is_none_or(|entry| entry.offset < index.valid_len)
            && index
                .entries
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence && pair[0].offset < pair[1].offset)
            && index.entries.first().is_none_or(|entry| entry.offset == 0)
        {
            return Ok(index);
        }
        self.rebuild_journal_index(id)
    }

    fn rebuild_journal_index(&self, id: SupervisorRunId) -> Result<JournalIndex> {
        let file = fs::File::open(self.journal_path(id))?;
        let journal_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut index = JournalIndex {
            journal_len,
            ..JournalIndex::default()
        };
        loop {
            let offset = index.valid_len;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            #[cfg(test)]
            self.journal_bytes_read
                .set(self.journal_bytes_read.get() + bytes as u64);
            if !line.ends_with('\n') {
                break;
            }
            match serde_json::from_str::<SupervisorEvent>(line.trim_end_matches('\n')) {
                Ok(event) => {
                    index.entries.push(JournalIndexEntry {
                        sequence: event.sequence,
                        offset,
                    });
                    index.valid_len += u64::try_from(bytes)?;
                }
                Err(error) => return Err(error).context("corrupt supervisor event journal"),
            }
        }
        self.write_journal_index(id, &index)?;
        Ok(index)
    }

    fn write_journal_index(&self, id: SupervisorRunId, index: &JournalIndex) -> Result<()> {
        json_file::write_atomic_cache(&self.dir, &self.journal_index_path(id), index)
    }

    fn read_journal_page(
        &self,
        id: SupervisorRunId,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<SupervisorEvent>> {
        let mut file = fs::File::open(self.journal_path(id))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::with_capacity(limit);
        for _ in 0..limit {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            #[cfg(test)]
            self.journal_bytes_read
                .set(self.journal_bytes_read.get() + bytes as u64);
            if !line.ends_with('\n') {
                break;
            }
            match serde_json::from_str(line.trim_end_matches('\n')) {
                Ok(event) => events.push(event),
                Err(error) => return Err(error).context("corrupt supervisor event journal"),
            }
        }
        Ok(events)
    }
    fn checkpoint_current_journal(
        &self,
        id: SupervisorRunId,
        snapshot_revision: u64,
    ) -> Result<()> {
        // Only LF-terminated records are durable journal entries. The file can
        // be longer after a crash-torn append, but publishing that raw length
        // would place the replay cursor inside an incomplete record.
        let journal_offset = self.journal_index(id)?.valid_len;
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
        let file_end = file.metadata()?.len();
        if offset > file_end {
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
        let mut valid_end = offset;
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
        for line in &lines {
            if !line.ends_with('\n') {
                break;
            }
            match serde_json::from_str(line.trim_end_matches('\n')) {
                Ok(event) => {
                    valid_end += u64::try_from(line.len())?;
                    result.push(event);
                }
                Err(error) => return Err(error).context("corrupt supervisor event journal"),
            }
        }
        Ok((result, valid_end))
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
    fn indexed_event_pages_read_only_the_requested_journal_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        fs::create_dir_all(&store.dir).unwrap();
        let id = SupervisorRunId::new();
        let mut writer = std::io::BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(store.journal_path(id))
                .unwrap(),
        );
        let mut index = JournalIndex::default();
        for sequence in 1..=100_000 {
            let item = event(sequence);
            index.entries.push(JournalIndexEntry {
                sequence,
                offset: index.journal_len,
            });
            let bytes = serde_json::to_vec(&item).unwrap();
            writer.write_all(&bytes).unwrap();
            writer.write_all(b"\n").unwrap();
            index.journal_len += u64::try_from(bytes.len() + 1).unwrap();
            index.valid_len = index.journal_len;
        }
        writer.flush().unwrap();
        store.write_journal_index(id, &index).unwrap();

        store.journal_bytes_read.set(0);
        let (page, cursor) = store
            .events(
                id,
                EventCursor {
                    next_sequence: 99_901,
                },
                100,
            )
            .unwrap();
        assert_eq!(page.len(), 100);
        assert_eq!(page.first().unwrap().sequence, 99_901);
        assert_eq!(page.last().unwrap().sequence, 100_000);
        assert_eq!(cursor.next_sequence, 100_001);
        assert!(
            store.journal_bytes_read.get() * 500 < index.journal_len,
            "page query read the journal prefix"
        );

        store.journal_bytes_read.set(0);
        assert_eq!(
            store
                .events(
                    id,
                    EventCursor {
                        next_sequence: 50_000,
                    },
                    1,
                )
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(store.journal_bytes_read.get() < 1_024);
    }

    #[test]
    fn event_query_edges_and_corrupt_derived_indexes_are_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let id = SupervisorRunId::new();

        assert_eq!(
            store
                .events(id, EventCursor { next_sequence: 7 }, 0)
                .unwrap(),
            (Vec::new(), EventCursor { next_sequence: 7 })
        );
        assert_eq!(
            store
                .events(id, EventCursor { next_sequence: 1 }, 1)
                .unwrap(),
            (Vec::new(), EventCursor { next_sequence: 1 })
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
        store.apply(id, 0, &event(1)).unwrap();
        store.apply(id, 1, &event(2)).unwrap();
        fs::write(store.journal_index_path(id), "{broken").unwrap();

        let (events, cursor) = store
            .events(id, EventCursor { next_sequence: 2 }, 1)
            .unwrap();
        assert_eq!(events[0].sequence, 2);
        assert_eq!(cursor.next_sequence, 3);
        assert_eq!(store.journal_index(id).unwrap().entries.len(), 2);
        assert_eq!(
            store
                .events(id, EventCursor { next_sequence: 3 }, 1)
                .unwrap(),
            (Vec::new(), EventCursor { next_sequence: 3 })
        );
    }

    #[test]
    fn journal_rebuild_and_page_reads_fail_closed_on_corrupt_committed_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        fs::create_dir_all(&store.dir).unwrap();
        let id = SupervisorRunId::new();

        fs::write(store.journal_path(id), "{broken}\n").unwrap();
        assert!(
            store
                .rebuild_journal_index(id)
                .unwrap_err()
                .to_string()
                .contains("corrupt supervisor event journal")
        );
        assert!(
            store
                .read_journal_page(id, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("corrupt supervisor event journal")
        );

        fs::write(
            store.journal_path(id),
            serde_json::to_vec(&event(1)).unwrap(),
        )
        .unwrap();
        assert!(store.read_journal_page(id, 0, 1).unwrap().is_empty());
    }

    #[test]
    fn compaction_bounds_journal_and_exact_ids_without_reviving_old_events() {
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
        let mut applied = Vec::new();
        for sequence in 1..=u64::try_from(JOURNAL_MAX_EVENTS + 1).unwrap() {
            let item = event(sequence);
            saved = store.apply(id, saved.state_revision, &item).unwrap();
            applied.push(item);
        }

        let index = store.journal_index(id).unwrap();
        assert_eq!(index.entries.len(), JOURNAL_RETAIN_EVENTS);
        assert_eq!(saved.applied_events.len(), JOURNAL_RETAIN_EVENTS);
        assert!(
            store
                .events(id, EventCursor { next_sequence: 1 }, 1)
                .unwrap_err()
                .to_string()
                .contains("cursor expired")
        );

        let revision = saved.state_revision;
        let mut replayed_as_fresh = applied[0].clone();
        replayed_as_fresh.sequence = revision + 1;
        assert!(
            store
                .apply(id, revision, &replayed_as_fresh)
                .unwrap_err()
                .to_string()
                .contains("outside the retained idempotency window")
        );
        assert_eq!(store.load(id).unwrap().unwrap().state_revision, revision);

        let fresh = event(revision + 1);
        assert_eq!(
            store.apply(id, revision, &fresh).unwrap().state_revision,
            revision + 1
        );
    }

    #[test]
    fn malformed_compaction_tombstones_fail_closed_on_snapshot_load() {
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
        let mut document = serde_json::to_value(run).unwrap();
        document["compacted_event_tombstones"] = serde_json::json!([1]);
        fs::write(
            store.snapshot_path(id),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();

        assert!(
            store
                .load(id)
                .unwrap_err()
                .to_string()
                .contains("invalid compaction tombstone")
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
        fs::write(store.journal_path(id), "{final-corrupt\n").unwrap();
        assert!(
            store
                .load(id)
                .unwrap_err()
                .to_string()
                .contains("corrupt supervisor event journal")
        );

        // A JSON value without its LF commit marker is still crash-torn even
        // when the bytes happen to parse. It must not enter the index or a
        // replay checkpoint, and the next append truncates it first.
        let torn = event(1);
        fs::write(store.journal_path(id), serde_json::to_vec(&torn).unwrap()).unwrap();
        assert_eq!(store.load(id).unwrap().unwrap().state_revision, 0);
        let index = store.journal_index(id).unwrap();
        assert_eq!(index.valid_len, 0);
        assert!(index.entries.is_empty());
        store
            .checkpoint_current_journal(id, run.state_revision)
            .unwrap();
        let checkpoint: ReplayCheckpoint = json_file::read(&store.checkpoint_path(id))
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.journal_offset, 0);
        let repaired = event(1);
        store.append(id, &repaired).unwrap();
        assert_eq!(store.load(id).unwrap().unwrap().state_revision, 1);

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

    /// Pruning runs on every start, including the very first one, and is best
    /// effort there — so an absent directory is "nothing to remove", while a
    /// directory that cannot be listed is a failure the caller may report.
    #[test]
    fn pruning_tolerates_an_absent_directory_and_reports_an_unlistable_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        assert!(!store.dir.exists());
        assert_eq!(store.prune_finished_runs().unwrap(), 0);

        // A regular file where the state directory belongs cannot be listed.
        fs::write(&store.dir, b"not a directory").unwrap();
        let error = store
            .prune_finished_runs()
            .expect_err("an unlistable state directory reported success");
        assert!(
            format!("{error:#}").contains("failed to list supervisor runs"),
            "{error:#}"
        );
    }

    fn indexed_run_fixture() -> (tempfile::TempDir, SupervisorStore, Vec<SupervisorRunId>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let mut caller_runs = Vec::new();
        for index in 0..8 {
            let caller = if index % 3 == 0 { "other" } else { "caller" };
            let mut run = SupervisorRun::new(
                caller.into(),
                format!("task-{index}"),
                "input".into(),
                "policy".into(),
                now() + chrono::Duration::seconds(index),
            );
            run.state = if index % 2 == 0 {
                SupervisorRunState::Running
            } else {
                SupervisorRunState::WaitingForDecision
            };
            if caller == "caller" {
                caller_runs.push(run.supervisor_run_id);
            }
            store.initialize(&run).unwrap();
        }
        (tmp, store, caller_runs)
    }

    #[test]
    fn run_list_validates_arguments_and_its_derived_index() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = SupervisorStore::new(tmp.path());
        assert!(
            empty
                .runs_page("caller", None, 0, 0)
                .unwrap_err()
                .to_string()
                .contains("limit must be positive")
        );
        assert!(
            empty
                .runs_page("caller", None, 0, 1)
                .unwrap()
                .runs
                .is_empty()
        );
        assert!(
            empty
                .runs_page("caller", None, 1, 1)
                .unwrap_err()
                .to_string()
                .contains("cursor is outside")
        );

        let (_tmp, store, caller_runs) = indexed_run_fixture();
        let first = store.runs_page("caller", None, 0, 2).unwrap();
        assert!(
            first
                .runs
                .iter()
                .all(|run| caller_runs.contains(&run.supervisor_run_id))
        );

        let valid_index = store.run_list_index().unwrap();
        let mut wrong_count = valid_index.clone();
        wrong_count.entries.pop();
        assert!(!store.run_list_index_is_valid(&wrong_count).unwrap());
        let mut duplicate = valid_index.clone();
        duplicate.entries[1] = duplicate.entries[0].clone();
        assert!(!store.run_list_index_is_valid(&duplicate).unwrap());
        let mut missing = valid_index;
        missing.entries.last_mut().unwrap().supervisor_run_id = SupervisorRunId::new();
        assert!(!store.run_list_index_is_valid(&missing).unwrap());
    }

    #[test]
    fn run_list_pages_hydrate_only_selected_aggregates_and_bound_serialized_bytes() {
        let (_tmp, store, caller_runs) = indexed_run_fixture();
        store.run_snapshots_read.set(0);
        let first = store.runs_page("caller", None, 0, 2).unwrap();
        assert_eq!(first.runs.len(), 2);
        assert!(first.next_cursor.is_some());
        assert_eq!(store.run_snapshots_read.get(), 2);
        assert!(
            store
                .runs_page("nobody", None, 0, 2)
                .unwrap()
                .runs
                .is_empty()
        );

        store.run_snapshots_read.set(0);
        let waiting = store
            .runs_page("caller", Some(SupervisorRunState::WaitingForDecision), 0, 2)
            .unwrap();
        assert_eq!(waiting.runs.len(), 2);
        assert!(
            waiting
                .runs
                .iter()
                .all(|run| run.state == SupervisorRunState::WaitingForDecision)
        );
        assert_eq!(store.run_snapshots_read.get(), 2);

        let first_query = store.load(caller_runs[0]).unwrap().unwrap().query();
        let one_run_budget = serde_json::to_vec(&SupervisorRunPage {
            runs: vec![first_query],
            next_cursor: Some("2".into()),
        })
        .unwrap()
        .len();
        let byte_page = store
            .runs_page_with_budget("caller", None, 0, 100, one_run_budget)
            .unwrap();
        assert_eq!(byte_page.runs.len(), 1);
        assert!(byte_page.next_cursor.is_some());
        assert!(
            store
                .runs_page_with_budget("caller", None, 0, 100, one_run_budget - 1)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
    }

    #[test]
    fn run_list_rebuilds_once_and_falls_back_from_stale_or_unreadable_state() {
        let (tmp, store, caller_runs) = indexed_run_fixture();
        let reopened = SupervisorStore::new(tmp.path());
        let snapshot_count = reopened.snapshot_count().unwrap();
        reopened.run_snapshots_read.set(0);
        assert_eq!(
            reopened.runs_page("caller", None, 0, 1).unwrap().runs.len(),
            1
        );
        assert_eq!(
            reopened.run_snapshots_read.get(),
            u64::try_from(snapshot_count + 1).unwrap()
        );
        reopened.run_snapshots_read.set(0);
        assert_eq!(
            reopened.runs_page("caller", None, 0, 1).unwrap().runs.len(),
            1
        );
        assert_eq!(reopened.run_snapshots_read.get(), 1);

        let mut advanced = store.load(caller_runs[0]).unwrap().unwrap();
        advanced.state = SupervisorRunState::Failed;
        advanced.state_revision += 1;
        json_file::write_atomic(
            &store.dir,
            &store.snapshot_path(advanced.supervisor_run_id),
            &advanced,
        )
        .unwrap();
        assert_eq!(
            store.runs_page("caller", None, 0, 1).unwrap().runs[0].state,
            SupervisorRunState::Failed
        );

        let blocked = tempfile::tempdir().unwrap();
        fs::write(blocked.path().join("supervisor-runs"), "not a directory").unwrap();
        assert!(
            SupervisorStore::new(blocked.path())
                .runs_page("caller", None, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("failed to list supervisor runs")
        );
    }

    #[test]
    fn list_byte_budget_rechecks_a_cursor_that_grows_during_page_trim() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(tmp.path());
        let mut first_query = None;
        for index in 0..=100 {
            let caller = if matches!(index, 98 | 100) {
                "caller"
            } else {
                "other"
            };
            let run = SupervisorRun::new(
                caller.into(),
                format!("task-{index}"),
                "input".into(),
                "policy".into(),
                now() + chrono::Duration::seconds(index),
            );
            if index == 98 {
                first_query = Some(run.query());
            }
            store.initialize(&run).unwrap();
        }
        let budget = serde_json::to_vec(&SupervisorRunPage {
            runs: vec![first_query.unwrap()],
            next_cursor: Some("99".into()),
        })
        .unwrap()
        .len();
        assert!(
            store
                .runs_page_with_budget("caller", None, 0, 100, budget)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
    }

    #[cfg(unix)]
    #[test]
    fn derived_run_index_failures_fall_back_without_hiding_authoritative_runs() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let owner = SupervisorStore::new(tmp.path());
        let run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        owner.initialize(&run).unwrap();
        let second = SupervisorRun::new(
            "caller".into(),
            "task-2".into(),
            "input".into(),
            "policy".into(),
            now() + chrono::Duration::seconds(1),
        );
        owner.initialize(&second).unwrap();

        let cold = SupervisorStore::new(tmp.path());
        cold.refresh_run_list_index(&run);
        assert!(!cold.run_list_index_trusted.get());
        cold.remove_from_run_list_index(&[]);
        cold.remove_from_run_list_index(&[run.supervisor_run_id]);

        assert_eq!(cold.runs_page("caller", None, 0, 1).unwrap().runs.len(), 1);
        fs::remove_file(cold.run_list_index_path()).unwrap();
        cold.refresh_run_list_index(&run);
        assert!(!cold.run_list_index_trusted.get());

        assert_eq!(cold.runs_page("caller", None, 0, 1).unwrap().runs.len(), 1);
        fs::remove_file(cold.run_list_index_path()).unwrap();
        cold.remove_from_run_list_index(&[run.supervisor_run_id]);
        assert!(!cold.run_list_index_trusted.get());

        assert_eq!(cold.runs_page("caller", None, 0, 1).unwrap().runs.len(), 1);
        let mode = fs::metadata(&cold.dir).unwrap().permissions().mode();
        fs::set_permissions(&cold.dir, fs::Permissions::from_mode(0o555)).unwrap();
        cold.refresh_run_list_index(&run);
        assert!(!cold.run_list_index_trusted.get());
        fs::set_permissions(&cold.dir, fs::Permissions::from_mode(mode)).unwrap();

        assert_eq!(cold.runs_page("caller", None, 0, 1).unwrap().runs.len(), 1);
        fs::set_permissions(&cold.dir, fs::Permissions::from_mode(0o555)).unwrap();
        cold.remove_from_run_list_index(&[run.supervisor_run_id]);
        assert!(!cold.run_list_index_trusted.get());
        fs::set_permissions(&cold.dir, fs::Permissions::from_mode(mode)).unwrap();

        let rebuild = SupervisorStore::new(tmp.path());
        fs::set_permissions(&rebuild.dir, fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(
            rebuild.runs_page("caller", None, 0, 1).unwrap().runs.len(),
            1
        );
        assert!(!rebuild.run_list_index_trusted.get());
        fs::set_permissions(&rebuild.dir, fs::Permissions::from_mode(mode)).unwrap();
    }
}
