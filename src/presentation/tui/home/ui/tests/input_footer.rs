use super::*;

#[test]
fn command_palette_renders_the_prompt() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    state.push_char('m');
    let frame = stripped(&render_frame(40, 80, &state));
    assert!(frame.contains('m'));
    assert!(frame.contains('❯'));
    // The palette is titled and footers its keys.
    assert!(frame.contains("Command"));
    assert!(frame.contains("Esc: close"));
}

#[test]
fn command_palette_draws_the_caret_without_shifting_the_text() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.cursor_left();
    // The block caret recolours the character it sits on rather than inserting a
    // glyph, so the text reads intact whatever the caret position. (Where the
    // reverse-video cell lands is covered by `widgets::block_caret`'s own tests.)
    let plain = stripped(&render_frame(40, 80, &state));
    assert!(plain.contains("❯ man"));
}

#[test]
fn command_palette_shows_hints_and_the_latest_response() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    // A bare prompt lists the workspace commands as hints.
    let listed = stripped(&render_frame(40, 80, &state));
    assert!(listed.contains("workspace commands"));
    // After running a command its response shows in the band.
    state.open_command_palette();
    for c in "history".chars() {
        state.push_char(c);
    }
    let _ = state.submit();
    let ran = stripped(&render_frame(40, 80, &state));
    // The seeded usage hint is part of the response band's tail.
    assert!(ran.contains("man"));
}

#[test]
fn command_palette_caps_a_long_response_with_a_more_line() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    // Seed more than the cap response lines, so the band shows only the tail
    // with an `↑ N more` summary above it.
    for i in 0..20 {
        state.log_output(format!("out {i}"));
    }
    let frame = stripped(&render_frame(40, 80, &state));
    assert!(frame.contains("more"), "the overflow is summarised");
    // The newest lines stay in view; the oldest are elided.
    assert!(frame.contains("out 19"));
    assert!(!frame.contains("out 0\n"));
}

#[test]
fn command_palette_keeps_a_constant_height_and_position() {
    // The box must not jump as its content changes — a fixed-height modal centred
    // over a constant-height frame keeps the same rows whether it shows a bare
    // hint list or a long response (no layout shift while typing / running).
    let border_rows = |f: &[String]| -> (Option<usize>, Option<usize>) {
        let top = f
            .iter()
            .position(|r| console::strip_ansi_codes(r).contains('┌'));
        let bottom = f
            .iter()
            .position(|r| console::strip_ansi_codes(r).contains('└'));
        (top, bottom)
    };

    let mut bare = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    bare.open_command_palette();
    let empty = render_frame(40, 80, &bare);

    let mut busy = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    busy.open_command_palette();
    for i in 0..20 {
        busy.log_output(format!("line {i}"));
    }
    let full = render_frame(40, 80, &busy);

    let (top, bottom) = border_rows(&empty);
    assert!(top.is_some(), "the box renders");
    assert_eq!(top, border_rows(&full).0, "the top border never moves");
    assert_eq!(
        bottom,
        border_rows(&full).1,
        "the bottom border never moves",
    );
}

#[test]
fn command_palette_floats_over_the_visible_workspace() {
    // Unlike a full-screen modal, the palette is composited over the live frame,
    // so the workspace shows around it instead of a black backdrop.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_command_palette();
    state.push_char('m');
    let frame = render_frame(40, 80, &state);
    let joined = stripped(&frame);
    // The palette box and its prompt are drawn …
    assert!(joined.contains('┌'));
    assert!(joined.contains("❯ m"));
    // … and the workspace chrome behind it stays visible (the mode ladder and the
    // workspace title), which a black full-screen modal would have hidden.
    assert!(joined.contains("Switch"), "the mode ladder shows behind");
    assert!(joined.contains("usagi"), "the workspace title shows behind");
    // A row above the box still carries workspace content (it is not blanked).
    let top = frame
        .iter()
        .position(|r| console::strip_ansi_codes(r).contains('┌'))
        .expect("the box renders");
    assert!(
        (0..top).any(|r| !console::strip_ansi_codes(&frame[r]).trim().is_empty()),
        "the workspace shows above the floating box",
    );
}

