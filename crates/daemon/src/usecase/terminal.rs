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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{
    Mutex, MutexGuard, PoisonError,
    atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{ClientId, ConnectionId, OperationId, RequestId, TerminalRef};
use usagi_core::infrastructure::ipc::{TERMINAL_CHECKPOINT_REVISION, terminal_input_digest};
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
/// Serializes process-shared screen growth from preflight through accounting.
/// Different terminal actors otherwise could both observe the same remaining
/// allowance and commit grids whose combined retention exceeds the ceiling.
static SCREEN_BUDGET_MUTATION: Mutex<()> = Mutex::new(());

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
/// Agent adapters (Antigravity/Claude/Codex) and the generic shell path differ only in
/// how they resolve a launch; once a `TerminalRef` is reserved, they use this
/// same lifecycle vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalRuntimeState {
    Reserved,
    Running,
    /// The previous owner and child are proved gone. History remains available
    /// for Agent resume, but this state holds no runtime capacity.
    Interrupted,
    /// The Agent was intentionally stopped to free concurrency while retaining
    /// an exact provider resume source.
    Sleeping,
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

/// A terminal's retained output window and liveness, captured without building
/// a screen checkpoint.
///
/// It carries exactly the scalars a caller can learn from the bounded journal:
/// where the retained window starts, how many bytes the terminal has accepted,
/// and whether the child has exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputWindow {
    /// Offset of the oldest byte the bounded journal still retains.
    pub base_offset: u64,
    /// Total bytes accepted for this terminal.
    pub output_offset: u64,
    /// The committed exit status once the child has exited.
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

impl SnapshotContent {
    /// The raw tail, or `None` when this payload is a checkpoint.
    #[must_use]
    pub fn replay(&self) -> Option<&[u8]> {
        match self {
            Self::RawTail { replay } => Some(replay),
            Self::Screen { .. } => None,
        }
    }

    /// The semantic screen, or `None` when this payload is a raw tail.
    #[must_use]
    pub fn screen(&self) -> Option<&ScreenCheckpoint> {
        match self {
            Self::Screen { screen } => Some(screen),
            Self::RawTail { .. } => None,
        }
    }
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
    /// The next input sequence expected for the attaching connection/client.
    ///
    /// `None` is used only by internal callers that do not carry a client
    /// identity. Wire-facing attach paths always populate this value.
    pub next_input_seq: Option<u64>,
}

impl Attached {
    /// Narrows the attached view to the negotiated wire revision.
    #[must_use]
    pub fn into_frame(self, wire: SnapshotWire) -> AttachedFrame {
        AttachedFrame {
            subscription: self.subscription,
            snapshot: self.snapshot.into_frame(wire),
            next_input_seq: self.next_input_seq,
        }
    }
}

/// An [`Attached`] narrowed to one negotiated wire revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachedFrame {
    pub subscription: u64,
    pub snapshot: SnapshotFrame,
    /// Optional for backward-compatible generation-1 decoding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_input_seq: Option<u64>,
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
///
/// The three identities here are deliberately independent. `connection` and
/// `subscription` fence the *attachment* that may write; `input_seq` orders the
/// writes of one connection epoch's subscription; `operation` identifies the
/// logical input itself and therefore survives the connection that carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRequest {
    pub subscription: u64,
    pub connection: ConnectionId,
    /// The producer's client incarnation. It is stable for one client process,
    /// so the durable operation ledger outlives any single connection.
    pub client: ClientId,
    pub request: RequestId,
    pub input_seq: u64,
    /// Producer-issued durable identity of this input. `None` is a peer that
    /// predates the ledger: it keeps the connection-local sequence contract and
    /// gets no cross-connection resolution.
    pub operation: Option<OperationId>,
}

/// Bounds for the durable input operation ledger.
///
/// Every dimension the ledger can grow along is bounded: how many operations one
/// client may keep, how many the process keeps in total, how many payload bytes
/// they hold, and how long a record survives. Exceeding a count or byte bound
/// releases the oldest records; exceeding the age bound releases records on the
/// next lookup or insert. A released record answers as unknown, never as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputOperationBounds {
    pub max_operations: usize,
    pub max_operations_per_client: usize,
    pub max_bytes: usize,
    pub max_age_ms: u64,
}

impl Default for InputOperationBounds {
    fn default() -> Self {
        Self {
            max_operations: 4_096,
            max_operations_per_client: 256,
            max_bytes: 1024 * 1024,
            max_age_ms: 5 * 60 * 1000,
        }
    }
}

