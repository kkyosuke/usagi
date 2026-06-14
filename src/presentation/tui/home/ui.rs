//! Rendering for the home (workspace) screen's three-pane layout.
//!
//! Top to bottom: a title bar, then a body split into the worktree list (left)
//! and the command log — or, while the `terminal` command runs, a live embedded
//! terminal — (right), then the command input line and a mode-aware footer. All
//! functions take plain data and return styled lines, so the layout is rendered
//! without any terminal IO.

use std::collections::HashSet;
use std::path::PathBuf;

use console::style;

use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::widgets;

use super::command::{CommandHint, Hint};
use super::state::{
    HomeState, LineKind, LogLine, Mode, RemoveModal, RightPane, SessionModal, WorktreeList,
    ROOT_NAME,
};
use super::terminal_view::TerminalView;

/// Shown below the root row when the workspace has no recorded worktrees.
const EMPTY_MESSAGE: &str = "No worktrees recorded yet. Run usagi to sync.";

/// The status label shown for the root row (which has no git status).
const ROOT_STATUS: &str = "—";

/// Shown for a worktree whose HEAD is detached (no branch).
const DETACHED: &str = "(detached)";

/// Visible columns a worktree row spends on everything but the branch name
/// (cursor, active marker, primary marker, waiting marker, separators, and the
/// fixed-width status label).
const ROW_OVERHEAD: usize = 16;

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

/// The centred title bar: workspace name and session count. The count covers
/// every row in the left pane — the root row plus each worktree — so it matches
/// what the user sees, rather than the bare worktree count.
fn title_bar(width: usize, list: &WorktreeList) -> String {
    let count = list.session_count();
    let label = format!(
        "{} · {count} session{}",
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
/// for the active worktree, a `●` marker for the primary worktree, a `◆` marker
/// when its background session is waiting for input, the (truncated, padded)
/// branch name, and status.
fn worktree_row(
    worktree: &WorktreeState,
    branch_width: usize,
    selected: bool,
    active: bool,
    waiting: bool,
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

    // A bright marker for a session whose agent has rung the bell to ask for
    // input, so it stands out while the user is elsewhere in the screen.
    let waiting_marker = if waiting {
        style("◆").yellow().bold().to_string()
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
    format!("{marker} {active_marker} {primary} {waiting_marker} {branch}  {status}")
}

/// Builds the root row: the workspace itself, belonging to no session. It uses
/// the same cursor/active markers as a worktree row, a `⌂` icon in the primary
/// column, a blank waiting column (the root never waits for input), the
/// [`ROOT_NAME`] label, and a placeholder status.
fn root_row(branch_width: usize, selected: bool, active: bool) -> String {
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

    let icon = style("⌂").magenta().to_string();

    // The root never waits for input; a blank waiting column keeps the row
    // column-aligned with `worktree_row`.
    let waiting_marker = " ";

    let label = format!("{:<branch_width$}", clip_to_width(ROOT_NAME, branch_width));
    let label = if active || selected {
        style(label).cyan().bold().to_string()
    } else {
        style(label).cyan().to_string()
    };

    let status = style(format!("{ROOT_STATUS:<6}")).dim().to_string();
    format!("{marker} {active_marker} {icon} {waiting_marker} {label}  {status}")
}

/// Builds the left pane: the root row, then one row per worktree (or the empty
/// message when none are recorded), trimmed to the available `rows`. `waiting`
/// holds the worktree paths whose background session is awaiting input, marked
/// with `◆`.
fn left_pane(
    list: &WorktreeList,
    waiting: &HashSet<PathBuf>,
    left_w: usize,
    rows: usize,
) -> Vec<String> {
    let branch_width = left_w.saturating_sub(ROW_OVERHEAD);
    let mut lines = vec![root_row(
        branch_width,
        list.root_selected(),
        list.root_active(),
    )];
    if list.is_empty() {
        lines.push(clip_to_width(EMPTY_MESSAGE, left_w));
    } else {
        for (i, w) in list.worktrees().iter().enumerate() {
            // Row 0 is the root row, so worktree `i` sits at selectable row i + 1.
            let row = i + 1;
            lines.push(worktree_row(
                w,
                branch_width,
                row == list.selected_index(),
                row == list.active_index(),
                waiting.contains(&w.path),
            ));
        }
    }
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

/// Shown in the right pane between starting the `terminal` command and its
/// first screen snapshot arriving.
const TERMINAL_STARTING: &str = "Starting terminal…";

/// Builds the right pane from an embedded terminal snapshot: each grid row,
/// clipped to the pane width, up to `rows` rows.
fn terminal_pane(view: &TerminalView, right_w: usize, rows: usize) -> Vec<String> {
    view.rows()
        .iter()
        .take(rows)
        .map(|row| clip_to_width(row, right_w))
        .collect()
}

/// Chooses the right pane's contents: the command log, or — while the
/// `terminal` command runs — the live terminal snapshot (or a starting hint
/// until the first one arrives).
fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    match state.right_pane() {
        RightPane::Log => right_pane(state.log(), right_w, rows),
        RightPane::Terminal => match state.terminal_view() {
            Some(view) => terminal_pane(view, right_w, rows),
            None => vec![style(clip_to_width(TERMINAL_STARTING, right_w))
                .dim()
                .to_string()],
        },
    }
}

/// Where the embedded terminal lives on screen: the size of the right pane and
/// the screen coordinates of its top-left cell. The PTY is sized to `rows`×
/// `cols`, and the real cursor is placed relative to (`origin_col`,
/// `origin_row`) so it tracks the shell's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGeometry {
    pub rows: u16,
    pub cols: u16,
    pub origin_col: u16,
    pub origin_row: u16,
}

/// Computes the [`TerminalGeometry`] for a raw terminal size, matching the
/// layout [`render_frame`] draws (title + blank above the body, the left pane
/// and divider to its left). `rows` and `cols` are at least 1.
pub fn terminal_geometry(raw_height: usize, raw_width: usize) -> TerminalGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, right_w) = layout(width);
    let pane_rows = height.saturating_sub(4).max(1);
    TerminalGeometry {
        rows: pane_rows.max(1) as u16,
        cols: right_w.max(1) as u16,
        origin_col: (left_w + SEP_WIDTH) as u16,
        // The body starts below the title bar and its blank separator.
        origin_row: 2,
    }
}

