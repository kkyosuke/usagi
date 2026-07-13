//! Daemon-owned generic terminal launch / attach adapter.
//!
//! The controller owns command validation and the pane reducer owns visible
//! state.  This adapter is the intentionally small join between them: it maps
//! a stable target to a daemon-resolved scope, reuses an exact inventory ref,
//! or asks the daemon to launch one.  It never has a PTY or a path/name lookup.

use std::collections::HashSet;

use usagi_core::{
    domain::{
        id::{OperationId, TerminalRef},
        terminal_launch::TerminalLaunchRequest,
    },
    usecase::client::{TerminalGeometry, TerminalLaunchIntent},
};

use super::{
    controller::{Effect, Target},
    pane::{PaneEvent, PaneKind},
    pane_runtime::{Geometry, PaneRuntime, TerminalPort},
};

/// Stable scope resolver owned by the snapshot/lifecycle adapter.
///
/// Implementations must resolve only stored IDs.  In particular, display names
/// and filesystem paths are deliberately absent from this interface.
pub trait TerminalScopePort {
    /// Return the daemon launch scope for this exact selected target.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe error when the stored target cannot be
    /// resolved to an available daemon scope.
    fn terminal_scope(&mut self, target: Target) -> Result<TerminalLaunchRequest, String>;
}

/// The terminal launch surface needed in addition to [`TerminalPort`].
pub trait TerminalLaunchPort: TerminalScopePort {
    /// Return only complete, live terminal identities for one stable scope.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon inventory failure.
    fn terminal_inventory(
        &mut self,
        scope: &TerminalLaunchRequest,
    ) -> Result<Vec<TerminalRef>, String>;
    /// Ask the daemon to create a generic terminal; no local fallback exists.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon launch failure.
    fn launch_terminal(&mut self, intent: TerminalLaunchIntent) -> Result<TerminalRef, String>;
}

/// Runs `OpenTerminal` once per controller-issued durable operation.
pub struct TerminalLaunchAdapter<P> {
    port: P,
    submitted: HashSet<OperationId>,
    geometry: Geometry,
}

impl<P> TerminalLaunchAdapter<P> {
    #[must_use]
    pub fn new(port: P, geometry: Geometry) -> Self {
        Self {
            port,
            submitted: HashSet::new(),
            geometry,
        }
    }

    #[must_use]
    pub fn port(&self) -> &P {
        &self.port
    }
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }
}

impl<P: TerminalLaunchPort + TerminalPort> TerminalLaunchAdapter<P> {
    /// Apply only generic-terminal effects. Invalid and repeated effects cannot
    /// create local terminals or duplicate daemon launch intent.
    pub fn dispatch(&mut self, runtime: &mut PaneRuntime, effect: Effect) {
        let Effect::OpenTerminal {
            target,
            operation_id,
            arguments,
        } = effect
        else {
            return;
        };
        if !self.submitted.insert(operation_id) {
            return;
        }
        runtime.dispatch(
            &mut self.port,
            PaneEvent::Request {
                operation: operation_id,
                target,
                kind: PaneKind::Terminal,
            },
        );
        let result = self.resolve(target, &arguments);
        match result {
            Ok(terminal) => runtime.dispatch(
                &mut self.port,
                PaneEvent::Succeeded {
                    operation: operation_id,
                    terminal,
                },
            ),
            Err(message) => runtime.dispatch(
                &mut self.port,
                PaneEvent::Failed {
                    operation: operation_id,
                    message,
                },
            ),
        }
    }

