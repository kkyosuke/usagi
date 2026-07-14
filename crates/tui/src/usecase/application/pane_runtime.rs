//! Daemon-owned terminal streams as a pane-reducer adapter.
//!
//! This module owns only TUI-local screen/cursor state. A [`TerminalPort`]
//! owns daemon communication, so detaching never kills a PTY and a client never
//! creates a local fallback terminal.

use usagi_core::domain::id::TerminalRef;

use super::pane::{self, PaneEffect, PaneEvent, PaneState, PaneTab, TabSelection};

/// The terminal dimensions requested by this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
}

/// A daemon inventory item carrying the complete fenced reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInventory {
    pub terminal: TerminalRef,
    pub live: bool,
}

/// The atomic view returned by attach or resync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub terminal: TerminalRef,
    pub output_offset: u64,
    pub geometry: Geometry,
    pub replay: Vec<u8>,
    pub exited: bool,
}

/// A terminal stream observation projected by the daemon adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStreamEvent {
    Output {
        terminal: TerminalRef,
        start_offset: u64,
        end_offset: u64,
        data: Vec<u8>,
    },
    Exited(TerminalRef),
    ResyncRequired(TerminalRef),
    Disconnected,
    Orphaned(TerminalRef),
}

/// Safe client-visible terminal transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalError {
    Unavailable,
    StaleTarget,
    Orphaned,
    Failed(String),
}

/// The daemon boundary used by the pane runtime.
///
/// `resume_from` is the last contiguous output cursor. Implementations return
/// an atomic snapshot when retained replay cannot prove continuity.
pub trait TerminalPort {
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn inventory(&mut self) -> Result<Vec<TerminalInventory>, TerminalError>;
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn attach(
        &mut self,
        terminal: &TerminalRef,
        resume_from: Option<u64>,
    ) -> Result<TerminalSnapshot, TerminalError>;
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn resync(&mut self, terminal: &TerminalRef) -> Result<TerminalSnapshot, TerminalError>;
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn input(&mut self, terminal: &TerminalRef, bytes: &[u8]) -> Result<(), TerminalError>;
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError>;
    /// Detach only this client subscription. It must not kill the terminal.
    fn detach(&mut self, terminal: &TerminalRef);
}

/// Connection status rendered without leaking transport details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
    Orphaned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamState {
    terminal: TerminalRef,
    output_offset: u64,
    geometry: Geometry,
    output: Vec<u8>,
}

/// Stateful adapter joining local pane tabs to daemon inventory and streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRuntime {
    pane: PaneState,
    streams: Vec<StreamState>,
    connection: ConnectionState,
    error: Option<String>,
}

impl PaneRuntime {
    #[must_use]
    pub fn new(pane: PaneState) -> Self {
        Self {
            pane,
            streams: Vec::new(),
            connection: ConnectionState::Connected,
            error: None,
        }
    }
    #[must_use]
    pub fn pane(&self) -> &PaneState {
        &self.pane
    }
    #[must_use]
    pub const fn connection(&self) -> ConnectionState {
        self.connection
    }
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    #[must_use]
    pub fn output(&self, terminal: &TerminalRef) -> Option<&[u8]> {
        self.stream(terminal).map(|stream| stream.output.as_slice())
    }

    /// Reduces a local pane event and performs only its resulting attachments.
    pub fn dispatch<P: TerminalPort>(&mut self, port: &mut P, event: PaneEvent) {
        for effect in pane::reduce(&mut self.pane, event) {
            self.run_effect(port, effect);
        }
    }