#[test]
fn text_modal_floats_over_the_visible_workspace() {
    // The text modal (`man` / `history` / `session list` output) is composited
    // over the live frame like the `:` palette, so the workspace shows around it
    // instead of a black backdrop.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_text_modal(
        "Help",
        vec![LogLine::output("man — show this help")],
        ModalSize::Normal,
    );
    let frame = render_frame(40, 80, &state);
    let joined = stripped(&frame);
    // The modal box and its content are drawn …
    assert!(joined.contains('┌'));
    assert!(joined.contains("Help"));
    assert!(joined.contains("man — show this help"));
    // … and the workspace chrome behind it stays visible (the mode ladder and the
    // workspace title), which a black full-screen modal would have hidden.
    assert!(joined.contains("Switch"), "the mode ladder shows behind");
    assert!(joined.contains("usagi"), "the workspace title shows behind");
    // A row above the box still carries workspace content (it is not blanked).
    let top = frame
        .iter()
        .position(|r| console::strip_ansi_codes(r).contains('┌'))
        .expect("the box renders");
    assert!(
        (0..top).any(|r| !console::strip_ansi_codes(&frame[r]).trim().is_empty()),
        "the workspace shows above the floating box",
    );
}

#[test]
fn input_line_differs_by_mode() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert!(input_line(&state).contains("Pick a session"));
    state.enter_focus(1);
    assert!(input_line(&state).contains("Operating session: main"));
    state.show_attached();
    assert!(input_line(&state).contains("live terminal"));
}

#[test]
fn footer_line_differs_by_mode() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    // The default mode is 切替; its footer advertises the close-tab key and the
    // `:` palette.
    let switch = footer_line(120, &state);
    assert!(switch.contains("switch"));
    assert!(switch.contains("x close tab"));
    assert!(switch.contains(": commands"));
    state.enter_focus(1);
    let focus = footer_line(80, &state);
    assert!(focus.contains("session: main"));
    assert!(footer_line(120, &state).contains(": commands"));
    state.show_attached();
    // 没入 no longer advertises scroll keys in the footer; by default it names the
    // Ctrl-O prefix sequence.
    let attached = footer_line(80, &state);
    assert!(attached.contains("attached"));
    assert!(!attached.contains("scroll"));
    assert!(attached.contains("Ctrl-O then"));
    // The Alt scheme names the Alt-chords instead.
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    let alt = footer_line(80, &state);
    assert!(alt.contains("Alt:"));
    assert!(!alt.contains("Ctrl-O then"));
}

#[test]
fn attached_prefix_footer_flips_to_the_waiting_hint_while_a_leader_is_pending() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.show_attached();
    // Idle, the footer advertises the leader sequence ("Ctrl-O then: …").
    assert!(footer_line(80, &state).contains("Ctrl-O then"));
    // Once the leader is pressed the footer flips to the waiting hint, so a
    // Ctrl-O that drew no visible response reads as "waiting" not "ignored", and
    // names how to back out.
    state.set_prefix_pending(true);
    let waiting = footer_line(80, &state);
    assert!(!waiting.contains("Ctrl-O then"));
    assert!(waiting.contains("Ctrl-O ▸"));
    assert!(waiting.contains("Esc cancel"));
    // The Alt scheme has no pending state, so the hint never applies there.
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    assert!(footer_line(80, &state).contains("Alt:"));
}

#[test]
fn switch_footer_reflects_the_waiting_first_sort_toggle() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    // Off by default, the footer offers the toggle plainly.
    let off = footer_line(120, &state);
    assert!(off.contains("s sort"));
    assert!(!off.contains("s sort:on"));
    // On, the footer marks it active.
    state.toggle_sort_waiting();
    assert!(footer_line(120, &state).contains("s sort:on"));
}

#[test]
fn footer_line_shows_palette_controls_while_open() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_command_palette();
    let footer = footer_line(80, &state);
    assert!(footer.contains("command"));
    assert!(footer.contains("Esc: close"));
}

