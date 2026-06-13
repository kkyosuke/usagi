use console::style;

use crate::presentation::tui::widgets;

use super::state::ProjectList;

const TITLE: &str = "Open Project";
const SUBTITLE: &str = "Open a registered workspace";

/// Fixed width of the list block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 52;

/// Shown in place of the list when no workspaces are registered.
const EMPTY_MESSAGE: &str = "No workspaces yet. Choose \"New\" to add one.";

/// Builds the centred mascot, title, and subtitle block.
///
/// Vertical placement is handled by [`render_frame`], so this adds no leading
/// padding.
fn header_lines(width: usize) -> Vec<String> {
    let mut lines = widgets::rabbit_lines(width);
    lines.push(String::new());
    lines.push(widgets::title_line(width, TITLE));
    lines.push(widgets::dim_line(width, SUBTITLE));
    lines
}

/// Shortens `text` to at most `max` columns, keeping the tail and prefixing an
/// ellipsis when truncated (a path's tail is the most informative part).
fn truncate_start(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = text.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}

/// Builds one project row: a `>` cursor for the selected entry, the name in a
/// fixed-width column, and the (possibly truncated) path dimmed beside it.
fn project_row(
    block_pad: &str,
    name: &str,
    name_width: usize,
    path: &str,
    selected: bool,
) -> String {
    let marker = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let padded = format!("{name:<name_width$}");
    let name = if selected {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    };

    // "> " + name column + "  " precede the path; cap the path to the block.
    let path_budget = BLOCK_WIDTH.saturating_sub(2 + name_width + 2);
    let path = style(truncate_start(path, path_budget)).dim().to_string();

    format!("{block_pad}{marker} {name}  {path}")
}

/// Builds the list body: one row per workspace, or a centred empty message.
fn list_lines(width: usize, block_pad: &str, list: &ProjectList) -> Vec<String> {
    if list.is_empty() {
        return vec![widgets::dim_line(width, EMPTY_MESSAGE)];
    }

    let name_width = list
        .workspaces()
        .iter()
        .map(|w| w.name.chars().count())
        .max()
        .unwrap_or(0);

    list.workspaces()
        .iter()
        .enumerate()
        .map(|(i, w)| {
            project_row(
                block_pad,
                &w.name,
                name_width,
                &w.path.to_string_lossy(),
                i == list.selected_index(),
            )
        })
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

/// Builds the full project selection frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    list: &ProjectList,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));

    // The body (mascot, title, list and notice slot) is centred vertically; the
    // footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width);
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
    use crate::domain::workspace::Workspace;

    fn list_with(names: &[&str]) -> ProjectList {
        ProjectList::new(
            names
                .iter()
                .map(|n| Workspace::new(*n, format!("/home/user/projects/{n}")))
                .collect(),
        )
    }

    #[test]
    fn header_lines_render_mascot_title_and_subtitle() {
        let lines = header_lines(80);
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("Open Project"));
        assert!(joined.contains("Open a registered workspace"));
    }

    #[test]
    fn truncate_start_keeps_short_text() {
        assert_eq!(truncate_start("/a/b", 10), "/a/b");
    }

    #[test]
    fn truncate_start_keeps_the_tail_with_ellipsis() {
        // Eight chars capped to five: ellipsis + last four characters.
        assert_eq!(truncate_start("/a/b/c/d", 5), "…/c/d");
    }

    #[test]
    fn truncate_start_with_zero_budget_is_empty() {
        assert_eq!(truncate_start("/a/b", 0), "");
    }

    #[test]
    fn project_row_marks_only_the_selected_entry() {
        let selected = project_row("", "alpha", 5, "/p/alpha", true);
        assert!(selected.contains('>'));
        assert!(selected.contains("alpha"));
        let unselected = project_row("", "beta", 5, "/p/beta", false);
        assert!(!unselected.contains('>'));
        assert!(unselected.contains("beta"));
    }

    #[test]
    fn list_lines_render_one_row_per_workspace() {
        let list = list_with(&["alpha", "beta"]);
        let lines = list_lines(80, "", &list);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("alpha"));
        assert!(lines[1].contains("beta"));
        // Only the first (selected) row carries the cursor.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
    }

    #[test]
    fn list_lines_show_empty_message_when_no_workspaces() {
        let list = ProjectList::new(Vec::new());
        let lines = list_lines(80, "", &list);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No workspaces yet"));
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
        let list = list_with(&["alpha"]);
        let frame = render_frame(0, 0, &list, Some("coming soon"));
        let joined = frame.join("\n");
        assert!(joined.contains("Open Project"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("coming soon"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_renders_empty_state() {
        let list = ProjectList::new(Vec::new());
        let frame = render_frame(24, 80, &list, None);
        let joined = frame.join("\n");
        assert!(joined.contains("No workspaces yet"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let list = list_with(&["alpha"]);
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
        let list = list_with(&["alpha"]);
        let frame = render_frame(3, 80, &list, None);
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let list = list_with(&["alpha"]);
        let without = render_frame(24, 80, &list, None);
        let with = render_frame(24, 80, &list, Some("Opening \"alpha\" is coming soon 🐰"));
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn long_paths_are_truncated_into_the_block() {
        let list = ProjectList::new(vec![Workspace::new(
            "proj",
            "/very/deeply/nested/directory/structure/that/keeps/going/proj",
        )]);
        let lines = list_lines(80, "", &list);
        // The ellipsis marks that the long path was shortened to fit the block.
        assert!(lines[0].contains('…'));
    }
}
