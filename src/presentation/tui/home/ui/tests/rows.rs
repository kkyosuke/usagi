use super::*;
use crate::presentation::theme::Palette;
use console::Style;

#[test]
fn text_modal_body_windows_a_long_dump_with_more_counts() {
    let lines: Vec<LogLine> = (0..30)
        .map(|i| LogLine::output(format!("entry {i}")))
        .collect();
    let modal = TextModal {
        title: "Help".to_string(),
        lines,
        scroll: 5,
        size: ModalSize::Normal,
    };
    let body = stripped(&text_modal_body(&modal, 60, TEXT_MODAL_VISIBLE));
    // The hidden-line counts above and below the window show.
    assert!(body.contains("↑ 5 more"));
    // 30 total - (scroll 5 + 16 visible) = 9 hidden below.
    assert!(body.contains("↓ 9 more"));
    // A windowed line is visible; ones outside the window are not.
    assert!(body.contains("entry 5"));
    assert!(!body.contains("entry 0"));
    assert!(body.contains("Esc / Enter / q: close"));
}

#[test]
fn text_modal_geometry_sizes_by_modal_size() {
    // A compact modal is the fixed inner width / visible-line count, regardless
    // of the terminal.
    assert_eq!(
        text_modal_geometry(40, 90, ModalSize::Normal),
        (TEXT_MODAL_INNER, TEXT_MODAL_VISIBLE),
    );
    // A large modal scales to the terminal (matching the widget geometry).
    let large = text_modal_geometry(40, 90, ModalSize::Large);
    assert_eq!(large.0, 90 - 8);
    assert_eq!(large.1, 40 - 8);
    // It is wider and taller than the compact modal on a roomy screen.
    assert!(large.0 > TEXT_MODAL_INNER);
    assert!(large.1 > TEXT_MODAL_VISIBLE);
}

#[test]
fn text_modal_body_shows_a_short_dump_without_scroll_counts() {
    let modal = TextModal {
        title: "History".to_string(),
        lines: vec![LogLine::output("  1  man"), LogLine::output("  2  history")],
        scroll: 0,
        size: ModalSize::Normal,
    };
    let body = stripped(&text_modal_body(&modal, 60, TEXT_MODAL_VISIBLE));
    assert!(body.contains("man"));
    assert!(!body.contains("more"));
}

#[test]
fn clip_to_width_keeps_short_text() {
    assert_eq!(clip_to_width("main", 10), "main");
}

#[test]
fn clip_to_width_truncates_with_an_ellipsis() {
    let clipped = clip_to_width("feature/long", 5);
    assert_eq!(console::measure_text_width(&clipped), 5);
    assert!(clipped.ends_with('…'));
}

#[test]
fn clip_to_width_with_zero_budget_is_empty() {
    assert_eq!(clip_to_width("main", 0), "");
}

#[test]
fn clip_to_width_counts_wide_glyphs_as_two_columns() {
    // Each full-width character is two display columns, so a 5-column budget fits
    // two of them plus the ellipsis (2 + 2 + 1).
    let clipped = clip_to_width("あいうえお", 5);
    assert_eq!(console::measure_text_width(&clipped), 5);
    assert_eq!(clipped, "あい…");
}

#[test]
fn clip_to_width_keeps_ansi_escapes_without_counting_them() {
    // A red-coloured "hello" (literal SGR escapes, since `console::style` emits
    // none without a TTY) carries sequences of zero display width: the clip
    // measures only the visible text and copies the escapes verbatim, so the
    // result keeps the colour, stays exactly the budget wide, and keeps "he" —
    // the escapes never eat into the three-column budget. Because it carried a
    // style across the cut, the tail is closed with a reset (after the ellipsis)
    // so the colour cannot bleed into what follows.
    let styled = "\x1b[31mhello\x1b[0m";
    let clipped = clip_to_width(styled, 3);
    assert_eq!(console::measure_text_width(&clipped), 3);
    assert!(clipped.starts_with("\x1b[31m"));
    assert!(clipped.contains("he"));
    assert!(clipped.ends_with("\x1b[0m"));
    assert!(clipped.contains('…'));
}

#[test]
fn clip_to_width_leaves_styled_text_within_budget_untouched() {
    // Escapes do not count toward the width, so a short styled string fits the
    // budget and is returned whole (the early `measure_text_width` path).
    let styled = "\x1b[32mok\x1b[0m";
    assert_eq!(clip_to_width(styled, 5), styled);
}

#[test]
fn pad_to_width_fills_short_content() {
    assert_eq!(pad_to_width("ab".to_string(), 5), "ab   ");
}

#[test]
fn pad_to_width_leaves_full_content_alone() {
    assert_eq!(pad_to_width("abcde".to_string(), 5), "abcde");
}

#[test]
fn layout_splits_a_standard_width() {
    let (left, right) = layout(80, Sidebar::Full);
    assert_eq!(left, 26);
    assert_eq!(right, 80 - 26 - SEP_WIDTH);
}

#[test]
fn layout_does_not_overrun_a_narrow_terminal() {
    let (left, right) = layout(4, Sidebar::Full);
    assert!(left <= 4);
    assert_eq!(right, 0);
}

#[test]
fn layout_collapses_to_the_rail_width() {
    // A collapsed sidebar is the fixed-width rail, handing the rest to the right
    // pane regardless of how wide the full sidebar would have been.
    let (left, right) = layout(80, Sidebar::Rail);
    assert_eq!(left, RAIL_WIDTH);
    assert_eq!(right, 80 - RAIL_WIDTH - SEP_WIDTH);
    // The rail's right pane is wider than the full sidebar's would be.
    let (_, full_right) = layout(80, Sidebar::Full);
    assert!(right > full_right);
}

#[test]
fn title_bar_singular_and_plural() {
    let one = title_bar(80, &list_with(vec![]));
    assert!(one.contains("usagi"));
    assert!(one.contains("1 session"));
    assert!(!one.contains("1 sessions"));
    let three = title_bar(
        80,
        &list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("x"), false, BranchStatus::Local),
        ]),
    );
    assert!(three.contains("3 sessions"));
}

#[test]
fn title_bar_names_the_union_and_counts_workspaces_in_unite_mode() {
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let bar = console::strip_ansi_codes(&title_bar(120, &list)).into_owned();
    assert!(bar.contains("unite"));
    assert!(bar.contains("across 2 workspaces"));
    // 2 roots + 2 sessions = 4 rows.
    assert!(bar.contains("4 sessions across"));
}

#[test]
fn title_bar_shows_the_active_session_name() {
    // With nothing activated the root row is active, so the workspace itself is
    // named — keeping the active entry identifiable even when the sidebar is the
    // collapsed rail (which shows no names).
    let mut list = list_with(vec![worktree(
        Some("feat-login"),
        true,
        BranchStatus::Local,
    )]);
    let root = console::strip_ansi_codes(&title_bar(80, &list)).into_owned();
    assert!(root.contains(&format!("▸ {ROOT_NAME}")));
    // Activating a session names it in the title.
    list.move_down();
    list.activate_selected();
    let active = console::strip_ansi_codes(&title_bar(80, &list)).into_owned();
    assert!(active.contains("▸ feat-login"));
}

#[test]
fn title_bar_keeps_the_centre_fixed_as_the_active_session_changes() {
    // The active-session-name field is pinned to a width that depends only on
    // the terminal size, so switching between a short and a long name leaves the
    // centred title at the same column (the bar never shifts sideways).
    let mut short = list_with(vec![worktree(Some("x"), true, BranchStatus::Local)]);
    short.move_down();
    short.activate_selected();
    let mut long = list_with(vec![worktree(
        Some("a-much-longer-session-name"),
        true,
        BranchStatus::Local,
    )]);
    long.move_down();
    long.activate_selected();

    let lead = |list: &_| {
        let line = console::strip_ansi_codes(&title_bar(80, list)).into_owned();
        line.len() - line.trim_start().len()
    };
    assert_eq!(lead(&short), lead(&long));
}

#[test]
fn status_label_pairs_a_git_icon_with_each_word() {
    for (status, icon, word) in [
        (BranchStatus::New, NEW_ICON, "new"),
        (BranchStatus::Dirty, DIRTY_ICON, "dirty"),
        (BranchStatus::Local, LOCAL_ICON, "local"),
        (BranchStatus::Pushed, PUSHED_ICON, "pushed"),
        (BranchStatus::Synced, SYNCED_ICON, "synced"),
    ] {
        let plain = console::strip_ansi_codes(&status_label(status)).into_owned();
        assert!(plain.contains(icon), "{plain:?} missing its icon");
        assert!(plain.contains(word), "{plain:?} missing its word");
        // The icon leads the word: `<icon> <word>`.
        assert_eq!(plain, format!("{icon} {word}"));
    }
}