    /// Validates saved `TerminalRef`s against inventory and reattaches only the
    /// selected tab. Missing/exited entries are removed, never guessed by name.
    pub fn reconnect<P: TerminalPort>(&mut self, port: &mut P) {
        let inventory = match port.inventory() {
            Ok(inventory) => inventory,
            Err(error) => return self.transport_error(error),
        };
        self.connection = ConnectionState::Connected;
        let saved: Vec<_> = self
            .pane
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                PaneTab::Live(live) => Some(live.terminal.clone()),
                PaneTab::Pending(_) | PaneTab::Ready(_) => None,
            })
            .collect();
        for terminal in saved {
            if !inventory
                .iter()
                .any(|item| item.live && item.terminal.fences(&terminal))
            {
                self.dispatch(port, PaneEvent::Exited(terminal));
            }
        }
        if let Some(terminal) = selected_live(&self.pane) {
            self.attach(port, &terminal);
        }
    }

    /// Applies one daemon stream event. A cursor gap replaces state via resync.
    pub fn stream_event<P: TerminalPort>(&mut self, port: &mut P, event: TerminalStreamEvent) {
        match event {
            TerminalStreamEvent::Output {
                terminal,
                start_offset,
                end_offset,
                data,
            } => {
                let valid = self.stream(&terminal).is_some_and(|stream| {
                    start_offset == stream.output_offset
                        && end_offset >= start_offset
                        && end_offset - start_offset == data.len() as u64
                });
                if !valid {
                    if self.stream(&terminal).is_some() {
                        self.resync(port, &terminal);
                    }
                    return;
                }
                if let Some(stream) = self.stream_mut(&terminal) {
                    stream.output.extend_from_slice(&data);
                    stream.output_offset = end_offset;
                }
            }
            TerminalStreamEvent::Exited(terminal) => {
                self.streams
                    .retain(|stream| !stream.terminal.fences(&terminal));
                self.dispatch(port, PaneEvent::Exited(terminal));
            }
            TerminalStreamEvent::ResyncRequired(terminal) => self.resync(port, &terminal),
            TerminalStreamEvent::Disconnected => self.transport_error(TerminalError::Unavailable),
            TerminalStreamEvent::Orphaned(terminal) => {
                if self.has_live(&terminal) {
                    self.transport_error(TerminalError::Orphaned);
                }
            }
        }
    }

    /// Forwards input exactly once to the selected attached terminal.
    pub fn input<P: TerminalPort>(&mut self, port: &mut P, bytes: &[u8]) {
        let Some(terminal) = selected_live(&self.pane) else {
            return;
        };
        if self.connection != ConnectionState::Connected || self.stream(&terminal).is_none() {
            return;
        }
        if let Err(error) = port.input(&terminal, bytes) {
            self.transport_error(error);
        }
    }

    /// Sends a resize only when this terminal's geometry changes.
    pub fn resize<P: TerminalPort>(&mut self, port: &mut P, geometry: Geometry) {
        let Some(terminal) = selected_live(&self.pane) else {
            return;
        };
        let Some(stream) = self.stream(&terminal) else {
            return;
        };
        if stream.geometry == geometry || self.connection != ConnectionState::Connected {
            return;
        }
        if let Err(error) = port.resize(&terminal, geometry) {
            self.transport_error(error);
            return;
        }
        if let Some(stream) = self.stream_mut(&terminal) {
            stream.geometry = geometry;
        }
    }

    /// Releases subscriptions without changing daemon terminal ownership.
    pub fn detach<P: TerminalPort>(&mut self, port: &mut P) {
        for stream in &self.streams {
            port.detach(&stream.terminal);
        }
        self.streams.clear();
        self.connection = ConnectionState::Disconnected;
    }

    fn run_effect<P: TerminalPort>(&mut self, port: &mut P, effect: PaneEffect) {
        if let PaneEffect::Attach(terminal) = effect {
            self.attach(port, &terminal);
        }
    }
    fn attach<P: TerminalPort>(&mut self, port: &mut P, terminal: &TerminalRef) {
        if !self.has_live(terminal) {
            return;
        }
        let cursor = self.stream(terminal).map(|stream| stream.output_offset);
        match port.attach(terminal, cursor) {
            Ok(snapshot) if snapshot.terminal.fences(terminal) => self.replace(snapshot),
            Ok(_) => self.transport_error(TerminalError::StaleTarget),
            Err(error) => self.transport_error(error),
        }
    }
    fn resync<P: TerminalPort>(&mut self, port: &mut P, terminal: &TerminalRef) {
        match port.resync(terminal) {
            Ok(snapshot) if snapshot.terminal.fences(terminal) => self.replace(snapshot),
            Ok(_) => self.transport_error(TerminalError::StaleTarget),
            Err(error) => self.transport_error(error),
        }
    }
    fn replace(&mut self, snapshot: TerminalSnapshot) {
        if snapshot.exited {
            let _ = pane::reduce(&mut self.pane, PaneEvent::Exited(snapshot.terminal));
            return;
        }
        self.connection = ConnectionState::Connected;
        self.error = None;
        self.streams
            .retain(|stream| !stream.terminal.fences(&snapshot.terminal));
        self.streams.push(StreamState {
            terminal: snapshot.terminal,
            output_offset: snapshot.output_offset,
            geometry: snapshot.geometry,
            output: snapshot.replay,
        });
    }
    fn transport_error(&mut self, error: TerminalError) {
        self.connection = if error == TerminalError::Orphaned {
            ConnectionState::Orphaned
        } else {
            ConnectionState::Disconnected
        };
        self.error = Some(match error {
            TerminalError::Unavailable => "daemon disconnected; reconnect to continue".to_owned(),
            TerminalError::StaleTarget => "terminal is no longer available".to_owned(),
            TerminalError::Orphaned => {
                "terminal ownership is unknown; input is disabled".to_owned()
            }
            TerminalError::Failed(message) => message,
        });
    }
    fn has_live(&self, terminal: &TerminalRef) -> bool {
        self.pane
            .tabs()
            .iter()
            .any(|tab| matches!(tab, PaneTab::Live(live) if live.terminal.fences(terminal)))
    }
    fn stream(&self, terminal: &TerminalRef) -> Option<&StreamState> {
        self.streams
            .iter()
            .find(|stream| stream.terminal.fences(terminal))
    }
    fn stream_mut(&mut self, terminal: &TerminalRef) -> Option<&mut StreamState> {
        self.streams
            .iter_mut()
            .find(|stream| stream.terminal.fences(terminal))
    }
}

