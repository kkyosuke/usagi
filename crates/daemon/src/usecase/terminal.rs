//! Terminal lifetime and attachment registry.
//!
//! The registry is deliberately independent of a concrete PTY implementation.
//! The daemon's actor owns one instance and supplies output/exit observations;
//! this keeps all fencing, cursor and input-deduplication decisions in one
//! serial turn.
//!
//! The registry is also the terminal **grid authority**: every terminal owns one
//! [`VtScreen`], fed with the bytes the PTY produced and resized with the
//! terminal, so an attaching client is handed a complete semantic screen
//! checkpoint instead of a raw byte tail cut at an arbitrary boundary. The
//! bounded raw journal stays: it serves the incremental `Resume` suffix a
//! client feeds into the screen restored from the checkpoint.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{ClientId, ConnectionId, RequestId, TerminalRef};
use usagi_core::infrastructure::ipc::TERMINAL_CHECKPOINT_REVISION;
use usagi_core::usecase::vt_screen::{
    CHECKPOINT_BYTES_MAX, COLS_MAX, ROWS_MAX, ScreenCheckpoint, VtScreen,
};

/// Maximum terminal bytes retained for attach/resync and incremental replay.
///
/// A JSON byte array can require four payload bytes per terminal byte. Keeping
/// this window at 64 KiB leaves ample room for the response envelope and
/// terminal identity inside the protocol's one MiB frame limit.
pub const MAX_RETAINED_OUTPUT_BYTES: usize = 64 * 1024;

/// Cells one terminal's screen may retain (both buffers' visible grid plus
/// their scrollback). A decoded cell costs roughly 32 bytes plus its style, so
/// this bounds a single terminal at about 16 MiB of screen state.
pub const SCREEN_CELLS_PER_TERMINAL_MAX: usize = 512 * 1024;

/// Process-local ceiling for the cells retained by every daemon-owned screen,
/// about 64 MiB of screen state.
///
/// It is enforced on the terminal that just grew: that terminal is trimmed to
/// whatever the ceiling leaves after the other terminals' current retention, so
/// the process total stays at or below the ceiling. A newly registered terminal
/// adds only its visible grid before its first output is accounted for.
pub const SCREEN_CELLS_AGGREGATE_MAX: usize = 2 * 1024 * 1024;

static RETENTION_DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static RETENTION_COALESCED_BYTES: AtomicU64 = AtomicU64::new(0);
static SCREEN_TRIMMED_ROWS: AtomicU64 = AtomicU64::new(0);
static CHECKPOINT_TRIMMED_ROWS: AtomicU64 = AtomicU64::new(0);
static RETAINED_SCREEN_CELLS: AtomicU64 = AtomicU64::new(0);

/// Process-local terminal retention counters. Values are byte, row and cell
/// counts only and never contain terminal output or identity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputPipelineCounters {
    pub dropped_bytes: u64,
    pub coalesced_bytes: u64,
    /// Scrollback rows dropped from a screen to keep retention inside the
    /// per-terminal and process aggregate cell budgets.
    pub screen_trimmed_rows: u64,
    /// Scrollback rows dropped from a checkpoint payload to keep it inside the
    /// frame budget. The screen itself keeps those rows.
    pub checkpoint_trimmed_rows: u64,
    /// Cells currently retained by daemon-owned screens in this process.
    pub retained_screen_cells: u64,
}