#[test]
fn worktree_row_marks_the_selected_session_in_switch_and_shows_detached() {
    // The selected session in 切替 (Switch) uses the one-line usagi glyph on line
    // 1 and a vertical continuation on line 2. (The kind dot reflects freshness —
    // a just-built fixture is fresh `●`; heat fading is covered in its own test.)
    let (top, detail) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        true,
        false,
        true,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(top.contains('󰤇'));
    assert!(detail.contains('▎'));
    assert!(!top.contains('>'));
    assert!(top.contains('●'));
    assert!(top.contains("main"));

    // The same selected row outside Switch keeps the session marker so it remains
    // visible after the side menu has selected the session.
    let (top_no_switch, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        true,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(!top_no_switch.contains('>'));
    assert!(top_no_switch.contains('󰤇'));
    assert!(
        top_no_switch.starts_with(&Style::new().success().bold().apply_to("󰤇").to_string()),
        "selected marker after side-menu selection should be green: {top_no_switch:?}"
    );

    let (other_top, _) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        true,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(!other_top.contains('>'));
    assert!(!other_top.contains('󰤇'));
    assert!(other_top.contains('●'));
    assert!(other_top.contains("feature"));

    let (detached_top, _) = worktree_row(
        &worktree(None, false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(detached_top.contains("(detached)"));
}

#[test]
fn worktree_row_shows_a_memo_marker_only_when_the_session_has_a_note() {
    // A session carrying a note shows the memo glyph on line 1; one without does
    // not. The glyph sits in the cell between the name and the right-edge status.
    let with_note = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        true,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .0;
    let without_note = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .0;
    assert!(with_note.contains(NOTE_ICON));
    assert!(!without_note.contains(NOTE_ICON));
    // The marker is purely additive to line 1: its presence must not shift the row,
    // so both variants render the same display width.
    assert_eq!(
        console::measure_text_width(&console::strip_ansi_codes(&with_note)),
        console::measure_text_width(&console::strip_ansi_codes(&without_note)),
    );
}

#[test]
fn worktree_row_heat_dot_fades_with_time_since_touched() {
    // The kind dot is the session's freshness, measured against `now`: `●` within
    // the quarter-hour, `◐` within four hours, `○` older. Asserted on the stripped
    // row, with the worktree's `updated_at` set `ago` before a fixed `now`.
    let now = Utc::now();
    let dot = |ago: chrono::Duration| {
        let worktree = WorktreeState {
            updated_at: now - ago,
            ..worktree(Some("s"), false, BranchStatus::Local)
        };
        let (top, _) = worktree_row(
            &worktree,
            "",
            None,
            0,
            10,
            10,
            DetailCols::default(),
            false,
            now,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        );
        console::strip_ansi_codes(&top).into_owned()
    };
    // Just touched (and a clock that ran backwards) read as fresh.
    assert!(dot(chrono::Duration::minutes(1)).contains('●'));
    assert!(dot(chrono::Duration::minutes(-5)).contains('●'));
    // Within four hours, but not the quarter-hour: warm.
    assert!(dot(chrono::Duration::hours(1)).contains('◐'));
    // Older than the warm window: cold.
    assert!(dot(chrono::Duration::hours(48)).contains('○'));
}

#[test]
fn worktree_row_marks_the_active_worktree_with_a_gutter_bar_on_both_lines() {
    let (active_top, active_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        true,
        false,
        true,
        false,
        false,
        false,
        None,
    );
    // The green `▎` accent bar runs down both lines of the active row (the
    // detail line carries it too, to the left of the agent state).
    assert!(active_top.contains('▎'));
    assert!(active_detail.contains('▎'));
    // The old `*` marker is gone.
    assert!(!active_top.contains('*'));
    let (idle_top, idle_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        10,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(!idle_top.contains('▎'));
    assert!(!idle_detail.contains('▎'));
}

#[test]
fn worktree_row_shows_the_agent_state_through_its_lifecycle() {
    // A live session that has not begun a turn yet is idle: `☾` (word dropped).
    let (_, ready_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        12,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        true,
        false,
        false,
        false,
        None,
    );
    // Icon only: the phase glyph shows, the spelled-out word does not.
    assert!(ready_detail.contains('☾'));
    assert!(!ready_detail.contains("ready"));

    // Working a turn: `▶` (word dropped).
    let (_, running_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        12,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        true,
        true,
        false,
        false,
        None,
    );
    assert!(running_detail.contains('▶'));
    assert!(!running_detail.contains("running"));

    // Awaiting input wins over running: `◆` (word dropped).
    let (_, waiting_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        12,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        true,
        true,
        true,
        false,
        None,
    );
    assert!(waiting_detail.contains('◆'));
    assert!(!waiting_detail.contains('▶'));
    assert!(!waiting_detail.contains("waiting"));

    // A finished agent shows `✓` (word dropped), taking precedence over running.
    let (_, done_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        12,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        true,
        true,
        false,
        true,
        None,
    );
    assert!(done_detail.contains('✓'));
    assert!(!done_detail.contains("done"));
    assert!(!done_detail.contains('▶'));

    // No live session: the detail line carries no agent state.
    let (absent_top, absent_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        None,
        0,
        10,
        12,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(!absent_detail.contains('▶'));
    assert!(!absent_detail.contains('◆'));
    assert!(!absent_detail.contains('☾'));
    // The git status word no longer rides line 1; the branch name still does.
    assert!(!absent_top.contains("local"));
    assert!(absent_top.contains("feature"));
}

#[test]
fn worktree_row_truncates_a_long_branch() {
    let (top, _) = worktree_row(
        &worktree(
            Some("feature/a-very-long-branch-name"),
            false,
            BranchStatus::Local,
        ),
        "",
        None,
        0,
        7,
        8,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(top.contains('…'));
}

#[test]
fn worktree_row_shows_the_label_override_instead_of_the_branch() {
    let (top, _) = worktree_row(
        &worktree(Some("feat-login"), false, BranchStatus::Local),
        "Login flow",
        None,
        0,
        20,
        20,
        DetailCols::default(),
        false,
        Utc::now(),
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    );
    assert!(top.contains("Login flow"));
    assert!(!top.contains("feat-login"));
}

#[test]
fn root_row_marks_selected_and_active() {
    // The `>` cursor shows on the selected root only in 切替 (Switch).
    let (top, detail) = root_row(10, 0, 20, false, true, false, true);
    assert!(top.contains('>'));
    assert!(top.contains('⌂'));
    assert!(top.contains(ROOT_NAME));
    assert!(detail.contains("workspace root"));
    // The same selected root outside Switch shows no cursor.
    let (top_no_switch, _) = root_row(10, 0, 20, false, true, false, false);
    assert!(!top_no_switch.contains('>'));

    // The active root carries the green `▎` bar down both lines, not a `*`.
    let (active_top, active_detail) = root_row(10, 0, 20, false, false, true, false);
    assert!(active_top.contains('▎'));
    assert!(active_detail.contains('▎'));
    assert!(!active_top.contains('*'));

    let (idle_top, idle_detail) = root_row(10, 0, 20, false, false, false, false);
    assert!(!idle_top.contains('>'));
    assert!(!idle_top.contains('▎'));
    assert!(!idle_detail.contains('▎'));
    assert!(idle_top.contains(ROOT_NAME));
}

#[test]
fn root_row_shows_the_memo_marker_only_when_it_has_a_note() {
    // The root row carries its own note, like a session: the memo marker shows on
    // line 1 only when `has_note`, and is purely additive (the right-edge status
    // column does not shift), matching the worktree row.
    let (with_note, _) = root_row(10, 0, 20, true, false, false, false);
    let (without_note, _) = root_row(10, 0, 20, false, false, false, false);
    assert!(with_note.contains(NOTE_ICON));
    assert!(!without_note.contains(NOTE_ICON));
    assert_eq!(
        console::measure_text_width(&console::strip_ansi_codes(&with_note)),
        console::measure_text_width(&console::strip_ansi_codes(&without_note)),
    );
}

#[test]
fn left_pane_marks_the_root_row_when_it_carries_a_note() {
    let mut list = list_with(Vec::new());
    list.set_root_note_marker(true);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // Line 0 is the root row; with the marker set it shows the memo glyph.
    assert!(lines[0].contains(NOTE_ICON));
}

#[test]
fn left_pane_renders_the_root_entry_then_the_empty_message() {
    let lines = left_pane(
        &list_with(Vec::new()),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[1].contains("workspace root"));
    assert!(lines[2].contains('─'));
    assert!(console::strip_ansi_codes(&lines[3]).contains("+ new session"));
}

#[test]
fn left_pane_inserts_a_three_row_pending_session_above_the_create_row() {
    let mut state = state_with(Vec::new());
    state.set_root_path("/repo");
    state.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    let lines = left_pane(
        state.list(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        state.pending_sessions(),
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        8,
        true,
        Sidebar::Full,
        chrono::DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        None,
    );
    let name = console::strip_ansi_codes(&lines[3]);
    let detail = console::strip_ansi_codes(&lines[4]);
    let resource = console::strip_ansi_codes(&lines[5]);
    let create = console::strip_ansi_codes(&lines[6]);
    assert!(name.contains("newx"));
    // The detail row is blank height only; the resource row keeps the CPU/MEM
    // shape but shimmers instead of resting.
    assert!(!detail.contains("creating session"));
    assert!(detail.trim().is_empty());
    assert!(resource.contains("0%"));
    assert!(resource.contains("0MB"));
    assert!(create.contains("+ new session"));
}

#[test]
fn left_pane_inserts_the_pending_session_at_the_foot_with_a_session_present() {
    // A workspace that already has a session draws its "+ new session" row at the
    // foot; a pending create reserves a three-line skeleton immediately above it.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.set_root_path("/repo");
    state.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    let lines = left_pane(
        state.list(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        state.pending_sessions(),
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        10,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // Root(2) + divider(1) + session(3) = 6, so the pending skeleton is 6..=8
    // and the persistent create row remains selectable at line 9.
    assert!(console::strip_ansi_codes(&lines[6]).contains("newx"));
    assert!(console::strip_ansi_codes(&lines[7]).trim().is_empty()); // height-only detail row
    assert!(console::strip_ansi_codes(&lines[8]).contains("0MB"));
    assert!(console::strip_ansi_codes(&lines[9]).contains("+ new session"));
}

#[test]
fn rail_pending_session_rows_reserve_three_rows() {
    let rows = rail_pending_session_rows(0);
    assert_eq!(rows.len(), SESSION_ROWS);
    assert!(console::strip_ansi_codes(&rows[0]).contains('+'));
}

#[test]
fn rail_pane_inserts_three_pending_rows_before_the_create_slot() {
    // The rail draws only the pulsing `+` glyph (no room for the name), so it
    // keeps the same plain `+` affordance text while reserving the three rows a
    // real session will occupy before the still-selectable create slot.
    let render_rail = |state: &HomeState| {
        left_pane(
            state.list(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            state.pending_sessions(),
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            30,
            10,
            false,
            Sidebar::Rail,
            Utc::now(),
            None,
        )
    };

    // Empty workspace: the create slot sits under the pending skeleton.
    let mut empty = state_with(Vec::new());
    empty.set_root_path("/repo");
    empty.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    let empty_rail = render_rail(&empty);
    assert!(console::strip_ansi_codes(&empty_rail[3]).contains('+'));
    assert!(console::strip_ansi_codes(&empty_rail[6]).contains('+'));

    // Populated workspace: the create slot sits at the foot of the sessions.
    let mut full = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    full.set_root_path("/repo");
    full.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    let full_rail = render_rail(&full);
    assert!(console::strip_ansi_codes(&full_rail[6]).contains('+'));
    assert!(console::strip_ansi_codes(&full_rail[9]).contains('+'));
}

#[test]
fn left_pane_shows_each_sessions_relative_update_time_on_the_detail_line() {
    let mut w = worktree(Some("main"), true, BranchStatus::Pushed);
    w.updated_at = chrono::DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let now = chrono::DateTime::parse_from_rfc3339("2026-06-27T12:05:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let lines = left_pane(
        &list_with(vec![w]),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        5,
        false,
        Sidebar::Full,
        now,
        None,
    );
    // Root (2 lines) + divider + the session's 2 lines: the freshness label sits
    // on the session's detail line (index 4).
    let detail = console::strip_ansi_codes(&lines[4]);
    assert!(
        detail.contains("5m ago"),
        "{detail:?} missing the relative time"
    );
}

#[test]
fn left_pane_shows_the_ahead_behind_marker_on_the_detail_line() {
    let mut w = worktree(Some("feature"), false, BranchStatus::Local);
    w.ahead_behind = Some(crate::domain::workspace_state::AheadBehind {
        ahead: 2,
        behind: 1,
    });
    let lines = left_pane(
        &list_with(vec![w]),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        40,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // The session's detail line (index 4) carries the `↑N ↓M` commit marker.
    let detail = console::strip_ansi_codes(&lines[4]);
    assert!(
        detail.contains("↑2 ↓1"),
        "{detail:?} missing the ahead/behind marker"
    );
}

#[test]
fn left_pane_lines_the_detail_fields_up_across_sessions_of_different_sizes() {
    use crate::domain::workspace_state::{AheadBehind, DiffStat};
    let now = chrono::DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    // A tiny, just-touched session with a live agent…
    let mut small = worktree(Some("small"), false, BranchStatus::Local);
    small.path = PathBuf::from("/repo/small");
    small.updated_at = now - chrono::Duration::seconds(30);
    small.diff = Some(DiffStat {
        added: 5,
        removed: 3,
    });
    small.ahead_behind = Some(AheadBehind {
        ahead: 2,
        behind: 0,
    });
    // …beside a much larger, staler one with no agent. The change counts differ by
    // orders of magnitude and the "ago" labels differ in width.
    let mut big = worktree(Some("big"), false, BranchStatus::Local);
    big.path = PathBuf::from("/repo/big");
    big.updated_at = now - chrono::Duration::minutes(12);
    big.diff = Some(DiffStat {
        added: 140,
        removed: 88,
    });
    big.ahead_behind = Some(AheadBehind {
        ahead: 0,
        behind: 7,
    });
    let live = HashSet::from([PathBuf::from("/repo/small")]);
    let lines = left_pane(
        &list_with(vec![small, big]),
        &live,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        9,
        false,
        Sidebar::Full,
        now,
        None,
    );
    // Detail lines: small at index 4, big at index 7 (each entry spans three rows).
    let small_detail = console::strip_ansi_codes(&lines[4]);
    let big_detail = console::strip_ansi_codes(&lines[7]);
    assert!(small_detail.contains('☾')); // the live agent's icon (word dropped)
    assert!(!small_detail.contains("ready"));
    assert!(small_detail.contains("+  5 - 3")); // counts padded to the wide columns
    assert!(big_detail.contains("+140 -88"));
    assert!(big_detail.contains("12m ago"));
    // The diff `+` lands in the same painted column on both rows regardless of how
    // many changed lines each carries — the point of the fixed columns. Measured
    // CJK-aware (ambiguous glyphs = two columns), since that is the width the detail
    // line is laid out and the terminal paints it in.
    let col_of =
        |s: &str, ch: char| console::measure_text_width(&s[..s.find(ch).expect("char present")]);
    assert_eq!(col_of(&small_detail, '+'), col_of(&big_detail, '+'));
    // Both detail lines fill the same width, so the cluster's right edge lines up.
    assert_eq!(
        console::measure_text_width(&small_detail),
        console::measure_text_width(&big_detail)
    );
}

#[test]
fn left_pane_freshness_column_does_not_shift_the_detail_line_as_a_session_ages() {
    // A single running session in a pane just wide enough that the freshness label
    // sits at the trim boundary: when its width is sized to the live label, the
    // young `now` (3 cols) fits but the aged `12m ago` (7 cols) tips the cluster
    // over and drops the field — the detail line jumping purely because the clock
    // advanced. Reserving a constant width for the column decouples the decision
    // from the clock, so the same layout renders at every age.
    let base = chrono::DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut w = worktree(Some("feature"), false, BranchStatus::Local);
    w.path = PathBuf::from("/repo/feature");
    w.updated_at = base;
    let agent = HashSet::from([PathBuf::from("/repo/feature")]);
    let render_at = |now| {
        let lines = left_pane(
            &list_with(vec![w.clone()]),
            &agent, // live
            &agent, // running — the `▶` agent icon
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            18,
            5,
            false,
            Sidebar::Full,
            now,
            None,
        );
        console::strip_ansi_codes(&lines[4]).into_owned()
    };
    let young = render_at(base + chrono::Duration::seconds(30)); // `now`
    let aged = render_at(base + chrono::Duration::minutes(12)); // `12m ago`
    let shows_freshness = |detail: &str| detail.contains("ago") || detail.contains("now");
    assert_eq!(
        shows_freshness(&young),
        shows_freshness(&aged),
        "the freshness field must not appear/disappear as the session ages:\n\
         young={young:?}\naged={aged:?}"
    );
    // The running agent's icon keeps the same room at both ages.
    assert!(young.contains('▶') && aged.contains('▶'));
}

#[test]
fn left_pane_renders_the_root_entry_then_one_entry_per_worktree() {
    let list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        9,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // Root (2 lines), a divider, then 3 lines per worktree (identity, detail,
    // resource).
    assert_eq!(lines.len(), 9);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains("main"));
    assert!(lines[6].contains("feature"));
}

#[test]
fn left_pane_in_unite_mode_heads_each_workspace_with_its_name() {
    // Two stacked workspaces (統合): each gets a name header, its own root row, a
    // divider, and its sessions, in the flat order the cursor navigates.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        40,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    let rendered = stripped(&lines);
    // Both workspace names head their blocks (the unite header bar), and both
    // groups' sessions render below their own root.
    assert!(rendered.contains("▌ wsA"));
    assert!(rendered.contains("▌ wsB"));
    assert!(rendered.contains("a1"));
    assert!(rendered.contains("b1"));
    // The header precedes its workspace's session, and a two-row blank gap makes
    // the next workspace's boundary obvious.
    let a_header = rendered.find("▌ wsA").unwrap();
    let b_header = rendered.find("▌ wsB").unwrap();
    assert!(a_header < rendered.find("a1").unwrap());
    assert!(a_header < b_header);
    let plain_lines: Vec<_> = lines
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    // wsA's own "+ new session" row sits at the foot of its block (line 7), then
    // the two-row gap (8-9), then wsB's header (10).
    assert!(plain_lines[7].contains("+ new session"));
    assert!(plain_lines[8].trim().is_empty());
    assert!(plain_lines[9].trim().is_empty());
    assert!(plain_lines[10].contains("▌ wsB"));
}

#[test]
fn rail_pane_in_unite_mode_separates_each_workspace() {
    // The collapsed rail stacks both workspaces too: two root entries, separated
    // by two blank rows, with each group's session below its root.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        40,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    let plain_lines: Vec<_> = lines
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    // wsA's rail create row sits at the foot of its block (line 6), then the
    // two-row unite gap (7-8) separates the two workspaces.
    assert!(plain_lines[6].contains('+'));
    assert!(plain_lines[7].trim().is_empty());
    assert!(plain_lines[8].trim().is_empty());
    assert!(!stripped(&lines).contains('━'));
}

#[test]
fn left_pane_in_unite_mode_shows_an_empty_workspace_create_row() {
    // A stacked workspace with no sessions still shows its header, root, divider,
    // and create row under it.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new("wsB", Vec::new()),
    ]);
    let full = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        40,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    let rendered = stripped(&full);
    assert!(rendered.contains("a1")); // wsA's session
    assert!(rendered.contains("▌ wsB")); // the empty workspace still gets a header
    assert!(rendered.contains("+ new session")); // and a create row
    assert!(!rendered.contains("no sessions"));
}

fn unite_pair() -> WorktreeList {
    WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ])
}

fn full_left_pane(list: &WorktreeList, sidebar: Sidebar) -> Vec<String> {
    left_pane(
        list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        40,
        false,
        sidebar,
        Utc::now(),
        None,
    )
}

#[test]
fn left_pane_folds_a_collapsed_workspace_to_a_single_header_line() {
    let mut list = unite_pair();
    list.toggle_collapsed(0); // fold wsA
    let rendered = stripped(&full_left_pane(&list, Sidebar::Full));
    // wsA collapses to a header carrying its session count; a1 is hidden.
    assert!(rendered.contains("▸ wsA  (1)"));
    assert!(!rendered.contains("a1"));
    // wsB stays expanded and shows its session and its own create row.
    assert!(rendered.contains("b1"));
    assert!(rendered.contains("+ new session"));
}

#[test]
fn rail_pane_folds_a_collapsed_workspace_to_a_marker_line() {
    let mut list = unite_pair();
    list.toggle_collapsed(0); // fold wsA
    let lines = full_left_pane(&list, Sidebar::Rail);
    let plain: Vec<_> = lines
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    // The folded workspace is a single ▸ marker line at the top of the rail.
    assert!(plain[0].contains('▸'));
    // Below the two-row gap, wsB's rail entry still shows its status glyphs.
    assert!(plain.iter().any(|l| l.contains('○') || l.contains('●')));
}

fn switch_left_pane(list: &WorktreeList, sidebar: Sidebar) -> Vec<String> {
    left_pane(
        list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        40,
        true, // in 切替
        sidebar,
        Utc::now(),
        None,
    )
}

#[test]
fn a_folded_workspace_dims_when_it_is_not_the_selected_row_in_switch() {
    let mut list = unite_pair();
    list.toggle_collapsed(0); // fold wsA
    list.focus_index(1); // cursor on wsB's root, so wsA's folded row is unselected
                         // Full sidebar: the folded header still renders (dimmed) while wsB is selected.
    let full = switch_left_pane(&list, Sidebar::Full);
    assert!(stripped(&full).contains("▸ wsA  (1)"));
    // Rail: the folded marker still renders (dimmed) at the top.
    let rail = switch_left_pane(&list, Sidebar::Rail);
    let plain0 = console::strip_ansi_codes(&rail[0]).into_owned();
    assert!(plain0.contains('▸'));
}

#[test]
fn sidebar_row_at_line_maps_a_folded_workspace_header_to_its_root_slot() {
    let mut list = unite_pair();
    list.toggle_collapsed(0); // fold wsA
    let at = |line| sidebar_row_at_line_for_sidebar(&list, line, Sidebar::Full, 0);
    // wsA folded to one header line (flat 0); then the two-row gap; then wsB
    // expanded: 3 hdr, 4-5 root(flat1), 6 div, 7-9 b1(flat2), 10 create(flat3).
    assert_eq!(at(0), Some(0)); // wsA folded header (root slot)
    assert_eq!(at(1), None); // gap
    assert_eq!(at(2), None); // gap
    assert_eq!(at(3), None); // wsB header
    assert_eq!(at(4), Some(1)); // wsB root
    assert_eq!(at(7), Some(2)); // b1
    assert_eq!(at(10), Some(3)); // wsB create row
}

#[test]
fn sidebar_scroll_walks_folded_workspaces() {
    let mut list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![
                worktree(Some("b0"), true, BranchStatus::Pushed),
                worktree(Some("b1"), false, BranchStatus::Local),
            ],
        ),
    ]);
    list.toggle_collapsed(0); // wsA → a single line
                              // Folded flat rows: wsA header 0, wsB root 1, b0 2, b1 3, wsB create 4.
                              // Selecting the folded header keeps the top pinned.
    list.focus_index(0);
    assert_eq!(sidebar_scroll(&list, true, 6), 0);
    // Layout lines: wsA folded 0; gap 1-2; wsB hdr 3, root 4-5, div 6, b0 7-9,
    // b1 10-12, create 13 → total 14. Selecting b1 (span 10-12) scrolls by 7.
    list.focus_index(3); // b1
    assert_eq!(sidebar_scroll(&list, true, 6), 7);
}

#[test]
fn left_pane_stops_at_a_later_group_once_the_pane_is_full() {
    // The first (empty) workspace alone fills the pane — header + root (2) +
    // divider + empty message = 5 rows — so the second group is never started
    // (the per-group full check breaks before building it).
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new("wsA", Vec::new()),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert_eq!(lines.len(), 5);
    let rendered = stripped(&lines);
    assert!(rendered.contains("▌ wsA"));
    assert!(!rendered.contains("wsB")); // the second group was never reached
}

#[test]
fn rail_pane_stops_at_a_later_group_once_the_rail_is_full() {
    // The first (empty) workspace fills the rail — root (2) + divider + blank = 4
    // rows — so the second group's gap and rows are never built.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new("wsA", Vec::new()),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        4,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    assert_eq!(lines.len(), 4);
    // The second group's blank gap was never built; only the first empty workspace
    // contributes its spacer row.
}

#[test]
fn sidebar_row_at_line_walks_a_single_group_layout() {
    // root (lines 0,1) → row 0, divider (2) → none, then 3 lines per worktree.
    let list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    let at = |line| sidebar_row_at_line_for_sidebar(&list, line, Sidebar::Full, 0);
    assert_eq!(at(0), Some(0)); // root id
    assert_eq!(at(1), Some(0)); // root detail
    assert_eq!(at(2), None); // divider
    assert_eq!(at(3), Some(1)); // main
    assert_eq!(at(6), Some(2)); // feature
    assert_eq!(at(99), None); // past the end
}

#[test]
fn sidebar_row_at_line_walks_a_unite_layout_with_headers() {
    // Two groups; each is headed by a name line (unite), then root (2), divider,
    // sessions, and its own "+ new session" row. A two-row visual gap separates
    // workspace blocks. Flat rows run across groups: wsA root=0, a1=1, wsA create=2,
    // wsB root=3, b1=4, wsB create=5. Layout lines:
    // 0 hdrA, 1-2 root, 3 div, 4-6 a1, 7 create, 8-9 gap, 10 hdrB, 11-12 root,
    // 13 div, 14-16 b1, 17 create.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let at = |line| sidebar_row_at_line_for_sidebar(&list, line, Sidebar::Full, 0);
    assert_eq!(at(0), None); // wsA header
    assert_eq!(at(1), Some(0)); // wsA root
    assert_eq!(at(3), None); // wsA divider
    assert_eq!(at(4), Some(1)); // a1
    assert_eq!(at(7), Some(2)); // wsA create row
    assert_eq!(at(8), None); // first gap row
    assert_eq!(at(9), None); // second gap row
    assert_eq!(at(10), None); // wsB header
    assert_eq!(at(11), Some(3)); // wsB root
    assert_eq!(at(13), None); // wsB divider
    assert_eq!(at(14), Some(4)); // b1
    assert_eq!(at(17), Some(5)); // wsB create row
}

#[test]
fn sidebar_row_at_line_walks_a_unite_rail_layout_with_gaps() {
    // The rail does not draw workspace-name headers, but it keeps the two-row
    // inter-workspace gap so row-selection matches what the rail renders.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    // Flat rows: wsA root=0(lines 0-1), div 2, a1=1(3-5), createA=2(6), gap 7-8,
    // wsB root=3(9-10), div 11, b1=4(12-14), createB=5(15).
    let at = |line| sidebar_row_at_line_for_sidebar(&list, line, Sidebar::Rail, 0);
    assert_eq!(at(0), Some(0)); // wsA root
    assert_eq!(at(2), None); // wsA divider
    assert_eq!(at(3), Some(1)); // a1
    assert_eq!(at(6), Some(2)); // wsA create row (rail)
    assert_eq!(at(7), None); // first gap row
    assert_eq!(at(8), None); // second gap row
    assert_eq!(at(9), Some(3)); // wsB root
    assert_eq!(at(11), None); // wsB divider
    assert_eq!(at(12), Some(4)); // b1
    assert_eq!(at(15), Some(5)); // wsB create row (rail)
}

#[test]
fn sidebar_row_at_line_skips_an_empty_workspaces_message() {
    // An empty group contributes header, root, divider, and an empty message line
    // (which maps to no row); the next group's rows follow.
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new("wsA", Vec::new()),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    let at = |line| sidebar_row_at_line_for_sidebar(&list, line, Sidebar::Full, 0);
    // wsA (empty): 0 hdr, 1-2 root(flat0), 3 div, 4 create(flat1),
    //   5-6 gap. wsB: 7 hdr, 8-9 root(flat2), 10 div, 11-13 b1(flat3), 14 create.
    assert_eq!(at(1), Some(0)); // wsA root
    assert_eq!(at(4), Some(1)); // wsA create row (empty workspace)
    assert_eq!(at(5), None); // first gap row
    assert_eq!(at(6), None); // second gap row

    assert_eq!(at(8), Some(2)); // wsB root
    assert_eq!(at(11), Some(3)); // b1
}

#[test]
fn group_inline_insert_line_includes_unite_gaps_before_later_groups() {
    let list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new(
            "wsA",
            vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
        ),
        WorkspaceGroup::new(
            "wsB",
            vec![worktree(Some("b1"), false, BranchStatus::Local)],
        ),
    ]);
    // Group A: header + root(2) + divider + session(3) + create(1) = 8 rows; its
    // create row (the inline-input anchor) is the last line, index 7.
    assert_eq!(group_inline_insert_line(&list, 0), 7);
    // Group B starts after the two-row gap, then has an 8-row block plus that gap
    // (10 lines from 8..=17); its create row is the last line, index 17.
    assert_eq!(group_inline_insert_line(&list, 1), 17);
}

#[test]
fn row_select_click_works_in_unite_mode() {
    // The row-select click maps to the right session across groups.
    let mut state = state_with_sessions(&["main"]); // primary "usagi" with one session
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
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);
    // Body line 4 (header 0, root 1-2, divider 3, session 4) is the primary's
    // session → flat row 1. Screen row = CHROME_TOP_ROWS (3) + 4 = 7.
    assert_eq!(left_pane_session_at(&state, 2, 7, 24, 120), Some(1));
}

