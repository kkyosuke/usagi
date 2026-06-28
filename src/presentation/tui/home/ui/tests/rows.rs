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
fn worktree_row_marks_the_cursor_in_switch_and_shows_detached() {
    // The `>` cursor only appears in 切替 (Switch): the selected row carries it
    // when `in_switch` is set. (The kind dot reflects freshness — a just-built
    // fixture is fresh `●`; heat fading is covered in its own test.)
    let (top, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
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
    );
    assert!(!top_no_switch.contains('>'));

    let (other_top, _) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
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
    );
    assert!(!other_top.contains('>'));
    assert!(other_top.contains('●'));
    assert!(other_top.contains("feature"));

    let (detached_top, _) = worktree_row(
        &worktree(None, false, BranchStatus::Local),
        "",
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
    )
    .0;
    let without_note = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
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
            &worktree,
            "",
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
    );
    assert!(ready_detail.contains('☾'));
    assert!(ready_detail.contains("ready"));

    // Working a turn: `▶ running`.
    let (_, running_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
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
    );
    assert!(running_detail.contains('▶'));
    assert!(running_detail.contains("running"));

    // Awaiting input wins over running: `◆ waiting`.
    let (_, waiting_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
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
    );
    assert!(!absent_detail.contains('▶'));
    assert!(!absent_detail.contains('◆'));
    assert!(!absent_detail.contains('☾'));
    assert!(absent_top.contains("local"));
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
    );
    assert!(top.contains("Login flow"));
    assert!(!top.contains("feat-login"));
}

#[test]
fn root_row_marks_selected_and_active() {
    // The `>` cursor shows on the selected root only in 切替 (Switch).
    let (top, detail) = root_row(10, 20, false, true, false, true);
    assert!(top.contains('>'));
    assert!(top.contains('⌂'));
    assert!(top.contains(ROOT_NAME));
    assert!(detail.contains("workspace root"));
    // The same selected root outside Switch shows no cursor.
    let (top_no_switch, _) = root_row(10, 20, false, true, false, false);
    assert!(!top_no_switch.contains('>'));

    // The active root carries the green `▎` bar down both lines, not a `*`.
    let (active_top, active_detail) = root_row(10, 20, false, false, true, false);
    assert!(active_top.contains('▎'));
    assert!(active_detail.contains('▎'));
    assert!(!active_top.contains('*'));

    let (idle_top, idle_detail) = root_row(10, 20, false, false, false, false);
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
    let (with_note, _) = root_row(10, 20, true, false, false, false);
    let (without_note, _) = root_row(10, 20, false, false, false, false);
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
        &HashMap::new(),
        80,
        6,
        false,
        Sidebar::Full,
        Utc::now(),
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
        detail.contains("5min ago"),
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
        &HashMap::new(),
        80,
        9,
        false,
        Sidebar::Full,
        now,
    );
    // Detail lines: small at index 4, big at index 7 (each entry spans three rows).
    let small_detail = console::strip_ansi_codes(&lines[4]);
    let big_detail = console::strip_ansi_codes(&lines[7]);
    assert!(small_detail.contains("☾ ready")); // the live agent's label
    assert!(small_detail.contains("+  5 - 3")); // counts padded to the wide columns
    assert!(big_detail.contains("+140 -88"));
    assert!(big_detail.contains("12min ago"));
    // The diff `+` lands in the same display column on both rows regardless of how
    // many changed lines each carries — the point of the fixed columns.
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
        9,
        false,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        30,
        40,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    let rendered = stripped(&lines);
    // Both workspace names head their blocks (the unite header bar), and both
    // groups' sessions render below their own root.
    assert!(rendered.contains("▌ wsA"));
    assert!(rendered.contains("▌ wsB"));
    assert!(rendered.contains("a1"));
    assert!(rendered.contains("b1"));
    // The header precedes its workspace's session.
    let a_header = rendered.find("▌ wsA").unwrap();
    let b_header = rendered.find("▌ wsB").unwrap();
    assert!(a_header < rendered.find("a1").unwrap());
    assert!(a_header < b_header);
}

