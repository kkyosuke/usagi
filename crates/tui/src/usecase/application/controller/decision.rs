//! User-decision projection reducer.
//!
//! Decisions own an independently refreshed collection, unread markers, and a
//! modal editor. Keeping their convergence rules here prevents the workspace
//! controller's top-level router from also becoming the feature reducer.

use super::{
    AppState, DecisionEditor, DecisionOverlayState, Effect, Overlay, SafeError, UserDecision,
    UserDecisionId, UserDecisionStatus, WorkspaceId, reconcile_decision_overlay,
};
use std::collections::BTreeSet;

pub(super) enum Event {
    Snapshot {
        workspace: WorkspaceId,
        decisions: Vec<UserDecision>,
    },
    Resolved {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
    },
    Error {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        error: SafeError,
    },
}

pub(super) fn update(state: &mut AppState, event: Event) -> Vec<Effect> {
    match event {
        Event::Snapshot {
            workspace,
            decisions,
        } => {
            if workspace != state.workspace {
                return Vec::new();
            }
            let previously_known = state
                .decisions
                .iter()
                .map(|decision| decision.decision_id)
                .collect::<BTreeSet<_>>();
            state.decisions = decisions
                .into_iter()
                .filter(|decision| {
                    decision.owner.workspace_id == workspace
                        && decision.status == UserDecisionStatus::Pending
                })
                .collect();
            state.unread_decisions.retain(|id| {
                state
                    .decisions
                    .iter()
                    .any(|decision| decision.decision_id == *id)
            });
            state.unread_decisions.extend(
                state
                    .decisions
                    .iter()
                    .filter(|decision| !previously_known.contains(&decision.decision_id))
                    .map(|decision| decision.decision_id),
            );
            reconcile_decision_overlay(state);
            // A newly created decision is actionable immediately. Open its
            // answer editor rather than making the user traverse the list.
            // Reconnect snapshots contain only known rows, so they preserve a
            // deliberate dismiss and never steal focus.
            if state.overlay.is_none()
                && !state.workspace_drawer_open()
                && let Some(decision) = state
                    .decisions
                    .iter()
                    .find(|decision| !previously_known.contains(&decision.decision_id))
                    .cloned()
            {
                state.overlay = Some(Overlay::Decisions);
                state.decision_overlay = Some(DecisionOverlayState {
                    selected: 0,
                    editor: Some(DecisionEditor::new(decision)),
                });
            }
        }
        Event::Resolved {
            workspace,
            decision_id,
        } => {
            if workspace != state.workspace {
                return Vec::new();
            }
            let closes_active_modal = state
                .decision_overlay
                .as_ref()
                .and_then(|overlay| overlay.editor.as_ref())
                .is_some_and(|editor| editor.decision.decision_id == decision_id);
            state
                .decisions
                .retain(|decision| decision.decision_id != decision_id);
            state.unread_decisions.remove(&decision_id);
            if closes_active_modal {
                state.overlay = None;
                state.decision_overlay = None;
            } else {
                reconcile_decision_overlay(state);
            }
        }
        Event::Error {
            workspace,
            decision_id,
            error,
        } => {
            if workspace == state.workspace
                && let Some(editor) = state
                    .decision_overlay
                    .as_mut()
                    .and_then(|overlay| overlay.editor.as_mut())
                    .filter(|editor| editor.decision.decision_id == decision_id)
            {
                editor.error = Some(error);
            }
        }
    }
    Vec::new()
}
