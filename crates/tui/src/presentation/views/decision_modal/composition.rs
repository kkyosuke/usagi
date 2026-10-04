//! Comment entry and answer review rendering.
use super::{Role, layout, modal, wrapped_content_lines};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::user_decision::UserDecisionAnswer;

pub(super) fn comment_rows(editor: &DecisionEditor, width: usize) -> Vec<String> {
    input_rows(
        "comment (optional)",
        editor.comment(),
        editor.input_comment(),
        width,
    )
}

pub(super) fn freeform_rows(editor: &DecisionEditor, width: usize) -> Vec<String> {
    input_rows(
        "freeform",
        editor.freeform(),
        editor.input_freeform(),
        width,
    )
}

fn input_rows(label: &str, value: &str, focused: bool, width: usize) -> Vec<String> {
    let rows = layout::wrapped_rows(
        &format!("{}{label}: {value}", if focused { "> " } else { "" }),
        "",
        layout::content_width(width),
    );
    layout::card(width, &rows, focused)
}

pub(super) fn editor_footer(editor: &DecisionEditor, multiple: bool) -> Vec<String> {
    let decision = editor.decision();
    let action = if decision.require_confirmation {
        "review"
    } else {
        "submit"
    };
    let mut rows = vec![modal::footer(&format!(
        "↑↓: move  {}Enter: {action}  Esc: back",
        if multiple { "Space: check  " } else { "" }
    ))];
    let tabs = if decision.allow_comment {
        if decision.allow_freeform {
            "Tab: choices/comment/freeform"
        } else {
            "Tab: choices/comment"
        }
    } else if multiple && decision.allow_freeform {
        "Tab: choices/freeform"
    } else {
        ""
    };
    let count = if multiple {
        let count = decision
            .options
            .iter()
            .filter(|option| editor.option_checked(&option.id))
            .count();
        let (min, max) = decision.selection_bounds();
        format!("{count} selected ({min}-{max})  ")
    } else {
        String::new()
    };
    rows.push(modal::footer(&format!("{count}{tabs} PgUp/PgDn: scroll")));
    rows
}

pub(super) fn confirmation_body(
    editor: &DecisionEditor,
    answer: &UserDecisionAnswer,
    width: usize,
    capacity: usize,
) -> Vec<String> {
    let mut rows = wrapped_content_lines("Review answer", "", width)
        .into_iter()
        .map(|line| Role::Accent.style().bold().paint(&line))
        .collect::<Vec<_>>();
    rows.extend(wrapped_content_lines(&editor.decision().title, "", width));
    rows.push(String::new());
    match answer {
        UserDecisionAnswer::Option { option_id, .. } => {
            add_choice(&mut rows, editor, option_id, width);
        }
        UserDecisionAnswer::Options { option_ids, .. } => {
            for id in option_ids {
                add_choice(&mut rows, editor, id, width);
            }
        }
        UserDecisionAnswer::Freeform { text } => {
            add_card(&mut rows, text, "Answer: ", width);
        }
    }
    if let Some(comment) = answer.comment() {
        add_card(&mut rows, comment, "Comment: ", width);
    }
    if let Some(error) = editor.error() {
        rows.extend(
            wrapped_content_lines(error.message.as_str(), "", width)
                .into_iter()
                .map(|line| Role::Danger.style().paint(&line)),
        );
    }
    let (start, end) = editor.scroll_offset().map_or_else(
        || {
            let start = if editor.error().is_some() {
                rows.len().saturating_sub(capacity)
            } else {
                0
            };
            (start, start.saturating_add(capacity).min(rows.len()))
        },
        |offset| layout::manual_window(rows.len(), offset, capacity),
    );
    let mut body = modal::scroll_window(&rows, start, end);
    body.push(modal::footer("Enter: send  Esc: edit  PgUp/PgDn: scroll"));
    body
}

fn add_choice(rows: &mut Vec<String>, editor: &DecisionEditor, id: &str, width: usize) {
    let label = editor
        .decision()
        .options
        .iter()
        .find(|option| option.id == id)
        .map_or(id, |option| option.label.as_str());
    add_card(rows, &format!("{label} [{id}]"), "Choice: ", width);
}

fn add_card(rows: &mut Vec<String>, text: &str, prefix: &str, width: usize) {
    rows.extend(layout::card(
        width,
        &layout::wrapped_rows(text, prefix, layout::content_width(width)),
        false,
    ));
}