/// Most command-hint rows drawn above the input at once. Beyond this a
/// "… and N more" line stands in for the rest, so the hints never crowd out the
/// body on a normal terminal.
const HINT_MAX: usize = 6;

/// Display width of the command-name column in the hints.
const HINT_NAME_COL: usize = 12;

/// Columns before the name column in a hint row: `"  "` indent + the marker
/// cell + a space.
const HINT_INDENT: usize = 4;

/// Renders one command-hint row: a `›` marker for the highlighted best match,
/// the command name with its already-typed prefix emphasised, and the dimmed
/// description, clipped to `width`.
fn command_hint_row(hint: &CommandHint, typed_len: usize, selected: bool, width: usize) -> String {
    let marker = if selected {
        style("›").red().bold().to_string()
    } else {
        " ".to_string()
    };
    // Bold the part of the name the user has already typed, so it reads as a
    // continuation of what is in the input line.
    let split = typed_len.min(hint.name.len());
    let (head, tail) = hint.name.split_at(split);
    let name = format!("{}{}", style(head).cyan().bold(), style(tail).cyan());
    let name_col = pad_to_width(name, HINT_NAME_COL);
    let desc_budget = width.saturating_sub(HINT_INDENT + HINT_NAME_COL);
    let desc = style(clip_to_width(hint.description, desc_budget)).dim();
    format!("  {marker} {name_col}{desc}")
}