fn selected_live(pane: &PaneState) -> Option<TerminalRef> {
    match pane.selected() {
        super::pane::PaneSelection::Tab(TabSelection::Live(terminal)) => Some(terminal.clone()),
        super::pane::PaneSelection::Target(_)
        | super::pane::PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Ready(_)) => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        controller::Target,
        pane::{LivePane, PaneKind, PaneSelection},
    };
    use super::*;
    use usagi_core::domain::id::{
        DaemonGeneration, OperationId, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };
    #[derive(Default)]
    struct FakeDaemon {
        inventory: Vec<TerminalInventory>,
        snapshots: Vec<TerminalSnapshot>,
        error: Option<TerminalError>,
        inputs: Vec<Vec<u8>>,
        resizes: Vec<Geometry>,
        detached: usize,
    }
    impl TerminalPort for FakeDaemon {
        fn inventory(&mut self) -> Result<Vec<TerminalInventory>, TerminalError> {
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            Ok(self.inventory.clone())
        }
        fn attach(
            &mut self,
            _: &TerminalRef,
            _: Option<u64>,
        ) -> Result<TerminalSnapshot, TerminalError> {
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            Ok(self.snapshots.remove(0))
        }
        fn resync(&mut self, _: &TerminalRef) -> Result<TerminalSnapshot, TerminalError> {
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            Ok(self.snapshots.remove(0))
        }
        fn input(&mut self, _: &TerminalRef, bytes: &[u8]) -> Result<(), TerminalError> {
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            self.inputs.push(bytes.to_vec());
            Ok(())
        }
        fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            self.resizes.push(geometry);
            Ok(())
        }
        fn detach(&mut self, _: &TerminalRef) {
            self.detached += 1;
        }
    }
    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }
    fn pane(terminal: TerminalRef) -> PaneState {
        PaneState::with_live(
            PaneSelection::Tab(TabSelection::Live(terminal.clone())),
            vec![LivePane {
                terminal,
                kind: PaneKind::Terminal,
            }],
        )
    }
    fn snapshot(terminal: TerminalRef, offset: u64, output: &[u8]) -> TerminalSnapshot {
        TerminalSnapshot {
            terminal,
            output_offset: offset,
            geometry: Geometry { cols: 80, rows: 24 },
            replay: output.to_vec(),
            exited: false,
        }
    }
    #[test]
    fn reconnect_validates_saved_ref_then_attaches_and_resync_replaces_a_gap() {
        let terminal = terminal();
        let mut daemon = FakeDaemon {
            inventory: vec![TerminalInventory {
                terminal: terminal.clone(),
                live: true,
            }],
            snapshots: vec![
                snapshot(terminal.clone(), 3, b"one"),
                snapshot(terminal.clone(), 6, b"resync"),
            ],
            ..FakeDaemon::default()
        };
        let mut runtime = PaneRuntime::new(pane(terminal.clone()));
        runtime.reconnect(&mut daemon);
        assert_eq!(runtime.output(&terminal), Some(&b"one"[..]));
        runtime.stream_event(
            &mut daemon,
            TerminalStreamEvent::Output {
                terminal: terminal.clone(),
                start_offset: 4,
                end_offset: 5,
                data: b"x".to_vec(),
            },
        );
        assert_eq!(runtime.output(&terminal), Some(&b"resync"[..]));
    }
    #[test]
    fn input_resize_and_detach_are_safe_and_resize_is_deduplicated() {
        let terminal = terminal();
        let mut daemon = FakeDaemon {
            inventory: vec![TerminalInventory {
                terminal: terminal.clone(),
                live: true,
            }],
            snapshots: vec![snapshot(terminal.clone(), 0, b"")],
            ..FakeDaemon::default()
        };
        let mut runtime = PaneRuntime::new(pane(terminal.clone()));
        runtime.reconnect(&mut daemon);
        runtime.input(&mut daemon, b"x");
        runtime.resize(
            &mut daemon,
            Geometry {
                cols: 100,
                rows: 30,
            },
        );
        runtime.resize(
            &mut daemon,
            Geometry {
                cols: 100,
                rows: 30,
            },
        );
        runtime.detach(&mut daemon);
        runtime.input(&mut daemon, b"y");
        assert_eq!(daemon.inputs, vec![b"x".to_vec()]);
        assert_eq!(
            daemon.resizes,
            vec![Geometry {
                cols: 100,
                rows: 30
            }]
        );
        assert_eq!(daemon.detached, 1);
    }
    #[test]
    fn stale_inventory_exits_the_saved_tab_and_orphan_disables_input() {
        let terminal = terminal();
        let mut runtime = PaneRuntime::new(pane(terminal.clone()));
        let mut daemon = FakeDaemon::default();
        runtime.reconnect(&mut daemon);
        assert!(runtime.pane().tabs().is_empty());
        let mut runtime = PaneRuntime::new(pane(terminal.clone()));
        runtime.stream_event(&mut daemon, TerminalStreamEvent::Orphaned(terminal));
        assert_eq!(runtime.connection(), ConnectionState::Orphaned);
        assert_eq!(
            runtime.error(),
            Some("terminal ownership is unknown; input is disabled")
        );
    }

    #[test]
    fn safe_failures_and_all_stream_lifecycle_paths_never_fall_back_locally() {
        let terminal = terminal();
        let mut runtime = PaneRuntime::new(pane(terminal.clone()));
        let mut daemon = FakeDaemon {
            inventory: vec![TerminalInventory {
                terminal: terminal.clone(),
                live: true,
            }],
            snapshots: vec![snapshot(terminal.clone(), 0, b"attached")],
            ..FakeDaemon::default()
        };
        runtime.reconnect(&mut daemon);
        runtime.stream_event(
            &mut daemon,
            TerminalStreamEvent::Output {
                terminal: terminal.clone(),
                start_offset: 0,
                end_offset: 1,
                data: b"x".to_vec(),
            },
        );
        assert_eq!(runtime.output(&terminal), Some(&b"attachedx"[..]));

        daemon
            .snapshots
            .push(snapshot(terminal.clone(), 8, b"resynced"));
        runtime.stream_event(
            &mut daemon,
            TerminalStreamEvent::ResyncRequired(terminal.clone()),
        );
        assert_eq!(runtime.output(&terminal), Some(&b"resynced"[..]));

        runtime.stream_event(&mut daemon, TerminalStreamEvent::Disconnected);
        assert_eq!(runtime.connection(), ConnectionState::Disconnected);
        daemon.error = Some(TerminalError::Failed("safe failure".to_owned()));
        runtime.reconnect(&mut daemon);
        assert_eq!(runtime.error(), Some("safe failure"));
        runtime.transport_error(TerminalError::StaleTarget);
        assert_eq!(runtime.error(), Some("terminal is no longer available"));

        let mut pending = PaneRuntime::new(PaneState::new(PaneSelection::Target(Target::Session(
            SessionId::new(),
        ))));
        pending.dispatch(
            &mut daemon,
            PaneEvent::Request {
                operation: OperationId::new(),
                target: Target::Session(SessionId::new()),
                kind: PaneKind::Agent,
            },
        );
        daemon.error = None;
        pending.reconnect(&mut daemon);

        let mut failing = PaneRuntime::new(pane(terminal.clone()));
        daemon.error = Some(TerminalError::Failed("attach failed".to_owned()));
        failing.attach(&mut daemon, &terminal);
        daemon.error = Some(TerminalError::Failed("resync failed".to_owned()));
        failing.resync(&mut daemon, &terminal);
        daemon.error = None;
        daemon.snapshots.push(snapshot(terminal.clone(), 0, b""));
        failing.reconnect(&mut daemon);
        daemon.error = Some(TerminalError::Failed("input failed".to_owned()));
        failing.input(&mut daemon, b"x");
        daemon.error = None;
        daemon.snapshots.push(snapshot(terminal.clone(), 0, b""));
        failing.reconnect(&mut daemon);
        daemon.error = Some(TerminalError::Failed("resize failed".to_owned()));
        failing.resize(&mut daemon, Geometry { cols: 81, rows: 24 });
        daemon.error = None;
        let mut wrong = terminal.clone();
        wrong.terminal_id = TerminalId::new();
        daemon.snapshots.push(snapshot(wrong.clone(), 0, b"wrong"));
        failing.run_effect(&mut daemon, PaneEffect::Attach(terminal.clone()));
        daemon.snapshots.push(snapshot(wrong, 0, b"wrong"));
        failing.resync(&mut daemon, &terminal);

        let no_selection = PaneRuntime::new(PaneState::new(PaneSelection::Target(
            Target::Session(SessionId::new()),
        )));
        assert!(selected_live(no_selection.pane()).is_none());
        let mut no_selection = no_selection;
        no_selection.attach(&mut daemon, &terminal);
        no_selection.input(&mut daemon, b"ignored");
        no_selection.resize(&mut daemon, Geometry { cols: 1, rows: 1 });
        let mut unattached = PaneRuntime::new(pane(terminal.clone()));
        unattached.resize(&mut daemon, Geometry { cols: 1, rows: 1 });

        let mut exited = PaneRuntime::new(pane(terminal.clone()));
        exited.replace(TerminalSnapshot {
            exited: true,
            ..snapshot(terminal.clone(), 0, b"")
        });
        assert!(exited.pane().tabs().is_empty());
        runtime.stream_event(&mut daemon, TerminalStreamEvent::Exited(terminal));
        assert!(runtime.pane().tabs().is_empty());
    }
}
