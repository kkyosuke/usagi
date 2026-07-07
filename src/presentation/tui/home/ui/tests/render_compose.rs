use super::*;

#[test]
fn render_frame_combines_all_sections_at_full_height() {
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    let frame = render_frame(24, 80, &state);
    assert_eq!(frame.len(), 24);
    assert!(frame[0].contains("usagi"));
    assert!(frame[0].contains("Switch"));
    assert!(frame[0].contains("Closeup"));
    // Row 1 is the blank separator, so the two-pane body (with its `│` divider)
    // starts at row 2.
    assert!(frame[1].trim().is_empty());
    assert!(frame[2].contains('│'));
    // The default mode is 選択, so the footer carries its tag.
    assert!(frame.last().unwrap().contains("switch"));
    let joined = frame.join("\n");
    assert!(joined.contains("main"));
}

#[test]
fn render_frame_overlays_tab_rename_modal() {
    let mut state = state_with(vec![worktree(Some("main"), false, BranchStatus::Local)]);
    state.open_tab_menu(PathBuf::from("/repo/wt"), 0, "agent", 10, 3);
    assert!(state.begin_tab_rename_from_menu().is_some());
    let frame = render_frame(24, 100, &state);
    let text = stripped(&frame);
    assert!(text.contains("Rename tab"));
    assert!(text.contains("label:"));
    assert!(text.contains("agent"));
}

#[test]
fn mascot_hit_rect_covers_where_the_rabbit_is_drawn() {
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    let frame = render_frame(24, 80, &state);
    let rect = mascot_hit_rect(24, 80, &state).expect("the mascot is shown at full size");
    // Locate the rabbit's feet row in the rendered frame and assert it (and the
    // body two rows up) falls inside the click rectangle — so the hit-test lands
    // exactly where the rabbit was painted, not on hand-computed coordinates.
    // The idle right pane rests a mascot too (sharing the feet art), so scan from
    // the bottom for the sidebar mascot — the lower of the two.
    let (feet_row, line) = frame
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| console::strip_ansi_codes(l).contains("o(_(\")(\")"))
        .expect("the rabbit's feet are drawn");
    let feet_col = console::strip_ansi_codes(line)
        .find('o')
        .expect("the feet lead with `o`");
    assert!(rect.contains(feet_col as u16, feet_row as u16));
    assert!(rect.contains(feet_col as u16, (feet_row - 2) as u16));
    // Cells well outside the block are not on the rabbit.
    assert!(!rect.contains(0, 0));
    assert!(!rect.contains(60, feet_row as u16));
}

#[test]
fn mascot_hit_rect_is_none_when_there_is_no_room() {
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    // A very short screen leaves no room for the mascot below the list, so there is
    // nothing to click.
    assert!(mascot_hit_rect(8, 80, &state).is_none());
}

#[test]
fn mascot_hit_rect_tracks_the_collapsed_rail_chibi() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.toggle_sidebar(); // collapse to the rail
    let frame = render_frame(24, 80, &state);
    let rect = mascot_hit_rect(24, 80, &state).expect("the rail chibi is shown");
    // The two-row chibi's face is inside the (narrower) rail click target.
    let (face_row, line) = frame
        .iter()
        .enumerate()
        .find(|(_, l)| console::strip_ansi_codes(l).contains("(･･)"))
        .expect("the chibi face is drawn");
    let face_col = console::strip_ansi_codes(line)
        .find('(')
        .expect("the chibi has an opening paren");
    assert!(rect.contains(face_col as u16, face_row as u16));
}

