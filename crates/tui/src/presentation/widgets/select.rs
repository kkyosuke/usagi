//! A compact selectable value row shared by form-like views.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;

/// Fixed label column used by Config selects so their value controls align.
const LABEL_WIDTH: usize = 13;
/// Fixed value column so changing `dark` to `light` does not move a centred row.
const VALUE_WIDTH: usize = 6;

/// Render a labelled select row. The selected value is bracketed so a static
/// frame remains understandable without colour. Focus uses the accent colour,
/// while an unsaved value uses the warning colour across its whole row.
#[must_use]
pub fn render(label: &str, value: &str, focused: bool, changed: bool) -> String {
    let marker = modal::selection_marker(focused);
    let changed_marker = if changed {
        Role::Warning.style().bold().paint("●")
    } else {
        " ".to_string()
    };
    let style = if changed {
        Role::Warning.style().bold()
    } else if focused {
        Role::Accent.style().bold()
    } else {
        Style::new()
    };
    let label = style.paint(&format!("{label:<LABEL_WIDTH$}"));
    let control = style.paint(&format!("< {value:<VALUE_WIDTH$} >"));
    format!("{marker} {changed_marker} {label}{control}")
}

/// Render an unavailable select value as a non-focusable, dimmed row.
#[must_use]
pub fn disabled(label: &str, value: &str) -> String {
    Style::new().dim().paint(&format!(
        "    {label:<LABEL_WIDTH$}< {value:<VALUE_WIDTH$} >"
    ))
}

/// Render a form action. Disabled actions remain visible but dimmed.
#[must_use]
pub fn action(label: &str, focused: bool, enabled: bool) -> String {
    let marker = modal::selection_marker(focused);
    let style = if enabled {
        Role::Success.style().bold()
    } else {
        Style::new().dim()
    };
    format!("{marker}   {}", style.paint(&format!("[ {label} ]")))
}

#[cfg(test)]
mod tests {
    use super::{action, disabled, render};

    #[test]
    fn select_and_action_expose_focus_change_and_enabled_state() {
        let changed = render("Modal mode", "action", true, true);
        assert!(changed.contains("›") && changed.contains("●") && changed.contains("Modal mode"));
        assert!(changed.contains("\u{1b}[1;33m"));
        assert!(render("Theme", "system", false, false).contains("Theme"));
        assert!(action("Save", true, true).contains("\u{1b}[1;32m[ Save ]"));
        assert!(action("Save", false, false).contains("[ Save ]"));
        assert!(disabled("Agent model", "none").contains("\u{1b}[2m"));
    }

    #[test]
    fn values_start_in_the_same_column_for_different_label_widths() {
        let theme = render("Theme", "dark", false, false);
        let mode = render("Modal mode", "action", false, false);
        assert_eq!(theme.find('<'), mode.find('<'));
        assert!(theme.contains("< dark   >"));
        assert!(mode.contains("< action >"));
    }
}
