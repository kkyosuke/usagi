use super::*;

#[test]
fn attached_tab_at_switches_to_a_clicked_inactive_tab() {
    let (state, geo) = attached_with_tabs(0);
    let col = chip_column(&state, geo, "2 terminal");
    let chips_row = geo.origin_row - super::TAB_BAR_ROWS as u16;
    // A click on the second chip (the inactive one) selects tab index 1.
    assert_eq!(attached_tab_at(&state, col, chips_row, geo), Some(1));
    // The underline marker row below the chips is part of the same target.
    assert_eq!(
        attached_tab_at(&state, col, geo.origin_row - 1, geo),
        Some(1)
    );
}

#[test]
fn attached_tab_at_ignores_a_click_on_the_active_tab() {
    let (state, geo) = attached_with_tabs(0);
    let col = chip_column(&state, geo, "1 agent");
    let chips_row = geo.origin_row - super::TAB_BAR_ROWS as u16;
    // Clicking the already-active tab is a no-op, so selection handling keeps it.
    assert_eq!(attached_tab_at(&state, col, chips_row, geo), None);
}

#[test]
fn attached_tab_at_ignores_clicks_off_the_chips() {
    let (state, geo) = attached_with_tabs(0);
    let chips_row = geo.origin_row - super::TAB_BAR_ROWS as u16;
    // The indent before the first chip (the pane's left edge) hits no tab.
    assert_eq!(
        attached_tab_at(&state, geo.origin_col, chips_row, geo),
        None
    );
    // A column left of the pane is outside the strip entirely.
    assert_eq!(attached_tab_at(&state, 0, chips_row, geo), None);
    // A row below the strip is the terminal body, not a tab.
    let col = chip_column(&state, geo, "2 terminal");
    assert_eq!(attached_tab_at(&state, col, geo.origin_row, geo), None);
}

#[test]
fn attached_tab_at_is_none_without_a_published_strip() {
    // Attached but no tabs published yet: there is nothing to click.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    let geo = attached_geometry(24, 80, Sidebar::Full);
    let chips_row = geo.origin_row - super::TAB_BAR_ROWS as u16;
    assert_eq!(
        attached_tab_at(&state, geo.origin_col, chips_row, geo),
        None
    );
}

#[test]
fn attached_tab_at_is_none_when_the_strip_has_no_room_above_the_body() {
    let (state, _) = attached_with_tabs(0);
    // A pathological geometry whose body starts above `TAB_BAR_ROWS`: there is no
    // strip row, so any click misses it instead of underflowing.
    let geo = TerminalGeometry {
        rows: 10,
        cols: 40,
        origin_col: 20,
        origin_row: 1,
    };
    assert_eq!(attached_tab_at(&state, 25, 0, geo), None);
}

#[test]
fn focus_tab_at_switches_from_new_tab_to_a_clicked_pane_tab() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    let geo = terminal_geometry(24, 120, Sidebar::Full);
    let col = chip_column(&state, geo, "2 terminal");
    assert_eq!(
        focus_tab_at(&state, col, geo.origin_row, 24, 120),
        Some(FocusTabClick::Pane(1))
    );
    // The underline row is part of the same tab target.
    assert_eq!(
        focus_tab_at(&state, col, geo.origin_row + 1, 24, 120),
        Some(FocusTabClick::Pane(1))
    );
}

#[test]
fn focus_tab_at_ignores_the_active_tab_and_clicks_off_the_strip() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    let geo = terminal_geometry(24, 120, Sidebar::Full);
    // While Focus opens on "+ new", clicking that already-active tab is a no-op.
    let new_col = chip_column(&state, geo, "3 + new");
    assert_eq!(focus_tab_at(&state, new_col, geo.origin_row, 24, 120), None);
    // Off the chip columns, and below the two tab rows, miss.
    assert_eq!(
        focus_tab_at(&state, geo.origin_col, geo.origin_row, 24, 120),
        None
    );
    assert_eq!(
        focus_tab_at(
            &state,
            new_col,
            geo.origin_row + TAB_BAR_ROWS as u16,
            24,
            120
        ),
        None
    );
}

