use super::*;

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
    // the escapes never eat into the three-column budget.
    let styled = "\x1b[31mhello\x1b[0m";
    let clipped = clip_to_width(styled, 3);
    assert_eq!(console::measure_text_width(&clipped), 3);
    assert!(clipped.starts_with("\x1b[31m"));
    assert!(clipped.contains("he"));
    assert!(clipped.ends_with('…'));
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
fn worktree_row_marks_the_cursor_in_switch_and_shows_detached() {
    // The `>` cursor only appears in 切替 (Switch): the selected row carries it
    // when `in_switch` is set. (The kind dot reflects freshness — a just-built
    // fixture is fresh `●`; heat fading is covered in its own test.)
    let (top, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
        10,
        10,
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
    assert!(top.contains('>'));
    assert!(top.contains('●'));
    assert!(top.contains("main"));

    // The same selected row outside Switch shows no cursor.
    let (top_no_switch, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
        10,
        10,
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

    let (other_top, _) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        10,
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
    assert!(other_top.contains('●'));
    assert!(other_top.contains("feature"));

    let (detached_top, _) = worktree_row(
        &worktree(None, false, BranchStatus::Local),
        "",
        10,
        10,
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
        10,
        10,
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
        10,
        10,
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
    // The marker is purely additive to line 1: its presence must not shift the
    // right-edge status column, so both variants render the same display width.
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
            &worktree, "", 10, 10, false, now, false, false, false, false, false, false, false,
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
        10,
        10,
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
        10,
        10,
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
    // A live session that has not begun a turn yet is idle: `☾ ready`.
    let (_, ready_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        12,
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
    assert!(ready_detail.contains('☾'));
    assert!(ready_detail.contains("ready"));

    // Working a turn: `▶ running`.
    let (_, running_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        12,
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
    assert!(running_detail.contains("running"));

    // Awaiting input wins over running: `◆ waiting`.
    let (_, waiting_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        12,
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
    assert!(waiting_detail.contains("waiting"));

    // A finished agent shows `✓ done`, taking precedence over running.
    let (_, done_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        12,
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
    assert!(done_detail.contains("done"));
    assert!(!done_detail.contains('▶'));

    // No live session: the detail line carries no agent state.
    let (absent_top, absent_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        12,
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
    assert!(absent_top.contains("local"));
}

#[test]
fn worktree_row_shows_the_resource_figure_only_when_one_is_supplied() {
    let usage = ResourceUsage {
        cpu_percent: 8,
        memory_bytes: 120 * 1024 * 1024,
    };
    // A live row given a sample carries `<cpu>% <mem>` on its detail line.
    let (_, with_usage) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        12,
        24,
        false,
        Utc::now(),
        false,
        false,
        false,
        true,
        true,
        false,
        false,
        Some(usage),
    );
    assert!(console::strip_ansi_codes(&with_usage).contains("8% 120MB"));

    // The same row with no sample (a session with no live process) shows none.
    let (_, without_usage) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        12,
        24,
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
    assert!(!console::strip_ansi_codes(&without_usage).contains("MB"));
}

#[test]
fn left_pane_threads_each_sessions_resource_to_its_row() {
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
    let lines = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &resources,
        40,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    // Row 4 is the worktree's detail line, where the resource figure rides.
    assert!(console::strip_ansi_codes(&lines[4]).contains("12% 256MB"));
}

#[test]
fn status_cell_right_aligns_the_status_and_blanks_the_root() {
    let pushed = console::strip_ansi_codes(&status_cell(Some(BranchStatus::Pushed))).into_owned();
    assert_eq!(console::measure_text_width(&pushed), STATUS_COL);
    assert!(pushed.ends_with("pushed"));
    // The icon leads the word inside the field.
    assert!(pushed.contains(PUSHED_ICON));
    // "local" (icon + space + 5 cols = 7) is right-aligned within the 8-col
    // field, so a single lead space precedes the icon.
    let local = console::strip_ansi_codes(&status_cell(Some(BranchStatus::Local))).into_owned();
    assert_eq!(local, format!(" {LOCAL_ICON} local"));
    // The root has no status: an all-blank field of the same width.
    assert_eq!(status_cell(None), " ".repeat(STATUS_COL));
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
        8,
        8,
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
        20,
        20,
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
    let (top, detail) = root_row(10, 20, true, false, true);
    assert!(top.contains('>'));
    assert!(top.contains('⌂'));
    assert!(top.contains(ROOT_NAME));
    assert!(detail.contains("workspace root"));
    // The same selected root outside Switch shows no cursor.
    let (top_no_switch, _) = root_row(10, 20, true, false, false);
    assert!(!top_no_switch.contains('>'));

    // The active root carries the green `▎` bar down both lines, not a `*`.
    let (active_top, active_detail) = root_row(10, 20, false, true, false);
    assert!(active_top.contains('▎'));
    assert!(active_detail.contains('▎'));
    assert!(!active_top.contains('*'));

    let (idle_top, idle_detail) = root_row(10, 20, false, false, false);
    assert!(!idle_top.contains('>'));
    assert!(!idle_top.contains('▎'));
    assert!(!idle_detail.contains('▎'));
    assert!(idle_top.contains(ROOT_NAME));
}

#[test]
fn left_pane_renders_the_root_entry_then_the_empty_message() {
    let lines = left_pane(
        &list_with(Vec::new()),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashSet::new(),
        &HashMap::new(),
        80,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[1].contains("workspace root"));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains("no sessions"));
    let hint = console::strip_ansi_codes(&lines[3]);
    assert!(hint.starts_with(&" ".repeat(NAME_PREFIX)));
    assert!(hint[NAME_PREFIX..].starts_with("no sessions"));
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
        &HashMap::new(),
        30,
        5,
        false,
        Sidebar::Full,
        now,
    );
    // Root (2 lines) + divider + the session's 2 lines: the freshness label sits
    // on the session's detail line (index 4).
    let detail = console::strip_ansi_codes(&lines[4]);
    assert!(
        detail.contains("5分前"),
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
        &HashMap::new(),
        40,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    // The session's detail line (index 4) carries the `↑N ↓M` commit marker.
    let detail = console::strip_ansi_codes(&lines[4]);
    assert!(
        detail.contains("↑2 ↓1"),
        "{detail:?} missing the ahead/behind marker"
    );
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
        &HashMap::new(),
        30,
        7,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    // Root (2 lines), a divider, then 2 lines per worktree.
    assert_eq!(lines.len(), 7);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains("main"));
    assert!(lines[5].contains("feature"));
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
        &HashMap::new(),
        30,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        8,
        5,
        false,
        Sidebar::Rail,
        Utc::now(),
    );
    assert_eq!(lines.len(), 5);
}

#[test]
fn left_pane_marks_the_agent_state_through_its_lifecycle() {
    let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let empty = HashSet::new();
    // Rows: 0/1 root, 2 divider, 3 worktree identity, 4 worktree detail.
    // Live but no turn yet: `☾ ready`.
    let ready = left_pane(
        &list,
        &path,
        &empty,
        &empty,
        &empty,
        &HashMap::new(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert!(ready[4].contains('☾'));
    assert!(ready[4].contains("ready"));
    // Working a turn: `▶ running`.
    let running = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &HashMap::new(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert!(running[4].contains('▶'));
    assert!(running[4].contains("running"));
    // Awaiting input wins over running: `◆ waiting`.
    let waiting = left_pane(
        &list,
        &path,
        &path,
        &path,
        &empty,
        &HashMap::new(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        30,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert!(!absent[4].contains('▶'));
    assert!(!absent[4].contains('◆'));
    assert!(!absent[4].contains('☾'));
    assert!(absent[3].contains("local"));
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
        &HashMap::new(),
        30,
        4,
        false,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        30,
        7,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    // The root is not active; the active "feature" row carries the green `▎`
    // accent bar down both of its lines (identity + detail).
    assert!(!lines[0].contains('▎'));
    assert!(lines[5].contains("feature"));
    assert!(lines[5].contains('▎'));
    assert!(lines[6].contains('▎'));
}

#[test]
fn rail_collapses_each_entry_to_two_rows_without_names_or_numbers() {
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
        &HashMap::new(),
        RAIL_WIDTH,
        8,
        false,
        Sidebar::Rail,
        Utc::now(),
    );
    // Root (2 rows), a divider, then 2 rows per worktree — the same shape as the
    // full sidebar, so toggling never shifts an entry to a different row.
    assert_eq!(lines.len(), 7);
    let plain: Vec<String> = lines
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();
    // No names and no numbers on the rail.
    assert!(!plain.iter().any(|l| l.contains("feature")));
    assert!(!plain.iter().any(|l| l.chars().any(|c| c.is_ascii_digit())));
    // Root glyph on row 1, divider, then each worktree's kind dot + git-status
    // glyph share row 1 (the 2×2 grid's top half).
    assert!(plain[0].contains('⌂'));
    assert!(plain[2].contains('─'));
    assert!(plain[3].contains('●')); // fresh heat dot (main, just touched)
    assert!(plain[3].contains(PUSHED_ICON)); // main's git status
    assert!(plain[5].contains('●')); // fresh heat dot (feature, just touched)
    assert!(plain[5].contains(LOCAL_ICON)); // feature's git status
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
            &HashMap::new(),
            30,
            20,
            false,
            sidebar,
            Utc::now(),
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
            &HashMap::new(),
            30,
            20,
            false,
            sidebar,
            Utc::now(),
        )
        .len()
    };
    assert_eq!(empty_mk(Sidebar::Full), empty_mk(Sidebar::Rail));
}

#[test]
fn rail_shows_the_active_bar_down_both_rows_and_the_agent_glyph_on_row_two() {
    let mut list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    list.activate_by_name("feature");
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let empty = HashSet::new();
    // "feature" is active and running: the green `▎` bar runs down both of its
    // rows, the kind dot is on row 1, and the running glyph on row 2.
    let lines = left_pane(
        &list,
        &path,
        &path,
        &empty,
        &empty,
        &HashMap::new(),
        RAIL_WIDTH,
        8,
        false,
        Sidebar::Rail,
        Utc::now(),
    );
    let top = console::strip_ansi_codes(&lines[5]).into_owned();
    let detail = console::strip_ansi_codes(&lines[6]).into_owned();
    assert!(top.contains('▎'));
    assert!(top.contains('●')); // fresh heat dot on row 1
    assert!(detail.contains('▎'));
    assert!(detail.contains('▶')); // agent state on row 2
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
            &HashMap::new(),
            RAIL_WIDTH,
            8,
            false,
            Sidebar::Rail,
            Utc::now(),
        );
        // Rows: 0/1 root, 2 divider, 3 worktree kind, 4 worktree agent state.
        console::strip_ansi_codes(&lines[4]).into_owned()
    };
    // The rail's detail glyph follows the same lifecycle as the full sidebar's
    // agent label: ready ☾, running ▶, waiting ◆, done ✓.
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
            &HashMap::new(),
            RAIL_WIDTH,
            8,
            true,
            Sidebar::Rail,
            Utc::now(),
        )
    };
    // In 切替 the cursor row shows the `>` marker; here the cursor is on the root.
    let on_root = rail(&list);
    assert!(console::strip_ansi_codes(&on_root[0]).contains('>'));
    // Moving the cursor onto the worktree marks its row 1 and fades the root entry
    // (the cursor leaves it), so the highlighted session still reads first.
    list.move_down();
    let on_session = rail(&list);
    assert!(!console::strip_ansi_codes(&on_session[0]).contains('>'));
    assert!(console::strip_ansi_codes(&on_session[3]).contains('>'));
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
        &HashMap::new(),
        30,
        6,
        true,
        Sidebar::Full,
        Utc::now(),
    );
    assert_eq!(dimmed.len(), 6);
    assert!(console::strip_ansi_codes(&dimmed[0]).contains(ROOT_NAME));
    assert!(console::strip_ansi_codes(&dimmed[3]).contains("main"));
    assert!(console::strip_ansi_codes(&dimmed[5]).contains("feature"));
}

#[test]
fn log_line_colours_each_kind_and_prompts_commands() {
    assert!(log_line(&LogLine::command("man"), 40).contains("❯ man"));
    assert_eq!(log_line(&LogLine::output("plain"), 40), "plain");
    assert!(log_line(&LogLine::error("boom"), 40).contains("boom"));
    assert!(log_line(&LogLine::notice("note"), 40).contains("note"));
}
