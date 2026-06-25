use super::chrome::*;
use super::panes::*;
use super::*;

use super::super::command::{CommandHint, CommandInfo};
use super::super::state::{LogLine, Preview, TextModal, WorktreeList, ROOT_NAME};
use super::super::terminal_pool::MonitorSnapshot;
use super::super::terminal_view::TerminalView;
use crate::domain::settings::{SessionActionUi, Sidebar};
use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Span, SpanStyle};
use chrono::Utc;
use std::collections::HashSet;
use std::path::PathBuf;

fn worktree(branch: Option<&str>, primary: bool, status: BranchStatus) -> WorktreeState {
    WorktreeState {
        branch: branch.map(|b| b.to_string()),
        path: PathBuf::from("/repo/wt"),
        head: "abc1234".to_string(),
        primary,
        upstream: None,
        status,
        updated_at: Utc::now(),
    }
}

fn list_with(worktrees: Vec<WorktreeState>) -> WorktreeList {
    WorktreeList::new("usagi", worktrees)
}

fn state_with(worktrees: Vec<WorktreeState>) -> HomeState {
    HomeState::new("usagi", worktrees, None)
}

fn stripped(lines: &[String]) -> String {
    console::strip_ansi_codes(&lines.join("\n")).into_owned()
}

#[test]
fn text_modal_frame_windows_a_long_dump_with_more_counts() {
    let lines: Vec<LogLine> = (0..30)
        .map(|i| LogLine::output(format!("entry {i}")))
        .collect();
    let modal = TextModal {
        title: "Help".to_string(),
        lines,
        scroll: 5,
    };
    let frame = stripped(&text_modal_frame(40, 120, &modal));
    // The title and the hidden-line counts above and below the window show.
    assert!(frame.contains("Help"));
    assert!(frame.contains("↑ 5 more"));
    // 30 total - (scroll 5 + 16 visible) = 9 hidden below.
    assert!(frame.contains("↓ 9 more"));
    // A windowed line is visible; ones outside the window are not.
    assert!(frame.contains("entry 5"));
    assert!(!frame.contains("entry 0"));
    assert!(frame.contains("Esc / Enter / q: close"));
}

#[test]
fn text_modal_frame_shows_a_short_dump_without_scroll_counts() {
    let modal = TextModal {
        title: "History".to_string(),
        lines: vec![LogLine::output("  1  man"), LogLine::output("  2  history")],
        scroll: 0,
    };
    let frame = stripped(&text_modal_frame(40, 120, &modal));
    assert!(frame.contains("History"));
    assert!(frame.contains("man"));
    assert!(!frame.contains("more"));
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
fn worktree_row_marks_selected_primary_and_detached() {
    // The `>` cursor only appears in 切替 (Switch): the selected row carries it
    // when `in_switch` is set.
    let (top, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        "",
        10,
        10,
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
        false,
        false,
        true,
        false,
        false,
        false,
        false,
    );
    assert!(!other_top.contains('>'));
    assert!(other_top.contains('○'));
    assert!(other_top.contains("feature"));

    let (detached_top, _) = worktree_row(
        &worktree(None, false, BranchStatus::Local),
        "",
        10,
        10,
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
fn worktree_row_marks_the_active_worktree_with_a_gutter_bar_on_both_lines() {
    let (active_top, active_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        "",
        10,
        10,
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
        80,
        6,
        false,
        Sidebar::Full,
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
        30,
        7,
        false,
        Sidebar::Full,
    );
    // Root (2 lines), a divider, then 2 lines per worktree.
    assert_eq!(lines.len(), 7);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('─'));
    assert!(lines[3].contains("main"));
    assert!(lines[5].contains("feature"));
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
        30,
        6,
        false,
        Sidebar::Full,
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
        30,
        6,
        false,
        Sidebar::Full,
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
        30,
        6,
        false,
        Sidebar::Full,
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
        30,
        6,
        false,
        Sidebar::Full,
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
        30,
        4,
        false,
        Sidebar::Full,
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
        30,
        7,
        false,
        Sidebar::Full,
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
        RAIL_WIDTH,
        8,
        false,
        Sidebar::Rail,
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
    assert!(plain[3].contains('●')); // primary main
    assert!(plain[3].contains(PUSHED_ICON)); // main's git status
    assert!(plain[5].contains('○')); // ordinary feature
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
            &list, &empty, &empty, &empty, &empty, 30, 20, false, sidebar,
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
            30,
            20,
            false,
            sidebar,
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
        RAIL_WIDTH,
        8,
        false,
        Sidebar::Rail,
    );
    let top = console::strip_ansi_codes(&lines[5]).into_owned();
    let detail = console::strip_ansi_codes(&lines[6]).into_owned();
    assert!(top.contains('▎'));
    assert!(top.contains('○')); // kind dot on row 1
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
            RAIL_WIDTH,
            8,
            false,
            Sidebar::Rail,
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
            RAIL_WIDTH,
            8,
            true,
            Sidebar::Rail,
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
        30,
        6,
        true,
        Sidebar::Full,
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

// --- right pane by mode ------------------------------------------------

#[test]
fn right_pane_previews_the_cursor_row_in_switch() {
    // 切替 (Switch) is the default mode: the right pane previews the would-be
    // screen for the cursor row.
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert_eq!(state.mode(), Mode::Switch);
    let preview = stripped(&right_pane_contents(&state, 40, 12));
    // The root row previews its action menu (the workspace-root note shows).
    assert!(preview.contains("root"));
    assert!(preview.contains("workspace root"));
    assert!(preview.contains("terminal"));
}

#[test]
fn switch_preview_shows_a_live_session_as_a_reattach() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.enter_switch(super::super::state::ReturnMode::Base);
    // Move the cursor off the root onto the session row.
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("feat"));
    // Header carries the git status and the agent state. The session is live but
    // has reported no turn, so it shows as `ready` (idle, awaiting input).
    assert!(preview.contains("local"));
    assert!(preview.contains("ready"));
    // A live session with no snapshot yet falls back to the re-attach label,
    // not the action menu.
    assert!(preview.contains("live terminal"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_a_live_session_as_its_actual_screen() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    // The event loop snapshots the highlighted live session before painting.
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ echo hi".to_string(), "hi".to_string()],
        None,
    ));
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    // The real terminal screen is shown, not the placeholder label.
    assert!(preview.contains("$ echo hi"));
    assert!(preview.contains("hi"));
    assert!(!preview.contains("live terminal"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_the_tab_strip_beside_the_header_for_a_live_session() {
    // In 切替 the highlighted live session's tabs render on the header's own row,
    // so the identity and the `←`/`→` targets read together; the live screen
    // follows below. The event loop publishes the strip from the pool before
    // painting.
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let lines = switch_preview(&state, 80, 12);
    // The header (name + status + agent) and both numbered chips share the top
    // row; the live screen follows below.
    let top = console::strip_ansi_codes(&lines[0]).into_owned();
    assert!(top.contains("feat") && top.contains("ready"));
    // A dim divider separates the identity from the tab chips.
    assert!(top.contains('│'));
    assert!(top.contains("1 agent") && top.contains("2 terminal"));
    assert!(stripped(&lines).contains("$ echo hi"));
}

#[test]
fn switch_preview_keeps_a_fixed_identity_width_so_tabs_do_not_jitter() {
    // The header identity is a fixed width, so the divider and tabs land in the
    // same column whichever session the cursor is on — the row does not shift as
    // the cursor moves between sessions — and a long name is clipped, not spilled.
    let mut short = worktree(Some("x"), false, BranchStatus::Local);
    short.path = PathBuf::from("/repo/short");
    let mut long = worktree(
        Some("feature/really-long-branch-name-here"),
        false,
        BranchStatus::Synced,
    );
    long.path = PathBuf::from("/repo/long");
    let mut state = HomeState::new("usagi", vec![short, long], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/short"), PathBuf::from("/repo/long")].into(),
        ..Default::default()
    });
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ ".to_string()], None));
    state.enter_switch(super::super::state::ReturnMode::Base);

    // The display column of the divider, used as the anchor for the tabs.
    let divider_col = |lines: &[String]| {
        let top = console::strip_ansi_codes(&lines[0]).into_owned();
        let at = top.find('│').expect("the divider is drawn");
        console::measure_text_width(&top[..at])
    };

    state.switch_move_down(); // cursor on the short-named session
    let short_top = switch_preview(&state, 80, 12);
    let short_col = divider_col(&short_top);

    state.switch_move_down(); // cursor on the long-named session
    let long_lines = switch_preview(&state, 80, 12);
    let long_col = divider_col(&long_lines);

    // The divider (and so the tabs beside it) sits in the same column for both.
    assert_eq!(short_col, long_col);
    // The long name is clipped with an ellipsis rather than pushing the divider.
    assert!(console::strip_ansi_codes(&long_lines[0])
        .into_owned()
        .contains('…'));
}

#[test]
fn switch_preview_shows_the_root_live_session_as_its_screen() {
    // Regression: the root row (`⌂ root`) hard-coded `live = false`, so a running
    // root agent previewed its action menu in 切替 instead of its live screen —
    // it only re-appeared once selected. With the workspace root recorded, the
    // root row is matched against the live set like any worktree, so its running
    // agent previews live.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo")].into(),
        ..Default::default()
    });
    // The event loop snapshots the highlighted live session (the root here)
    // before painting.
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ claude".to_string(), "How can I help?".to_string()],
        None,
    ));
    state.enter_switch(super::super::state::ReturnMode::Base);
    // The cursor starts on the root row, so no navigation is needed.
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("root"));
    // The live root agent's actual screen is shown, not the action menu.
    assert!(preview.contains("$ claude"));
    assert!(preview.contains("How can I help?"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_an_idle_root_as_its_action_menu() {
    // The mirror of the regression above: a root with no live embedded session
    // still previews the action menu it would open, even with the root path set.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.enter_switch(super::super::state::ReturnMode::Base);
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("workspace root"));
    assert!(preview.contains("Run a command"));
    assert!(!preview.contains("live terminal"));
}

#[test]
fn switch_preview_shows_an_idle_session_as_its_action_menu() {
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    // An idle session previews the 在席 action menu it would open.
    assert!(preview.contains("pushed"));
    assert!(preview.contains("Run a command"));
    assert!(preview.contains("terminal"));
    assert!(preview.contains("agent"));
    assert!(!preview.contains("live terminal"));
}

#[test]
fn switch_right_pane_fades_the_preview_but_keeps_its_text() {
    // In 切替 the keyboard is on the session list, so the composited right pane
    // fades the whole preview (each row dimmed) to signal it is not where
    // selection happens. The fade only re-styles the rows, so the right pane
    // shows exactly the preview's text. (Styling is off in non-TTY tests, so we
    // assert the text survives rather than that a dim code was added — see
    // `dim_row_strips_existing_colour_but_keeps_the_text`.)
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    let pane = stripped(&right_pane_contents(&state, 40, 12));
    assert_eq!(pane, preview, "the faded right pane is the preview's text");
}

#[test]
fn switch_preview_fills_the_pane_without_a_pinned_key_hint() {
    // The 切替 right pane no longer pins a key hint to its bottom row — the keys
    // live in the footer, so the preview uses the pane's full height and does not
    // duplicate the footer's key list.
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = switch_preview(&state, 60, 12);
    // The pane fills its rows, and the bottom row is no longer a key hint.
    assert_eq!(preview.len(), 12);
    let last = console::strip_ansi_codes(preview.last().unwrap()).into_owned();
    assert!(!last.contains("Enter focus"));
    assert!(!last.contains("x close tab"));
    // The action-menu preview is still shown.
    assert!(stripped(&preview).contains("Run a command"));
}

#[test]
fn switch_preview_shows_an_idle_session_as_its_prompt_when_prompt_ui() {
    // With the Prompt action UI, the idle-session preview must mirror the prompt
    // surface (`❯`) — not the command menu — so the 切替 preview matches what
    // focusing the session actually reveals (regression: it previewed the menu
    // regardless of the setting).
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("pushed"));
    assert!(preview.contains('❯'), "the prompt surface is previewed");
    assert!(
        !preview.contains("Run a command"),
        "the command menu must not be shown in prompt mode"
    );
    assert!(!preview.contains("live terminal"));
}

