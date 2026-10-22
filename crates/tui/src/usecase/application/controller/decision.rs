//! User-decision projection reducer.
//!
//! Decisions own an independently refreshed collection, unread markers, and a
//! modal editor. Keeping their convergence rules here prevents the workspace
//! controller's top-level router from also becoming the feature reducer.

mod composition;

use super::{
    AppKey, AppState, DecisionEditor, DecisionOverlayState, DecisionSnapshot, Effect, Overlay,
    SafeError, SafeMessage, SavedAnswerMode, UserDecision, UserDecisionAnswer, UserDecisionId,
    UserDecisionSelectionMode, UserDecisionStatus, WorkspaceId, reconcile_decision_overlay,
};
use std::collections::BTreeSet;

pub(super) fn restore(state: &mut AppState, snapshot: DecisionSnapshot) -> Vec<Effect> {
    state.decisions = snapshot.decisions;
    state.decision_answers = snapshot.answers;
    state.unread_decisions = snapshot.unread;
    Vec::new()
}

pub(super) fn unavailable(state: &mut AppState, workspace: WorkspaceId) -> Vec<Effect> {
    if workspace != state.workspace {
        return Vec::new();
    }
    if state.overlay == Some(Overlay::Decisions)
        && let Some(overlay) = state.decision_overlay.as_mut()
        && overlay.requested.take().is_some()
    {
        overlay.error = Some("This question could not be loaded. Close and try again.");
    }
    state.notice = Some(super::Notice::new("User decisions are unavailable."));
    Vec::new()
}

fn open_requested(state: &mut AppState) -> bool {
    let visible = state.overlay == Some(Overlay::Decisions);
    let requested = state.decision_overlay.as_mut().and_then(|overlay| {
        // A resident observer may deliver an old inventory before the
        // explicitly woken read. Recover only the originally chosen ID.
        let retry = if overlay.error.is_some() {
            overlay.exact
        } else {
            None
        };
        overlay.requested.take().or(retry)
    });
    let Some(decision_id) = requested else {
        return false;
    };
    if visible {
        let overlay = state
            .decision_overlay
            .as_mut()
            .expect("requested decision overlay");
        if let Some(decision) = state
            .decisions
            .iter()
            .chain(&state.decision_answers)
            .find(|decision| decision.decision_id == decision_id)
        {
            overlay.show_answers = decision.status == UserDecisionStatus::Resolved;
            let rows = if overlay.show_answers {
                &state.decision_answers
            } else {
                &state.decisions
            };
            overlay.selected = rows
                .iter()
                .position(|row| row.decision_id == decision_id)
                .unwrap_or(0);
            overlay.editor = Some(DecisionEditor::new(decision.clone()));
            overlay.error = None;
            state.unread_decisions.remove(&decision_id);
        } else {
            overlay.error =
                Some("This question is no longer available. Close and refresh the inbox.");
        }
    } else {
        state.decision_overlay = None;
    }
    true
}

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

fn observe(
    state: &mut AppState,
    workspace: WorkspaceId,
    decisions: Vec<UserDecision>,
) -> Vec<Effect> {
    if workspace != state.workspace {
        return Vec::new();
    }
    let previously_known = state
        .decisions
        .iter()
        .map(|decision| decision.decision_id)
        .collect::<BTreeSet<_>>();
    (state.decisions, state.decision_answers) = decisions
        .into_iter()
        .filter(|decision| {
            matches!(
                decision.status,
                UserDecisionStatus::Pending | UserDecisionStatus::Resolved
            )
        })
        .partition(|decision| decision.status == UserDecisionStatus::Pending);
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
    if open_requested(state) {
        return Vec::new();
    }
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
            show_answers: false,
            selected: 0,
            editor: Some(DecisionEditor::new(decision)),
            requested: None,
            error: None,
            exact: None,
        });
    }
    Vec::new()
}

pub(super) fn update(state: &mut AppState, event: Event) -> Vec<Effect> {
    match event {
        Event::Snapshot {
            workspace,
            decisions,
        } => return observe(state, workspace, decisions),
        Event::Resolved {
            workspace,
            decision_id,
        } => {
            if !state
                .decisions
                .iter()
                .chain(&state.decision_answers)
                .any(|decision| {
                    decision.decision_id == decision_id && decision.owner.workspace_id == workspace
                })
            {
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
            state
                .decision_answers
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
            if let Some(editor) = state
                .decision_overlay
                .as_mut()
                .and_then(|overlay| overlay.editor.as_mut())
                .filter(|editor| {
                    editor.decision.decision_id == decision_id
                        && editor.decision.owner.workspace_id == workspace
                })
            {
                editor.error = Some(error);
                editor.scroll_offset = None;
            }
        }
    }
    Vec::new()
}

pub(super) fn scroll(
    state: &mut AppState,
    decision_id: UserDecisionId,
    offset: usize,
) -> Vec<Effect> {
    if state.overlay == Some(Overlay::Decisions)
        && let Some(editor) = state
            .decision_overlay
            .as_mut()
            .and_then(|overlay| overlay.editor.as_mut())
        && editor.decision.decision_id == decision_id
    {
        editor.scroll_offset = Some(offset);
        editor.follow_freeform = false;
        state.pending_session_click = None;
        state.interaction_count = state.interaction_count.saturating_add(1);
    }
    Vec::new()
}

pub(super) fn update_decision_editor(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    key: AppKey,
) -> Vec<Effect> {
    if editor.decision.status == UserDecisionStatus::Resolved {
        return update_saved_answer(workspace, editor, &key);
    }
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
            composition::move_input(editor, false);
        }
        AppKey::DecisionNext | AppKey::Down if !editor.decision.options.is_empty() => {
            composition::move_input(editor, true);
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

fn update_saved_answer(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    key: &AppKey,
) -> Vec<Effect> {
    match key {
        AppKey::Escape => {
            editor.saved_answer_mode = SavedAnswerMode::View;
            editor.error = None;
        }
        AppKey::Char('d') if !editor.discard_confirmation() => {
            editor.saved_answer_mode = SavedAnswerMode::ConfirmDiscard;
            editor.scroll_offset = Some(0);
            editor.error = None;
        }
        AppKey::Enter if editor.discard_confirmation() => {
            return vec![Effect::DiscardDecision {
                workspace,
                decision_id: editor.decision.decision_id,
            }];
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
