//! Durable user-decision list and answer editor overlays.

#![coverage(off)] // ANSI-only composition over the shared modal primitive; state transitions are covered in the controller.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DecisionOverlayState;

const INNER_WIDTH: usize = 62;
const BODY_HEIGHT: usize = 16;
// Leave room for the persistent footer and for a scroll indicator above and
// below the viewport.  This keeps every decision field reachable even when a
// prompt, option label, or description spans many rows.
const CONTENT_CAPACITY: usize = BODY_HEIGHT - 4;

fn wrapped_content_lines(text: &str, prefix: &str, inner_width: usize) -> Vec<String> {
    let width = inner_width.saturating_sub(modal::BODY_INDENT_WIDTH);
    let continuation = " ".repeat(prefix.len());
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut wrapped = widgets::wrap_to_width(line, width.saturating_sub(prefix.len()));
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        for (index, segment) in wrapped.into_iter().enumerate() {
            let indent = if index == 0 { prefix } else { &continuation };
            rows.push(modal::content_line(
                &format!("{indent}{segment}"),
                inner_width,
            ));
        }
    }
    rows
}

fn wrapped_dim_lines(text: &str, prefix: &str, inner_width: usize) -> Vec<String> {
    wrapped_content_lines(text, prefix, inner_width)
        .into_iter()
        .map(|line| Style::new().dim().paint(&line))
        .collect()
}

fn editor_body(
    editor: &crate::usecase::application::controller::DecisionEditor,
    inner_width: usize,
) -> Vec<String> {
    let decision = editor.decision();
    let mut rows = Vec::new();
    rows.extend(
        wrapped_content_lines(&decision.title, "", inner_width)
            .into_iter()
            .map(|line| Role::Accent.style().bold().paint(&line)),
    );
    rows.extend(wrapped_content_lines(&decision.prompt, "", inner_width));
    if let Some(deadline) = decision.expires_at {
        rows.push(modal::caption(&format!(
            "expires: {}",
            deadline.format("%Y-%m-%d %H:%M UTC")
        )));
    }
    rows.push(String::new());

    let mut selected_row = rows.len();
    for (index, option) in decision.options.iter().enumerate() {
        if index == editor.selected_option() {
            selected_row = rows.len();
        }
        let marker = format!(
            "{} ",
            modal::selection_marker(index == editor.selected_option())
        );
        rows.extend(wrapped_content_lines(&option.label, &marker, inner_width));
        if let Some(description) = &option.description {
            rows.extend(wrapped_dim_lines(description, "     ", inner_width));
        }
    }
    if decision.allow_freeform {
        rows.push(String::new());
        rows.extend(wrapped_content_lines(
            &format!("freeform: {}", editor.freeform()),
            "",
            inner_width,
        ));
    }
    if let Some(error) = editor.error() {
        rows.extend(
            wrapped_content_lines(error.message.as_str(), "", inner_width)
                .into_iter()
                .map(|line| Role::Danger.style().paint(&line)),
        );
    }

    let (start, end) = editor.scroll_offset().map_or_else(
        || modal::list_window(rows.len(), selected_row, CONTENT_CAPACITY),
        |offset| {
            let start = offset.min(rows.len().saturating_sub(CONTENT_CAPACITY));
            let end = start.saturating_add(CONTENT_CAPACITY).min(rows.len());
            (start, end)
        },
    );
    let mut body = modal::scroll_window(&rows, start, end);
    body.push(modal::footer(
        "↑↓: select  PgUp/PgDn: scroll  Enter: submit  Esc: back",
    ));
    body
}

fn list_body(
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
    inner_width: usize,
) -> Vec<String> {
    let mut rows = vec![modal::caption("Pending decisions for this workspace")];
    if decisions.is_empty() {
        rows.push(modal::empty_notice("(none)"));
    }
    let mut selected_row = rows.len();
    for (index, decision) in decisions.iter().enumerate() {
        if index == overlay.selected() {
            selected_row = rows.len();
        }
        let marker = format!("{} ", modal::selection_marker(index == overlay.selected()));
        let session = decision
            .owner
            .session_id
            .as_ref()
            .map_or_else(|| "workspace root".to_owned(), ToString::to_string);
        rows.extend(wrapped_content_lines(
            &format!("{session}: {}", decision.title),
            &marker,
            inner_width,
        ));
    }
    let (start, end) = modal::list_window(rows.len(), selected_row, CONTENT_CAPACITY);
    let mut body = modal::scroll_window(&rows, start, end);
    body.push(String::new());
    body.push(modal::footer("↑↓: select   Enter: open   Esc: close"));
    body
}

/// Render either the workspace pending list or the selected decision editor.
#[must_use]
#[coverage(off)] // Pure ANSI composition follows the shared modal primitive; controller/render integration is covered by workspace views.
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
) -> Vec<String> {
    let inner_width = modal::modal_inner_width(width, INNER_WIDTH);
    let (title, body) = if let Some(editor) = overlay.editor() {
        ("User decision", editor_body(editor, inner_width))
    } else {
        (
            "Pending decisions",
            list_body(overlay, decisions, inner_width),
        )
    };
    modal::render_over(
        height,
        width,
        base,
        title,
        inner_width,
        &modal::fixed_body(body, BODY_HEIGHT),
    )
}
