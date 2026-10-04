//! Bordered decision sections and focus-following viewports.

use std::ops::Range;

use super::{Role, modal, widgets};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::presentation_text::sanitize_presentation_line;

pub(super) fn content_width(inner_width: usize) -> usize {
    inner_width.saturating_sub(modal::BODY_INDENT_WIDTH + border_rows(inner_width) * 4)
}

pub(super) fn border_rows(inner_width: usize) -> usize {
    usize::from(inner_width >= modal::BODY_INDENT_WIDTH + 6)
}

/// Prefixes may contain trusted styles; measure their terminal cells, not bytes.
pub(super) fn wrapped_rows(text: &str, prefix: &str, width: usize) -> Vec<String> {
    // On narrow terminals, shrink the indent before sacrificing the content.
    let prefix = if widgets::display_width(prefix) + 2 > width {
        prefix.trim_start()
    } else {
        prefix
    };
    let prefix = widgets::clip_to_width(prefix, width.saturating_sub(2));
    let prefix_width = widgets::display_width(&prefix);
    let continuation = " ".repeat(prefix_width);
    text.split('\n')
        .flat_map(|line| {
            let mut wrapped = widgets::wrap_to_width(
                &sanitize_presentation_line(line),
                width.saturating_sub(prefix_width),
            );
            if wrapped.is_empty() {
                wrapped.push(String::new());
            }
            wrapped.into_iter().enumerate().map(|(index, segment)| {
                let indent = if index == 0 { &prefix } else { &continuation };
                widgets::clip_to_width(&format!("{indent}{segment}"), width)
            })
        })
        .collect()
}

pub(super) fn heading_rows(text: &str, prefix: &str, width: usize) -> Vec<String> {
    let prefix = widgets::clip_to_width(prefix, width.saturating_sub(2));
    let indent = " ".repeat(widgets::display_width(&prefix));
    wrapped_rows(text, &indent, width)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let marker = if index == 0 { &prefix } else { &indent };
            format!(
                "{marker}{}",
                super::Style::new().bold().paint(&line[indent.len()..])
            )
        })
        .collect()
}

/// Keep neutral outlines visible; accent the whole outline of the focused section.
pub(super) fn card(inner_width: usize, rows: &[String], focused: bool) -> Vec<String> {
    if border_rows(inner_width) == 0 {
        return rows
            .iter()
            .map(|line| modal::content_line(line, inner_width))
            .collect();
    }
    let border = if focused {
        Role::Accent.style().bold()
    } else {
        super::Style::new()
    };
    let frame = modal::compact_boxed("", content_width(inner_width), rows);
    frame
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let line = if index == 0 || index + 1 == frame.len() {
                border.paint(line)
            } else {
                // Style the edges separately so a styled label's reset cannot
                // erase the focus outline on the right side.
                format!(
                    "{}{}{}",
                    border.paint("│"),
                    &line['│'.len_utf8()..line.len() - '│'.len_utf8()],
                    border.paint("│")
                )
            };
            modal::content_line(&line, inner_width)
        })
        .collect()
}

/// Show a complete card when it fits, or its top when it does not. A one-row
/// viewport prioritizes the label or input tail over a decorative border.
pub(super) fn focus_window(
    len: usize,
    focus: Range<usize>,
    anchor: usize,
    capacity: usize,
) -> (usize, usize) {
    let visible = len.min(capacity);
    let start = if focus.len() <= visible {
        focus.end.saturating_sub(visible)
    } else if visible == 1 {
        anchor
    } else {
        focus.start
    }
    .min(len.saturating_sub(visible));
    (start, start + visible)
}

/// A page must not skip content when the viewport holds fewer than eight rows.
pub(super) fn manual_window(len: usize, offset: usize, capacity: usize) -> (usize, usize) {
    let offset = (offset / DecisionEditor::SCROLL_STEP)
        .saturating_mul(capacity.min(DecisionEditor::SCROLL_STEP));
    let start = offset.min(len.saturating_sub(capacity));
    (start, start.saturating_add(capacity).min(len))
}