/// A 統合(unite) state at 120×24 with the full sidebar: the primary workspace
/// "usagi" carries session `main` with PR #412, and an extra workspace "wsB"
/// carries session `b1` with PR #777 — so a badge / popup must reach across the
/// per-workspace group headers and the inter-workspace gap, not just the first
/// group. Worktree rows (per [`full_sidebar_worktree_entries`]): `main` starts on
/// body line 4 (header 0, root 1-2, divider 3) → detail screen row 8; `b1` starts
/// on body line 13 (the 2-row gap, wsB's header, root, divider) → detail row 17.
fn unite_with_prs() -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.restore_sessions(vec![SessionRecord {
        name: "main".to_string(),
        display_name: None,
        note: None,
        label_id: None,
        agent: Default::default(),
        root: PathBuf::from("/ws/main"),
        worktrees: vec![worktree_with_pr(412)],
        created_at: Utc::now(),
        last_active: None,
    }]);
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
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: vec![worktree_with_pr(777)],
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);
    state
}

#[test]
fn sidebar_pr_badge_at_maps_badges_across_unite_groups() {
    let state = unite_with_prs();
    // The primary workspace's badge (detail row 8) maps to global index 0, and the
    // extra workspace's badge (detail row 18, past the create row, gap, and header) to global
    // index 1 — the popup reaches a session in any group, not just the first.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 8), Some(0));
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 18), Some(1));
    // The identity line (row 7 / 16) above each detail line carries no badge.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 7), None);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 17), None);
}