    fn resolve(&mut self, target: Target, mode: &str) -> Result<TerminalRef, String> {
        let scope = self.port.terminal_scope(target)?;
        if mode == "open"
            && let Some(terminal) = self.port.terminal_inventory(&scope)?.into_iter().next()
        {
            return Ok(terminal);
        }
        self.port.launch_terminal(TerminalLaunchIntent {
            request: scope,
            geometry: TerminalGeometry {
                cols: self.geometry.cols,
                rows: self.geometry.rows,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::application::{
        controller::Target,
        pane::{PaneSelection, PaneTab},
    };
    use usagi_core::domain::{
        id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId},
        terminal_launch::{TerminalLaunchScope, TerminalProfileId},
    };

    struct Fake {
        scope: TerminalLaunchRequest,
        inventory: Vec<TerminalRef>,
        launched: usize,
        attached: Vec<TerminalRef>,
    }
    impl TerminalScopePort for Fake {
        fn terminal_scope(&mut self, _: Target) -> Result<TerminalLaunchRequest, String> {
            Ok(self.scope.clone())
        }
    }
    impl TerminalLaunchPort for Fake {
        fn terminal_inventory(
            &mut self,
            _: &TerminalLaunchRequest,
        ) -> Result<Vec<TerminalRef>, String> {
            Ok(self.inventory.clone())
        }
        fn launch_terminal(&mut self, _: TerminalLaunchIntent) -> Result<TerminalRef, String> {
            self.launched += 1;
            Ok(self.inventory[0].clone())
        }
    }
    impl TerminalPort for Fake {
        fn inventory(
            &mut self,
        ) -> Result<
            Vec<super::super::pane_runtime::TerminalInventory>,
            super::super::pane_runtime::TerminalError,
        > {
            Ok(vec![])
        }
        fn attach(
            &mut self,
            terminal: &TerminalRef,
            _: Option<u64>,
        ) -> Result<
            super::super::pane_runtime::TerminalSnapshot,
            super::super::pane_runtime::TerminalError,
        > {
            self.attached.push(terminal.clone());
            Ok(super::super::pane_runtime::TerminalSnapshot {
                terminal: terminal.clone(),
                output_offset: 0,
                geometry: Geometry { cols: 80, rows: 24 },
                replay: vec![],
                exited: false,
            })
        }
        fn resync(
            &mut self,
            _: &TerminalRef,
        ) -> Result<
            super::super::pane_runtime::TerminalSnapshot,
            super::super::pane_runtime::TerminalError,
        > {
            unreachable!()
        }
        fn input(
            &mut self,
            _: &TerminalRef,
            _: &[u8],
        ) -> Result<(), super::super::pane_runtime::TerminalError> {
            Ok(())
        }
        fn resize(
            &mut self,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<(), super::super::pane_runtime::TerminalError> {
            Ok(())
        }
        fn detach(&mut self, _: &TerminalRef) {}
    }
    fn setup() -> (Target, TerminalRef, TerminalLaunchRequest) {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let terminal = TerminalRef {
            workspace_id: workspace,
            worktree_id: worktree,
            session_id: Some(session),
            terminal_id: TerminalId::new(),
            daemon_generation: DaemonGeneration::new(),
        };
        let scope = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: worktree,
            },
        };
        (Target::Session(session), terminal, scope)
    }
    #[test]
    fn open_reuses_exact_inventory_ref_and_duplicate_effect_is_ignored() {
        let (target, terminal, scope) = setup();
        let operation_id = OperationId::new();
        let fake = Fake {
            scope,
            inventory: vec![terminal.clone()],
            launched: 0,
            attached: vec![],
        };
        let mut adapter = TerminalLaunchAdapter::new(fake, Geometry { cols: 80, rows: 24 });
        let mut runtime = PaneRuntime::new(super::super::pane::PaneState::new(
            PaneSelection::Target(target),
        ));
        let effect = Effect::OpenTerminal {
            target,
            operation_id,
            arguments: "open".into(),
        };
        adapter.dispatch(&mut runtime, effect.clone());
        adapter.dispatch(&mut runtime, effect);
        assert_eq!(adapter.port().launched, 0);
        assert_eq!(adapter.port().attached, vec![terminal]);
        assert!(matches!(runtime.pane().tabs(), [PaneTab::Live(_)]));
    }
    #[test]
    fn new_launches_once_then_attaches_returned_ref() {
        let (target, terminal, scope) = setup();
        let fake = Fake {
            scope,
            inventory: vec![terminal.clone()],
            launched: 0,
            attached: vec![],
        };
        let mut adapter = TerminalLaunchAdapter::new(fake, Geometry { cols: 80, rows: 24 });
        let mut runtime = PaneRuntime::new(super::super::pane::PaneState::new(
            PaneSelection::Target(target),
        ));
        adapter.dispatch(
            &mut runtime,
            Effect::OpenTerminal {
                target,
                operation_id: OperationId::new(),
                arguments: "new".into(),
            },
        );
        assert_eq!(adapter.port().launched, 1);
        assert_eq!(adapter.port().attached, vec![terminal]);
    }
}