#[test]
fn rail_pane_in_unite_mode_separates_each_workspace() {
    // The collapsed rail stacks both workspaces too: two root entries, separated
    // by a rule, with each group's session below its root.
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
        &HashMap::new(),
        30,
        40,
        false,
        Sidebar::Rail,
        Utc::now(),
    );
    let rendered = stripped(&lines);
    // The unite group separator (a heavy rule) appears between the two workspaces.
    assert!(rendered.contains('━'));
}

#[test]
fn left_pane_in_unite_mode_shows_an_empty_workspaces_message() {
    // A stacked workspace with no sessions still shows its header, root, divider,
    // and the empty message under it.
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
        &HashMap::new(),
        30,
        40,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    let rendered = stripped(&full);
    assert!(rendered.contains("a1")); // wsA's session
    assert!(rendered.contains("▌ wsB")); // the empty workspace still gets a header
    assert!(rendered.contains(EMPTY_MESSAGE)); // and the "no sessions" message
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
        &HashMap::new(),
        30,
        5,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert_eq!(lines.len(), 5);
    let rendered = stripped(&lines);
    assert!(rendered.contains("▌ wsA"));
    assert!(!rendered.contains("wsB")); // the second group was never reached
}

#[test]
fn rail_pane_stops_at_a_later_group_once_the_rail_is_full() {
    // The first (empty) workspace fills the rail — root (2) + divider + blank = 4
    // rows — so the second group's rule and rows are never built.
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
        &HashMap::new(),
        30,
        4,
        false,
        Sidebar::Rail,
        Utc::now(),
    );
    assert_eq!(lines.len(), 4);
    // The second group's heavy separator was never built.
    assert!(!stripped(&lines).contains('━'));
}

#[test]
fn sidebar_row_at_line_walks_a_single_group_layout() {
    // root (lines 0,1) → row 0, divider (2) → none, then 3 lines per worktree.
    let list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    assert_eq!(sidebar_row_at_line(&list, 0), Some(0)); // root id
    assert_eq!(sidebar_row_at_line(&list, 1), Some(0)); // root detail
    assert_eq!(sidebar_row_at_line(&list, 2), None); // divider
    assert_eq!(sidebar_row_at_line(&list, 3), Some(1)); // main
    assert_eq!(sidebar_row_at_line(&list, 6), Some(2)); // feature
    assert_eq!(sidebar_row_at_line(&list, 99), None); // past the end
}

#[test]
fn sidebar_row_at_line_walks_a_unite_layout_with_headers() {
    // Two groups; each is headed by a name line (unite), then root (2), divider,
    // then sessions. Flat rows run across groups: wsA root=0, a1=1, wsB root=2,
    // b1=3. Layout lines: 0 hdrA, 1-2 root, 3 div, 4-6 a1, 7 hdrB, 8-9 root,
    // 10 div, 11-13 b1.
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
    assert_eq!(sidebar_row_at_line(&list, 0), None); // wsA header
    assert_eq!(sidebar_row_at_line(&list, 1), Some(0)); // wsA root
    assert_eq!(sidebar_row_at_line(&list, 3), None); // wsA divider
    assert_eq!(sidebar_row_at_line(&list, 4), Some(1)); // a1
    assert_eq!(sidebar_row_at_line(&list, 7), None); // wsB header
    assert_eq!(sidebar_row_at_line(&list, 8), Some(2)); // wsB root
    assert_eq!(sidebar_row_at_line(&list, 10), None); // wsB divider
    assert_eq!(sidebar_row_at_line(&list, 11), Some(3)); // b1
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
    // wsA: 0 hdr, 1-2 root, 3 div, 4 empty message.
    assert_eq!(sidebar_row_at_line(&list, 1), Some(0)); // wsA root
    assert_eq!(sidebar_row_at_line(&list, 4), None); // empty-workspace message
                                                     // wsB: 5 hdr, 6-7 root, 8 div, 9-11 b1.
    assert_eq!(sidebar_row_at_line(&list, 6), Some(1)); // wsB root
    assert_eq!(sidebar_row_at_line(&list, 9), Some(2)); // b1
}

