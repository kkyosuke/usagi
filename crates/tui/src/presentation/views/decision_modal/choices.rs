//! Answer selection remains visible while the optional comment owns focus.

use super::{Role, layout, modal, widgets};
use crate::usecase::application::controller::DecisionEditor;
use usagi_core::domain::user_decision::UserDecisionSelectionMode;

pub(super) fn rows(editor: &DecisionEditor, index: usize, width: usize) -> Vec<String> {
    let decision = editor.decision();
    let option = &decision.options[index];
    let multiple = decision.selection_mode == UserDecisionSelectionMode::Multiple;
    let focused =
        index == editor.selected_option() && !editor.input_freeform() && !editor.input_comment();
    let selected = if multiple {
        editor.option_checked(&option.id)
    } else {
        index == editor.selected_option() && !editor.input_freeform()
    };
    let indicator = if multiple {
        if selected { "[x]" } else { "[ ]" }
    } else if selected {
        "●"
    } else {
        "○"
    };
    let marker = format!(
        "{} {} ",
        modal::selection_marker(focused),
        if selected {
            Role::Success.style().bold().paint(indicator)
        } else {
            indicator.to_owned()
        }
    );
    let label = if decision
        .recommendation
        .as_ref()
        .is_some_and(|rec| rec.option_ids.contains(&option.id))
    {
        format!("{} [recommended]", option.label)
    } else {
        option.label.clone()
    };
    let content_width = layout::content_width(width);
    let mut rows = layout::styled_heading_rows(
        &label,
        &marker,
        content_width,
        if focused {
            Role::Accent.style().bold().reverse()
        } else {
            super::Style::new().bold()
        },
    );
    let indent = " ".repeat(widgets::display_width(&marker));
    if let Some(description) = &option.description {
        rows.extend(layout::wrapped_rows(description, &indent, content_width));
    }
    for (prefix, points) in [("Pro: ", &option.pros), ("Con: ", &option.cons)] {
        for point in points {
            rows.extend(layout::wrapped_rows(
                point,
                &format!("{indent}{prefix}"),
                content_width,
            ));
        }
    }
    let title = if multiple && focused {
        if selected {
            "Selected · Space to uncheck"
        } else {
            "Focused · Space to select"
        }
    } else if selected {
        "Selected"
    } else {
        "Option"
    };
    layout::titled_card(width, &rows, title, focused)
}