#[test]
fn header_tab_rows_number_each_pane_beside_the_header_and_clip_to_width() {
    use super::super::super::terminal::tabs::TabStrip;
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
        scope: super::super::super::command::CommandScope::Session,
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
fn render_frame_rests_the_mascot_in_the_bottom_left_with_a_mode_face() {
    // With the full sidebar and a short list there is room at the bottom of the
    // sidebar for the resting mascot, whose face follows the current mode:
    // browsing in 切替 (the default), attentive in 在席, heads-down in 没入.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let switch = stripped(&render_frame(24, 80, &state));
    assert!(switch.contains("(o.o)?"), "browsing face in 切替: {switch}");

    state.enter_focus(1);
    let focus = stripped(&render_frame(24, 80, &state));
    assert!(focus.contains("(^.^)/"), "attentive face in 在席: {focus}");

    state.show_attached();
    let attached = stripped(&render_frame(24, 80, &state));
    assert!(
        attached.contains("(>.<)9"),
        "working face in 没入: {attached}"
    );
}

#[test]
fn render_frame_blinks_the_resting_mascot_after_an_interaction() {
    use std::time::Instant;
    // A kicked blink shuts the resting rabbit's eyes on the painted frame, so the
    // mascot reacts to the user (here in the default 切替 browsing face).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let now = Instant::now();
    state.kick_mascot_blink(now);
    state.tick_mascot(now);
    let blinked = stripped(&render_frame(24, 80, &state));
    assert!(blinked.contains("(-.-)?"), "the rabbit blinks: {blinked}");
}

#[test]
fn render_frame_rests_a_chibi_in_the_collapsed_rail() {
    // Folded to the rail there is no room for the full mascot, so a tiny two-row
    // chibi sits at the bottom of the strip instead — the usagi stays around.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.set_sidebar(Sidebar::Rail);
    let frame = stripped(&render_frame(24, 80, &state));
    assert!(frame.contains("(･･)"), "the rail chibi is drawn: {frame}");
}

#[test]
fn render_frame_keeps_a_blank_row_between_the_list_and_the_resting_mascot() {
    // A short list leaves the mascot resting at the bottom with at least one blank
    // sidebar row above its ears, so the art reads apart from the session list
    // rather than as the next entry.
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let lines = render_frame(24, 80, &state);
    // The ears row (the mascot's top row); its left sidebar cell carries the art.
    let ears = lines
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .position(|l| l.split(" │ ").next().unwrap_or("").contains("(\\(\\"))
        .expect("the resting mascot is drawn");
    let left_cell_above = console::strip_ansi_codes(&lines[ears - 1])
        .split(" │ ")
        .next()
        .unwrap_or("")
        .to_string();
    assert!(
        left_cell_above.trim().is_empty(),
        "a blank sidebar row separates the list from the mascot: {left_cell_above:?}"
    );
}

#[test]
fn render_frame_keeps_a_blank_row_below_the_resting_mascot() {
    // The mascot rests just above the bottom input line (the `● live terminal`
    // indicator in 没入), so a blank sidebar row sits below its feet to keep it
    // from reading as flush against that line.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.show_attached();
    let lines = render_frame(24, 80, &state);
    // The feet row (the mascot's bottom row); its left sidebar cell carries the art.
    let feet = lines
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .position(|l| l.split(" │ ").next().unwrap_or("").contains("o(_("))
        .expect("the resting mascot is drawn");
    let left_cell_below = console::strip_ansi_codes(&lines[feet + 1])
        .split(" │ ")
        .next()
        .unwrap_or("")
        .to_string();
    assert!(
        left_cell_below.trim().is_empty(),
        "a blank sidebar row separates the mascot from the input line: {left_cell_below:?}"
    );
}

#[test]
fn render_frame_aligns_the_mascot_left_edge_with_the_live_terminal_indicator() {
    // The mascot is indented one column so its left edge lines up with the bottom
    // input line's content — the `● live terminal` indicator carries a single
    // leading space, and the mascot's feet (`o`) should start in that same column.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.show_attached();
    let lines: Vec<String> = render_frame(24, 80, &state)
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();
    let feet = lines
        .iter()
        .position(|l| l.split(" │ ").next().unwrap_or("").contains("o(_("))
        .expect("the resting mascot is drawn");
    let indicator = lines
        .iter()
        .position(|l| l.contains("● live terminal"))
        .expect("the live terminal indicator is drawn");
    let mascot_col = lines[feet].find("o(_(").unwrap();
    let indicator_col = lines[indicator].find('●').unwrap();
    assert_eq!(
        mascot_col, indicator_col,
        "the mascot's left edge lines up with the live terminal indicator: {:?} vs {:?}",
        lines[feet], lines[indicator]
    );
}

#[test]
fn render_frame_hides_the_mascot_when_the_session_list_fills_the_sidebar() {
    // A list long enough to reach the bottom rows takes precedence: the mascot
    // hides rather than overlapping the sessions.
    let many: Vec<WorktreeState> = (0..9)
        .map(|i| worktree(Some(&format!("feat-{i}")), false, BranchStatus::Local))
        .collect();
    let state = state_with(many);
    let frame = stripped(&render_frame(24, 80, &state));
    assert!(
        !frame.contains("(o.o)?"),
        "mascot must yield to a full list: {frame}"
    );
}

#[test]
fn render_frame_omits_the_mascot_when_the_sidebar_or_body_is_too_small() {
    // A terminal too narrow leaves the full sidebar narrower than the art, and one
    // too short leaves no body rows to spare — either way the mascot is skipped
    // rather than overrunning the layout (and the frame still renders).
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let narrow = stripped(&render_frame(24, 11, &state));
    assert!(
        !narrow.contains("(o.o)?"),
        "no mascot in a narrow sidebar: {narrow}"
    );
    let short = stripped(&render_frame(7, 80, &state));
    assert!(
        !short.contains("(o.o)?"),
        "no mascot with no body rows to spare: {short}"
    );
}

#[test]
fn render_frame_omits_the_mascot_on_the_collapsed_rail() {
    // The rail is too narrow to hold the art, so the mascot does not render there.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.set_sidebar(Sidebar::Rail);
    let frame = stripped(&render_frame(24, 80, &state));
    assert!(!frame.contains("(o.o)?"), "no mascot on the rail: {frame}");
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
