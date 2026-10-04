//! User-decision projection reducer.
//!
//! Decisions own an independently refreshed collection, unread markers, and a
//! modal editor. Keeping their convergence rules here prevents the workspace
//! controller's top-level router from also becoming the feature reducer.

mod composition;

use super::{
    AppKey, AppState, DecisionEditor, DecisionOverlayState, Effect, Overlay, SafeError,
    SafeMessage, UserDecision, UserDecisionAnswer, UserDecisionId, UserDecisionSelectionMode,
    UserDecisionStatus, WorkspaceId, reconcile_decision_overlay,
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
                editor.scroll_offset = None;
            }
        }
    }
    Vec::new()
}

pub(super) fn update_decision_editor(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    key: AppKey,
) -> Vec<Effect> {
    if editor.confirmation.is_some() {
        return composition::update_confirmation(workspace, editor, &key);
    }
    if editor.input_comment && composition::edit_comment(editor, &key) {
        return Vec::new();
    }
    let multiple = editor.decision.selection_mode == UserDecisionSelectionMode::Multiple;
    match key {
        AppKey::Left => {
            editor.context_column = editor.context_column.saturating_sub(8);
        }
        AppKey::Right => {
            editor.context_column = editor
                .context_column
                .saturating_add(8)
                .min(usagi_core::domain::user_decision::UserDecisionPolicy::DIAGRAM_MAX_BYTES);
        }
        AppKey::Tab
            if !editor.decision.options.is_empty()
                && (editor.decision.allow_comment || editor.decision.allow_freeform) =>
        {
            composition::cycle_input(editor);
        }
        AppKey::Char(' ') if multiple && !editor.input_freeform && !editor.input_comment => {
            toggle_decision_option(editor);
        }
        AppKey::DecisionPrevious | AppKey::Up if !editor.decision.options.is_empty() => {
            editor.selected_option = editor.selected_option.saturating_sub(1);
            editor.scroll_offset = None;
            editor.follow_freeform = false;
            editor.input_freeform = false;
            editor.input_comment = false;
            editor.error = None;
        }
        AppKey::DecisionNext | AppKey::Down if !editor.decision.options.is_empty() => {
            editor.selected_option =
                (editor.selected_option + 1).min(editor.decision.options.len().saturating_sub(1));
            editor.scroll_offset = None;
            editor.follow_freeform = false;
            editor.input_freeform = false;
            editor.input_comment = false;
            editor.error = None;
        }
        AppKey::PageUp => {
            editor.scroll_offset = Some(
                editor
                    .scroll_offset
                    .unwrap_or_default()
                    .saturating_sub(DecisionEditor::SCROLL_STEP),
            );
            editor.follow_freeform = false;
        }
        AppKey::PageDown => {
            editor.scroll_offset = Some(
                editor
                    .scroll_offset
                    .unwrap_or_default()
                    .saturating_add(DecisionEditor::SCROLL_STEP),
            );
            editor.follow_freeform = false;
        }
        AppKey::SetDecisionFreeform(text) => {
            if editor.decision.allow_freeform {
                editor.freeform = text;
                follow_decision_freeform(editor);
            }
        }
        AppKey::Char(ch) if editor.decision.allow_freeform && editor.input_freeform => {
            editor.freeform.push(ch);
            follow_decision_freeform(editor);
        }
        AppKey::Backspace if editor.decision.allow_freeform && editor.input_freeform => {
            editor.freeform.pop();
            follow_decision_freeform(editor);
        }
        AppKey::Paste(text) if editor.decision.allow_freeform && editor.input_freeform => {
            paste_decision_freeform(editor, &text);
        }
        AppKey::SubmitDecision | AppKey::Enter => {
            return submit_decision(workspace, editor, multiple);
        }
        _ => {}
    }
    Vec::new()
}

fn follow_decision_freeform(editor: &mut DecisionEditor) {
    editor.input_comment = false;
    editor.scroll_offset = None;
    editor.follow_freeform = true;
    editor.input_freeform = true;
    editor.input_comment = false;
    editor.error = None;
}

fn paste_decision_freeform(editor: &mut DecisionEditor, text: &str) {
    editor.freeform.push_str(text);
    follow_decision_freeform(editor);
}

fn submit_decision(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    multiple: bool,
) -> Vec<Effect> {
    editor.scroll_offset = None;
    editor.error = None;
    let answer = if editor.decision.allow_freeform && editor.input_freeform {
        UserDecisionAnswer::Freeform {
            text: editor.freeform.trim().to_owned(),
        }
    } else if multiple {
        UserDecisionAnswer::Options {
            comment: composition::answer_comment(editor),
            option_ids: editor
                .decision
                .options
                .iter()
                .filter(|option| editor.checked_options.contains(&option.id))
                .map(|option| option.id.clone())
                .collect(),
        }
    } else if let Some(option) = editor.decision.options.get(editor.selected_option) {
        UserDecisionAnswer::Option {
            comment: composition::answer_comment(editor),
            option_id: option.id.clone(),
        }
    } else {
        editor.error = Some(SafeError {
            message: SafeMessage::new("select a valid answer"),
            error_id: "decision-invalid-answer".to_owned(),
        });
        return Vec::new();
    };
    if let UserDecisionAnswer::Options { option_ids, .. } = &answer {
        let (min, max) = editor.decision.selection_bounds();
        if !(min..=max).contains(&option_ids.len()) {
            editor.error = Some(SafeError {
                message: SafeMessage::new(format!(
                    "Choose {min}-{max} options ({} selected).",
                    option_ids.len()
                )),
                error_id: "decision-selection-limit".into(),
            });
            return Vec::new();
        }
    }
    composition::prepare_answer(workspace, editor, answer)
}

fn toggle_decision_option(editor: &mut DecisionEditor) {
    if let Some(option) = editor.decision.options.get(editor.selected_option) {
        editor.scroll_offset = None;
        editor.follow_freeform = false;
        editor.error = None;
        if !editor.checked_options.remove(&option.id) {
            let (min, max) = editor.decision.selection_bounds();
            if editor.checked_options.len() >= max {
                editor.error = Some(SafeError {
                    message: SafeMessage::new(format!(
                        "Choose {min}-{max} options; uncheck one before adding another."
                    )),
                    error_id: "decision-selection-limit".into(),
                });
            } else {
                editor.checked_options.insert(option.id.clone());
            }
        }
    }
}
