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
    // `:` palette. Measured at a width wide enough for the full footer — at a
    // narrow width the lowest-priority trailing keys are elided (see
    // `footer_elides_to_fit_a_narrow_terminal`).
    let switch = footer_line(200, &state);
    assert!(switch.contains("switch"));
    assert!(switch.contains("x close tab"));
    assert!(switch.contains(": commands"));
    state.enter_focus(1);
    let focus = footer_line(80, &state);
    assert!(focus.contains("session: main"));
    assert!(footer_line(200, &state).contains(": commands"));
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
fn focus_footer_reflects_the_prefix_leader_and_scheme() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    // Under the prefix scheme 在席 advertises the same `Ctrl-O` leader as 没入.
    let idle = footer_line(120, &state);
    assert!(idle.contains("Ctrl-O then"));
    // While the leader is pending the footer flips to the waiting hint, naming how
    // to back out — mirroring 没入.
    state.set_prefix_pending(true);
    let waiting = footer_line(120, &state);
    assert!(!waiting.contains("Ctrl-O then"));
    assert!(waiting.contains("Ctrl-O ▸"));
    assert!(waiting.contains("Esc cancel"));
    // The alt scheme keeps `Ctrl-O` a direct zoom-out here, so the footer names it
    // plainly with no leader (and no pending state applies).
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    let alt = footer_line(120, &state);
    assert!(alt.contains("Ctrl-O: switch"));
    assert!(!alt.contains("Ctrl-O then"));
    assert!(!alt.contains("Ctrl-O ▸"));
}

#[test]
fn attached_prefix_footer_flips_to_the_waiting_hint_while_a_leader_is_pending() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.show_attached();
    // Idle, the footer advertises the leader sequence ("Ctrl-O then: …") and the
    // close-tab key among the actions it leads to.
    assert!(footer_line(200, &state).contains("Ctrl-O then"));
    assert!(footer_line(200, &state).contains("x close"));
    // Once the leader is pressed the footer flips to the waiting hint, so a
    // Ctrl-O that drew no visible response reads as "waiting" not "ignored", and
    // names how to back out (`Esc cancel` is the lowest-priority trailing key, so
    // it needs a width wide enough to keep the whole footer).
    state.set_prefix_pending(true);
    let waiting = footer_line(200, &state);
    assert!(!waiting.contains("Ctrl-O then"));
    assert!(waiting.contains("Ctrl-O ▸"));
    assert!(waiting.contains("Esc cancel"));
    // The Alt scheme has no pending state, so the hint never applies there.
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    assert!(footer_line(200, &state).contains("Alt:"));
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
fn footer_and_palette_scope_to_the_cursor_group_in_unite_mode() {
    let mut state = state_with_sessions(&["main"]);
    // Single-workspace: the footer carries no scope tag (there is no ambiguity).
    assert!(!footer_line(200, &state).contains(" · usagi"));

    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![SessionRecord {
            name: "b1".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);

    // Flat rows: 0 usagi root, 1 main, 2 wsB root, 3 b1. The switch footer names
    // the cursor group's workspace inside its mode tag, so it survives elision.
    state.switch_select(1); // primary session
    assert!(footer_line(200, &state).contains("switch · usagi"));
    state.switch_select(3); // extra group session
    assert!(footer_line(200, &state).contains("switch · wsB"));

    // The `:` palette input line names the same scope, so a `config` / `issue` run
    // from the palette shows which workspace it targets.
    state.open_command_palette();
    let frame = stripped(&render_frame(40, 100, &state));
    assert!(frame.contains("[wsB]"));
}

#[test]
fn footer_elides_to_fit_a_narrow_terminal() {
    // The switch footer spells out every key and is far wider than an 80-column
    // terminal; it must be trimmed to fit (never overrun the row), while keeping
    // the leading mode tag and the highest-priority keys and marking the drop
    // with `…`.
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    let width = 80;
    let footer = footer_line(width, &state);
    assert!(
        console::measure_text_width(&console::strip_ansi_codes(&footer)) <= width,
        "footer overruns {width} cols: {footer:?}",
    );
    let plain = console::strip_ansi_codes(&footer);
    // The mode tag and the first keys survive …
    assert!(plain.contains("[switch]"));
    assert!(plain.contains("↑↓ session"));
    // … the low-priority tail is dropped, marked with an ellipsis.
    assert!(plain.contains('…'));
    assert!(!plain.contains("Esc back"));
}

#[test]
fn switch_footer_advertises_backing_out_while_the_note_shows() {
    // The read-only note overlay does not capture `Esc` (it follows the cursor
    // instead of being dismissed), so the footer keeps advertising the back-out
    // even while a note is showing.
    let state = switch_state_with_note("todo");
    let footer = footer_line(200, &state);
    assert!(footer.contains("Esc back"));
    assert!(!footer.contains("close note"));
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