#[test]
fn mascot_hit_rect_targets_the_rabbit_below_the_update_bubble() {
    // When the mascot speaks the update notice the block is taller (bubble + the
    // rabbit), but only the rabbit's body — the bottom three rows — is clickable,
    // so a click on the bubble does not count.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let frame = render_frame(24, 100, &state);
    let rect = mascot_hit_rect(24, 100, &state).expect("the speaking mascot is shown");
    // The idle right pane rests a mascot too (sharing the feet art), so scan from
    // the bottom for the sidebar mascot — the lower of the two.
    let (feet_row, _) = frame
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| console::strip_ansi_codes(l).contains("o(_(\")(\")"))
        .expect("the rabbit's feet are drawn");
    // The feet are on the rabbit (clickable); the bubble well above it is not.
    assert!(rect.contains(1, feet_row as u16));
    assert!(!rect.contains(1, (feet_row - 4) as u16));
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
    // the blank separator row below the header.
    let frame = render_frame(26, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // The sidebar shows the agent state as icons only (no spelled-out word).
    assert!(joined.contains('▶'));
    assert!(joined.contains('◆'));
}

#[test]
fn workspace_total_label_shows_cpu_and_memory_figures_or_is_absent_when_idle() {
    // Nothing live → no label, so the resting mascot carries no number.
    assert_eq!(workspace_total_label(ResourceUsage::default()), None);
    let label = workspace_total_label(ResourceUsage {
        cpu_percent: 23,
        memory_bytes: 512 * 1024 * 1024,
    })
    .unwrap();
    let plain = console::strip_ansi_codes(&label);
    // The figures show, led by the CPU / memory icons (the words `CPU` / `MEM` are
    // gone). The CPU figure is left-padded to a fixed width, so MEM lands in the
    // same column whatever the percentage's digit count.
    assert!(plain.contains("23%"), "{plain:?} missing the CPU figure");
    assert!(
        plain.contains("512MB"),
        "{plain:?} missing the memory figure"
    );
    assert!(!plain.contains("CPU"));
    assert!(!plain.contains("MEM"));
}

#[test]
fn append_total_beside_mascot_writes_on_the_feet_row_when_it_fits() {
    // Three mascot rows (ears / face / feet); the total joins the bottom feet row
    // so the label rests on the rabbit's foot line.
    let rabbit_rows = || {
        vec![
            " (\\(\\".to_string(),
            " (^.^)/".to_string(),
            "o(_(\")(\")".to_string(),
        ]
    };
    let total = ResourceUsage {
        cpu_percent: 23,
        memory_bytes: 512 * 1024 * 1024,
    };
    let mut rabbit = rabbit_rows();
    append_total_beside_mascot(&mut rabbit, total, 40);
    let feet = console::strip_ansi_codes(&rabbit[2]);
    assert!(feet.contains("23%"), "{feet:?} missing the CPU figure");
    assert!(feet.contains("512MB"), "{feet:?} missing the memory figure");
    // Only the feet row gains it; the ears and face are untouched.
    assert!(!rabbit[0].contains("512MB"));
    assert!(!rabbit[1].contains("512MB"));

    // Too narrow for the art plus the label → the row is left alone (never
    // overrunning the sidebar and pushing the right pane out of line).
    let mut narrow = rabbit_rows();
    append_total_beside_mascot(&mut narrow, total, 8);
    assert!(!narrow[2].contains("512MB"));

    // Idle total → nothing is appended at all.
    let mut idle = rabbit_rows();
    append_total_beside_mascot(&mut idle, ResourceUsage::default(), 40);
    assert!(!idle[2].contains("512MB"));

    // A two-row chibi is too short to be the full mascot → it is left untouched.
    let mut chibi = vec![" ∩∩".to_string(), "(･･)".to_string()];
    append_total_beside_mascot(&mut chibi, total, 40);
    assert!(!chibi.iter().any(|r| r.contains("512MB")));
}

#[test]
fn render_frame_rests_the_workspace_total_beside_the_mascot() {
    let mut wt = worktree(Some("feat"), true, BranchStatus::Local);
    wt.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![wt], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        running: [PathBuf::from("/repo/run")].into(),
        resource_total: ResourceUsage {
            cpu_percent: 23,
            memory_bytes: 512 * 1024 * 1024,
        },
        ..Default::default()
    });
    // A wide terminal gives the sidebar room for the art plus the icon-led total
    // beside it (a narrow one omits it — see the fit guard's own test).
    let joined = console::strip_ansi_codes(&render_frame(24, 120, &state).join("\n")).into_owned();
    assert!(joined.contains("23%"));
    assert!(joined.contains("512MB"));
}

