//! Client-side coordinator for one daemon-owned terminal, driven by polling.
//!
//! The daemon owns the PTY and journals its output; the synchronous IPC client
//! the TUI uses cannot receive pushed stream events, so this coordinator keeps a
//! live view by **polling**: it attaches once (restoring the daemon's semantic
//! screen checkpoint and taking an output cursor), then asks for the bytes after
//! that cursor on every redraw tick.  It feeds those bytes into the restored
//! [`TerminalScreen`], forwards keystrokes once each with a monotonic input
//! sequence, and never spawns a local process — a transport failure only
//! produces safe feedback.
//!
//! Panes of one TUI share a single persistent transport, so a subscription is
//! identified by the port's connection epoch as well as its wire id
//! ([`TerminalSubscription`]). When the port replaces that transport every
//! subscription taken on the previous epoch becomes invalid at once — the daemon
//! released those attachments with the connection — so each session attaches
//! freshly before it sends the next `Resume` or `Input` instead of spending a
//! keystroke on an attachment that no longer exists.
//!
//! Retained history is **only** rebuilt from a checkpoint. A daemon that cannot
//! serve one offers a raw byte tail cut at an arbitrary boundary; parsing that
//! would expose partial UTF-8 / CSI / OSC sequences and lose the cursor, SGR,
//! scroll region and alternate buffer established before the window, so this
//! coordinator fails closed to a limited, history-less view instead
//! ([`TerminalAttachScreen::HistoryUnavailable`]).
//!
//! The daemon boundary is the injected [`TerminalStreamPort`], so the whole
//! coordinator is exercised with a fake port in unit tests.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use usagi_core::domain::id::{OperationId, TerminalRef};
use usagi_core::usecase::vt_screen::{CheckpointError, ScreenCheckpoint};

use super::pane_runtime::Geometry;
use super::terminal_screen::TerminalScreen;
use super::terminal_selection::{TerminalPoint, TerminalSelection};

/// How an attach snapshot carries the terminal screen.
///
/// The variant is decided at the wire boundary by capability / revision
/// negotiation, so a legacy raw tail never reaches this use case at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAttachScreen {
    /// The daemon's semantic screen checkpoint, complete at `output_offset`.
    Checkpoint(Box<ScreenCheckpoint>),
    /// The daemon cannot serve a checkpoint (the capability is absent, or the
    /// common wire revision fell back to the legacy raw tail). Retained history
    /// is unavailable and only output after `output_offset` is rendered.
    HistoryUnavailable,
}

/// A connection-owned terminal subscription: the daemon's wire id together with
/// the client-local epoch of the transport incarnation that issued it.
///
/// The daemon releases every attachment of a connection when that connection
/// goes away, so a subscription id alone does not identify a usable attachment:
/// panes of one shipping TUI share a single persistent transport, and replacing
/// it invalidates every subscription taken on the previous epoch at once. Both
/// halves therefore travel together, and a subscription from an earlier epoch is
/// never equal to one taken after the replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSubscription {
    /// The daemon-issued wire id, valid only on `epoch`'s connection.
    pub id: u64,
    /// Client-local incarnation of the shared transport that issued `id`.
    /// Reattach on the same epoch preserves the daemon's per-client input
    /// sequence; a new epoch starts a fresh ledger at zero.
    pub epoch: u64,
}

/// The atomic view returned by attaching to a daemon terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAttach {
    /// The connection-owned subscription used to fence later input.
    pub subscription: TerminalSubscription,
    /// The daemon terminal revision this view was taken at. It advances on
    /// geometry commit and exit, so it fences a stale snapshot.
    pub revision: u64,
    /// The output offset the screen is complete at; polling resumes here.
    pub output_offset: u64,
    /// The daemon ledger cursor for this connection/client. Older generation-1
    /// peers omit it, in which case the client falls back to epoch continuity.
    pub next_input_seq: Option<u64>,
    /// The screen state to rebuild this session's view from.
    pub screen: TerminalAttachScreen,
    /// Whether the terminal has already exited.
    pub exited: bool,
}

/// Whether the current view carries the terminal's retained history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalHistory {
    /// The screen was reconstructed from the daemon's semantic checkpoint, so
    /// pre-window cursor, style, scroll region and buffers are present.
    Restored,
    /// The daemon cannot serve a checkpoint. History is not shown at all rather
    /// than reconstructed from a raw tail, and only live output after the attach
    /// offset appears.
    Unavailable,
}

/// Why a restored view was refused. Every reason keeps the session's current
/// screen untouched: an old and a new screen state are never mixed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotRefusal {
    /// The checkpoint was captured at a geometry other than the one this pane
    /// synchronized with the daemon (a resize interleaved the snapshot).
    Geometry {
        expected: Geometry,
        snapshot: (u32, u32),
    },
    /// The snapshot is older than one already applied, so its screen and the
    /// suffix after it cannot be trusted to belong together.
    StaleRevision { seen: u64, snapshot: u64 },
    /// The checkpoint violates a bound and was rejected before reconstruction.
    Rejected(CheckpointError),
}

impl SnapshotRefusal {
    /// Presentation-safe explanation. It never carries daemon internals beyond
    /// the geometry, revision and typed bound involved.
    fn message(&self) -> String {
        match self {
            // Sizes are reported as `cols`x`rows`, matching the pane geometry.
            Self::Geometry { expected, snapshot } => format!(
                "terminal screen changed size during attach (snapshot {}x{}, pane {}x{}); resynchronizing",
                snapshot.1, snapshot.0, expected.cols, expected.rows
            ),
            Self::StaleRevision { seen, snapshot } => format!(
                "terminal screen snapshot is stale (revision {snapshot} after {seen}); resynchronizing"
            ),
            Self::Rejected(error) => {
                format!("terminal screen snapshot was rejected: {error}; resynchronizing")
            }
        }
    }
}

/// A contiguous output segment returned by polling after a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalChunk {
    pub start_offset: u64,
    pub end_offset: u64,
    pub data: Vec<u8>,
}

/// The daemon's final outcome for one consumed terminal input sequence.
///
/// Every variant advances the daemon ledger. Only [`Self::Written`] is a
/// normal success; known failures stay attached so the next input can use the
/// following sequence without an unnecessary reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalInputOutcome {
    /// Every byte was accepted by the PTY master.
    Written,
    /// No byte was accepted by the PTY master.
    Failed,
    /// A prefix was accepted before the writer failed. The command-level
    /// effect is uncertain and must never be retried automatically.
    Ambiguous { applied_prefix: usize },
}

/// How the daemon answered a durable input operation resolution query.
///
/// This is the *only* way an input whose acknowledgement was lost becomes
/// certain again: the client asks what happened to that operation instead of
/// writing the bytes a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalInputResolution {
    /// The daemon still holds this operation's recorded final. It is the same
    /// value the lost acknowledgement carried, including a non-success one.
    Final(TerminalInputOutcome),
    /// The daemon has no record: it never saw the operation, its bounded ledger
    /// released it, or the peer has no durable ledger at all. That is typed
    /// uncertainty, never permission to send the bytes again.
    Unknown,
}

/// A safe, client-visible terminal transport failure.  None of these authorize
/// a local PTY fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalError {
    /// The output cursor fell outside the daemon's retained journal. The
    /// terminal remains owned and must be rebuilt from an atomic snapshot.
    ResyncRequired,
    /// The daemon connection is unavailable; a reconnect may recover it.
    Unavailable,
    /// The referenced terminal is gone or its generation no longer matches.
    Stale,
    /// Ownership is unknown; input is disabled until reconciled.
    Orphaned,
    /// The terminal process has exited; its final output is retained.
    Exited,
    /// The input request may have reached the PTY, but its acknowledgement was
    /// not received or could not be decoded. Blind replay is unsafe.
    InputEffectUnknown,
    /// The daemon rejected the client's epoch-local input ordering. This is a
    /// client/ledger synchronization failure, not daemon unavailability.
    OrderingMismatch,
}

/// The daemon boundary consumed by [`TerminalSession`].  Every call is fenced by
/// the complete [`TerminalRef`]; implementations poll the daemon and must not
/// substitute a local terminal on failure.
pub trait TerminalStreamPort {
    /// The epoch of the shared transport incarnation this port currently holds,
    /// or `None` when it has no shared transport to invalidate against.
    ///
    /// A production port multiplexes every pane's attach / input / detach over
    /// one persistent connection. Replacing that connection advances the epoch,
    /// which invalidates every subscription taken on an earlier one: the daemon
    /// released those attachments together with the connection, so a session
    /// holding one must attach freshly before it sends any `Resume` or `Input`.
    /// A port without a shared transport keeps the default, so nothing is ever
    /// invalidated.
    fn connection_epoch(&self) -> Option<u64> {
        None
    }

    /// Resize the daemon-owned PTY to match the pane viewport.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn resize(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<(), TerminalError> {
        Ok(())
    }

    /// Attach and take an atomic snapshot plus a subscription.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError>;
    /// Fetch the contiguous output produced after `after_offset`.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Exited`] once the process has ended, or a safe
    /// daemon communication / ownership failure.
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError>;
    /// Write input bytes exactly once, fenced by `subscription` and `input_seq`
    /// and identified across connections by `operation`.
    ///
    /// `operation` is producer-issued and stable for one logical input, so the
    /// daemon can answer for it later through [`Self::input_outcome`] even though
    /// the connection, subscription, and epoch-local `input_seq` that carried it
    /// are gone.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError>;

    /// Read the recorded final of one durable input operation, without writing.
    ///
    /// The default answers [`TerminalInputResolution::Unknown`], which is what a
    /// port with no durable ledger must do: the caller keeps the uncertainty
    /// latched instead of replaying bytes that may already have been applied.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure. A
    /// failure leaves the operation unresolved, so the fence stays in place.
    fn input_outcome(
        &mut self,
        _terminal: &TerminalRef,
        _operation: OperationId,
        _input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        Ok(TerminalInputResolution::Unknown)
    }
    /// Release only this subscription; it must not stop the daemon terminal.
    ///
    /// A subscription from a replaced epoch is released locally: the daemon
    /// dropped that attachment with its connection, so the request must not be
    /// re-sent on the current transport, where it would neither find the
    /// subscription nor be allowed to disturb the attachments its peers hold.
    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription);
}

/// The coordinator's connection status, rendered without leaking transport
/// details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Attached and streaming.
    Live,
    /// The daemon transport is temporarily unavailable; attach will be retried.
    Reconnecting,
    /// Not attached; a reconnect is required to resume.
    Disconnected,
    /// Ownership is unknown; input is disabled.
    Orphaned,
    /// The terminal process has exited; the final screen is retained.
    Exited,
}

/// Why a keystroke was not accepted by the daemon-owned terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalInputError {
    /// There is no live, connection-owned subscription to fence the input.
    NotLive(SessionState),
    /// The daemon consumed the sequence and returned a known non-success
    /// outcome. The live subscription remains usable for the next sequence.
    Rejected(TerminalInputOutcome),
    /// A live input request reached the port but failed.
    Transport(TerminalError),
    /// Accepted into this terminal's ordered producer queue behind an input
    /// whose effect is still unknown. It has *not* reached the PTY: sending it
    /// now could overtake the unresolved input or be concatenated onto a command
    /// that input may have half-written. It is sent, in order, once the fence
    /// resolves.
    Fenced { queued: usize },
    /// The bounded queue behind the fence is full, so this keystroke is refused
    /// as typed backpressure rather than dropped silently or reordered.
    FenceFull { queued: usize },
}