#[test]
fn right_pane_shows_the_focus_menu_or_prompt() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    // Menu (the default) lists the session commands.
    let menu = stripped(&right_pane_contents(&state, 40, 12));
    assert!(menu.contains("session: main"));
    assert!(menu.contains("terminal"));
    assert!(menu.contains("agent"));
    assert!(menu.contains('›'));

    // Prompt shows a typed command line with the session-scope hint.
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "ter".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let prompt = stripped(&right_pane_contents(&state, 40, 12));
    assert!(prompt.contains("session: main"));
    assert!(prompt.contains("❯ ter"));
    // The session-scope hint lists terminal as a match.
    assert!(prompt.contains("terminal"));
}

#[test]
fn focus_menu_agent_row_shows_the_default_and_expands_into_a_picker() {
    use crate::domain::settings::AgentCli;
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    // The agent row always names the default CLI a plain launch uses.
    let base = stripped(&right_pane_contents(&state, 50, 16));
    assert!(base.contains("Launch Claude"));
    // The expand affordance (▸ / "→ pick agent") shows once the agent row is the
    // highlighted one (terminal is highlighted on entry).
    assert!(!base.contains("→ pick agent"));
    state.focus_menu_move_down(); // terminal -> agent
    let on_agent = stripped(&right_pane_contents(&state, 50, 16));
    assert!(on_agent.contains('▸'));
    assert!(on_agent.contains("→ pick agent"));
    // Expanding lists every installed agent (default tagged) and swaps the hint.
    state.focus_menu_expand_agent();
    let expanded = stripped(&right_pane_contents(&state, 50, 16));
    assert!(expanded.contains('▾'));
    assert!(expanded.contains("Codex"));
    assert!(expanded.contains("(default)"));
    assert!(expanded.contains("Enter launch"));
}

#[test]
fn focus_shows_pane_tabs_with_a_trailing_new_tab_and_the_action_surface() {
    // With live panes published, 在席 gains a tab strip — one chip per pane plus a
    // trailing "+ new" tab. On the "+ new" tab (the default on entry) the action
    // surface shows below, not a pane preview.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    // A wide pane so the whole strip (identity + chips + "+ new") fits unclipped.
    let out = stripped(&right_pane_contents(&state, 100, 12));
    // The identity rides the strip row alongside the pane chips and the "+ new" tab.
    assert!(out.contains("main"));
    assert!(out.contains("agent"));
    assert!(out.contains("+ new"));
    // On the "+ new" tab the action surface shows; the pane preview does not.
    assert!(out.contains("Run a command:"));
    assert!(!out.contains("$ echo hi"));
}

