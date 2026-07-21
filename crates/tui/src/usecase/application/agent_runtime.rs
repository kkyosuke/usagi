//! Controller effect runner for daemon-owned Agent panes.
//!
//! The v2 presentation host keeps this object for the lifetime of a workspace.
//! It is the only place where a `LaunchAgent` effect is joined to the
//! session-scoped pane runtime; neither the controller nor a renderer opens a
//! local process.

use std::collections::HashMap;

use super::{
    agent_launch::{AgentLaunchAdapter, AgentLaunchEvent, AgentLaunchPort},
    controller::{self, AppEvent, AppState, Effect, Target},
    pane::{PaneEvent, PaneSelection, PaneState, PaneTab, TabSelection},
    pane_runtime::{Geometry, PaneRuntime, TerminalPort, TerminalStreamEvent},
};

/// Stateful v2 runtime bridge for Agent effects and terminal observations.
///
/// Panes are keyed by [`Target`] so the workspace root (`Target::Root`) owns an
/// Agent pane strip exactly like a managed session (`Target::Session`) does.
pub struct AgentRuntimeHost<P> {
    adapter: AgentLaunchAdapter<P>,
    panes: HashMap<Target, PaneRuntime>,
}

/// Home controller と daemon-owned Agent pane host を同じ workspace
/// lifetime で合成する runtime。
///
/// `AppState` が発行した effect だけを [`AgentRuntimeHost`] へ渡すため、
/// presentation は profile / operation / terminal identity を組み立てない。
pub struct AgentRuntime<P> {
    state: AppState,
    host: AgentRuntimeHost<P>,
}

impl<P> AgentRuntime<P> {
    #[must_use]
    pub fn new(state: AppState, port: P) -> Self {
        Self {
            state,
            host: AgentRuntimeHost::new(port),
        }
    }

    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    #[must_use]
    pub fn host(&self) -> &AgentRuntimeHost<P> {
        &self.host
    }
}

impl<P: AgentLaunchPort + TerminalPort> AgentRuntime<P> {
    /// Reduce one controller event and dispatch only its Agent effects.
    /// Other effects remain available to their existing composition owners.
    pub fn update(&mut self, event: AppEvent) -> Vec<Effect> {
        let effects = controller::update(&mut self.state, event);
        for effect in &effects {
            self.host.dispatch(effect.clone());
        }
        self.sync_live_pane();
        effects
    }

    /// Feed an async daemon completion into the v2 pane host.  The adapter
    /// accepts it only when operation and terminal fences match.
    pub fn apply(&mut self, event: &AgentLaunchEvent) {
        self.host.apply(event);
        self.sync_live_pane();
    }

    fn sync_live_pane(&mut self) {
        // The active target's pane owns the live signal whether it is a session
        // or the workspace root; both are keyed uniformly in the host.
        let live = self.host.pane(self.state.active()).is_some_and(|runtime| {
            runtime
                .pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Live(_)))
        });
        let _ = controller::update(&mut self.state, AppEvent::LivePaneAvailability(live));
    }
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
    pub fn pane(&self, target: Target) -> Option<&PaneRuntime> {
        self.panes.get(&target)
    }

    #[must_use]
    pub fn port(&self) -> &P {
        self.adapter.port()
    }

    /// Expose the injected daemon boundary to the composition host.  This is
    /// intentionally mutable only at the outer boundary: pane transitions
    /// continue to go through this host so an effect cannot bypass its
    /// operation and scope fencing.
    pub fn port_mut(&mut self) -> &mut P {
        self.adapter.port_mut()
    }
}

impl<P: AgentLaunchPort + TerminalPort> AgentRuntimeHost<P> {
    /// Run a controller effect. Only Agent launch effects enter this host;
    /// other controller effects remain owned by their existing runners.
    pub fn dispatch(&mut self, effect: Effect) {
        let Effect::LaunchAgent {
            workspace, session, ..
        } = &effect
        else {
            return;
        };
        // A workspace-root Agent (`session == None`) is keyed by `Target::Root`;
        // a session Agent by `Target::Session`.
        let target = session.map_or(Target::Root(*workspace), Target::Session);
        let runtime = self
            .panes
            .entry(target)
            .or_insert_with(|| PaneRuntime::new(PaneState::new(PaneSelection::Target(target))));
        self.adapter.dispatch(runtime, effect);
    }

    /// Apply a daemon completion to the matching pending session pane. The
    /// adapter fences the operation and terminal scope before it can attach.
    pub fn apply(&mut self, event: &AgentLaunchEvent) {
        for runtime in self.panes.values_mut() {
            self.adapter.apply(runtime, event.clone());
        }
    }

