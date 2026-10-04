//! Comment entry and answer review rendering.
use super::{Role, controls, layout, modal, widgets, wrapped_content_lines};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::user_decision::UserDecisionAnswer;

pub(super) fn comment_rows(editor: &DecisionEditor, width: usize) -> Vec<String> {
    input_rows(
        "comment (optional)",
        editor.comment(),
        editor.input_comment(),
        if editor.input_comment() {
            "Comment · Editing"
        } else if editor.input_freeform() {
            "Comment · Not included with freeform"
        } else {
            "Comment · Optional note for selected choice"
        },
        "Add a note to your selected choice",
        width,
    )
}

pub(super) fn freeform_rows(editor: &DecisionEditor, width: usize) -> Vec<String> {
    input_rows(
        "freeform",
        editor.freeform(),
        editor.input_freeform(),
        if editor.input_freeform() {
            "Freeform · Editing"
        } else {
            "Freeform · Alternative answer"
        },
        "Move down to write your own answer",
        width,
    )
}

fn input_rows(
    label: &str,
    value: &str,
    focused: bool,
    title: &str,
    placeholder: &str,
    width: usize,
) -> Vec<String> {
    let content_width = layout::content_width(width);
    let mut rows = layout::wrapped_rows(
        &format!(
            "{}{label}: {}",
            if focused { "> " } else { "" },
            if value.is_empty() && !focused {
                placeholder
            } else {
                value
            },
        ),
        "",
        content_width,
    );
    if focused {
        let tail = rows.pop().unwrap_or_default();
        let style = crate::presentation::theme::editor_surface_style();
        rows = rows.into_iter().map(|row| style.paint(&row)).collect();
        rows.extend(widgets::wrap_with_trailing_caret(
            &tail,
            content_width,
            &style,
        ));
        for row in &mut rows {
            row.push_str(
                &style
                    .paint(&" ".repeat(content_width.saturating_sub(widgets::display_width(row)))),
            );
        }
    } else {
        rows = rows
            .into_iter()
            .map(|row| super::Style::new().dim().paint(&row))
            .collect();
    }
    layout::titled_card(width, &rows, title, focused)
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
            add_card(&mut rows, text, "Answer: ", "Freeform answer", width);
        }
    }
    if let Some(comment) = answer.comment() {
        add_card(&mut rows, comment, "Comment: ", "Included comment", width);
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
    body.extend(controls::review_footer(answer, width));
    body
}

fn add_choice(rows: &mut Vec<String>, editor: &DecisionEditor, id: &str, width: usize) {
    let label = editor
        .decision()
        .options
        .iter()
        .find(|option| option.id == id)
        .map_or(id, |option| option.label.as_str());
    add_card(
        rows,
        &format!("{label} [{id}]"),
        "Choice: ",
        "Selected answer",
        width,
    );
}

fn add_card(rows: &mut Vec<String>, text: &str, prefix: &str, title: &str, width: usize) {
    rows.extend(layout::titled_card(
        width,
        &layout::wrapped_rows(text, prefix, layout::content_width(width)),
        title,
        false,
    ));
}