#[test]
fn row_select_click_works_in_unite_mode_but_pr_mouse_stays_single_group() {
    // The row-select click maps to the right session across groups; the PR
    // click/hover (whose popup geometry is single-group) is disabled in unite.
    let mut state = state_with_sessions(&["main"]); // primary "usagi" with one session
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![SessionRecord {
            name: "b1".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/wsB/.usagi/sessions/b1"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }],
    }]);
    // Body line 4 (header 0, root 1-2, divider 3, session 4) is the primary's
    // session → flat row 1. Screen row = CHROME_TOP_ROWS (3) + 4 = 7.
    assert_eq!(left_pane_session_at(&state, 2, 7, 24, 120), Some(1));
    // The PR affordance is still off in unite (its popup geometry is single-group).
    assert!(sidebar_pr_links_at(&state, 24, 120, 2, 7).is_empty());
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 7), None);
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
        &resources,
        40,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert!(with_usage[4].contains("running"));
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
        &HashMap::new(),
        40,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    assert!(without[4].contains("running"));
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
        &resources,
        40,
        8,
        true,
        Sidebar::Full,
        Utc::now(),
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
        9,
        false,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        RAIL_WIDTH,
        9,
        false,
        Sidebar::Rail,
        Utc::now(),
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
    // Root glyph on row 1, divider, then each worktree's kind dot + git-status
    // glyph share row 1 (the 2×2 grid's top half).
    assert!(plain[0].contains('⌂'));
    assert!(plain[2].contains('─'));
    assert!(plain[3].contains('●')); // fresh heat dot (main, just touched)
    assert!(plain[3].contains(PUSHED_ICON)); // main's git status
    assert!(plain[6].contains('●')); // fresh heat dot (feature, just touched)
    assert!(plain[6].contains(LOCAL_ICON)); // feature's git status
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
        &HashMap::new(),
        RAIL_WIDTH,
        9,
        false,
        Sidebar::Rail,
        Utc::now(),
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
        9,
        true,
        Sidebar::Full,
        Utc::now(),
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
        &HashMap::new(),
        30,
        8,
        false,
        Sidebar::Full,
        Utc::now(),
    );
    let rendered = stripped(&lines);
    // The first session (rows 3,4 after the root pair + divider) shows `<icon> 1`.
    assert!(rendered.contains(format!("{PR_ICON} 1").as_str()));
    // The plain session contributes no PR, so the icon appears exactly once.
    assert_eq!(rendered.matches(PR_ICON).count(), 1);
}

/// An attached (没入) state at 120×24 with the full sidebar, listing a session that
/// carries two PRs followed by one with no PR — the fixture the
/// `sidebar_pr_links_at` click tests share. Worktree rows start at screen row 6
/// (the body begins at row 3; the root entry and divider take its first 3 lines),
/// three screen rows each: the PR session at rows 6–8, the PR-less one at rows 9–11.
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
fn sidebar_pr_links_at_opens_every_pr_when_the_badge_is_clicked() {
    let state = attached_with_pr_sidebar();
    // The left pane is 40 columns at width 120, so the folded `<icon> <count>` badge
    // (here ` 2`, three columns wide) seats flush at its right edge — columns 37–39
    // on the entry's detail line (row 7, the second of its three rows). A click on
    // the badge opens every PR the session carries, in order.
    assert_eq!(
        sidebar_pr_links_at(&state, 24, 120, 37, 7),
        vec![
            "https://github.com/o/r/pull/412".to_string(),
            "https://github.com/o/other/pull/98".to_string(),
        ],
    );
    assert_eq!(sidebar_pr_links_at(&state, 24, 120, 39, 7).len(), 2);
    // Left of the badge (the agent-label side of the detail line) opens nothing.
    assert!(sidebar_pr_links_at(&state, 24, 120, 33, 7).is_empty());
    assert!(sidebar_pr_links_at(&state, 24, 120, 10, 7).is_empty());
}