#[test]
fn switch_footer_advertises_closing_the_note_while_it_shows() {
    // While the highlighted session's note is showing, `Esc` first closes it, so
    // the footer names that instead of the back-out it normally advertises.
    let mut state = switch_state_with_note("todo");
    assert!(
        footer_line(120, &state).contains("Esc close note"),
        "the footer offers closing the note"
    );
    // Dismissed, `Esc` backs out again — the footer reverts.
    state.hide_switch_note();
    let backed = footer_line(120, &state);
    assert!(backed.contains("Esc back"));
    assert!(!backed.contains("close note"));
}

#[test]
fn render_frame_honours_the_rail_in_switch_too() {
    let mut state = state_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    state.set_sidebar(Sidebar::Rail);
    // 切替 (the default) honours the rail: collapsed, the session name is not
    // spelled out on the left, but the active entry still rides in the title bar
    // (`▸`). The cursored root previews `workspace root` with no `feature` name.
    let rail = stripped(&render_frame(24, 80, &state));
    assert!(rail.contains('▸'));
    assert!(!rail.contains("feature"));
    // The picker keeps working collapsed (the cursor lives on the rail).
    let rail_switch = stripped(&render_frame(24, 80, &state));
    assert!(!rail_switch.contains("feature"));
    // Expanding the sidebar (Ctrl-B) brings the names back inline in the picker.
    state.toggle_sidebar();
    let full_switch = stripped(&render_frame(24, 80, &state));
    assert!(full_switch.contains("feature"));
}

#[test]
fn switch_create_on_the_rail_renders_the_input_in_the_right_pane() {
    let mut state = state_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    state.set_sidebar(Sidebar::Rail);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    // The rail is too narrow for the inline `+ new: …` row, so opening the create
    // input moves it into the (wide) right pane with its own header and key hint.
    state.switch_begin_create(Vec::new());
    state.create_mut().unwrap().push_char('x');
    let frame = stripped(&render_frame(24, 80, &state));
    assert!(frame.contains("+ new session"));
    assert!(frame.contains('x'));
    assert!(frame.contains("Enter 作成"));
    // The left-pane inline form (`+ new:`) is not used while collapsed.
    assert!(!frame.contains("+ new:"));
    // A live validation error replaces the dim hint below the box in place.
    state.create_mut().unwrap().push_char('/');
    let invalid = stripped(&render_frame(24, 80, &state));
    assert!(invalid.contains("path separators"));
}

#[test]
fn switch_rename_on_the_rail_renders_the_input_in_the_right_pane() {
    let mut state = state_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    state.set_sidebar(Sidebar::Rail);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    // Move the cursor off the root onto the session, then rename it: collapsed to
    // the rail the input takes over the right pane just like create does.
    state.switch_move_down();
    assert!(state.switch_begin_rename());
    state.rename_mut().unwrap().push_char('z');
    let frame = stripped(&render_frame(24, 80, &state));
    assert!(frame.contains("rename feature"));
    assert!(frame.contains('z'));
    assert!(frame.contains("Enter 確定"));
}

#[test]
fn switch_create_with_the_full_sidebar_stays_inline_on_the_left() {
    let mut state = state_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    state.set_sidebar(Sidebar::Full);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_begin_create(Vec::new());
    let frame = stripped(&render_frame(24, 80, &state));
    // Full width keeps the original inline left-pane form, not the right-pane box.
    assert!(frame.contains("+ new:"));
    assert!(!frame.contains("+ new session"));
}

#[test]
fn mode_ladder_lists_every_step_and_keeps_them_for_each_mode() {
    for mode in [Mode::Switch, Mode::Focus, Mode::Attached] {
        let ladder = console::strip_ansi_codes(&mode_ladder(80, mode)).into_owned();
        for step in ["Switch", "Focus", "Attached"] {
            assert!(ladder.contains(step), "{mode:?} ladder missing {step}");
        }
        // 統括 (Overview) is gone from the ladder.
        assert!(!ladder.contains("Overview"));
    }
}

#[test]
fn command_palette_frame_is_a_bordered_box() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    for c in "session".chars() {
        state.push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // The palette is a framed modal (top/bottom borders) carrying the prompt.
    assert!(joined.contains('┌'));
    assert!(joined.contains('└'));
    assert!(joined.contains("❯ session"));
}
