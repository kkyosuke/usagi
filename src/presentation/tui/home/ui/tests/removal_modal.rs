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
