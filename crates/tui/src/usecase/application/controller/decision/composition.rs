//! Optional comment drafts and the explicit answer review step.
use super::{
    AppKey, DecisionEditor, Effect, SafeError, SafeMessage, UserDecisionAnswer, WorkspaceId,
};

pub(super) fn answer_comment(editor: &DecisionEditor) -> Option<String> {
    (editor.decision.allow_comment && !editor.comment.trim().is_empty())
        .then(|| editor.comment.trim().to_owned())
}

pub(super) fn cycle_input(editor: &mut DecisionEditor) {
    if editor.input_comment {
        editor.input_comment = false;
        editor.input_freeform = editor.decision.allow_freeform;
    } else if editor.input_freeform {
        editor.input_freeform = false;
    } else if editor.decision.allow_comment {
        editor.input_comment = true;
    } else {
        editor.input_freeform = editor.decision.allow_freeform;
    }
    editor.scroll_offset = None;
    editor.follow_freeform = editor.input_freeform;
    editor.error = None;
}

pub(super) fn edit_comment(editor: &mut DecisionEditor, key: &AppKey) -> bool {
    match key {
        AppKey::Char(ch) => editor.comment.push(*ch),
        AppKey::Paste(text) => editor.comment.push_str(text),
        AppKey::Backspace => {
            editor.comment.pop();
        }
        _ => return false,
    }
    editor.scroll_offset = None;
    editor.error = None;
    true
}

fn valid_answer(editor: &mut DecisionEditor, answer: &UserDecisionAnswer) -> bool {
    if matches!(answer, UserDecisionAnswer::Freeform { text } if text.trim().is_empty()) {
        editor.error = Some(SafeError {
            message: SafeMessage::new("Write a freeform answer before continuing."),
            error_id: "decision-empty-freeform".into(),
        });
        return false;
    }
    if editor
        .decision
        .validate_answer(answer, chrono::Utc::now())
        .is_err()
    {
        editor.error = Some(SafeError {
            message: SafeMessage::new(
                "select a valid answer; comments must be at most 2048 UTF-8 bytes without control characters.",
            ),
            error_id: "decision-invalid-answer".into(),
        });
        return false;
    }
    editor.error = None;
    true
}

pub(super) fn prepare_answer(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    answer: UserDecisionAnswer,
) -> Vec<Effect> {
    if !valid_answer(editor, &answer) {
        return Vec::new();
    }
    if editor.decision.require_confirmation {
        editor.confirmation = Some(answer);
        editor.scroll_offset = Some(0);
        Vec::new()
    } else {
        vec![Effect::ResolveDecision {
            workspace,
            decision_id: editor.decision.decision_id,
            answer,
        }]
    }
}

pub(super) fn update_confirmation(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    key: &AppKey,
) -> Vec<Effect> {
    match key {
        AppKey::Escape => {
            editor.confirmation = None;
            editor.scroll_offset = None;
            editor.error = None;
        }
        AppKey::PageUp => {
            editor.scroll_offset = Some(
                editor
                    .scroll_offset
                    .unwrap_or_default()
                    .saturating_sub(DecisionEditor::SCROLL_STEP),
            );
        }
        AppKey::PageDown => {
            editor.scroll_offset = Some(
                editor
                    .scroll_offset
                    .unwrap_or_default()
                    .saturating_add(DecisionEditor::SCROLL_STEP),
            );
        }
        AppKey::Enter | AppKey::SubmitDecision => {
            let answer = editor
                .confirmation
                .clone()
                .expect("confirmation route requires an answer");
            editor.scroll_offset = None;
            if valid_answer(editor, &answer) {
                return vec![Effect::ResolveDecision {
                    workspace,
                    decision_id: editor.decision.decision_id,
                    answer,
                }];
            }
        }
        _ => {}
    }
    Vec::new()
}
