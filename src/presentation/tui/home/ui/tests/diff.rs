use super::*;

/// A single-file patch: file header + index/---/+++ meta, a hunk, a context line,
/// a removed/added replacement (word-level change), and a closing context.
const PATCH: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 111..222 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,3 +1,3 @@\n\
 fn main() {\n\
-    let value = old_thing;\n\
+    let value = new_thing;\n\
 }";

/// A three-file patch across two directories: an edit under `src/ui/`, an edit
/// under `src/`, and a top-level deletion — the shape the explorer tree groups.
const MULTI: &str = "diff --git a/src/main.rs b/src/main.rs\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n\
diff --git a/src/ui/render.rs b/src/ui/render.rs\n\
--- /dev/null\n\
+++ b/src/ui/render.rs\n\
@@ -0,0 +1,2 @@\n\
+one\n\
+two\n\
diff --git a/README.md b/README.md\n\
--- a/README.md\n\
+++ /dev/null\n\
@@ -1 +0,0 @@\n\
-gone\n";

/// A home state with the diff view open on `patch`, titled `title`.
fn diff_state(title: &str, patch: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_diff_result(Ok((title.to_string(), patch.to_string())));
    state
}

#[test]
fn diff_header_shows_title_file_count_and_layout() {
    let state = diff_state("feature → main", MULTI);
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 80, 12));
    assert!(out.contains("feature → main"));
    assert!(out.contains("3 files"));
    assert!(out.contains("[unified]"));
}

#[test]
fn diff_pane_renders_the_explorer_tree_with_dirs_and_counts() {
    let state = diff_state("feature → main", MULTI);
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 90, 12));
    // Directory nodes with an expand marker, files with their names and counts.
    assert!(
        out.contains("▾ src/"),
        "expected the src/ directory node: {out}"
    );
    assert!(out.contains("▾ ui/"), "expected the nested ui/ node: {out}");
    assert!(out.contains("render.rs"));
    assert!(out.contains("main.rs"));
    assert!(out.contains("README.md"));
    // The `render.rs` addition shows +2 -0; the README deletion +0 -1.
    assert!(out.contains("+2 -0"), "render.rs add counts: {out}");
    assert!(out.contains("+0 -1"), "README delete counts: {out}");
}

#[test]
fn diff_pane_shows_the_selected_files_diff() {
    // The first file (src/ui/render.rs, directories-first order) is selected, so
    // its added lines show on the right.
    let state = diff_state("feature → main", MULTI);
    let view = state.diff_view().unwrap();
    assert_eq!(view.selected_file().unwrap().path, "src/ui/render.rs");
    let out = stripped(&diff_pane(view, 90, 12));
    assert!(out.contains("diff --git a/src/ui/render.rs"));
    assert!(out.contains("one"));
    assert!(out.contains("two"));
    // A file from a different section is not shown while it is not selected.
    assert!(
        !out.contains("gone"),
        "only the selected file's diff renders: {out}"
    );
}

#[test]
fn diff_pane_renders_the_split_layout_side_by_side() {
    let mut state = diff_state("feature → main", PATCH);
    state.diff_toggle_split();
    let view = state.diff_view().unwrap();
    let rows = diff_pane(view, 120, 12);
    let out = stripped(&rows);
    assert!(out.contains("[split]"));
    // The old and new content share a paired row in the diff column.
    let paired = rows
        .iter()
        .map(|r| console::strip_ansi_codes(r).into_owned())
        .find(|r| r.contains("old_thing"))
        .expect("a row shows the removed content");
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
    assert!(out.contains("main → main"));
    assert!(out.contains("No changes"));
}

#[test]
fn diff_pane_with_one_row_shows_only_the_header() {
    let state = diff_state("feature → main", MULTI);
    let view = state.diff_view().unwrap();
    let rows = diff_pane(view, 60, 1);
    assert_eq!(rows.len(), 1);
    assert!(stripped(&rows).contains("feature → main"));
}

#[test]
fn diff_pane_narrow_pane_falls_back_to_diff_only() {
    // Too narrow to split usefully: the explorer is dropped and the diff fills the
    // pane, so there is no column separator but the selected file's diff still shows.
    let state = diff_state("feature → main", PATCH);
    let view = state.diff_view().unwrap();
    let rows = diff_pane(view, 20, 8);
    let out = stripped(&rows);
    assert!(out.contains("fn main"));
    // No tree column means no changed-file names in a side list.
    assert!(
        rows.iter().all(|r| !r.contains("│")),
        "narrow pane has no separator"
    );
}