#[test]
fn sidebar_pr_badge_at_skips_an_empty_earlier_unite_group() {
    // An empty primary workspace contributes root/divider/create (no "no
    // sessions" message), then the extra workspace's badge still resolves. `b1`
    // starts on body line 11 (empty primary: header 0, root 1-2, divider 3,
    // create 4; then the gap 5-6, wsB header 7, root 8-9, divider 10) → screen
    // detail row 15.
    let mut state = HomeState::new("usagi", Vec::new(), None);
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
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: vec![worktree_with_pr(777)],
            created_at: Utc::now(),
            last_active: None,
        }],
        issues: Vec::new(),
    }]);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 15), Some(0));
}

#[test]
fn pr_popup_floats_and_opens_across_unite_groups() {
    let mut state = unite_with_prs();
    // The primary workspace's PR (global 0) floats at its entry's first row (screen
    // row 7) just past the 40-column pane and 3-column divider (left 43).
    state.set_pr_popup(Some(0));
    let (popup, top, left) = pr_popup_placement(&state, 24, 120).expect("a box for group 0");
    assert_eq!((top, left), (7, 43));
    assert!(stripped(&popup).contains("#412"));
    // The extra workspace's PR (global 1) floats lower, past the first group's
    // create row, gap, and header (entry starts on body line 14 → screen row 17).
    state.set_pr_popup(Some(1));
    let (popup, top, left) = pr_popup_placement(&state, 24, 120).expect("a box for group 1");
    assert_eq!((top, left), (17, 43));
    assert!(stripped(&popup).contains("#777"));
    // Clicking `#777` in that box (content row 18, the token flush at left+2 = 45)
    // opens the extra workspace's PR — the click resolves the right session's URL
    // across groups.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 18),
        PopupClick::Open(url) if url == "https://github.com/o/r/pull/777"
    ));
}