impl TerminalInputError {
    /// Presentation-safe explanation that distinguishes definite rejection
    /// from a partial write or lost acknowledgement.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::NotLive(SessionState::Reconnecting) => {
                "terminal is reconnecting; keystroke not delivered".to_owned()
            }
            Self::NotLive(SessionState::Disconnected) => {
                "terminal is disconnected; keystroke not delivered".to_owned()
            }
            Self::NotLive(SessionState::Orphaned) | Self::Transport(TerminalError::Orphaned) => {
                "terminal ownership is unknown; keystroke not delivered".to_owned()
            }
            Self::NotLive(SessionState::Exited) | Self::Transport(TerminalError::Exited) => {
                "terminal has exited; keystroke not delivered".to_owned()
            }
            Self::NotLive(SessionState::Live) => {
                "terminal subscription is unavailable; keystroke not delivered".to_owned()
            }
            Self::Rejected(TerminalInputOutcome::Failed) => {
                "terminal input was not applied; retry manually".to_owned()
            }
            Self::Rejected(TerminalInputOutcome::Ambiguous { applied_prefix }) => {
                format!(
                    "terminal input is uncertain; {applied_prefix} bytes were applied before failure"
                )
            }
            Self::Rejected(TerminalInputOutcome::Written) => {
                "terminal returned an invalid input outcome".to_owned()
            }
            Self::Transport(TerminalError::ResyncRequired) => {
                "terminal output is resynchronizing; keystroke not delivered".to_owned()
            }
            Self::Transport(TerminalError::Unavailable) => {
                "daemon unavailable; keystroke not delivered".to_owned()
            }
            Self::Transport(TerminalError::Stale) => {
                "terminal is no longer available; keystroke not delivered".to_owned()
            }
            Self::Transport(TerminalError::InputEffectUnknown) => {
                "terminal input acknowledgement was lost; delivery is unknown".to_owned()
            }
            Self::Transport(TerminalError::OrderingMismatch) => {
                "terminal input ordering is out of sync; keystroke not delivered".to_owned()
            }
            Self::Fenced { queued } => format!(
                "terminal input is held in order behind an unresolved input ({queued} waiting)"
            ),
            Self::FenceFull { queued } => format!(
                "terminal input queue is full behind an unresolved input ({queued} waiting); keystroke not delivered"
            ),
        }
    }
}

const RETRY_INITIAL: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// How many inputs may wait behind one unresolved input operation.
///
/// The fence must be bounded in both directions: an unresolved input blocks the
/// PTY, so the queue behind it cannot be allowed to grow with every keystroke a
/// user types into a stalled pane.
const FENCE_QUEUE_MAX_INPUTS: usize = 64;
/// How many payload bytes may wait behind one unresolved input operation.
const FENCE_QUEUE_MAX_BYTES: usize = 8 * 1024;

/// Feedback for an input operation the daemon can no longer account for. The
/// fence stays: only discarding this session (or an explicit recovery policy)
/// may release it, never an automatic resend.
const FENCE_UNRESOLVED_MESSAGE: &str =
    "terminal input effect cannot be resolved by the daemon; later input stays held";

/// How many additional atomic snapshots one `connect` takes when a refused
/// snapshot may converge immediately (a resize that interleaved the capture).
/// A bounded retry keeps a hostile or persistently racing daemon from spinning
/// the redraw tick; the next attempt goes through the ordinary reconnect backoff.
const SNAPSHOT_RETRY_LIMIT: u32 = 1;

/// Feedback shown when the daemon cannot serve a semantic screen checkpoint.
/// The retained raw tail is deliberately not parsed, so this view starts empty
/// and fills from live output only.
const HISTORY_UNAVAILABLE_MESSAGE: &str = "terminal history is unavailable; this daemon cannot restore the screen, showing new output only";

/// One terminal input whose effect is unknown, retained as this terminal's
/// ordering fence until the daemon accounts for it.
///
/// The bytes are kept only to describe the fence, never to resend: the operation
/// identity is what resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnresolvedInput {
    operation: OperationId,
    length: usize,
    /// Whether the daemon has already answered "unknown" for this operation.
    /// It only ever forgets, so the answer cannot change and is not asked again;
    /// the fence stays latched instead of being polled every redraw tick.
    exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputUncertainty {
    first: String,
    latest: String,
    count: u64,
}

impl InputUncertainty {
    fn message(&self) -> String {
        if self.count == 1 {
            self.first.clone()
        } else {
            format!(
                "{} terminal inputs have uncertain effects; first: {}; latest: {}",
                self.count, self.first, self.latest
            )
        }
    }
}

/// A polling view of one daemon-owned terminal and its rendered screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSession {
    terminal: TerminalRef,
    geometry: Geometry,
    /// The viewport size last accepted by the daemon PTY.  This remains
    /// `None` after a transport failure so an unchanged outer-terminal size
    /// is retried on the next redraw instead of leaving the PTY at its old
    /// width indefinitely.
    synchronized_geometry: Option<Geometry>,
    /// One-shot permission to reuse `synchronized_geometry` after this client
    /// explicitly released only its subscription. Transport reconnects never
    /// set it and therefore always reassert PTY geometry.
    detached_geometry: bool,
    screen: TerminalScreen,
    subscription: Option<TerminalSubscription>,
    cursor: u64,
    input_seq: u64,
    /// The transport epoch whose daemon-side input ledger `input_seq` belongs to.
    ///
    /// Kept apart from `subscription` because a failure may release the
    /// subscription while the connection — and therefore the ledger the daemon
    /// counts this client's input on — survives.
    connection_epoch: Option<u64>,
    /// Whether the current screen carries restored history.
    history: TerminalHistory,
    /// The highest daemon terminal revision this session has applied, so an
    /// out-of-order snapshot cannot replace a newer screen.
    snapshot_revision: Option<u64>,
    state: SessionState,
    current_error: Option<String>,
    current_error_is_input: bool,
    error: Option<String>,
    input_uncertainty: Option<InputUncertainty>,
    /// The at-most-one input whose effect is unknown. While it is set this
    /// terminal's producer queue is fenced, so effect uncertainty is an ordering
    /// constraint and not only a message.
    ///
    /// It is deliberately independent of `subscription` and `connection_epoch`:
    /// a replacement subscription adopts the daemon ledger cursor, and neither
    /// that nor a new transport resolves what happened to an earlier operation.
    unresolved_input: Option<UnresolvedInput>,
    /// Inputs accepted behind the fence, in production order.
    fenced_queue: VecDeque<Vec<u8>>,
    fenced_bytes: usize,
    retry_attempt: u32,
    retry_at: Option<Instant>,
    /// Independent retry budget for the stateless resize lane. A failed resize
    /// must not be retried by every 16 ms frame while the daemon is saturated.
    resize_retry_attempt: u32,
    resize_retry_at: Option<Instant>,
}

impl TerminalSession {
    /// Creates a detached session for `terminal`; call [`Self::connect`] to
    /// attach.  The screen starts blank at the requested geometry.
    #[must_use]
    pub fn new(terminal: TerminalRef, geometry: Geometry) -> Self {
        let screen = screen_for(geometry);
        Self {
            terminal,
            geometry,
            synchronized_geometry: None,
            detached_geometry: false,
            screen,
            subscription: None,
            cursor: 0,
            input_seq: 0,
            connection_epoch: None,
            history: TerminalHistory::Restored,
            snapshot_revision: None,
            state: SessionState::Disconnected,
            current_error: None,
            current_error_is_input: false,
            error: None,
            input_uncertainty: None,
            unresolved_input: None,
            fenced_queue: VecDeque::new(),
            fenced_bytes: 0,
            retry_attempt: 0,
            retry_at: None,
            resize_retry_attempt: 0,
            resize_retry_at: None,
        }
    }

    /// The fenced identity of the daemon terminal this session mirrors.
    #[must_use]
    pub const fn terminal(&self) -> &TerminalRef {
        &self.terminal
    }

    /// The current connection status.
    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// A safe, human-readable transport failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Cheap fingerprint of every input that can change the rendered viewport.
    /// The decoded screen remains authoritative; this value is only a cache
    /// fence and advances through the daemon cursor/snapshot/geometry state.
    #[must_use]
    pub fn projection_key(&self) -> u64 {
        let mut key = std::collections::hash_map::DefaultHasher::new();
        self.cursor.hash(&mut key);
        self.snapshot_revision.hash(&mut key);
        self.geometry.rows.hash(&mut key);
        self.geometry.cols.hash(&mut key);
        (self.state as u8).hash(&mut key);
        (self.history as u8).hash(&mut key);
        self.error.hash(&mut key);
        key.finish()
    }

    /// Whether the current view restored the terminal's retained history, or is
    /// the limited view a daemon without checkpoint support fails closed to.
    #[must_use]
    pub const fn history(&self) -> TerminalHistory {
        self.history
    }

