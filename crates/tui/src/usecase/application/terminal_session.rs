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

use std::time::{Duration, Instant};
use usagi_core::domain::id::TerminalRef;
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
    /// Write input bytes exactly once, fenced by `subscription` and `input_seq`.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError>;
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
        }
    }
}

const RETRY_INITIAL: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// How many additional atomic snapshots one `connect` takes when a refused
/// snapshot may converge immediately (a resize that interleaved the capture).
/// A bounded retry keeps a hostile or persistently racing daemon from spinning
/// the redraw tick; the next attempt goes through the ordinary reconnect backoff.
const SNAPSHOT_RETRY_LIMIT: u32 = 1;

/// Feedback shown when the daemon cannot serve a semantic screen checkpoint.
/// The retained raw tail is deliberately not parsed, so this view starts empty
/// and fills from live output only.
const HISTORY_UNAVAILABLE_MESSAGE: &str = "terminal history is unavailable; this daemon cannot restore the screen, showing new output only";

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
    screen: TerminalScreen,
    /// Rendered retained rows cached between output/resize/state changes.
    ///
    /// The presentation loop redraws much more often than terminal output
    /// changes. Keeping this projection here avoids rebuilding and rescanning up
    /// to the full scrollback limit on every 16 ms UI tick.
    display_cache: Vec<String>,
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
    retry_attempt: u32,
    retry_at: Option<Instant>,
}

