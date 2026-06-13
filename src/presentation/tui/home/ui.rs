use console::style;

use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::widgets;

use super::state::WorktreeList;

/// Fixed width of the worktree block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 56;

/// Widest a branch name is allowed to grow before it is truncated.
const BRANCH_MAX: usize = 24;

/// Shown in place of the list when the workspace has no recorded worktrees.
const EMPTY_MESSAGE: &str = "No worktrees recorded yet. Run usagi to sync.";

/// Shown for a worktree whose HEAD is detached (no branch).
const DETACHED: &str = "(detached)";

/// Builds the centred mascot, title (workspace name), and subtitle block.
///
/// Vertical placement is handled by [`render_frame`], so this adds no leading
/// padding.
fn header_lines(width: usize, list: &WorktreeList) -> Vec<String> {
    let count = list.worktrees().len();
    let subtitle = format!("{count} worktree{}", if count == 1 { "" } else { "s" });

    let mut lines = widgets::rabbit_lines(width);
    lines.push(String::new());
    lines.push(widgets::title_line(width, list.workspace_name()));
    lines.push(widgets::dim_line(width, &subtitle));
    lines
}

/// Shortens `text` to at most `max` columns, keeping the head and appending an
/// ellipsis when truncated (a branch's head is the most informative part).
fn truncate_end(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{head}…")
}

/// The fixed-width, colour-coded label for a branch's lifecycle status.
fn status_label(status: BranchStatus) -> String {
    let padded = format!("{:<6}", status.as_str());
    match status {
        BranchStatus::Local => style(padded).yellow().to_string(),
        BranchStatus::Pushed => style(padded).green().to_string(),
        BranchStatus::Merged => style(padded).dim().to_string(),
    }
}

/// Builds one worktree row: a `>` cursor for the selected entry, a `●` marker
/// for the primary worktree, the branch name, its status, and its short hash.
fn worktree_row(
    block_pad: &str,
    worktree: &WorktreeState,
    branch_width: usize,
    selected: bool,
) -> String {
    let marker = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let primary = if worktree.primary {
        style("●").magenta().to_string()
    } else {
        " ".to_string()
    };

    let branch = worktree.branch.as_deref().unwrap_or(DETACHED);
    let branch = format!("{:<branch_width$}", truncate_end(branch, BRANCH_MAX));
    let branch = if selected {
        style(branch).cyan().bold().to_string()
    } else {
        style(branch).cyan().to_string()
    };

    let status = status_label(worktree.status);
    let head = style(&worktree.head).dim().to_string();

    format!("{block_pad}{marker} {primary} {branch}  {status}  {head}")
}

/// Builds the list body: one row per worktree, or a centred empty message.
fn list_lines(width: usize, block_pad: &str, list: &WorktreeList) -> Vec<String> {
    if list.is_empty() {
        return vec![widgets::dim_line(width, EMPTY_MESSAGE)];
    }

    let branch_width = list
        .worktrees()
        .iter()
        .map(|w| {
            w.branch
                .as_deref()
                .unwrap_or(DETACHED)
                .chars()
                .count()
                .min(BRANCH_MAX)
        })
        .max()
        .unwrap_or(0);

    list.worktrees()
        .iter()
        .enumerate()
        .map(|(i, w)| worktree_row(block_pad, w, branch_width, i == list.selected_index()))
        .collect()
}

/// Builds the transient notice line below the list.
///
/// Always returns two lines — a blank separator plus the notice slot (blank
/// when absent) — so showing or clearing a notice never shifts the layout.
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    let slot = match notice {
        Some(notice) => format!("{block_pad}{}", style(notice).yellow()),
        None => String::new(),
    };
    vec![String::new(), slot]
}

/// Builds the footer help line.
///
/// Returns the footer text only; [`render_frame`] pins it to the bottom edge.
fn footer_lines(width: usize) -> Vec<String> {
    vec![widgets::dim_line(
        width,
        "↑↓: move / Enter: open / Esc: back",
    )]
}

