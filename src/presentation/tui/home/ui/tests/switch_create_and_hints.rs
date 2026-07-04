use super::*;

#[test]
fn switch_create_rows_show_the_input_and_an_error() {
    // Caret at the end of the name: the whole name precedes it, and the block
    // caret adds one trailing cell.
    let rows = switch_create_rows("wip", 3, None, 30);
    assert_eq!(rows.len(), 1);
    let plain = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(plain.contains("+ new: wip"));
    assert!(
        plain.starts_with("  +"),
        "input mode keeps the + aligned with the persistent create row: {plain:?}"
    );

    // Caret in the middle: the block caret sits on a character, so the name reads
    // intact without an inserted glyph.
    let mid = switch_create_rows("wip", 2, None, 30);
    let plain_mid = console::strip_ansi_codes(&mid[0]).into_owned();
    assert!(plain_mid.contains("+ new: wip"));

    let with_error = switch_create_rows("feature", 7, Some("\"feature\" already exists."), 40);
    assert_eq!(with_error.len(), 2);
    let err = console::strip_ansi_codes(&with_error[1]).into_owned();
    assert!(err.contains("already exists"));
    assert!(err.starts_with("  "));
}

#[test]
fn render_frame_shows_the_inline_create_row_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("+ new: wip"));
    assert!(joined.contains("switch"));
}

