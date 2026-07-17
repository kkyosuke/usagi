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

use usagi_core::domain::id::TerminalRef;

use super::pane_runtime::Geometry;
use super::terminal_screen::TerminalScreen;
use super::terminal_selection::{TerminalPoint, TerminalSelection};

/// The atomic view returned by attaching to a daemon terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAttach {
    /// The connection-owned subscription used to fence later input.
    pub subscription: u64,
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
    ) -> Result<(), TerminalError>;
    /// Release only this subscription; it must not stop the daemon terminal.
    fn detach(&mut self, terminal: &TerminalRef, subscription: u64);
}

/// The coordinator's connection status, rendered without leaking transport
/// details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Attached and streaming.
    Live,
    /// Not attached; a reconnect is required to resume.
    Disconnected,
    /// Ownership is unknown; input is disabled.
    Orphaned,
    /// The terminal process has exited; the final screen is retained.
    Exited,
}

/// A polling view of one daemon-owned terminal and its rendered screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSession {
    terminal: TerminalRef,
    geometry: Geometry,
    screen: TerminalScreen,
    subscription: Option<u64>,
    cursor: u64,
    input_seq: u64,
    state: SessionState,
    error: Option<String>,
}

impl TerminalSession {
    /// Creates a detached session for `terminal`; call [`Self::connect`] to
    /// attach.  The screen starts blank at the requested geometry.
    #[must_use]
    pub fn new(terminal: TerminalRef, geometry: Geometry) -> Self {
        Self {
            terminal,
            geometry,
            screen: screen_for(geometry),
            subscription: None,
            cursor: 0,
            input_seq: 0,
            state: SessionState::Disconnected,
            error: None,
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
            SessionState::Disconnected | SessionState::Orphaned | SessionState::Exited => {
                self.screen.rows()
            }
        }
    }

    /// The retained terminal history projected into an active terminal pane.
    #[must_use]
    pub fn display_rows_with_scrollback(&self) -> Vec<String> {
        match self.state {
            SessionState::Live => self.screen.rows_with_scrollback_and_cursor(),
            SessionState::Disconnected | SessionState::Orphaned | SessionState::Exited => {
                self.screen.rows_with_scrollback()
            }
        }
    }

    /// Projects the retained output with a cell-precise visual selection.
    #[must_use]
    #[coverage(off)]
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
    #[coverage(off)]
    pub fn cells(&self) -> Vec<String> {
        self.screen.cells_with_scrollback()
    }

    /// Starts a stable selection from the current visible terminal cells.
    /// Later stream output, reconnects, and screen replacement do not mutate
    /// the returned selection's copy text.
    #[must_use]
    #[coverage(off)]
    pub fn begin_selection(&self, anchor: TerminalPoint) -> TerminalSelection {
        TerminalSelection::begin(self.cells(), anchor)
    }

    /// Attaches (or reattaches) and rebuilds the screen from the retained
    /// replay before attempting viewport synchronization. A resize failure
    /// therefore cannot hide an otherwise attachable terminal.
    pub fn connect<P: TerminalStreamPort>(&mut self, port: &mut P) {
        match port.attach(&self.terminal, self.geometry) {
            Ok(attach) => {
                self.replace(&attach);
                if let Err(error) = port.resize(&self.terminal, self.geometry) {
                    self.error = Some(format!(
                        "terminal attached, but viewport synchronization failed: {}",
                        error_message(error)
                    ));
                }
            }
            Err(error) => self.fail(error),
        }
    }

    /// Fetches and applies any output produced since the last cursor.  A cursor
    /// gap (retained output already trimmed) triggers a full reattach; the
    /// process having exited transitions to [`SessionState::Exited`].
    pub fn poll<P: TerminalStreamPort>(&mut self, port: &mut P) {
        if self.state != SessionState::Live {
            return;
        }
        match port.poll(&self.terminal, self.cursor) {
            Ok(chunks) => self.apply(port, chunks),
            Err(TerminalError::ResyncRequired) => self.connect(port),
            Err(error) => self.fail(error),
        }
    }

    /// Rebuilds the local screen after the visible pane changes size.
    pub fn resize<P: TerminalStreamPort>(&mut self, port: &mut P, geometry: Geometry) {
        if self.geometry != geometry {
            self.geometry = geometry;
            self.connect(port);
        }
    }

    /// Sends input bytes to the terminal exactly once.  Input is ignored unless
    /// the session is live and attached.
    pub fn send_input<P: TerminalStreamPort>(&mut self, port: &mut P, bytes: &[u8]) {
        let (SessionState::Live, Some(subscription)) = (self.state, self.subscription) else {
            return;
        };
        match port.input(&self.terminal, subscription, self.input_seq, bytes) {
            Ok(()) => self.input_seq += 1,
            Err(error) => self.fail(error),
        }
    }

