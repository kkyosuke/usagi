//! Terminal lifetime and attachment registry.
//!
//! The registry is deliberately independent of a concrete PTY implementation.
//! The daemon's actor owns one instance and supplies output/exit observations;
//! this keeps all fencing, cursor and input-deduplication decisions in one
//! serial turn.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{ClientId, ConnectionId, RequestId, TerminalRef};

/// Maximum terminal bytes retained for attach/resync and incremental replay.
///
/// A JSON byte array can require four payload bytes per terminal byte. Keeping
/// this window at 64 KiB leaves ample room for the response envelope and
/// terminal identity inside the protocol's one MiB frame limit.
pub const MAX_RETAINED_OUTPUT_BYTES: usize = 64 * 1024;

static RETENTION_DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static RETENTION_COALESCED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Process-local terminal retention counters. Values are byte counts only and
/// never contain terminal output or identity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputPipelineCounters {
    pub dropped_bytes: u64,
    pub coalesced_bytes: u64,
}

#[must_use]
pub fn output_pipeline_counters() -> OutputPipelineCounters {
    OutputPipelineCounters {
        dropped_bytes: RETENTION_DROPPED_BYTES.load(Ordering::Relaxed),
        coalesced_bytes: RETENTION_COALESCED_BYTES.load(Ordering::Relaxed),
    }
}

/// The durable process state shared by every daemon-owned terminal.
///
/// Agent adapters (Claude/Codex) and the generic shell path differ only in
/// how they resolve a launch; once a `TerminalRef` is reserved, they use this
/// same lifecycle vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalRuntimeState {
    Reserved,
    Running,
    Exited,
    Reclaimed,
    ReconcileRequired(TerminalReconcileState),
    SpawnFailed,
}

/// A fail-closed condition that must be reconciled, never replaced by spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReconcileState {
    SpawnAmbiguous,
    PersistAfterSpawn,
    IdentityUnknown,
    OrphanRunning,
    PersistAfterExit,
}

/// Result of spawning a terminal PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnFailure {
    Definite,
    Ambiguous,
}

/// The effective terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
}

/// A point-in-time terminal view returned by attach and resync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub terminal: TerminalRef,
    pub revision: u64,
    /// Offset of the first byte in `replay`.
    pub base_offset: u64,
    pub output_offset: u64,
    pub geometry: Geometry,
    pub replay: Vec<u8>,
    pub exited: Option<i32>,
}

/// A retained contiguous output segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Output {
    pub terminal: TerminalRef,
    pub start_offset: u64,
    pub end_offset: u64,
    pub data: Vec<u8>,
}

/// Events observed by an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Output(Output),
    Exited {
        terminal: TerminalRef,
        revision: u64,
        final_output_offset: u64,
        status: i32,
    },
    ResyncRequired {
        terminal: TerminalRef,
    },
}

/// Result of atomically registering an attachment and taking its initial view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attached {
    pub subscription: u64,
    pub snapshot: Snapshot,
}

/// Result of an input write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum InputAck {
    Written,
    Failed,
    Ambiguous { applied_prefix: usize },
    Cached(Box<InputAck>),
}

/// A fakeable PTY writer.  The actual PTY adapter must return only after bytes
/// were accepted by the master endpoint.
pub trait PtyWriter {
    /// Selects the daemon-owned PTY that receives the following write.  Fake
    /// writers may ignore it; real multiplexing adapters use the full fenced
    /// terminal identity rather than a client-selected process handle.
    fn select_terminal(&mut self, _terminal: &TerminalRef) {}
    /// Resize the daemon-owned PTY. The default keeps existing injected writers
    /// focused on input semantics.
    ///
    /// # Errors
    ///
    /// Returns a safe PTY error when geometry cannot be applied.
    fn resize(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<(), PtyWriteError> {
        Ok(())
    }
    /// # Errors
    ///
    /// Returns the number of bytes that may have reached the PTY on failure.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError>;
}

/// A write failure, including a prefix which may already have reached the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyWriteError {
    pub applied_prefix: usize,
}

/// The authenticated input identity carried by one terminal-key command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRequest {
    pub subscription: u64,
    pub connection: ConnectionId,
    pub client: ClientId,
    pub request: RequestId,
    pub input_seq: u64,
}