#[test]
fn left_pane_stops_building_rows_once_the_pane_is_full() {
    // More sessions than fit: the root (2 lines) + divider take 3 rows, so with
    // rows = 5 only the first worktree's two lines fit. Building stops at the
    // visible height instead of rendering all five worktrees and truncating.
    let list = list_with(vec![
        worktree(Some("one"), true, BranchStatus::Pushed),
        worktree(Some("two"), false, BranchStatus::Local),
        worktree(Some("three"), false, BranchStatus::Local),
        worktree(Some("four"), false, BranchStatus::Local),
        worktree(Some("five"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert_eq!(lines.len(), 5);
    // Root, divider, then only the first worktree made it in; the rest were never
    // built (the result is identical to building all and truncating).
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains("one"));
    let rendered = stripped(&lines);
    assert!(!rendered.contains("five"));
}

#[test]
fn rail_pane_stops_building_rows_once_the_rail_is_full() {
    // The rail collapses the same list; with rows = 5 only the first worktree's
    // two lines fit past the root (2 lines) and divider, so building breaks early
    // just as the full sidebar does.
    let list = list_with(vec![
        worktree(Some("one"), true, BranchStatus::Pushed),
        worktree(Some("two"), false, BranchStatus::Local),
        worktree(Some("three"), false, BranchStatus::Local),
        worktree(Some("four"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        8,
        5,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    assert_eq!(lines.len(), 5);
}

#[test]
fn left_pane_marks_the_agent_state_through_its_lifecycle() {
    let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let empty = HashSet::new();
    // Rows: 0/1 root, 2 divider, 3 worktree identity, 4 worktree detail.
    // Live but no turn yet: `☾` (word dropped).
    let ready = left_pane(
        &list,
        &path,
        &empty,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(ready[4].contains('☾'));
    assert!(!ready[4].contains("ready"));
    // Working a turn: `▶` (word dropped).
    let running = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(running[4].contains('▶'));
    assert!(!running[4].contains("running"));
    // Awaiting input wins over running: `◆` (word dropped).
    let waiting = left_pane(
        &list,
        &path,
        &path,
        &path,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(waiting[4].contains('◆'));
    assert!(!waiting[4].contains('▶'));
    // No live session: no agent detail at all.
    let absent = left_pane(
        &list,
        &empty,
        &empty,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(!absent[4].contains('▶'));
    assert!(!absent[4].contains('◆'));
    assert!(!absent[4].contains('☾'));
    // Line 1 carries the branch name but no longer the git status word.
    assert!(absent[3].contains("feature"));
    assert!(!absent[3].contains("local"));
}

#[test]
fn left_pane_always_draws_a_fixed_three_line_resource_row() {
    let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let empty = HashSet::new();
    let resources: HashMap<PathBuf, ResourceUsage> = [(
        PathBuf::from("/repo/wt"),
        ResourceUsage {
            cpu_percent: 12,
            memory_bytes: 256 * 1024 * 1024,
        },
    )]
    .into_iter()
    .collect();

    // Rows: 0/1 root, 2 divider, 3 identity, 4 agent detail, 5 the resource line.
    let with_usage = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &[],
        &resources,
        &crate::domain::settings::SessionLabelMaster::default(),
        40,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(with_usage[4].contains('▶'));
    // The resource line is icon-led (the CPU / memory glyphs in place of the words),
    // so it carries the figures but not the words `CPU` / `MEM`.
    let resource = console::strip_ansi_codes(&with_usage[5]);
    assert!(resource.contains("12%"));
    assert!(resource.contains("256MB"));
    assert!(!resource.contains("CPU"));
    assert!(!resource.contains("MEM"));

    // With no sample the session keeps its fixed three lines — the resource row is
    // still drawn, reading `0%` / `0MB` rather than dropping the row.
    let without = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        40,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(without[4].contains('▶'));
    let idle_resource = console::strip_ansi_codes(&without[5]);
    assert!(idle_resource.contains("0%"));
    assert!(idle_resource.contains("0MB"));

    // In 切替 the unselected rows (the cursor rests on the root) are dimmed — the
    // resource line is faded along with the rest of its entry, but its text stays.
    let in_switch = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &[],
        &resources,
        &crate::domain::settings::SessionLabelMaster::default(),
        40,
        8,
        true,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert!(console::strip_ansi_codes(&in_switch[5]).contains("12%"));
}

#[test]
fn left_pane_is_trimmed_to_available_rows() {
    let list = list_with(vec![
        worktree(Some("a"), false, BranchStatus::Local),
        worktree(Some("b"), false, BranchStatus::Local),
        worktree(Some("c"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        4,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // 3 worktrees would be 2 (root) + 1 (divider) + 6 lines; trimmed to 4.
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains('a'));
}

#[test]
fn left_pane_marks_the_active_worktree_with_a_gutter_bar() {
    let mut list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    list.activate_by_name("feature");
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        9,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // The root is not active; the active "feature" row carries the green `▎`
    // accent bar down all three of its lines (identity + detail + resource).
    assert!(!lines[0].contains('▎'));
    assert!(lines[6].contains("feature"));
    assert!(lines[6].contains('▎'));
    assert!(lines[7].contains('▎'));
    assert!(lines[8].contains('▎'));
}

#[test]
fn left_pane_marks_the_selected_session_with_a_rabbit_stack() {
    let mut list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    list.move_down(); // root -> main
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        9,
        true,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    let plain: Vec<String> = lines
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();
    // root(2) + divider(1), then the selected session's three rows.
    assert!(plain[3].starts_with('󰤇'));
    assert!(plain[4].starts_with('▎'));
    assert!(plain[5].starts_with('▎'));
}

#[test]
fn rail_collapses_each_entry_to_three_rows_without_names_or_numbers() {
    let list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    let empty = HashSet::new();
    let lines = left_pane(
        &list,
        &empty,
        &empty,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        RAIL_WIDTH,
        9,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    // Root (2 rows), a divider, then 3 rows per worktree — the same shape as the
    // full sidebar, so toggling never shifts an entry to a different row.
    assert_eq!(lines.len(), 9);
    let plain: Vec<String> = lines
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();
    // No names and no numbers on the rail.
    assert!(!plain.iter().any(|l| l.contains("feature")));
    assert!(!plain.iter().any(|l| l.chars().any(|c| c.is_ascii_digit())));
    // Root glyph on row 1, divider, then each worktree's kind dot on row 1 (the
    // git-status glyph no longer rides the rail).
    assert!(plain[0].contains('⌂'));
    assert!(plain[2].contains('─'));
    assert!(plain[3].contains('●')); // fresh heat dot (main, just touched)
    assert!(plain[6].contains('●')); // fresh heat dot (feature, just touched)
                                     // A space separates the gutter from the glyph, and every row fills the rail.
    assert!(plain[3].starts_with("  ●") || plain[3].starts_with(" ●"));
    assert!(lines
        .iter()
        .all(|l| console::measure_text_width(l) == RAIL_WIDTH));
}

#[test]
fn rail_keeps_the_same_row_count_as_the_full_sidebar() {
    // The anti-CLS guarantee: for the same list, the rail and the full sidebar
    // produce the same number of rows, so Ctrl-B only changes the width — no
    // session jumps to a different row.
    let mk = |sidebar| {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let empty = HashSet::new();
        left_pane(
            &list,
            &empty,
            &empty,
            &empty,
            &empty,
            &[],
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            30,
            20,
            false,
            sidebar,
            Utc::now(),
            None,
        )
        .len()
    };
    assert_eq!(mk(Sidebar::Full), mk(Sidebar::Rail));
    // And the empty workspace stays aligned too.
    let empty_mk = |sidebar| {
        let empty = HashSet::new();
        left_pane(
            &list_with(Vec::new()),
            &empty,
            &empty,
            &empty,
            &empty,
            &[],
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            30,
            20,
            false,
            sidebar,
            Utc::now(),
            None,
        )
        .len()
    };
    assert_eq!(empty_mk(Sidebar::Full), empty_mk(Sidebar::Rail));
}

#[test]
fn rail_shows_the_active_bar_down_all_rows_and_the_agent_glyph_on_row_two() {
    let mut list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    list.activate_by_name("feature");
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let empty = HashSet::new();
    // "feature" is active and running: the green `▎` bar runs down all three of its
    // rows, the kind dot is on row 1, the running glyph on row 2, and the (blank)
    // resource row keeps the bar on row 3.
    let lines = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        RAIL_WIDTH,
        9,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    // "feature" is the second worktree: root (2) + divider (1) + main (3) puts its
    // rows at indices 6,7,8.
    let top = console::strip_ansi_codes(&lines[6]).into_owned();
    let detail = console::strip_ansi_codes(&lines[7]).into_owned();
    let resource = console::strip_ansi_codes(&lines[8]).into_owned();
    assert!(top.contains('▎'));
    assert!(top.contains('●')); // fresh heat dot on row 1
    assert!(detail.contains('▎'));
    assert!(detail.contains('▶')); // agent state on row 2
    assert!(resource.contains('▎')); // the bar reaches the (blank) resource row
                                     // The root row (not active) carries no bar.
    assert!(!console::strip_ansi_codes(&lines[0]).contains('▎'));
}

#[test]
fn rail_shows_each_agent_state_glyph_on_the_detail_row() {
    let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let p: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let e = HashSet::new();
    let glyph = |live: &HashSet<PathBuf>,
                 running: &HashSet<PathBuf>,
                 waiting: &HashSet<PathBuf>,
                 done: &HashSet<PathBuf>| {
        let lines = left_pane(
            &list,
            live,
            running,
            waiting,
            done,
            &[],
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            RAIL_WIDTH,
            8,
            false,
            Sidebar::Rail,
            Utc::now(),
            None,
        );
        // Rows: 0/1 root, 2 divider, 3 worktree kind, 4 worktree agent state.
        console::strip_ansi_codes(&lines[4]).into_owned()
    };
    // The rail's detail glyph follows the same lifecycle as the full sidebar's
    // agent phase: ready ☾, running ▶, waiting ◆, done ✓.
    assert!(glyph(&p, &e, &e, &e).contains('☾'));
    assert!(glyph(&p, &p, &e, &e).contains('▶'));
    assert!(glyph(&p, &p, &p, &e).contains('◆'));
    assert!(glyph(&p, &p, &p, &p).contains('✓'));
}

#[test]
fn rail_sidebar_marks_the_switch_cursor() {
    let mut list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let empty = HashSet::new();
    let rail = |list: &WorktreeList| {
        left_pane(
            list,
            &empty,
            &empty,
            &empty,
            &empty,
            &[],
            &HashMap::new(),
            &crate::domain::settings::SessionLabelMaster::default(),
            RAIL_WIDTH,
            8,
            true,
            Sidebar::Rail,
            Utc::now(),
            None,
        )
    };
    // In 切替 the cursor row shows the `>` marker on non-session rows; here the
    // cursor is on the root.
    let on_root = rail(&list);
    assert!(console::strip_ansi_codes(&on_root[0]).contains('>'));
    // Moving the cursor onto the worktree marks its three session rows with the
    // one-line usagi glyph and vertical continuations, and fades the root entry
    // (the cursor leaves it), so the highlighted session still reads first.
    list.move_down();
    let on_session = rail(&list);
    assert!(!console::strip_ansi_codes(&on_session[0]).contains('>'));
    assert!(console::strip_ansi_codes(&on_session[3]).contains('󰤇'));
    assert!(console::strip_ansi_codes(&on_session[4]).contains('▎'));
    assert!(console::strip_ansi_codes(&on_session[5]).contains('▎'));
}

#[test]
fn left_pane_detail_line_with_commit_arrows_does_not_overrun_the_sidebar() {
    use crate::domain::workspace_state::{AheadBehind, DiffStat};
    // The `↑` / `↓` commit arrows are ambiguous-width — the terminal paints them two
    // columns wide. If the detail line is laid out counting them as one, the row is
    // built wider than it measures, so the sidebar's CJK-aware clip chops its right
    // edge (the PR badge) and the layout looks broken. The row's painted width must
    // stay within the pane.
    let mut w = worktree(Some("pr"), false, BranchStatus::Pushed);
    w.path = PathBuf::from("/repo/pr");
    w.ahead_behind = Some(AheadBehind {
        ahead: 1,
        behind: 4,
    });
    w.diff = Some(DiffStat {
        added: 71,
        removed: 1,
    });
    w.pr = vec![PrLink {
        number: 1,
        url: "https://github.com/o/r/pull/1".into(),
    }];
    let left_w = 34usize;
    let lines = left_pane(
        &list_with(vec![w]),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        left_w,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    let detail = console::strip_ansi_codes(&lines[4]);
    // Measured as the terminal paints it (arrows = two columns), the row fits the
    // pane — nothing bleeds into the right pane or gets clipped away.
    assert!(
        console::measure_text_width(&detail) <= left_w,
        "detail line overruns the {left_w}-column sidebar: {detail:?}"
    );
    // Both the arrows and the PR badge survive intact.
    assert!(
        detail.contains("↑1 ↓4"),
        "commit arrows dropped: {detail:?}"
    );
    assert!(detail.contains(PR_ICON), "PR badge chopped off: {detail:?}");
}

#[test]
fn dim_row_strips_existing_colour_but_keeps_the_text() {
    // Fading a row drops its colour codes (so it reads as muted) while the
    // text survives. (Styling is off in non-TTY tests, so we assert the
    // colour is gone rather than that a dim code was added.)
    let coloured = "\u{1b}[36mfeature\u{1b}[0m";
    let dimmed = dim_row(coloured);
    assert!(!dimmed.contains("\u{1b}[36m"));
    assert!(console::strip_ansi_codes(&dimmed).contains("feature"));
}

#[test]
fn left_pane_fades_every_row_but_the_cursor_when_asked() {
    let list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    // Cursor is on the root row (index 0). Dimming on fades the non-cursor
    // session rows; every row keeps its text.
    let dimmed = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        9,
        true,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    assert_eq!(dimmed.len(), 9);
    assert!(console::strip_ansi_codes(&dimmed[0]).contains(ROOT_NAME));
    assert!(console::strip_ansi_codes(&dimmed[3]).contains("main"));
    assert!(console::strip_ansi_codes(&dimmed[6]).contains("feature"));
}

#[test]
fn left_pane_shows_the_pr_badge_for_a_session_that_has_one() {
    // A session whose worktree carries a PR renders the `<icon> <count>` badge on
    // its detail line; a session without one shows no badge. `left_pane` sizes the
    // PR column (via `detail_cols`) and `worktree_row` fills the `pr_cell`.
    let list = list_with(vec![
        worktree_with_pr(412),
        worktree(Some("plain"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    let rendered = stripped(&lines);
    // The first session (rows 3,4 after the root pair + divider) shows `<icon> 1`.
    assert!(rendered.contains(format!("{PR_ICON} 1").as_str()));
    // The plain session contributes no PR, so the icon appears exactly once.
    assert_eq!(rendered.matches(PR_ICON).count(), 1);
}

/// An attached (没入) state at 120×24 with the full sidebar, listing a session that
/// carries two PRs followed by one with no PR — the fixture the PR badge / popup
/// click tests share. Worktree rows start at screen row 6 (the body begins at row 3;
/// the root entry and divider take its first 3 lines), three screen rows each: the
/// PR session at rows 6–8, the PR-less one at rows 9–11.
fn attached_with_pr_sidebar() -> HomeState {
    let mut wt = worktree_with_pr(412);
    wt.pr.push(PrLink {
        number: 98,
        url: "https://github.com/o/other/pull/98".to_string(),
    });
    let mut state = state_with(vec![
        wt,
        worktree(Some("plain"), false, BranchStatus::Local),
    ]);
    state.enter_focus(1);
    state.show_attached();
    state
}

#[test]
fn sidebar_pr_badge_at_maps_the_badge_columns_to_its_session() {
    let state = attached_with_pr_sidebar();
    // The left pane is 40 columns at width 120, so the folded `<icon> <count>` badge
    // (here ` 2`, three columns wide) seats flush at its right edge — columns 37–39
    // on the entry's detail line (row 7, the second of its three rows). A click on
    // the badge maps to that session (index 0), so the loop pins its PR popup.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 37, 7), Some(0));
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 39, 7), Some(0));
    // Left of the badge (the agent-label side of the detail line) maps to nothing.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 33, 7), None);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 10, 7), None);
}

#[test]
fn sidebar_pr_badge_at_ignores_the_rows_other_than_the_detail_line() {
    let state = attached_with_pr_sidebar();
    // The badge columns on the identity line (row 6) and the CPU / memory line
    // (row 8) of the same session carry no badge, so a click there maps to nothing.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 6), None);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 8), None);
}

#[test]
fn sidebar_pr_badge_at_ignores_rows_without_a_pr() {
    let state = attached_with_pr_sidebar();
    // The second session's detail line (row 10 of rows 9–11) has no PR.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 10), None);
    // The root entry (rows 3–4) and the divider (row 5) are not session rows.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 4), None);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 5), None);
    // A body row past the end of the session list maps to no worktree.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 13), None);
}

#[test]
fn sidebar_pr_badge_at_ignores_clicks_off_the_sidebar() {
    let state = attached_with_pr_sidebar();
    // Left pane is 40 columns at width 120, so a click at column 40+ is the
    // divider / right pane, not a sidebar row.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 40, 7), None);
    // Rows above the body (the title bar / mode ladder / blank separator).
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 1), None);
    // A row below the two-pane body (past `body_rows`).
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 38, 22), None);
}

#[test]
fn pr_popup_worktree_entries_count_pending_skeletons_before_later_groups() {
    let mut a = WorkspaceGroup::new(
        "wsA",
        vec![worktree(Some("a1"), true, BranchStatus::Pushed)],
    );
    a.set_root_path("/a");
    let mut b = WorkspaceGroup::new("wsB", vec![worktree_with_pr(7)]);
    b.set_root_path("/b");
    let list = WorktreeList::from_groups(vec![a, b]);
    let mut state = state_with(Vec::new());
    state.begin_pending_session(PathBuf::from("/a"), "newx".to_string());

    let without = full_sidebar_worktree_entries_with_pending(&list, &[]);
    let with = full_sidebar_worktree_entries_with_pending(&list, state.pending_sessions());
    assert_eq!(with[0], without[0]);
    assert_eq!(with[1].1, without[1].1 + SESSION_ROWS);
}

#[test]
fn sidebar_pr_badge_at_is_none_on_the_collapsed_rail() {
    let mut state = attached_with_pr_sidebar();
    // The rail shows no PR badge, so a click there maps to nothing.
    state.set_sidebar(Sidebar::Rail);
    assert_eq!(sidebar_pr_badge_at(&state, 24, 120, 3, 7), None);
}

#[test]
fn sidebar_pr_badge_at_ignores_a_badge_clipped_by_a_cramped_pane() {
    let state = attached_with_pr_sidebar();
    // On a very narrow screen the left pane shrinks until the folded badge can no
    // longer seat flush-right past the name indent, so its columns can't be placed —
    // a click maps to nothing rather than guessing. At width 9 the left pane is 6
    // columns and the 3-column badge would start at column 3, inside `NAME_PREFIX`.
    assert_eq!(sidebar_pr_badge_at(&state, 24, 9, 3, 7), None);
}

#[test]
fn pr_popup_placement_floats_the_box_beside_the_pinned_session() {
    let mut state = attached_with_pr_sidebar();
    // Nothing pinned → no box to float.
    assert!(pr_popup_placement(&state, 24, 120).is_none());
    // Pinning the PR session floats its box at its first row (screen row 6) just
    // past the 40-column pane and the 3-column divider (left 43).
    state.set_pr_popup(Some(0));
    let (popup, top, left) = pr_popup_placement(&state, 24, 120).expect("a box for a pinned PR");
    assert_eq!((top, left), (6, 43));
    let plain = stripped(&popup);
    assert!(plain.contains("#412") && plain.contains("#98"));
    // Pinning the PR-less second session yields nothing, and so does the rail.
    state.set_pr_popup(Some(1));
    assert!(pr_popup_placement(&state, 24, 120).is_none());
    state.set_pr_popup(Some(0));
    state.set_sidebar(Sidebar::Rail);
    assert!(pr_popup_placement(&state, 24, 120).is_none());
}

#[test]
fn pr_popup_click_opens_the_number_under_the_pointer() {
    let mut state = attached_with_pr_sidebar();
    state.set_pr_popup(Some(0));
    // Content sits two columns in from the box's left edge (left 43 → col 45). The
    // packed row is `#412 #98`: `#412` spans cols 45–48, a gap at 49, `#98` 50–52,
    // all on the box's content row (top 6 + 1 = row 7).
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 7),
        PopupClick::Open(url) if url == "https://github.com/o/r/pull/412"
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 50, 7),
        PopupClick::Open(url) if url == "https://github.com/o/other/pull/98"
    ));
    // The gap between tokens is inside the box but on no number → stays pinned.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 49, 7),
        PopupClick::Inside
    ));
    // The box's borders (top row 6, bottom row 8) are inside it too.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 6),
        PopupClick::Inside
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 8),
        PopupClick::Inside
    ));
}

#[test]
fn pr_popup_click_outside_the_box_dismisses_it() {
    let mut state = attached_with_pr_sidebar();
    // With nothing pinned every click is outside.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 7),
        PopupClick::Outside
    ));
    state.set_pr_popup(Some(0));
    // A click left of the box (over the sidebar), above it, and below it all land
    // outside the box's rectangle and so dismiss the popup.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 2, 7),
        PopupClick::Outside
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 5),
        PopupClick::Outside
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 45, 9),
        PopupClick::Outside
    ));
}