    /// The rendered screen rows.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.screen.rows()
    }

    /// The rows projected into an active terminal pane, including its cursor.
    #[must_use]
    pub fn display_rows(&self) -> Vec<String> {
        match self.state {
            SessionState::Live => self.screen.rows_with_cursor(),
            SessionState::Reconnecting
            | SessionState::Disconnected
            | SessionState::Orphaned
            | SessionState::Exited => self.screen.rows(),
        }
    }

    /// The retained terminal history projected into an active terminal pane.
    #[must_use]
    pub fn display_rows_with_scrollback(&self) -> Vec<String> {
        match self.state {
            SessionState::Live => self.screen.rows_with_scrollback_and_cursor(),
            SessionState::Reconnecting
            | SessionState::Disconnected
            | SessionState::Orphaned
            | SessionState::Exited => self.screen.rows_with_scrollback(),
        }
    }

    /// Number of retained rendered rows without projecting the scrollback.
    #[must_use]
    pub fn display_row_count(&self) -> usize {
        self.screen
            .rows_with_scrollback_count(self.state == SessionState::Live)
    }

    /// Number of retained rows needed to display live content and the complete
    /// selection highlight, including selected blank grid padding.
    #[must_use]
    pub fn display_row_count_selection(&self, selection: &TerminalSelection) -> usize {
        self.screen.rows_with_scrollback_selection_count(
            (selection.anchor().row, selection.anchor().column),
            (selection.focus().row, selection.focus().column),
        )
    }

    /// Render only the retained rows needed by the current viewport.
    #[must_use]
    pub fn display_row_window(&self, start: usize, end: usize) -> Vec<String> {
        self.screen
            .rows_with_scrollback_window(start, end, self.state == SessionState::Live)
    }

    /// Projects the retained output with a cell-precise visual selection.
    #[must_use]
    pub fn display_rows_with_scrollback_selection(
        &self,
        selection: &TerminalSelection,
    ) -> Vec<String> {
        self.display_row_window_selection(0, usize::MAX, selection)
    }

    /// Render only the retained rows needed by the current viewport, including
    /// a cell-precise selection highlight.
    #[must_use]
    pub fn display_row_window_selection(
        &self,
        start: usize,
        end: usize,
        selection: &TerminalSelection,
    ) -> Vec<String> {
        self.screen.rows_with_scrollback_window_selection(
            start,
            end,
            (selection.anchor().row, selection.anchor().column),
            (selection.focus().row, selection.focus().column),
        )
    }

    /// Complete visible screen cells for selection/copy. Unlike [`Self::rows`]
    /// this retains trailing spaces, while still containing no ANSI styling.
    #[must_use]
    pub fn cells(&self) -> Vec<String> {
        self.screen.cells_with_scrollback()
    }

    /// Starts a stable selection from the current visible terminal cells.
    /// Later stream output, reconnects, and screen replacement do not mutate
    /// the returned selection's copy text.
    #[must_use]
    pub fn begin_selection(&self, anchor: TerminalPoint) -> TerminalSelection {
        TerminalSelection::begin(self.cells(), anchor)
    }

    /// Synchronizes the daemon PTY to the visible pane before attaching (or
    /// reattaching) and rebuilding the screen from the daemon's semantic
    /// checkpoint.  This ensures an application that redraws on `SIGWINCH` is
    /// snapshotted at the same width as the right pane. A resize failure
    /// therefore cannot hide an otherwise attachable terminal.
    ///
    /// A snapshot whose geometry or revision does not fence against this pane is
    /// refused rather than mixed into the current view: the attempt is retried
    /// once atomically, and a still-mismatching snapshot leaves the previous
    /// screen intact and falls back to the ordinary reconnect backoff.
    pub fn connect<P: TerminalStreamPort>(&mut self, port: &mut P) {
        self.connect_at(port, Instant::now());
    }

    /// Connects at an injected monotonic instant. This is the deterministic
    /// clock boundary used by reconnect tests.
    pub fn connect_at<P: TerminalStreamPort>(&mut self, port: &mut P, now: Instant) {
        let reuse_detached_geometry = std::mem::take(&mut self.detached_geometry);
        for attempt in 0..=SNAPSHOT_RETRY_LIMIT {
            // A retained coordinator already synchronized at this exact
            // geometry does not need another SIGWINCH merely because its
            // subscription was released. A refused snapshot is different: its
            // immediate retry must reassert geometry because capture raced a
            // resize after the preceding synchronization.
            let resize_error = if attempt == 0
                && reuse_detached_geometry
                && self.synchronized_geometry == Some(self.geometry)
            {
                None
            } else {
                let error = port.resize(&self.terminal, self.geometry).err();
                self.synchronized_geometry = error.is_none().then_some(self.geometry);
                self.record_resize_result(error, now);
                error
            };
            let attach = match port.attach(&self.terminal, self.geometry) {
                Ok(attach) => attach,
                Err(error) => return self.fail_at(error, now),
            };
            match self.restore(&attach) {
                Ok(()) => {
                    // Release the superseded subscription only after the new one
                    // exists. Both halves of the identity are compared, so a
                    // reused wire id from another epoch still counts as
                    // superseded, and releasing it must leave the attachment
                    // just taken — and the transport carrying it — untouched.
                    if let Some(previous) = self.subscription
                        && previous != attach.subscription
                    {
                        port.detach(&self.terminal, previous);
                    }
                    self.commit(&attach);
                    if let Some(error) = resize_error {
                        self.set_current_error(Some(format!(
                            "terminal attached, but viewport synchronization failed: {}",
                            error_message(error)
                        )));
                    }
                    return;
                }
                Err(refusal) => {
                    // The refused view is never displayed, so its subscription
                    // is released instead of being left registered.
                    port.detach(&self.terminal, attach.subscription);
                    if attempt == SNAPSHOT_RETRY_LIMIT {
                        return self.resync_at(port, &refusal, now);
                    }
                }
            }
        }
    }

    /// Fetches and applies any output produced since the last cursor.  A cursor
    /// gap (retained output already trimmed) triggers a full reattach; the
    /// process having exited transitions to [`SessionState::Exited`].
    pub fn poll<P: TerminalStreamPort>(&mut self, port: &mut P) {
        self.poll_at(port, Instant::now());
    }

    /// Polls at an injected monotonic instant, retrying an unavailable daemon
    /// only after the capped exponential backoff expires.
    pub fn poll_at<P: TerminalStreamPort>(&mut self, port: &mut P, now: Instant) {
        // Resolve an unknown input effect before streaming again. The fence blocks
        // this terminal's producer queue, so converging it is what lets the held
        // input reach the PTY in its original order (#519).
        if self.state == SessionState::Live
            && !self.subscription_replaced(port)
            && let Some(unresolved) = self.pending_resolution()
        {
            self.resolve_input_fence_at(port, unresolved, now);
            return;
        }
        match self.state {
            // The shared transport was replaced, so this pane's attachment is
            // gone: take a fresh one before asking the new connection for
            // anything. Every pane does this independently, which is what keeps
            // one pane's reconnect from leaving its peers streaming on a
            // subscription the daemon has already released.
            SessionState::Live if self.subscription_replaced(port) => self.connect_at(port, now),
            SessionState::Live => match port.poll(&self.terminal, self.cursor) {
                Ok(chunks) => self.apply_at(port, chunks, now),
                Err(TerminalError::ResyncRequired) => self.connect_at(port, now),
                Err(error) => self.fail_at(error, now),
            },
            SessionState::Reconnecting if self.retry_at.is_some_and(|retry_at| now >= retry_at) => {
                self.connect_at(port, now);
            }
            SessionState::Reconnecting
            | SessionState::Disconnected
            | SessionState::Orphaned
            | SessionState::Exited => {}
        }
    }

    /// Resizes the daemon PTY and decoded terminal cells without replaying
    /// historical cursor movement sequences at the new width.
    pub fn resize<P: TerminalStreamPort>(&mut self, port: &mut P, geometry: Geometry) {
        self.resize_at(port, geometry, Instant::now());
    }

    fn resize_at<P: TerminalStreamPort>(&mut self, port: &mut P, geometry: Geometry, now: Instant) {
        if self.geometry != geometry {
            self.geometry = geometry;
            self.screen
                .resize(geometry.rows as usize, geometry.cols as usize);
            self.synchronized_geometry = None;
            self.resize_retry_attempt = 0;
            self.resize_retry_at = None;
        }
        if self.synchronized_geometry == Some(geometry)
            || self.state != SessionState::Live
            || self.resize_retry_at.is_some_and(|retry_at| now < retry_at)
        {
            return;
        }
        let error = port.resize(&self.terminal, geometry).err();
        self.synchronized_geometry = error.is_none().then_some(geometry);
        self.record_resize_result(error, now);
    }

    /// Sends input bytes to the terminal exactly once.
    ///
    /// # Errors
    ///
    /// Returns a typed outcome when no live subscription exists or when the
    /// daemon rejects the input. Input is never silently discarded.
    pub fn send_input<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        bytes: &[u8],
    ) -> Result<(), TerminalInputError> {
        self.send_input_at(port, bytes, Instant::now())
    }

    fn send_input_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        bytes: &[u8],
        now: Instant,
    ) -> Result<(), TerminalInputError> {
        // An input whose effect is unknown fences this terminal. Sending the next
        // keystroke now would risk reordering it against a request that may still
        // be applied, or concatenating it onto a command the unresolved input
        // half-wrote, so it is held in order instead.
        if self.unresolved_input.is_some() {
            return Err(self.enqueue_behind_fence(bytes));
        }
        self.write_input_at(port, bytes, now)
    }

    /// Sends one input immediately, issuing its durable operation identity.
    fn write_input_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        bytes: &[u8],
        now: Instant,
    ) -> Result<(), TerminalInputError> {
        // Re-attach before the first keystroke rather than spending it on a
        // subscription the daemon released with the previous connection: that
        // request would be rejected without effect, losing the key, and would
        // drop the socket the other panes just attached on.
        if self.state == SessionState::Live && self.subscription_replaced(port) {
            self.connect_at(port, now);
        }
        let (SessionState::Live, Some(subscription)) = (self.state, self.subscription) else {
            return Err(TerminalInputError::NotLive(self.state));
        };
        // Issued before the request is written, so the retry, reconnect, and
        // resolution of *this* logical input all name the same operation.
        let operation = OperationId::new();
        match port.input(
            &self.terminal,
            subscription,
            self.input_seq,
            operation,
            bytes,
        ) {
            Ok(outcome) => {
                self.input_seq += 1;
                match outcome {
                    TerminalInputOutcome::Written => {
                        self.clear_current_input_error();
                        Ok(())
                    }
                    TerminalInputOutcome::Failed | TerminalInputOutcome::Ambiguous { .. } => {
                        let error = TerminalInputError::Rejected(outcome);
                        let message = error.message();
                        if matches!(outcome, TerminalInputOutcome::Ambiguous { .. }) {
                            self.latch_input_uncertainty(message);
                        } else {
                            self.set_current_input_error(message);
                        }
                        Err(error)
                    }
                }
            }
            Err(error) => {
                // Only a lost acknowledgement leaves the effect unknown; a
                // definitive failure did not write and therefore does not fence
                // the ordering of what follows.
                if error == TerminalError::InputEffectUnknown {
                    self.unresolved_input = Some(UnresolvedInput {
                        operation,
                        length: bytes.len(),
                        exhausted: false,
                    });
                }
                self.fail_at(error, now);
                Err(TerminalInputError::Transport(error))
            }
        }
    }

    /// Accepts one input into the bounded ordered queue behind the fence, or
    /// refuses it as typed backpressure.
    fn enqueue_behind_fence(&mut self, bytes: &[u8]) -> TerminalInputError {
        if self.fenced_queue.len() >= FENCE_QUEUE_MAX_INPUTS
            || self.fenced_bytes.saturating_add(bytes.len()) > FENCE_QUEUE_MAX_BYTES
        {
            let error = TerminalInputError::FenceFull {
                queued: self.fenced_queue.len(),
            };
            self.set_current_input_error(error.message());
            return error;
        }
        self.fenced_queue.push_back(bytes.to_vec());
        self.fenced_bytes += bytes.len();
        let error = TerminalInputError::Fenced {
            queued: self.fenced_queue.len(),
        };
        self.set_current_input_error(error.message());
        error
    }

    /// The unresolved input this session may still ask the daemon about.
    ///
    /// An exhausted fence is not asked again: the ledger only ever forgets, so a
    /// second query cannot change the answer and would inflate the uncertainty
    /// aggregate on every redraw tick.
    fn pending_resolution(&self) -> Option<UnresolvedInput> {
        self.unresolved_input
            .filter(|unresolved| !unresolved.exhausted)
    }

    /// Asks the daemon what happened to the unresolved input, then releases the
    /// fence in production order when it converges.
    fn resolve_input_fence_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        unresolved: UnresolvedInput,
        now: Instant,
    ) {
        match port.input_outcome(&self.terminal, unresolved.operation, unresolved.length) {
            Ok(TerminalInputResolution::Final(outcome)) => {
                self.unresolved_input = None;
                // This input is no longer uncertain: the daemon accounted for it.
                // That is the only sanctioned way its warning goes away.
                self.retract_input_uncertainty();
                // A recorded non-success stays a non-success: resolution only
                // removes the uncertainty, it never upgrades the outcome.
                match outcome {
                    TerminalInputOutcome::Written => self.clear_current_input_error(),
                    TerminalInputOutcome::Failed | TerminalInputOutcome::Ambiguous { .. } => {
                        let message = TerminalInputError::Rejected(outcome).message();
                        if matches!(outcome, TerminalInputOutcome::Ambiguous { .. }) {
                            self.latch_input_uncertainty(message);
                        } else {
                            self.set_current_input_error(message);
                        }
                    }
                }
                self.drain_input_fence_at(port, now);
            }
            // The daemon cannot account for the operation. It only ever forgets,
            // so this answer is final: the fence latches and the queued input is
            // neither sent nor discarded.
            Ok(TerminalInputResolution::Unknown) => {
                self.unresolved_input = Some(UnresolvedInput {
                    exhausted: true,
                    ..unresolved
                });
                self.latch_input_uncertainty(FENCE_UNRESOLVED_MESSAGE.to_owned());
            }
            // Resolution itself failed; the fence and the query both stand.
            Err(error) => self.fail_at(error, now),
        }
    }

    /// Sends the inputs held behind a released fence, oldest first. A send that
    /// leaves another effect unknown re-establishes the fence and keeps the rest
    /// of the queue in order behind it.
    fn drain_input_fence_at<P: TerminalStreamPort>(&mut self, port: &mut P, now: Instant) {
        while self.unresolved_input.is_none() {
            let Some(bytes) = self.fenced_queue.pop_front() else {
                break;
            };
            self.fenced_bytes = self.fenced_bytes.saturating_sub(bytes.len());
            let _ = self.write_input_at(port, &bytes, now);
            if self.state != SessionState::Live {
                break;
            }
        }
    }

    /// How many inputs are held behind an unresolved input operation.
    #[must_use]
    pub fn fenced_input_count(&self) -> usize {
        self.fenced_queue.len()
    }

    /// The length of the input whose effect is unknown, when this terminal's
    /// producer queue is fenced by one.
    #[must_use]
    pub fn unresolved_input_length(&self) -> Option<usize> {
        self.unresolved_input
            .as_ref()
            .map(|unresolved| unresolved.length)
    }

    /// Releases the subscription without stopping the daemon terminal.
    pub fn detach<P: TerminalStreamPort>(&mut self, port: &mut P) {
        if let Some(subscription) = self.subscription.take() {
            port.detach(&self.terminal, subscription);
        }
        self.state = SessionState::Disconnected;
        self.detached_geometry = true;
        self.retry_at = None;
        self.retry_attempt = 0;
        self.set_current_error(Some("terminal detached".to_owned()));
    }

    fn apply_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        chunks: Vec<TerminalChunk>,
        now: Instant,
    ) {
        for chunk in chunks {
            let contiguous = chunk.start_offset == self.cursor
                && chunk.end_offset >= chunk.start_offset
                && chunk.end_offset - chunk.start_offset == chunk.data.len() as u64;
            if !contiguous {
                // Lost or overlapping output: rebuild from an atomic snapshot.
                self.connect_at(port, now);
                return;
            }
            self.screen.advance(&chunk.data);
            self.cursor = chunk.end_offset;
        }
    }

    /// Rebuilds the screen for `attach`, or refuses the snapshot and leaves the
    /// current view untouched.
    ///
    /// A checkpoint is reconstructed at the geometry the daemon captured it at —
    /// the grid authority's own dimensions — so a restored screen is never a
    /// blend of two geometries. Historical control bytes are never replayed.
    fn restore(&mut self, attach: &TerminalAttach) -> Result<(), SnapshotRefusal> {
        if let Some(seen) = self.snapshot_revision
            && attach.revision < seen
        {
            return Err(SnapshotRefusal::StaleRevision {
                seen,
                snapshot: attach.revision,
            });
        }
        let (screen, history) = match &attach.screen {
            TerminalAttachScreen::Checkpoint(checkpoint) => {
                self.fence_geometry(checkpoint)?;
                (
                    TerminalScreen::from_checkpoint(checkpoint)
                        .map_err(SnapshotRefusal::Rejected)?,
                    TerminalHistory::Restored,
                )
            }
            // Fail closed: the retained raw tail is never fed to the parser, so
            // this view starts blank and only live output appears.
            TerminalAttachScreen::HistoryUnavailable => {
                (screen_for(self.geometry), TerminalHistory::Unavailable)
            }
        };
        self.screen = screen;
        self.history = history;
        Ok(())
    }

    /// Refuses a checkpoint captured at a geometry other than the one this pane
    /// synchronized with the daemon. When the resize did not reach the daemon
    /// there is nothing to fence against, so the daemon's geometry is accepted
    /// and the unsynchronized viewport is reported separately.
    fn fence_geometry(&self, checkpoint: &ScreenCheckpoint) -> Result<(), SnapshotRefusal> {
        let expected = self.synchronized_geometry.filter(|_| {
            u32::from(self.geometry.rows) != checkpoint.geometry.rows
                || u32::from(self.geometry.cols) != checkpoint.geometry.cols
        });
        match expected {
            Some(expected) => Err(SnapshotRefusal::Geometry {
                expected,
                snapshot: (checkpoint.geometry.rows, checkpoint.geometry.cols),
            }),
            None => Ok(()),
        }
    }

    /// Adopts a restored view: its subscription, output cursor, revision fence
    /// and session state.
    fn commit(&mut self, attach: &TerminalAttach) {
        self.subscription = Some(attach.subscription);
        self.cursor = attach.output_offset;
        self.snapshot_revision = Some(attach.revision);
        // The daemon counts input per connection, so only a new epoch starts a
        // fresh ledger. A resync that merely replaces the subscription on the
        // same connection continues the sequence the daemon already expects.
        //
        // The attach ledger cursor is authoritative when the peer supplies it;
        // an older peer falls back to resetting only on a new epoch. An input
        // operation issued on an earlier epoch may still be outstanding, so
        // `unresolved_input` and its fenced queue are deliberately preserved:
        // recovering streaming must not declare a lost acknowledgement resolved.
        if let Some(next_input_seq) = attach.next_input_seq {
            self.input_seq = next_input_seq;
        } else if self.connection_epoch != Some(attach.subscription.epoch) {
            self.input_seq = 0;
        }
        self.connection_epoch = Some(attach.subscription.epoch);
        self.retry_attempt = 0;
        self.retry_at = None;
        self.state = if attach.exited {
            SessionState::Exited
        } else {
            SessionState::Live
        };
        let exit = attach
            .exited
            .then(|| error_message(TerminalError::Exited).to_owned());
        self.set_current_error(match (self.history, exit) {
            (TerminalHistory::Restored, exit) => exit,
            (TerminalHistory::Unavailable, None) => Some(HISTORY_UNAVAILABLE_MESSAGE.to_owned()),
            (TerminalHistory::Unavailable, Some(exit)) => {
                Some(format!("{exit}; {HISTORY_UNAVAILABLE_MESSAGE}"))
            }
        });
    }

    /// Whether this session holds a subscription taken on a transport
    /// incarnation the port has since replaced.
    ///
    /// Only a port that reports an epoch can invalidate anything, and only when
    /// it differs from the one this subscription was issued on. A session
    /// without a subscription has nothing to invalidate: it is already
    /// reconnecting.
    fn subscription_replaced<P: TerminalStreamPort>(&self, port: &P) -> bool {
        matches!(
            (self.subscription, port.connection_epoch()),
            (Some(subscription), Some(current)) if subscription.epoch != current
        )
    }

    fn record_resize_result(&mut self, error: Option<TerminalError>, now: Instant) {
        match error {
            None => {
                self.resize_retry_attempt = 0;
                self.resize_retry_at = None;
                self.set_current_error(None);
            }
            Some(error) => {
                self.resize_retry_at = Some(now + retry_delay(self.resize_retry_attempt));
                self.resize_retry_attempt = self.resize_retry_attempt.saturating_add(1);
                self.set_current_error(Some(format!(
                    "terminal viewport synchronization failed: {}",
                    error_message(error)
                )));
            }
        }
    }

    /// Falls back to a typed resync after a refused snapshot: the previous
    /// screen stays as it was, the stale subscription is released, and the
    /// ordinary reconnect backoff schedules the next atomic attach.
    fn resync_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        refusal: &SnapshotRefusal,
        now: Instant,
    ) {
        if let Some(previous) = self.subscription.take() {
            port.detach(&self.terminal, previous);
        }
        self.state = SessionState::Reconnecting;
        self.retry_at = Some(now + retry_delay(self.retry_attempt));
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        self.set_current_error(Some(refusal.message()));
    }

    fn fail_at(&mut self, error: TerminalError, now: Instant) {
        let state = match error {
            TerminalError::Unavailable | TerminalError::InputEffectUnknown => {
                self.subscription = None;
                self.state = SessionState::Reconnecting;
                self.retry_at = Some(now + retry_delay(self.retry_attempt));
                self.retry_attempt = self.retry_attempt.saturating_add(1);
                let message = error_message(error).to_owned();
                if error == TerminalError::InputEffectUnknown {
                    self.latch_input_uncertainty(message);
                } else {
                    self.set_current_error(Some(message));
                }
                return;
            }
            TerminalError::Orphaned => SessionState::Orphaned,
            TerminalError::Exited => SessionState::Exited,
            TerminalError::ResyncRequired
            | TerminalError::Stale
            | TerminalError::OrderingMismatch => SessionState::Disconnected,
        };
        if error != TerminalError::Exited {
            self.subscription = None;
        }
        self.retry_at = None;
        self.retry_attempt = 0;
        self.state = state;
        self.set_current_error(Some(error_message(error).to_owned()));
    }

    fn latch_input_uncertainty(&mut self, message: String) {
        match &mut self.input_uncertainty {
            Some(uncertainty) => {
                uncertainty.latest = message;
                uncertainty.count = uncertainty.count.saturating_add(1);
            }
            None => {
                self.input_uncertainty = Some(InputUncertainty {
                    first: message.clone(),
                    latest: message,
                    count: 1,
                });
            }
        }
        self.clear_current_input_error();
    }

    /// Removes the aggregate entry a now-resolved lost acknowledgement
    /// contributed.
    ///
    /// A durable answer from the daemon is the only thing that may retract an
    /// uncertainty; transport recovery and later successful input must not
    /// (#517). The aggregate keeps fixed memory (count plus first/latest), so
    /// when other uncertain inputs remain only the count is adjusted — the
    /// retained messages stay as they were rather than being invented.
    fn retract_input_uncertainty(&mut self) {
        self.input_uncertainty = match self.input_uncertainty.take() {
            Some(uncertainty) if uncertainty.count > 1 => Some(InputUncertainty {
                count: uncertainty.count - 1,
                ..uncertainty
            }),
            _ => None,
        };
        self.refresh_error();
    }

    fn set_current_input_error(&mut self, error: String) {
        self.current_error = Some(error);
        self.current_error_is_input = true;
        self.refresh_error();
    }

    fn set_current_error(&mut self, error: Option<String>) {
        self.current_error = error;
        self.current_error_is_input = false;
        self.refresh_error();
    }

    fn clear_current_input_error(&mut self) {
        if self.current_error_is_input {
            self.current_error = None;
            self.current_error_is_input = false;
        }
        self.refresh_error();
    }

    fn refresh_error(&mut self) {
        let uncertainty = self
            .input_uncertainty
            .as_ref()
            .map(InputUncertainty::message);
        self.error = match (&self.current_error, uncertainty) {
            (Some(current), Some(uncertainty)) if current != &uncertainty => Some(format!(
                "{current}; prior terminal input uncertainty: {uncertainty}"
            )),
            (Some(current), _) => Some(current.clone()),
            (None, Some(uncertainty)) => Some(uncertainty),
            (None, None) => None,
        };
    }
}

