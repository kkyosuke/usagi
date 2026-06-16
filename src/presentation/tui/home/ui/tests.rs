use super::chrome::*;
use super::panes::*;
use super::*;

use super::super::command::{CommandHint, CommandInfo};
use super::super::state::{LogLine, TextModal, WorktreeList, ROOT_NAME};
use super::super::terminal_view::TerminalView;
use crate::domain::settings::SessionActionUi;
use crate::domain::workspace_state::{BranchStatus, WorktreeState};
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
fn pad_to_width_fills_short_content() {
    assert_eq!(pad_to_width("ab".to_string(), 5), "ab   ");
}

#[test]
fn pad_to_width_leaves_full_content_alone() {
    assert_eq!(pad_to_width("abcde".to_string(), 5), "abcde");
}

#[test]
fn layout_splits_a_standard_width() {
    let (left, right) = layout(80);
    assert_eq!(left, 26);
    assert_eq!(right, 80 - 26 - SEP_WIDTH);
}

#[test]
fn layout_does_not_overrun_a_narrow_terminal() {
    let (left, right) = layout(4);
    assert!(left <= 4);
    assert_eq!(right, 0);
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
        10,
        10,
        true,
        false,
        true,
        false,
        false,
    );
    assert!(top.contains('>'));
    assert!(top.contains('●'));
    assert!(top.contains("main"));

    // The same selected row outside Switch shows no cursor.
    let (top_no_switch, _) = worktree_row(
        &worktree(Some("main"), true, BranchStatus::Pushed),
        10,
        10,
        true,
        false,
        false,
        false,
        false,
    );
    assert!(!top_no_switch.contains('>'));

    let (other_top, _) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        10,
        10,
        false,
        false,
        true,
        false,
        false,
    );
    assert!(!other_top.contains('>'));
    assert!(other_top.contains('○'));
    assert!(other_top.contains("feature"));

    let (detached_top, _) = worktree_row(
        &worktree(None, false, BranchStatus::Local),
        10,
        10,
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
        10,
        10,
        false,
        true,
        false,
        true,
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
        10,
        10,
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
fn worktree_row_shows_a_running_agent_and_one_waiting_for_input() {
    let (_, running_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        10,
        12,
        false,
        false,
        false,
        true,
        false,
    );
    assert!(running_detail.contains('▶'));
    assert!(running_detail.contains("running"));

    let (_, waiting_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        10,
        12,
        false,
        false,
        false,
        true,
        true,
    );
    assert!(waiting_detail.contains('◆'));
    assert!(!waiting_detail.contains('▶'));
    assert!(waiting_detail.contains("waiting"));

    let (idle_top, idle_detail) = worktree_row(
        &worktree(Some("feature"), false, BranchStatus::Local),
        10,
        12,
        false,
        false,
        false,
        false,
        false,
    );
    assert!(!idle_detail.contains('▶'));
    assert!(!idle_detail.contains('◆'));
    assert!(idle_top.contains("local"));
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
        8,
        8,
        false,
        false,
        false,
        false,
        false,
    );
    assert!(top.contains('…'));
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
        80,
        6,
        false,
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
    let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
    assert_eq!(lines.len(), 6);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains("main"));
    assert!(lines[4].contains("feature"));
}

#[test]
fn left_pane_marks_a_running_agent_and_one_waiting_for_input() {
    let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
    let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
    let running = left_pane(&list, &path, &HashSet::new(), 30, 6, false);
    assert!(running[3].contains('▶'));
    assert!(running[3].contains("running"));
    let waiting = left_pane(&list, &path, &path, 30, 6, false);
    assert!(waiting[3].contains('◆'));
    assert!(!waiting[3].contains('▶'));
    let idle = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
    assert!(!idle[3].contains('▶'));
    assert!(!idle[3].contains('◆'));
    assert!(idle[2].contains("local"));
}

#[test]
fn left_pane_is_trimmed_to_available_rows() {
    let list = list_with(vec![
        worktree(Some("a"), false, BranchStatus::Local),
        worktree(Some("b"), false, BranchStatus::Local),
        worktree(Some("c"), false, BranchStatus::Local),
    ]);
    let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 3, false);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(ROOT_NAME));
    assert!(lines[2].contains('a'));
}

