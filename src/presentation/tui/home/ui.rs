//! Rendering for the home (workspace) screen's mode-aware layout.
//!
//! Top to bottom: a title bar, the engagement-ladder mode indicator, a body
//! split into the worktree list (left) and a mode-dependent right pane, the
//! command input, and a footer. The right pane is blank in 統括 (Overview); a
//! detail card for the highlighted session in 切替 (Switch); the session's action
//! surface (a menu or a prompt) in 在席 (Focus); and the live embedded terminal in
//! 没入 (Attached). In Overview the input is a bordered box and the command
//! results render as a band below it. All functions take plain data and return
//! styled lines, so the layout is rendered without any terminal IO.

use std::collections::HashSet;
use std::path::PathBuf;

use console::style;

use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::widgets;

use crate::domain::settings::SessionActionUi;

use super::command::{CommandHint, CommandInfo, Hint};
use super::state::{
    HomeState, LineKind, LogLine, Mode, RemoveModal, TextModal, WorktreeList, ROOT_NAME,
};
use super::terminal_view::TerminalView;

/// Shown below the root row when the workspace has no recorded worktrees.
const EMPTY_MESSAGE: &str = "no sessions";

/// The detail shown on the root row's second line (it has no git status).
const ROOT_DETAIL: &str = "workspace root";

/// Shown for a worktree whose HEAD is detached (no branch).
const DETACHED: &str = "(detached)";

/// Columns line 1 spends before the branch name: a cursor cell and a kind-icon
/// cell (`⌂`/`●`/`○`), each followed by a space.
const NAME_PREFIX: usize = 4;

/// Right-edge field width for the git `status` label on line 1: a status icon,
/// a space, and the widest status word (`merged` / `pushed`, 6 columns).
const STATUS_COL: usize = 8;

/// Nerd Font (git) glyphs paired with each branch lifecycle status, for an
/// at-a-glance read of the right-edge status field. They need a patched "Nerd
/// Font" terminal font to render; without one the terminal shows a fallback box,
/// but the colour-coded word beside the icon still carries the meaning.
const LOCAL_ICON: char = '\u{e725}'; // nf-dev-git_branch — lives only locally
const PUSHED_ICON: char = '\u{f0ee}'; // nf-fa-cloud_upload — pushed to the remote
const SYNCED_ICON: char = '\u{f00c}'; // nf-fa-check — up to date, nothing un-merged

/// Width of the spacing cell between the branch name and the right-edge status
/// field on line 1. It once held the `*` active marker; the active session is now
/// marked by the green `▎` bar in the far-left gutter, so this is blank spacing
/// that keeps the status field aligned.
const ACTIVE_COL: usize = 2;

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
/// every row in the left pane — the root row plus each session (one row per
/// session, not per repository) — so it matches what the user sees.
fn title_bar(width: usize, list: &WorktreeList) -> String {
    let count = list.session_count();
    let label = format!(
        "{} · {count} session{}",
        list.workspace_name(),
        if count == 1 { "" } else { "s" }
    );
    widgets::title_line(width, &label)
}

/// The engagement-ladder indicator drawn just under the title bar: the four
/// modes in order with the current one highlighted (cyan-bold) and the rest
/// dimmed, so the screen always shows which step the keys act on. Centred for
/// the terminal width.
fn mode_ladder(width: usize, current: Mode) -> String {
    const STEPS: [(Mode, &str); 4] = [
        (Mode::Overview, "Overview"),
        (Mode::Switch, "Switch"),
        (Mode::Focus, "Focus"),
        (Mode::Attached, "Attached"),
    ];
    let steps: Vec<String> = STEPS
        .iter()
        .map(|(mode, label)| {
            if *mode == current {
                style(*label).cyan().bold().to_string()
            } else {
                style(*label).dim().to_string()
            }
        })
        .collect();
    let ladder = steps.join(&style(" › ").dim().to_string());
    let pad = widgets::centered_padding(width, console::measure_text_width(&ladder));
    format!("{}{ladder}", " ".repeat(pad))
}

/// The Nerd Font git glyph for a branch lifecycle status.
fn status_icon(status: BranchStatus) -> char {
    match status {
        BranchStatus::Local => LOCAL_ICON,
        BranchStatus::Pushed => PUSHED_ICON,
        BranchStatus::UpToDate => SYNCED_ICON,
    }
}

/// The colour-coded `<icon> <word>` label for a branch's lifecycle status. The
/// icon gives an at-a-glance read; the word keeps it legible without a Nerd
/// Font and disambiguates the colour.
fn status_label(status: BranchStatus) -> String {
    let text = format!("{} {}", status_icon(status), status.as_str());
    match status {
        BranchStatus::Local => style(text).yellow().to_string(),
        BranchStatus::Pushed => style(text).green().to_string(),
        BranchStatus::UpToDate => style(text).cyan().to_string(),
    }
}

/// The line-1 right-edge status field: the colour-coded `<icon> <word>` label
/// right-aligned within [`STATUS_COL`] columns, or all blanks when there is no
/// status (the root row).
fn status_cell(status: Option<BranchStatus>) -> String {
    match status {
        None => " ".repeat(STATUS_COL),
        Some(status) => {
            let label = status_label(status);
            let pad = STATUS_COL.saturating_sub(console::measure_text_width(&label));
            format!("{}{label}", " ".repeat(pad))
        }
    }
}

/// The running/waiting state of a session's embedded agent, shown by an icon on
/// the row's first line and spelled out on its detail line.
#[derive(Clone, Copy)]
enum AgentState {
    /// No live embedded session.
    Idle,
    /// A live session whose agent is running (not awaiting input).
    Running,
    /// A live session whose agent rang the bell and awaits input.
    Waiting,
}

impl AgentState {
    /// Pick the state from the live / waiting flags. Waiting takes precedence: a
    /// session awaiting input is necessarily live.
    fn from_flags(live: bool, waiting: bool) -> Self {
        if waiting {
            AgentState::Waiting
        } else if live {
            AgentState::Running
        } else {
            AgentState::Idle
        }
    }

    /// The detail-line content: an icon together with its label — `▶ running`
    /// (green) or `◆ waiting` (yellow) — clipped to `width`, or `None` while idle
    /// (the row has no agent in use).
    fn detail(self, width: usize) -> Option<String> {
        match self {
            AgentState::Idle => None,
            AgentState::Running => Some(
                style(clip_to_width("▶ running", width))
                    .green()
                    .bold()
                    .to_string(),
            ),
            AgentState::Waiting => Some(
                style(clip_to_width("◆ waiting", width))
                    .yellow()
                    .bold()
                    .to_string(),
            ),
        }
    }
}