#[test]
fn focus_new_tab_with_panes_shows_the_prompt_surface() {
    // The "+ new" tab honours the Prompt action UI just as the menu does — with
    // live panes its command line shows below the strip (header-less).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    for c in "ter".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let out = stripped(&right_pane_contents(&state, 100, 12));
    assert!(out.contains("+ new"));
    assert!(out.contains("❯ ter"));
}

#[test]
fn focus_previews_the_selected_pane_when_a_pane_tab_is_chosen() {
    // Off the "+ new" tab (as after navigating onto a pane tab with `Ctrl-N`/`Ctrl-P`),
    // the selected pane's live snapshot previews below the strip instead of the
    // action surface, and the "+ new" chip is gone — it shows only while selected.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.focus_tab_prev(); // "+ new" -> the last (active) pane tab
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let out = stripped(&right_pane_contents(&state, 100, 12));
    assert!(!out.contains("+ new")); // stepping off "+ new" drops the chip
    assert!(out.contains("$ echo hi")); // the selected pane previews
    assert!(!out.contains("Run a command:")); // not the action surface
}

#[test]
fn focus_pane_tab_falls_back_to_a_hint_until_the_first_snapshot() {
    // A pane tab is selected but no snapshot has arrived yet: a live-terminal hint
    // stands in until one does.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.focus_tab_next(); // "+ new" -> the sole pane tab
    let out = stripped(&right_pane_contents(&state, 60, 12));
    assert!(out.contains("live terminal"));
    assert!(out.contains("再アタッチ"));
}

#[test]
fn focus_prompt_shows_usage_for_arguments() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "terminal ".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let prompt = stripped(&right_pane_contents(&state, 60, 12));
    assert!(prompt.contains("usage"));
    assert!(prompt.contains("terminal"));
}

#[test]
fn focus_prompt_has_no_hint_for_an_unknown_command_word() {
    // An unknown word yields `Hint::None`, so no hint rows are drawn.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "zzz".chars() {
        state.focus_prompt_mut().insert(c);
    }
    // The header, blank, and prompt lines are present, but no hint rows follow.
    let rows = right_pane_contents(&state, 60, 12);
    let text = stripped(&rows);
    assert!(text.contains("❯ zzz"));
    // An unknown word yields `Hint::None`, so neither a usage line nor example
    // rows are drawn below the prompt.
    assert!(!text.contains("usage"));
    assert!(!text.contains("e.g."));
}

#[test]
fn right_pane_shows_the_terminal_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    // The active session's header tops the pane, filling the reserved tab-strip
    // rows; the terminal (or its starting hint) follows below.
    let starting = right_pane_contents(&state, 40, 5);
    assert!(console::strip_ansi_codes(&starting[0])
        .into_owned()
        .contains("main"));
    assert!(starting[super::TAB_BAR_ROWS].contains("Starting terminal"));
    // Once a snapshot arrives, its rows are shown below the header.
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let running = right_pane_contents(&state, 40, 5);
    assert!(running[super::TAB_BAR_ROWS].contains("$ echo hi"));
}

#[test]
fn right_pane_shows_the_tab_strip_above_the_terminal_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let rows = right_pane_contents(&state, 80, 5);
    // The strip takes the top two rows — the identity + chips, then the active-tab
    // underline — listing both panes; the terminal follows below them.
    let chips = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(chips.contains("agent") && chips.contains("terminal"));
    assert!(rows[2].contains("$ echo hi"));
}

#[test]
fn attached_header_shows_the_active_session_identity_beside_the_tabs() {
    // 没入 carries the same identity line as 切替: the active session's name, git
    // status, and agent state on the top row, sharing it with the tab chips.
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        running: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let rows = right_pane_contents(&state, 80, 8);
    let header = console::strip_ansi_codes(&rows[0]).into_owned();
    // Name + status + agent state, then the numbered chips, all on the top row.
    assert!(header.contains("feat") && header.contains("local") && header.contains("running"));
    assert!(header.contains("1 agent") && header.contains("2 terminal"));
    // The terminal follows below the reserved tab-strip rows.
    assert!(rows[super::TAB_BAR_ROWS].contains("$ echo hi"));
}

#[test]
fn attached_header_shows_the_root_note_when_the_root_is_active() {
    // With the workspace root active, the 没入 header mirrors the 切替 root row:
    // the root name and its `workspace root` note (no git status / agent).
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.enter_focus(0);
    state.show_attached();
    let header = console::strip_ansi_codes(&right_pane_contents(&state, 60, 6)[0]).into_owned();
    assert!(header.contains(ROOT_NAME));
    assert!(header.contains("workspace root"));
}

#[test]
fn header_tab_rows_number_each_pane_beside_the_header_and_clip_to_width() {
    use super::super::terminal_tabs::TabStrip;
    // Styling is stripped in the (non-TTY) test environment, so assert on content.
    let strip = TabStrip {
        labels: vec!["agent".to_string(), "terminal".to_string()],
        active: 0,
    };
    // Header + chips on the top row; the active-tab underline on the row below.
    let rows = header_tab_rows("feat".to_string(), Some(&strip), 80);
    assert_eq!(rows.len(), 2);
    let chips = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(chips.contains("feat"));
    // Each chip is 1-based numbered to match the ←/→ order.
    assert!(chips.contains("1 agent") && chips.contains("2 terminal"));
    // The marker row underlines the active (first) chip.
    assert!(console::strip_ansi_codes(&rows[1])
        .into_owned()
        .contains('▔'));

    // A narrow pane clips both rows to its width.
    let narrow = header_tab_rows("feat".to_string(), Some(&strip), 8);
    assert!(narrow.iter().all(|l| console::measure_text_width(l) <= 8));

    // No strip — or an empty one — leaves the header alone on a single row.
    assert_eq!(header_tab_rows("feat".to_string(), None, 40).len(), 1);
    let empty = TabStrip {
        labels: Vec::new(),
        active: 0,
    };
    assert_eq!(
        header_tab_rows("feat".to_string(), Some(&empty), 40).len(),
        1
    );
}

#[test]
fn focus_menu_row_marks_the_cursor() {
    let info = CommandInfo {
        name: "terminal",
        description: "Open a shell",
        usage: "terminal",
        examples: &[],
        scope: super::super::command::CommandScope::Session,
    };
    let selected = console::strip_ansi_codes(&focus_menu_row(&info, true, 60)).into_owned();
    assert!(selected.contains('›'));
    assert!(selected.contains("terminal"));
    let idle = console::strip_ansi_codes(&focus_menu_row(&info, false, 60)).into_owned();
    assert!(!idle.contains('›'));
}

#[test]
fn terminal_pane_clips_rows_to_the_pane_width() {
    let view = TerminalView::from_rows(
        vec!["a long command line".to_string(), "$ ".to_string()],
        Some((1, 2)),
    );
    let lines = terminal_pane(&view, 8, 5);
    assert_eq!(lines.len(), 2);
    assert!(console::measure_text_width(&lines[0]) <= 8);
    assert!(lines[0].ends_with('…'));
    assert!(lines[1].starts_with("$ "));
}

#[test]
fn terminal_geometry_matches_the_rendered_layout() {
    let geo = terminal_geometry(24, 80, Sidebar::Full);
    let (left, _) = layout(80, Sidebar::Full);
    assert_eq!(geo.origin_col as usize, left + SEP_WIDTH);
    // Three chrome rows above the body (title + mode ladder + blank separator).
    assert_eq!(geo.origin_row, 3);
    // 24 rows less the three above and two below (input + footer).
    assert_eq!(geo.rows, 19);
    assert_eq!(geo.cols as usize, 80 - left - SEP_WIDTH);
}