/// What the ledger knows about one presented operation identity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OperationLookup {
    /// Never recorded (or already released): the caller may write it as new.
    Absent,
    /// Recorded for the same target and semantic content: replay this final.
    Recorded(InputAck),
    /// Recorded for a different target or different bytes: the identity is
    /// being reused for another meaning.
    Conflict,
}

/// One recorded terminal input operation and the bounds accounting it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InputOperationRecord {
    terminal: String,
    digest: String,
    ack: InputAck,
    bytes: usize,
    recorded_at_ms: u64,
}

/// A bounded, process-local ledger of terminal input operation finals, keyed by
/// client incarnation and producer operation identity.
///
/// It is deliberately *not* per attachment: a client that lost an acknowledgement
/// has also lost the subscription that carried it, so binding the outcome to the
/// attachment would make the outcome unreachable exactly when it is needed.
#[derive(Debug)]
struct InputOperationLedger {
    bounds: InputOperationBounds,
    records: HashMap<(ClientId, OperationId), InputOperationRecord>,
    order: VecDeque<(ClientId, OperationId)>,
    bytes: usize,
}

impl InputOperationLedger {
    fn new(bounds: InputOperationBounds) -> Self {
        Self {
            bounds,
            records: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Releases every record older than the age bound. An aged-out record is
    /// indistinguishable from one never seen, which is why the client contract
    /// treats "unknown" as uncertainty rather than as permission to write again.
    fn expire(&mut self, now_ms: u64) {
        let bounds = self.bounds;
        let expired: Vec<(ClientId, OperationId)> = self
            .records
            .iter()
            .filter(|(_, record)| now_ms.saturating_sub(record.recorded_at_ms) > bounds.max_age_ms)
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            self.release(&key);
        }
    }

    fn release(&mut self, key: &(ClientId, OperationId)) {
        if let Some(record) = self.records.remove(key) {
            self.bytes = self.bytes.saturating_sub(record.bytes);
            self.order.retain(|queued| queued != key);
        }
    }

    fn lookup(
        &mut self,
        client: ClientId,
        operation: OperationId,
        terminal: &str,
        digest: &str,
        now_ms: u64,
    ) -> OperationLookup {
        self.expire(now_ms);
        match self.records.get(&(client, operation)) {
            Some(record) if record.terminal == terminal && record.digest == digest => {
                OperationLookup::Recorded(record.ack.clone())
            }
            Some(_) => OperationLookup::Conflict,
            None => OperationLookup::Absent,
        }
    }

    /// Reads a recorded final without asserting anything about the request that
    /// produced it. `None` means unknown: never seen, or already released.
    fn recorded(
        &mut self,
        client: ClientId,
        operation: OperationId,
        terminal: &str,
        now_ms: u64,
    ) -> Option<InputAck> {
        self.expire(now_ms);
        self.records
            .get(&(client, operation))
            .filter(|record| record.terminal == terminal)
            .map(|record| record.ack.clone())
    }

    fn record(
        &mut self,
        client: ClientId,
        operation: OperationId,
        record: InputOperationRecord,
        now_ms: u64,
    ) {
        self.expire(now_ms);
        if self.bounds.max_operations == 0 || self.bounds.max_operations_per_client == 0 {
            return;
        }
        let key = (client, operation);
        self.release(&key);
        self.bytes = self.bytes.saturating_add(record.bytes);
        self.records.insert(key, record);
        self.order.push_back(key);
        self.enforce_bounds(client);
    }

    /// Releases oldest-first until every bound holds again.
    ///
    /// `records` and `order` are always mutated together, so an over-budget
    /// ledger always has something to release; the lookups are part of the loop
    /// conditions rather than defensive breaks that could never be taken.
    fn enforce_bounds(&mut self, client: ClientId) {
        while self.per_client(client) > self.bounds.max_operations_per_client
            && let Some(oldest) = self
                .order
                .iter()
                .find(|(owner, _)| *owner == client)
                .copied()
        {
            self.release(&oldest);
        }
        while (self.records.len() > self.bounds.max_operations
            || self.bytes > self.bounds.max_bytes)
            && let Some(oldest) = self.order.front().copied()
        {
            self.release(&oldest);
        }
    }

    fn per_client(&self, client: ClientId) -> usize {
        self.records
            .keys()
            .filter(|(owner, _)| *owner == client)
            .count()
    }
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
    /// One durable operation identity was presented for different bytes or a
    /// different terminal than the one it already records. The daemon writes
    /// nothing and never applies it to the other target.
    IdempotencyConflict,
    Exited,
    PtyResizeFailed,
    /// The requested visible grid cannot fit the per-terminal or process-wide
    /// screen budget. The registry rejects it before allocating cells or
    /// resizing the PTY.
    ScreenBudgetExceeded,
    /// The screen cannot be captured inside the frame budget even with all of
    /// its history dropped. The daemon emits no oversized frame and no partial
    /// screen; the client keeps its current state and retries.
    CheckpointUnavailable,
}

/// One connection-owned subscription to a terminal.
///
/// The client incarnation travels with it because the PTY viewport is shared
/// per *client*, not per connection: one TUI process opens several lanes
/// (per-request, terminal stream, poll pump) and every one of them declares the
/// same [`ClientId`], so the client is what identifies "one window" across them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attachment {
    connection: ConnectionId,
    /// The client incarnation that took this attachment. Internal callers that
    /// attach without one contribute no viewport constraint.
    client: Option<ClientId>,
}