#[test]
fn render_frame_floats_the_pr_popup_only_while_it_is_pinned() {
    let mut state = state_with(vec![worktree_with_pr(412)]);
    // With no popup pinned the row shows only the folded `<icon> <count>` badge —
    // the expanded `#412` lives in the popup, which is not drawn yet.
    let resting = stripped(&render_frame(24, 120, &state));
    assert!(!resting.contains("#412"));
    // Pinning the session's popup floats its `#<number>` list in a titled box.
    assert!(state.set_pr_popup(Some(0)));
    let pinned = stripped(&render_frame(24, 120, &state));
    assert!(pinned.contains("#412"));
    assert!(pinned.contains("PR"));
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
fn render_frame_clips_body_rows_to_a_narrow_terminal() {
    // A long session name on a narrow terminal must not push the `│` divider (or
    // the whole right pane) sideways: every composed row stays within the
    // terminal width. The fixed-column design forbids this layout shift.
    let state = state_with(vec![worktree(
        Some("a-very-long-feature-branch-name-that-overflows"),
        true,
        BranchStatus::Local,
    )]);
    let width = 24;
    let frame = render_frame(20, width, &state);
    for line in &frame {
        assert!(
            console::measure_text_width(line) <= width,
            "row overflows {width} cols: {line:?}",
        );
    }
}

#[test]
fn render_frame_closeup_menu_keeps_its_height() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_closeup(1);
    let frame = render_frame(24, 80, &state);
    assert_eq!(frame.len(), 24);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    // The right pane carries the action menu; no results band in Closeup.
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
    // default 選択.
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
    let overview_visible = preview_visible(24, 80, &switch);

    let mut focus = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    focus.enter_closeup(1);
    let closeup_visible = preview_visible(24, 80, &focus);

    // Both are positive and equal (same body height in either mode).
    assert!(overview_visible >= 1);
    assert_eq!(overview_visible, closeup_visible);
    // A short terminal floors at one visible row.
    assert_eq!(preview_visible(4, 80, &switch), 1);
}

