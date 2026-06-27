use chrono::{DateTime, Utc};
use console::style;

use crate::presentation::tui::welcome;
use crate::presentation::tui::widgets;
use crate::usecase::workspace::WorkspaceOverview;

use super::state::ProjectList;

const TITLE: &str = "Open Project";
const SUBTITLE: &str = "Open a registered workspace";

/// Shown in place of the list when no workspaces are registered.
const EMPTY_MESSAGE: &str = "No workspaces yet. Choose \"New\" to add one.";

/// Glyphs tagging each figure on a project's stats line. They are plain
/// monochrome symbols drawn in the terminal's text font (not colour emoji), so
/// they inherit the dimmed style of the line and align as single columns.
const SESSION_ICON: &str = "⎇"; // sessions ≈ per-session worktrees / branches
const ISSUE_ICON: &str = "●"; // issues still open
const CLOCK_ICON: &str = "◷"; // when the workspace was last used

/// Indent of the stats line, placing it under the project name (past the two
/// columns the `> ` cursor and its trailing space occupy).
const STATS_INDENT: &str = "    ";

/// Formats how long ago `from` was, relative to `now`, as a compact label:
/// `just now`, `5m ago`, `3h ago`, `2d ago`, `3w ago`, falling back to an
/// absolute `YYYY-MM-DD` date once it is over a month old. A `from` in the
/// future (clock skew) reads as `just now`.
fn relative_time(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - from).num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    if days < 30 {
        return format!("{}w ago", days / 7);
    }
    from.format("%Y-%m-%d").to_string()
}

/// Builds the dimmed stats line shown under a project: session count, open-issue
/// count, and how recently the workspace was used, each tagged with a glyph.
fn stats_line(block_pad: &str, overview: &WorkspaceOverview, now: DateTime<Utc>) -> String {
    let sessions = overview.session_count;
    let session_word = if sessions == 1 { "session" } else { "sessions" };
    let open = overview.open_issue_count;
    let updated = relative_time(overview.workspace.updated_at, now);
    let content = format!(
        "{SESSION_ICON} {sessions} {session_word}   {ISSUE_ICON} {open} open   {CLOCK_ICON} {updated}",
    );
    style(format!("{block_pad}{STATS_INDENT}{content}"))
        .dim()
        .to_string()
}

/// Builds the centred mascot, title, and subtitle block.
///
/// Vertical placement is handled by [`render_frame`], so this adds no leading
/// padding.
fn header_lines(width: usize) -> Vec<String> {
    widgets::header_lines(width, TITLE, Some(SUBTITLE))
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
    let marker = widgets::cursor_marker(selected);

    let padded = format!("{name:<name_width$}");
    let name = if selected {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    };

    // "> " + name column + "  " precede the path; cap the path to the block.
    let path_budget = widgets::BLOCK_WIDTH.saturating_sub(2 + name_width + 2);
    let path = style(truncate_start(path, path_budget)).dim().to_string();

    format!("{block_pad}{marker} {name}  {path}")
}

/// Builds the list body: two rows per workspace — a name/path row and a dimmed
/// stats row below it — or a centred empty message when nothing is registered.
fn list_lines(
    width: usize,
    block_pad: &str,
    list: &ProjectList,
    now: DateTime<Utc>,
) -> Vec<String> {
    if list.is_empty() {
        return vec![widgets::dim_line(width, EMPTY_MESSAGE)];
    }

    let name_width = list
        .overviews()
        .iter()
        .map(|o| o.workspace.name.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::with_capacity(list.overviews().len() * 2);
    for (i, overview) in list.overviews().iter().enumerate() {
        lines.push(project_row(
            block_pad,
            &overview.workspace.name,
            name_width,
            &overview.workspace.path.to_string_lossy(),
            i == list.selected_index(),
        ));
        lines.push(stats_line(block_pad, overview, now));
    }
    lines
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

/// Builds the body block — mascot, title, list, and notice slot — that
/// [`render_frame`] centres vertically above the footer. The mascot occupies its
/// first [`widgets::rabbit_height`] rows. Extracted so [`mascot_top`] can measure
/// where the mascot lands without rebuilding the whole frame.
fn body_lines(
    width: usize,
    block_pad: &str,
    list: &ProjectList,
    notice: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut body = header_lines(width);
    body.push(String::new());
    body.extend(list_lines(width, block_pad, list, now));
    body.extend(notice_lines(block_pad, notice));
    body
}

/// The frame row the mascot's first line occupies — the shared
/// [`welcome::mascot_top_padding`] anchor, so the rabbit sits at the same row as
/// on the welcome / New / Config screens and never jumps between them. It is only
/// pulled up from that row when the body would otherwise overrun the footer on a
/// short terminal.
///
/// [`render_frame`] places the mascot here, and the open→home transition lifts
/// the rabbit off exactly this row, so it glides out from its resting place.
pub fn mascot_top(
    raw_height: usize,
    raw_width: usize,
    list: &ProjectList,
    notice: Option<&str>,
    now: DateTime<Utc>,
) -> usize {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, widgets::BLOCK_WIDTH));
    let body = body_lines(width, &block_pad, list, notice, now);
    let footer = footer_lines(width);
    // Anchor to the shared mascot row, but never so low that the body overflows
    // the footer on a short terminal.
    let available = height.saturating_sub(body.len() + footer.len());
    welcome::mascot_top_padding(height).min(available)
}

