//! Persistent mode and action hints, including the next Tab destination.

use super::{Role, Style, modal, widgets};
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

pub(super) fn next_field(editor: &DecisionEditor) -> Option<&'static str> {
    let decision = editor.decision();
    if decision.options.is_empty() {
        None
    } else if editor.input_comment() {
        Some(if decision.allow_freeform {
            "Freeform"
        } else {
            "Choices"
        })
    } else if editor.input_freeform() {
        Some("Choices")
    } else if decision.allow_comment {
        Some("Comment")
    } else {
        decision.allow_freeform.then_some("Freeform")
    }
}

fn mode_strip(editor: &DecisionEditor, width: usize) -> String {
    let decision = editor.decision();
    let mut modes = Vec::new();
    for (label, active, enabled) in [
        (
            "Choices",
            !editor.input_comment() && !editor.input_freeform(),
            !decision.options.is_empty(),
        ),
        (
            "Comment",
            editor.input_comment(),
            decision.allow_comment && !decision.options.is_empty(),
        ),
        ("Freeform", editor.input_freeform(), decision.allow_freeform),
    ] {
        if enabled && (width >= 50 || active) {
            let style = if active {
                Role::Accent.style().bold().reverse()
            } else {
                Style::new().dim()
            };
            modes.push(
                widgets::button::InlineButton::new(label)
                    .render(width, style)
                    .line,
            );
        }
    }
    modes.join(" ")
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
    let scroll = if width < 50 {
        String::new()
    } else {
        Style::new().dim().paint("  PgUp/PgDn: scroll")
    };
    vec![
        modal::content_line(
            &format!("{}  {summary}{scroll}", mode_strip(editor, width)),
            width,
        ),
        modal::content_line(&editor_hints(editor, width), width),
    ]
}

fn editor_hints(editor: &DecisionEditor, width: usize) -> String {
    let decision = editor.decision();
    let multiple = decision.selection_mode == UserDecisionSelectionMode::Multiple;
    let action = if decision.require_confirmation {
        "review"
    } else {
        "submit"
    };
    let mut hints = vec![
        Role::Accent
            .style()
            .bold()
            .reverse()
            .paint(&format!(" Enter: {action} ")),
    ];
    if let Some(next) = next_field(editor) {
        hints.push(Role::Info.style().bold().paint(&format!("Tab: {next}")));
    }
    hints.push("Esc: back".into());
    if !decision.options.is_empty() {
        hints.push(
            if editor.input_comment() || editor.input_freeform() {
                "↑↓: choices"
            } else {
                "↑↓: select"
            }
            .into(),
        );
        if multiple && !editor.input_comment() && !editor.input_freeform() {
            hints.push("Space: check".into());
        }
    }
    if width < 50 {
        hints = vec![Role::Accent.style().bold().reverse().paint(&if width < 28 {
            "Enter".to_owned()
        } else {
            format!("Enter:{action}")
        })];
        if let Some(next) = next_field(editor) {
            hints.push(if width < 28 {
                "Tab".into()
            } else {
                format!("Tab:{next}")
            });
        }
        hints.push("Esc".into());
    }
    hints.join(if width < 50 { " " } else { "  " })
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
        modal::content_line(
            &format!(
                "{}  {summary}{}",
                Role::Warning.style().bold().paint("Review · not sent"),
                if answer.comment().is_some() {
                    " + comment"
                } else {
                    ""
                }
            ),
            width,
        ),
        modal::content_line(
            &format!(
                "{}  Esc: edit  PgUp/PgDn: scroll",
                Role::Success
                    .style()
                    .bold()
                    .reverse()
                    .paint(" Enter: send ")
            ),
            width,
        ),
    ]
}
