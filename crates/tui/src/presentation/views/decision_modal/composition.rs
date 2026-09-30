//! Comment entry and answer review rendering.
use super::{modal, wrapped_content_lines};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::user_decision::UserDecisionAnswer;

pub(super) fn comment_rows(editor: &DecisionEditor, width: usize) -> Vec<String> {
    wrapped_content_lines(
        &format!(
            "{}comment (optional): {}",
            if editor.input_comment() { "> " } else { "" },
            editor.comment()
        ),
        "",
        width,
    )
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
    let mut rows = wrapped_content_lines("Review answer", "", width);
    rows.extend(wrapped_content_lines(&editor.decision().title, "", width));
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
            rows.extend(wrapped_content_lines(text, "Answer: ", width));
        }
    }
    if let Some(comment) = answer.comment() {
        rows.extend(wrapped_content_lines(comment, "Comment: ", width));
    }
    if let Some(error) = editor.error() {
        rows.extend(wrapped_content_lines(error.message.as_str(), "", width));
    }
    let offset = editor.scroll_offset().unwrap_or_else(|| {
        if editor.error().is_some() {
            rows.len()
        } else {
            0
        }
    });
    let start = offset.min(rows.len().saturating_sub(capacity));
    let end = (start + capacity).min(rows.len());
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
    rows.extend(wrapped_content_lines(
        &format!("{label} [{id}]"),
        "Choice: ",
        width,
    ));
}
