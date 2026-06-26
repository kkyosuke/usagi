use super::*;

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
                color: None,
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
    state.enter_switch(super::super::super::state::ReturnMode::Base);
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