#[test]
fn diff_pane_clips_content_wider_than_the_pane() {
    // A very long added line rendered into a narrow pane must not overflow: every
    // row is clipped to the pane width (measured without ANSI).
    let long = format!("diff --git a/x b/x\n@@ -0,0 +1 @@\n+{}\n", "x".repeat(300));
    let state = diff_state("feature → main", &long);
    let view = state.diff_view().unwrap();
    for row in diff_pane(view, 40, 6) {
        let width = console::measure_text_width(&console::strip_ansi_codes(&row));
        assert!(width <= 40, "row overflows: {width}");
    }
}

#[test]
fn diff_pane_goes_through_the_right_pane_dispatch() {
    // Opened from the palette, the diff view takes over the whole right pane.
    let state = diff_state("feature → main", MULTI);
    let out = stripped(&right_pane_contents(&state, 90, 12));
    assert!(out.contains("feature → main"));
    assert!(out.contains("render.rs"));
}

#[test]
fn diff_pane_collapsed_directory_hides_its_files() {
    // Fold `src/` and the diff of a file under it drops out of the explorer, while
    // the top-level README.md stays.
    let mut state = diff_state("feature → main", MULTI);
    // Cursor starts on render.rs (first file); step up to `src/` and collapse it.
    state.diff_move_up(); // render.rs -> ui/
    state.diff_move_up(); // ui/ -> src/
    state.diff_collapse();
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 90, 12));
    assert!(out.contains("▸ src/"), "src/ is collapsed: {out}");
    // `main.rs` is folded away and is not the selected file's diff, so it is gone;
    // the top-level README.md stays.
    assert!(!out.contains("main.rs"), "a folded file is hidden: {out}");
    assert!(out.contains("README.md"), "a sibling stays visible: {out}");
}

#[test]
fn diff_pane_marks_a_selected_expanded_directory() {
    // When the cursor sits on an expanded directory, the selected row shows the
    // open marker `▾` (the selected-row branch, distinct from a selected file).
    let mut state = diff_state("feature → main", MULTI);
    state.diff_move_up(); // render.rs -> ui/
    state.diff_move_up(); // ui/ -> src/ (expanded, now selected)
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 90, 12));
    assert!(
        out.contains("▾ src/"),
        "the selected expanded directory shows the open marker: {out}"
    );
}

#[test]
fn diff_pane_renders_with_the_diff_pane_focused() {
    // With the diff pane focused the selection and separator take their dimmed
    // styling; the render still shows the explorer and the selected file's diff.
    let mut state = diff_state("feature → main", MULTI);
    state.diff_toggle_focus();
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 90, 12));
    assert!(out.contains("render.rs"));
    assert!(out.contains("one"));
}

#[test]
fn diff_pane_renders_an_empty_added_line() {
    // A blank added line has no content spans; the row still renders without
    // panicking, and the neighbouring line shows.
    let state = diff_state(
        "feature → main",
        "diff --git a/f b/f\n@@ -0,0 +2 @@\n+first\n+\n",
    );
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 60, 6));
    assert!(out.contains("first"));
}

#[test]
fn diff_pane_split_places_surplus_and_pure_insertions_on_one_side() {
    // Two removed, one added, plus a pure insertion: the split layout leaves the
    // unmatched halves blank rather than misaligning the columns.
    let patch = "diff --git a/f b/f\n@@ -1,2 +1,2 @@\n-alpha\n-beta\n+ALPHA\n+brand new\n";
    let mut state = diff_state("feature → main", patch);
    state.diff_toggle_split();
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 100, 10));
    assert!(out.contains("alpha"));
    assert!(out.contains("brand new"));
}

#[test]
fn diff_pane_binary_file_shows_no_line_counts() {
    // A binary change has no counted add/remove lines, so its explorer row carries
    // the file name without a `+A -B` badge.
    let patch = "diff --git a/logo.png b/logo.png\n\
index 111..222 100644\n\
Binary files a/logo.png and b/logo.png differ\n";
    let state = diff_state("feature → main", patch);
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 70, 8));
    assert!(out.contains("logo.png"));
    assert!(
        !out.contains("+0 -0"),
        "no counts badge for a binary file: {out}"
    );
}

#[test]
fn diff_pane_windows_a_tall_tree_to_keep_the_cursor_visible() {
    // More files than the pane is tall: moving the cursor down past the window
    // still shows the selected file's row (the window follows the cursor).
    let mut patch = String::new();
    // Zero-padded names so the tree's alphabetical order matches the numeric one.
    for i in 0..20 {
        patch.push_str(&format!(
            "diff --git a/f{i:02}.rs b/f{i:02}.rs\n@@ -1 +1 @@\n-a\n+b\n"
        ));
    }
    let mut state = diff_state("feature → main", &patch);
    for _ in 0..15 {
        state.diff_move_down();
    }
    let view = state.diff_view().unwrap();
    let out = stripped(&diff_pane(view, 80, 6));
    // f15.rs is the selected file after 15 downs from f00; its row is in the window.
    assert!(out.contains("f15.rs"), "the cursor stays visible: {out}");
}