#[test]
fn render_frame_edits_the_note_in_the_right_pane_not_a_full_screen_modal() {
    // The note editor is edited *in place* in the right pane: the session name
    // header, the note body, and the footer hints all show — and crucially the
    // surrounding chrome (header) and the sidebar stay on screen, so the
    // screen never switches to a full-screen modal.
    let mut state = state_with(vec![worktree(Some("main"), false, BranchStatus::Local)]);
    let session = SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some("first line\nsecond".to_string()),
        label_id: None,
        agent: Default::default(),
        origin: Default::default(),
        started_from: None,
        root: PathBuf::from("/repo/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), false, BranchStatus::Local)],
        created_at: Utc::now(),
        last_active: None,
    };
    state.restore_sessions(vec![session]);
    state.enter_switch();
    state.overview_move_down(); // root -> alpha
    assert!(state.overview_begin_note());

    let frame = stripped(&render_frame(24, 80, &state));
    // The right-pane editor: a `note` box (the session is named in the sidebar) +
    // the multi-line note.
    assert!(frame.contains("─ note"), "the box is titled `note`");
    assert!(
        frame.contains("編集中"),
        "the open editor is marked as editing, distinct from the read-only note"
    );
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
    assert!(frame.contains("Switch"), "the mode header stays visible");
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
    let mut state = overview_state_with_note(&note);
    assert!(state.overview_begin_note()); // caret parks at the end (last line, "L9")

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
    let mut state = overview_state_with_note("one\ntwo\nthree\nfour");
    assert!(state.overview_begin_note()); // caret parks at the end ("four")
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
fn right_pane_overlays_the_read_only_note_in_overview() {
    let mut state = overview_state_with_note("do X\ndo Y");
    // The selected session's note shows in the right pane (overlaid on top).
    let pane = stripped(&right_pane_contents(&state, 40, 12));
    assert!(pane.contains("─ note"), "the overlay is titled");
    assert!(
        !pane.contains("編集中"),
        "the read-only note carries no editing marker"
    );
    assert!(pane.contains("do X"));
    assert!(pane.contains("do Y"));

    // Back on the root row there is no session note, so no overlay shows.
    state.overview_move_up();
    let root = stripped(&right_pane_contents(&state, 40, 12));
    assert!(!root.contains("do X"));
}

#[test]
fn read_only_note_overlay_elides_a_long_note() {
    let note = (0..8)
        .map(|i| format!("todo {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let state = overview_state_with_note(&note);
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
    let idle = overview_state_with_note("todo");
    let idle_rows = right_pane_contents(&idle, 40, 12);
    let idle_box_top = idle_rows
        .iter()
        .position(|l| console::strip_ansi_codes(l).contains('┌'))
        .expect("the read-only box has a top border");

    // Make the same session live (a running shell with a snapshot).
    let mut live = overview_state_with_note("todo");
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
    let mut live = overview_state_with_note("next: ship it");
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
    let mut state = overview_state_with_note(&note);
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
fn render_frame_keeps_the_pane_divider_straight_across_commit_stat_rows() {
    // A session's detail line carries the `↑N ↓M` commit-divergence marker. Its
    // arrows are ambiguous-width glyphs the terminal paints one column wide, and
    // the detail cluster reserves them at that width — so the composed left cell
    // must measure them the same, or its `│` divider jogs left on those rows. This
    // regression guards that the divider column is constant down every body row.
    use crate::domain::workspace_state::{AheadBehind, DiffStat};
    let mut behind = worktree(Some("focus-prompt"), false, BranchStatus::Pushed);
    behind.diff = Some(DiffStat {
        added: 300,
        removed: 249,
    });
    behind.ahead_behind = Some(AheadBehind {
        ahead: 1,
        behind: 7,
    });
    behind.pr = vec![PrLink {
        number: 1,
        url: "https://github.com/o/r/pull/1".into(),
    }];
    let state = state_with(vec![
        worktree(Some("main"), true, BranchStatus::Pushed),
        behind,
    ]);
    let frame = render_frame(24, 120, &state);
    // The `↑1 ↓7` marker renders in full (not clipped to an ellipsis) — the
    // over-count that shifted the divider also truncated the cluster.
    let joined = stripped(&frame);
    assert!(joined.contains("↑1 ↓7"), "the commit marker is not clipped");
    // Every body row's divider sits in the same display column.
    let bars: Vec<usize> = frame
        .iter()
        .filter_map(|l| {
            let s = console::strip_ansi_codes(l);
            s.char_indices()
                .find(|(_, c)| *c == '│')
                .map(|(b, _)| console::measure_text_width(&s[..b]))
        })
        .collect();
    assert!(bars.len() > 4, "the body has several divided rows");
    assert!(
        bars.iter().all(|c| *c == bars[0]),
        "the `│` divider stays in one column: {bars:?}"
    );
}

#[test]
fn note_editor_overlay_keeps_the_preview_visible_behind_it() {
    // Editing is a floating box at the top, so the preview/terminal underneath
    // stays visible below it (the screen never switches).
    let mut state = overview_state_with_note("hi");
    assert!(state.overview_begin_note());
    let pane = stripped(&right_pane_contents(&state, 40, 16));
    assert!(pane.contains("─ note"), "the editor box shows");
    // The idle session's resting mascot still shows behind the box.
    assert!(
        pane.contains("(='-')"),
        "the preview behind the box is still visible"
    );
}