/// The advisory hint lines drawn just above the command input: the matching
/// commands while the command word is typed, or the usage and examples once a
/// known command is given arguments. Empty outside command mode and while the
/// terminal pane is live.
fn hint_lines(state: &HomeState, width: usize) -> Vec<String> {
    if state.right_pane() == RightPane::Terminal || state.mode() != Mode::Command {
        return Vec::new();
    }
    match state.hint() {
        Hint::Commands(hints) => {
            let typed = state.input().trim_start();
            // Only point a marker at a best match once something is typed; a
            // bare ":" shows the whole menu with nothing pre-selected.
            let highlight = !typed.is_empty();
            let header = if highlight { "matches" } else { "commands" };
            let mut lines = vec![style(format!("  {header}")).dim().to_string()];
            for (i, hint) in hints.iter().take(HINT_MAX).enumerate() {
                lines.push(command_hint_row(
                    hint,
                    typed.len(),
                    highlight && i == 0,
                    width,
                ));
            }
            if hints.len() > HINT_MAX {
                let rest = hints.len() - HINT_MAX;
                lines.push(style(format!("    … and {rest} more")).dim().to_string());
            }
            lines
        }
        Hint::Usage { usage, examples } => {
            let mut lines = vec![format!(
                "  {} {}",
                style("usage").dim(),
                style(usage).cyan()
            )];
            for example in examples.iter().take(HINT_MAX) {
                let text = clip_to_width(example, width.saturating_sub(HINT_INDENT + 6));
                lines.push(format!("    {} {}", style("e.g.").dim(), style(text).dim()));
            }
            lines
        }
        Hint::None => Vec::new(),
    }
}