#[must_use]
pub fn output_pipeline_counters() -> OutputPipelineCounters {
    OutputPipelineCounters {
        dropped_bytes: RETENTION_DROPPED_BYTES.load(Ordering::Relaxed),
        coalesced_bytes: RETENTION_COALESCED_BYTES.load(Ordering::Relaxed),
        screen_trimmed_rows: SCREEN_TRIMMED_ROWS.load(Ordering::Relaxed),
        checkpoint_trimmed_rows: CHECKPOINT_TRIMMED_ROWS.load(Ordering::Relaxed),
        retained_screen_cells: RETAINED_SCREEN_CELLS.load(Ordering::Relaxed),
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
///
/// It holds both wire payloads: the legacy raw tail retained by the bounded
/// journal, and the semantic checkpoint of the authoritative screen. The
/// negotiated wire revision selects exactly one when the view is projected onto
/// the wire with [`into_frame`](Self::into_frame), so one frame never carries
/// both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub terminal: TerminalRef,
    pub revision: u64,
    /// Offset of the first byte in `replay`.
    pub base_offset: u64,
    pub output_offset: u64,
    pub geometry: Geometry,
    pub replay: Vec<u8>,
    /// The complete screen state at `output_offset`.
    pub screen: Box<ScreenCheckpoint>,
    pub exited: Option<i32>,
}

/// Which snapshot payload a negotiated wire revision receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotWire {
    /// Generation 1 revision 1: the legacy raw byte tail. Kept for the
    /// migration window so an older client observes its existing contract.
    #[default]
    RawTail,
    /// Generation 1 revision 2: the semantic screen checkpoint.
    ScreenCheckpoint,
}

impl SnapshotWire {
    /// The payload a negotiated generation 1 revision receives.
    #[must_use]
    pub const fn for_revision(revision: u16) -> Self {
        if revision >= TERMINAL_CHECKPOINT_REVISION {
            Self::ScreenCheckpoint
        } else {
            Self::RawTail
        }
    }
}

/// One negotiated wire payload of a snapshot. Revision 1 carries the raw tail
/// `[base_offset, output_offset)`; revision 2 carries the checkpoint, which is
/// complete at `output_offset` and therefore has no tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SnapshotContent {
    RawTail { replay: Vec<u8> },
    Screen { screen: Box<ScreenCheckpoint> },
}

/// A [`Snapshot`] narrowed to one negotiated wire revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotFrame {
    pub terminal: TerminalRef,
    pub revision: u64,
    pub base_offset: u64,
    pub output_offset: u64,
    pub geometry: Geometry,
    #[serde(flatten)]
    pub content: SnapshotContent,
    pub exited: Option<i32>,
}