impl TerminalSession {
    /// Creates a detached session for `terminal`; call [`Self::connect`] to
    /// attach.  The screen starts blank at the requested geometry.
    #[must_use]
    pub fn new(terminal: TerminalRef, geometry: Geometry) -> Self {
        let screen = screen_for(geometry);
        let display_cache = screen.rows_with_scrollback();
        Self {
            terminal,
            geometry,
            synchronized_geometry: None,
            screen,
            display_cache,
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
            retry_attempt: 0,
            retry_at: None,
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
        self.display_cache.clone()
    }

    /// Number of retained rendered rows without cloning the scrollback.
    #[must_use]
    pub const fn display_row_count(&self) -> usize {
        self.display_cache.len()
    }

    /// Clone only the retained rows needed by the current viewport.
    #[must_use]
    pub fn display_row_window(&self, start: usize, end: usize) -> Vec<String> {
        self.display_cache
            .get(start.min(self.display_cache.len())..end.min(self.display_cache.len()))
            .unwrap_or_default()
            .to_vec()
    }

    /// Projects the retained output with a cell-precise visual selection.
    #[must_use]
    pub fn display_rows_with_scrollback_selection(
        &self,
        selection: &TerminalSelection,
    ) -> Vec<String> {
        self.screen.rows_with_scrollback_and_cursor_selection(
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
        for attempt in 0..=SNAPSHOT_RETRY_LIMIT {
            let resize_error = port.resize(&self.terminal, self.geometry).err();
            self.synchronized_geometry = resize_error.is_none().then_some(self.geometry);
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
        if self.geometry != geometry {
            match port.resize(&self.terminal, geometry) {
                Ok(()) => {
                    self.geometry = geometry;
                    self.synchronized_geometry = Some(geometry);
                    self.screen
                        .resize(geometry.rows as usize, geometry.cols as usize);
                    self.refresh_display_cache();
                    self.set_current_error(None);
                }
                Err(error) => {
                    self.synchronized_geometry = None;
                    self.set_current_error(Some(format!(
                        "terminal viewport synchronization failed: {}",
                        error_message(error)
                    )));
                }
            }
        } else if self.synchronized_geometry != Some(geometry) {
            match port.resize(&self.terminal, geometry) {
                Ok(()) => {
                    self.synchronized_geometry = Some(geometry);
                    self.set_current_error(None);
                }
                Err(error) => {
                    self.set_current_error(Some(format!(
                        "terminal viewport synchronization failed: {}",
                        error_message(error)
                    )));
                }
            }
        }
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
        match port.input(&self.terminal, subscription, self.input_seq, bytes) {
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
                self.fail_at(error, now);
                Err(TerminalInputError::Transport(error))
            }
        }
    }

    /// Releases the subscription without stopping the daemon terminal.
    pub fn detach<P: TerminalStreamPort>(&mut self, port: &mut P) {
        if let Some(subscription) = self.subscription.take() {
            port.detach(&self.terminal, subscription);
        }
        self.state = SessionState::Disconnected;
        self.retry_at = None;
        self.retry_attempt = 0;
        self.refresh_display_cache();
        self.set_current_error(Some("terminal detached".to_owned()));
    }

    fn apply_at<P: TerminalStreamPort>(
        &mut self,
        port: &mut P,
        chunks: Vec<TerminalChunk>,
        now: Instant,
    ) {
        let mut changed = false;
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
            changed = true;
        }
        if changed {
            self.refresh_display_cache();
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
        if self.connection_epoch != Some(attach.subscription.epoch) {
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
        self.refresh_display_cache();
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
        self.refresh_display_cache();
        self.set_current_error(Some(refusal.message()));
    }

    fn fail_at(&mut self, error: TerminalError, now: Instant) {
        let state = match error {
            TerminalError::Unavailable | TerminalError::InputEffectUnknown => {
                self.subscription = None;
                self.state = SessionState::Reconnecting;
                self.refresh_display_cache();
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
            TerminalError::ResyncRequired | TerminalError::Stale => SessionState::Disconnected,
        };
        if error != TerminalError::Exited {
            self.subscription = None;
        }
        self.retry_at = None;
        self.retry_attempt = 0;
        self.state = state;
        self.refresh_display_cache();
        self.set_current_error(Some(error_message(error).to_owned()));
    }

    fn refresh_display_cache(&mut self) {
        self.display_cache = match self.state {
            SessionState::Live => self.screen.rows_with_scrollback_and_cursor(),
            SessionState::Reconnecting
            | SessionState::Disconnected
            | SessionState::Orphaned
            | SessionState::Exited => self.screen.rows_with_scrollback(),
        };
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
        input_outcomes: Vec<TerminalInputOutcome>,
        inputs: Vec<(u64, u64, Vec<u8>)>,
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
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            if let Some(error) = self.input {
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
            default_port.input(&terminal(), sub(1), 0, b"x"),
            Err(TerminalError::Unavailable)
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
        session.refresh_display_cache();
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

        session.connect(&mut port);

        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "reply");
        assert_eq!(port.resized, vec![geometry()]);
        assert_eq!(
            session.error(),
            Some(
                "terminal attached, but viewport synchronization failed: daemon unavailable; reconnecting"
            )
        );

        // The outer terminal has not changed size, but the first resize did
        // not reach the daemon. Retry it on the next redraw so an enlarged
        // pane cannot remain stuck at its earlier PTY width.
        session.resize(&mut port, geometry());
        assert_eq!(port.resized, vec![geometry(), geometry()]);
        assert_eq!(session.error(), None);

        let changed = Geometry { cols: 30, rows: 4 };
        port.resize_error = Some(TerminalError::Stale);
        session.resize(&mut port, changed);
        assert!(session.error().unwrap().contains("no longer available"));
        port.resize_error = Some(TerminalError::Unavailable);
        session.resize(&mut port, geometry());
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
        assert_eq!(
            session.send_input(&mut port, b"y"),
            Err(TerminalInputError::NotLive(SessionState::Reconnecting))
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
        assert!(uncertainty.contains("delivery is unknown"));
        assert!(uncertainty.contains("1 bytes were applied"));

        port.polls.push(Err(TerminalError::Orphaned));
        session.poll_at(&mut port, clock.0);
        let feedback = session.error().unwrap();
        assert!(feedback.starts_with("terminal ownership is unknown"));
        assert!(feedback.contains("prior terminal input uncertainty"));
        assert!(feedback.contains("delivery is unknown"));
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
    fn detach_releases_the_subscription_and_reconnect_recovers() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach(4, 0, b"", false)),
                Ok(attach(5, 0, b"back", false)),
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.detach(&mut port);
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(port.detached, [4].map(sub));
        // A second detach without a subscription is a no-op on the port.
        session.detach(&mut port);
        assert_eq!(port.detached, [4].map(sub));
        session.connect(&mut port);
        assert_eq!(session.state(), SessionState::Live);
        assert_eq!(session.rows()[0], "back");
        assert_eq!(session.terminal().terminal_id, session.terminal.terminal_id);
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
            screen: checkpoint_of(b"rewound", geometry()),
            exited: false,
        };
        let mut port = FakePort {
            attach: vec![
                Ok(TerminalAttach {
                    subscription: TerminalSubscription { id: 1, epoch: 1 },
                    revision: 9,
                    output_offset: 3,
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