/// The command input line: a status line while the terminal runs, an editable
/// prompt in command mode, or a hint in sidebar mode.
fn input_line(state: &HomeState) -> String {
    if state.right_pane() == RightPane::Terminal {
        return style(" ● live terminal".to_string()).green().to_string();
    }
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

/// The footer help line, aware of the terminal pane and the current mode.
fn footer_line(width: usize, state: &HomeState) -> String {
    let help = if state.right_pane() == RightPane::Terminal {
        "Embedded terminal — Ctrl-O: detach / Ctrl-O n,p: switch session"
    } else {
        match state.mode() {
            Mode::Sidebar => "↑↓: move / Enter: activate / :: command / Esc: back",
            Mode::Command => "Tab: complete / ↑↓: history / Enter: run / Esc: cancel",
        }
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

/// Most session rows the removal modal shows at once; a longer list scrolls to
/// keep the cursor in view, with a count of the hidden rows above and below.
const REMOVE_MODAL_VISIBLE: usize = 8;

/// Builds one removal-modal row: a `>` cursor for the highlighted entry, a
/// `[x]` / `[ ]` checkbox for its selection, and the (clipped) session name.
/// The cursored row is emphasised, a checked row stays bright, and the rest are
/// dimmed.
fn remove_modal_row(name: &str, cursor: bool, selected: bool, inner: usize) -> String {
    let marker = if cursor { ">" } else { " " };
    let check = if selected { "[x]" } else { "[ ]" };
    let text = clip_to_width(name, inner.saturating_sub(6));
    let line = format!("{marker} {check} {text}");
    if cursor {
        style(line).cyan().bold().to_string()
    } else if selected {
        style(line).cyan().to_string()
    } else {
        style(line).dim().to_string()
    }
}

/// Builds the centred session-removal modal: a scrolling checklist of the
/// workspace's sessions, with the count selected and the key hints below.
fn remove_modal_frame(raw_height: usize, raw_width: usize, modal: &RemoveModal) -> Vec<String> {
    const INNER: usize = 40;

    let mut body = vec![
        style("Select sessions to remove (Space to toggle).")
            .dim()
            .to_string(),
        String::new(),
    ];

    let names = modal.names();
    if names.is_empty() {
        body.push(style("No sessions to remove.").dim().to_string());
    } else {
        // Scroll the window so the cursor is always visible on a long list.
        let total = names.len();
        let start = if modal.cursor() < REMOVE_MODAL_VISIBLE {
            0
        } else {
            modal.cursor() + 1 - REMOVE_MODAL_VISIBLE
        };
        let end = (start + REMOVE_MODAL_VISIBLE).min(total);
        if start > 0 {
            body.push(style(format!("  ↑ {start} more")).dim().to_string());
        }
        for (offset, name) in names[start..end].iter().enumerate() {
            let i = start + offset;
            body.push(remove_modal_row(
                name,
                i == modal.cursor(),
                modal.is_selected(i),
                INNER,
            ));
        }
        if end < total {
            body.push(style(format!("  ↓ {} more", total - end)).dim().to_string());
        }
        body.push(String::new());
        body.push(
            style(format!("{} selected", modal.selected_count()))
                .dim()
                .to_string(),
        );
    }

    body.push(String::new());
    body.push(
        style("Space: toggle   Enter: remove   Esc: cancel")
            .dim()
            .to_string(),
    );
    widgets::render_modal(raw_height, raw_width, "Remove sessions", INNER, &body)
}

/// Builds the full home-screen frame for a raw terminal size.
pub fn render_frame(raw_height: usize, raw_width: usize, state: &HomeState) -> Vec<String> {
    // The session-removal modal, when open, overlays the whole screen.
    if let Some(modal) = state.remove_modal() {
        return remove_modal_frame(raw_height, raw_width, modal);
    }

    // The session-name modal, when open, overlays the whole screen.
    if let Some(modal) = state.modal() {
        return session_modal_frame(raw_height, raw_width, modal);
    }

    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, right_w) = layout(width);

    // Chrome around the body: title + blank separator on top, input + footer at
    // the bottom. Everything between is the two-pane body, whose height never
    // depends on the mode — so entering or leaving command mode never resizes
    // the panes. The command-mode hints float over the body's bottom rows as an
    // overlay anchored to the input, appearing and vanishing without shifting
    // anything beneath them.
    let body_rows = height.saturating_sub(4).max(1);
    let left = left_pane(state.list(), state.waiting_paths(), left_w, body_rows);
    let right = right_pane_contents(state, right_w, body_rows);

    let mut lines = Vec::with_capacity(height);
    lines.push(title_bar(width, state.list()));
    lines.push(String::new());
    let body_start = lines.len();
    for row in 0..body_rows {
        let left_cell = pad_to_width(left.get(row).cloned().unwrap_or_default(), left_w);
        let right_cell = right.get(row).cloned().unwrap_or_default();
        lines.push(format!("{left_cell}{SEP}{right_cell}"));
    }

    // Overlay the hints onto the bottom of the body, always leaving at least one
    // body row uncovered. Padding each hint to the full width clears the body
    // text it sits on top of.
    let hints = hint_lines(state, width);
    let hint_rows = hints.len().min(body_rows.saturating_sub(1));
    let overlay_start = body_start + body_rows - hint_rows;
    for (i, hint) in hints.into_iter().take(hint_rows).enumerate() {
        lines[overlay_start + i] = pad_to_width(hint, width);
    }

    lines.push(input_line(state));
    lines.push(footer_line(width, state));
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
        // No worktrees: only the root row counts, so the title reads "1 session".
        let one = title_bar(80, &list_with(vec![]));
        assert!(one.contains("usagi"));
        assert!(one.contains("1 session"));
        assert!(!one.contains("1 sessions"));

        // Two worktrees plus the root row: three sessions.
        let three = title_bar(
            80,
            &list_with(vec![
                worktree(Some("main"), true, BranchStatus::Pushed),
                worktree(Some("x"), false, BranchStatus::Local),
            ]),
        );
        assert!(three.contains("3 sessions"));
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
            false,
        );
        assert!(!other.contains('>'));
        assert!(other.contains("feature"));

        let detached = worktree_row(
            &worktree(None, false, BranchStatus::Local),
            10,
            false,
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
            false,
        );
        assert!(active.contains('*'));

        let inactive = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            false,
            false,
        );
        assert!(!inactive.contains('*'));
    }

    #[test]
    fn worktree_row_marks_a_session_waiting_for_input() {
        let waiting = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            false,
            true,
        );
        assert!(waiting.contains('◆'));

        let quiet = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            false,
            false,
            false,
        );
        assert!(!quiet.contains('◆'));
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
            false,
        );
        assert!(row.contains('…'));
    }

    #[test]
    fn root_row_marks_selected_and_active() {
        let selected = root_row(10, true, false);
        assert!(selected.contains('>'));
        assert!(selected.contains('⌂'));
        assert!(selected.contains(ROOT_NAME));

        let active = root_row(10, false, true);
        assert!(active.contains('*'));

        let idle = root_row(10, false, false);
        assert!(!idle.contains('>'));
        assert!(!idle.contains('*'));
        assert!(idle.contains(ROOT_NAME));
    }

    #[test]
    fn left_pane_renders_the_root_row_then_the_empty_message() {
        let lines = left_pane(&list_with(Vec::new()), &HashSet::new(), 80, 5);
        // The root row is always present, with the empty hint below it.
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[1].contains("No worktrees recorded"));
    }

    #[test]
    fn left_pane_renders_the_root_row_then_one_row_per_worktree() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let lines = left_pane(&list, &HashSet::new(), 30, 5);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[1].contains("main"));
        assert!(lines[2].contains("feature"));
    }

    #[test]
    fn left_pane_marks_worktrees_whose_session_is_waiting() {
        let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
        // The test worktree's path is "/repo/wt"; flagging it adds the marker.
        // The root row is line 0, so the worktree is line 1.
        let mut waiting = HashSet::new();
        waiting.insert(PathBuf::from("/repo/wt"));
        let marked = left_pane(&list, &waiting, 30, 5);
        assert!(marked[1].contains('◆'));
        // With nothing waiting the marker is absent.
        let unmarked = left_pane(&list, &HashSet::new(), 30, 5);
        assert!(!unmarked[1].contains('◆'));
    }

    #[test]
    fn left_pane_is_trimmed_to_available_rows() {
        let list = list_with(vec![
            worktree(Some("a"), false, BranchStatus::Local),
            worktree(Some("b"), false, BranchStatus::Local),
            worktree(Some("c"), false, BranchStatus::Local),
        ]);
        // Two rows: the root row and the first worktree.
        let lines = left_pane(&list, &HashSet::new(), 30, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[1].contains('a'));
    }

    #[test]
    fn left_pane_marks_the_active_worktree_below_the_root_row() {
        let mut list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        list.activate_by_name("feature");
        let lines = left_pane(&list, &HashSet::new(), 30, 5);
        // The root row is no longer active; the "feature" row carries `*`.
        assert!(!lines[0].contains('*'));
        assert!(lines[2].contains('*'));
        assert!(lines[2].contains("feature"));
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
        let mut state = HomeState::new("usagi", Vec::new(), None);
        assert!(footer_line(80, &state).contains("move"));
        state.enter_command_mode();
        assert!(footer_line(80, &state).contains("complete"));
    }

    #[test]
    fn footer_and_input_switch_to_a_terminal_hint_when_it_runs() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.show_terminal();
        // Both the footer and the status line advertise the embedded terminal.
        assert!(footer_line(80, &state).contains("detach"));
        assert!(input_line(&state).contains("live terminal"));
    }

    #[test]
    fn terminal_pane_clips_rows_to_the_pane_width() {
        let view = TerminalView::from_rows(
            vec!["a long command line".to_string(), "$ ".to_string()],
            Some((1, 2)),
        );
        let lines = terminal_pane(&view, 8, 5);
        assert_eq!(lines.len(), 2);
        assert!(console::measure_text_width(&lines[0]) <= 8);
        assert!(lines[0].ends_with('…'));
        assert!(lines[1].starts_with("$ "));
    }

    #[test]
    fn right_pane_contents_follows_the_pane_mode() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        // Log mode shows the seeded hint line.
        let log = right_pane_contents(&state, 40, 5);
        assert!(log.iter().any(|l| l.contains("man")));

        // Terminal mode with no snapshot yet shows the starting hint.
        state.show_terminal();
        let starting = right_pane_contents(&state, 40, 5);
        assert!(starting[0].contains("Starting terminal"));

        // Once a snapshot arrives, its rows are shown instead.
        state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
        let running = right_pane_contents(&state, 40, 5);
        assert!(running[0].contains("$ echo hi"));
    }

    #[test]
    fn terminal_geometry_matches_the_rendered_layout() {
        let geo = terminal_geometry(24, 80);
        // The left pane (26) plus the divider (3) is where the terminal starts.
        let (left, _) = layout(80);
        assert_eq!(geo.origin_col as usize, left + SEP_WIDTH);
        // The body sits below the title bar and its blank separator.
        assert_eq!(geo.origin_row, 2);
        // The pane is the body height (24 - 4 chrome rows) and the right width.
        assert_eq!(geo.rows, 20);
        assert_eq!(geo.cols as usize, 80 - left - SEP_WIDTH);
    }

    #[test]
    fn terminal_geometry_stays_positive_in_a_tiny_terminal() {
        // Far too small for a real layout: rows and cols are clamped to 1.
        let geo = terminal_geometry(1, 1);
        assert!(geo.rows >= 1);
        assert!(geo.cols >= 1);
    }

    #[test]
    fn render_frame_draws_the_terminal_in_the_right_pane() {
        let mut state = HomeState::new(
            "usagi",
            vec![worktree(Some("main"), true, BranchStatus::Pushed)],
            None,
        );
        state.show_terminal();
        state.set_terminal_view(TerminalView::from_rows(
            vec!["$ cargo test".to_string()],
            None,
        ));
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        // The sidebar is still drawn alongside the live terminal output.
        assert!(joined.contains("main"));
        assert!(joined.contains("$ cargo test"));
        assert!(joined.contains("detach"));
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

    /// A state seeded with `names` as recorded sessions, for the removal modal.
    fn state_with_sessions(names: &[&str]) -> HomeState {
        use crate::domain::workspace_state::SessionRecord;
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let sessions = names
            .iter()
            .map(|n| SessionRecord {
                name: n.to_string(),
                root: PathBuf::from(format!("/ws/{n}")),
                worktrees: Vec::new(),
                created_at: Utc::now(),
            })
            .collect();
        state.restore_sessions(sessions);
        state
    }

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
        assert!(checked.contains("beta"));

        let idle =
            console::strip_ansi_codes(&remove_modal_row("gamma", false, false, 40)).into_owned();
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
        state.remove_modal_toggle(); // check "alpha"
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("Remove sessions"));
        assert!(joined.contains("Select sessions to remove"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("beta"));
        // The checked session shows a ticked box, and the count is reported.
        assert!(joined.contains("[x]"));
        assert!(joined.contains("1 selected"));
        assert!(joined.contains("Enter: remove"));
        // The three-pane chrome (its sidebar mode footer) is not drawn underneath.
        assert!(!joined.contains("activate"));
    }

    #[test]
    fn render_frame_removal_modal_reports_when_there_are_no_sessions() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.open_remove_modal(false);
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("No sessions to remove"));
        // The selected-count line is omitted when the list is empty.
        assert!(!joined.contains("selected"));
    }

    #[test]
    fn remove_modal_frame_scrolls_to_keep_the_cursor_visible() {
        // More sessions than fit: scrolling the cursor down past the first window
        // shows both the "more above" and "more below" indicators.
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut state = state_with_sessions(&refs);
        state.open_remove_modal(false);
        for _ in 0..9 {
            state.remove_modal_move_down(); // cursor on "s09"
        }
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains('↑'));
        assert!(joined.contains('↓'));
        assert!(joined.contains("more"));
        // The cursor row stays within the visible window.
        assert!(joined.contains("s09"));
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

    /// A `HomeState` in command mode with `typed` already entered.
    fn typing(typed: &str) -> HomeState {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.enter_command_mode();
        for c in typed.chars() {
            state.push_char(c);
        }
        state
    }

    fn stripped(lines: &[String]) -> String {
        console::strip_ansi_codes(&lines.join("\n")).into_owned()
    }

    #[test]
    fn command_hint_row_emphasises_the_typed_prefix_and_marks_the_selection() {
        let hint = CommandHint {
            name: "session",
            description: "Create, list, or switch sessions",
        };
        let selected = command_hint_row(&hint, 3, true, 80);
        let plain = console::strip_ansi_codes(&selected).into_owned();
        assert!(plain.contains('›'));
        assert!(plain.contains("session"));
        assert!(plain.contains("Create, list"));

        // Without selection there is no marker.
        let plain = console::strip_ansi_codes(&command_hint_row(&hint, 0, false, 80)).into_owned();
        assert!(!plain.contains('›'));
    }

    #[test]
    fn command_hint_row_clips_a_long_description_to_width() {
        let hint = CommandHint {
            name: "session",
            description: "A very long description that should be cut down to fit the pane width",
        };
        let row = command_hint_row(&hint, 0, false, 30);
        assert!(console::measure_text_width(&row) <= 30);
        assert!(console::strip_ansi_codes(&row).contains('…'));
    }

    #[test]
    fn hint_lines_are_empty_outside_command_mode() {
        let state = HomeState::new("usagi", Vec::new(), None);
        assert!(hint_lines(&state, 80).is_empty());
    }

    #[test]
    fn hint_lines_are_empty_while_the_terminal_runs() {
        let mut state = typing("session");
        state.show_terminal();
        assert!(hint_lines(&state, 80).is_empty());
    }

    #[test]
    fn hint_lines_list_every_command_for_a_bare_prompt() {
        let state = typing("");
        let lines = hint_lines(&state, 80);
        let joined = stripped(&lines);
        // The header reads "commands" and nothing is pre-selected.
        assert!(joined.contains("commands"));
        assert!(!joined.contains('›'));
        // More commands than fit, so a summary line stands in for the rest.
        assert!(joined.contains("more"));
        assert!(joined.contains("session"));
    }

    #[test]
    fn hint_lines_highlight_the_best_match_while_typing() {
        let state = typing("s");
        let joined = stripped(&hint_lines(&state, 80));
        // "s" narrows to "session", shown under a "matches" header with a marker.
        assert!(joined.contains("matches"));
        assert!(joined.contains('›'));
        assert!(joined.contains("session"));
        assert!(!joined.contains("more"));
    }

    #[test]
    fn hint_lines_show_usage_and_examples_for_arguments() {
        let state = typing("session ");
        let joined = stripped(&hint_lines(&state, 80));
        assert!(joined.contains("usage"));
        assert!(joined.contains("session [new"));
        assert!(joined.contains("e.g."));
        assert!(joined.contains("session new"));
    }

    #[test]
    fn hint_lines_show_usage_without_examples_when_a_command_has_none() {
        // `terminal` takes no arguments and lists no examples.
        let state = typing("terminal ");
        let joined = stripped(&hint_lines(&state, 80));
        assert!(joined.contains("usage"));
        assert!(joined.contains("terminal"));
        assert!(!joined.contains("e.g."));
    }

    #[test]
    fn hint_lines_are_empty_for_an_unknown_command() {
        assert!(hint_lines(&typing("frobnicate "), 80).is_empty());
        assert!(hint_lines(&typing("zzz"), 80).is_empty());
    }

    #[test]
    fn render_frame_shows_command_hints_above_the_input_and_keeps_its_height() {
        let state = typing("s");
        let frame = render_frame(24, 80, &state);
        // The hints overlay the body's bottom rows; the overall height is unchanged.
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("matches"));
        assert!(joined.contains("session"));
        // The hints sit directly above the input prompt and footer.
        let prompt_row = frame.len() - 2;
        assert!(frame[prompt_row].contains('❯'));
        let above = console::strip_ansi_codes(&frame[prompt_row - 1]).into_owned();
        assert!(above.contains("session"));
    }

    #[test]
    fn render_frame_keeps_the_body_in_place_when_command_mode_opens() {
        // Entering command mode must not shift the body: the title, blank
        // separator, and every body row stay at the same screen position whether
        // or not the command hints are showing. Only the bottom rows the hints
        // overlay differ, and the input / footer stay last.
        let sidebar = state_with_sessions(&["alpha", "beta"]);
        let command = {
            let mut s = state_with_sessions(&["alpha", "beta"]);
            s.enter_command_mode();
            s.push_char('s');
            s
        };
        let before = render_frame(24, 80, &sidebar);
        let after = render_frame(24, 80, &command);
        assert_eq!(before.len(), after.len());

        // The hints occupy the rows just above the input line; everything above
        // that overlay region is byte-for-byte identical, so nothing jumps.
        let hint_rows = hint_lines(&command, 80).len();
        let input_row = after.len() - 2;
        let overlay_start = input_row - hint_rows;
        for row in 0..overlay_start {
            assert_eq!(before[row], after[row], "body row {row} shifted");
        }
        // The hints really did land in the overlay region, directly above input.
        assert!(stripped(&after[overlay_start..input_row]).contains("session"));
    }

    #[test]
    fn render_frame_keeps_a_body_row_when_hints_would_fill_a_short_screen() {
        // A short screen in command mode: hints must not crowd out the body
        // entirely, and the height is still respected.
        let state = typing("");
        let frame = render_frame(8, 80, &state);
        assert_eq!(frame.len(), 8);
        // The body divider row survives between the title and the hints/input.
        assert!(frame[2].contains('│'));
    }
}