fn retry_delay(attempt: u32) -> Duration {
    RETRY_INITIAL
        .checked_mul(1_u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .unwrap_or(RETRY_MAX)
        .min(RETRY_MAX)
}

fn error_message(error: TerminalError) -> &'static str {
    match error {
        TerminalError::ResyncRequired => "terminal output is resynchronizing",
        TerminalError::Unavailable => "daemon unavailable; reconnecting",
        TerminalError::Stale => "terminal is no longer available",
        TerminalError::Orphaned => "terminal ownership is unknown; input is disabled",
        TerminalError::Exited => "terminal has exited",
        TerminalError::InputEffectUnknown => {
            "terminal input acknowledgement was lost; delivery is unknown"
        }
        TerminalError::OrderingMismatch => {
            "terminal input ordering is out of sync; input is disabled"
        }
    }
}

fn screen_for(geometry: Geometry) -> TerminalScreen {
    TerminalScreen::new(geometry.rows as usize, geometry.cols as usize)
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };
    use usagi_core::usecase::vt_screen::VtScreen;

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn geometry() -> Geometry {
        Geometry { cols: 20, rows: 3 }
    }

    #[derive(Default)]
    struct FakePort {
        attach: Vec<Result<TerminalAttach, TerminalError>>,
        polls: Vec<Result<Vec<TerminalChunk>, TerminalError>>,
        input: Option<TerminalError>,
        /// A failure applied to the next input only, then consumed.
        input_error_once: Option<TerminalError>,
        input_outcomes: Vec<TerminalInputOutcome>,
        inputs: Vec<(u64, u64, Vec<u8>)>,
        /// Every durable operation identity the session issued, in order.
        issued_operations: Vec<OperationId>,
        /// Scripted answers for the durable resolution query, oldest first.
        resolutions: Vec<Result<TerminalInputResolution, TerminalError>>,
        /// Operations the session asked the daemon to account for.
        resolution_queries: Vec<OperationId>,
        detached: Vec<TerminalSubscription>,
        resized: Vec<Geometry>,
        resize_error: Option<TerminalError>,
        resize_count_at_attach: Vec<usize>,
        attached_terminals: Vec<TerminalRef>,
        /// The shared transport epoch this port reports, when it models one.
        epoch: Option<u64>,
    }
    impl TerminalStreamPort for FakePort {
        fn connection_epoch(&self) -> Option<u64> {
            self.epoch
        }

        fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
            self.resized.push(geometry);
            self.resize_error.take().map_or(Ok(()), Err)
        }

        fn attach(
            &mut self,
            terminal: &TerminalRef,
            _: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            self.resize_count_at_attach.push(self.resized.len());
            self.attached_terminals.push(terminal.clone());
            self.attach.remove(0)
        }
        fn poll(&mut self, _: &TerminalRef, _: u64) -> Result<Vec<TerminalChunk>, TerminalError> {
            self.polls.remove(0)
        }
        fn input(
            &mut self,
            _: &TerminalRef,
            subscription: TerminalSubscription,
            input_seq: u64,
            operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            self.issued_operations.push(operation);
            if let Some(error) = self.input.or_else(|| self.input_error_once.take()) {
                return Err(error);
            }
            self.inputs
                .push((subscription.id, input_seq, bytes.to_vec()));
            if self.input_outcomes.is_empty() {
                Ok(TerminalInputOutcome::Written)
            } else {
                Ok(self.input_outcomes.remove(0))
            }
        }
        fn input_outcome(
            &mut self,
            _: &TerminalRef,
            operation: OperationId,
            _: usize,
        ) -> Result<TerminalInputResolution, TerminalError> {
            self.resolution_queries.push(operation);
            if self.resolutions.is_empty() {
                Ok(TerminalInputResolution::Unknown)
            } else {
                self.resolutions.remove(0)
            }
        }
        fn detach(&mut self, _: &TerminalRef, subscription: TerminalSubscription) {
            self.detached.push(subscription);
        }
    }

    struct DefaultResizePort;

    impl TerminalStreamPort for DefaultResizePort {
        fn attach(
            &mut self,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Err(TerminalError::Unavailable)
        }

        fn poll(&mut self, _: &TerminalRef, _: u64) -> Result<Vec<TerminalChunk>, TerminalError> {
            Err(TerminalError::Unavailable)
        }

        fn input(
            &mut self,
            _: &TerminalRef,
            _: TerminalSubscription,
            _: u64,
            _: OperationId,
            _: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            Err(TerminalError::Unavailable)
        }

        fn detach(&mut self, _: &TerminalRef, _: TerminalSubscription) {}
    }

    /// The checkpoint a daemon at `geometry` produces after receiving `bytes`:
    /// the grid authority parses every byte, so the client never sees them.
    fn checkpoint_of(bytes: &[u8], geometry: Geometry) -> TerminalAttachScreen {
        let mut screen = VtScreen::new(usize::from(geometry.rows), usize::from(geometry.cols));
        screen.advance(bytes);
        TerminalAttachScreen::Checkpoint(Box::new(screen.checkpoint()))
    }

    /// A subscription on the default test epoch.
    fn sub(id: u64) -> TerminalSubscription {
        TerminalSubscription { id, epoch: 1 }
    }

    fn attach(subscription: u64, offset: u64, replay: &[u8], exited: bool) -> TerminalAttach {
        attach_at(1, subscription, offset, replay, exited)
    }

    fn attach_at(
        connection_epoch: u64,
        subscription: u64,
        offset: u64,
        replay: &[u8],
        exited: bool,
    ) -> TerminalAttach {
        TerminalAttach {
            subscription: TerminalSubscription {
                id: subscription,
                epoch: connection_epoch,
            },
            revision: 1,
            output_offset: offset,
            next_input_seq: None,
            screen: checkpoint_of(replay, geometry()),
            exited,
        }
    }

    fn chunk(start: u64, data: &[u8]) -> TerminalChunk {
        TerminalChunk {
            start_offset: start,
            end_offset: start + data.len() as u64,
            data: data.to_vec(),
        }
    }

    #[test]
    fn projection_key_gates_terminal_row_and_link_scan_across_one_thousand_idle_ticks() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 20, b"https://example.com", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        let mut cached_key = None;
        let mut scans = 0;
        let mut rows = Vec::new();

        for _ in 0..1_000 {
            let key = session.projection_key();
            if cached_key != Some(key) {
                rows = session.display_row_window(0, usize::from(geometry().rows));
                cached_key = Some(key);
                scans += 1;
            }
        }

        assert_eq!(scans, 1);
        assert!(rows.join("\n").contains("https://example.com"));

        port.polls.push(Ok(vec![chunk(20, b"/changed")]));
        session.poll(&mut port);
        assert_ne!(cached_key, Some(session.projection_key()));
    }

    #[test]
    fn connect_renders_replay_and_poll_appends_contiguous_output() {
        let mut default_port = DefaultResizePort;
        assert_eq!(default_port.resize(&terminal(), geometry()), Ok(()));
        assert_eq!(
            default_port.attach(&terminal(), geometry()),
            Err(TerminalError::Unavailable)
        );
        assert_eq!(
            default_port.poll(&terminal(), 0),
            Err(TerminalError::Unavailable)
        );
        assert_eq!(
            default_port.input(&terminal(), sub(1), 0, OperationId::new(), b"x"),
            Err(TerminalError::Unavailable)
        );
        // A port without a durable ledger answers unknown, which keeps a lost
        // acknowledgement latched instead of replayed (#519).
        assert_eq!(
            default_port.input_outcome(&terminal(), OperationId::new(), 1),
            Ok(TerminalInputResolution::Unknown)
        );
        default_port.detach(&terminal(), sub(1));
        let mut port = FakePort {
            attach: vec![Ok(attach(7, 3, b"$ ", false))],
            polls: vec![Ok(vec![chunk(3, b"ls\r\n"), chunk(7, b"a.txt")])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(port.resized, vec![geometry()]);
        assert_eq!(session.rows()[0], "$");
        session.poll(&mut port);
        // The prompt echo advances a row; the command output follows it.
        assert_eq!(session.rows(), vec!["$ ls", "a.txt", ""]);
    }

    #[test]
    fn resizing_clips_current_and_retained_output_without_reattaching() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 3, b"old", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        let resized = Geometry { cols: 40, rows: 8 };
        session.resize(&mut port, resized);
        session.resize(&mut port, resized);

        assert_eq!(port.resized, vec![geometry(), resized]);
        assert_eq!(session.rows()[0], "old");
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(port.resize_count_at_attach, vec![1]);
    }

    #[test]
    fn attach_reporting_exit_marks_the_session_exited() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 4, b"done", true))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.state(), SessionState::Exited);
        assert_eq!(session.rows()[0], "done");
        // Polling an exited session is inert.
        session.poll(&mut port);
        assert_eq!(session.state(), SessionState::Exited);
    }

    #[test]
    fn display_rows_shows_the_cursor_only_while_live() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 2, b"$ ", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(
            session.display_rows()[0],
            "$ \x1b[7m\u{e0001} \x1b[0m".to_string()
        );

        for state in [
            SessionState::Reconnecting,
            SessionState::Disconnected,
            SessionState::Orphaned,
            SessionState::Exited,
        ] {
            session.state = state;
            assert_eq!(session.display_rows(), session.rows());
        }
    }

    #[test]
    fn scrollback_display_hides_the_cursor_after_the_session_stops() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 0, b"one\r\ntwo\r\nthree", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        // While live, the scrollback projection includes the cursor row (this is
        // what the controller's live-terminal viewport polls each frame).
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(
            session.display_rows_with_scrollback(),
            session.screen.rows_with_scrollback_and_cursor()
        );
        session.state = SessionState::Exited;
        assert_eq!(
            session.display_rows_with_scrollback(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn connect_failure_reports_safe_feedback_without_a_subscription() {
        let mut port = FakePort {
            attach: vec![Err(TerminalError::Unavailable)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(session.error(), Some("daemon unavailable; reconnecting"));
        assert_eq!(
            session.send_input(&mut port, b"ls\r"),
            Err(TerminalInputError::NotLive(SessionState::Reconnecting))
        );
        assert!(port.inputs.is_empty());
    }

    #[test]
    fn resize_failure_does_not_prevent_attach_or_hide_replay() {
        let mut port = FakePort {
            attach: vec![Ok(attach(7, 5, b"reply", false))],
            resize_error: Some(TerminalError::Unavailable),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        let now = Instant::now();

        session.connect_at(&mut port, now);

        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "reply");
        assert_eq!(port.resized, vec![geometry()]);
        assert_eq!(
            session.error(),
            Some(
                "terminal attached, but viewport synchronization failed: daemon unavailable; reconnecting"
            )
        );

        // Frame redraws inside the backoff do not open another connection.
        session.resize_at(&mut port, geometry(), now);
        session.resize_at(&mut port, geometry(), now + RETRY_INITIAL / 2);
        assert_eq!(port.resized, vec![geometry()]);

        // The retry is admitted at the backoff boundary.
        session.resize_at(&mut port, geometry(), now + RETRY_INITIAL);
        assert_eq!(port.resized, vec![geometry(), geometry()]);
        assert_eq!(session.error(), None);

        let changed = Geometry { cols: 30, rows: 4 };
        port.resize_error = Some(TerminalError::Stale);
        session.resize_at(&mut port, changed, now + RETRY_INITIAL);
        assert!(session.error().unwrap().contains("no longer available"));
        port.resize_error = Some(TerminalError::Unavailable);
        session.resize_at(&mut port, geometry(), now + RETRY_INITIAL);
        assert!(session.error().unwrap().contains("reconnecting"));
    }

    #[test]
    fn input_is_sent_once_with_a_monotonic_sequence() {
        let mut port = FakePort {
            attach: vec![Ok(attach(9, 0, b"", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"l"), Ok(()));
        assert_eq!(session.send_input(&mut port, b"s\r"), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(9, 0, b"l".to_vec()), (9, 1, b"s\r".to_vec())]
        );
    }

    #[test]
    fn known_input_outcomes_advance_sequence_without_losing_the_subscription() {
        let mut port = FakePort {
            attach: vec![Ok(attach(9, 0, b"", false))],
            input_outcomes: vec![
                TerminalInputOutcome::Failed,
                TerminalInputOutcome::Ambiguous { applied_prefix: 2 },
                TerminalInputOutcome::Written,
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);

        assert_eq!(
            session.send_input(&mut port, b"x"),
            Err(TerminalInputError::Rejected(TerminalInputOutcome::Failed))
        );
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(
            session.error(),
            Some("terminal input was not applied; retry manually")
        );

        assert_eq!(
            session.send_input(&mut port, b"abc"),
            Err(TerminalInputError::Rejected(
                TerminalInputOutcome::Ambiguous { applied_prefix: 2 }
            ))
        );
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(
            session.error(),
            Some("terminal input is uncertain; 2 bytes were applied before failure")
        );

        assert_eq!(session.send_input(&mut port, b"z"), Ok(()));
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(
            session.error(),
            Some("terminal input is uncertain; 2 bytes were applied before failure")
        );
        assert_eq!(
            port.inputs,
            vec![
                (9, 0, b"x".to_vec()),
                (9, 1, b"abc".to_vec()),
                (9, 2, b"z".to_vec()),
            ]
        );
    }

    #[test]
    fn same_connection_cursor_gap_reattach_preserves_the_next_input_sequence() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(11, 1, 0, b"", false)),
                Ok(attach_at(11, 2, 0, b"fresh", false)),
            ],
            polls: vec![Ok(vec![chunk(2, b"gap")])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"a"), Ok(()));

        session.poll(&mut port);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.send_input(&mut port, b"b"), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(1, 0, b"a".to_vec()), (2, 1, b"b".to_vec())]
        );
    }

    #[test]
    fn fresh_connection_epoch_resets_the_input_sequence() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(11, 1, 0, b"", false)),
                Ok(attach_at(12, 2, 0, b"", false)),
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"a"), Ok(()));

        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"b"), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(1, 0, b"a".to_vec()), (2, 0, b"b".to_vec())]
        );
    }

    #[test]
    fn daemon_ledger_cursor_is_adopted_when_a_detached_session_was_evicted() {
        let mut adopted = attach_at(11, 1, 0, b"", false);
        adopted.next_input_seq = Some(7);
        let mut port = FakePort {
            attach: vec![Ok(adopted)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"a"), Ok(()));
        assert_eq!(port.inputs, vec![(1, 7, b"a".to_vec())]);
    }

    /// The ordering half of #519. An input whose acknowledgement was lost fences
    /// this terminal: later keystrokes are held in order, the fence is resolved
    /// by asking about the *operation* (never by resending), and only then does
    /// the held input reach the PTY — still in the order it was produced.
    #[test]
    fn an_unknown_input_fences_the_queue_until_its_operation_resolves() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Ok(attach_at(2, 2, 0, b"", false)),
            ],
            input: Some(TerminalError::InputEffectUnknown),
            resolutions: vec![Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written,
            ))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        assert_eq!(
            session.send_input_at(&mut port, b"first", now),
            Err(TerminalInputError::Transport(
                TerminalError::InputEffectUnknown
            ))
        );
        let fenced = port.issued_operations[0];
        assert_eq!(session.unresolved_input_length(), Some(5));

        // Nothing more reaches the port while the effect is unknown.
        port.input = None;
        assert_eq!(
            session.send_input_at(&mut port, b"second", now),
            Err(TerminalInputError::Fenced { queued: 1 })
        );
        assert_eq!(
            session.send_input_at(&mut port, b"third", now),
            Err(TerminalInputError::Fenced { queued: 2 })
        );
        assert_eq!(port.inputs.len(), 0);
        assert_eq!(session.fenced_input_count(), 2);

        // Recover the transport. A fresh epoch restarts the epoch-local sequence
        // but leaves the fence and its queue intact.
        session.poll_at(&mut port, now + RETRY_INITIAL);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.fenced_input_count(), 2);
        assert_eq!(session.unresolved_input_length(), Some(5));

        // The next tick resolves the operation and releases the queue in order.
        session.poll_at(&mut port, now + RETRY_INITIAL);
        assert_eq!(port.resolution_queries, vec![fenced]);
        assert_eq!(session.unresolved_input_length(), None);
        assert_eq!(session.fenced_input_count(), 0);
        assert_eq!(
            port.inputs,
            vec![(2, 0, b"second".to_vec()), (2, 1, b"third".to_vec())]
        );
        // Every input carried its own durable identity; nothing was resent.
        assert_eq!(port.issued_operations.len(), 3);
        assert_eq!(
            port.issued_operations
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
    }

    /// A daemon that cannot account for the operation leaves the fence latched:
    /// the bytes are never resent, the queue is never reordered, and the query is
    /// not repeated on every redraw tick.
    #[test]
    fn an_unresolvable_operation_latches_the_fence_without_resending() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Ok(attach_at(2, 2, 0, b"", false)),
            ],
            input: Some(TerminalError::InputEffectUnknown),
            resolutions: vec![Ok(TerminalInputResolution::Unknown)],
            polls: vec![Ok(Vec::new()), Ok(Vec::new()), Ok(Vec::new())],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        let _ = session.send_input_at(&mut port, b"lost", now);
        port.input = None;
        let _ = session.send_input_at(&mut port, b"held", now);

        session.poll_at(&mut port, now + RETRY_INITIAL);
        for tick in 1..4 {
            session.poll_at(&mut port, now + RETRY_INITIAL * tick);
        }
        // Asked exactly once: the answer cannot change, so the fence latches.
        assert_eq!(port.resolution_queries.len(), 1);
        assert_eq!(session.unresolved_input_length(), Some(4));
        assert_eq!(session.fenced_input_count(), 1);
        assert!(port.inputs.is_empty());
        assert!(
            session
                .error()
                .is_some_and(|error| error.contains("cannot be resolved"))
        );

        // The bounded queue refuses further input instead of growing.
        for _ in 0..FENCE_QUEUE_MAX_INPUTS {
            let _ = session.send_input_at(&mut port, b"x", now);
        }
        assert_eq!(
            session.send_input_at(&mut port, b"x", now),
            Err(TerminalInputError::FenceFull {
                queued: FENCE_QUEUE_MAX_INPUTS
            })
        );
        assert_eq!(session.fenced_input_count(), FENCE_QUEUE_MAX_INPUTS);
        assert!(port.inputs.is_empty());
    }

    /// Resolution never upgrades an outcome, and a resolution query that fails on
    /// the transport leaves the fence exactly where it was.
    #[test]
    fn resolution_preserves_non_success_finals_and_survives_a_failed_query() {
        for (final_outcome, lingering) in [
            // A resolved `Failed` is certain: nothing uncertain is left behind.
            (TerminalInputOutcome::Failed, None),
            // A resolved `Ambiguous` is still uncertain and stays latched.
            (
                TerminalInputOutcome::Ambiguous { applied_prefix: 2 },
                Some("2 bytes were applied"),
            ),
        ] {
            let now = Instant::now();
            let mut port = FakePort {
                input: Some(TerminalError::InputEffectUnknown),
                attach: vec![
                    Ok(attach_at(1, 1, 0, b"", false)),
                    Ok(attach_at(2, 2, 0, b"", false)),
                    Ok(attach_at(3, 3, 0, b"", false)),
                ],
                resolutions: vec![
                    Err(TerminalError::Unavailable),
                    Ok(TerminalInputResolution::Final(final_outcome)),
                ],
                ..FakePort::default()
            };
            let mut session = TerminalSession::new(terminal(), geometry());
            session.connect_at(&mut port, now);
            let _ = session.send_input_at(&mut port, b"ab", now);
            port.input = None;
            let _ = session.send_input_at(&mut port, b"next", now);

            // Reattach, then a failing query: still fenced, still queued.
            session.poll_at(&mut port, now + RETRY_INITIAL);
            session.poll_at(&mut port, now + RETRY_INITIAL);
            assert_eq!(session.unresolved_input_length(), Some(2));
            assert_eq!(session.fenced_input_count(), 1);
            assert!(port.inputs.is_empty());

            // The second query converges on the recorded non-success, which is
            // reported as itself and still releases the ordered queue.
            session.poll_at(&mut port, now + RETRY_INITIAL * 2);
            session.poll_at(&mut port, now + RETRY_INITIAL * 2);
            assert_eq!(session.unresolved_input_length(), None);
            // The third attach is the one that finally resolved the fence, and
            // its fresh subscription restarted the epoch-local sequence at zero.
            assert_eq!(port.inputs, vec![(3, 0, b"next".to_vec())]);
            match lingering {
                Some(expected) => assert!(
                    session
                        .error()
                        .is_some_and(|error| error.contains(expected)),
                    "{:?}",
                    session.error()
                ),
                None => assert_eq!(session.error(), None),
            }
        }
    }

    /// Draining stops at the first input that leaves the session non-Live, and
    /// the rest of the queue keeps its order behind it.
    ///
    /// Resolving one fence must not turn the held input into a burst that races
    /// a transport which has just failed again.
    #[test]
    fn an_interrupted_drain_keeps_the_rest_of_the_queue_in_order() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Ok(attach_at(2, 2, 0, b"", false)),
            ],
            input: Some(TerminalError::InputEffectUnknown),
            resolutions: vec![Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written,
            ))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        let _ = session.send_input_at(&mut port, b"lost", now);
        port.input = None;
        let _ = session.send_input_at(&mut port, b"one", now);
        let _ = session.send_input_at(&mut port, b"two", now);

        // The transport fails again on the first drained input.
        port.input_error_once = Some(TerminalError::Unavailable);
        session.poll_at(&mut port, now + RETRY_INITIAL);
        session.poll_at(&mut port, now + RETRY_INITIAL);

        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(session.unresolved_input_length(), None);
        // "one" was attempted and definitively failed; "two" is still held.
        assert_eq!(session.fenced_input_count(), 1);
        assert!(port.inputs.is_empty());
    }

    /// Retracting a resolved input's uncertainty leaves the other uncertain
    /// inputs counted: durable resolution accounts for one input, not for all.
    #[test]
    fn retraction_only_removes_the_resolved_inputs_uncertainty() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Ok(attach_at(2, 2, 0, b"", false)),
            ],
            input_outcomes: vec![TerminalInputOutcome::Ambiguous { applied_prefix: 1 }],
            resolutions: vec![Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written,
            ))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        // One ambiguous input, then one whose acknowledgement is lost.
        let _ = session.send_input_at(&mut port, b"ab", now);
        port.input = Some(TerminalError::InputEffectUnknown);
        let _ = session.send_input_at(&mut port, b"cd", now);
        assert!(
            session
                .error()
                .is_some_and(|error| error.starts_with("2 terminal inputs have uncertain effects"))
        );

        port.input = None;
        session.poll_at(&mut port, now + RETRY_INITIAL);
        session.poll_at(&mut port, now + RETRY_INITIAL);
        // The resolved input is accounted for; the ambiguous one still is not.
        assert_eq!(
            session.error(),
            Some("terminal input is uncertain; 1 bytes were applied before failure")
        );
    }

    /// A definitive failure did not write, so it must not fence what follows.
    #[test]
    fn a_definitive_input_failure_does_not_fence_the_queue() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![Ok(attach_at(1, 1, 0, b"", false))],
            input_outcomes: vec![TerminalInputOutcome::Failed],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        assert_eq!(
            session.send_input_at(&mut port, b"a", now),
            Err(TerminalInputError::Rejected(TerminalInputOutcome::Failed))
        );
        assert_eq!(session.unresolved_input_length(), None);
        assert_eq!(session.send_input_at(&mut port, b"b", now), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(1, 0, b"a".to_vec()), (1, 1, b"b".to_vec())]
        );
    }

    #[test]
    fn same_socket_decode_failure_reattach_preserves_the_input_sequence() {
        let now = Instant::now();
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(21, 1, 0, b"", false)),
                Ok(attach_at(21, 2, 0, b"fresh", false)),
            ],
            polls: vec![Err(TerminalError::Unavailable)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);
        assert_eq!(session.send_input(&mut port, b"a"), Ok(()));

        session.poll_at(&mut port, now);
        session.poll_at(&mut port, now + RETRY_INITIAL);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.send_input(&mut port, b"b"), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(1, 0, b"a".to_vec()), (2, 1, b"b".to_vec())]
        );
    }

    #[test]
    fn a_replaced_connection_attaches_freshly_before_the_next_poll() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Ok(attach_at(2, 2, 0, b"fresh", false)),
            ],
            // Consuming this would move the session off `Live`, so it fixes that
            // no `Resume` is sent on the replaced attachment.
            polls: vec![Err(TerminalError::Stale)],
            epoch: Some(1),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"a"), Ok(()));

        // The port replaced the shared transport, so the daemon released this
        // pane's attachment together with the connection.
        port.epoch = Some(2);
        session.poll(&mut port);

        assert_eq!(port.polls.len(), 1);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "fresh");
        assert_eq!(
            session.subscription,
            Some(TerminalSubscription { id: 2, epoch: 2 })
        );
        // The superseded subscription is released with the epoch it was taken
        // on, which is what lets the port keep the release local.
        assert_eq!(port.detached, [TerminalSubscription { id: 1, epoch: 1 }]);
        // A new connection means a new daemon-side ledger, so the sequence
        // restarts instead of leaving a gap.
        assert_eq!(session.send_input(&mut port, b"b"), Ok(()));
        assert_eq!(
            port.inputs,
            vec![(1, 0, b"a".to_vec()), (2, 0, b"b".to_vec())]
        );
    }

    #[test]
    fn the_first_key_after_a_replaced_connection_is_written_once_on_a_fresh_subscription() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(7, 1, 0, b"", false)),
                Ok(attach_at(8, 2, 0, b"", false)),
            ],
            epoch: Some(7),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);

        // No poll intervenes: the keystroke itself finds the stale attachment.
        port.epoch = Some(8);
        assert_eq!(session.send_input(&mut port, b"x"), Ok(()));

        // Written exactly once, on the subscription the fresh attach returned —
        // never on the released one, which the daemon would reject without
        // effect and which would cost the keystroke.
        assert_eq!(port.inputs, vec![(2, 0, b"x".to_vec())]);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.error(), None);
    }

    #[test]
    fn a_replaced_connection_whose_attach_fails_reports_reconnecting_without_input() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(1, 1, 0, b"", false)),
                Err(TerminalError::Unavailable),
            ],
            epoch: Some(1),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);

        port.epoch = Some(2);
        assert_eq!(
            session.send_input(&mut port, b"x"),
            Err(TerminalInputError::NotLive(SessionState::Reconnecting))
        );
        assert!(port.inputs.is_empty());
        assert_eq!(session.state(), SessionState::Reconnecting);
    }

    #[test]
    fn input_failure_reports_safe_feedback() {
        let mut port = FakePort {
            attach: vec![Ok(attach(9, 0, b"", false))],
            input: Some(TerminalError::Stale),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(
            session.send_input(&mut port, b"x"),
            Err(TerminalInputError::Transport(TerminalError::Stale))
        );
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(session.error(), Some("terminal is no longer available"));
    }

    #[test]
    fn unknown_input_effect_never_advances_sequence_or_replays_the_bytes() {
        let mut port = FakePort {
            attach: vec![Ok(attach(9, 0, b"", false))],
            input: Some(TerminalError::InputEffectUnknown),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);

        assert_eq!(
            session.send_input(&mut port, b"x"),
            Err(TerminalInputError::Transport(
                TerminalError::InputEffectUnknown
            ))
        );
        assert_eq!(session.input_seq, 0);
        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(
            session.error(),
            Some("terminal input acknowledgement was lost; delivery is unknown")
        );
        assert!(port.inputs.is_empty());
        // The unknown effect fences this terminal's producer queue, so the next
        // keystroke is held in order rather than merely refused: it must not be
        // able to overtake an input that may still be applied (#519).
        assert_eq!(
            session.send_input(&mut port, b"y"),
            Err(TerminalInputError::Fenced { queued: 1 })
        );
        assert!(port.inputs.is_empty());
    }

    #[test]
    fn unknown_input_warning_survives_recovery_and_composes_with_a_later_fatal_error() {
        let mut clock = FakeClock(Instant::now());
        let mut port = FakePort {
            attach: vec![
                Ok(attach_at(31, 1, 0, b"", false)),
                Ok(attach_at(32, 2, 0, b"fresh", false)),
            ],
            input: Some(TerminalError::InputEffectUnknown),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, clock.0);
        assert_eq!(
            session.send_input_at(&mut port, b"x", clock.0),
            Err(TerminalInputError::Transport(
                TerminalError::InputEffectUnknown
            ))
        );

        port.input = None;
        clock.advance(RETRY_INITIAL);
        session.poll_at(&mut port, clock.0);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(
            session.error(),
            Some("terminal input acknowledgement was lost; delivery is unknown")
        );
        // Resolving the fence converges the lost acknowledgement on the daemon's
        // recorded final, which here is itself uncertain: the warning is replaced
        // by what the daemon actually recorded, not cleared.
        port.resolutions.push(Ok(TerminalInputResolution::Final(
            TerminalInputOutcome::Ambiguous { applied_prefix: 1 },
        )));
        session.poll_at(&mut port, clock.0);
        assert_eq!(
            session.error(),
            Some("terminal input is uncertain; 1 bytes were applied before failure")
        );

        // A second, independently uncertain input aggregates with it instead of
        // overwriting it.
        port.input_outcomes
            .push(TerminalInputOutcome::Ambiguous { applied_prefix: 1 });
        assert_eq!(
            session.send_input(&mut port, b"yz"),
            Err(TerminalInputError::Rejected(
                TerminalInputOutcome::Ambiguous { applied_prefix: 1 }
            ))
        );
        let uncertainty = session.error().unwrap();
        assert!(uncertainty.starts_with("2 terminal inputs have uncertain effects"));
        assert!(uncertainty.contains("1 bytes were applied"));

        port.polls.push(Err(TerminalError::Orphaned));
        session.poll_at(&mut port, clock.0);
        let feedback = session.error().unwrap();
        assert!(feedback.starts_with("terminal ownership is unknown"));
        assert!(feedback.contains("prior terminal input uncertainty"));
        assert!(feedback.contains("2 terminal inputs have uncertain effects"));
    }

    #[test]
    fn a_cursor_gap_triggers_a_full_reattach() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach(1, 0, b"", false)),
                Ok(attach(2, 5, b"fresh", false)),
            ],
            // Non-contiguous: the daemon trimmed output before offset 2.
            polls: vec![Ok(vec![chunk(2, b"late")])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.poll(&mut port);
        assert_eq!(session.rows()[0], "fresh");
        assert_eq!(session.state(), SessionState::Live);
    }

    #[test]
    fn a_mismatched_chunk_length_also_reattaches() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 0, b"", false)), Ok(attach(2, 0, b"ok", false))],
            polls: vec![Ok(vec![TerminalChunk {
                start_offset: 0,
                end_offset: 9,
                data: b"short".to_vec(),
            }])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.poll(&mut port);
        assert_eq!(session.rows()[0], "ok");
    }

    #[test]
    fn a_trimmed_output_cursor_reattaches_to_the_atomic_snapshot() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach(1, 0, b"old", false)),
                Ok(attach(2, 12, b"fresh output", false)),
            ],
            polls: vec![Err(TerminalError::ResyncRequired)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.poll(&mut port);
        assert_eq!(session.rows()[0], "fresh output");
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.error(), None);

        // `poll` recovers this error before calling `fail`, but keep the
        // defensive terminal-state mapping covered as well.
        session.fail_at(TerminalError::ResyncRequired, Instant::now());
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(session.error(), Some("terminal output is resynchronizing"));
    }

    #[test]
    fn poll_reporting_exit_transitions_to_exited() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 0, b"", false))],
            polls: vec![Err(TerminalError::Exited)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.poll(&mut port);
        assert_eq!(session.state(), SessionState::Exited);
    }

    #[test]
    fn input_ordering_mismatch_disables_input_without_claiming_daemon_unavailability() {
        let mut session = TerminalSession::new(terminal(), geometry());

        session.fail_at(TerminalError::OrderingMismatch, Instant::now());

        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(
            session.error(),
            Some("terminal input ordering is out of sync; input is disabled")
        );
    }

    #[test]
    fn poll_transport_failure_reports_orphaned_and_disables_input() {
        let mut port = FakePort {
            attach: vec![Ok(attach(1, 0, b"", false))],
            polls: vec![Err(TerminalError::Orphaned)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.poll(&mut port);
        assert_eq!(session.state(), SessionState::Orphaned);
        assert_eq!(
            session.error(),
            Some("terminal ownership is unknown; input is disabled")
        );
    }

    #[test]
    fn detach_releases_the_subscription_and_reconnect_preserves_input_ordering() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach(4, 0, b"", false)),
                Ok(attach(5, 0, b"back", false)),
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"before"), Ok(()));
        session.detach(&mut port);
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(port.detached, [4].map(sub));
        // A second detach without a subscription is a no-op on the port.
        session.detach(&mut port);
        assert_eq!(port.detached, [4].map(sub));
        session.connect(&mut port);
        assert_eq!(session.send_input(&mut port, b"after"), Ok(()));
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "back");
        assert_eq!(
            port.resized,
            vec![geometry()],
            "same-geometry explicit reattach must not resend resize"
        );
        assert_eq!(
            port.inputs,
            vec![(4, 0, b"before".to_vec()), (5, 1, b"after".to_vec())]
        );
        assert_eq!(session.terminal().terminal_id, session.terminal.terminal_id);
    }

    #[test]
    fn detach_and_reattach_preserve_an_unresolved_input_and_its_fenced_queue() {
        let mut reattached = attach(5, 0, b"", false);
        reattached.next_input_seq = Some(1);
        let mut port = FakePort {
            attach: vec![Ok(attach(4, 0, b"", false)), Ok(reattached)],
            input_error_once: Some(TerminalError::InputEffectUnknown),
            resolutions: vec![Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written,
            ))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        assert!(session.send_input(&mut port, b"uncertain").is_err());
        assert!(session.send_input(&mut port, b"queued").is_err());
        assert_eq!(session.unresolved_input_length(), Some(9));
        assert_eq!(session.fenced_input_count(), 1);

        session.detach(&mut port);
        session.connect(&mut port);
        assert_eq!(session.unresolved_input_length(), Some(9));
        assert_eq!(session.fenced_input_count(), 1);

        session.poll(&mut port);
        assert_eq!(session.unresolved_input_length(), None);
        assert_eq!(session.fenced_input_count(), 0);
        assert_eq!(port.inputs, vec![(5, 1, b"queued".to_vec())]);
    }

    #[derive(Clone, Copy)]
    struct FakeClock(Instant);

    impl FakeClock {
        fn advance(&mut self, duration: Duration) {
            self.0 += duration;
        }
    }

    #[test]
    fn unavailable_retries_same_terminal_with_capped_backoff_and_resets_after_attach() {
        let mut clock = FakeClock(Instant::now());
        let mut port = FakePort {
            attach: vec![
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Err(TerminalError::Unavailable),
                Ok(attach(7, 5, b"back", false)),
            ],
            ..FakePort::default()
        };
        let terminal = terminal();
        let mut session = TerminalSession::new(terminal.clone(), geometry());

        session.connect_at(&mut port, clock.0);
        for delay in [100, 200, 400, 800, 1_600, 2_000, 2_000] {
            clock.advance(Duration::from_millis(delay - 1));
            session.poll_at(&mut port, clock.0);
            clock.advance(Duration::from_millis(1));
            session.poll_at(&mut port, clock.0);
        }

        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "back");
        assert!(
            port.attached_terminals
                .iter()
                .all(|attached| attached == &terminal)
        );
        assert_eq!(session.retry_attempt, 0);
        assert_eq!(session.retry_at, None);

        port.polls.push(Err(TerminalError::Unavailable));
        session.poll_at(&mut port, clock.0);
        assert_eq!(session.retry_at, Some(clock.0 + Duration::from_millis(100)));
    }

    #[test]
    fn detach_cancels_a_scheduled_retry_and_non_live_input_is_typed() {
        let mut clock = FakeClock(Instant::now());
        let mut port = FakePort {
            attach: vec![
                Ok(attach(4, 0, b"", false)),
                Ok(attach(5, 0, b"unexpected", false)),
            ],
            polls: vec![Err(TerminalError::Unavailable)],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, clock.0);
        session.poll_at(&mut port, clock.0);
        assert_eq!(session.state(), SessionState::Reconnecting);

        session.detach(&mut port);
        clock.advance(RETRY_MAX * 2);
        session.poll_at(&mut port, clock.0);

        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(port.attached_terminals.len(), 1);
        assert_eq!(session.retry_at, None);
        assert_eq!(
            session.send_input(&mut port, b"x"),
            Err(TerminalInputError::NotLive(SessionState::Disconnected))
        );
    }

    #[test]
    fn every_input_failure_has_explicit_effect_feedback() {
        let outcomes = [
            TerminalInputError::NotLive(SessionState::Live),
            TerminalInputError::NotLive(SessionState::Reconnecting),
            TerminalInputError::NotLive(SessionState::Disconnected),
            TerminalInputError::NotLive(SessionState::Orphaned),
            TerminalInputError::NotLive(SessionState::Exited),
            TerminalInputError::Transport(TerminalError::ResyncRequired),
            TerminalInputError::Transport(TerminalError::Unavailable),
            TerminalInputError::Transport(TerminalError::Stale),
            TerminalInputError::Transport(TerminalError::Orphaned),
            TerminalInputError::Transport(TerminalError::Exited),
            TerminalInputError::Rejected(TerminalInputOutcome::Written),
            TerminalInputError::Rejected(TerminalInputOutcome::Failed),
            TerminalInputError::Rejected(TerminalInputOutcome::Ambiguous { applied_prefix: 1 }),
            TerminalInputError::Transport(TerminalError::InputEffectUnknown),
            TerminalInputError::Transport(TerminalError::OrderingMismatch),
        ];
        for outcome in outcomes {
            assert!(!outcome.message().is_empty());
        }
        assert!(
            TerminalInputError::Rejected(TerminalInputOutcome::Failed)
                .message()
                .contains("not applied")
        );
        assert_eq!(
            TerminalInputError::Rejected(TerminalInputOutcome::Written).message(),
            "terminal returned an invalid input outcome"
        );
        for uncertain in [
            TerminalInputError::Rejected(TerminalInputOutcome::Ambiguous { applied_prefix: 1 }),
            TerminalInputError::Transport(TerminalError::InputEffectUnknown),
        ] {
            assert!(!uncertain.message().contains("not delivered"));
            assert!(
                uncertain.message().contains("uncertain")
                    || uncertain.message().contains("unknown")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_socket_restart_reconnects_and_resyncs_the_same_terminal() {
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::thread;

        struct SocketPort {
            path: PathBuf,
            next_attach: TerminalAttach,
            attached: Vec<TerminalRef>,
        }

        impl SocketPort {
            fn available(&self) -> Result<(), TerminalError> {
                UnixStream::connect(&self.path)
                    .map(drop)
                    .map_err(|_| TerminalError::Unavailable)
            }
        }

        impl TerminalStreamPort for SocketPort {
            fn resize(&mut self, _: &TerminalRef, _: Geometry) -> Result<(), TerminalError> {
                self.available()
            }

            fn attach(
                &mut self,
                terminal: &TerminalRef,
                _: Geometry,
            ) -> Result<TerminalAttach, TerminalError> {
                self.available()?;
                self.attached.push(terminal.clone());
                Ok(self.next_attach.clone())
            }

            fn poll(
                &mut self,
                _: &TerminalRef,
                _: u64,
            ) -> Result<Vec<TerminalChunk>, TerminalError> {
                self.available().map(|()| Vec::new())
            }

            fn input(
                &mut self,
                _: &TerminalRef,
                _: TerminalSubscription,
                _: u64,
                _: OperationId,
                _: &[u8],
            ) -> Result<TerminalInputOutcome, TerminalError> {
                self.available().map(|()| TerminalInputOutcome::Written)
            }

            fn detach(&mut self, _: &TerminalRef, _: TerminalSubscription) {
                let _ = self.available();
            }
        }

        fn serve(listener: UnixListener, connections: usize) -> thread::JoinHandle<()> {
            thread::spawn(move || {
                for _ in 0..connections {
                    listener.accept().expect("test socket accepts connection");
                }
            })
        }

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("terminal.sock");
        let first_server = serve(UnixListener::bind(&path).unwrap(), 2);
        let terminal = terminal();
        let mut port = SocketPort {
            path: path.clone(),
            next_attach: attach(1, 3, b"old", false),
            attached: Vec::new(),
        };
        let start = Instant::now();
        let mut session = TerminalSession::new(terminal.clone(), geometry());
        session.connect_at(&mut port, start);
        first_server.join().unwrap();

        session.poll_at(&mut port, start);
        assert_eq!(session.state(), SessionState::Reconnecting);

        std::fs::remove_file(&path).unwrap();
        let restarted_server = serve(UnixListener::bind(&path).unwrap(), 5);
        port.next_attach = attach(2, 5, b"fresh", false);
        session.poll_at(&mut port, start + RETRY_INITIAL);

        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "fresh");
        assert_eq!(port.attached, vec![terminal.clone(), terminal]);
        session.poll_at(&mut port, start + RETRY_INITIAL);
        assert_eq!(session.send_input(&mut port, b"x"), Ok(()));
        session.detach(&mut port);
        restarted_server.join().unwrap();
    }

    /// The checkpoint the daemon produces at a geometry other than the pane's,
    /// i.e. after a resize interleaved the capture.
    fn checkpoint_at(bytes: &[u8], geometry: Geometry) -> TerminalAttachScreen {
        checkpoint_of(bytes, geometry)
    }

    /// A reference screen fed every byte contiguously — what an untrimmed client
    /// would render. Restoring a checkpoint plus its suffix must match it.
    fn reference(bytes: &[u8]) -> TerminalScreen {
        let mut screen = screen_for(geometry());
        screen.advance(bytes);
        screen
    }

    #[test]
    fn a_daemon_without_checkpoints_shows_no_history_instead_of_parsing_a_tail() {
        let mut port = FakePort {
            attach: vec![Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 3, epoch: 1 },
                revision: 7,
                output_offset: 64,
                next_input_seq: None,
                screen: TerminalAttachScreen::HistoryUnavailable,
                exited: false,
            })],
            polls: vec![Ok(vec![chunk(64, b"live")])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);

        // Live, but explicitly history-less: nothing was reconstructed and no
        // retained byte was parsed, so the screen is blank.
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.history(), TerminalHistory::Unavailable);
        assert_eq!(session.rows(), vec!["", "", ""]);
        assert_eq!(session.error(), Some(HISTORY_UNAVAILABLE_MESSAGE));

        // Only output after the attach offset is rendered.
        session.poll(&mut port);
        assert_eq!(session.rows()[0], "live");
        assert_eq!(session.history(), TerminalHistory::Unavailable);
    }

    #[test]
    fn a_history_less_exited_attach_reports_both_facts() {
        let mut port = FakePort {
            attach: vec![Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 3, epoch: 1 },
                revision: 1,
                output_offset: 0,
                next_input_seq: None,
                screen: TerminalAttachScreen::HistoryUnavailable,
                exited: true,
            })],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);

        assert_eq!(session.state(), SessionState::Exited);
        let error = session.error().unwrap();
        assert!(error.starts_with("terminal has exited"));
        assert!(error.contains("history is unavailable"));
    }

    #[test]
    fn a_checkpoint_taken_mid_sequence_matches_an_untrimmed_reference() {
        // The retained window starts mid CSI and mid UTF-8: a raw tail would
        // expose these bytes as text, while a checkpoint carries the decoder
        // state so the suffix continues the sequence.
        let complete = "\u{1b}[31mred\u{1b}[1mあ".as_bytes();
        for split in 1..complete.len() {
            let (head, suffix) = complete.split_at(split);
            let mut port = FakePort {
                attach: vec![Ok(TerminalAttach {
                    subscription: TerminalSubscription { id: 1, epoch: 1 },
                    revision: 1,
                    output_offset: head.len() as u64,
                    next_input_seq: None,
                    screen: checkpoint_of(head, geometry()),
                    exited: false,
                })],
                polls: vec![Ok(vec![chunk(head.len() as u64, suffix)])],
                ..FakePort::default()
            };
            let mut session = TerminalSession::new(terminal(), geometry());

            session.connect(&mut port);
            session.poll(&mut port);

            let expected = reference(complete);
            assert_eq!(session.history(), TerminalHistory::Restored);
            assert_eq!(
                session.display_rows(),
                expected.rows_with_cursor(),
                "split at {split} must match the reference cells, style and cursor"
            );
            assert_eq!(session.cells(), expected.cells_with_scrollback());
        }
    }

    #[test]
    fn an_alternate_buffer_checkpoint_restores_the_saved_primary_and_copy_history() {
        // The primary transcript, its scrollback and the live alternate frame
        // exist only in the checkpoint: the raw journal window is long gone.
        let head = b"one\r\ntwo\r\nthree\r\n\x1b[?1049h\x1b[1;1Halt-frame";
        let suffix = b"\x1b[?1049lback";
        let mut port = FakePort {
            attach: vec![Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: head.len() as u64,
                next_input_seq: None,
                screen: checkpoint_of(head, geometry()),
                exited: false,
            })],
            polls: vec![Ok(vec![chunk(head.len() as u64, suffix)])],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);

        // While the alternate buffer is active it alone is visible, and its
        // frame is not mixed into the scrollback copy history.
        let in_alternate = reference(head);
        assert_eq!(session.rows(), in_alternate.rows());
        assert_eq!(session.cells(), in_alternate.cells_with_scrollback());

        session.poll(&mut port);

        // Leaving the alternate buffer restores the saved primary buffer with
        // its scrollback, matching an untrimmed reference exactly.
        let expected = reference(&[head.as_slice(), suffix.as_slice()].concat());
        assert_eq!(session.cells(), expected.cells_with_scrollback());
        assert_eq!(session.display_rows(), expected.rows_with_cursor());
        assert!(session.cells().iter().any(|row| row.contains("one")));
        assert!(!session.cells().iter().any(|row| row.contains("alt-frame")));
    }

    #[test]
    fn a_checkpoint_captured_at_another_geometry_retries_then_resyncs() {
        let interleaved = Geometry { cols: 40, rows: 4 };
        let stale = |subscription: u64| TerminalAttach {
            subscription: sub(subscription),
            revision: 2,
            output_offset: 5,
            next_input_seq: None,
            screen: checkpoint_at(b"wide", interleaved),
            exited: false,
        };
        let mut port = FakePort {
            attach: vec![
                Ok(attach(1, 3, b"old", false)),
                // A resize interleaves both snapshots of the reconnect.
                Ok(stale(2)),
                Ok(stale(3)),
                // The retry after the backoff converges on the pane geometry.
                Ok(attach(4, 9, b"converged", false)),
            ],
            polls: vec![Err(TerminalError::ResyncRequired)],
            ..FakePort::default()
        };
        let now = Instant::now();
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);

        session.poll_at(&mut port, now);

        // Both refused snapshots are released, then the previous subscription:
        // neither refused view is displayed, and the previous screen stays
        // intact rather than being mixed with the wider one.
        assert_eq!(port.detached, [2, 3, 1].map(sub));
        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(session.rows()[0], "old");
        assert_eq!(session.subscription, None);
        let error = session.error().unwrap();
        assert!(error.contains("changed size during attach"), "{error}");
        assert!(error.contains("snapshot 40x4"), "{error}");
        assert!(error.contains("pane 20x3"), "{error}");

        session.poll_at(&mut port, now + RETRY_INITIAL);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "converged");
        assert_eq!(session.error(), None);
    }

    #[test]
    fn one_interleaved_snapshot_converges_on_the_immediate_retry() {
        let mut port = FakePort {
            attach: vec![
                Ok(TerminalAttach {
                    subscription: TerminalSubscription { id: 1, epoch: 1 },
                    revision: 2,
                    output_offset: 4,
                    next_input_seq: None,
                    screen: checkpoint_at(b"wide", Geometry { cols: 40, rows: 4 }),
                    exited: false,
                }),
                Ok(attach(2, 6, b"paned", false)),
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);

        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "paned");
        assert_eq!(session.error(), None);
        assert_eq!(port.detached, [1].map(sub));
        // Each attempt re-synchronizes the viewport before capturing.
        assert_eq!(port.resized, vec![geometry(), geometry()]);
    }

    #[test]
    fn an_unsynchronized_viewport_accepts_the_daemon_geometry_instead_of_hiding_output() {
        let daemon_geometry = Geometry { cols: 40, rows: 4 };
        let mut port = FakePort {
            attach: vec![Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: 4,
                next_input_seq: None,
                screen: checkpoint_at(b"wide", daemon_geometry),
                exited: false,
            })],
            resize_error: Some(TerminalError::Unavailable),
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());

        session.connect(&mut port);

        // There is nothing to fence against when the resize never reached the
        // daemon, so its own geometry is restored and the failure is reported.
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.history(), TerminalHistory::Restored);
        assert_eq!(session.rows()[0], "wide");
        assert!(
            session
                .error()
                .unwrap()
                .contains("viewport synchronization failed")
        );
    }

    #[test]
    fn a_snapshot_older_than_the_applied_revision_is_refused() {
        let stale = |subscription: u64| TerminalAttach {
            subscription: sub(subscription),
            revision: 4,
            output_offset: 0,
            next_input_seq: None,
            screen: checkpoint_of(b"rewound", geometry()),
            exited: false,
        };
        let mut port = FakePort {
            attach: vec![
                Ok(TerminalAttach {
                    subscription: TerminalSubscription { id: 1, epoch: 1 },
                    revision: 9,
                    output_offset: 3,
                    next_input_seq: None,
                    screen: checkpoint_of(b"new", geometry()),
                    exited: false,
                }),
                Ok(stale(2)),
                Ok(stale(3)),
            ],
            ..FakePort::default()
        };
        let now = Instant::now();
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);

        session.connect_at(&mut port, now);

        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(session.rows()[0], "new");
        assert_eq!(port.detached, [2, 3, 1].map(sub));
        let error = session.error().unwrap();
        assert!(error.contains("stale (revision 4 after 9)"), "{error}");
    }

    #[test]
    fn an_out_of_bounds_checkpoint_is_rejected_before_it_replaces_the_screen() {
        let hostile = |subscription: u64| {
            let TerminalAttachScreen::Checkpoint(mut checkpoint) =
                checkpoint_of(b"hostile", geometry())
            else {
                unreachable!("helper builds a checkpoint")
            };
            checkpoint.schema_version += 1;
            TerminalAttach {
                subscription: sub(subscription),
                revision: 1,
                output_offset: 0,
                next_input_seq: None,
                screen: TerminalAttachScreen::Checkpoint(checkpoint),
                exited: false,
            }
        };
        let mut port = FakePort {
            attach: vec![
                Ok(attach(1, 4, b"kept", false)),
                Ok(hostile(2)),
                Ok(hostile(3)),
            ],
            ..FakePort::default()
        };
        let now = Instant::now();
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect_at(&mut port, now);

        session.connect_at(&mut port, now);

        assert_eq!(session.state(), SessionState::Reconnecting);
        assert_eq!(session.rows()[0], "kept");
        assert_eq!(session.history(), TerminalHistory::Restored);
        let error = session.error().unwrap();
        assert!(error.contains("snapshot was rejected"), "{error}");
        assert!(
            error.contains("unknown checkpoint schema version"),
            "{error}"
        );
    }

    #[test]
    fn begin_selection_snapshots_the_current_terminal_cells() {
        let session = TerminalSession::new(terminal(), geometry());
        let point = TerminalPoint { row: 0, column: 0 };
        let selection = session.begin_selection(point);
        assert_eq!(selection.anchor(), point);
        assert_eq!(selection.focus(), point);
    }
}