/// What one client asks of a shared terminal, and what it has already been told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientViewport {
    /// The pane size this client asked the PTY to take. A client that attached
    /// without ever resizing states no requirement, so it never shrinks the
    /// terminal for the clients that did.
    requested: Option<Geometry>,
    /// The geometry of the last snapshot this client was handed (attach or
    /// resize). A client whose `seen` no longer matches the terminal's geometry
    /// is decoding output at the wrong width, so its next incremental poll is
    /// answered with [`RegistryError::ResyncRequired`] instead of bytes.
    seen: Geometry,
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
    attachments: BTreeMap<u64, Attachment>,
    /// Per-client viewports of the clients sharing this terminal. The PTY takes
    /// the per-dimension minimum of the requests, so no client is ever handed a
    /// screen larger than its own pane (#681).
    viewports: BTreeMap<ClientId, ClientViewport>,
    next_subscription: u64,
    /// Epoch-local input sequence ledgers.
    ///
    /// Keyed by the *connection* as well as the client, because `input_seq` is a
    /// per-connection-epoch ordering number: a client that reconnects restarts it
    /// at zero, so a ledger that outlived the connection would reject the first
    /// input after every reconnect as a stale sequence. Cross-connection identity
    /// lives in the separate operation ledger, keyed by client incarnation.
    inputs: BTreeMap<(ConnectionId, ClientId), InputLedger>,
    /// The authoritative decoded screen for this terminal. Every byte
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
    screen_cells_aggregate_limit: usize,
    /// Production registries share the process counter. Test-only budget
    /// overrides account this registry alone so parallel fixtures cannot spend
    /// one another's deliberately tiny ceilings.
    screen_cells_process_shared: bool,
    /// Cross-connection input operation finals. It lives beside `entries`
    /// instead of inside one, so reusing an operation identity for a second
    /// terminal is detected as a conflict rather than written twice.
    operations: InputOperationLedger,
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
            screen_cells_aggregate_limit: SCREEN_CELLS_AGGREGATE_MAX,
            screen_cells_process_shared: true,
            operations: InputOperationLedger::new(InputOperationBounds::default()),
        }
    }

    /// Overrides the durable input operation bounds.
    ///
    /// The defaults are [`InputOperationBounds::default`]; a smaller budget makes
    /// eviction and expiry observable without recording thousands of operations.
    #[must_use]
    pub fn with_input_operation_bounds(mut self, bounds: InputOperationBounds) -> Self {
        self.operations = InputOperationLedger::new(bounds);
        self
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

    /// Overrides the screen retention budgets: what one terminal may retain and
    /// the ceiling its process shares.
    ///
    /// The defaults are [`SCREEN_CELLS_PER_TERMINAL_MAX`] and
    /// [`SCREEN_CELLS_AGGREGATE_MAX`]. A terminal keeps the smaller of its own
    /// budget and whatever the ceiling leaves after the other terminals'
    /// current retention, so a test can isolate either bound.
    #[must_use]
    #[cfg(test)]
    pub const fn with_screen_cell_budgets(
        mut self,
        per_terminal: usize,
        process_aggregate: usize,
    ) -> Self {
        self.screen_cells_limit = per_terminal;
        self.screen_cells_aggregate_limit = process_aggregate;
        self.screen_cells_process_shared = false;
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
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let (rows, cols) = screen_dimensions(geometry);
        let budgets = self.screen_budgets();
        let visible = counted(rows.saturating_mul(cols));
        let available = counted(budgets.process_aggregate).saturating_sub(budgets.retained_cells);
        if visible > available && visible <= counted(budgets.per_terminal) {
            self.reclaim_screen_cells(visible - available);
        }
        ensure_screen_geometry_fits(geometry, self.screen_budgets(), 0)?;
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
                viewports: BTreeMap::new(),
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
        self.attach_client(reference, connection, None, None, None)
    }

    /// Attaches on behalf of `client`, stating that client's viewport and
    /// recording what geometry it has now been shown.
    ///
    /// The viewport claim is stated here rather than by a separate request
    /// because it lives exactly as long as the attachment: a window that
    /// backgrounds a pane releases its claim, and the window that foregrounds
    /// one must state it again. Doing it in the same exclusive section as the
    /// capture also means the snapshot a client receives is already at the
    /// shared geometry its own claim produced (#681).
    fn attach_client(
        &mut self,
        reference: &TerminalRef,
        connection: ConnectionId,
        client: Option<ClientId>,
        viewport: Option<Geometry>,
        writer: Option<&mut dyn PtyWriter>,
    ) -> Result<Attached, RegistryError> {
        let checkpoint_bytes_limit = self.checkpoint_bytes_limit;
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        let entry = self.entry_mut(reference)?;
        let existing = entry
            .attachments
            .iter()
            .find_map(|(subscription, attachment)| {
                (attachment.connection == connection).then_some(*subscription)
            });
        if let Some(client) = client {
            let seen = entry.geometry;
            entry
                .viewports
                .entry(client)
                .and_modify(|held| held.requested = viewport.or(held.requested))
                .or_insert(ClientViewport {
                    requested: viewport,
                    seen,
                });
            // A caller without a writer (the internal attach) states no
            // viewport either, so there is nothing for it to reshape.
            if let Some(writer) = writer {
                reconcile_geometry(entry, Some(&client), writer, budgets);
            }
        }
        let snapshot = snapshot(entry, checkpoint_bytes_limit)?;
        // The client leaves with a screen at this geometry, so it is no longer
        // out of date with the shared PTY even if a peer moved it earlier.
        if let Some(client) = client {
            let geometry = entry.geometry;
            entry
                .viewports
                .entry(client)
                .and_modify(|held| held.seen = geometry);
        }
        // Capture before the subscription is recorded: a snapshot the client
        // cannot be handed must not leave an attachment behind.
        let subscription = existing.unwrap_or_else(|| {
            let subscription = entry.next_subscription;
            entry.next_subscription += 1;
            entry
                .attachments
                .insert(subscription, Attachment { connection, client });
            subscription
        });
        Ok(Attached {
            subscription,
            snapshot,
            next_input_seq: None,
        })
    }

    /// Atomically attaches and reports the daemon ledger position for this
    /// connection/client pair. A missing ledger starts at zero.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a different generation or
    /// ownership scope.
    pub fn attach_for_client(
        &mut self,
        reference: &TerminalRef,
        connection: ConnectionId,
        client: ClientId,
        viewport: Option<Geometry>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Attached, RegistryError> {
        let mut attached =
            self.attach_client(reference, connection, Some(client), viewport, Some(writer))?;
        let entry = self.entry(reference)?;
        attached.next_input_seq = Some(
            entry
                .inputs
                .get(&(connection, client))
                .map_or(0, |ledger| ledger.next_seq),
        );
        Ok(attached)
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
        writer: &mut dyn PtyWriter,
    ) -> Result<(), RegistryError> {
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        let entry = self.entry_mut(reference)?;
        match entry.attachments.get(&subscription) {
            Some(attachment) if attachment.connection == connection => {
                entry.attachments.remove(&subscription);
                // The window that was holding the shared viewport down is gone.
                reconcile_geometry(entry, None, writer, budgets);
                Ok(())
            }
            _ => Err(RegistryError::UnknownSubscription),
        }
    }

    /// Releases only this connection's subscriptions.  It intentionally leaves
    /// the PTY, output journal and process ownership alive.
    pub fn disconnect(&mut self, connection: ConnectionId, writer: &mut dyn PtyWriter) {
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        for entry in self.entries.values_mut() {
            let before = entry.attachments.len();
            entry
                .attachments
                .retain(|_, attachment| attachment.connection != connection);
            // A window that went away stops constraining the terminals it shared.
            if entry.attachments.len() != before {
                reconcile_geometry(entry, None, writer, budgets);
            }
            // The epoch-local sequence ledger dies with its connection, exactly
            // as the client's `input_seq` restarts on a fresh transport. Durable
            // operation finals are unaffected: they are what a reconnecting
            // client still has to be able to resolve.
            entry.inputs.retain(|(owner, _), _| *owner != connection);
        }
    }

    /// Releases every connection-local attachment and input epoch which is no
    /// longer present in the daemon's bounded live-connection census.
    ///
    /// This is the bulk form of [`Self::disconnect`]. It lets the composition
    /// root coalesce an arbitrary number of historical disconnect notifications
    /// into one sweep without retaining one queue item per old connection.
    pub fn retain_live_connections(
        &mut self,
        live: &BTreeSet<ConnectionId>,
        writer: &mut dyn PtyWriter,
    ) {
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        for entry in self.entries.values_mut() {
            let before = entry.attachments.len();
            entry
                .attachments
                .retain(|_, attachment| live.contains(&attachment.connection));
            if entry.attachments.len() != before {
                reconcile_geometry(entry, None, writer, budgets);
            }
            entry.inputs.retain(|(owner, _), _| live.contains(owner));
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
        self.append_output_with_replies(reference, data)
            .map(|(output, _)| output)
    }

    /// Applies PTY output and also returns terminal-protocol replies for the
    /// daemon-owned PTY endpoint. Replies are not user input and therefore do
    /// not enter the client input ledger.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when the terminal is not owned by
    /// this registry.
    ///
    /// # Panics
    ///
    /// Panics only if the internal retained-byte accounting invariant is
    /// broken.
    pub fn append_output_with_replies(
        &mut self,
        reference: &TerminalRef,
        data: Vec<u8>,
    ) -> Result<(Output, Vec<u8>), RegistryError> {
        let limit = self.journal_limit;
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        let entry = self.entry_mut(reference)?;
        // The screen is the authority: it sees every accepted byte, including
        // the bytes the bounded journal is about to drop.
        let replies = entry.screen.advance_with_replies(&data);
        enforce_screen_budget(entry, budgets);
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
        Ok((output, replies))
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
        client: Option<&ClientId>,
    ) -> Result<Vec<Output>, RegistryError> {
        let entry = self.entry(reference)?;
        // A peer moved the shared viewport since this client was last handed a
        // screen, so the bytes after its cursor were produced for a different
        // grid. Feeding them into the client's stale screen is exactly what
        // corrupts a second window's display, so the incremental lane fails
        // closed to the atomic reattach the client already implements (#681).
        if client
            .and_then(|client| entry.viewports.get(client))
            .is_some_and(|viewport| viewport.seen != entry.geometry)
        {
            return Err(RegistryError::ResyncRequired);
        }
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
        client: Option<&ClientId>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, RegistryError> {
        // Hold the registry's exclusive borrow across preflight, effect, and
        // commit. The terminal actor mutex then keeps exit/replacement from
        // racing an already validated resize, so a client observes either the
        // old or the new geometry with its matching revision, never a mix.
        let checkpoint_bytes_limit = self.checkpoint_bytes_limit;
        let _screen_budget_guard = lock_screen_budget(self.screen_cells_process_shared);
        let budgets = self.screen_budgets();
        let entry = self.entry_mut(reference)?;
        if entry.exited.is_some() {
            return Err(RegistryError::Exited);
        }
        let previous = entry.geometry;
        // The request states what this window wants; the terminal takes the
        // smallest request among the windows sharing it, so this is a request
        // and not a command (#681).
        if let Some(client) = client {
            entry
                .viewports
                .entry(*client)
                .and_modify(|viewport| viewport.requested = Some(geometry))
                .or_insert(ClientViewport {
                    requested: Some(geometry),
                    seen: entry.geometry,
                });
            prune_viewports(entry, Some(client));
        }
        // With windows attached, their shared minimum decides; with none — an
        // internal caller on a terminal nobody is watching — the request stands.
        let target = requested_geometry(entry).unwrap_or(geometry);
        if target != entry.geometry {
            ensure_screen_geometry_fits(target, budgets, entry.screen_cells)?;
            writer
                .resize(reference, target)
                .map_err(|_| RegistryError::PtyResizeFailed)?;
            let entry = self.entry_mut(reference)?;
            commit_geometry(entry, target, budgets);
        }
        let entry = self.entry_mut(reference)?;
        let committed = entry.geometry;
        if let Some(client) = client
            && let Some(viewport) = entry.viewports.get_mut(client)
        {
            // A client that was already current stays current: it reshapes its
            // own screen exactly as this commit reshaped the authority. A client
            // whose screen predates a *peer's* commit is not made current by
            // asking a question — it never saw that reshape, so its resync
            // marker stays and its next poll still sends it for a fresh screen.
            if viewport.seen == previous {
                viewport.seen = committed;
            }
        }
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
        now_ms: u64,
        writer: &mut dyn PtyWriter,
    ) -> Result<InputAck, RegistryError> {
        let input_cache_limit = self.input_cache_limit;
        let target = key(reference);
        // Resolve the durable identity first. A client whose acknowledgement was
        // lost reconnects with a *new* connection and a *new* subscription, so
        // requiring the attachment before consulting the ledger is exactly what
        // made an existing outcome unreachable.
        let digest = terminal_input_digest(&target, bytes);
        if let Some(operation) = input.operation {
            match self
                .operations
                .lookup(input.client, operation, &target, &digest, now_ms)
            {
                OperationLookup::Recorded(ack) => {
                    // The terminal identity must still fence, but neither the
                    // attachment nor the exit state may turn a recorded final
                    // into a different answer.
                    self.entry(reference)?;
                    return Ok(InputAck::Cached(Box::new(ack)));
                }
                OperationLookup::Conflict => return Err(RegistryError::IdempotencyConflict),
                OperationLookup::Absent => {}
            }
        }
        let entry = self.entry_mut(reference)?;
        if entry
            .attachments
            .get(&input.subscription)
            .map(|attachment| attachment.connection)
            != Some(input.connection)
        {
            return Err(RegistryError::NotAttached);
        }
        if entry.exited.is_some() {
            return Err(RegistryError::Exited);
        }
        let ledger = entry
            .inputs
            .entry((input.connection, input.client))
            .or_default();
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
        if let Some(operation) = input.operation {
            self.operations.record(
                input.client,
                operation,
                InputOperationRecord {
                    terminal: target,
                    digest,
                    ack: ack.clone(),
                    bytes: bytes.len(),
                    recorded_at_ms: now_ms,
                },
                now_ms,
            );
        }
        Ok(ack)
    }

    /// Applies an explicit user clear to the authoritative primary screen.
    /// The PTY still receives the original control byte; this mutation only
    /// makes future attach checkpoints agree with the client that initiated it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] when `reference` is not owned by
    /// this registry.
    pub fn clear_primary_for_user(
        &mut self,
        reference: &TerminalRef,
    ) -> Result<bool, RegistryError> {
        let entry = self.entry_mut(reference)?;
        let cleared = entry.screen.clear_primary_for_user();
        if cleared {
            RETENTION_DROPPED_BYTES.fetch_add(counted(entry.retained_bytes), Ordering::Relaxed);
            entry.journal.clear();
            entry.retained_bytes = 0;
            account_screen(entry);
        }
        Ok(cleared)
    }

    /// Reads the recorded final of one durable input operation without writing.
    ///
    /// `Ok(None)` is a typed unknown: the operation was never recorded here, or
    /// the bounded ledger already released it. It is deliberately not an error
    /// and deliberately not permission to write the bytes again.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal. An
    /// exited terminal still answers, because the input it recorded happened
    /// before the exit.
    pub fn input_outcome(
        &mut self,
        reference: &TerminalRef,
        client: ClientId,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<Option<InputAck>, RegistryError> {
        self.entry(reference)?;
        Ok(self
            .operations
            .recorded(client, operation, &key(reference), now_ms))
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

    /// The retained output window and liveness of a terminal, without capturing
    /// a screen.
    ///
    /// Every caller that needs only offsets must use this instead of
    /// [`snapshot`](Self::snapshot): a snapshot builds a complete semantic
    /// checkpoint and measures its serialized size, which is proportional to the
    /// retained screen. Paying that to read an integer offset would cost a full
    /// screen capture on **every** accepted PTY chunk and on every tombstone of
    /// every inventory query.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StaleTarget`] for a non-current terminal.
    pub fn output_window(&self, reference: &TerminalRef) -> Result<OutputWindow, RegistryError> {
        let entry = self.entry(reference)?;
        Ok(OutputWindow {
            base_offset: base_offset(entry),
            output_offset: entry.next_offset,
            exited: entry.exited,
        })
    }

    /// Bytes this terminal's bounded output journal currently retains.
    ///
    /// Aggregate retention accounting (#526) charges a final for what its
    /// tombstone actually holds, so an unknown or stale terminal is charged
    /// nothing rather than an assumed worst case.
    #[must_use]
    pub fn retained_bytes(&self, reference: &TerminalRef) -> u64 {
        self.entry(reference).map_or(0, |entry| {
            u64::try_from(entry.retained_bytes).unwrap_or(u64::MAX)
        })
    }

    /// Whether any connection still holds a subscription to this terminal. A
    /// client draining the final replay of an exited terminal keeps it here, so
    /// retention can protect it from collection while it is being read.
    #[must_use]
    pub fn is_attached(&self, reference: &TerminalRef) -> bool {
        self.entry(reference)
            .is_ok_and(|entry| !entry.attachments.is_empty())
    }

    /// Drops a collected terminal's journal, screen, and attachments, releasing
    /// its retained bytes and screen cells. Forgetting an unknown or stale
    /// terminal is a no-op, so a retried collection cannot remove another
    /// incarnation.
    pub fn forget(&mut self, reference: &TerminalRef) -> bool {
        if self.entry(reference).is_err() {
            return false;
        }
        self.entries.remove(&key(reference)).is_some()
    }

    /// Cells this registry is holding, independent of the process aggregate.
    ///
    /// A test that asserts "this operation allocated nothing" has to read the
    /// registry rather than the process counter: libtest runs every test in one
    /// process, so a neighbouring fixture registering a screen moves the process
    /// number under the assertion and fails a test that did nothing wrong.
    #[must_use]
    pub fn retained_screen_cells(&self) -> u64 {
        self.entries
            .values()
            .map(|entry| counted(entry.screen_cells))
            .sum()
    }

    /// Frees up to `needed` cells of scrollback from this registry's screens so
    /// a new visible grid can be admitted.
    ///
    /// A growing screen takes whatever the process ceiling leaves, and nothing
    /// hands it back until that screen is forgotten. Exited screens are kept
    /// until retention evicts them, so a session that keeps launching Agents —
    /// a workflow's planner and reviewer turns — filled the ceiling with
    /// history nobody is reading, and every later launch was refused. Exited
    /// screens give up their history first, then the largest live ones; no
    /// visible grid is touched, so a screen stays drawable.
    fn reclaim_screen_cells(&mut self, needed: u64) {
        let mut order = self
            .entries
            .iter()
            .map(|(key, entry)| {
                (
                    entry.exited.is_none(),
                    std::cmp::Reverse(entry.screen_cells),
                    key.clone(),
                )
            })
            .collect::<Vec<_>>();
        order.sort();
        let mut remaining = needed;
        for (_, _, key) in order {
            if remaining == 0 {
                break;
            }
            // The keys were read from this map under the same `&mut self`.
            let entry = self
                .entries
                .get_mut(&key)
                .expect("reclaim order lists this registry's own entries");
            let before = entry.screen_cells;
            let target = usize::try_from(counted(before).saturating_sub(remaining)).unwrap_or(0);
            let dropped = entry.screen.trim_to_cells(target);
            SCREEN_TRIMMED_ROWS.fetch_add(counted(dropped), Ordering::Relaxed);
            let after = account_screen(entry);
            remaining = remaining.saturating_sub(counted(before.saturating_sub(after)));
        }
    }

    fn screen_budgets(&self) -> ScreenBudgets {
        ScreenBudgets {
            per_terminal: self.screen_cells_limit,
            process_aggregate: self.screen_cells_aggregate_limit,
            retained_cells: if self.screen_cells_process_shared {
                RETAINED_SCREEN_CELLS.load(Ordering::Relaxed)
            } else {
                self.retained_screen_cells()
            },
        }
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

/// Applies the shared viewport policy: the PTY takes the per-dimension minimum
/// of what the clients still attached to this terminal asked for.
///
/// This is what keeps two windows of different sizes on one terminal readable.
/// The smallest attached pane wins, so every client can render the authoritative
/// screen without clipping it, and the surplus rows/columns of a larger pane
/// stay blank (#681). Clients whose attachment is gone are dropped first, so
/// closing the small window gives the terminal back to the large one.
///
/// A geometry that did not move commits nothing: no PTY call, no revision, and
/// therefore no resync for the clients already at that size. A PTY that refuses
/// the new size leaves the committed geometry untouched, so the next reconcile
/// tries again rather than letting clients decode at a size the PTY never took.
fn reconcile_geometry(
    entry: &mut Entry,
    requester: Option<&ClientId>,
    writer: &mut dyn PtyWriter,
    budgets: ScreenBudgets,
) {
    if entry.exited.is_some() {
        return;
    }
    // `requester` is the client being served right now. Its attachment is
    // recorded only after the snapshot it is about to receive is captured, so
    // without this it would prune the very claim this call is applying.
    prune_viewports(entry, requester);
    let Some(geometry) = requested_geometry(entry) else {
        return;
    };
    if geometry == entry.geometry {
        return;
    }
    if ensure_screen_geometry_fits(geometry, budgets, entry.screen_cells).is_err() {
        return;
    }
    let reference = entry.reference.clone();
    if writer.resize(&reference, geometry).is_err() {
        return;
    }
    commit_geometry(entry, geometry, budgets);
}

/// Drops the viewports of clients that no longer hold an attachment, keeping
/// `requester` — the client being served right now, whose attachment may not be
/// recorded yet, or which is stating a viewport on a different lane than the one
/// its attachment lives on.
///
/// A claim lives exactly as long as the attachment that stated it, so this is
/// what makes a window that closed — or merely backgrounded the pane — stop
/// holding the terminal down. It also bounds the map by the number of windows
/// actually sharing the terminal, plus the one being served.
fn prune_viewports(entry: &mut Entry, requester: Option<&ClientId>) {
    let attached: std::collections::BTreeSet<ClientId> = entry
        .attachments
        .values()
        .filter_map(|attachment| attachment.client)
        .collect();
    entry
        .viewports
        .retain(|client, _| attached.contains(client) || requester == Some(client));
}

/// The per-dimension minimum of what the clients sharing this terminal asked
/// for, or `None` when nobody stated a viewport.
fn requested_geometry(entry: &Entry) -> Option<Geometry> {
    entry
        .viewports
        .values()
        .filter_map(|viewport| viewport.requested)
        .reduce(|left, right| Geometry {
            cols: left.cols.min(right.cols),
            rows: left.rows.min(right.rows),
        })
}

/// Commits a geometry the PTY has already accepted: it advances the revision
/// that fences client snapshots and reshapes the decoded cells rather than
/// replaying historical control bytes at the new width.
fn commit_geometry(entry: &mut Entry, geometry: Geometry, budgets: ScreenBudgets) {
    entry.geometry = geometry;
    entry.revision += 1;
    let (rows, cols) = screen_dimensions(geometry);
    entry.screen.resize(rows, cols);
    enforce_screen_budget(entry, budgets);
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

fn lock_screen_budget(process_shared: bool) -> Option<MutexGuard<'static, ()>> {
    process_shared.then(|| {
        SCREEN_BUDGET_MUTATION
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    })
}

/// The screen retention budgets one registry enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenBudgets {
    per_terminal: usize,
    process_aggregate: usize,
    /// Accounted cells before the current mutation. This is process-global in
    /// production and registry-local for an injected test budget.
    retained_cells: u64,
}

/// Refuses a visible grid that cannot fit before `VtScreen` allocates or a PTY
/// observes the new dimensions. `current_cells` is excluded from the process
/// total for resize because that screen is replaced rather than added.
fn ensure_screen_geometry_fits(
    geometry: Geometry,
    budgets: ScreenBudgets,
    current_cells: usize,
) -> Result<(), RegistryError> {
    let (rows, cols) = screen_dimensions(geometry);
    let visible = rows.saturating_mul(cols);
    let retained_elsewhere = budgets
        .retained_cells
        .saturating_sub(counted(current_cells));
    let aggregate_available = counted(budgets.process_aggregate).saturating_sub(retained_elsewhere);
    let fits = visible <= budgets.per_terminal && counted(visible) <= aggregate_available;
    fits.then_some(())
        .ok_or(RegistryError::ScreenBudgetExceeded)
}

/// Re-accounts this screen's retention and trims its oldest history until it
/// fits both the per-terminal budget and whatever the process aggregate ceiling
/// leaves after the other terminals' current retention.
fn enforce_screen_budget(entry: &mut Entry, budgets: ScreenBudgets) {
    let previous_cells = entry.screen_cells;
    let cells = account_screen(entry);
    let others = budgets
        .retained_cells
        .saturating_sub(counted(previous_cells));
    let aggregate_share =
        usize::try_from(counted(budgets.process_aggregate).saturating_sub(others)).unwrap_or(0);
    let budget = budgets.per_terminal.min(aggregate_share);
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

/// The offset of the oldest byte the bounded journal still retains. An empty
/// journal has nothing older than what the terminal has already accepted.
fn base_offset(entry: &Entry) -> u64 {
    entry
        .journal
        .front()
        .map_or(entry.next_offset, |segment| segment.start_offset)
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
    let base_offset = base_offset(entry);
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
    checkpoint.primary.scrollback_origin = checkpoint
        .primary
        .scrollback_origin
        .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
    if let Some(alternate) = &mut checkpoint.alternate {
        let alternate_dropped = drop_oldest_half(&mut alternate.scrollback);
        alternate.scrollback_origin = alternate
            .scrollback_origin
            .saturating_add(u64::try_from(alternate_dropped).unwrap_or(u64::MAX));
        dropped += alternate_dropped;
    }
    dropped
}

fn drop_oldest_half<T>(rows: &mut Vec<T>) -> usize {
    let dropped = rows.len().div_ceil(2);
    rows.drain(..dropped);
    dropped
}

#[cfg(test)]
mod tests;