#[test]
fn terminal_geometry_stays_positive_in_a_tiny_terminal() {
    let geo = terminal_geometry(1, 1, Sidebar::Full);
    assert!(geo.rows >= 1);
    assert!(geo.cols >= 1);
}

#[test]
fn attached_geometry_reserves_the_tab_strip_row() {
    let full = terminal_geometry(24, 80, Sidebar::Full);
    let attached = attached_geometry(24, 80, Sidebar::Full);
    // The tab strip takes two rows off the top: fewer rows, origin pushed down,
    // same width and left edge.
    assert_eq!(attached.rows as usize, full.rows as usize - TAB_BAR_ROWS);
    assert_eq!(attached.origin_row, full.origin_row + TAB_BAR_ROWS as u16);
    assert_eq!(attached.cols, full.cols);
    assert_eq!(attached.origin_col, full.origin_col);
}

#[test]
fn attached_geometry_stays_positive_in_a_tiny_terminal() {
    let geo = attached_geometry(1, 1, Sidebar::Full);
    assert!(geo.rows >= 1);
    assert!(geo.cols >= 1);
}

#[test]
fn collapsing_the_sidebar_widens_the_terminal_geometry() {
    // The embedded terminal tracks the sidebar: collapsing to the rail moves its
    // origin left and widens it, so the live shell fills the reclaimed columns.
    let full = attached_geometry(24, 80, Sidebar::Full);
    let rail = attached_geometry(24, 80, Sidebar::Rail);
    assert!(rail.cols > full.cols);
    assert!(rail.origin_col < full.origin_col);
    assert_eq!(rail.origin_col as usize, RAIL_WIDTH + SEP_WIDTH);
    // The vertical layout (rows / origin / tab strip) is unaffected.
    assert_eq!(rail.rows, full.rows);
    assert_eq!(rail.origin_row, full.origin_row);
}

#[test]
fn cursor_screen_pos_places_the_cursor_one_past_the_origin() {
    let geo = terminal_geometry(24, 80, Sidebar::Full);
    // A cursor at the pane's top-left maps to the 1-based cell just inside it.
    let (x, y) = geo.cursor_screen_pos(0, 0);
    assert_eq!(x, geo.origin_col + 1);
    assert_eq!(y, geo.origin_row + 1);
    // An interior cursor is offset straight through.
    let (x, y) = geo.cursor_screen_pos(3, 5);
    assert_eq!(x, geo.origin_col + 6);
    assert_eq!(y, geo.origin_row + 4);
}

#[test]
fn cursor_screen_pos_clamps_a_deferred_wrap_onto_the_last_cell() {
    let geo = terminal_geometry(24, 80, Sidebar::Full);
    // vt100 parks the cursor one column/row past the grid on a deferred wrap;
    // the placed cursor must stay on the last cell instead of spilling past the
    // pane (which would jump the real cursor to the screen edge).
    let (x, _) = geo.cursor_screen_pos(0, geo.cols);
    assert_eq!(x, geo.origin_col + geo.cols);
    let (_, y) = geo.cursor_screen_pos(geo.rows, 0);
    assert_eq!(y, geo.origin_row + geo.rows);
}

#[test]
fn render_frame_draws_the_terminal_in_the_right_pane_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ cargo test".to_string()],
        None,
    ));
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("main"));
    assert!(joined.contains("$ cargo test"));
    // The attached footer advertises Ctrl-O.
    assert!(joined.contains("attached"));
}

#[test]
fn note_editor_box_keeps_its_bottom_border_over_a_short_pane() {
    // The note editor floats over the attached session's pane. Even when the pane
    // beneath is shorter than the box — e.g. no terminal snapshot has arrived, so
    // it falls back to a one-line hint — the box must render its bottom border in
    // full as the note grows with each newline, never clipping it off.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    // No snapshot set: the attached pane is the one-line starting hint.
    // `true` = opened from 没入 (`Ctrl-E`), re-attaching on close.
    assert!(state.open_focused_note(true));
    // Type a few lines so the editor box is taller than that short fallback pane.
    let area = state.note_editor_mut().unwrap().area_mut();
    area.insert('a');
    area.newline();
    area.insert('b');
    area.newline();
    area.insert('c');
    let rows = right_pane_contents(&state, 60, 12);
    let plain = stripped(&rows);
    // The box frames the note in full: a titled top border and a bottom border.
    // The title is just `note` (the session is named in the pane header).
    assert!(plain.contains("─ note"));
    assert!(
        rows.iter()
            .any(|r| console::strip_ansi_codes(r).trim_start().starts_with('└')),
        "the box's bottom border must render even over a short pane: {plain}",
    );
}

// --- input / footer by mode --------------------------------------------

#[test]
fn command_palette_frame_renders_the_prompt() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    state.push_char('m');
    let frame = stripped(&command_palette_frame(24, 80, &state));
    assert!(frame.contains('m'));
    assert!(frame.contains('❯'));
    // The palette is titled and footers its keys.
    assert!(frame.contains("Command"));
    assert!(frame.contains("Esc: close"));
}

#[test]
fn command_palette_frame_draws_the_caret_without_shifting_the_text() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.cursor_left();
    // The block caret recolours the character it sits on rather than inserting a
    // glyph, so the text reads intact whatever the caret position. (Where the
    // reverse-video cell lands is covered by `widgets::block_caret`'s own tests.)
    let plain = stripped(&command_palette_frame(24, 80, &state));
    assert!(plain.contains("❯ man"));
}

#[test]
fn command_palette_frame_shows_hints_and_the_latest_response() {
    let mut state = state_with(Vec::new());
    state.open_command_palette();
    // A bare prompt lists the workspace commands as hints.
    let listed = stripped(&command_palette_frame(24, 80, &state));
    assert!(listed.contains("workspace commands"));
    // After running a command its response shows in the band.
    state.open_command_palette();
    for c in "history".chars() {
        state.push_char(c);
    }
    let _ = state.submit();
    let ran = stripped(&command_palette_frame(40, 80, &state));
    // The seeded usage hint is part of the response band's tail.
    assert!(ran.contains("man"));
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
    // 没入 no longer advertises scroll keys in the footer.
    let attached = footer_line(80, &state);
    assert!(attached.contains("attached"));
    assert!(!attached.contains("scroll"));
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
    state.enter_switch(super::super::state::ReturnMode::Base);
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
    state.enter_switch(super::super::state::ReturnMode::Base);
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
    state.enter_switch(super::super::state::ReturnMode::Base);
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

// --- update-available notice -------------------------------------------

#[test]
fn update_banner_pairs_the_mascot_with_the_latest_version() {
    let latest = crate::domain::version::Version::parse("0.2.0").unwrap();
    let banner = update_banner(&latest);
    // 3 行のマスコット ＋ 一番下の空行で計 4 行。
    assert_eq!(banner.len(), 4);
    assert_eq!(banner.last().map(String::as_str), Some(""));
    let plain = stripped(&banner);
    assert!(plain.contains("アップデートがあるぴょん"));
    assert!(plain.contains("v0.2.0"));
    // The usagi mascot rides alongside the notice.
    assert!(plain.contains("(='-')"));
}

#[test]
fn render_frame_shows_the_update_notice_when_a_newer_release_exists() {
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("アップデートがあるぴょん"));
    assert!(joined.contains("v9.9.9"));
}