#[test]
fn left_pane_marks_the_active_worktree_with_a_gutter_bar() {
    let mut list = list_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        worktree(Some("feature"), false, BranchStatus::Local),
    ]);
    list.activate_by_name("feature");
    let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
    // The root is not active; the active "feature" row carries the green `▎`
    // accent bar down both of its lines (identity + detail).
    assert!(!lines[0].contains('▎'));
    assert!(lines[4].contains("feature"));
    assert!(lines[4].contains('▎'));
    assert!(lines[5].contains('▎'));
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
    let dimmed = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, true);
    assert_eq!(dimmed.len(), 6);
    assert!(console::strip_ansi_codes(&dimmed[0]).contains(ROOT_NAME));
    assert!(console::strip_ansi_codes(&dimmed[2]).contains("main"));
    assert!(console::strip_ansi_codes(&dimmed[4]).contains("feature"));
}

#[test]
fn log_line_colours_each_kind_and_prompts_commands() {
    assert!(log_line(&LogLine::command("man"), 40).contains("❯ man"));
    assert_eq!(log_line(&LogLine::output("plain"), 40), "plain");
    assert!(log_line(&LogLine::error("boom"), 40).contains("boom"));
    assert!(log_line(&LogLine::notice("note"), 40).contains("note"));
}

#[test]
fn log_tail_shows_only_the_tail_that_fits() {
    let log: Vec<LogLine> = (0..5)
        .map(|i| LogLine::output(format!("line {i}")))
        .collect();
    let lines = log_tail(&log, 40, 3);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("line 2"));
    assert!(lines[2].contains("line 4"));
}

#[test]
fn log_tail_keeps_everything_when_it_fits() {
    let log = vec![LogLine::output("only")];
    assert_eq!(log_tail(&log, 40, 5).len(), 1);
}

// --- right pane by mode ------------------------------------------------

#[test]
fn right_pane_is_blank_in_overview_but_previews_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert!(right_pane_contents(&state, 40, 5).is_empty());
    // In 切替 the right pane previews the would-be screen for the cursor row.
    state.enter_switch(super::super::state::ReturnMode::Overview);
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
    state.set_live([PathBuf::from("/repo/run")].into());
    state.enter_switch(super::super::state::ReturnMode::Overview);
    // Move the cursor off the root onto the session row.
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("feat"));
    // Header carries the git status and the running agent state.
    assert!(preview.contains("local"));
    assert!(preview.contains("running"));
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
    state.set_live([PathBuf::from("/repo/run")].into());
    // The event loop snapshots the highlighted live session before painting.
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ echo hi".to_string(), "hi".to_string()],
        None,
    ));
    state.enter_switch(super::super::state::ReturnMode::Overview);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    // The real terminal screen is shown, not the placeholder label.
    assert!(preview.contains("$ echo hi"));
    assert!(preview.contains("hi"));
    assert!(!preview.contains("live terminal"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_an_idle_session_as_its_action_menu() {
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::state::ReturnMode::Overview);
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
        state.focus_prompt_push_char(c);
    }
    let prompt = stripped(&right_pane_contents(&state, 40, 12));
    assert!(prompt.contains("session: main"));
    assert!(prompt.contains("❯ ter"));
    // The session-scope hint lists terminal as a match.
    assert!(prompt.contains("terminal"));
}

#[test]
fn focus_prompt_shows_usage_for_arguments() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "terminal ".chars() {
        state.focus_prompt_push_char(c);
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
        state.focus_prompt_push_char(c);
    }
    // The header, blank, and prompt lines are present, but no hint rows follow.
    let rows = right_pane_contents(&state, 60, 12);
    assert!(stripped(&rows).contains("❯ zzz"));
    // The prompt body has exactly the header, a blank, the prompt, and a blank
    // separator — no hint rows after it.
    assert_eq!(rows.len(), 4);
}