#[test]
fn pr_popup_click_on_the_box_borders_stays_pinned() {
    let mut state = attached_with_pr_sidebar();
    state.set_pr_popup(Some(0));
    // The box spans columns 43–54 (left 43, `#412 #98` → width 12) on content row 7.
    // Its left border / pad (43, 44) and right border (54) are inside the rectangle
    // but on no `#<number>`, as is a content column past the last token — all keep
    // the popup pinned rather than opening or dismissing.
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 43, 7),
        PopupClick::Inside
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 44, 7),
        PopupClick::Inside
    ));
    assert!(matches!(
        pr_popup_click(&state, 24, 120, 54, 7),
        PopupClick::Inside
    ));
}

#[test]
fn pr_popup_placement_is_none_when_empty_or_too_narrow() {
    // No session carries the pinned index (both workspaces are empty), so there is
    // no worktree to anchor a box on.
    let mut empty = state_with(vec![worktree_with_pr(412)]);
    empty.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: Vec::new(),
        issues: Vec::new(),
    }]);
    empty.set_pr_popup(Some(0));
    assert!(pr_popup_placement(&empty, 24, 120).is_none());
    // Too narrow: the `PR` box can't fit the terminal width, so it is not placed.
    let mut narrow = attached_with_pr_sidebar();
    narrow.set_pr_popup(Some(0));
    assert!(pr_popup_placement(&narrow, 24, 10).is_none());
}