#[test]
fn render_frame_hides_the_update_notice_by_default() {
    let state = state_with(Vec::new());
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

// --- background-task panel ---------------------------------------------

#[test]
fn task_status_line_shows_the_running_lead_its_bar_and_count() {
    use super::super::tasks::{TaskMark, TaskRow};
    // A batch with one task still running and two finished: the line leads with
    // the running task (its spinner), counts two of three done, and draws a
    // partial bar.
    let rows = vec![
        TaskRow {
            label: "削除完了 feat".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            label: "作成中… main".to_string(),
            mark: TaskMark::Running(0),
        },
        TaskRow {
            label: "作成失敗 dup".to_string(),
            mark: TaskMark::Done(false),
        },
    ];
    let line = task_status_line(&rows, 100);
    // Two rows: the label on the first, the bar and count on the second.
    assert_eq!(line.len(), 2);
    // The first still-running task is the representative label, on row 1.
    assert!(stripped(&[line[0].clone()]).contains("作成中… main"));
    let plain = stripped(&line);
    // Two of the three tasks have finished.
    assert!(plain.contains("2/3"));
    // A partial bar: started but not full.
    assert!(plain.contains('[') && plain.contains('>') && plain.contains(']'));
}

#[test]
fn task_status_line_settles_on_the_last_result_once_all_finish() {
    use super::super::tasks::{TaskMark, TaskRow};
    // With nothing running the line falls back to the last finished task and
    // fills the bar to 2/2.
    let rows = vec![
        TaskRow {
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            label: "削除完了 b".to_string(),
            mark: TaskMark::Done(true),
        },
    ];
    let plain = stripped(&task_status_line(&rows, 100));
    assert!(plain.contains("削除完了 b"));
    assert!(plain.contains("2/2"));
    // A full bar carries no `>` head: at width 100 the bar field is 17 wide.
    assert!(plain.contains(&format!("[{}]", "=".repeat(17))));
}

#[test]
fn task_status_line_leads_with_a_failure_when_the_last_task_failed() {
    use super::super::tasks::{TaskMark, TaskRow};
    // Nothing running and the most recent task failed: the line settles on that
    // failure (the `✗` mark) rather than an earlier success.
    let rows = vec![
        TaskRow {
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            label: "作成失敗 dup".to_string(),
            mark: TaskMark::Done(false),
        },
    ];
    let plain = stripped(&task_status_line(&rows, 100));
    assert!(plain.contains("作成失敗 dup"));
    assert!(plain.contains('✗'));
    assert!(plain.contains("2/2"));
}

#[test]
fn task_status_line_holds_one_fixed_width_however_the_label_changes() {
    use super::super::tasks::{TaskMark, TaskRow};
    // A short running label and a longer finished one must produce the same
    // block width, so the right-anchored block never shifts as a row's text
    // changes. Both rows of the block also share a single width.
    let short = task_status_line(
        &[TaskRow {
            label: "作成中… a".to_string(),
            mark: TaskMark::Running(0),
        }],
        100,
    );
    let long = task_status_line(
        &[TaskRow {
            label: "作成完了 a-very-long-session-name".to_string(),
            mark: TaskMark::Done(true),
        }],
        100,
    );
    let row_w = |line: &[String], row: usize| console::measure_text_width(&line[row]);
    assert_eq!(row_w(&short, 0), row_w(&long, 0));
    assert_eq!(row_w(&short, 1), row_w(&long, 1));
    // Both rows of a block are the same width, so it right-aligns as a column.
    assert_eq!(row_w(&long, 0), row_w(&long, 1));
}

#[test]
fn task_status_line_is_empty_without_rows() {
    use super::super::tasks::TaskRow;
    let none: Vec<TaskRow> = Vec::new();
    assert!(task_status_line(&none, 100).is_empty());
}

#[test]
fn render_frame_shows_the_task_status_over_the_update_notice() {
    use super::super::tasks::{TaskMark, TaskRow};
    let mut state = state_with(Vec::new());
    // Even with an update available, in-flight tasks take the corner.
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.set_tasks(vec![TaskRow {
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中… main"));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_shows_the_task_status_on_the_header_over_a_live_terminal() {
    use super::super::tasks::{TaskMark, TaskRow};
    // A live embedded terminal owns the right pane (没入's attached shell, or
    // 切替's live preview). The task status now rides the header's title-bar row,
    // outside the pane, so it shows without clobbering the shell output below.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_tasks(vec![TaskRow {
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let joined = stripped(&frame);
    // The status shows on the header row and the shell output stays intact.
    assert!(joined.contains("作成中… main"));
    assert!(joined.contains("$ echo hi"));
    // The status rides row 0 (the title bar), not the body where the shell is.
    assert!(stripped(&[frame[0].clone()]).contains("作成中… main"));
}

#[test]
fn render_frame_rides_the_update_notice_on_the_header_over_a_live_terminal() {
    // The update notice now anchors to the header rows (like the task status),
    // so it shows even over a live terminal without overdrawing the shell output.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let frame = render_frame(24, 100, &state);
    let joined = stripped(&frame);
    assert!(joined.contains("アップデートがあるぴょん"));
    // The shell output stays intact in the body.
    assert!(joined.contains("$ echo hi"));
    // The notice rides the header rows (rows 0-1), not the body where the shell is.
    let header = stripped(&frame[0..2]);
    assert!(header.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_keeps_the_loading_rabbit_over_a_live_terminal() {
    // The transient launch indicator is deliberate, so it still takes the corner
    // even while a live preview is on screen (it is painted during the blocking
    // terminal / agent spawn, before the new pane draws over the screen).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ ".to_string()], None));
    state.step_loading("ターミナル起動中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("ターミナル起動中…"));
}

#[test]
fn update_notice_is_skipped_when_the_terminal_is_too_narrow() {
    // The banner block is wider than this terminal, so it is dropped rather than
    // wrapping or clobbering the chrome.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 20, &state));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_shows_the_loading_rabbit_while_an_action_runs() {
    let mut state = state_with(Vec::new());
    state.step_loading("削除中… 1/2");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("削除中… 1/2"));
    // The hopping rabbit's face rides the corner.
    assert!(joined.contains("(･ㅅ･)"));
}

#[test]
fn loading_rabbit_takes_the_corner_over_the_update_notice() {
    // With both a pending update and a running action, the loading rabbit wins
    // the top-right corner so the in-flight work is what the user sees.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中…"));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn loading_rabbit_is_skipped_when_the_terminal_is_too_narrow() {
    // Like the update notice, the block is dropped rather than clobbering the
    // chrome when it cannot fit the width.
    let mut state = state_with(Vec::new());
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 10, &state));
    assert!(!joined.contains("作成中…"));
}

// --- Switch inline create ----------------------------------------------

#[test]
fn switch_create_rows_show_the_input_and_an_error() {
    // Caret at the end of the name: the whole name precedes it, and the block
    // caret adds one trailing cell.
    let rows = switch_create_rows("wip", 3, None, 30);
    assert_eq!(rows.len(), 1);
    let plain = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(plain.contains("+ new: wip"));

    // Caret in the middle: the block caret sits on a character, so the name reads
    // intact without an inserted glyph.
    let mid = switch_create_rows("wip", 2, None, 30);
    let plain_mid = console::strip_ansi_codes(&mid[0]).into_owned();
    assert!(plain_mid.contains("+ new: wip"));

    let with_error = switch_create_rows("feature", 7, Some("\"feature\" already exists."), 40);
    assert_eq!(with_error.len(), 2);
    assert!(console::strip_ansi_codes(&with_error[1]).contains("already exists"));
}

#[test]
fn render_frame_shows_the_inline_create_row_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::state::ReturnMode::Base);
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
fn switch_rename_rows_show_the_target_and_typed_label() {
    // Caret at the end of the label.
    let rows = switch_rename_rows("main", "My main", "My main".len(), 40);
    assert_eq!(rows.len(), 1);
    let plain = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(plain.contains("rename main: My main"));
}

#[test]
fn switch_rename_rows_place_the_caret_mid_label() {
    // With the caret in the middle of the label the block caret falls on the
    // character at the cursor rather than past the end, matching the create row.
    let rows = switch_rename_rows("main", "abc", 1, 40);
    let plain = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(plain.contains("rename main: abc"));
}

#[test]
fn render_frame_shows_the_inline_rename_row_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down(); // cursor onto "main"
    assert!(state.switch_begin_rename());
    for c in " 2".chars() {
        state.rename_mut().unwrap().push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // Prefilled with the branch name, then edited to "main 2".
    assert!(joined.contains("rename main: main 2"));
}

// --- command hints (command palette) -----------------------------------

fn typing(typed: &str) -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    // The hints belong to the `:` command palette; open it first.
    state.open_command_palette();
    for c in typed.chars() {
        state.push_char(c);
    }
    state
}

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
    assert!(joined.contains("more"));
    assert!(joined.contains("session"));
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

// --- removal modal -----------------------------------------------------

fn state_with_sessions(names: &[&str]) -> HomeState {
    use crate::domain::workspace_state::SessionRecord;
    let mut state = HomeState::new("usagi", Vec::new(), None);
    let sessions = names
        .iter()
        .map(|n| SessionRecord {
            name: n.to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from(format!("/ws/{n}")),
            worktrees: Vec::new(),
            created_at: Utc::now(),
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

#[test]
fn remove_modal_row_marks_the_cursor_and_checkbox() {
    let cursor =
        console::strip_ansi_codes(&remove_modal_row("alpha", true, false, 40)).into_owned();
    assert!(cursor.contains('>'));
    assert!(cursor.contains("[ ]"));
    assert!(cursor.contains("alpha"));
    let checked =
        console::strip_ansi_codes(&remove_modal_row("beta", false, true, 40)).into_owned();
    assert!(!checked.contains('>'));
    assert!(checked.contains("[x]"));
    let idle = console::strip_ansi_codes(&remove_modal_row("gamma", false, false, 40)).into_owned();
    assert!(idle.contains("[ ]"));
    assert!(idle.contains("gamma"));
}

#[test]
fn remove_modal_row_clips_a_long_name() {
    let row = remove_modal_row("a-very-long-session-name-indeed", false, false, 12);
    assert!(console::strip_ansi_codes(&row).contains('…'));
}

#[test]
fn render_frame_overlays_the_removal_modal_with_a_checklist() {
    let mut state = state_with_sessions(&["alpha", "beta"]);
    state.open_remove_modal(false);
    state.remove_modal_mut().unwrap().toggle();
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("Remove sessions"));
    assert!(joined.contains("Select sessions to remove"));
    assert!(joined.contains("alpha"));
    assert!(joined.contains("beta"));
    assert!(joined.contains("[x]"));
    assert!(joined.contains("1 selected"));
    assert!(joined.contains("Enter: remove"));
    // The mode chrome is not drawn underneath.
    assert!(!joined.contains("switch"));
}

#[test]
fn render_frame_overlays_the_quit_confirmation_modal() {
    let mut state = state_with_sessions(&["alpha", "beta"]);
    let live: std::collections::HashSet<std::path::PathBuf> =
        ["/ws/alpha", "/ws/beta"].iter().map(Into::into).collect();
    state.apply_badges(MonitorSnapshot {
        live,
        ..Default::default()
    });
    state.open_quit_confirm();
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("Quit usagi?"));
    assert!(joined.contains("2 session(s) still running"));
    assert!(joined.contains("Close anyway?"));
    assert!(joined.contains("y / Enter: close"));
    // Every bordered line of the modal must share the same width: a line
    // that overflows `INNER` would lose its right border and break this.
    let widths: Vec<usize> = joined
        .lines()
        .filter(|line| line.trim_start().starts_with('│'))
        .map(|line| console::measure_text_width(line.trim()))
        .collect();
    assert!(widths.iter().all(|&w| w == widths[0]));
}

#[test]
fn render_frame_removal_modal_reports_when_there_are_no_sessions() {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.open_remove_modal(false);
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("No sessions to remove"));
    assert!(!joined.contains("selected"));
}

#[test]
fn remove_modal_frame_scrolls_to_keep_the_cursor_visible() {
    let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut state = state_with_sessions(&refs);
    state.open_remove_modal(false);
    for _ in 0..9 {
        state.remove_modal_mut().unwrap().move_down();
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains('↑'));
    assert!(joined.contains('↓'));
    assert!(joined.contains("more"));
    assert!(joined.contains("s09"));
}

#[test]
fn remove_modal_frame_keeps_every_row_within_the_box() {
    let mut state = state_with_sessions(&["scroll", "session-new", "config"]);
    state.open_remove_modal(false);
    let frame = render_frame(24, 80, &state);
    let widths: Vec<usize> = frame
        .iter()
        .map(|l| console::strip_ansi_codes(l))
        .filter(|l| l.trim_start().starts_with(['┌', '│', '└']))
        .map(|l| console::measure_text_width(l.trim_end()))
        .collect();
    assert!(!widths.is_empty());
    assert!(widths.iter().all(|&w| w == widths[0]));
}

// --- render_frame composition ------------------------------------------

#[test]
fn render_frame_combines_all_sections_at_full_height() {
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let frame = render_frame(24, 80, &state);
    assert_eq!(frame.len(), 24);
    assert!(frame[0].contains("usagi"));
    // Row 1 is the mode ladder, row 2 the blank separator, so the two-pane body
    // (with its `│` divider) starts at row 3.
    assert!(frame[2].trim().is_empty());
    assert!(frame[3].contains('│'));
    // The default mode is 切替, so the footer carries its tag.
    assert!(frame.last().unwrap().contains("switch"));
    let joined = frame.join("\n");
    assert!(joined.contains("main"));
}

#[test]
fn command_palette_frame_shows_command_output_in_its_band() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_command_palette();
    // A command that logs a response (the echoed command + its output) shows in
    // the palette's response band.
    for c in "session list".chars() {
        state.push_char(c);
    }
    state.submit();
    // The palette stays open behind the response (no transitioning effect), so
    // the echoed command shows in its band.
    assert!(state.command_palette_open());
    let joined = stripped(&render_frame(24, 80, &state));
    assert!(joined.contains("session list"));
}

#[test]
fn render_frame_surfaces_running_and_waiting_agent_icons() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = HomeState::new("usagi", vec![running, waiting], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run"), PathBuf::from("/repo/wait")].into(),
        running: [PathBuf::from("/repo/run")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        ..Default::default()
    });
    // Height accommodates root (2 lines) + divider + 2 sessions (2 lines each)
    // without the lowest detail row slipping behind the bottom hint band — plus
    // the blank separator row below the mode ladder.
    let frame = render_frame(26, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains('▶'));
    assert!(joined.contains("running"));
    assert!(joined.contains('◆'));
    assert!(joined.contains("waiting"));
}