#[test]
fn sidebar_pr_links_at_ignores_the_rows_other_than_the_detail_line() {
    let state = attached_with_pr_sidebar();
    // The badge columns on the identity line (row 6) and the CPU / memory line
    // (row 8) of the same session carry no badge, so a click there opens nothing.
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 6).is_empty());
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 8).is_empty());
}

#[test]
fn sidebar_pr_links_at_ignores_rows_without_a_pr() {
    let state = attached_with_pr_sidebar();
    // The second session's detail line (row 10 of rows 9–11) has no PR.
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 10).is_empty());
    // The root entry (rows 3–4) and the divider (row 5) are not session rows.
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 4).is_empty());
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 5).is_empty());
    // A body row past the end of the session list maps to no worktree.
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 13).is_empty());
}

#[test]
fn sidebar_pr_links_at_ignores_clicks_off_the_sidebar() {
    let state = attached_with_pr_sidebar();
    // Left pane is 40 columns at width 120, so a click at column 40+ is the
    // divider / right pane, not a sidebar row.
    assert!(sidebar_pr_links_at(&state, 24, 120, 40, 7).is_empty());
    // Rows above the body (the title bar / mode ladder / blank separator).
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 1).is_empty());
    // A row below the two-pane body (past `body_rows`).
    assert!(sidebar_pr_links_at(&state, 24, 120, 38, 22).is_empty());
}

#[test]
fn sidebar_pr_links_at_is_empty_on_the_collapsed_rail() {
    let mut state = attached_with_pr_sidebar();
    // The rail shows no PR badge, so a click there opens nothing.
    state.set_sidebar(Sidebar::Rail);
    assert!(sidebar_pr_links_at(&state, 24, 120, 3, 7).is_empty());
}

#[test]
fn sidebar_pr_links_at_ignores_a_badge_clipped_by_a_cramped_pane() {
    let state = attached_with_pr_sidebar();
    // On a very narrow screen the left pane shrinks until the folded badge can no
    // longer seat flush-right past the name indent, so its columns can't be placed —
    // a click opens nothing rather than guessing. At width 9 the left pane is 6
    // columns and the 3-column badge would start at column 3, inside `NAME_PREFIX`.
    assert!(sidebar_pr_links_at(&state, 24, 9, 3, 7).is_empty());
}

#[test]
fn sidebar_pr_hover_at_maps_a_pr_row_to_its_session_and_misses_elsewhere() {
    let state = attached_with_pr_sidebar();
    // Both rows of the PR-bearing session (rows 6 and 7) hover its index.
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 6), Some(0));
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 30, 7), Some(0));
    // The PR-less second session (rows 9–11), the root entry / divider, a row past
    // the list, and a row below the two-pane body all raise no popup.
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 9), None);
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 3), None);
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 5), None);
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 12), None);
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 22), None);
    // Off the sidebar (right pane / chrome) and on the collapsed rail, nothing.
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 40, 6), None);
    assert_eq!(sidebar_pr_hover_at(&state, 24, 120, 2, 1), None);
    let mut rail = attached_with_pr_sidebar();
    rail.set_sidebar(Sidebar::Rail);
    assert_eq!(sidebar_pr_hover_at(&rail, 24, 120, 2, 6), None);
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
    // Below the only session (rows 6,7,8) the rows are mascot / blank filler.
    assert_eq!(at(0, 9), None);
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