#[test]
fn log_line_colours_each_kind_and_prompts_commands() {
    assert!(log_line(&LogLine::command("man"), 40).contains("❯ man"));
    assert_eq!(log_line(&LogLine::output("plain"), 40), "plain");
    assert!(log_line(&LogLine::error("boom"), 40).contains("boom"));
    assert!(log_line(&LogLine::notice("note"), 40).contains("note"));
}

#[test]
fn left_pane_session_at_maps_each_row_pair_to_its_session() {
    // Two sessions on a 24×120 screen: left pane is 40 columns, body starts at
    // screen row 3 (after the title / ladder / blank chrome). The root pair spans
    // rows 3,4, a divider on row 5, then each worktree spans three rows.
    let state = state_with(vec![
        worktree(Some("main"), true, BranchStatus::Local),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    let at = |col, row| left_pane_session_at(&state, col, row, 24, 120);
    // The root entry's two rows both select the root (index 0).
    assert_eq!(at(0, 3), Some(0));
    assert_eq!(at(10, 4), Some(0));
    // The divider between the root and the sessions selects nothing.
    assert_eq!(at(0, 5), None);
    // Worktree 0 spans rows 6,7,8 (index 1); worktree 1 spans rows 9,10,11 (index 2).
    assert_eq!(at(0, 6), Some(1));
    assert_eq!(at(39, 7), Some(1));
    assert_eq!(at(0, 8), Some(1));
    assert_eq!(at(0, 9), Some(2));
    assert_eq!(at(0, 11), Some(2));
}

#[test]
fn left_pane_session_at_ignores_clicks_off_the_session_rows() {
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    let at = |col, row| left_pane_session_at(&state, col, row, 24, 120);
    // The right pane (past the 40-column left pane) is not a row select.
    assert_eq!(at(40, 3), None);
    assert_eq!(at(80, 6), None);
    // The chrome above the body (title / ladder / blank) selects nothing.
    assert_eq!(at(0, 0), None);
    assert_eq!(at(0, 2), None);
    // The persistent create row sits below the only session (rows 6,7,8).
    assert_eq!(at(0, 9), Some(2));
    // Far below the body (past its 19 rows) selects nothing either.
    assert_eq!(at(0, 23), None);
}

#[test]
fn left_pane_session_at_maps_clicks_on_the_collapsed_rail() {
    // The rail is 5 columns wide but keeps the same two-rows-per-entry layout, so
    // the same row maps to the same session — only the column bound narrows.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_sidebar(Sidebar::Rail);
    // A click within the rail on worktree 0's rows selects it.
    assert_eq!(left_pane_session_at(&state, 0, 6, 24, 120), Some(1));
    // A click just past the 5-column rail is in the right pane, not a select.
    assert_eq!(left_pane_session_at(&state, 5, 6, 24, 120), None);
}

#[test]
fn left_pane_draws_the_create_row_at_the_foot_and_marks_the_cursor() {
    // The persistent "+ new session" row is always the last built list row; in
    // 切替 the cursor on it shows the `>` gutter while the other rows fade.
    let mut list = list_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    list.focus_index(list.create_row());
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        80,
        12,
        true,
        Sidebar::Full,
        Utc::now(),
        None,
    );
    // root(2) + divider(1) + session(3) = 6, so the create row is line 6.
    let create = console::strip_ansi_codes(&lines[6]).into_owned();
    assert!(create.contains("+ new session"));
    assert!(create.trim_start().starts_with('>'));
}

#[test]
fn rail_pane_draws_the_create_row_glyph() {
    let mut list = list_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    list.focus_index(list.create_row());
    let lines = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        5,
        12,
        true,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    let create = console::strip_ansi_codes(lines.last().unwrap()).into_owned();
    assert!(create.contains('+'));
    assert!(create.trim_start().starts_with('>'));
}

#[test]
fn sidebar_row_at_line_maps_the_create_row_after_the_sessions() {
    // root(0,1) → 0, divider(2), main 3,4,5 → row 1, then the create row on line 6
    // maps to the list's create-row index (2).
    let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert_eq!(list.create_row(), 2);
    assert_eq!(
        sidebar_row_at_line_for_sidebar(&list, 6, Sidebar::Full, 0),
        Some(2)
    );
    // The rail keeps the same row layout, so the create row lands on the same line.
    assert_eq!(
        sidebar_row_at_line_for_sidebar(&list, 6, Sidebar::Rail, 0),
        Some(2)
    );
}

#[test]
fn sidebar_row_at_line_skips_pending_skeleton_and_maps_create_after_it() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.set_root_path("/repo");
    state.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    // root(0,1), divider(2), main(3,4,5), pending skeleton(6,7,8), create(9).
    for line in 6..=8 {
        assert_eq!(
            sidebar_row_at_line_for_sidebar_with_pending(
                state.list(),
                line,
                Sidebar::Full,
                0,
                state.pending_sessions(),
            ),
            None
        );
    }
    assert_eq!(
        sidebar_row_at_line_for_sidebar_with_pending(
            state.list(),
            9,
            Sidebar::Full,
            0,
            state.pending_sessions(),
        ),
        Some(2)
    );
}