    /// Releases the subscription without stopping the daemon terminal.
    pub fn detach<P: TerminalStreamPort>(&mut self, port: &mut P) {
        if let Some(subscription) = self.subscription.take() {
            port.detach(&self.terminal, subscription);
        }
        self.state = SessionState::Disconnected;
    }

    fn apply<P: TerminalStreamPort>(&mut self, port: &mut P, chunks: Vec<TerminalChunk>) {
        for chunk in chunks {
            let contiguous = chunk.start_offset == self.cursor
                && chunk.end_offset >= chunk.start_offset
                && chunk.end_offset - chunk.start_offset == chunk.data.len() as u64;
            if !contiguous {
                // Lost or overlapping output: rebuild from an atomic snapshot.
                self.connect(port);
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
        self.error = None;
        self.state = if attach.exited {
            SessionState::Exited
        } else {
            SessionState::Live
        };
    }

    fn fail(&mut self, error: TerminalError) {
        self.state = match error {
            TerminalError::Orphaned => SessionState::Orphaned,
            TerminalError::Exited => SessionState::Exited,
            TerminalError::ResyncRequired | TerminalError::Unavailable | TerminalError::Stale => {
                SessionState::Disconnected
            }
        };
        self.error = Some(error_message(error).to_owned());
    }
}

fn error_message(error: TerminalError) -> &'static str {
    match error {
        TerminalError::ResyncRequired => "terminal output is resynchronizing",
        TerminalError::Unavailable => "daemon disconnected; reconnect to continue",
        TerminalError::Stale => "terminal is no longer available",
        TerminalError::Orphaned => "terminal ownership is unknown; input is disabled",
        TerminalError::Exited => "terminal has exited",
    }
}

fn screen_for(geometry: Geometry) -> TerminalScreen {
    TerminalScreen::new(geometry.rows as usize, geometry.cols as usize)
}

#[cfg(test)]
mod tests {
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
        inputs: Vec<(u64, u64, Vec<u8>)>,
        detached: Vec<u64>,
        resized: Vec<Geometry>,
        resize_error: Option<TerminalError>,
    }
    impl TerminalStreamPort for FakePort {
        fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
            self.resized.push(geometry);
            self.resize_error.take().map_or(Ok(()), Err)
        }

        fn attach(
            &mut self,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
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
        ) -> Result<(), TerminalError> {
            if let Some(error) = self.input {
                return Err(error);
            }
            self.inputs.push((subscription, input_seq, bytes.to_vec()));
            Ok(())
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
        ) -> Result<(), TerminalError> {
            Err(TerminalError::Unavailable)
        }

        fn detach(&mut self, _: &TerminalRef, _: u64) {}
    }

    fn attach(subscription: u64, offset: u64, replay: &[u8], exited: bool) -> TerminalAttach {
        TerminalAttach {
            subscription,
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
    fn resizing_rebuilds_the_screen_from_a_same_geometry_daemon_replay() {
        let mut port = FakePort {
            attach: vec![
                Ok(attach(1, 3, b"old", false)),
                Ok(attach(2, 5, b"wide", false)),
            ],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        let resized = Geometry { cols: 40, rows: 8 };
        session.resize(&mut port, resized);

        assert_eq!(port.resized, vec![geometry(), resized]);
        assert_eq!(session.rows()[0], "wide");
        assert_eq!(session.state(), SessionState::Live);
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
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(
            session.error(),
            Some("daemon disconnected; reconnect to continue")
        );
        // Input is dropped while not live, and no bytes reach the port.
        session.send_input(&mut port, b"ls\r");
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
                "terminal attached, but viewport synchronization failed: daemon disconnected; reconnect to continue"
            )
        );
    }

    #[test]
    fn input_is_sent_once_with_a_monotonic_sequence() {
        let mut port = FakePort {
            attach: vec![Ok(attach(9, 0, b"", false))],
            ..FakePort::default()
        };
        let mut session = TerminalSession::new(terminal(), geometry());
        session.connect(&mut port);
        session.send_input(&mut port, b"l");
        session.send_input(&mut port, b"s\r");
        assert_eq!(
            port.inputs,
            vec![(9, 0, b"l".to_vec()), (9, 1, b"s\r".to_vec())]
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
        session.send_input(&mut port, b"x");
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(session.error(), Some("terminal is no longer available"));
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
        session.fail(TerminalError::ResyncRequired);
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
}
