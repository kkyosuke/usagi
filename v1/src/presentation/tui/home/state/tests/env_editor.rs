//! Tests for the workspace-env editor overlay (the `env` command) on
//! [`HomeState`]: opening it over the palette, editing the buffer, and confirming
//! or cancelling the parsed bindings.

use super::*;
use crate::domain::settings::SecretEnv;

fn bindings(pairs: &[(&str, &str)]) -> SecretEnv {
    pairs
        .iter()
        .map(|(n, r)| (n.to_string(), r.to_string()))
        .collect()
}

#[test]
fn env_editor_accessors_are_none_until_opened() {
    let mut state = state();
    assert!(state.env_editor().is_none());
    assert!(state.env_editor_mut().is_none());
    // Confirming with no editor open leaves the (empty) overlay untouched.
    assert!(state.confirm_env_editor().is_none());
}

#[test]
fn open_env_editor_seeds_the_buffer_from_the_bindings_in_sorted_order() {
    let mut state = state();
    state.open_env_editor(bindings(&[
        ("B_TOKEN", "op://v/i/b"),
        ("A_TOKEN", "op://v/i/a"),
    ]));
    let editor = state.env_editor().expect("editor open");
    assert_eq!(
        editor.area().lines(),
        &[
            "A_TOKEN=op://v/i/a".to_string(),
            "B_TOKEN=op://v/i/b".to_string()
        ]
    );
    // The caret opens at the end of the buffer.
    assert_eq!(editor.area().cursor(), (1, "B_TOKEN=op://v/i/b".len()));
}

#[test]
fn editing_then_confirming_returns_the_parsed_bindings_and_closes() {
    let mut state = state();
    state.open_env_editor(SecretEnv::new());
    // Type a valid binding plus a malformed line (dropped on confirm).
    let area = state.env_editor_mut().expect("editor open").area_mut();
    for c in "GH_TOKEN=op://v/i/t".chars() {
        area.insert(c);
    }
    area.newline();
    for c in "no_equals".chars() {
        area.insert(c);
    }

    let env = state
        .confirm_env_editor()
        .expect("confirm returns bindings");
    assert_eq!(env.get("GH_TOKEN").map(String::as_str), Some("op://v/i/t"));
    assert_eq!(env.len(), 1);
    // Confirming closed the overlay.
    assert!(state.env_editor().is_none());
}

#[test]
fn cancelling_the_env_editor_discards_the_buffer() {
    let mut state = state();
    state.open_env_editor(bindings(&[("A", "op://v/i/a")]));
    state.env_editor_mut().unwrap().area_mut().insert('X');
    state.env_editor_cancel();
    assert!(state.env_editor().is_none());
}
