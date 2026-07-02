//! Event-loop tests for the `env` command's workspace-env editor overlay: it
//! opens over the command palette, edits the `NAME=op://…` buffer, and on
//! `Ctrl-S` writes the bindings to the workspace's local settings (returning to
//! the Overview), or on `Esc` cancels without saving.

use super::*;

/// Keys that open `:env`, edit the buffer to a single valid binding while
/// exercising every editing / caret arm, then whatever `tail` requests.
fn env_session(tail: Vec<io::Result<Key>>) -> Vec<io::Result<Key>> {
    let mut keys = cmd("env");
    keys.push(Ok(Key::Enter)); // run `:env` → overlay opens over the palette
    keys.extend(typed("GH_TOKEN=op://v/i/tZ"));
    keys.push(Ok(Key::Backspace)); // drop the trailing Z → …/t
    keys.push(Ok(Key::Del)); // at end of line: no-op (covers the Del arm)
    keys.push(Ok(Key::Home));
    keys.push(Ok(Key::End));
    keys.push(Ok(Key::ArrowLeft));
    keys.push(Ok(Key::ArrowRight));
    keys.push(Ok(Key::Enter)); // newline → empty second line
    keys.push(Ok(Key::ArrowUp)); // back to the binding line
    keys.push(Ok(Key::ArrowDown)); // to the empty line
    keys.push(Ok(Key::Tab)); // an unhandled key inside the editor is ignored
    keys.extend(tail);
    keys
}

#[test]
fn env_command_opens_an_overlay_and_ctrl_s_saves_the_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let keys = env_session(vec![
        Ok(Key::Char(CTRL_S)), // save → back to the palette
        Ok(Key::Escape),       // close the palette → base Switch
        Ok(Key::Escape),       // inert at the base Switch
        Ok(Key::CtrlC),        // quit
    ]);
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
    // The one valid binding was written to the workspace's local settings; the
    // malformed empty line was dropped.
    let saved = crate::usecase::settings::load_local(dir.path()).unwrap();
    assert_eq!(
        saved.env.get("GH_TOKEN").map(String::as_str),
        Some("op://v/i/t")
    );
    assert_eq!(saved.env.len(), 1);
}

#[test]
fn escaping_the_env_overlay_discards_the_edits() {
    let dir = tempfile::tempdir().unwrap();
    let keys = env_session(vec![
        Ok(Key::Escape), // cancel the editor → back to the palette (no save)
        Ok(Key::Escape), // close the palette → base Switch
        Ok(Key::CtrlC),  // quit
    ]);
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
    // Nothing was persisted.
    let saved = crate::usecase::settings::load_local(dir.path()).unwrap();
    assert!(saved.env.is_empty());
}

#[test]
fn a_failing_env_save_is_reported_and_the_loop_continues() {
    // A workspace root that is a *file* makes `save_local` fail (it cannot create
    // the `.usagi` directory under it), exercising the save-error branch.
    let file = tempfile::NamedTempFile::new().unwrap();
    let keys = env_session(vec![
        Ok(Key::Char(CTRL_S)), // save fails → error logged, editor closes
        Ok(Key::Escape),       // close the palette → base Switch
        Ok(Key::CtrlC),        // quit
    ]);
    assert!(matches!(
        run_at(keys, sample_state(), file.path()).unwrap(),
        Outcome::Quit
    ));
}
