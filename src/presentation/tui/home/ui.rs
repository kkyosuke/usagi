//! Rendering for the home (workspace) screen's three-pane layout.
//!
//! Top to bottom: a title bar, then a body split into the worktree list (left)
//! and the command log (right), then the command input line and a mode-aware
//! footer. All functions take plain data and return styled lines, so the layout
//! is rendered without any terminal IO.

use console::style;

use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::widgets;

use super::state::{HomeState, LineKind, LogLine, Mode, SessionModal, WorktreeList};

/// Shown in place of the list when the workspace has no recorded worktrees.
const EMPTY_MESSAGE: &str = "No worktrees recorded yet. Run usagi to sync.";

/// Shown for a worktree whose HEAD is detached (no branch).
const DETACHED: &str = "(detached)";

/// Visible columns a worktree row spends on everything but the branch name
/// (cursor, active marker, primary marker, separators, and the fixed-width
/// status label).
const ROW_OVERHEAD: usize = 14;

/// The vertical bar (with surrounding spaces) dividing the two panes.
const SEP: &str = " │ ";

/// Visible width of [`SEP`].
const SEP_WIDTH: usize = 3;

/// Block caret drawn at the end of the command input.
const CARET: &str = "▏";

/// Narrowest and widest the left (worktree) pane is allowed to be.
const LEFT_MIN: usize = 16;
const LEFT_MAX: usize = 40;

/// Shortens `text` to at most `max` display columns, appending an ellipsis when
/// it has to cut (the head of the text is the most informative part).
fn clip_to_width(text: &str, max: usize) -> String {
    if console::measure_text_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut candidate = out.clone();
        candidate.push(ch);
        // Reserve one column for the ellipsis.
        if console::measure_text_width(&candidate) > max - 1 {
            break;
        }
        out = candidate;
    }
    out.push('…');
    out
}

/// Right-pads `content` with spaces to fill `width` display columns. Content
/// already at least that wide is returned unchanged.
fn pad_to_width(content: String, width: usize) -> String {
    let visible = console::measure_text_width(&content);
    if visible >= width {
        content
    } else {
        let mut content = content;
        content.push_str(&" ".repeat(width - visible));
        content
    }
}

/// Splits the terminal `width` into the left pane width and the right pane
/// width, leaving room for the divider. The left pane is clamped to a readable
/// band and never overruns the terminal.
fn layout(width: usize) -> (usize, usize) {
    let left = (width / 3).clamp(LEFT_MIN, LEFT_MAX);
    let left = left.min(width.saturating_sub(SEP_WIDTH));
    let right = width.saturating_sub(left + SEP_WIDTH);
    (left, right)
}