impl Snapshot {
    /// Narrows this view to the payload the negotiated revision expects.
    ///
    /// A checkpoint represents the screen exactly at `output_offset`, so its
    /// frame reports `base_offset == output_offset`: the client resumes from
    /// there and never feeds a tail into a restored screen twice.
    #[must_use]
    pub fn into_frame(self, wire: SnapshotWire) -> SnapshotFrame {
        let (base_offset, content) = match wire {
            SnapshotWire::RawTail => (
                self.base_offset,
                SnapshotContent::RawTail {
                    replay: self.replay,
                },
            ),
            SnapshotWire::ScreenCheckpoint => (
                self.output_offset,
                SnapshotContent::Screen {
                    screen: self.screen,
                },
            ),
        };
        SnapshotFrame {
            terminal: self.terminal,
            revision: self.revision,
            base_offset,
            output_offset: self.output_offset,
            geometry: self.geometry,
            content,
            exited: self.exited,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub subscription: u64,
    pub snapshot: Snapshot,
}

impl Attached {
    /// Narrows the attached view to the negotiated wire revision.
    #[must_use]
    pub fn into_frame(self, wire: SnapshotWire) -> AttachedFrame {
        AttachedFrame {
            subscription: self.subscription,
            snapshot: self.snapshot.into_frame(wire),
        }
    }
}

/// An [`Attached`] narrowed to one negotiated wire revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachedFrame {
    pub subscription: u64,
    pub snapshot: SnapshotFrame,
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
    /// Releases process-local transport ownership after the terminal exit has
    /// been committed. Implementations must fence the complete terminal
    /// identity and make repeated calls harmless.
    fn release(&mut self, _terminal: &TerminalRef) -> bool {
        false
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
    PtyResizeFailed,
    /// The screen cannot be captured inside the frame budget even with all of
    /// its history dropped. The daemon emits no oversized frame and no partial
    /// screen; the client keeps its current state and retries.
    CheckpointUnavailable,
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
    /// The authoritative decoded screen for this terminal (#199). Every byte
    /// this registry accepts is fed to it, so a checkpoint never depends on
    /// where the bounded journal happens to start.
    screen: VtScreen,
    /// Cells this screen contributed to [`RETAINED_SCREEN_CELLS`] when it was
    /// last accounted for.
    screen_cells: usize,
}

impl Drop for Entry {
    fn drop(&mut self) {
        release_screen_cells(self.screen_cells);
    }
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
    checkpoint_bytes_limit: usize,
    screen_cells_limit: usize,
}

impl TerminalRegistry {
    #[must_use]
    pub fn new(journal_limit: usize, input_cache_limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            journal_limit: journal_limit.min(MAX_RETAINED_OUTPUT_BYTES),
            input_cache_limit,
            checkpoint_bytes_limit: CHECKPOINT_BYTES_MAX,
            screen_cells_limit: SCREEN_CELLS_PER_TERMINAL_MAX,
        }
    }

    /// Overrides the serialized checkpoint budget.
    ///
    /// The default is [`CHECKPOINT_BYTES_MAX`], the largest payload a peer
    /// accepts and the value that keeps a snapshot inside the one MiB frame; a
    /// smaller budget only makes the trimming and fail-closed paths observable
    /// without building a multi-megabyte screen.
    #[must_use]
    pub const fn with_checkpoint_bytes_limit(mut self, bytes: usize) -> Self {
        self.checkpoint_bytes_limit = bytes;
        self
    }

    /// Overrides the per-terminal screen retention budget.
    ///
    /// The default is [`SCREEN_CELLS_PER_TERMINAL_MAX`]. The process-local
    /// [`SCREEN_CELLS_AGGREGATE_MAX`] ceiling still applies: a terminal keeps
    /// the smaller of the two.
    #[must_use]
    pub const fn with_screen_cells_limit(mut self, cells: usize) -> Self {
        self.screen_cells_limit = cells;
        self
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when this terminal identity was
    /// already registered.
    pub fn register(
        &mut self,
        reference: TerminalRef,
        geometry: Geometry,
    ) -> Result<(), RegistryError> {
        let key = key(&reference);
        if self.entries.contains_key(&key) {
            return Err(RegistryError::StaleTarget);
        }
        let (rows, cols) = screen_dimensions(geometry);
        let screen = VtScreen::new(rows, cols);
        let screen_cells = screen.retained_cells();
        reserve_screen_cells(screen_cells);
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
                screen,
                screen_cells,
            },
        );
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a different generation or
    /// ownership scope.
    pub fn attach(
        &mut self,
        reference: &TerminalRef,
        connection: ConnectionId,
    ) -> Result<Attached, RegistryError> {
        let checkpoint_bytes_limit = self.checkpoint_bytes_limit;
        let entry = self.entry_mut(reference)?;
        if let Some(subscription) = entry
            .attachments
            .iter()
            .find_map(|(subscription, owner)| (*owner == connection).then_some(*subscription))
        {
            return Ok(Attached {
                subscription,
                snapshot: snapshot(entry, checkpoint_bytes_limit)?,
            });
        }
        // Capture before the subscription is recorded: a snapshot the client
        // cannot be handed must not leave an attachment behind.
        let snapshot = snapshot(entry, checkpoint_bytes_limit)?;
        let subscription = entry.next_subscription;
        entry.next_subscription += 1;
        entry.attachments.insert(subscription, connection);
        Ok(Attached {
            subscription,
            snapshot,
        })
    }

    /// # Errors
    ///
    /// Returns [`RegistryError::UnknownSubscription`] unless this connection
    /// owns the exact subscription.
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
    pub fn append_output(
        &mut self,
        reference: &TerminalRef,
        data: Vec<u8>,
    ) -> Result<Output, RegistryError> {
        let limit = self.journal_limit;
        let cells_limit = self.screen_cells_limit;
        let entry = self.entry_mut(reference)?;
        // The screen is the authority: it sees every accepted byte, including
        // the bytes the bounded journal is about to drop.
        entry.screen.advance(&data);
        enforce_screen_budget(entry, cells_limit);
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
    pub fn resize(
        &mut self,
        reference: &TerminalRef,
        geometry: Geometry,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, RegistryError> {
        // Hold the registry's exclusive borrow across preflight, effect, and
        // commit. The terminal actor mutex then keeps exit/replacement from
        // racing an already validated resize, so a client observes either the
        // old or the new geometry with its matching revision, never a mix.
        let checkpoint_bytes_limit = self.checkpoint_bytes_limit;
        let cells_limit = self.screen_cells_limit;
        let entry = self.entry(reference)?;
        if entry.exited.is_some() {
            return Err(RegistryError::Exited);
        }
        writer
            .resize(reference, geometry)
            .map_err(|_| RegistryError::PtyResizeFailed)?;
        let entry = self.entry_mut(reference)?;
        entry.geometry = geometry;
        entry.revision += 1;
        // Reshape the decoded cells rather than replaying historical control
        // bytes at the new width, then re-account the changed cell retention.
        let (rows, cols) = screen_dimensions(geometry);
        entry.screen.resize(rows, cols);
        enforce_screen_budget(entry, cells_limit);
        snapshot(entry, checkpoint_bytes_limit)
    }

    /// # Errors
    ///
    /// Returns a fencing, attachment, or input-sequencing error without
    /// writing any bytes.
    pub fn write_input(
        &mut self,
        reference: &TerminalRef,
        input: InputRequest,
        bytes: &[u8],
        writer: &mut dyn PtyWriter,
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
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal, or
    /// [`RegistryError::CheckpointUnavailable`] when the screen does not fit the
    /// frame budget.
    pub fn snapshot(&self, reference: &TerminalRef) -> Result<Snapshot, RegistryError> {
        snapshot(self.entry(reference)?, self.checkpoint_bytes_limit)
    }

    /// The committed exit status of a terminal, without capturing a screen.
    ///
    /// The incremental `Resume` path only needs liveness, so it must not pay for
    /// a checkpoint on every poll.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal.
    pub fn exit_status(&self, reference: &TerminalRef) -> Result<Option<i32>, RegistryError> {
        Ok(self.entry(reference)?.exited)
    }

    fn entry(&self, reference: &TerminalRef) -> Result<&Entry, RegistryError> {
        self.entries
            .get(&key(reference))
            .filter(|entry| entry.reference.fences(reference))
            .ok_or(RegistryError::StaleTarget)
    }
    fn entry_mut(&mut self, reference: &TerminalRef) -> Result<&mut Entry, RegistryError> {
        self.entries
            .get_mut(&key(reference))
            .filter(|entry| entry.reference.fences(reference))
            .ok_or(RegistryError::StaleTarget)
    }
}

fn key(reference: &TerminalRef) -> String {
    reference.terminal_id.as_str()
}

/// The screen dimensions a wire geometry maps to, clamped to the checkpoint's
/// bounds so a forged or absurd geometry cannot drive a huge grid allocation.
/// The IPC boundary rejects such a geometry outright; this keeps the authority
/// bounded regardless of the caller.
fn screen_dimensions(geometry: Geometry) -> (usize, usize) {
    (
        usize::from(geometry.rows).clamp(1, ROWS_MAX as usize),
        usize::from(geometry.cols).clamp(1, COLS_MAX as usize),
    )
}

fn counted(cells: usize) -> u64 {
    u64::try_from(cells).unwrap_or(u64::MAX)
}
fn reserve_screen_cells(cells: usize) {
    RETAINED_SCREEN_CELLS.fetch_add(counted(cells), Ordering::Relaxed);
}
fn release_screen_cells(cells: usize) {
    RETAINED_SCREEN_CELLS.fetch_sub(counted(cells), Ordering::Relaxed);
}

/// Re-accounts this screen's retention and trims its oldest history until it
/// fits both the per-terminal budget and whatever the process-local aggregate
/// ceiling leaves after the other terminals' current retention.
fn enforce_screen_budget(entry: &mut Entry, per_terminal_limit: usize) {
    let cells = account_screen(entry);
    let others = RETAINED_SCREEN_CELLS
        .load(Ordering::Relaxed)
        .saturating_sub(counted(cells));
    let aggregate_share =
        usize::try_from(counted(SCREEN_CELLS_AGGREGATE_MAX).saturating_sub(others)).unwrap_or(0);
    let budget = per_terminal_limit.min(aggregate_share);
    if cells <= budget {
        return;
    }
    let dropped = entry.screen.trim_to_cells(budget);
    SCREEN_TRIMMED_ROWS.fetch_add(counted(dropped), Ordering::Relaxed);
    account_screen(entry);
}

/// Publishes this screen's current retention to the process-local aggregate and
/// returns it.
fn account_screen(entry: &mut Entry) -> usize {
    let cells = entry.screen.retained_cells();
    if cells >= entry.screen_cells {
        reserve_screen_cells(cells - entry.screen_cells);
    } else {
        release_screen_cells(entry.screen_cells - cells);
    }
    entry.screen_cells = cells;
    cells
}

/// Captures the terminal view: the retained raw tail plus a screen checkpoint
/// that fits `checkpoint_bytes_limit`.
///
/// The checkpoint is trimmed, oldest history first, until its serialized form
/// fits the budget, so an attach frame stays inside the protocol's frame limit.
/// Only the payload is trimmed; the authoritative screen keeps those rows for
/// the terminal's own bounds. A screen whose visible grids alone exceed the
/// budget fails closed rather than emitting a partial screen.
fn snapshot(entry: &Entry, checkpoint_bytes_limit: usize) -> Result<Snapshot, RegistryError> {
    let base_offset = entry
        .journal
        .front()
        .map_or(entry.next_offset, |segment| segment.start_offset);
    let mut replay = Vec::with_capacity(entry.retained_bytes);
    for segment in &entry.journal {
        replay.extend_from_slice(&segment.data);
    }
    let screen = checkpoint_within(&entry.screen, checkpoint_bytes_limit)?;
    Ok(Snapshot {
        terminal: entry.reference.clone(),
        revision: entry.revision,
        base_offset,
        output_offset: entry.next_offset,
        geometry: entry.geometry,
        replay,
        screen,
        exited: entry.exited,
    })
}

/// Serialized size of a checkpoint payload, the quantity the frame budget bounds.
fn checkpoint_bytes(checkpoint: &ScreenCheckpoint) -> usize {
    // Serializing a well-formed checkpoint cannot fail; an unmeasurable payload
    // is treated as over budget so the trimming loop stays fail-closed.
    serde_json::to_vec(checkpoint).map_or(usize::MAX, |bytes| bytes.len())
}

fn checkpoint_within(
    screen: &VtScreen,
    bytes_limit: usize,
) -> Result<Box<ScreenCheckpoint>, RegistryError> {
    let mut checkpoint = screen.checkpoint();
    while checkpoint_bytes(&checkpoint) > bytes_limit {
        let dropped = halve_history(&mut checkpoint);
        if dropped == 0 {
            return Err(RegistryError::CheckpointUnavailable);
        }
        CHECKPOINT_TRIMMED_ROWS.fetch_add(counted(dropped), Ordering::Relaxed);
    }
    Ok(Box::new(checkpoint))
}

/// Drops the oldest half of each buffer's checkpoint history, returning the rows
/// dropped. Halving converges in a bounded number of measurements; returning
/// zero means only the visible grids remain.
fn halve_history(checkpoint: &mut ScreenCheckpoint) -> usize {
    let mut dropped = drop_oldest_half(&mut checkpoint.primary.scrollback);
    if let Some(alternate) = &mut checkpoint.alternate {
        dropped += drop_oldest_half(&mut alternate.scrollback);
    }
    dropped
}

fn drop_oldest_half<T>(rows: &mut Vec<T>) -> usize {
    let dropped = rows.len().div_ceil(2);
    rows.drain(..dropped);
    dropped
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

    /// The screen a client reconstructs from a revision 2 frame.
    fn restored(frame: &SnapshotFrame) -> VtScreen {
        let SnapshotContent::Screen { screen } = &frame.content else {
            panic!("a revision 2 frame carries a screen checkpoint");
        };
        VtScreen::from_checkpoint(screen).expect("the daemon emits a decodable checkpoint")
    }

    #[test]
    fn revision_2_snapshot_reconstructs_a_screen_a_trimmed_raw_tail_cannot() {
        let r = reference();
        // A four byte journal keeps almost nothing, so the raw tail starts in the
        // middle of an escape sequence — the #199 regression this replaces.
        let mut registry = TerminalRegistry::new(4, 2);
        registry
            .register(r.clone(), Geometry { cols: 12, rows: 3 })
            .unwrap();
        let stream: Vec<&[u8]> = vec![
            b"\x1b[1;31mred\x1b[0m plain\r\n",
            "\u{65e5}\u{672c}".as_bytes(),
            b"\x1b[?1049halt\r\nscreen\x1b[?1049l",
            b"tail\x1b[2;3Hx\x1b[1;38;5;208m",
        ];
        for chunk in &stream {
            registry.append_output(&r, (*chunk).to_vec()).unwrap();
        }

        let attached = registry.attach(&r, ConnectionId::new()).unwrap();
        let checkpoint = attached
            .clone()
            .into_frame(SnapshotWire::ScreenCheckpoint)
            .snapshot;
        // A checkpoint is complete at `output_offset`, so it carries no tail.
        assert_eq!(checkpoint.base_offset, checkpoint.output_offset);
        assert_eq!(checkpoint.geometry, Geometry { cols: 12, rows: 3 });

        // The reconstructed screen equals a reference parser fed every byte,
        // including the cursor move, the styles and the alternate excursion the
        // four byte tail cannot express.
        let mut reference_screen = VtScreen::new(3, 12);
        for chunk in &stream {
            reference_screen.advance(chunk);
        }
        let rebuilt = restored(&checkpoint);
        assert_eq!(rebuilt.cells(), reference_screen.cells());
        assert_eq!(
            rebuilt.cells_with_scrollback(),
            reference_screen.cells_with_scrollback()
        );
        assert_eq!(rebuilt.cursor(), reference_screen.cursor());
        assert_eq!(rebuilt.cursor_style(), reference_screen.cursor_style());

        // A revision 1 client keeps the legacy raw tail contract unchanged.
        let raw = attached.into_frame(SnapshotWire::RawTail).snapshot;
        assert_eq!(
            raw.content,
            SnapshotContent::RawTail {
                replay: b"208m".to_vec()
            }
        );
        assert_eq!(raw.base_offset + 4, raw.output_offset);

        // Wire selection follows the negotiated revision.
        assert_eq!(SnapshotWire::for_revision(0), SnapshotWire::RawTail);
        assert_eq!(SnapshotWire::for_revision(1), SnapshotWire::RawTail);
        assert_eq!(
            SnapshotWire::for_revision(2),
            SnapshotWire::ScreenCheckpoint
        );
        assert_eq!(SnapshotWire::default(), SnapshotWire::RawTail);
    }

    #[test]
    fn checkpoint_and_resume_suffix_reconstruct_the_authoritative_screen() {
        let r = reference();
        let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2);
        registry
            .register(r.clone(), Geometry { cols: 10, rows: 4 })
            .unwrap();
        registry
            .append_output(&r, b"first\r\nsecond\x1b[1m".to_vec())
            .unwrap();

        let frame = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        let mut client = restored(&frame);

        // Output produced after the checkpoint arrives as a contiguous raw
        // suffix; the restored parser continues the interrupted sequence.
        registry
            .append_output(&r, b"bold\r\nthird".to_vec())
            .unwrap();
        for segment in registry.replay_from(&r, frame.output_offset).unwrap() {
            client.advance(&segment.data);
        }
        let authority = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        assert_eq!(client, restored(&authority));
    }

    #[test]
    fn resize_fences_revision_and_geometry_around_checkpoint_capture() {
        let r = reference();
        let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2);
        registry
            .register(r.clone(), Geometry { cols: 12, rows: 3 })
            .unwrap();
        registry
            .append_output(&r, b"alpha\r\nbeta\r\ngamma".to_vec())
            .unwrap();

        // Captured before the resize.
        let before = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);

        // The resize holds the registry's exclusive borrow across preflight, PTY
        // effect and commit, and returns the post-resize view.
        let after = registry
            .resize(&r, Geometry { cols: 6, rows: 2 }, &mut Writer::default())
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.geometry, Geometry { cols: 6, rows: 2 });

        // No frame ever mixes geometries: the envelope and the screen it carries
        // always agree, so a client detects the fence by comparing either one.
        for frame in [&before, &after] {
            let SnapshotContent::Screen { screen } = &frame.content else {
                panic!("checkpoint frame");
            };
            assert_eq!(u32::from(frame.geometry.rows), screen.geometry.rows);
            assert_eq!(u32::from(frame.geometry.cols), screen.geometry.cols);
        }

        // A suffix applied to the pre-resize checkpoint diverges from the
        // authority, which is exactly why the client must retry on the revision
        // or geometry mismatch instead of merging the two states.
        registry.append_output(&r, b"\r\nafter".to_vec()).unwrap();
        let mut stale = restored(&before);
        for segment in registry.replay_from(&r, before.output_offset).unwrap() {
            stale.advance(&segment.data);
        }
        let authority = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        assert_ne!(stale, restored(&authority));
        assert!(authority.revision > before.revision);

        // Re-attaching after the fence converges: the fresh checkpoint plus its
        // own suffix reproduces the authority at the new geometry.
        let fresh = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        assert_eq!(restored(&fresh), restored(&authority));
        assert_eq!(fresh.geometry, Geometry { cols: 6, rows: 2 });
    }

