//! A compact selectable value row shared by form-like views.

use crate::presentation::theme::Style;

/// Render a labelled select row. The selected value is bracketed so a static
/// frame remains understandable without colour, while focus adds emphasis.
#[must_use]
pub fn render(label: &str, value: &str, focused: bool, changed: bool) -> String {
    let marker = if focused { "›" } else { " " };
    let changed = if changed { "●" } else { " " };
    let style = if focused {
        Style::new().bold()
    } else {
        Style::new()
    };
    let control = style.paint("‹›");
    format!("{marker} {changed} {label}: {value}  {control}")
}

/// Render a form action. Disabled actions remain visible but dimmed.
#[must_use]
pub fn action(label: &str, focused: bool, enabled: bool) -> String {
    let marker = if focused { "›" } else { " " };
    let style = if enabled {
        if focused {
            Style::new().bold()
        } else {
            Style::new()
        }
    } else {
        Style::new().dim()
    };
    format!("{marker}   {}", style.paint(&format!("[ {label} ]")))
}

#[cfg(test)]
mod tests {
    use super::{action, render};

    #[test]
    fn select_and_action_expose_focus_and_change_state_without_colour() {
        assert!(render("Modal mode", "Action", true, true).contains("› ● Modal mode"));
        assert!(render("Theme", "System", false, false).contains("Theme: System"));
        assert!(action("Save", true, true).contains("[ Save ]"));
        assert!(action("Save", false, false).contains("[ Save ]"));
    }
}