/// The far-left gutter cell shared by both of a row's lines. In 切替 (Switch) the
/// keyboard is on the list, so the selected row shows a red `>` cursor. The
/// **active** session — the one subsequent commands operate on — is marked by a
/// green `▎` accent bar that runs down both of its lines (this replaces the old
/// `*` marker, whose meaning and mid-row position read poorly). Outside Switch
/// there is no cursor, so the gutter only ever carries the active bar; when the
/// cursor and the active row coincide in Switch, the cursor takes the column.
fn gutter_cell(selected: bool, active: bool, in_switch: bool) -> String {
    if in_switch && selected {
        style(">").red().bold().to_string()
    } else if active {
        style("▎").green().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// The branch / root name cell: clipped and padded to `width`, cyan, and bold
/// when the row is active or under the cursor.
fn name_cell(text: &str, width: usize, emphasised: bool) -> String {
    let padded = format!("{:<width$}", clip_to_width(text, width));
    if emphasised {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    }
}

/// Builds a row's second (detail) line: the row's `gutter` cell at the far-left
/// column (so the active accent bar runs down both lines), padded to sit under
/// the branch name, then the already-styled, already-clipped `detail`.
fn detail_line(gutter: &str, detail: String) -> String {
    let indent = " ".repeat(NAME_PREFIX - 1);
    format!("{gutter}{indent}{detail}")
}

/// Builds a worktree's two lines. The far-left gutter carries a `>` cursor for
/// the selected entry in 切替 (Switch) or a green `▎` accent bar down the active
/// worktree's two lines; line 1 then has a `●`/`○` kind icon (primary or ordinary
/// worktree), the branch name, and the git `status` at the right edge. Line 2 is
/// indented under the name and, when an agent is in use, carries its icon + label
/// (`▶ running` / `◆ waiting`).
#[allow(clippy::too_many_arguments)]
fn worktree_row(
    worktree: &WorktreeState,
    name_width: usize,
    detail_width: usize,
    selected: bool,
    active: bool,
    in_switch: bool,
    live: bool,
    waiting: bool,
) -> (String, String) {
    let kind = if worktree.primary {
        style("●").magenta().to_string()
    } else {
        style("○").dim().to_string()
    };
    let branch = name_cell(
        worktree.branch.as_deref().unwrap_or(DETACHED),
        name_width,
        active || selected,
    );
    let status = status_cell(Some(worktree.status));
    let gutter = gutter_cell(selected, active, in_switch);
    // Three columns sit between the name and the right-edge status (the old
    // active-marker cell, now blank — the active bar lives in the gutter).
    let line1 = format!("{gutter} {kind} {branch}   {status}");

    // Line 2 spells out the agent state with its icon, or is blank when idle.
    // Only the active bar runs down to it — the `>` cursor stays a single point
    // on line 1, so the detail-line gutter ignores the cursor.
    let detail = AgentState::from_flags(live, waiting)
        .detail(detail_width)
        .unwrap_or_default();
    let line2 = detail_line(&gutter_cell(false, active, in_switch), detail);
    (line1, line2)
}

/// Builds the root's two lines: the workspace itself, belonging to no session.
/// The far-left gutter carries the `>` cursor (in 切替 (Switch)) or the green `▎`
/// active bar; line 1 then has a `⌂` kind icon, the [`ROOT_NAME`] label, and a
/// blank status field (the root has no git status). Line 2 carries a
/// `workspace root` detail.
fn root_row(
    name_width: usize,
    detail_width: usize,
    selected: bool,
    active: bool,
    in_switch: bool,
) -> (String, String) {
    let kind = style("⌂").magenta().to_string();
    let name = name_cell(ROOT_NAME, name_width, active || selected);
    let status = status_cell(None);
    let gutter = gutter_cell(selected, active, in_switch);
    let line1 = format!("{gutter} {kind} {name}   {status}");

    // Only the active bar reaches line 2; the cursor stays a point on line 1.
    let detail = style(clip_to_width(ROOT_DETAIL, detail_width))
        .dim()
        .to_string();
    let line2 = detail_line(&gutter_cell(false, active, in_switch), detail);
    (line1, line2)
}

/// Re-renders an already-styled row uniformly dimmed: strips its colours and
/// wraps the plain text in `dim`. Used to fade the rows the cursor is *not* on
/// in 切替 (Switch), so the highlighted session stands out without a box.
fn dim_row(line: &str) -> String {
    style(console::strip_ansi_codes(line).into_owned())
        .dim()
        .to_string()
}

/// Builds the left pane: each entry spans two lines (an identity line and a
/// detail line) — the root entry first, then one per worktree (or the empty
/// message when none are recorded), trimmed to the available `rows`. `live`
/// holds the worktree paths with a running agent (`▶ running`) and `waiting` the
/// ones whose agent awaits input (`◆ waiting`, taking precedence over running).
/// When `in_switch` is set (in 切替), the keyboard is on the list: the selected
/// row shows a `>` cursor and every other row is faded so the highlighted session
/// reads first.
fn left_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    left_w: usize,
    rows: usize,
    in_switch: bool,
) -> Vec<String> {
    // Line 1: prefix + name + the (now-blank) active-marker cell + a space + the
    // right-edge status field.
    let name_width = left_w.saturating_sub(NAME_PREFIX + ACTIVE_COL + 1 + STATUS_COL);
    // Line 2: indented under the branch name, then the detail text.
    let detail_width = left_w.saturating_sub(NAME_PREFIX);
    let (mut root_top, mut root_detail) = root_row(
        name_width,
        detail_width,
        list.root_selected(),
        list.root_active(),
        in_switch,
    );
    if in_switch && !list.root_selected() {
        root_top = dim_row(&root_top);
        root_detail = dim_row(&root_detail);
    }
    let mut lines = vec![root_top, root_detail];
    if list.is_empty() {
        // A divider under the root, then the empty message — both indented to
        // start under the `root` label (past the cursor and kind-icon cells).
        let indent = " ".repeat(NAME_PREFIX);
        let inner_w = left_w.saturating_sub(NAME_PREFIX);
        lines.push(
            style(format!("{indent}{}", "─".repeat(inner_w)))
                .dim()
                .to_string(),
        );
        lines.push(
            style(format!("{indent}{}", clip_to_width(EMPTY_MESSAGE, inner_w)))
                .dim()
                .to_string(),
        );
    } else {
        for (i, w) in list.worktrees().iter().enumerate() {
            // The root occupies the first entry, so worktree `i` sits at
            // selectable row i + 1.
            let row = i + 1;
            let selected = row == list.selected_index();
            let (mut top, mut detail) = worktree_row(
                w,
                name_width,
                detail_width,
                selected,
                row == list.active_index(),
                in_switch,
                live.contains(&w.path),
                waiting.contains(&w.path),
            );
            if in_switch && !selected {
                top = dim_row(&top);
                detail = dim_row(&detail);
            }
            lines.push(top);
            lines.push(detail);
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

/// Builds a `rows`-tall window pinned to the tail of the log, so the newest
/// lines are always shown (like a terminal). The TUI never scrolls, so the
/// window is always at the bottom. Used for the Overview results band.
fn log_tail(log: &[LogLine], width: usize, rows: usize) -> Vec<String> {
    let start = log.len().saturating_sub(rows);
    log[start..]
        .iter()
        .take(rows)
        .map(|l| log_line(l, width))
        .collect()
}

/// Shown in the right pane between attaching the terminal and its first screen
/// snapshot arriving.
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

/// The `›` cursor cell for a highlighted action-menu row, or a blank otherwise.
fn menu_marker(selected: bool) -> String {
    if selected {
        style("›").red().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// Builds one 在席 (Focus) menu row: a `›` cursor for the highlighted command,
/// its name, and its dimmed description, clipped to `width`.
fn focus_menu_row(info: &CommandInfo, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{:<9}", info.name)).cyan().bold().to_string()
    } else {
        style(format!("{:<9}", info.name)).cyan().to_string()
    };
    let desc_budget = width.saturating_sub(HINT_INDENT + 9);
    let desc = style(clip_to_width(info.description, desc_budget)).dim();
    clip_to_width(&format!("  {marker} {name}{desc}"), width)
}

/// Builds the 在席 (Focus) menu: a short header, one row per Session-scope
/// command (`›` cursor on the highlighted one), and a key hint.
fn focus_menu(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![
        style(format!("session: {}", state.focused_session_name()))
            .cyan()
            .bold()
            .to_string(),
        String::new(),
        style("Run a command:").dim().to_string(),
    ];
    let cursor = state.focus_menu_cursor();
    for (i, info) in state.focus_menu_commands().iter().enumerate() {
        lines.push(focus_menu_row(info, i == cursor, width));
    }
    lines.push(String::new());
    lines.push(
        style("↑↓ move   Enter run   t terminal   a agent")
            .dim()
            .to_string(),
    );
    lines
}

/// Builds the 在席 (Focus) prompt surface: a header, the session-scoped command
/// line (`❯ <input>▏`), and the Session-scope hint below it.
fn focus_prompt(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![
        style(format!("session: {}", state.focused_session_name()))
            .cyan()
            .bold()
            .to_string(),
        String::new(),
    ];
    let prompt = style("❯").red().bold();
    let text = style(state.focus_prompt()).cyan();
    lines.push(clip_to_width(&format!("{prompt} {text}{CARET}"), width));
    lines.push(String::new());
    lines.extend(focus_hint_lines(state.focus_prompt_hint(), width));
    lines
}

/// The hint rows for the 在席 prompt's Session-scope hint: the matching commands
/// while the word is typed, or the usage / examples once arguments are given.
fn focus_hint_lines(hint: Hint, width: usize) -> Vec<String> {
    match hint {
        Hint::Commands(hints) => hints
            .iter()
            .take(HINT_MAX)
            .map(|h| {
                let name = style(format!("{:<9}", h.name)).cyan().to_string();
                let desc = style(clip_to_width(
                    h.description,
                    width.saturating_sub(HINT_INDENT + 9),
                ))
                .dim();
                clip_to_width(&format!("    {name}{desc}"), width)
            })
            .collect(),
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

/// The 切替 (Switch) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; an idle session previews its 在席 action menu. The
/// header line carries the session's status and agent state.
fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // Identify the highlighted row. `selected()` is `Some` for a real session
    // row and `None` on the root row (which carries no git status / tracked
    // path), so the match doubles as the root/session split.
    let (name, live, waiting, status) = match state.list().selected() {
        Some(w) => (
            w.branch.as_deref().unwrap_or(DETACHED).to_string(),
            state.is_live(&w.path),
            state.is_waiting(&w.path),
            Some(w.status),
        ),
        None => (ROOT_NAME.to_string(), false, false, None),
    };

    // Header: the name, then either the git status + agent state (a session) or
    // the workspace-root note (the root row).
    let mut header = style(clip_to_width(&name, width)).cyan().bold().to_string();
    match status {
        Some(status) => {
            header.push_str(&format!("   {}", status_label(status)));
            if let Some(agent) = AgentState::from_flags(live, waiting).detail(width) {
                header.push_str(&format!("   {agent}"));
            }
        }
        None => header.push_str(&format!("   {}", style(ROOT_DETAIL).dim())),
    }
    let mut lines = vec![header, String::new()];

    if live {
        // Selecting re-attaches the running shell / agent: preview its actual
        // screen (the live snapshot taken before painting) so the choice shows
        // what re-attaching reveals. Fall back to a label until the first
        // snapshot is available.
        match state.terminal_view() {
            Some(view) => {
                let body = rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").green().to_string());
                lines.push(style("Enter / l で再アタッチ").dim().to_string());
            }
        }
    } else {
        // Selecting opens 在席 on this session: preview its action menu.
        lines.push(style("Run a command:").dim().to_string());
        for (i, info) in state.focus_menu_commands().iter().enumerate() {
            lines.push(focus_menu_row(info, i == 0, width));
        }
        lines.push(String::new());
        lines.push(style("Enter / l で開く").dim().to_string());
    }

    lines.truncate(rows);
    lines
}

/// The right pane's contents, by mode. Blank in 統括 (the user is on the command
/// line); a preview of the would-be session screen in 切替; the session's action
/// surface — a menu or a prompt, per [`SessionActionUi`] — in 在席; and the live
/// embedded terminal in 没入 (a starting hint until the first snapshot arrives).
fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    match state.mode() {
        Mode::Overview => Vec::new(),
        Mode::Switch => switch_preview(state, right_w, rows),
        Mode::Focus => match state.session_action_ui() {
            SessionActionUi::Menu => focus_menu(state, right_w),
            SessionActionUi::Prompt => focus_prompt(state, right_w),
        },
        Mode::Attached => match state.terminal_view() {
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

/// Fixed height of the command-hint band overlaid on the body in command mode.
/// It is tall enough for the largest hint list — a header line, [`HINT_MAX`]
/// rows, and a trailing "… and N more" — so the band's height never changes as
/// the match list grows or shrinks while typing. Because the band always covers
/// the same body rows, nothing beneath it jitters when the count changes.
const HINT_BAND: usize = HINT_MAX + 2;

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

/// The advisory hint lines drawn just above the command input in 統括: the
/// matching commands while the command word is typed, or the usage and examples
/// once a known command is given arguments. Empty outside Overview.
fn hint_lines(state: &HomeState, width: usize) -> Vec<String> {
    if state.mode() != Mode::Overview {
        return Vec::new();
    }
    match state.hint() {
        Hint::Commands(hints) => {
            let typed = state.input().trim_start();
            // Only point a marker at a best match once something is typed; a
            // bare prompt shows the whole menu with nothing pre-selected.
            let highlight = !typed.is_empty();
            // The Overview line is always workspace-scoped; a partial match just
            // says "matches".
            let header = if highlight {
                "matches".to_string()
            } else {
                "workspace commands".to_string()
            };
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

/// The command input line, by mode: the editable 統括 (Overview) command prompt,
/// a left-pane hint in 切替 (Switch), the focused session in 在席 (Focus), and a
/// live-terminal status in 没入 (Attached).
fn input_line(state: &HomeState) -> String {
    match state.mode() {
        Mode::Overview => {
            let prompt = style("❯").red().bold();
            let text = style(state.input()).cyan();
            format!(" {prompt} {text}{CARET}")
        }
        Mode::Switch => style(" Pick a session — ↑↓ move, Enter focus, c new".to_string())
            .dim()
            .to_string(),
        Mode::Focus => style(format!(
            " Operating session: {}",
            state.focused_session_name()
        ))
        .dim()
        .to_string(),
        Mode::Attached => style(" ● live terminal".to_string()).green().to_string(),
    }
}

/// Minimum frame height at which the 統括 input is drawn as a bordered box. Below
/// it the chrome (the box is 3 rows) would crowd out the body, so a short
/// terminal falls back to the single-line [`input_line`].
const INPUT_BOX_MIN_HEIGHT: usize = 8;

/// The 統括 (Overview) command input rendered as a bordered field — an
/// HTML-input-like box — so it reads clearly as *where you type*, set apart from
/// the hints above it and the results band below. Spans the full `width` (three
/// rows: top border, the `❯ <input>▏` line, bottom border).
fn overview_input_box(state: &HomeState, width: usize) -> Vec<String> {
    let prompt = style("❯").red().bold();
    let text = style(state.input()).cyan();
    let content = format!("{prompt} {text}{CARET}");
    // `boxed` adds the two borders and one space of padding on each side, so the
    // inner content area is the width less those four columns.
    widgets::boxed("", width.saturating_sub(4), &[content])
}

/// The footer help line, aware of the current mode. It leads with a mode tag so
/// it is always clear which engagement level the keys act on.
fn footer_line(width: usize, state: &HomeState) -> String {
    let help = match state.mode() {
        Mode::Overview => {
            "[overview]  Tab: complete / ↑↓: history / Enter: run / \"session switch\": pick session"
                .to_string()
        }
        Mode::Switch => {
            "[switch]  ↑↓ move / Enter focus / c new / Esc back / Ctrl-O overview".to_string()
        }
        Mode::Focus => {
            format!(
                "[session: {}]  Enter: run / Ctrl-O: switch / Esc: overview",
                state.focused_session_name()
            )
        }
        Mode::Attached => {
            "[attached]  Ctrl-O: switch session / Shift+↑↓/PgUp/PgDn: scroll".to_string()
        }
    };
    widgets::dim_line(width, &help)
}

/// Builds the inline create row appended to the left pane in 切替 (Switch) while
/// naming a new session: `+ new: <input>▏`, with an inline error below it. The
/// rows are clipped to the pane width.
fn switch_create_rows(input: &str, error: Option<&str>, left_w: usize) -> Vec<String> {
    let label = clip_to_width(&format!("+ new: {input}{CARET}"), left_w);
    let mut rows = vec![style(label).green().bold().to_string()];
    if let Some(err) = error {
        rows.push(style(clip_to_width(err, left_w)).red().to_string());
    }
    rows
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
    // Wide enough for the longest body line, the key-hints row below.
    const INNER: usize = 44;

    let mut body = vec![
        style("Select sessions to remove.").dim().to_string(),
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

/// Builds the centred quit-confirmation modal, shown when the user presses
/// `Ctrl-C` while a session is still live: it names how many sessions are still
/// running and asks whether to close anyway.
fn quit_confirm_frame(raw_height: usize, raw_width: usize, live: usize) -> Vec<String> {
    // Wide enough for the longest body line ("Close anyway? Running agents
    // will be stopped." = 45 columns) so it does not overflow the box.
    const INNER: usize = 46;
    let body = vec![
        style(format!("{live} session(s) still running."))
            .dim()
            .to_string(),
        String::new(),
        style("Close anyway? Running agents will be stopped.").to_string(),
        String::new(),
        style("y / Enter: close   n / Esc: cancel")
            .dim()
            .to_string(),
    ];
    widgets::render_modal(raw_height, raw_width, "Quit usagi?", INNER, &body)
}

/// Body lines the text modal shows at once; a longer dump scrolls, with a count
/// of the hidden lines above and below. Shared with the event loop's scroll
/// clamp and paging step.
pub const TEXT_MODAL_VISIBLE: usize = 16;

/// Builds the centred text modal: a scrollable window over a text-dumping
/// command's output (`man` / `history` / `session list`), coloured by line kind,
/// with `↑`/`↓` more-counts and the dismiss hint below.
fn text_modal_frame(raw_height: usize, raw_width: usize, modal: &TextModal) -> Vec<String> {
    const INNER: usize = 60;

    let total = modal.lines.len();
    let start = modal.scroll.min(total.saturating_sub(TEXT_MODAL_VISIBLE));
    let end = (start + TEXT_MODAL_VISIBLE).min(total);

    let mut body = Vec::new();
    if start > 0 {
        body.push(style(format!("  ↑ {start} more")).dim().to_string());
    }
    for line in &modal.lines[start..end] {
        body.push(log_line(line, INNER));
    }
    if end < total {
        body.push(style(format!("  ↓ {} more", total - end)).dim().to_string());
    }
    body.push(String::new());
    body.push(
        style("↑↓ scroll   Esc / Enter / q: close")
            .dim()
            .to_string(),
    );
    widgets::render_modal(raw_height, raw_width, &modal.title, INNER, &body)
}

/// How many rows the 統括 (Overview) results band spends below the input on the
/// command log tail. The newest output stays visible while typing. Kept small
/// so the bordered input box and the band together leave the session list its
/// full height.
const RESULTS_BAND: usize = 4;

/// Builds the full home-screen frame for a raw terminal size.
pub fn render_frame(raw_height: usize, raw_width: usize, state: &HomeState) -> Vec<String> {
    // The quit-confirmation modal, when open, overlays everything else.
    if state.quit_confirm() {
        return quit_confirm_frame(raw_height, raw_width, state.live_count());
    }
    // The session-removal modal, when open, overlays the whole screen.
    if let Some(modal) = state.remove_modal() {
        return remove_modal_frame(raw_height, raw_width, modal);
    }
    // The text modal (a text-dumping command's output) overlays the screen too.
    if let Some(modal) = state.text_modal() {
        return text_modal_frame(raw_height, raw_width, modal);
    }

    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, right_w) = layout(width);

    // The 統括 input is a bordered box (3 rows) when there is height for it;
    // every other mode — and a short terminal — uses a single status line.
    let input_lines = if state.mode() == Mode::Overview && height >= INPUT_BOX_MIN_HEIGHT {
        overview_input_box(state, width)
    } else {
        vec![input_line(state)]
    };
    let input_h = input_lines.len();

    // In 統括 the command log renders as a band below the input; it is sized so
    // the body keeps at least one row. Other modes use no results band.
    let results = if state.mode() == Mode::Overview {
        RESULTS_BAND.min(height.saturating_sub(4 + input_h))
    } else {
        0
    };

    // Chrome: title + mode ladder on top, the input block + footer + the
    // optional results band at the bottom. Everything between is the two-pane
    // body.
    let body_rows = height.saturating_sub(3 + input_h + results).max(1);
    let mut left = left_pane(
        state.list(),
        state.live_paths(),
        state.waiting_paths(),
        left_w,
        body_rows,
        // In 切替 the keyboard is on the list: show the `>` cursor and fade the
        // rows it is not on.
        state.mode() == Mode::Switch,
    );
    // While naming a new session in 切替, append the inline create row(s) to the
    // left pane (trimmed back to the body if it would overflow).
    if state.is_creating() {
        for row in switch_create_rows(
            state.create_input().unwrap_or_default(),
            state.create_error(),
            left_w,
        ) {
            left.push(row);
        }
        left.truncate(body_rows);
    }
    let right = right_pane_contents(state, right_w, body_rows);

    let mut lines = Vec::with_capacity(height);
    lines.push(title_bar(width, state.list()));
    lines.push(mode_ladder(width, state.mode()));
    let body_start = lines.len();
    for row in 0..body_rows {
        let left_cell = pad_to_width(left.get(row).cloned().unwrap_or_default(), left_w);
        let right_cell = right.get(row).cloned().unwrap_or_default();
        lines.push(format!("{left_cell}{SEP}{right_cell}"));
    }

    // Overlay the 統括 command hints onto a fixed-height band at the bottom of the
    // body, always leaving at least one body row uncovered. The band is a
    // constant height regardless of how many hints currently match, so the body
    // rows it covers never change as the match list grows or shrinks while
    // typing. The band is cleared first (so no stale body text shows through),
    // then the hints are bottom-anchored just above the input.
    let hints = hint_lines(state, width);
    if !hints.is_empty() {
        let band = HINT_BAND.min(body_rows.saturating_sub(1));
        let band_start = body_start + body_rows - band;
        for line in lines.iter_mut().skip(band_start).take(band) {
            *line = pad_to_width(String::new(), width);
        }
        let shown = hints.len().min(band);
        let hint_top = body_start + body_rows - shown;
        for (i, hint) in hints.into_iter().take(shown).enumerate() {
            lines[hint_top + i] = pad_to_width(hint, width);
        }
    }

    lines.extend(input_lines);
    // The 統括 results band: only the latest command's response, drawn below the
    // input. Always exactly `results` rows tall (blank-padded) so the footer stays
    // at the bottom regardless of how much output there is.
    if results > 0 {
        let tail = log_tail(state.response_lines(), width, results);
        for row in 0..results {
            let line = tail.get(row).cloned().unwrap_or_default();
            lines.push(pad_to_width(line, width));
        }
    }
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

    fn state_with(worktrees: Vec<WorktreeState>) -> HomeState {
        HomeState::new("usagi", worktrees, None)
    }

    fn stripped(lines: &[String]) -> String {
        console::strip_ansi_codes(&lines.join("\n")).into_owned()
    }

    #[test]
    fn text_modal_frame_windows_a_long_dump_with_more_counts() {
        let lines: Vec<LogLine> = (0..30)
            .map(|i| LogLine::output(format!("entry {i}")))
            .collect();
        let modal = TextModal {
            title: "Help".to_string(),
            lines,
            scroll: 5,
        };
        let frame = stripped(&text_modal_frame(40, 120, &modal));
        // The title and the hidden-line counts above and below the window show.
        assert!(frame.contains("Help"));
        assert!(frame.contains("↑ 5 more"));
        // 30 total - (scroll 5 + 16 visible) = 9 hidden below.
        assert!(frame.contains("↓ 9 more"));
        // A windowed line is visible; ones outside the window are not.
        assert!(frame.contains("entry 5"));
        assert!(!frame.contains("entry 0"));
        assert!(frame.contains("Esc / Enter / q: close"));
    }

    #[test]
    fn text_modal_frame_shows_a_short_dump_without_scroll_counts() {
        let modal = TextModal {
            title: "History".to_string(),
            lines: vec![LogLine::output("  1  man"), LogLine::output("  2  history")],
            scroll: 0,
        };
        let frame = stripped(&text_modal_frame(40, 120, &modal));
        assert!(frame.contains("History"));
        assert!(frame.contains("man"));
        assert!(!frame.contains("more"));
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
        let (left, right) = layout(4);
        assert!(left <= 4);
        assert_eq!(right, 0);
    }

    #[test]
    fn title_bar_singular_and_plural() {
        let one = title_bar(80, &list_with(vec![]));
        assert!(one.contains("usagi"));
        assert!(one.contains("1 session"));
        assert!(!one.contains("1 sessions"));
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
    fn status_label_pairs_a_git_icon_with_each_word() {
        for (status, icon, word) in [
            (BranchStatus::Local, LOCAL_ICON, "local"),
            (BranchStatus::Pushed, PUSHED_ICON, "pushed"),
            (BranchStatus::UpToDate, SYNCED_ICON, "synced"),
        ] {
            let plain = console::strip_ansi_codes(&status_label(status)).into_owned();
            assert!(plain.contains(icon), "{plain:?} missing its icon");
            assert!(plain.contains(word), "{plain:?} missing its word");
            // The icon leads the word: `<icon> <word>`.
            assert_eq!(plain, format!("{icon} {word}"));
        }
    }

    #[test]
    fn worktree_row_marks_selected_primary_and_detached() {
        // The `>` cursor only appears in 切替 (Switch): the selected row carries it
        // when `in_switch` is set.
        let (top, _) = worktree_row(
            &worktree(Some("main"), true, BranchStatus::Pushed),
            10,
            10,
            true,
            false,
            true,
            false,
            false,
        );
        assert!(top.contains('>'));
        assert!(top.contains('●'));
        assert!(top.contains("main"));

        // The same selected row outside Switch shows no cursor.
        let (top_no_switch, _) = worktree_row(
            &worktree(Some("main"), true, BranchStatus::Pushed),
            10,
            10,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(!top_no_switch.contains('>'));

        let (other_top, _) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            10,
            false,
            false,
            true,
            false,
            false,
        );
        assert!(!other_top.contains('>'));
        assert!(other_top.contains('○'));
        assert!(other_top.contains("feature"));

        let (detached_top, _) = worktree_row(
            &worktree(None, false, BranchStatus::Local),
            10,
            10,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(detached_top.contains("(detached)"));
    }

    #[test]
    fn worktree_row_marks_the_active_worktree_with_a_gutter_bar_on_both_lines() {
        let (active_top, active_detail) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            10,
            false,
            true,
            false,
            true,
            false,
        );
        // The green `▎` accent bar runs down both lines of the active row (the
        // detail line carries it too, to the left of the agent state).
        assert!(active_top.contains('▎'));
        assert!(active_detail.contains('▎'));
        // The old `*` marker is gone.
        assert!(!active_top.contains('*'));
        let (idle_top, idle_detail) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            10,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(!idle_top.contains('▎'));
        assert!(!idle_detail.contains('▎'));
    }

    #[test]
    fn worktree_row_shows_a_running_agent_and_one_waiting_for_input() {
        let (_, running_detail) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            12,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(running_detail.contains('▶'));
        assert!(running_detail.contains("running"));

        let (_, waiting_detail) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            12,
            false,
            false,
            false,
            true,
            true,
        );
        assert!(waiting_detail.contains('◆'));
        assert!(!waiting_detail.contains('▶'));
        assert!(waiting_detail.contains("waiting"));

        let (idle_top, idle_detail) = worktree_row(
            &worktree(Some("feature"), false, BranchStatus::Local),
            10,
            12,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(!idle_detail.contains('▶'));
        assert!(!idle_detail.contains('◆'));
        assert!(idle_top.contains("local"));
    }

    #[test]
    fn status_cell_right_aligns_the_status_and_blanks_the_root() {
        let pushed =
            console::strip_ansi_codes(&status_cell(Some(BranchStatus::Pushed))).into_owned();
        assert_eq!(console::measure_text_width(&pushed), STATUS_COL);
        assert!(pushed.ends_with("pushed"));
        // The icon leads the word inside the field.
        assert!(pushed.contains(PUSHED_ICON));
        // "local" (icon + space + 5 cols = 7) is right-aligned within the 8-col
        // field, so a single lead space precedes the icon.
        let local = console::strip_ansi_codes(&status_cell(Some(BranchStatus::Local))).into_owned();
        assert_eq!(local, format!(" {LOCAL_ICON} local"));
        // The root has no status: an all-blank field of the same width.
        assert_eq!(status_cell(None), " ".repeat(STATUS_COL));
    }

    #[test]
    fn worktree_row_truncates_a_long_branch() {
        let (top, _) = worktree_row(
            &worktree(
                Some("feature/a-very-long-branch-name"),
                false,
                BranchStatus::Local,
            ),
            8,
            8,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(top.contains('…'));
    }

    #[test]
    fn root_row_marks_selected_and_active() {
        // The `>` cursor shows on the selected root only in 切替 (Switch).
        let (top, detail) = root_row(10, 20, true, false, true);
        assert!(top.contains('>'));
        assert!(top.contains('⌂'));
        assert!(top.contains(ROOT_NAME));
        assert!(detail.contains("workspace root"));
        // The same selected root outside Switch shows no cursor.
        let (top_no_switch, _) = root_row(10, 20, true, false, false);
        assert!(!top_no_switch.contains('>'));

        // The active root carries the green `▎` bar down both lines, not a `*`.
        let (active_top, active_detail) = root_row(10, 20, false, true, false);
        assert!(active_top.contains('▎'));
        assert!(active_detail.contains('▎'));
        assert!(!active_top.contains('*'));

        let (idle_top, idle_detail) = root_row(10, 20, false, false, false);
        assert!(!idle_top.contains('>'));
        assert!(!idle_top.contains('▎'));
        assert!(!idle_detail.contains('▎'));
        assert!(idle_top.contains(ROOT_NAME));
    }

    #[test]
    fn left_pane_renders_the_root_entry_then_the_empty_message() {
        let lines = left_pane(
            &list_with(Vec::new()),
            &HashSet::new(),
            &HashSet::new(),
            80,
            6,
            false,
        );
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[1].contains("workspace root"));
        assert!(lines[2].contains('─'));
        assert!(lines[3].contains("no sessions"));
        let hint = console::strip_ansi_codes(&lines[3]);
        assert!(hint.starts_with(&" ".repeat(NAME_PREFIX)));
        assert!(hint[NAME_PREFIX..].starts_with("no sessions"));
    }

    #[test]
    fn left_pane_renders_the_root_entry_then_one_entry_per_worktree() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[2].contains("main"));
        assert!(lines[4].contains("feature"));
    }

    #[test]
    fn left_pane_marks_a_running_agent_and_one_waiting_for_input() {
        let list = list_with(vec![worktree(Some("feature"), false, BranchStatus::Local)]);
        let path: HashSet<PathBuf> = [PathBuf::from("/repo/wt")].into_iter().collect();
        let running = left_pane(&list, &path, &HashSet::new(), 30, 6, false);
        assert!(running[3].contains('▶'));
        assert!(running[3].contains("running"));
        let waiting = left_pane(&list, &path, &path, 30, 6, false);
        assert!(waiting[3].contains('◆'));
        assert!(!waiting[3].contains('▶'));
        let idle = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
        assert!(!idle[3].contains('▶'));
        assert!(!idle[3].contains('◆'));
        assert!(idle[2].contains("local"));
    }

    #[test]
    fn left_pane_is_trimmed_to_available_rows() {
        let list = list_with(vec![
            worktree(Some("a"), false, BranchStatus::Local),
            worktree(Some("b"), false, BranchStatus::Local),
            worktree(Some("c"), false, BranchStatus::Local),
        ]);
        let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 3, false);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(ROOT_NAME));
        assert!(lines[2].contains('a'));
    }

    #[test]
    fn left_pane_marks_the_active_worktree_with_a_gutter_bar() {
        let mut list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        list.activate_by_name("feature");
        let lines = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, false);
        // The root is not active; the active "feature" row carries the green `▎`
        // accent bar down both of its lines (identity + detail).
        assert!(!lines[0].contains('▎'));
        assert!(lines[4].contains("feature"));
        assert!(lines[4].contains('▎'));
        assert!(lines[5].contains('▎'));
    }

    #[test]
    fn dim_row_strips_existing_colour_but_keeps_the_text() {
        // Fading a row drops its colour codes (so it reads as muted) while the
        // text survives. (Styling is off in non-TTY tests, so we assert the
        // colour is gone rather than that a dim code was added.)
        let coloured = "\u{1b}[36mfeature\u{1b}[0m";
        let dimmed = dim_row(coloured);
        assert!(!dimmed.contains("\u{1b}[36m"));
        assert!(console::strip_ansi_codes(&dimmed).contains("feature"));
    }

    #[test]
    fn left_pane_fades_every_row_but_the_cursor_when_asked() {
        let list = list_with(vec![
            worktree(Some("main"), true, BranchStatus::Pushed),
            worktree(Some("feature"), false, BranchStatus::Local),
        ]);
        // Cursor is on the root row (index 0). Dimming on fades the non-cursor
        // session rows; every row keeps its text.
        let dimmed = left_pane(&list, &HashSet::new(), &HashSet::new(), 30, 6, true);
        assert_eq!(dimmed.len(), 6);
        assert!(console::strip_ansi_codes(&dimmed[0]).contains(ROOT_NAME));
        assert!(console::strip_ansi_codes(&dimmed[2]).contains("main"));
        assert!(console::strip_ansi_codes(&dimmed[4]).contains("feature"));
    }

    #[test]
    fn log_line_colours_each_kind_and_prompts_commands() {
        assert!(log_line(&LogLine::command("man"), 40).contains("❯ man"));
        assert_eq!(log_line(&LogLine::output("plain"), 40), "plain");
        assert!(log_line(&LogLine::error("boom"), 40).contains("boom"));
        assert!(log_line(&LogLine::notice("note"), 40).contains("note"));
    }

    #[test]
    fn log_tail_shows_only_the_tail_that_fits() {
        let log: Vec<LogLine> = (0..5)
            .map(|i| LogLine::output(format!("line {i}")))
            .collect();
        let lines = log_tail(&log, 40, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("line 2"));
        assert!(lines[2].contains("line 4"));
    }

    #[test]
    fn log_tail_keeps_everything_when_it_fits() {
        let log = vec![LogLine::output("only")];
        assert_eq!(log_tail(&log, 40, 5).len(), 1);
    }

    // --- right pane by mode ------------------------------------------------

    #[test]
    fn right_pane_is_blank_in_overview_but_previews_in_switch() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        assert!(right_pane_contents(&state, 40, 5).is_empty());
        // In 切替 the right pane previews the would-be screen for the cursor row.
        state.enter_switch(super::super::state::ReturnMode::Overview);
        let preview = stripped(&right_pane_contents(&state, 40, 12));
        // The root row previews its action menu (the workspace-root note shows).
        assert!(preview.contains("root"));
        assert!(preview.contains("workspace root"));
        assert!(preview.contains("terminal"));
    }

    #[test]
    fn switch_preview_shows_a_live_session_as_a_reattach() {
        let mut running = worktree(Some("feat"), false, BranchStatus::Local);
        running.path = PathBuf::from("/repo/run");
        let mut state = HomeState::new("usagi", vec![running], None);
        state.set_live([PathBuf::from("/repo/run")].into());
        state.enter_switch(super::super::state::ReturnMode::Overview);
        // Move the cursor off the root onto the session row.
        state.switch_move_down();
        let preview = stripped(&switch_preview(&state, 40, 12));
        assert!(preview.contains("feat"));
        // Header carries the git status and the running agent state.
        assert!(preview.contains("local"));
        assert!(preview.contains("running"));
        // A live session with no snapshot yet falls back to the re-attach label,
        // not the action menu.
        assert!(preview.contains("live terminal"));
        assert!(!preview.contains("Run a command"));
    }

    #[test]
    fn switch_preview_shows_a_live_session_as_its_actual_screen() {
        let mut running = worktree(Some("feat"), false, BranchStatus::Local);
        running.path = PathBuf::from("/repo/run");
        let mut state = HomeState::new("usagi", vec![running], None);
        state.set_live([PathBuf::from("/repo/run")].into());
        // The event loop snapshots the highlighted live session before painting.
        state.set_terminal_view(TerminalView::from_rows(
            vec!["$ echo hi".to_string(), "hi".to_string()],
            None,
        ));
        state.enter_switch(super::super::state::ReturnMode::Overview);
        state.switch_move_down();
        let preview = stripped(&switch_preview(&state, 40, 12));
        // The real terminal screen is shown, not the placeholder label.
        assert!(preview.contains("$ echo hi"));
        assert!(preview.contains("hi"));
        assert!(!preview.contains("live terminal"));
        assert!(!preview.contains("Run a command"));
    }

    #[test]
    fn switch_preview_shows_an_idle_session_as_its_action_menu() {
        let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
        let mut state = HomeState::new("usagi", vec![idle], None);
        state.enter_switch(super::super::state::ReturnMode::Overview);
        state.switch_move_down();
        let preview = stripped(&switch_preview(&state, 40, 12));
        // An idle session previews the 在席 action menu it would open.
        assert!(preview.contains("pushed"));
        assert!(preview.contains("Run a command"));
        assert!(preview.contains("terminal"));
        assert!(preview.contains("agent"));
        assert!(!preview.contains("live terminal"));
    }

    #[test]
    fn right_pane_shows_the_focus_menu_or_prompt() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.enter_focus(1);
        // Menu (the default) lists the session commands.
        let menu = stripped(&right_pane_contents(&state, 40, 12));
        assert!(menu.contains("session: main"));
        assert!(menu.contains("terminal"));
        assert!(menu.contains("agent"));
        assert!(menu.contains('›'));

        // Prompt shows a typed command line with the session-scope hint.
        state.set_session_action_ui(SessionActionUi::Prompt);
        state.enter_focus(1);
        for c in "ter".chars() {
            state.focus_prompt_push_char(c);
        }
        let prompt = stripped(&right_pane_contents(&state, 40, 12));
        assert!(prompt.contains("session: main"));
        assert!(prompt.contains("❯ ter"));
        // The session-scope hint lists terminal as a match.
        assert!(prompt.contains("terminal"));
    }

    #[test]
    fn focus_prompt_shows_usage_for_arguments() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.set_session_action_ui(SessionActionUi::Prompt);
        state.enter_focus(1);
        for c in "terminal ".chars() {
            state.focus_prompt_push_char(c);
        }
        let prompt = stripped(&right_pane_contents(&state, 60, 12));
        assert!(prompt.contains("usage"));
        assert!(prompt.contains("terminal"));
    }

    #[test]
    fn focus_prompt_has_no_hint_for_an_unknown_command_word() {
        // An unknown word yields `Hint::None`, so no hint rows are drawn.
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.set_session_action_ui(SessionActionUi::Prompt);
        state.enter_focus(1);
        for c in "zzz".chars() {
            state.focus_prompt_push_char(c);
        }
        // The header, blank, and prompt lines are present, but no hint rows follow.
        let rows = right_pane_contents(&state, 60, 12);
        assert!(stripped(&rows).contains("❯ zzz"));
        // The prompt body has exactly the header, a blank, the prompt, and a blank
        // separator — no hint rows after it.
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn right_pane_shows_the_terminal_when_attached() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.enter_focus(1);
        state.show_attached();
        // No snapshot yet: a starting hint.
        let starting = right_pane_contents(&state, 40, 5);
        assert!(starting[0].contains("Starting terminal"));
        // Once a snapshot arrives, its rows are shown.
        state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
        let running = right_pane_contents(&state, 40, 5);
        assert!(running[0].contains("$ echo hi"));
    }

    #[test]
    fn focus_menu_row_marks_the_cursor() {
        let info = CommandInfo {
            name: "terminal",
            description: "Open a shell",
            usage: "terminal",
            examples: &[],
            scope: super::super::command::CommandScope::Session,
        };
        let selected = console::strip_ansi_codes(&focus_menu_row(&info, true, 60)).into_owned();
        assert!(selected.contains('›'));
        assert!(selected.contains("terminal"));
        let idle = console::strip_ansi_codes(&focus_menu_row(&info, false, 60)).into_owned();
        assert!(!idle.contains('›'));
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
    fn terminal_geometry_matches_the_rendered_layout() {
        let geo = terminal_geometry(24, 80);
        let (left, _) = layout(80);
        assert_eq!(geo.origin_col as usize, left + SEP_WIDTH);
        assert_eq!(geo.origin_row, 2);
        assert_eq!(geo.rows, 20);
        assert_eq!(geo.cols as usize, 80 - left - SEP_WIDTH);
    }

    #[test]
    fn terminal_geometry_stays_positive_in_a_tiny_terminal() {
        let geo = terminal_geometry(1, 1);
        assert!(geo.rows >= 1);
        assert!(geo.cols >= 1);
    }

    #[test]
    fn render_frame_draws_the_terminal_in_the_right_pane_when_attached() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        state.enter_focus(1);
        state.show_attached();
        state.set_terminal_view(TerminalView::from_rows(
            vec!["$ cargo test".to_string()],
            None,
        ));
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("main"));
        assert!(joined.contains("$ cargo test"));
        // The attached footer advertises Ctrl-O.
        assert!(joined.contains("attached"));
    }

    // --- input / footer by mode --------------------------------------------

    #[test]
    fn input_line_renders_prompt_in_overview() {
        let mut state = state_with(Vec::new());
        state.push_char('m');
        let line = input_line(&state);
        assert!(line.contains('m'));
        assert!(line.contains(CARET));
    }

    #[test]
    fn input_line_differs_by_mode() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.enter_switch(super::super::state::ReturnMode::Overview);
        assert!(input_line(&state).contains("Pick a session"));
        state.enter_focus(1);
        assert!(input_line(&state).contains("Operating session: main"));
        state.show_attached();
        assert!(input_line(&state).contains("live terminal"));
    }

    #[test]
    fn footer_line_differs_by_mode() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        assert!(footer_line(80, &state).contains("overview"));
        state.enter_switch(super::super::state::ReturnMode::Overview);
        assert!(footer_line(80, &state).contains("switch"));
        state.enter_focus(1);
        assert!(footer_line(80, &state).contains("session: main"));
        state.show_attached();
        assert!(footer_line(80, &state).contains("attached"));
    }

    #[test]
    fn mode_ladder_lists_every_step_and_keeps_them_for_each_mode() {
        for mode in [Mode::Overview, Mode::Switch, Mode::Focus, Mode::Attached] {
            let ladder = console::strip_ansi_codes(&mode_ladder(80, mode)).into_owned();
            for step in ["Overview", "Switch", "Focus", "Attached"] {
                assert!(ladder.contains(step), "{mode:?} ladder missing {step}");
            }
        }
    }

    #[test]
    fn overview_input_is_a_bordered_box_at_full_height() {
        let mut state = state_with(Vec::new());
        for c in "session".chars() {
            state.push_char(c);
        }
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        // The input is framed (top/bottom borders) and still carries the prompt.
        assert!(joined.contains('┌'));
        assert!(joined.contains('└'));
        assert!(joined.contains("❯ session"));
    }

    #[test]
    fn overview_input_falls_back_to_a_single_line_on_a_short_terminal() {
        let state = state_with(Vec::new());
        // Too short for the 3-row box: the input is the plain prompt line.
        let lines = render_frame(6, 80, &state);
        let joined = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        assert!(!joined.contains('┌'));
        assert!(joined.contains('❯'));
    }

    // --- Switch inline create ----------------------------------------------

    #[test]
    fn switch_create_rows_show_the_input_and_an_error() {
        let rows = switch_create_rows("wip", None, 30);
        assert_eq!(rows.len(), 1);
        let plain = console::strip_ansi_codes(&rows[0]).into_owned();
        assert!(plain.contains("+ new: wip"));
        assert!(plain.contains(CARET));

        let with_error = switch_create_rows("feature", Some("\"feature\" already exists."), 40);
        assert_eq!(with_error.len(), 2);
        assert!(console::strip_ansi_codes(&with_error[1]).contains("already exists"));
    }

    #[test]
    fn render_frame_shows_the_inline_create_row_in_switch() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.enter_switch(super::super::state::ReturnMode::Overview);
        state.switch_begin_create();
        for c in "wip".chars() {
            state.create_push_char(c);
        }
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("+ new: wip"));
        assert!(joined.contains("switch"));
    }

    // --- command hints (Overview) ------------------------------------------

    fn typing(typed: &str) -> HomeState {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        for c in typed.chars() {
            state.push_char(c);
        }
        state
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
    fn hint_lines_are_empty_outside_overview() {
        let mut state = HomeState::new(
            "usagi",
            vec![worktree(Some("m"), true, BranchStatus::Local)],
            None,
        );
        state.enter_focus(1);
        assert!(hint_lines(&state, 80).is_empty());
    }

    #[test]
    fn hint_lines_list_every_command_for_a_bare_prompt() {
        let state = typing("");
        let joined = stripped(&hint_lines(&state, 80));
        assert!(joined.contains("commands"));
        assert!(!joined.contains('›'));
        assert!(joined.contains("more"));
        assert!(joined.contains("session"));
    }

    #[test]
    fn hint_lines_highlight_the_best_match_while_typing() {
        let state = typing("s");
        let joined = stripped(&hint_lines(&state, 80));
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
        assert!(joined.contains("session [create"));
        assert!(joined.contains("e.g."));
        assert!(joined.contains("session create"));
    }

    #[test]
    fn hint_lines_show_usage_without_examples_when_a_command_has_none() {
        let state = typing("doctor ");
        let joined = stripped(&hint_lines(&state, 80));
        assert!(joined.contains("usage"));
        assert!(joined.contains("doctor"));
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
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("matches"));
        assert!(joined.contains("session"));
    }

    // --- removal modal -----------------------------------------------------

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
        state.remove_modal_toggle();
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
        assert!(!joined.contains("overview"));
    }

    #[test]
    fn render_frame_overlays_the_quit_confirmation_modal() {
        let mut state = state_with_sessions(&["alpha", "beta"]);
        let live: std::collections::HashSet<std::path::PathBuf> =
            ["/ws/alpha", "/ws/beta"].iter().map(Into::into).collect();
        state.set_live(live);
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
            state.remove_modal_move_down();
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

    // --- render_frame composition ------------------------------------------

    #[test]
    fn render_frame_combines_all_sections_at_full_height() {
        let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
        let frame = render_frame(24, 80, &state);
        assert_eq!(frame.len(), 24);
        assert!(frame[0].contains("usagi"));
        assert!(frame[2].contains('│'));
        assert!(frame.last().unwrap().contains("overview"));
        let joined = frame.join("\n");
        assert!(joined.contains("main"));
        // The Overview results band carries the seeded log hint below the input.
        assert!(joined.contains("man"));
    }

    #[test]
    fn render_frame_results_band_shows_command_output_below_the_input() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        for c in "session list".chars() {
            state.push_char(c);
        }
        state.submit();
        let frame = render_frame(24, 80, &state);
        let input_row = frame.iter().position(|l| l.contains('❯')).unwrap();
        let joined_below =
            console::strip_ansi_codes(&frame[input_row + 1..].join("\n")).into_owned();
        // The echoed command shows in the results band, below the input.
        assert!(joined_below.contains("session list"));
    }

    #[test]
    fn render_frame_surfaces_running_and_waiting_agent_icons() {
        let mut running = worktree(Some("feat"), false, BranchStatus::Local);
        running.path = PathBuf::from("/repo/run");
        let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
        waiting.path = PathBuf::from("/repo/wait");
        let mut state = HomeState::new("usagi", vec![running, waiting], None);
        state.set_live([PathBuf::from("/repo/run"), PathBuf::from("/repo/wait")].into());
        state.set_waiting([PathBuf::from("/repo/wait")].into());
        let frame = render_frame(24, 80, &state);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains('▶'));
        assert!(joined.contains("running"));
        assert!(joined.contains('◆'));
        assert!(joined.contains("waiting"));
    }

    #[test]
    fn render_frame_survives_a_short_terminal() {
        let state = state_with(Vec::new());
        let frame = render_frame(3, 80, &state);
        assert!(frame[0].contains("usagi"));
        assert!(frame.last().unwrap().contains("overview"));
        assert!(frame.len() >= 4);
    }

    #[test]
    fn render_frame_focus_menu_keeps_its_height() {
        let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
        state.enter_focus(1);
        let frame = render_frame(24, 80, &state);
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        // The right pane carries the action menu; no results band in Focus.
        assert!(joined.contains("terminal"));
        assert!(joined.contains("session: main"));
    }
}
