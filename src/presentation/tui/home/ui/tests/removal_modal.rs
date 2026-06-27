use super::*;

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
    // The modal floats over the live workspace: the chrome (here the 切替 footer)
    // shows through around it rather than a black backdrop.
    assert!(joined.contains("[switch]"));
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
fn render_frame_overlays_the_update_confirmation_modal() {
    let mut state = state_with_sessions(&["alpha"]);
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.open_update_confirm();
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("アップデート"));
    assert!(joined.contains("v9.9.9"));
    assert!(joined.contains("再起動"));
    assert!(joined.contains("y / Enter: 更新"));
    // Every bordered line shares the same width (no line overflows `INNER`).
    let widths: Vec<usize> = joined
        .lines()
        .filter(|line| line.trim_start().starts_with('│'))
        .map(|line| console::measure_text_width(line.trim()))
        .collect();
    assert!(widths.iter().all(|&w| w == widths[0]));
}

#[test]
fn render_frame_skips_the_update_modal_when_no_update_is_known() {
    // The flag is only ever set with an update pending; defend against a stale
    // flag with no version by falling through to the normal frame rather than
    // showing an empty modal.
    let mut state = state_with_sessions(&["alpha"]);
    state.open_update_confirm();
    assert!(state.update().is_none());
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(!joined.contains("ダウンロードして入れ替える"));
    // The normal workspace frame shows instead (the session sidebar is present).
    assert!(joined.contains("alpha"));
}

#[test]
fn render_frame_quit_confirmation_modal_with_nothing_live_asks_a_plain_quit() {
    // Ctrl-Q raises the modal even with no live session; with `live == 0` it must
    // ask a plain "quit?" rather than warn about agents that are not running.
    let mut state = state_with_sessions(&["alpha"]);
    state.open_quit_confirm();
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("Quit usagi?"));
    assert!(joined.contains("No sessions are running."));
    assert!(joined.contains("y / Enter: quit"));
    assert!(!joined.contains("Close anyway?"));
}

#[test]
fn render_frame_removal_modal_reports_when_there_are_no_sessions() {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.open_remove_modal(false);
    let frame = render_frame(24, 80, &state);
    let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
    assert!(joined.contains("No sessions to remove"));
    // With nothing to remove the modal omits its "N selected" count line (the
    // workspace behind the overlay may say "selected" elsewhere, so match the
    // count line specifically).
    assert!(!joined.contains("0 selected"));
}

#[test]
fn remove_modal_scrolls_to_keep_the_cursor_visible() {
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
fn remove_modal_overlays_a_well_formed_box_over_the_workspace() {
    let mut state = state_with_sessions(&["scroll", "session-new", "config"]);
    state.open_remove_modal(false);
    let frame = render_frame(24, 80, &state);
    let stripped: Vec<String> = frame
        .iter()
        .map(|l| console::strip_ansi_codes(l).into_owned())
        .collect();

    // The contiguous box-drawing run between `open` and `close` on a line — the
    // box now floats with workspace content to its left, so the border no longer
    // starts the line and must be sliced out.
    fn box_run(line: &str, open: char, close: char) -> Option<String> {
        let mut out = String::new();
        let mut started = false;
        for c in line.chars() {
            if c == open {
                started = true;
            }
            if started {
                out.push(c);
            }
            if started && c == close {
                return Some(out);
            }
        }
        None
    }

    // The modal's top border carries the title; a bottom border of the same width
    // closes the box.
    let top_w = stripped
        .iter()
        .find(|l| l.contains("Remove sessions"))
        .and_then(|l| box_run(l, '┌', '┐'))
        .map(|run| console::measure_text_width(&run))
        .expect("the modal draws a titled top border");
    let has_matching_bottom = stripped
        .iter()
        .filter_map(|l| box_run(l, '└', '┘'))
        .any(|run| console::measure_text_width(&run) == top_w);
    assert!(
        has_matching_bottom,
        "the modal box closes at the same width"
    );

    // It is an overlay, not a full-screen frame: the 切替 footer shows through.
    assert!(stripped.join("\n").contains("[switch]"));
}