    /// Select a completed tab when the v2 UI focuses its owning session. This
    /// is deliberately explicit so a background completion cannot steal focus.
    pub fn select_live(&mut self, target: Target, terminal: &usagi_core::domain::id::TerminalRef) {
        if let Some(runtime) = self.panes.get_mut(&target) {
            runtime.dispatch(
                self.adapter.port_mut(),
                PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal.clone()))),
            );
        }
    }

    /// Relay one daemon terminal observation for its owning target only.
    pub fn stream(&mut self, target: Target, event: TerminalStreamEvent) {
        if let Some(runtime) = self.panes.get_mut(&target) {
            runtime.stream_event(self.adapter.port_mut(), event);
        }
    }

    /// Relay selected-pane input without a local fallback.
    pub fn input(&mut self, target: Target, bytes: &[u8]) {
        if let Some(runtime) = self.panes.get_mut(&target) {
            runtime.input(self.adapter.port_mut(), bytes);
        }
    }

    /// Relay a geometry change to the selected attached pane.
    pub fn resize(&mut self, target: Target, geometry: Geometry) {
        if let Some(runtime) = self.panes.get_mut(&target) {
            runtime.resize(self.adapter.port_mut(), geometry);
        }
    }

    /// Reconcile the selected pane after a daemon reconnect.
    pub fn reconnect(&mut self, target: Target) {
        if let Some(runtime) = self.panes.get_mut(&target) {
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
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::super::pane::PaneTab;
    use super::super::pane_runtime::{TerminalError, TerminalInventory, TerminalSnapshot};
    use super::*;
    use usagi_core::domain::id::{
        DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
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
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=agent_runtime_fake_port_contract
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
        host.dispatch(Effect::Detach);
        host.dispatch(Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        assert_eq!(host.port().launches.len(), 1);
        assert_eq!(host.port().launches[0].1.profile, None);
        assert!(matches!(
            host.pane(Target::Session(session)).unwrap().pane().tabs(),
            [PaneTab::Pending(_)]
        ));
        host.apply(&AgentLaunchEvent::Succeeded {
            operation,
            terminal: terminal.clone(),
        });
        host.port_mut().terminal = Some(terminal.clone());
        host.select_live(Target::Session(session), &terminal);
        assert!(
            matches!(host.pane(Target::Session(session)).unwrap().pane().selected(), PaneSelection::Tab(TabSelection::Live(selected)) if *selected == terminal)
        );
        host.input(Target::Session(session), b"hello");
        host.resize(
            Target::Session(session),
            Geometry {
                cols: 100,
                rows: 30,
            },
        );
        host.stream(
            Target::Session(session),
            TerminalStreamEvent::Output {
                terminal: terminal.clone(),
                start_offset: 0,
                end_offset: 2,
                data: b"ok".to_vec(),
            },
        );
        host.reconnect(Target::Session(session));
        host.detach();
        assert_eq!(host.port().input, vec![b"hello".to_vec()]);
        assert_eq!(
            host.port().resized,
            vec![Geometry {
                cols: 100,
                rows: 30
            }]
        );
    }

    #[test]
    fn explicit_codex_profile_uses_the_same_daemon_path_and_a_failure_clears_pending() {
        let (workspace, session, _) = ids();
        let operation = OperationId::new();
        let mut host = AgentRuntimeHost::new(Fake::default());
        host.dispatch(Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: Some(usagi_core::domain::agent::AgentProfileId::new("codex").unwrap()),
        });

        assert_eq!(host.port().launches.len(), 1);
        assert_eq!(
            host.port().launches[0]
                .1
                .profile
                .as_ref()
                .map(usagi_core::domain::agent::AgentProfileId::as_str),
            Some("codex")
        );
        host.apply(&AgentLaunchEvent::Failed {
            operation,
            message: "agent profile is unavailable".to_owned(),
        });

        let pane = host.pane(Target::Session(session)).unwrap().pane();
        assert!(pane.tabs().is_empty());
        assert_eq!(pane.error(), Some("agent profile is unavailable"));
    }

    #[test]
    fn completion_for_another_session_is_inert_at_the_host_boundary() {
        let (workspace, session, _) = ids();
        let operation = OperationId::new();
        let mut host = AgentRuntimeHost::new(Fake::default());
        host.dispatch(Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        let (_, other_session, other_terminal) = ids();
        assert_ne!(other_session, session);
        host.apply(&AgentLaunchEvent::Succeeded {
            operation,
            terminal: other_terminal,
        });

        assert!(matches!(
            host.pane(Target::Session(session)).unwrap().pane().tabs(),
            [PaneTab::Pending(pending)] if pending.operation == operation
        ));
    }

    #[test]
    fn controller_closeup_effect_reaches_the_host_once_with_default_and_codex_profiles() {
        let (workspace, session, _) = ids();
        let mut runtime =
            AgentRuntime::new(AppState::home(workspace, vec![session]), Fake::default());
        assert_eq!(runtime.state().workspace(), workspace);

        let _ = runtime.update(AppEvent::Key(controller::AppKey::Down));
        let _ = runtime.update(AppEvent::Key(controller::AppKey::Enter));
        let effects = runtime.update(AppEvent::Key(controller::AppKey::SubmitCloseup(
            "agent".to_owned(),
        )));
        assert!(matches!(
            effects.as_slice(),
            [Effect::LaunchAgent { profile: None, .. }]
        ));
        assert_eq!(runtime.host().port().launches.len(), 1);
        assert_eq!(runtime.host().port().launches[0].1.profile, None);

        // A successful submit closes the action modal, so re-open it before the
        // next launch. (The live resample no longer re-forces it; see #352.)
        let _ = runtime.update(AppEvent::Key(controller::AppKey::OpenCloseupOverlay));
        let effects = runtime.update(AppEvent::Key(controller::AppKey::SubmitCloseup(
            "agent codex".to_owned(),
        )));
        assert!(matches!(
            effects.as_slice(),
            [Effect::LaunchAgent {
                profile: Some(_),
                ..
            }]
        ));
        assert_eq!(runtime.host().port().launches.len(), 2);
        let operation = runtime.host().port().launches[1].0;
        runtime.apply(&AgentLaunchEvent::Failed {
            operation,
            message: "failed".to_owned(),
        });
        assert_eq!(
            runtime.host().port().launches[1]
                .1
                .profile
                .as_ref()
                .map(usagi_core::domain::agent::AgentProfileId::as_str),
            Some("codex")
        );
    }
}
