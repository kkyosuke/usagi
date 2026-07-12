//! Rendering tests for the workspace-env editor overlay (the `env` command): it
//! floats over the palette as a fixed-height box, windows around the caret, and
//! marks empty non-cursor rows with a placeholder.

use super::*;
use crate::domain::settings::SecretEnv;

fn env(pairs: &[(&str, &str)]) -> SecretEnv {
    pairs
        .iter()
        .map(|(n, r)| (n.to_string(), r.to_string()))
        .collect()
}

/// A home state with the palette open and the env editor floating over it,
/// seeded from `bindings`.
fn env_state(bindings: &[(&str, &str)]) -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.open_command_palette();
    state.open_env_editor(env(bindings));
    state
}

/// Count the box rows (lines containing a vertical border) so the modal's height
/// can be compared regardless of vertical centring padding.
fn box_row_count(frame: &[String]) -> usize {
    frame.iter().filter(|l| l.contains('│')).count()
}

#[test]
fn env_overlay_shows_the_bindings_title_and_format_hint() {
    let state = env_state(&[("GH_TOKEN", "op://Private/GitHub/token")]);
    let frame = render_frame(24, 80, &state);
    let joined = stripped(&frame);
    assert!(joined.contains("Env Vars"));
    assert!(joined.contains("op://vault/item/field"));
    assert!(joined.contains("GH_TOKEN=op://Private/GitHub/token"));
}

#[test]
fn env_overlay_keeps_a_constant_height_as_lines_are_added() {
    let empty = render_frame(24, 80, &env_state(&[]));
    let few = render_frame(
        24,
        80,
        &env_state(&[("A", "op://v/i/a"), ("B", "op://v/i/b")]),
    );
    let many: Vec<(&str, &str)> = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L"]
        .iter()
        .map(|n| (*n, "op://v/i/x"))
        .collect();
    let full = render_frame(24, 80, &env_state(&many));
    assert_eq!(box_row_count(&empty), box_row_count(&few));
    assert_eq!(box_row_count(&empty), box_row_count(&full));
}

#[test]
fn env_overlay_scrolls_to_keep_the_caret_row_visible() {
    // With more bindings than the window, the editor scrolls so the last (caret)
    // line stays rendered while the first drops out of view.
    let bindings: Vec<(&str, &str)> = ["AA", "BB", "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ"]
        .iter()
        .map(|n| (*n, "op://v/i/x"))
        .collect();
    let joined = stripped(&render_frame(24, 80, &env_state(&bindings)));
    assert!(joined.contains("JJ=op://v/i/x"));
    assert!(!joined.contains("AA=op://v/i/x"));
}

#[test]
fn env_overlay_marks_empty_non_cursor_rows_with_a_placeholder() {
    // A blank binding line that is not the cursor row renders with the "·"
    // placeholder. Open on one binding, add a blank line, then move the caret back
    // up so the blank line is non-cursor.
    let mut state = env_state(&[("A", "op://v/i/a")]);
    let area = state.env_editor_mut().unwrap().area_mut();
    area.move_end();
    area.newline();
    area.move_up();
    let joined = stripped(&render_frame(24, 80, &state));
    assert!(joined.contains('·'));
}