    #[test]
    fn screen_retention_stays_within_the_per_terminal_and_process_budgets() {
        // One row of history per fed line; the budget admits four rows total.
        let rows: u16 = 2;
        let cols: u16 = 8;
        let budget = 4 * usize::from(cols);
        let before = output_pipeline_counters();
        let terminals = [reference(), reference()];
        let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
            .with_screen_cells_limit(budget)
            .with_checkpoint_bytes_limit(CHECKPOINT_BYTES_MAX);
        for terminal in &terminals {
            registry
                .register(terminal.clone(), Geometry { cols, rows })
                .unwrap();
        }
        for line in 0..64 {
            for terminal in &terminals {
                registry
                    .append_output(terminal, format!("line{line}\r\n").into_bytes())
                    .unwrap();
            }
        }

        for terminal in &terminals {
            let frame = registry
                .snapshot(terminal)
                .unwrap()
                .into_frame(SnapshotWire::ScreenCheckpoint);
            let SnapshotContent::Screen { screen } = &frame.content else {
                panic!("checkpoint frame");
            };
            // Per-terminal peak: the visible grid plus the history that fits.
            let retained =
                (screen.primary.grid.len() + screen.primary.scrollback.len()) * usize::from(cols);
            assert!(
                retained <= budget,
                "retained {retained} cells exceeds the {budget} cell budget"
            );
            assert!(!screen.primary.scrollback.is_empty(), "history survives");
        }

        let after = output_pipeline_counters();
        assert!(after.screen_trimmed_rows > before.screen_trimmed_rows);
        // Process-local aggregate peak across every daemon-owned screen.
        assert!(after.retained_screen_cells <= SCREEN_CELLS_AGGREGATE_MAX as u64);
    }