#[test]
fn render_frame_survives_a_short_terminal() {
    let state = state_with(Vec::new());
    let frame = render_frame(3, 80, &state);
    assert!(frame[0].contains("usagi"));
    assert!(frame.last().unwrap().contains("switch"));
    assert!(frame.len() >= 4);
}

#[test]
fn render_frame_focus_menu_keeps_its_height() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    let frame = render_frame(24, 80, &state);
    assert_eq!(frame.len(), 24);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // The right pane carries the action menu; no results band in Focus.
    assert!(joined.contains("terminal"));
    assert!(joined.contains("session: main"));
}

/// A `MarkdownLine`-bearing preview opened from `content`, titled `title`.
fn preview_state(title: &str, content: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_preview_result(Ok((title.to_string(), content.to_string())));
    state
}

#[test]
fn preview_pane_renders_every_markdown_block_and_inline_style() {
    // One sample exercising every block kind and inline span the renderer emits,
    // so each style arm of `preview_pane` is drawn.
    let content = "\
# Title Heading
A line with **bold**, *italic*, `code`, and [a link](http://example.com).
- bullet item
1. ordered item
> quoted line
```
fn fenced() {}
```";
    let state = preview_state("README.md", content);
    let pane = preview_pane(state.preview().unwrap(), 60, 14);
    let out = stripped(&pane);

    // The header carries the file's path.
    assert!(out.contains("README.md"));
    // Block kinds: heading, bullet (• marker), ordered (1.), quote bar, code.
    assert!(out.contains("Title Heading"));
    assert!(out.contains("• bullet item"));
    assert!(out.contains("1. ordered item"));
    assert!(out.contains("│ quoted line"));
    assert!(out.contains("fn fenced()"));
    // Inline spans keep their text (styling stripped): bold, italic, code, link.
    assert!(out.contains("bold"));
    assert!(out.contains("italic"));
    assert!(out.contains("a link"));
    // The pane fills the full requested height.
    assert_eq!(pane.len(), 14);
}

#[test]
fn preview_pane_scrolls_to_the_offset_and_clips_a_long_title() {
    let content = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut state = preview_state("a/very/long/path/that/exceeds/the/pane/readme.md", &content);
    for _ in 0..5 {
        state.preview_scroll_down(8);
    }
    let out = stripped(&preview_pane(state.preview().unwrap(), 40, 12));
    // Scrolled five lines down: the top lines are gone, line 5 is in view.
    assert!(out.contains("line 5"));
    assert!(!out.contains("line 0"));
    // The over-long title is truncated with an ellipsis to fit beside the hint.
    assert!(out.contains('…'));
}

#[test]
fn right_pane_shows_the_preview_over_every_mode() {
    // The preview captures the right pane regardless of mode; opened here from the
    // default 切替.
    let state = preview_state("notes.md", "# Notes\nhello");
    let out = stripped(&right_pane_contents(&state, 50, 12));
    assert!(out.contains("notes.md"));
    assert!(out.contains("Notes"));
    assert!(out.contains("hello"));
}

#[test]
fn preview_pane_shows_a_position_counter_once_the_content_overflows() {
    // More lines than the body can hold: the header gains a `start-end/total`
    // position so the reader knows there is more above / below.
    let content = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let state = preview_state("big.md", &content);
    let out = stripped(&preview_pane(state.preview().unwrap(), 60, 8));
    // 8 rows -> 7 body lines, so 1-7/50 with the file still at the top.
    assert!(out.contains("(1-7/50)"));
}

#[test]
fn preview_pane_in_a_one_row_pane_shows_only_the_header() {
    let state = preview_state("solo.md", "# Heading\nbody");
    let pane = preview_pane(state.preview().unwrap(), 40, 1);
    assert_eq!(pane.len(), 1);
    assert!(stripped(&pane).contains("solo.md"));
}

#[test]
fn preview_pane_colours_headings_by_level() {
    // h1–h3 take distinct colours and deeper levels fall back to plain bold; this
    // walks every arm of the heading styling.
    let state = preview_state("h.md", "# One\n## Two\n### Three\n#### Four");
    let out = stripped(&preview_pane(state.preview().unwrap(), 40, 8));
    for heading in ["One", "Two", "Three", "Four"] {
        assert!(out.contains(heading));
    }
}

#[test]
fn preview_pane_keeps_a_prefix_unstyled_for_a_non_list_non_quote_line() {
    // A line that carries a prefix but is neither a list item nor a quote keeps the
    // prefix verbatim (the defensive arm of the prefix styling).
    let preview = Preview {
        title: "x.md".to_string(),
        lines: vec![MarkdownLine {
            style: LineStyle::Text,
            prefix: ">> ".to_string(),
            spans: vec![Span {
                text: "hello".to_string(),
                style: SpanStyle::Plain,
            }],
        }],
        scroll: 0,
    };
    let out = stripped(&preview_pane(&preview, 40, 4));
    assert!(out.contains(">> hello"));
}

#[test]
fn preview_visible_tracks_the_body_height() {
    // The body height is mode-independent now (every base mode uses the single
    // input line), so the visible window matches the body rows less the header
    // and does not change with the mode.
    let switch = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    let switch_visible = preview_visible(24, 80, &switch);

    let mut focus = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    focus.enter_focus(1);
    let focus_visible = preview_visible(24, 80, &focus);

    // Both are positive and equal (same body height in either mode).
    assert!(switch_visible >= 1);
    assert_eq!(switch_visible, focus_visible);
    // A short terminal floors at one visible row.
    assert_eq!(preview_visible(4, 80, &switch), 1);
}

#[test]
fn render_frame_edits_the_note_in_the_right_pane_not_a_full_screen_modal() {
    // The note editor is edited *in place* in the right pane: the session name
    // header, the note body, and the footer hints all show — and crucially the
    // surrounding chrome (mode ladder) and the sidebar stay on screen, so the
    // screen never switches to a full-screen modal.
    let mut state = state_with(vec![worktree(Some("main"), false, BranchStatus::Local)]);
    let session = SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some("first line\nsecond".to_string()),
        root: PathBuf::from("/repo/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), false, BranchStatus::Local)],
        created_at: Utc::now(),
    };
    state.restore_sessions(vec![session]);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down(); // root -> alpha
    assert!(state.switch_begin_note());

    let frame = stripped(&render_frame(24, 80, &state));
    // The right-pane editor: a `note` box (the session is named in the sidebar) +
    // the multi-line note.
    assert!(frame.contains("─ note"), "the box is titled `note`");
    assert!(
        frame.contains("alpha"),
        "the session is still named on screen"
    );
    assert!(frame.contains("first line"));
    assert!(frame.contains("second"));
    // The footer carries the editor's keys.
    assert!(frame.contains("Ctrl-S: save"));
    // The chrome and sidebar are still drawn (not replaced by a modal): the mode
    // ladder and the root row's `workspace root` line are present.
    assert!(frame.contains("Switch"), "the mode ladder stays visible");
    assert!(
        frame.contains("workspace root"),
        "the sidebar stays visible"
    );
}

/// A 切替 state with one session named `alpha` carrying `note`, the cursor moved
/// onto it. `state_with` seeds an unrelated `main` worktree so the root row is
/// distinct.
fn switch_state_with_note(note: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), false, BranchStatus::Local)]);
    state.restore_sessions(vec![SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some(note.to_string()),
        root: PathBuf::from("/repo/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), false, BranchStatus::Local)],
        created_at: Utc::now(),
    }]);
    state.enter_switch(super::super::state::ReturnMode::Base);
    state.switch_move_down(); // root -> alpha
    state
}

