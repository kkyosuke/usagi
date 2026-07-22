//! Client-side coordinator for one daemon-owned terminal, driven by polling.
//!
//! The daemon owns the PTY and journals its output; the synchronous IPC client
//! the TUI uses cannot receive pushed stream events, so this coordinator keeps a
//! live view by **polling**: it attaches once (taking the retained replay and an
//! output cursor), then asks for the bytes after that cursor on every redraw
//! tick.  It feeds the bytes into a [`TerminalScreen`], forwards keystrokes once
//! each with a monotonic input sequence, and never spawns a local process — a
//! transport failure only produces safe feedback.
//!
//! The daemon boundary is the injected [`TerminalStreamPort`], so the whole
//! coordinator is exercised with a fake port in unit tests.

use std::time::{Duration, Instant};
use usagi_core::domain::id::TerminalRef;

use super::pane_runtime::Geometry;
use super::terminal_screen::TerminalScreen;
use super::terminal_selection::{TerminalPoint, TerminalSelection};

/// The atomic view returned by attaching to a daemon terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAttach {
    /// The connection-owned subscription used to fence later input.
    pub subscription: u64,
    /// Client-local incarnation of the persistent transport. Reattach on the
    /// same epoch preserves the daemon's per-client input sequence; a new
    /// epoch starts a fresh ledger at zero.
    pub connection_epoch: u64,
    /// The output offset the retained `replay` ends at.
    pub output_offset: u64,
    /// The retained output buffer, rebuilt into the screen on every attach.
    pub replay: Vec<u8>,
    /// Whether the terminal has already exited.
    pub exited: bool,
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
        subscription: u64,
        input_seq: u64,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError>;
    /// Release only this subscription; it must not stop the daemon terminal.
    fn detach(&mut self, terminal: &TerminalRef, subscription: u64);
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
    subscription: Option<u64>,
    cursor: u64,
    input_seq: u64,
    connection_epoch: Option<u64>,
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
        Self {
            terminal,
            geometry,
            synchronized_geometry: None,
            screen: screen_for(geometry),
            subscription: None,
            cursor: 0,
            input_seq: 0,
            connection_epoch: None,
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
    /// reattaching) and rebuilding the screen from its retained replay.  This
    /// ensures an application that redraws on `SIGWINCH` is snapshotted at the
    /// same width as the right pane. A resize failure therefore cannot hide an
    /// otherwise attachable terminal.
    pub fn connect<P: TerminalStreamPort>(&mut self, port: &mut P) {
        self.connect_at(port, Instant::now());
    }

    /// Connects at an injected monotonic instant. This is the deterministic
    /// clock boundary used by reconnect tests.
    pub fn connect_at<P: TerminalStreamPort>(&mut self, port: &mut P, now: Instant) {
        let resize_error = port.resize(&self.terminal, self.geometry).err();
        self.synchronized_geometry = resize_error.is_none().then_some(self.geometry);
        match port.attach(&self.terminal, self.geometry) {
            Ok(attach) => {
                if let Some(previous) = self.subscription
                    && previous != attach.subscription
                {
                    port.detach(&self.terminal, previous);
                }
                self.replace(&attach);
                if let Some(error) = resize_error {
                    self.set_current_error(Some(format!(
                        "terminal attached, but viewport synchronization failed: {}",
                        error_message(error)
                    )));
                }
            }
            Err(error) => self.fail_at(error, now),
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

    fn replace(&mut self, attach: &TerminalAttach) {
        self.screen = screen_for(self.geometry);
        self.screen.advance(&attach.replay);
        self.subscription = Some(attach.subscription);
        self.cursor = attach.output_offset;
        if self.connection_epoch != Some(attach.connection_epoch) {
            self.input_seq = 0;
        }
        self.connection_epoch = Some(attach.connection_epoch);
        self.retry_attempt = 0;
        self.retry_at = None;
        self.state = if attach.exited {
            SessionState::Exited
        } else {
            SessionState::Live
        };
        self.set_current_error(
            attach
                .exited
                .then(|| error_message(TerminalError::Exited).to_owned()),
        );
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
            TerminalError::ResyncRequired | TerminalError::Stale => SessionState::Disconnected,
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
        detached: Vec<u64>,
        resized: Vec<Geometry>,
        resize_error: Option<TerminalError>,
        resize_count_at_attach: Vec<usize>,
        attached_terminals: Vec<TerminalRef>,
    }
    impl TerminalStreamPort for FakePort {
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
            subscription: u64,
            input_seq: u64,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            if let Some(error) = self.input {
                return Err(error);
            }
            self.inputs.push((subscription, input_seq, bytes.to_vec()));
            if self.input_outcomes.is_empty() {
                Ok(TerminalInputOutcome::Written)
            } else {
                Ok(self.input_outcomes.remove(0))
            }
        }
        fn detach(&mut self, _: &TerminalRef, subscription: u64) {
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
            _: u64,
            _: u64,
            _: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            Err(TerminalError::Unavailable)
        }

        fn detach(&mut self, _: &TerminalRef, _: u64) {}
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
            subscription,
            connection_epoch,
            output_offset: offset,
            replay: replay.to_vec(),
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
            default_port.input(&terminal(), 1, 0, b"x"),
            Err(TerminalError::Unavailable)
        );
        default_port.detach(&terminal(), 1);
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
        assert_eq!(port.detached, vec![4]);
        // A second detach without a subscription is a no-op on the port.
        session.detach(&mut port);
        assert_eq!(port.detached, vec![4]);
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
                _: u64,
                _: u64,
                _: &[u8],
            ) -> Result<TerminalInputOutcome, TerminalError> {
                self.available().map(|()| TerminalInputOutcome::Written)
            }

            fn detach(&mut self, _: &TerminalRef, _: u64) {
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

    #[test]
    fn begin_selection_snapshots_the_current_terminal_cells() {
        let session = TerminalSession::new(terminal(), geometry());
        let point = TerminalPoint { row: 0, column: 0 };
        let selection = session.begin_selection(point);
        assert_eq!(selection.anchor(), point);
        assert_eq!(selection.focus(), point);
    }
}