#[test]
fn right_pane_shows_the_terminal_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    // No snapshot yet: a starting hint.
    let starting = right_pane_contents(&state, 40, 5);
    assert!(starting[0].contains("Starting terminal"));
    // Once a snapshot arrives, its rows are shown.
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let running = right_pane_contents(&state, 40, 5);
    assert!(running[0].contains("$ echo hi"));
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
    let geo = terminal_geometry(24, 80);
    let (left, _) = layout(80);
    assert_eq!(geo.origin_col as usize, left + SEP_WIDTH);
    assert_eq!(geo.origin_row, 2);
    assert_eq!(geo.rows, 20);
    assert_eq!(geo.cols as usize, 80 - left - SEP_WIDTH);
}

#[test]
fn terminal_geometry_stays_positive_in_a_tiny_terminal() {
    let geo = terminal_geometry(1, 1);
    assert!(geo.rows >= 1);
    assert!(geo.cols >= 1);
}

#[test]
fn cursor_screen_pos_places_the_cursor_one_past_the_origin() {
    let geo = terminal_geometry(24, 80);
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
    let geo = terminal_geometry(24, 80);
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

// --- input / footer by mode --------------------------------------------

#[test]
fn input_line_renders_prompt_in_overview() {
    let mut state = state_with(Vec::new());
    state.push_char('m');
    let line = input_line(&state);
    assert!(line.contains('m'));
    assert!(line.contains(CARET));
}

#[test]
fn input_line_differs_by_mode() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::state::ReturnMode::Overview);
    assert!(input_line(&state).contains("Pick a session"));
    state.enter_focus(1);
    assert!(input_line(&state).contains("Operating session: main"));
    state.show_attached();
    assert!(input_line(&state).contains("live terminal"));
}

#[test]
fn footer_line_differs_by_mode() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert!(footer_line(80, &state).contains("overview"));
    state.enter_switch(super::super::state::ReturnMode::Overview);
    assert!(footer_line(80, &state).contains("switch"));
    state.enter_focus(1);
    assert!(footer_line(80, &state).contains("session: main"));
    state.show_attached();
    assert!(footer_line(80, &state).contains("attached"));
}

#[test]
fn mode_ladder_lists_every_step_and_keeps_them_for_each_mode() {
    for mode in [Mode::Overview, Mode::Switch, Mode::Focus, Mode::Attached] {
        let ladder = console::strip_ansi_codes(&mode_ladder(80, mode)).into_owned();
        for step in ["Overview", "Switch", "Focus", "Attached"] {
            assert!(ladder.contains(step), "{mode:?} ladder missing {step}");
        }
    }
}

#[test]
fn overview_input_is_a_bordered_box_at_full_height() {
    let mut state = state_with(Vec::new());
    for c in "session".chars() {
        state.push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // The input is framed (top/bottom borders) and still carries the prompt.
    assert!(joined.contains('┌'));
    assert!(joined.contains('└'));
    assert!(joined.contains("❯ session"));
}

#[test]
fn overview_input_falls_back_to_a_single_line_on_a_short_terminal() {
    let state = state_with(Vec::new());
    // Too short for the 3-row box: the input is the plain prompt line.
    let lines = render_frame(6, 80, &state);
    let joined = console::strip_ansi_codes(&lines.join("\n")).into_owned();
    assert!(!joined.contains('┌'));
    assert!(joined.contains('❯'));
}

// --- update-available notice -------------------------------------------

#[test]
fn update_banner_pairs_the_mascot_with_the_latest_version() {
    let latest = crate::domain::version::Version::parse("0.2.0").unwrap();
    let banner = update_banner(&latest);
    assert_eq!(banner.len(), 3);
    let plain = stripped(&banner);
    assert!(plain.contains("最新版があります"));
    assert!(plain.contains("v0.2.0"));
    // The usagi mascot rides alongside the notice.
    assert!(plain.contains("(='-')"));
}

#[test]
fn render_frame_shows_the_update_notice_when_a_newer_release_exists() {
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("最新版があります"));
    assert!(joined.contains("v9.9.9"));
}

#[test]
fn render_frame_hides_the_update_notice_by_default() {
    let state = state_with(Vec::new());
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(!joined.contains("最新版があります"));
}

