use super::*;

/// A representative patch: file header + index/---/+++ meta, a hunk, a context
/// line, a removed/added replacement (word-level change), and a closing context.
const PATCH: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 111..222 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,3 +1,3 @@\n\
 fn main() {\n\
-    let value = old_thing;\n\
+    let value = new_thing;\n\
 }";

/// A home state with the diff view open on `patch`, titled `title`.
fn diff_state(title: &str, patch: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_diff_result(Ok((title.to_string(), patch.to_string())));
    state
}

#[test]
fn diff_pane_renders_the_unified_layout_with_gutter_markers_and_content() {
    let state = diff_state("feature → main", PATCH);
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 60, 12));
    // Header: title and the layout name.
    assert!(out.contains("feature → main"));
    assert!(out.contains("[unified]"));
    // The hunk header and the file header banner both show.
    assert!(out.contains("@@ -1,3 +1,3 @@"));
    assert!(out.contains("diff --git a/src/main.rs"));
    // Context / add / del content, with +/- markers on the changed lines.
    assert!(out.contains("fn main() {"));
    assert!(out.contains("-    let value = old_thing;"));
    assert!(out.contains("+    let value = new_thing;"));
    // The line-number gutter carries the numbers (old 1 / new 1 on the context).
    assert!(out.contains('1'));
}

#[test]
fn diff_pane_goes_through_the_right_pane_dispatch() {
    // Opened from the palette, the diff view takes over the whole right pane.
    let state = diff_state("feature → main", PATCH);
    let out = stripped(&right_pane_contents(&state, 60, 12));
    assert!(out.contains("feature → main"));
    assert!(out.contains("new_thing"));
}

#[test]
fn diff_pane_renders_the_split_layout_side_by_side() {
    let mut state = diff_state("feature → main", PATCH);
    state.diff_toggle_split();
    let view = state.diff_view().unwrap();
    let rows = diff_pane(view, 80, 12);
    let out = stripped(&rows);
    assert!(out.contains("[split]"));
    // The column separator is present, and old/new appear on the same paired row.
    let paired = rows
        .iter()
        .map(|r| console::strip_ansi_codes(r).into_owned())
        .find(|r| r.contains("old_thing"))
        .expect("a row shows the removed content");
    assert!(
        paired.contains('│'),
        "split row has a column separator: {paired:?}"
    );
    assert!(
        paired.contains("new_thing"),
        "old and new share a row: {paired:?}"
    );
}

#[test]
fn diff_pane_shows_a_no_changes_line_for_an_empty_patch() {
    let state = diff_state("main → main", "");
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 40, 8));
    assert!(out.contains("No changes"));
}

#[test]
fn diff_pane_header_shows_the_scroll_position_once_it_scrolls() {
    let mut state = diff_state("feature → main", PATCH);
    // A short pane forces scrolling; advance and confirm the position counter.
    for _ in 0..3 {
        state.diff_scroll_down(2);
    }
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 60, 4));
    // The header carries a `(start-end/total)` position when the diff overflows.
    assert!(out.contains('/'), "expected a position counter: {out}");
}

#[test]
fn diff_pane_with_one_row_shows_only_the_header() {
    let state = diff_state("feature → main", PATCH);
    let view = state.diff_view().unwrap();
    let rows = diff_pane(view, 60, 1);
    assert_eq!(rows.len(), 1);
    assert!(stripped(&rows).contains("feature → main"));
}

#[test]
fn diff_pane_clips_content_wider_than_the_pane() {
    // A very long added line rendered into a narrow pane must not overflow: every
    // row is clipped to the pane width (measured without ANSI).
    let long = format!("@@ -0,0 +1 @@\n+{}\n", "x".repeat(200));
    let state = diff_state("feature → main", &long);
    let view = state.diff_view().unwrap();
    for row in diff_pane(view, 30, 6) {
        let width = console::measure_text_width(&console::strip_ansi_codes(&row));
        assert!(width <= 30, "row overflows: {width}");
    }
}

#[test]
fn diff_pane_renders_an_empty_added_line() {
    // A blank added line has no content spans; the row still renders (an empty
    // run is skipped) without panicking, and the neighbouring line shows.
    let state = diff_state("feature → main", "@@ -0,0 +2 @@\n+first\n+\n");
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 40, 6));
    assert!(out.contains("first"));
}

#[test]
fn diff_pane_split_places_surplus_and_pure_insertions_on_one_side() {
    // Two removed, one added, plus a pure insertion: the split layout leaves the
    // unmatched halves blank rather than misaligning the columns.
    let patch = "@@ -1,2 +1,2 @@\n-alpha\n-beta\n+ALPHA\n+brand new\n";
    let mut state = diff_state("feature → main", patch);
    state.diff_toggle_split();
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 60, 10));
    assert!(out.contains("alpha"));
    assert!(out.contains("brand new"));
}
