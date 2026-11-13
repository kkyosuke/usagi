//! Optional comment drafts and the explicit answer review step.
use super::{
    AppKey, DecisionEditor, Effect, SafeError, SafeMessage, UserDecisionAnswer, WorkspaceId,
};
use unicode_segmentation::UnicodeSegmentation;
use usagi_core::domain::presentation_text::presentation_character_is_safe;

pub(super) fn sanitize_decision_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', " ")
        .chars()
        .filter(|c| *c == '\n' || presentation_character_is_safe(*c))
        .collect()
}

pub(super) fn push_decision_char(target: &mut String, ch: char) {
    if ch == '\r' || ch == '\n' {
        target.push('\n');
    } else if ch == '\t' {
        target.push(' ');
    } else if presentation_character_is_safe(ch) {
        target.push(ch);
    }
}

pub(super) fn pop_decision_grapheme(target: &mut String) {
    target.truncate(
        target
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index),
    );
}

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

/// Traverse fields in their visual order without discarding any draft or checks.
pub(super) fn move_input(editor: &mut DecisionEditor, forward: bool) {
    let options = editor.decision.options.len();
    let comment = usize::from(editor.decision.allow_comment);
    let current = if editor.input_freeform {
        options + comment
    } else if editor.input_comment {
        options
    } else {
        editor.selected_option
    };
    let last = options + comment + usize::from(editor.decision.allow_freeform) - 1;
    let next = if forward {
        current.saturating_add(1).min(last)
    } else if current == options {
        // Tab can leave any option for an input field. Return to that answer,
        // rather than silently replacing it with the last option in the list.
        editor.selected_option
    } else {
        current.saturating_sub(1)
    };
    editor.input_comment = next == options && editor.decision.allow_comment;
    editor.input_freeform = next == options + comment && editor.decision.allow_freeform;
    if next < options {
        editor.selected_option = next;
    }
    editor.follow_freeform = editor.input_freeform;
    editor.scroll_offset = None;
    editor.error = None;
}

pub(super) fn edit_comment(editor: &mut DecisionEditor, key: &AppKey) -> bool {
    match key {
        AppKey::Char(ch) => push_decision_char(&mut editor.comment, *ch),
        AppKey::Paste(text) => editor.comment.push_str(&sanitize_decision_text(text)),
        AppKey::Backspace => pop_decision_grapheme(&mut editor.comment),
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
        let message = if matches!(answer, UserDecisionAnswer::Freeform { .. }) {
            "freeform answers must be at most 16384 UTF-8 bytes without control characters."
        } else {
            "select a valid answer; comments must be at most 2048 UTF-8 bytes without control characters."
        };
        editor.error = Some(SafeError {
            message: SafeMessage::new(message),
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