#[test]
fn update_notice_is_skipped_when_the_terminal_is_too_narrow() {
    // The banner block is wider than this terminal, so it is dropped rather than
    // wrapping or clobbering the chrome.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 20, &state));
    assert!(!joined.contains("最新版があります"));
}

#[test]
fn overlay_top_right_skips_a_row_whose_content_reaches_the_banner_column() {
    // The first line already fills the width, so the banner cannot be placed on
    // it; a later, empty line still receives its segment.
    let mut lines = vec!["X".repeat(100), String::new()];
    let banner = vec!["AB".to_string(), "CD".to_string()];
    overlay_top_right(&mut lines, 0, 100, &banner);
    // Row 0 is untouched (no room); row 1 gets its right-anchored segment.
    assert_eq!(console::measure_text_width(&lines[0]), 100);
    assert!(lines[1].ends_with("CD"));
}

#[test]
fn overlay_top_right_stops_when_the_banner_runs_past_the_last_row() {
    // The banner has more rows than remain from `top`, so placement stops at the
    // end of `lines` instead of panicking.
    let mut lines = vec![String::new()];
    let banner = vec!["AB".to_string(), "CD".to_string(), "EF".to_string()];
    overlay_top_right(&mut lines, 0, 100, &banner);
    assert!(lines[0].ends_with("AB"));
    assert_eq!(lines.len(), 1);
}

// --- Switch inline create ----------------------------------------------

#[test]
fn switch_create_rows_show_the_input_and_an_error() {
    let rows = switch_create_rows("wip", None, 30);
    assert_eq!(rows.len(), 1);
    let plain = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(plain.contains("+ new: wip"));
    assert!(plain.contains(CARET));

    let with_error = switch_create_rows("feature", Some("\"feature\" already exists."), 40);
    assert_eq!(with_error.len(), 2);
    assert!(console::strip_ansi_codes(&with_error[1]).contains("already exists"));
}

#[test]
fn render_frame_shows_the_inline_create_row_in_switch() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_switch(super::super::state::ReturnMode::Overview);
    state.switch_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_push_char(c);
    }
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("+ new: wip"));
    assert!(joined.contains("switch"));
}

// --- command hints (Overview) ------------------------------------------

fn typing(typed: &str) -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
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
fn hint_lines_are_empty_outside_overview() {
    let mut state = HomeState::new(
        "usagi",
        vec![worktree(Some("m"), true, BranchStatus::Local)],
        None,
    );
    state.enter_focus(1);
    assert!(hint_lines(&state, 80).is_empty());
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
fn render_frame_shows_command_hints_above_the_input_and_keeps_its_height() {
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
    state.remove_modal_toggle();
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
    assert!(!joined.contains("overview"));
}

#[test]
fn render_frame_overlays_the_quit_confirmation_modal() {
    let mut state = state_with_sessions(&["alpha", "beta"]);
    let live: std::collections::HashSet<std::path::PathBuf> =
        ["/ws/alpha", "/ws/beta"].iter().map(Into::into).collect();
    state.set_live(live);
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
        state.remove_modal_move_down();
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
    assert!(frame[2].contains('│'));
    assert!(frame.last().unwrap().contains("overview"));
    let joined = frame.join("\n");
    assert!(joined.contains("main"));
    // The Overview results band carries the seeded log hint below the input.
    assert!(joined.contains("man"));
}

#[test]
fn render_frame_results_band_shows_command_output_below_the_input() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    for c in "session list".chars() {
        state.push_char(c);
    }
    state.submit();
    let frame = render_frame(24, 80, &state);
    let input_row = frame.iter().position(|l| l.contains('❯')).unwrap();
    let joined_below = console::strip_ansi_codes(&frame[input_row + 1..].join("\n")).into_owned();
    // The echoed command shows in the results band, below the input.
    assert!(joined_below.contains("session list"));
}

#[test]
fn render_frame_surfaces_running_and_waiting_agent_icons() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = HomeState::new("usagi", vec![running, waiting], None);
    state.set_live([PathBuf::from("/repo/run"), PathBuf::from("/repo/wait")].into());
    state.set_waiting([PathBuf::from("/repo/wait")].into());
    let frame = render_frame(24, 80, &state);
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
    assert!(frame.last().unwrap().contains("overview"));
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