#[test]
fn group_inline_insert_line_counts_pending_skeleton_rows() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.set_root_path("/repo");
    state.begin_pending_session(PathBuf::from("/repo"), "newx".to_string());
    assert_eq!(
        group_inline_insert_line_with_pending(state.list(), 0, state.pending_sessions()),
        9
    );
}

#[test]
fn switch_preview_prompts_to_create_when_the_create_row_is_selected() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_select(state.list().create_row());
    let lines = switch_preview(&state, 60, 10);
    let text = console::strip_ansi_codes(&lines.join("\n")).into_owned();
    assert!(text.contains("+ new session"));
    assert!(text.contains("Type a name"));
}

/// A single workspace with `n` sessions named `s0`..`s{n-1}`, for the overflow
/// scroll tests. `s0` is the primary.
fn sessions(n: usize) -> Vec<WorktreeState> {
    (0..n)
        .map(|i| worktree(Some(&format!("s{i}")), i == 0, BranchStatus::Local))
        .collect()
}

fn full_pane(list: &WorktreeList, rows: usize) -> Vec<String> {
    left_pane(
        list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        30,
        rows,
        false,
        Sidebar::Full,
        Utc::now(),
        None,
    )
}

#[test]
fn sidebar_scroll_is_zero_while_the_list_fits_the_pane() {
    let list = list_with(sessions(6));
    // Fits (huge viewport) or a zero-height pane: nothing to scroll.
    assert_eq!(sidebar_scroll(&list, true, 100), 0);
    assert_eq!(sidebar_scroll(&list, true, 0), 0);
    // The default cursor rests on the root row at the top, so even an overflowing
    // list stays pinned to the top.
    assert_eq!(sidebar_scroll(&list, true, 9), 0);
}

#[test]
fn sidebar_scroll_reveals_a_selected_row_below_the_fold() {
    let mut list = list_with(sessions(6));
    // Root(2) + divider(1) + 6×3 sessions + create(1) = 22 lines; pane shows 9.
    // Selecting the last session (flat row 6) scrolls just enough to seat its
    // three-row block at the foot: its end line (21) minus the 9-row viewport.
    list.focus_index(6);
    assert_eq!(sidebar_scroll(&list, true, 9), 12);
    // The create row is the very last line; scrolling for it clamps to the maximum
    // (total 22 − viewport 9 = 13) rather than running past the list's foot.
    list.focus_index(7);
    assert_eq!(sidebar_scroll(&list, true, 9), 13);
}

#[test]
fn left_pane_scrolls_the_selected_session_into_view() {
    let mut list = list_with(sessions(6));
    // Cursor on the root row: the window stays pinned to the top, so the first
    // sessions show and the last is off screen.
    let top = full_pane(&list, 9);
    assert_eq!(top.len(), 9);
    let top_txt = stripped(&top);
    assert!(top_txt.contains("s0") && top_txt.contains("s1"));
    assert!(!top_txt.contains("s5"));
    // Selecting the last session scrolls it into view and pushes the first off the
    // top — the whole off-window prefix is skipped, not merely truncated.
    list.focus_index(6);
    let scrolled = full_pane(&list, 9);
    assert_eq!(scrolled.len(), 9);
    let scrolled_txt = stripped(&scrolled);
    assert!(scrolled_txt.contains("s5"));
    assert!(!scrolled_txt.contains("s0"));
}

#[test]
fn left_pane_keeps_the_create_row_visible_when_selected() {
    let mut list = list_with(sessions(6));
    list.focus_index(list.create_row());
    let lines = full_pane(&list, 9);
    assert_eq!(lines.len(), 9);
    let text = stripped(&lines);
    assert!(text.contains("+ new session"));
    assert!(!text.contains("s0"));
}

#[test]
fn rail_pane_scrolls_the_selected_session_into_view() {
    let mut list = list_with(sessions(6));
    list.focus_index(6);
    let scrolled = left_pane(
        &list,
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &HashMap::new(),
        &crate::domain::settings::SessionLabelMaster::default(),
        8,
        9,
        false,
        Sidebar::Rail,
        Utc::now(),
        None,
    );
    assert_eq!(scrolled.len(), 9);
    // The rail carries no branch text, but the selected entry's active gutter bar
    // still rides the scrolled window: the top rows (root + first sessions) are
    // gone, so the window is a pure slice of the tail.
    let rail_scroll = sidebar_scroll(&list, false, 9);
    assert!(rail_scroll > 0);
}

#[test]
fn sidebar_row_click_maps_through_the_scroll_offset() {
    let mut list = list_with(sessions(6));
    list.focus_index(6);
    let scroll = sidebar_scroll(&list, true, 9);
    assert_eq!(scroll, 12);
    // Screen line 6 sits at full-column line 18 — the last session's identity row
    // (flat row 6) — so a click there selects it, not whatever the un-scrolled
    // layout would have had at line 6.
    assert_eq!(
        sidebar_row_at_line_for_sidebar(&list, 6, Sidebar::Full, scroll),
        Some(6)
    );
}

#[test]
fn sidebar_scroll_walks_past_an_empty_workspace_group() {
    // Unite mode with an empty leading workspace still walks that workspace's
    // root/divider/create block before it reaches the selected session in the
    // second group.
    let mut list = WorktreeList::from_groups(vec![
        WorkspaceGroup::new("wsA", Vec::new()),
        WorkspaceGroup::new(
            "wsB",
            vec![
                worktree(Some("b0"), true, BranchStatus::Pushed),
                worktree(Some("b1"), false, BranchStatus::Local),
            ],
        ),
    ]);
    // Flat rows (each expanded workspace owns a create row): wsA root 0, wsA create
    // 1, wsB root 2, b0 3, b1 4, wsB create 5. Select b1.
    list.focus_index(4);
    // Group A block (header 1 + root 2 + divider 1 + create 1 = 5), then group B
    // (gap 2 + header 1 + root 2 + divider 1 + 2×3 + create 1 = 13) = 18. b1's
    // block spans lines 14-16, so a 9-row pane scrolls by 8 (17 − 9).
    assert_eq!(sidebar_scroll(&list, true, 9), 8);
}

#[test]
fn pr_popup_hides_when_the_pinned_session_scrolls_off_the_top() {
    let mut first = worktree_with_pr(412);
    first.primary = true;
    let mut worktrees = vec![first];
    worktrees.extend(sessions(5).into_iter().skip(1)); // s1..s5, plain
    let mut state = state_with(worktrees);
    state.set_pr_popup(Some(0));
    // Height 14 → a 9-row body. With the cursor at the top, the PR session's row is
    // on screen, so its popup floats.
    assert!(pr_popup_placement(&state, 14, 120).is_some());
    // Selecting the last session scrolls the PR session off the top; its badge is no
    // longer drawn, so nothing is pinned over an unrelated row.
    state.focus_session(state.list().create_row());
    assert!(pr_popup_placement(&state, 14, 120).is_none());
}