/// Builds the full home-screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    list: &WorktreeList,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));

    // The body (mascot, title, list and notice slot) is centred vertically; the
    // footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width, list);
    body.push(String::new());
    body.extend(list_lines(width, &block_pad, list));
    body.extend(notice_lines(&block_pad, notice));
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);

    // Centre the body in the space above the footer.
    let top_padding = height.saturating_sub(body.len() + footer.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);

    // Push the footer down to the bottom row of the frame.
    let bottom_padding = height.saturating_sub(lines.len() + footer.len());
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.extend(footer);

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
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

    #[test]
    fn header_lines_render_mascot_title_and_count() {
        let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let lines = header_lines(80, &list);
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("usagi"));
        // Singular when there is exactly one worktree.
        assert!(joined.contains("1 worktree"));
        assert!(!joined.contains("1 worktrees"));
    }

    #[test]
    fn header_count_is_pluralised() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let joined = header_lines(80, &list).join("\n");
        assert!(joined.contains("2 worktrees"));
    }

    #[test]
    fn truncate_end_keeps_short_text() {
        assert_eq!(truncate_end("main", 10), "main");
    }

    #[test]
    fn truncate_end_keeps_the_head_with_ellipsis() {
        // Eight chars capped to five: first four characters + ellipsis.
        assert_eq!(truncate_end("feature/x", 5), "feat…");
    }

    #[test]
    fn truncate_end_with_zero_budget_is_empty() {
        assert_eq!(truncate_end("main", 0), "");
    }

    #[test]
    fn status_label_colours_each_variant() {
        // Each label keeps the underlying status text regardless of styling.
        assert!(status_label(BranchStatus::Local).contains("local"));
        assert!(status_label(BranchStatus::Pushed).contains("pushed"));
        assert!(status_label(BranchStatus::Merged).contains("merged"));
    }

    #[test]
    fn worktree_row_marks_only_the_selected_entry() {
        let wt = worktree(Some("main"), true, BranchStatus::Pushed);
        let selected = worktree_row("", &wt, 7, true);
        assert!(selected.contains('>'));
        assert!(selected.contains("main"));
        // The primary marker is present.
        assert!(selected.contains('●'));

        let other = worktree(Some("feature"), false, BranchStatus::Local);
        let unselected = worktree_row("", &other, 7, false);
        assert!(!unselected.contains('>'));
        assert!(unselected.contains("feature"));
    }

    #[test]
    fn worktree_row_renders_detached_head() {
        let wt = worktree(None, false, BranchStatus::Local);
        let row = worktree_row("", &wt, 10, false);
        assert!(row.contains("(detached)"));
    }

    #[test]
    fn list_lines_render_one_row_per_worktree() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let lines = list_lines(80, "", &list);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("main"));
        assert!(lines[1].contains("feature"));
        // Only the first (selected) row carries the cursor.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
    }

    #[test]
    fn list_lines_show_empty_message_when_no_worktrees() {
        let list = list_with(Vec::new());
        let lines = list_lines(80, "", &list);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No worktrees recorded"));
    }

    #[test]
    fn notice_lines_reserve_a_slot_when_absent() {
        let lines = notice_lines("", None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.is_empty()));
    }

    #[test]
    fn notice_lines_render_text_when_present() {
        let lines = notice_lines("", Some("coming soon"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("coming soon"));
    }

    #[test]
    fn footer_lines_include_help_text() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains("Esc")));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let frame = render_frame(0, 0, &list, Some("coming soon"));
        let joined = frame.join("\n");
        assert!(joined.contains("usagi"));
        assert!(joined.contains("main"));
        assert!(joined.contains("coming soon"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_renders_empty_state() {
        let list = list_with(Vec::new());
        let frame = render_frame(24, 80, &list, None);
        let joined = frame.join("\n");
        assert!(joined.contains("No worktrees recorded"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let height = 40;
        let frame = render_frame(height, 80, &list, None);

        assert_eq!(frame.len(), height);
        assert!(frame.last().unwrap().contains("Esc"));
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let frame = render_frame(3, 80, &list, None);
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let list = list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let without = render_frame(24, 80, &list, None);
        let with = render_frame(
            24,
            80,
            &list,
            Some("Switching to \"main\" is coming soon 🐰"),
        );
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn long_branch_names_are_truncated_into_the_block() {
        let list = list_with(vec![worktree(
            Some("feature/a-very-long-branch-name-that-keeps-going"),
            false,
            BranchStatus::Local,
        )]);
        let lines = list_lines(80, "", &list);
        // The ellipsis marks that the long branch name was shortened.
        assert!(lines[0].contains('…'));
    }
}