/// Registry failures are explicit so stale references never fall back to names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// The output cursor predates the bounded journal. The terminal identity
    /// remains valid, so the client must attach again and replace its screen.
    ResyncRequired,
    StaleTarget,
    UnknownSubscription,
    NotAttached,
    SequenceGap,
    IdempotencyExpired,
    Exited,
}

#[derive(Debug)]
struct Entry {
    reference: TerminalRef,
    revision: u64,
    geometry: Geometry,
    journal: VecDeque<Output>,
    retained_bytes: usize,
    next_offset: u64,
    exited: Option<i32>,
    attachments: BTreeMap<u64, ConnectionId>,
    next_subscription: u64,
    inputs: BTreeMap<ClientId, InputLedger>,
}

#[derive(Debug, Default)]
struct InputLedger {
    next_seq: u64,
    // Keep a bounded, ordered result cache. A request ID fences retries from a
    // reused sequence on a different connection.
    entries: VecDeque<(u64, RequestId, InputAck)>,
}

/// A daemon-owned terminal registry.  Callers must serialize calls for a given
/// terminal (normally with a terminal actor).
#[derive(Debug)]
pub struct TerminalRegistry {
    entries: BTreeMap<String, Entry>,
    journal_limit: usize,
    input_cache_limit: usize,
}