#[test]
fn render_frame_inserts_the_inline_create_row_before_the_next_unite_group() {
    let mut state = state_with_sessions(&["a1"]);
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![SessionRecord {
            name: "b1".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_mut().unwrap().push_char(c);
    }

    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    let a1 = joined.find("a1").unwrap();
    let create = joined.find("+ new: wip").unwrap();
    let ws_b = joined.find("▌ wsB").unwrap();
    assert!(a1 < create);
    assert!(create < ws_b);
}

#[test]
fn render_frame_reuses_the_unite_gap_for_inline_create_without_shifting_lower_workspaces() {
    let mut state = state_with_sessions(&["a1"]);
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![SessionRecord {
            name: "b1".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    let before = render_frame(24, 80, &state)
        .iter()
        .position(|line| console::strip_ansi_codes(line).contains("▌ wsB"))
        .unwrap();

    state.switch_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let plain: Vec<_> = frame
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    let after = plain
        .iter()
        .position(|line| line.contains("▌ wsB"))
        .unwrap();

    assert_eq!(after, before, "lower workspace header must not shift");
    assert!(plain[after - 2].contains("+ new: wip"));
}

#[test]
fn splice_rows_inserts_inside_an_existing_column_without_replacing_rows() {
    let mut column = vec!["a".to_string(), "d".to_string()];

    splice_rows(&mut column, 1, vec!["b".to_string(), "c".to_string()]);

    assert_eq!(column, ["a", "b", "c", "d"]);
}

#[test]
fn render_frame_edits_the_selected_row_name_in_place_when_renaming_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down(); // cursor onto "main"
    assert!(state.switch_begin_rename());
    for c in " 2".chars() {
        state.rename_mut().unwrap().push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let plain: Vec<String> = frame
        .iter()
        .map(|row| console::strip_ansi_codes(row).into_owned())
        .collect();
    // The selected session's own name line becomes the editable label in place:
    // the `󰤇` marker and the typed "main 2" (prefilled "main", then edited) sit
    // on the same row — the rename is not banished to a separate row at the list foot.
    assert!(plain
        .iter()
        .any(|line| line.contains('󰤇') && line.contains("main 2")));
    // And the old foot-anchored `rename <target>:` affordance is gone.
    assert!(!plain.iter().any(|line| line.contains("rename main:")));
}

// --- command hints (command palette) -----------------------------------

#[test]
fn command_hint_row_emphasises_the_typed_prefix_and_marks_the_selection() {
    let hint = CommandHint {
        name: "session",
        description: "Create, list, or switch sessions",
    };
    let selected = command_hint_row(&hint, 3, true, 80);
    let plain = console::strip_ansi_codes(&selected).into_owned();
    assert!(plain.contains('›'));
    assert!(plain.contains("session"));
    assert!(plain.contains("Create, list"));
    let plain = console::strip_ansi_codes(&command_hint_row(&hint, 0, false, 80)).into_owned();
    assert!(!plain.contains('›'));
}

#[test]
fn command_hint_row_clips_a_long_description_to_width() {
    let hint = CommandHint {
        name: "session",
        description: "A very long description that should be cut down to fit the pane width",
    };
    let row = command_hint_row(&hint, 0, false, 30);
    assert!(console::measure_text_width(&row) <= 30);
    assert!(console::strip_ansi_codes(&row).contains('…'));
}

#[test]
fn hint_lines_are_empty_while_the_palette_is_closed() {
    let mut state = HomeState::new(
        "usagi",
        vec![worktree(Some("m"), true, BranchStatus::Local)],
        None,
    );
    // 在席 with the palette closed: no hints.
    state.enter_focus(1);
    assert!(hint_lines(&state, 80).is_empty());
    // The default base 切替 likewise has no hints until the palette opens.
    let closed = HomeState::new("usagi", Vec::new(), None);
    assert!(hint_lines(&closed, 80).is_empty());
}

#[test]
fn hint_lines_list_every_command_for_a_bare_prompt() {
    let state = typing("");
    let joined = stripped(&hint_lines(&state, 80));
    assert!(joined.contains("commands"));
    assert!(!joined.contains('›'));
    // The bare prompt lists the first `HINT_MAX` workspace commands (alphabetical
    // registry order) then folds the rest into an overflow line, so an early
    // command shows while the tail (`session`, `unite`, …) is summarised.
    assert!(joined.contains("more"));
    assert!(joined.contains("config"));
    assert!(!joined.contains("session"));
}

#[test]
fn hint_lines_highlight_the_best_match_while_typing() {
    let state = typing("s");
    let joined = stripped(&hint_lines(&state, 80));
    assert!(joined.contains("matches"));
    assert!(joined.contains('›'));
    assert!(joined.contains("session"));
    assert!(!joined.contains("more"));
}

#[test]
fn hint_lines_show_usage_and_examples_for_arguments() {
    let state = typing("session ");
    let joined = stripped(&hint_lines(&state, 80));
    assert!(joined.contains("usage"));
    assert!(joined.contains("session [create"));
    assert!(joined.contains("e.g."));
    assert!(joined.contains("session create"));
}

#[test]
fn hint_lines_show_usage_without_examples_when_a_command_has_none() {
    let state = typing("doctor ");
    let joined = stripped(&hint_lines(&state, 80));
    assert!(joined.contains("usage"));
    assert!(joined.contains("doctor"));
    assert!(!joined.contains("e.g."));
}

#[test]
fn hint_lines_are_empty_for_an_unknown_command() {
    assert!(hint_lines(&typing("frobnicate "), 80).is_empty());
    assert!(hint_lines(&typing("zzz"), 80).is_empty());
}

#[test]
fn render_frame_shows_command_hints_in_the_palette_and_keeps_its_height() {
    // The hints render inside the `:` palette modal (typing opens it).
    let state = typing("s");
    let frame = render_frame(24, 80, &state);
    assert_eq!(frame.len(), 24);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("matches"));
    assert!(joined.contains("session"));
}

#[test]
fn tab_menu_box_and_rename_body_render_action_surface() {
    let mut state = state_with(vec![]);
    state.open_tab_menu(PathBuf::from("/repo/main"), 1, "terminal", 12, 3);
    let menu = state.tab_menu().unwrap();
    let menu_text = stripped(&tab_menu_box(menu));
    assert!(menu_text.contains("tab 2"));
    assert!(menu_text.contains("Move left"));
    assert!(menu_text.contains("Move right"));
    assert!(menu_text.contains("Rename"));
    assert!(menu_text.contains("Close"));

    let body = tab_rename_body("term", 2, 30);
    let text = stripped(&body);
    assert!(text.contains("Rename tab label"));
    assert!(text.contains("label:"));
    assert!(text.contains("term"));
    assert!(text.contains("Enter save"));
}