    #[test]
    fn oversized_checkpoints_trim_history_and_then_fail_closed() {
        let r = reference();
        // A budget far below a full checkpoint forces payload trimming.
        let mut registry =
            TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2).with_checkpoint_bytes_limit(2048);
        registry
            .register(r.clone(), Geometry { cols: 16, rows: 2 })
            .unwrap();
        for line in 0..64 {
            registry
                .append_output(&r, format!("history line {line}\r\n").into_bytes())
                .unwrap();
        }
        let before = output_pipeline_counters();
        let frame = registry
            .snapshot(&r)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        let SnapshotContent::Screen { screen } = &frame.content else {
            panic!("checkpoint frame");
        };
        assert!(serde_json::to_vec(screen).unwrap().len() <= 2048);
        let trimmed_once = output_pipeline_counters().checkpoint_trimmed_rows;
        assert!(trimmed_once > before.checkpoint_trimmed_rows);

        // Only the payload was trimmed: the authoritative screen keeps its
        // history, so the next capture has to trim the same rows again.
        registry.snapshot(&r).unwrap();
        assert!(output_pipeline_counters().checkpoint_trimmed_rows > trimmed_once);

        // A budget that cannot hold even the visible grid fails closed, and the
        // failed attach leaves no subscription behind.
        let mut tiny =
            TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2).with_checkpoint_bytes_limit(8);
        tiny.register(r.clone(), Geometry { cols: 8, rows: 2 })
            .unwrap();
        let connection = ConnectionId::new();
        assert_eq!(
            tiny.attach(&r, connection),
            Err(RegistryError::CheckpointUnavailable)
        );
        assert_eq!(
            tiny.detach(&r, 1, connection),
            Err(RegistryError::UnknownSubscription)
        );
        assert_eq!(tiny.snapshot(&r), Err(RegistryError::CheckpointUnavailable));
        assert_eq!(
            tiny.resize(&r, Geometry { cols: 9, rows: 2 }, &mut Writer::default()),
            Err(RegistryError::CheckpointUnavailable)
        );
    }

    #[test]
    fn a_geometry_beyond_the_screen_bounds_is_clamped_by_the_authority() {
        // The IPC boundary rejects such a geometry; the authority still clamps so
        // a forged dimension cannot drive an unbounded grid allocation.
        assert_eq!(
            screen_dimensions(Geometry {
                cols: u16::MAX,
                rows: u16::MAX
            }),
            (ROWS_MAX as usize, COLS_MAX as usize)
        );
        assert_eq!(screen_dimensions(Geometry { cols: 0, rows: 0 }), (1, 1));
        assert_eq!(screen_dimensions(Geometry { cols: 80, rows: 24 }), (24, 80));
    }

    #[test]
    fn exit_status_is_readable_without_capturing_a_screen() {
        let r = reference();
        let mut registry = registry(r.clone());
        assert_eq!(registry.exit_status(&r), Ok(None));
        registry.exited(&r, 3).unwrap();
        assert_eq!(registry.exit_status(&r), Ok(Some(3)));
        let mut stale = r;
        stale.worktree_id = WorktreeId::new();
        assert_eq!(
            registry.exit_status(&stale),
            Err(RegistryError::StaleTarget)
        );
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
            // Both negotiated payloads stay inside one frame.
            for wire in [SnapshotWire::RawTail, SnapshotWire::ScreenCheckpoint] {
                let mut frame = Vec::new();
                write_json_frame(
                    &mut frame,
                    &attached.clone().into_frame(wire),
                    DEFAULT_MAX_FRAME_BYTES,
                )
                .unwrap();
                assert!(frame.len() < DEFAULT_MAX_FRAME_BYTES);
            }

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
        let mut writer = Writer::default();
        let snapshot = registry
            .resize(
                &r,
                Geometry {
                    cols: 100,
                    rows: 30,
                },
                &mut writer,
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
        assert_eq!(
            registry.resize(&r, Geometry { cols: 1, rows: 1 }, &mut Writer::default()),
            Err(RegistryError::Exited)
        );
    }
}