#[test]
fn note_overlay_editor_windows_around_the_caret() {
    // While editing, the overlay box shows a window around the caret (the end,
    // here), so a note taller than the box keeps the caret line in view.
    let note = (0..10)
        .map(|i| format!("L{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut state = switch_state_with_note(&note);
    assert!(state.switch_begin_note()); // caret parks at the end (last line, "L9")

    // A short pane: only a window of the last lines fits in the editor box.
    let pane = stripped(&right_pane_contents(&state, 40, 8));
    assert!(pane.contains("─ note"));
    assert!(pane.contains("L9"), "the caret line is kept visible");
    assert!(!pane.contains("L0"), "the top lines are windowed out");
}

#[test]
fn note_editor_renders_a_multi_line_selection_without_corrupting_the_text() {
    // A selection spanning two lines reverses the cells in the editor box. The
    // highlight only recolours existing cells, so every line still reads intact —
    // and lines outside the span (before it and after it) render unchanged.
    let mut state = switch_state_with_note("one\ntwo\nthree\nfour");
    assert!(state.switch_begin_note()); // caret parks at the end ("four")
    let area = state.note_editor_mut().unwrap().area_mut();
    // Anchor at the end of "three", then extend the selection up to the start of
    // "two": the span is (line 1, col 0)..(line 2, col 5), caret on line 1.
    area.move_up();
    area.move_end();
    area.select_up();
    area.select_home();
    assert!(area.has_selection());

    let pane = stripped(&right_pane_contents(&state, 40, 20));
    // The box title and every line of the note survive the highlight.
    assert!(pane.contains("─ note"));
    for word in ["one", "two", "three", "four"] {
        assert!(pane.contains(word), "`{word}` still renders: {pane}");
    }
}

#[test]
fn right_pane_overlays_the_read_only_note_in_switch() {
    let mut state = switch_state_with_note("do X\ndo Y");
    // The selected session's note shows in the right pane (overlaid on top).
    let pane = stripped(&right_pane_contents(&state, 40, 12));
    assert!(pane.contains("─ note"), "the overlay is titled");
    assert!(pane.contains("do X"));
    assert!(pane.contains("do Y"));

    // Back on the root row there is no session note, so no overlay shows.
    state.switch_move_up();
    let root = stripped(&right_pane_contents(&state, 40, 12));
    assert!(!root.contains("do X"));
}

#[test]
fn read_only_note_overlay_hides_on_dismiss_and_returns_on_move() {
    let mut state = switch_state_with_note("do X\ndo Y");
    assert!(
        state.switch_note_visible(),
        "the note auto-shows on selection"
    );

    // Dismissing it (the first `Esc`) hides the overlay without leaving 切替.
    state.hide_switch_note();
    assert!(!state.switch_note_visible(), "the dismissed note is hidden");
    let hidden = stripped(&right_pane_contents(&state, 40, 12));
    assert!(
        !hidden.contains("do X"),
        "the dismissed note does not render"
    );

    // Moving the cursor (here back onto the same session via a wrap) re-shows it:
    // the dismissal belonged to the row just left.
    state.switch_move_up(); // -> root
    state.switch_move_down(); // -> alpha
    assert!(state.switch_note_visible(), "moving re-shows the note");
    let shown = stripped(&right_pane_contents(&state, 40, 12));
    assert!(
        shown.contains("do X"),
        "the note renders again after moving"
    );
}

#[test]
fn read_only_note_overlay_elides_a_long_note() {
    let note = (0..8)
        .map(|i| format!("todo {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let state = switch_state_with_note(&note);
    let pane = stripped(&right_pane_contents(&state, 40, 16));
    // The first lines show; the overflow is elided with a `… (N more)` line.
    assert!(pane.contains("todo 0"));
    assert!(pane.contains("todo 5"));
    assert!(pane.contains("more)"));
    assert!(!pane.contains("todo 7"));
}

#[test]
fn note_overlay_anchors_at_the_top_for_both_idle_and_live_previews() {
    // The note overlay sits at the top of the right pane regardless of whether the
    // preview underneath is an idle session's action menu or a live terminal, so
    // its box top border lands on the same row either way — moving the cursor never
    // shifts where the note reads (no CLS).
    let idle = switch_state_with_note("todo");
    let idle_rows = right_pane_contents(&idle, 40, 12);
    let idle_box_top = idle_rows
        .iter()
        .position(|l| console::strip_ansi_codes(l).contains('┌'))
        .expect("the read-only box has a top border");

    // Make the same session live (a running shell with a snapshot).
    let mut live = switch_state_with_note("todo");
    live.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wt")].into(),
        ..Default::default()
    });
    live.set_terminal_view(TerminalView::from_rows(vec!["$ live".to_string()], None));
    let live_rows = right_pane_contents(&live, 40, 12);
    let live_box_top = live_rows
        .iter()
        .position(|l| console::strip_ansi_codes(l).contains('┌'))
        .expect("the read-only box has a top border");

    assert_eq!(
        idle_box_top, live_box_top,
        "the box top lands on the same (top) row for both previews"
    );
    assert_eq!(
        idle_box_top, 0,
        "the box anchors at the very top of the pane"
    );
}

#[test]
fn note_overlay_keeps_the_session_header_visible_beside_the_box() {
    // The note box is a top-right column: it overwrites only its own columns, so
    // the session header on the top row keeps its leading columns to the box's
    // left — the session identity stays readable right beside the note.
    let mut live = switch_state_with_note("next: ship it");
    live.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wt")].into(),
        ..Default::default()
    });
    live.set_terminal_view(TerminalView::from_rows(vec!["$".to_string()], None));
    let rows = right_pane_contents(&live, 60, 12);
    // The box top border ends the top row; the session name shows on the row to
    // its left (before the box's `┌`).
    let top = console::strip_ansi_codes(&rows[0]);
    let left_of_box = top.split('┌').next().unwrap_or("");
    assert!(
        left_of_box.contains("alpha"),
        "the session header stays visible to the left of the box: {top:?}"
    );
    let pane = stripped(&rows);
    assert!(pane.contains("─ note"), "the note box shows");
    assert!(pane.contains("next: ship it"), "the note body shows");
}