/// The centred title bar: workspace name and worktree count.
fn title_bar(width: usize, list: &WorktreeList) -> String {
    let count = list.worktrees().len();
    let label = format!(
        "{} · {count} worktree{}",
        list.workspace_name(),
        if count == 1 { "" } else { "s" }
    );
    widgets::title_line(width, &label)
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

/// Builds one worktree row: a `>` cursor for the selected entry, a `*` marker
/// for the active worktree, a `●` marker for the primary worktree, the
/// (truncated, padded) branch name, and status.
fn worktree_row(
    worktree: &WorktreeState,
    branch_width: usize,
    selected: bool,
    active: bool,
) -> String {
    let marker = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let active_marker = if active {
        style("*").green().bold().to_string()
    } else {
        " ".to_string()
    };

    let primary = if worktree.primary {
        style("●").magenta().to_string()
    } else {
        " ".to_string()
    };

    let branch_text = worktree.branch.as_deref().unwrap_or(DETACHED);
    let branch_text = format!(
        "{:<branch_width$}",
        clip_to_width(branch_text, branch_width)
    );
    // The active or cursored row is emphasized.
    let branch = if active || selected {
        style(branch_text).cyan().bold().to_string()
    } else {
        style(branch_text).cyan().to_string()
    };

    let status = status_label(worktree.status);
    format!("{marker} {active_marker} {primary} {branch}  {status}")
}

/// Builds the left pane: one row per worktree, or a single empty message,
/// trimmed to the available `rows`.
fn left_pane(list: &WorktreeList, left_w: usize, rows: usize) -> Vec<String> {
    let mut lines = if list.is_empty() {
        vec![clip_to_width(EMPTY_MESSAGE, left_w)]
    } else {
        let branch_width = left_w.saturating_sub(ROW_OVERHEAD);
        list.worktrees()
            .iter()
            .enumerate()
            .map(|(i, w)| {
                worktree_row(
                    w,
                    branch_width,
                    i == list.selected_index(),
                    i == list.active_index(),
                )
            })
            .collect()
    };
    lines.truncate(rows);
    lines
}

/// Renders one log line, coloured by kind. Command lines get a `❯` prompt.
fn log_line(line: &LogLine, width: usize) -> String {
    let raw = match line.kind {
        LineKind::Command => format!("❯ {}", line.text),
        _ => line.text.clone(),
    };
    let clipped = clip_to_width(&raw, width);
    match line.kind {
        LineKind::Command => style(clipped).cyan().bold().to_string(),
        LineKind::Output => clipped,
        LineKind::Error => style(clipped).red().to_string(),
        LineKind::Notice => style(clipped).yellow().to_string(),
    }
}

/// Builds the right pane: the tail of the log that fits in `rows`.
fn right_pane(log: &[LogLine], right_w: usize, rows: usize) -> Vec<String> {
    let start = log.len().saturating_sub(rows);
    log[start..].iter().map(|l| log_line(l, right_w)).collect()
}

/// The command input line: an editable prompt in command mode, or a hint in
/// sidebar mode.
fn input_line(state: &HomeState) -> String {
    match state.mode() {
        Mode::Command => {
            let prompt = style("❯").red().bold();
            let text = style(state.input()).cyan();
            format!(" {prompt} {text}{CARET}")
        }
        Mode::Sidebar => style(" Press \":\" to enter a command".to_string())
            .dim()
            .to_string(),
    }
}

/// The mode-aware footer help line.
fn footer_line(width: usize, mode: Mode) -> String {
    let help = match mode {
        Mode::Sidebar => "↑↓: move / Enter: activate / :: command / Esc: back",
        Mode::Command => "Tab: complete / ↑↓: history / Enter: run / Esc: cancel",
    };
    widgets::dim_line(width, help)
}

/// Builds the centred session-name modal over an otherwise blank frame.
fn session_modal_frame(raw_height: usize, raw_width: usize, modal: &SessionModal) -> Vec<String> {
    const INNER: usize = 36;
    const PROMPT: &str = "❯ ";

    // Reserve room for the prompt and trailing caret so a long name never
    // overruns the box border.
    let max_name = INNER.saturating_sub(console::measure_text_width(PROMPT) + 1);
    let name = clip_to_width(modal.input(), max_name);
    let input_line = style(format!("{PROMPT}{name}{CARET}")).cyan().to_string();

    let error_line = match modal.error() {
        Some(err) => style(clip_to_width(err, INNER)).red().to_string(),
        None => String::new(),
    };

    let body = vec![
        style("Enter a name for the new session.").dim().to_string(),
        String::new(),
        input_line,
        error_line,
        String::new(),
        style("Enter: create   Esc: cancel").dim().to_string(),
    ];
    widgets::render_modal(raw_height, raw_width, "New session", INNER, &body)
}

/// Builds the full home-screen frame for a raw terminal size.
pub fn render_frame(raw_height: usize, raw_width: usize, state: &HomeState) -> Vec<String> {
    // The session-name modal, when open, overlays the whole screen.
    if let Some(modal) = state.modal() {
        return session_modal_frame(raw_height, raw_width, modal);
    }

    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, right_w) = layout(width);

    // Chrome around the body: title + blank separator on top, input + footer
    // at the bottom. The rest is the two-pane body.
    let pane_rows = height.saturating_sub(4).max(1);
    let left = left_pane(state.list(), left_w, pane_rows);
    let right = right_pane(state.log(), right_w, pane_rows);

    let mut lines = Vec::with_capacity(height);
    lines.push(title_bar(width, state.list()));
    lines.push(String::new());
    for row in 0..pane_rows {
        let left_cell = pad_to_width(left.get(row).cloned().unwrap_or_default(), left_w);
        let right_cell = right.get(row).cloned().unwrap_or_default();
        lines.push(format!("{left_cell}{SEP}{right_cell}"));
    }
    lines.push(input_line(state));
    lines.push(footer_line(width, state.mode()));
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
    fn clip_to_width_keeps_short_text() {
        assert_eq!(clip_to_width("main", 10), "main");
    }

    #[test]
    fn clip_to_width_truncates_with_an_ellipsis() {
        let clipped = clip_to_width("feature/long", 5);
        assert_eq!(console::measure_text_width(&clipped), 5);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn clip_to_width_with_zero_budget_is_empty() {
        assert_eq!(clip_to_width("main", 0), "");
    }

    #[test]
    fn pad_to_width_fills_short_content() {
        assert_eq!(pad_to_width("ab".to_string(), 5), "ab   ");
    }

    #[test]
    fn pad_to_width_leaves_full_content_alone() {
        assert_eq!(pad_to_width("abcde".to_string(), 5), "abcde");
    }

    #[test]
    fn layout_splits_a_standard_width() {
        let (left, right) = layout(80);
        assert_eq!(left, 26);
        assert_eq!(right, 80 - 26 - SEP_WIDTH);
    }

    #[test]
    fn layout_does_not_overrun_a_narrow_terminal() {
        // Far below LEFT_MIN: the left pane shrinks to fit and the right is 0.
        let (left, right) = layout(4);
        assert!(left <= 4);
        assert_eq!(right, 0);
    }

    #[test]
    fn title_bar_singular_and_plural() {
        let one = title_bar(
            80,
            &list_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]),
        );
        assert!(one.contains("usagi"));
        assert!(one.contains("1 worktree"));
        assert!(!one.contains("1 worktrees"));

        let two = title_bar(
            80,
            &list_with(vec![
                worktree(Some("main"), true, BranchStatus::Pushed),
                worktree(Some("x"), false, BranchStatus::Local),
            ]),
        );
        assert!(two.contains("2 worktrees"));
    }

    #[test]
    fn status_label_colours_each_variant() {
        assert!(status_label(BranchStatus::Local).contains("local"));
        assert!(status_label(BranchStatus::Pushed).contains("pushed"));
        assert!(status_label(BranchStatus::Merged).contains("merged"));
    }

    #[test]
    fn worktree_row_marks_selected_primary_and_detached() {
        let selected = worktree_row(
            &worktree(Some("main"), true, BranchStatus::Pushed),
            10,
            true,
            false,
        );
        assert!(selected.contains('>'));
        assert!(selected.contains('●'));
        assert!(selected.contains("main"));

        let other = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            false,
        );
        assert!(!other.contains('>'));
        assert!(other.contains("feature"));

        let detached = worktree_row(
            &worktree(None, false, BranchStatus::Local),
            10,
            false,
            false,
        );
        assert!(detached.contains("(detached)"));
    }

    #[test]
    fn worktree_row_marks_the_active_worktree() {
        let active = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            true,
        );
        assert!(active.contains('*'));

        let inactive = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            false,
        );
        assert!(!inactive.contains('*'));
    }

    #[test]
    fn worktree_row_truncates_a_long_branch() {
        let row = worktree_row(
            &worktree(
                Some("feature/a-very-long-branch-name"),
                false,
                BranchStatus::Local,
            ),
            8,
            false,
            false,
        );
        assert!(row.contains('…'));
    }

    #[test]
    fn left_pane_renders_an_empty_message() {
        let lines = left_pane(&list_with(Vec::new()), 80, 5);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No worktrees recorded"));
    }

    #[test]
    fn left_pane_renders_one_row_per_worktree() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let lines = left_pane(&list, 30, 5);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("main"));
        assert!(lines[1].contains("feature"));
    }

    #[test]
    fn left_pane_is_trimmed_to_available_rows() {
        let list = list_with(vec![
            worktree(Some("a"), false, BranchStatus::Local),
            worktree(Some("b"), false, BranchStatus::Local),
            worktree(Some("c"), false, BranchStatus::Local),
        ]);
        let lines = left_pane(&list, 30, 2);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn log_line_colours_each_kind_and_prompts_commands() {
        assert!(log_line(&LogLine::command("man"), 40).contains("❯ man"));
        assert_eq!(log_line(&LogLine::output("plain"), 40), "plain");
        assert!(log_line(&LogLine::error("boom"), 40).contains("boom"));
        assert!(log_line(&LogLine::notice("note"), 40).contains("note"));
    }

    #[test]
    fn right_pane_shows_only_the_tail_that_fits() {
        let log: Vec<LogLine> = (0..5)
            .map(|i| LogLine::output(format!("line {i}")))
            .collect();
        let lines = right_pane(&log, 40, 3);
        assert_eq!(lines.len(), 3);
        // The oldest lines scroll off; the newest remain.
        assert!(lines[0].contains("line 2"));
        assert!(lines[2].contains("line 4"));
    }

    #[test]
    fn right_pane_keeps_everything_when_it_fits() {
        let log = vec![LogLine::output("only")];
        let lines = right_pane(&log, 40, 5);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn input_line_renders_prompt_in_command_mode() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.enter_command_mode();
        state.push_char('m');
        let line = input_line(&state);
        assert!(line.contains('m'));
        assert!(line.contains(CARET));
    }

    #[test]
    fn input_line_renders_hint_in_sidebar_mode() {
        let state = HomeState::new("usagi", Vec::new(), None);
        assert!(input_line(&state).contains("command"));
    }

    #[test]
    fn footer_line_differs_by_mode() {
        assert!(footer_line(80, Mode::Sidebar).contains("move"));
        assert!(footer_line(80, Mode::Command).contains("complete"));
    }

    #[test]
    fn render_frame_combines_all_sections_at_full_height() {
        let state = HomeState::new(
            "usagi",
            vec![worktree(Some("main"), true, BranchStatus::Pushed)],
            None,
        );
        let frame = render_frame(24, 80, &state);
        assert_eq!(frame.len(), 24);
        assert!(frame[0].contains("usagi"));
        // The body rows carry the divider.
        assert!(frame[2].contains('│'));
        assert!(frame.last().unwrap().contains("move"));
        let joined = frame.join("\n");
        assert!(joined.contains("main"));
        assert!(joined.contains("man"));
    }

    #[test]
    fn render_frame_overlays_the_session_modal_when_open() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.open_session_modal();
        state.modal_push_char('w');
        state.modal_push_char('i');
        state.modal_push_char('p');

        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        // The modal title, prompt, typed name, and hints are shown.
        assert!(joined.contains("New session"));
        assert!(joined.contains("Enter a name"));
        assert!(joined.contains("wip"));
        assert!(joined.contains("Enter: create"));
        // The three-pane chrome (its mode footer) is not drawn underneath.
        assert!(!joined.contains("move"));
    }

    #[test]
    fn session_modal_frame_shows_a_validation_error() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.open_session_modal();
        // An empty submit sets an inline error.
        assert!(state.submit_modal().is_none());
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("must not be empty"));
    }

    #[test]
    fn session_modal_frame_clips_a_very_long_name() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.open_session_modal();
        for _ in 0..80 {
            state.modal_push_char('x');
        }
        let frame = session_modal_frame(24, 80, state.modal().unwrap());
        // The clipped name carries the ellipsis and every box row is equal width.
        let joined = frame.join("\n");
        assert!(joined.contains('…'));
        let widths: Vec<usize> = frame
            .iter()
            .filter(|l| l.contains('│'))
            .map(|l| console::measure_text_width(l))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn render_frame_survives_a_short_terminal() {
        let state = HomeState::new("usagi", Vec::new(), None);
        let frame = render_frame(3, 80, &state);
        // Title first, footer last, at least one body row in between.
        assert!(frame[0].contains("usagi"));
        assert!(frame.last().unwrap().contains("move"));
        assert!(frame.len() >= 4);
    }
}
