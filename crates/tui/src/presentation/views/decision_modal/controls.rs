//! Quiet, persistent answer summaries and keyboard hints.

use super::{modal, widgets};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::user_decision::{UserDecision, UserDecisionSelectionMode};

pub(super) fn kind(decision: &UserDecision) -> &'static str {
    if decision.options.is_empty() {
        "Freeform only"
    } else if decision.selection_mode == UserDecisionSelectionMode::Multiple {
        "Multiple choice"
    } else {
        "Single choice"
    }
}

pub(super) fn editor_footer(editor: &DecisionEditor, width: usize) -> Vec<String> {
    let decision = editor.decision();
    let multiple = decision.selection_mode == UserDecisionSelectionMode::Multiple;
    let count = decision
        .options
        .iter()
        .filter(|option| editor.option_checked(&option.id))
        .count();
    let summary = if editor.input_freeform() {
        "custom answer only".to_owned()
    } else if multiple {
        let (min, max) = decision.selection_bounds();
        format!("{count} selected ({min}-{max})")
    } else if editor.input_comment() {
        "note for selected choice".to_owned()
    } else {
        "single choice".to_owned()
    };
    let action = if decision.require_confirmation {
        "review"
    } else {
        "submit"
    };
    let move_hint = if decision.options.is_empty() {
        ""
    } else {
        "↑↓ move  "
    };
    let hints = if width < 28 {
        "Enter Esc".to_owned()
    } else if width < 50 {
        format!(
            "Enter:{action} Esc{}",
            if decision.options.is_empty() {
                ""
            } else {
                " ↑↓"
            }
        )
    } else {
        format!(
            "{move_hint}{}Enter: {action}  Esc: back  PgUp/PgDn: scroll",
            if multiple && !editor.input_comment() && !editor.input_freeform() {
                "Space: check  "
            } else {
                ""
            }
        )
    };
    let summary = if decision.allow_comment && !decision.options.is_empty() {
        format!("{summary}  ·  Tab: fields (keep choice)")
    } else {
        summary
    };
    vec![modal::footer(&summary), modal::footer(&hints)]
        .into_iter()
        .map(|line| widgets::clip_to_width(&line, width))
        .collect()
}

pub(super) fn review_footer(
    answer: &usagi_core::domain::user_decision::UserDecisionAnswer,
    width: usize,
) -> Vec<String> {
    use usagi_core::domain::user_decision::UserDecisionAnswer;
    let summary = match answer {
        UserDecisionAnswer::Option { .. } => "1 choice".to_owned(),
        UserDecisionAnswer::Options { option_ids, .. } => format!("{} choices", option_ids.len()),
        UserDecisionAnswer::Freeform { .. } => "custom answer".to_owned(),
    };
    vec![
        modal::footer(&format!(
            "Review · not sent  {summary}{}",
            if answer.comment().is_some() {
                " + comment"
            } else {
                ""
            }
        )),
        modal::footer(if width < 50 {
            "Enter: send  Esc: edit"
        } else {
            "Enter: send  Esc: edit  PgUp/PgDn: scroll"
        }),
    ]
    .into_iter()
    .map(|line| widgets::clip_to_width(&line, width))
    .collect()
}