impl TerminalRegistry {
    #[must_use]
    #[coverage(off)]
    pub fn new(journal_limit: usize, input_cache_limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            journal_limit: journal_limit.min(MAX_RETAINED_OUTPUT_BYTES),
            input_cache_limit,
        }
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when this terminal identity was
    /// already registered.
    #[coverage(off)]
    pub fn register(
        &mut self,
        reference: TerminalRef,
        geometry: Geometry,
    ) -> Result<(), RegistryError> {
        let key = key(&reference);
        if self.entries.contains_key(&key) {
            return Err(RegistryError::StaleTarget);
        }
        self.entries.insert(
            key,
            Entry {
                reference,
                revision: 0,
                geometry,
                journal: VecDeque::new(),
                retained_bytes: 0,
                next_offset: 0,
                exited: None,
                attachments: BTreeMap::new(),
                next_subscription: 1,
                inputs: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a different generation or
    /// ownership scope.
    #[coverage(off)]
    pub fn attach(
        &mut self,
        reference: &TerminalRef,
        connection: ConnectionId,
    ) -> Result<Attached, RegistryError> {
        let entry = self.entry_mut(reference)?;
        if let Some(subscription) = entry
            .attachments
            .iter()
            .find_map(|(subscription, owner)| (*owner == connection).then_some(*subscription))
        {
            return Ok(Attached {
                subscription,
                snapshot: snapshot(entry),
            });
        }
        let subscription = entry.next_subscription;
        entry.next_subscription += 1;
        entry.attachments.insert(subscription, connection);
        Ok(Attached {
            subscription,
            snapshot: snapshot(entry),
        })
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::UnknownSubscription`] unless this connection
    /// owns the exact subscription.
    #[coverage(off)]
    pub fn detach(
        &mut self,
        reference: &TerminalRef,
        subscription: u64,
        connection: ConnectionId,
    ) -> Result<(), RegistryError> {
        let entry = self.entry_mut(reference)?;
        match entry.attachments.get(&subscription) {
            Some(owner) if *owner == connection => {
                entry.attachments.remove(&subscription);
                Ok(())
            }
            _ => Err(RegistryError::UnknownSubscription),
        }
    }

    /// Releases only this connection's subscriptions.  It intentionally leaves
    /// the PTY, output journal and process ownership alive.
    #[coverage(off)]
    pub fn disconnect(&mut self, connection: ConnectionId) {
        for entry in self.entries.values_mut() {
            entry.attachments.retain(|_, owner| *owner != connection);
        }
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when the terminal is not owned by
    /// this registry.
    ///
    /// # Panics
    ///
    /// Panics only if an internal retained-byte accounting invariant is broken.
    #[coverage(off)]
    pub fn append_output(
        &mut self,
        reference: &TerminalRef,
        data: Vec<u8>,
    ) -> Result<Output, RegistryError> {
        let limit = self.journal_limit;
        let entry = self.entry_mut(reference)?;
        let start_offset = entry.next_offset;
        entry.next_offset += data.len() as u64;
        let output = Output {
            terminal: entry.reference.clone(),
            start_offset,
            end_offset: entry.next_offset,
            data,
        };
        if output.data.len() >= limit {
            let dropped = entry
                .retained_bytes
                .saturating_add(output.data.len().saturating_sub(limit));
            RETENTION_DROPPED_BYTES.fetch_add(
                u64::try_from(dropped).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            entry.journal.clear();
            entry.retained_bytes = limit;
            if limit != 0 {
                entry.journal.push_back(Output {
                    terminal: output.terminal.clone(),
                    start_offset: output.end_offset - limit as u64,
                    end_offset: output.end_offset,
                    data: output.data[output.data.len() - limit..].to_vec(),
                });
            }
        } else {
            entry.retained_bytes += output.data.len();
            if let Some(tail) = entry.journal.back_mut() {
                tail.end_offset = output.end_offset;
                tail.data.extend_from_slice(&output.data);
                RETENTION_COALESCED_BYTES.fetch_add(
                    u64::try_from(output.data.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            } else {
                entry.journal.push_back(output.clone());
            }
            if entry.retained_bytes > limit {
                let overflow = entry.retained_bytes - limit;
                let front = entry
                    .journal
                    .front_mut()
                    .expect("retained output has a journal segment");
                front.data.drain(..overflow);
                front.start_offset += overflow as u64;
                entry.retained_bytes -= overflow;
                RETENTION_DROPPED_BYTES.fetch_add(
                    u64::try_from(overflow).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }
        }
        Ok(output)
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when the reference is stale, or
    /// [`RegistryError::ResyncRequired`] when the cursor has fallen out of the
    /// bounded journal.
    #[coverage(off)]
    pub fn replay_from(
        &self,
        reference: &TerminalRef,
        offset: u64,
    ) -> Result<Vec<Output>, RegistryError> {
        let entry = self.entry(reference)?;
        let oldest = entry
            .journal
            .front()
            .map_or(entry.next_offset, |segment| segment.start_offset);
        if offset < oldest || offset > entry.next_offset {
            return Err(RegistryError::ResyncRequired);
        }
        Ok(entry
            .journal
            .iter()
            .filter(|segment| segment.end_offset > offset)
            .map(|segment| {
                if segment.start_offset >= offset {
                    return segment.clone();
                }
                let remaining = usize::try_from(segment.end_offset - offset).unwrap_or(0);
                let consumed = segment.data.len().saturating_sub(remaining);
                Output {
                    terminal: segment.terminal.clone(),
                    start_offset: offset,
                    end_offset: segment.end_offset,
                    data: segment.data[consumed..].to_vec(),
                }
            })
            .collect())
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal.
    #[coverage(off)]
    pub fn resize(
        &mut self,
        reference: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Snapshot, RegistryError> {
        let entry = self.entry_mut(reference)?;
        entry.geometry = geometry;
        entry.revision += 1;
        Ok(snapshot(entry))
    }

    /// # Errors
    ///
    /// Returns a fencing, attachment, or input-sequencing error without
    /// writing any bytes.
    #[coverage(off)]
    pub fn write_input<W: PtyWriter>(
        &mut self,
        reference: &TerminalRef,
        input: InputRequest,
        bytes: &[u8],
        writer: &mut W,
    ) -> Result<InputAck, RegistryError> {
        let input_cache_limit = self.input_cache_limit;
        let entry = self.entry_mut(reference)?;
        if entry.attachments.get(&input.subscription) != Some(&input.connection) {
            return Err(RegistryError::NotAttached);
        }
        if entry.exited.is_some() {
            return Err(RegistryError::Exited);
        }
        let ledger = entry.inputs.entry(input.client).or_default();
        if input.input_seq < ledger.next_seq {
            return ledger
                .entries
                .iter()
                .find(|(seq, id, _)| *seq == input.input_seq && *id == input.request)
                .map(|(_, _, ack)| InputAck::Cached(Box::new(ack.clone())))
                .ok_or(RegistryError::IdempotencyExpired);
        }
        if input.input_seq > ledger.next_seq {
            return Err(RegistryError::SequenceGap);
        }
        let ack = match writer.write_all(bytes) {
            Ok(()) => InputAck::Written,
            Err(error) if error.applied_prefix == 0 => InputAck::Failed,
            Err(error) => InputAck::Ambiguous {
                applied_prefix: error.applied_prefix,
            },
        };
        ledger.next_seq += 1;
        ledger
            .entries
            .push_back((input.input_seq, input.request, ack.clone()));
        while ledger.entries.len() > input_cache_limit {
            ledger.entries.pop_front();
        }
        Ok(ack)
    }

    /// Commits exit only after the caller has drained PTY output into the journal.
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal.
    #[coverage(off)]
    pub fn exited(&mut self, reference: &TerminalRef, status: i32) -> Result<Event, RegistryError> {
        let entry = self.entry_mut(reference)?;
        entry.exited = Some(status);
        entry.revision += 1;
        Ok(Event::Exited {
            terminal: entry.reference.clone(),
            revision: entry.revision,
            final_output_offset: entry.next_offset,
            status,
        })
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal.
    #[coverage(off)]
    pub fn snapshot(&self, reference: &TerminalRef) -> Result<Snapshot, RegistryError> {
        Ok(snapshot(self.entry(reference)?))
    }

    #[coverage(off)]
    fn entry(&self, reference: &TerminalRef) -> Result<&Entry, RegistryError> {
        self.entries
            .get(&key(reference))
            .filter(|entry| entry.reference.fences(reference))
            .ok_or(RegistryError::StaleTarget)
    }
    #[coverage(off)]
    fn entry_mut(&mut self, reference: &TerminalRef) -> Result<&mut Entry, RegistryError> {
        self.entries
            .get_mut(&key(reference))
            .filter(|entry| entry.reference.fences(reference))
            .ok_or(RegistryError::StaleTarget)
    }
}

#[coverage(off)]
fn key(reference: &TerminalRef) -> String {
    reference.terminal_id.as_str()
}
#[coverage(off)]
fn snapshot(entry: &Entry) -> Snapshot {
    let base_offset = entry
        .journal
        .front()
        .map_or(entry.next_offset, |segment| segment.start_offset);
    let mut replay = Vec::with_capacity(entry.retained_bytes);
    for segment in &entry.journal {
        replay.extend_from_slice(&segment.data);
    }
    Snapshot {
        terminal: entry.reference.clone(),
        revision: entry.revision,
        base_offset,
        output_offset: entry.next_offset,
        geometry: entry.geometry,
        replay,
        exited: entry.exited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };
    use usagi_core::infrastructure::ipc::{DEFAULT_MAX_FRAME_BYTES, write_json_frame};

    #[derive(Default)]
    struct Writer {
        written: Vec<u8>,
        failure: Option<usize>,
    }
    impl PtyWriter for Writer {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            self.written.extend_from_slice(bytes);
            self.failure.map_or(Ok(()), |applied_prefix| {
                Err(PtyWriteError { applied_prefix })
            })
        }
    }
    fn reference() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }
    fn registry(reference: TerminalRef) -> TerminalRegistry {
        let mut registry = TerminalRegistry::new(4, 2);
        registry
            .register(reference, Geometry { cols: 80, rows: 24 })
            .unwrap();
        registry
    }
    fn input(
        subscription: u64,
        connection: ConnectionId,
        client: ClientId,
        request: RequestId,
        input_seq: u64,
    ) -> InputRequest {
        InputRequest {
            subscription,
            connection,
            client,
            request,
            input_seq,
        }
    }

    #[test]
    fn attach_is_atomic_and_disconnect_keeps_terminal() {
        let r = reference();
        let mut registry = registry(r.clone());
        let c = ConnectionId::new();
        let attached = registry.attach(&r, c).unwrap();
        assert_eq!(attached.snapshot.output_offset, 0);
        assert_eq!(
            registry.attach(&r, c).unwrap().subscription,
            attached.subscription
        );
        registry.disconnect(c);
        assert!(registry.snapshot(&r).is_ok());
        assert_eq!(
            registry.detach(&r, attached.subscription, c),
            Err(RegistryError::UnknownSubscription)
        );
    }
    #[test]
    fn duplicate_registration_and_exact_detach_are_fenced() {
        let r = reference();
        let mut registry = registry(r.clone());
        assert_eq!(
            registry.register(r.clone(), Geometry { cols: 80, rows: 24 }),
            Err(RegistryError::StaleTarget)
        );
        let connection = ConnectionId::new();
        let subscription = registry.attach(&r, connection).unwrap().subscription;
        assert_eq!(registry.detach(&r, subscription, connection), Ok(()));
    }
    #[test]
    fn output_offsets_are_contiguous_and_old_output_requires_resync() {
        let r = reference();
        let mut registry = registry(r.clone());
        assert_eq!(
            Writer::default().resize(&r, Geometry { cols: 80, rows: 24 }),
            Ok(())
        );
        assert_eq!(
            registry
                .append_output(&r, b"abc".to_vec())
                .unwrap()
                .end_offset,
            3
        );
        assert_eq!(
            registry
                .append_output(&r, b"def".to_vec())
                .unwrap()
                .start_offset,
            3
        );
        assert_eq!(
            registry.replay_from(&r, 0),
            Err(RegistryError::ResyncRequired)
        );
        assert_eq!(registry.replay_from(&r, 3).unwrap()[0].data, b"def");
        assert_eq!(registry.replay_from(&r, 4).unwrap()[0].data, b"ef");
        assert_eq!(
            registry.replay_from(&r, 7),
            Err(RegistryError::ResyncRequired)
        );
        let snapshot = registry.snapshot(&r).unwrap();
        assert_eq!(snapshot.base_offset, 2);
        assert_eq!(snapshot.output_offset, 6);
        assert_eq!(snapshot.replay, b"cdef");
    }
    #[test]
    fn oversized_output_retains_an_exact_frame_safe_tail() {
        let r = reference();
        let mut registry = TerminalRegistry::new(usize::MAX, 1);
        registry
            .register(r.clone(), Geometry { cols: 80, rows: 24 })
            .unwrap();
        let bytes = vec![7; MAX_RETAINED_OUTPUT_BYTES + 17];
        let output = registry.append_output(&r, bytes.clone()).unwrap();
        assert_eq!(output.data, bytes);
        let snapshot = registry.snapshot(&r).unwrap();
        assert_eq!(snapshot.base_offset, 17);
        assert_eq!(snapshot.output_offset, bytes.len() as u64);
        assert_eq!(snapshot.replay.len(), MAX_RETAINED_OUTPUT_BYTES);
        assert_eq!(
            registry.replay_from(&r, 17).unwrap()[0].data.len(),
            MAX_RETAINED_OUTPUT_BYTES
        );
        assert_eq!(
            registry.replay_from(&r, 16),
            Err(RegistryError::ResyncRequired)
        );
    }
    #[test]
    fn multi_megabyte_producers_keep_attach_and_resume_frames_bounded() {
        let counters_before = output_pipeline_counters();
        let first = reference();
        let second = reference();
        let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 1);
        for terminal in [&first, &second] {
            registry
                .register(terminal.clone(), Geometry { cols: 80, rows: 24 })
                .unwrap();
        }
        let chunk = vec![b'x'; 4096];
        for _ in 0..300 {
            registry.append_output(&first, chunk.clone()).unwrap();
            registry.append_output(&second, chunk.clone()).unwrap();
        }

        for terminal in [&first, &second] {
            let connection = ConnectionId::new();
            let attached = registry.attach(terminal, connection).unwrap();
            for _ in 0..8 {
                let reattached = registry.attach(terminal, connection).unwrap();
                assert_eq!(reattached.subscription, attached.subscription);
                assert_eq!(
                    reattached.snapshot.output_offset,
                    attached.snapshot.output_offset
                );
            }
            assert_eq!(attached.snapshot.replay.len(), MAX_RETAINED_OUTPUT_BYTES);
            assert_eq!(
                attached.snapshot.base_offset + attached.snapshot.replay.len() as u64,
                attached.snapshot.output_offset
            );
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &attached, DEFAULT_MAX_FRAME_BYTES).unwrap();
            assert!(frame.len() < DEFAULT_MAX_FRAME_BYTES);

            let cursor = attached.snapshot.base_offset + 123;
            let resumed = registry.replay_from(terminal, cursor).unwrap();
            assert_eq!(resumed.len(), 1);
            assert_eq!(resumed[0].start_offset, cursor);
            assert_eq!(resumed[0].end_offset, attached.snapshot.output_offset);
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &resumed, DEFAULT_MAX_FRAME_BYTES).unwrap();
            assert!(frame.len() < DEFAULT_MAX_FRAME_BYTES);
        }
        let counters_after = output_pipeline_counters();
        assert!(counters_after.dropped_bytes > counters_before.dropped_bytes);
        assert!(counters_after.coalesced_bytes > counters_before.coalesced_bytes);
    }
    #[test]
    fn input_is_acked_only_once_after_write_and_partial_is_ambiguous() {
        let r = reference();
        let mut registry = registry(r.clone());
        let connection = ConnectionId::new();
        let subscription = registry.attach(&r, connection).unwrap().subscription;
        let client = ClientId::new();
        let request = RequestId::new();
        let mut writer = Writer::default();
        assert_eq!(
            registry
                .write_input(
                    &r,
                    input(subscription, connection, client, request, 0),
                    b"ok",
                    &mut writer
                )
                .unwrap(),
            InputAck::Written
        );
        assert_eq!(
            registry
                .write_input(
                    &r,
                    input(subscription, connection, client, request, 0),
                    b"ok",
                    &mut writer
                )
                .unwrap(),
            InputAck::Cached(Box::new(InputAck::Written))
        );
        assert_eq!(writer.written, b"ok");
        let mut partial = Writer {
            written: Vec::new(),
            failure: Some(1),
        };
        assert_eq!(
            registry
                .write_input(
                    &r,
                    input(subscription, connection, client, RequestId::new(), 1),
                    b"x",
                    &mut partial
                )
                .unwrap(),
            InputAck::Ambiguous { applied_prefix: 1 }
        );
        assert_eq!(
            registry.write_input(
                &r,
                input(subscription, connection, client, RequestId::new(), 3),
                b"gap",
                &mut writer
            ),
            Err(RegistryError::SequenceGap)
        );
        let mut failed = Writer {
            written: Vec::new(),
            failure: Some(0),
        };
        assert_eq!(
            registry
                .write_input(
                    &r,
                    input(subscription, connection, client, RequestId::new(), 2),
                    b"fail",
                    &mut failed
                )
                .unwrap(),
            InputAck::Failed
        );
        assert_eq!(
            registry.write_input(
                &r,
                input(subscription, connection, client, request, 0),
                b"old",
                &mut writer
            ),
            Err(RegistryError::IdempotencyExpired)
        );
    }
    #[test]
    fn stale_refs_and_wrong_attachment_are_rejected() {
        let r = reference();
        let mut registry = registry(r.clone());
        let mut stale = r.clone();
        stale.worktree_id = WorktreeId::new();
        assert_eq!(registry.snapshot(&stale), Err(RegistryError::StaleTarget));
        assert_eq!(
            registry.write_input(
                &r,
                input(1, ConnectionId::new(), ClientId::new(), RequestId::new(), 0),
                b"x",
                &mut Writer::default()
            ),
            Err(RegistryError::NotAttached)
        );
    }
    #[test]
    fn resize_and_exit_follow_final_output() {
        let r = reference();
        let mut registry = registry(r.clone());
        registry.append_output(&r, b"done".to_vec()).unwrap();
        let snapshot = registry
            .resize(
                &r,
                Geometry {
                    cols: 100,
                    rows: 30,
                },
            )
            .unwrap();
        assert_eq!(snapshot.geometry.cols, 100);
        assert_eq!(
            registry.exited(&r, 0).unwrap(),
            Event::Exited {
                terminal: r.clone(),
                revision: 2,
                final_output_offset: 4,
                status: 0,
            }
        );
        let connection = ConnectionId::new();
        let subscription = registry.attach(&r, connection).unwrap().subscription;
        assert_eq!(
            registry.write_input(
                &r,
                input(
                    subscription,
                    connection,
                    ClientId::new(),
                    RequestId::new(),
                    0
                ),
                b"x",
                &mut Writer::default()
            ),
            Err(RegistryError::Exited)
        );
    }
}