#[test]
fn note_overlay_shows_fully_when_the_preview_is_sparse() {
    // When the base pane produces fewer lines than the note box is tall (a
    // session with little terminal / preview content), the box must still show
    // in full — it must not be clipped to the short base height (which made the
    // note vanish).
    let note = (0..4)
        .map(|i| format!("todo {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut state = switch_state_with_note(&note);
    // A live session whose terminal snapshot is a single line — a very short base.
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wt")].into(),
        ..Default::default()
    });
    state.set_terminal_view(TerminalView::from_rows(vec!["$".to_string()], None));
    let pane = stripped(&right_pane_contents(&state, 40, 16));
    assert!(pane.contains("─ note"), "the box title shows");
    // Every note line is visible, not just the top border.
    for i in 0..4 {
        assert!(pane.contains(&format!("todo {i}")), "todo {i} shows");
    }
}

#[test]
fn note_editor_overlay_keeps_the_preview_visible_behind_it() {
    // Editing is a floating box at the top, so the preview/terminal underneath
    // stays visible below it (the screen never switches).
    let mut state = switch_state_with_note("hi");
    assert!(state.switch_begin_note());
    let pane = stripped(&right_pane_contents(&state, 40, 16));
    assert!(pane.contains("─ note"), "the editor box shows");
    // The idle session's action-menu preview still shows below the box.
    assert!(
        pane.contains("Enter で開く"),
        "the preview behind the box is still visible"
    );
}
