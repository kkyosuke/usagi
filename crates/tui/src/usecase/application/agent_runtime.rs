//! Controller effect runner for daemon-owned Agent panes.
//!
//! The v2 presentation host keeps this object for the lifetime of a workspace.
//! It is the only place where a `LaunchAgent` effect is joined to the
//! session-scoped pane runtime; neither the controller nor a renderer opens a
//! local process.

use std::collections::HashMap;

use usagi_core::domain::id::SessionId;

use super::{
    agent_launch::{AgentLaunchAdapter, AgentLaunchEvent, AgentLaunchPort},
    controller::{Effect, Target},
    pane::{PaneEvent, PaneSelection, PaneState, TabSelection},
    pane_runtime::{Geometry, PaneRuntime, TerminalPort, TerminalStreamEvent},
};

/// Stateful v2 runtime bridge for Agent effects and terminal observations.
pub struct AgentRuntimeHost<P> {
    adapter: AgentLaunchAdapter<P>,
    panes: HashMap<SessionId, PaneRuntime>,
}

impl<P> AgentRuntimeHost<P> {
    #[must_use]
    pub fn new(port: P) -> Self {
        Self {
            adapter: AgentLaunchAdapter::new(port),
            panes: HashMap::new(),
        }
    }

    #[must_use]
    pub fn pane(&self, session: SessionId) -> Option<&PaneRuntime> {
        self.panes.get(&session)
    }

    #[must_use]
    pub fn port(&self) -> &P {
        self.adapter.port()
    }
}

impl<P: AgentLaunchPort + TerminalPort> AgentRuntimeHost<P> {
    /// Run a controller effect. Only Agent launch effects enter this host;
    /// other controller effects remain owned by their existing runners.
    pub fn dispatch(&mut self, effect: Effect) {
        let Effect::LaunchAgent { session, .. } = &effect else {
            return;
        };
        let runtime = self.panes.entry(*session).or_insert_with(|| {
            PaneRuntime::new(PaneState::new(PaneSelection::Target(Target::Session(
                *session,
            ))))
        });
        self.adapter.dispatch(runtime, effect);
    }

    /// Apply a daemon completion to the matching pending session pane. The
    /// adapter fences the operation and terminal scope before it can attach.
    pub fn apply(&mut self, event: AgentLaunchEvent) {
        for runtime in self.panes.values_mut() {
            self.adapter.apply(runtime, event.clone());
        }
    }

    /// Select a completed tab when the v2 UI focuses its owning session. This
    /// is deliberately explicit so a background completion cannot steal focus.
    pub fn select_live(
        &mut self,
        session: SessionId,
        terminal: &usagi_core::domain::id::TerminalRef,
    ) {
        if let Some(runtime) = self.panes.get_mut(&session) {
            runtime.dispatch(
                self.adapter.port_mut(),
                PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal.clone()))),
            );
        }
    }

    /// Relay one daemon terminal observation for its owning session only.
    pub fn stream(&mut self, session: SessionId, event: TerminalStreamEvent) {
        if let Some(runtime) = self.panes.get_mut(&session) {
            runtime.stream_event(self.adapter.port_mut(), event);
        }
    }

    /// Relay selected-pane input without a local fallback.
    pub fn input(&mut self, session: SessionId, bytes: &[u8]) {
        if let Some(runtime) = self.panes.get_mut(&session) {
            runtime.input(self.adapter.port_mut(), bytes);
        }
    }

    /// Relay a geometry change to the selected attached pane.
    pub fn resize(&mut self, session: SessionId, geometry: Geometry) {
        if let Some(runtime) = self.panes.get_mut(&session) {
            runtime.resize(self.adapter.port_mut(), geometry);
        }
    }

    /// Reconcile the selected pane after a daemon reconnect.
    pub fn reconnect(&mut self, session: SessionId) {
        if let Some(runtime) = self.panes.get_mut(&session) {
            runtime.reconnect(self.adapter.port_mut());
        }
    }

    /// Detach client subscriptions while leaving daemon PTY ownership intact.
    pub fn detach(&mut self) {
        for runtime in self.panes.values_mut() {
            runtime.detach(self.adapter.port_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::pane::PaneTab;
    use super::super::pane_runtime::{TerminalError, TerminalInventory, TerminalSnapshot};
    use super::*;
    use usagi_core::domain::id::{
        DaemonGeneration, OperationId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };
    use usagi_core::usecase::client::AgentLaunchIntent;

    #[derive(Default)]
    struct Fake {
        launches: Vec<(OperationId, AgentLaunchIntent)>,
        attached: usize,
        input: Vec<Vec<u8>>,
        resized: Vec<Geometry>,
        terminal: Option<TerminalRef>,
    }

    impl AgentLaunchPort for Fake {
        fn launch(
            &mut self,
            operation: OperationId,
            intent: AgentLaunchIntent,
        ) -> Result<AgentLaunchEvent, String> {
            self.launches.push((operation, intent));
            Ok(AgentLaunchEvent::Accepted { operation })
        }
    }
    impl TerminalPort for Fake {
        fn inventory(&mut self) -> Result<Vec<TerminalInventory>, TerminalError> {
            Ok(self
                .terminal
                .clone()
                .into_iter()
                .map(|terminal| TerminalInventory {
                    terminal,
                    live: true,
                })
                .collect())
        }
        fn attach(
            &mut self,
            terminal: &TerminalRef,
            _: Option<u64>,
        ) -> Result<TerminalSnapshot, TerminalError> {
            self.attached += 1;
            Ok(TerminalSnapshot {
                terminal: terminal.clone(),
                output_offset: 0,
                geometry: Geometry { cols: 80, rows: 24 },
                replay: Vec::new(),
                exited: false,
            })
        }
        fn resync(&mut self, terminal: &TerminalRef) -> Result<TerminalSnapshot, TerminalError> {
            self.attach(terminal, None)
        }
        fn input(&mut self, _: &TerminalRef, bytes: &[u8]) -> Result<(), TerminalError> {
            self.input.push(bytes.to_vec());
            Ok(())
        }
        fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
            self.resized.push(geometry);
            Ok(())
        }
        fn detach(&mut self, _: &TerminalRef) {}
    }

    fn ids() -> (WorkspaceId, SessionId, TerminalRef) {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        };
        (workspace, session, terminal)
    }

    #[test]
    fn effect_reaches_daemon_then_fenced_completion_attaches_and_relays_io() {
        let (workspace, session, terminal) = ids();
        let operation = OperationId::new();
        let mut host = AgentRuntimeHost::new(Fake::default());
        host.dispatch(Effect::LaunchAgent {
            workspace,
            session,
            operation_id: operation,
            profile: None,
        });
        assert_eq!(host.port().launches.len(), 1);
        assert_eq!(host.port().launches[0].1.profile, None);
        assert!(matches!(
            host.pane(session).unwrap().pane().tabs(),
            [PaneTab::Pending(_)]
        ));
        host.apply(AgentLaunchEvent::Succeeded {
            operation,
            terminal: terminal.clone(),
        });
        host.select_live(session, &terminal);
        assert!(
            matches!(host.pane(session).unwrap().pane().selected(), PaneSelection::Tab(TabSelection::Live(selected)) if *selected == terminal)
        );
        host.input(session, b"hello");
        host.resize(
            session,
            Geometry {
                cols: 100,
                rows: 30,
            },
        );
        assert_eq!(host.port().input, vec![b"hello".to_vec()]);
        assert_eq!(
            host.port().resized,
            vec![Geometry {
                cols: 100,
                rows: 30
            }]
        );
    }
}