/// Builds the full project selection frame for a raw terminal size. `now` dates
/// each project's "last used" figure relative to the current time.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    list: &ProjectList,
    notice: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, widgets::BLOCK_WIDTH));

    // The body (mascot, title, list and notice slot) hangs from the shared mascot
    // row; the footer is pinned to the bottom edge of the frame.
    let body = body_lines(width, &block_pad, list, notice, now);
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);

    // Pin the mascot to the shared row so it never jumps from the welcome screen.
    let top_padding = mascot_top(raw_height, raw_width, list, notice, now);
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

/// Inner content width of the stale-workspace confirmation modal.
const CONFIRM_INNER: usize = 46;

/// Builds the confirmation modal shown when the user opens a workspace whose
/// directory no longer exists. It offers to drop the stale entry from the list
/// instead of opening a path that is not there.
///
/// Drawn over an otherwise blank frame (like the home screen's quit prompt) so
/// the event loop can clear and repaint it the same way as the full list.
pub fn confirm_remove_frame(raw_height: usize, raw_width: usize, name: &str) -> Vec<String> {
    let body = vec![
        style(format!("\"{name}\" no longer exists on disk."))
            .dim()
            .to_string(),
        String::new(),
        style("Remove it from the list?").to_string(),
        String::new(),
        style("y / Enter: remove   n / Esc: cancel")
            .dim()
            .to_string(),
    ];
    widgets::render_modal(
        raw_height,
        raw_width,
        "Workspace not found",
        CONFIRM_INNER,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace::Workspace;

    /// A fixed reference time for deterministic stats rendering in tests.
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn overview(name: &str) -> WorkspaceOverview {
        WorkspaceOverview {
            workspace: Workspace::new(name, format!("/home/user/projects/{name}")),
            session_count: 0,
            open_issue_count: 0,
        }
    }

    fn list_with(names: &[&str]) -> ProjectList {
        ProjectList::new(names.iter().map(|n| overview(n)).collect())
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
    fn list_lines_render_a_name_and_stats_row_per_workspace() {
        let list = list_with(&["alpha", "beta"]);
        let lines = list_lines(80, "", &list, now());
        // Two rows per workspace: the name/path row and the stats row.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("alpha"));
        assert!(lines[2].contains("beta"));
        // Each workspace's stats row carries the figures.
        assert!(lines[1].contains("sessions") && lines[1].contains("open"));
        assert!(lines[3].contains("sessions") && lines[3].contains("open"));
        // Only the first (selected) name row carries the cursor.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
    }

    #[test]
    fn list_lines_show_empty_message_when_no_workspaces() {
        let list = ProjectList::new(Vec::new());
        let lines = list_lines(80, "", &list, now());
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
        let frame = render_frame(0, 0, &list, Some("coming soon"), now());
        let joined = frame.join("\n");
        assert!(joined.contains("Open Project"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("coming soon"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_renders_empty_state() {
        let list = ProjectList::new(Vec::new());
        let frame = render_frame(24, 80, &list, None, now());
        let joined = frame.join("\n");
        assert!(joined.contains("No workspaces yet"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let list = list_with(&["alpha"]);
        let height = 40;
        let frame = render_frame(height, 80, &list, None, now());

        assert_eq!(frame.len(), height);
        assert!(frame.last().unwrap().contains("Esc"));
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn mascot_top_points_at_the_rabbit_row_in_the_rendered_frame() {
        // The reported row is exactly where `render_frame` draws the mascot's first
        // line, so the transition lifts the rabbit off without a jump.
        let list = list_with(&["alpha"]);
        for height in [24usize, 40] {
            let frame = render_frame(height, 80, &list, None, now());
            let top = mascot_top(height, 80, &list, None, now());
            // The mascot's first row (the ears) lands on the reported row...
            assert!(console::strip_ansi_codes(&frame[top]).contains("(\\(\\"));
            // ...and the row above it is blank (the centring padding).
            assert!(frame[top - 1].is_empty());
        }
    }

    #[test]
    fn mascot_anchors_to_the_shared_welcome_row_so_it_never_jumps() {
        // The mascot sits on exactly the row the welcome screen places it, so the
        // rabbit does not shift (no CLS) when moving between the screens.
        let list = list_with(&["alpha", "beta"]);
        for height in [24usize, 40, 50] {
            let frame = render_frame(height, 80, &list, None, now());
            let row = welcome::mascot_top_padding(height);
            assert!(console::strip_ansi_codes(&frame[row]).contains("(\\(\\"));
        }
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let list = list_with(&["alpha"]);
        let frame = render_frame(3, 80, &list, None, now());
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let list = list_with(&["alpha"]);
        let without = render_frame(24, 80, &list, None, now());
        let with = render_frame(
            24,
            80,
            &list,
            Some("Opening \"alpha\" is coming soon 🐰"),
            now(),
        );
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn long_paths_are_truncated_into_the_block() {
        let list = ProjectList::new(vec![WorkspaceOverview {
            workspace: Workspace::new(
                "proj",
                "/very/deeply/nested/directory/structure/that/keeps/going/proj",
            ),
            session_count: 0,
            open_issue_count: 0,
        }]);
        let lines = list_lines(80, "", &list, now());
        // The ellipsis marks that the long path was shortened to fit the block.
        assert!(lines[0].contains('…'));
    }

    #[test]
    fn relative_time_scales_from_just_now_to_an_absolute_date() {
        use chrono::Duration;
        let base = now();
        assert_eq!(relative_time(base, base), "just now");
        assert_eq!(
            relative_time(base - Duration::seconds(30), base),
            "just now"
        );
        assert_eq!(relative_time(base - Duration::minutes(5), base), "5m ago");
        assert_eq!(relative_time(base - Duration::hours(3), base), "3h ago");
        assert_eq!(relative_time(base - Duration::days(2), base), "2d ago");
        assert_eq!(relative_time(base - Duration::days(20), base), "2w ago");
        // Over a month falls back to the absolute date of `from`.
        assert_eq!(relative_time(base - Duration::days(60), base), "2026-04-26");
    }

    #[test]
    fn relative_time_treats_a_future_timestamp_as_just_now() {
        use chrono::Duration;
        let base = now();
        // Clock skew can place `from` slightly ahead of `now`.
        assert_eq!(relative_time(base + Duration::hours(1), base), "just now");
    }

    #[test]
    fn stats_line_shows_counts_glyphs_and_singular_session() {
        let overview = WorkspaceOverview {
            workspace: Workspace::new("proj", "/p/proj"),
            session_count: 1,
            open_issue_count: 4,
        };
        let line = stats_line("", &overview, now());
        let plain = console::strip_ansi_codes(&line).into_owned();
        // A single session is not pluralised; the glyphs tag each figure.
        assert!(plain.contains("1 session"));
        assert!(!plain.contains("1 sessions"));
        assert!(plain.contains("4 open"));
        assert!(plain.contains(SESSION_ICON));
        assert!(plain.contains(ISSUE_ICON));
        assert!(plain.contains(CLOCK_ICON));
    }

    #[test]
    fn confirm_remove_frame_names_the_workspace_and_offers_the_choice() {
        let frame = confirm_remove_frame(24, 80, "mosou-wars");
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("Workspace not found"));
        assert!(joined.contains("mosou-wars"));
        assert!(joined.contains("no longer exists"));
        assert!(joined.contains("Remove it from the list?"));
        assert!(joined.contains("y / Enter: remove   n / Esc: cancel"));
    }

    #[test]
    fn stats_line_pluralises_zero_and_many_sessions() {
        let overview = WorkspaceOverview {
            workspace: Workspace::new("proj", "/p/proj"),
            session_count: 3,
            open_issue_count: 0,
        };
        let plain = console::strip_ansi_codes(&stats_line("", &overview, now())).into_owned();
        assert!(plain.contains("3 sessions"));
        assert!(plain.contains("0 open"));
    }
}
