//! The two-pane body: the worktree list (left) and the mode-dependent right
//! pane (a switch preview, the focus menu/prompt, or the embedded terminal).
//! All functions take plain data and return styled lines.

use std::collections::HashSet;
use std::path::PathBuf;

use console::style;

use super::super::command::{CommandInfo, Hint};
use super::super::state::{HomeState, LineKind, LogLine, Mode, WorktreeList, ROOT_NAME};
use super::super::terminal_view::TerminalView;
use super::{
    clip_to_width, ACTIVE_COL, CARET, DETACHED, DIRTY_ICON, EMPTY_MESSAGE, HINT_INDENT, HINT_MAX,
    LOCAL_ICON, NAME_PREFIX, NEW_ICON, PUSHED_ICON, ROOT_DETAIL, STATUS_COL, SYNCED_ICON,
    TERMINAL_STARTING,
};
use crate::domain::settings::SessionActionUi;
use crate::domain::workspace_state::{BranchStatus, WorktreeState};

/// The Nerd Font git glyph for a branch lifecycle status.
fn status_icon(status: BranchStatus) -> char {
    match status {
        BranchStatus::New => NEW_ICON,
        BranchStatus::Dirty => DIRTY_ICON,
        BranchStatus::Local => LOCAL_ICON,
        BranchStatus::Pushed => PUSHED_ICON,
        BranchStatus::Synced => SYNCED_ICON,
    }
}

/// The colour-coded `<icon> <word>` label for a branch's lifecycle status. The
/// icon gives an at-a-glance read; the word keeps it legible without a Nerd
/// Font and disambiguates the colour.
pub(super) fn status_label(status: BranchStatus) -> String {
    let text = format!("{} {}", status_icon(status), status.as_str());
    match status {
        BranchStatus::New => style(text).blue().to_string(),
        BranchStatus::Dirty => style(text).magenta().to_string(),
        BranchStatus::Local => style(text).yellow().to_string(),
        BranchStatus::Pushed => style(text).green().to_string(),
        BranchStatus::Synced => style(text).cyan().to_string(),
    }
}

/// The line-1 right-edge status field: the colour-coded `<icon> <word>` label
/// right-aligned within [`STATUS_COL`] columns, or all blanks when there is no
/// status (the root row).
pub(super) fn status_cell(status: Option<BranchStatus>) -> String {
    match status {
        None => " ".repeat(STATUS_COL),
        Some(status) => {
            let label = status_label(status);
            let pad = STATUS_COL.saturating_sub(console::measure_text_width(&label));
            format!("{}{label}", " ".repeat(pad))
        }
    }
}

/// The state of a session's embedded agent, shown by an icon on the row's first
/// line and spelled out on its detail line.
#[derive(Clone, Copy)]
enum AgentState {
    /// No live embedded session: the row carries no agent detail.
    Absent,
    /// A live session whose agent has started but not begun a turn yet — idle,
    /// awaiting the first prompt. Displayed as `☾ ready`.
    Ready,
    /// A live session whose agent is working a turn. Displayed as `▶ running`.
    Running,
    /// A live session whose agent paused mid-turn and awaits the user's input or
    /// permission. Displayed as `◆ waiting`.
    Waiting,
    /// A session whose agent has finished — a turn completed or its process
    /// exited; the bare shell it ran in may still be alive. Displayed as `✓ done`.
    Done,
}

impl AgentState {
    /// Pick the state from the live / running / waiting / done flags, in
    /// precedence order: done (the agent exited) wins over waiting, which wins
    /// over running, which wins over a plain live session (ready) — every state
    /// but [`Absent`](Self::Absent) is necessarily live.
    fn from_flags(live: bool, running: bool, waiting: bool, done: bool) -> Self {
        if done {
            AgentState::Done
        } else if waiting {
            AgentState::Waiting
        } else if running {
            AgentState::Running
        } else if live {
            AgentState::Ready
        } else {
            AgentState::Absent
        }
    }

    /// The detail-line content: an icon together with its label — `☾ ready`
    /// (dim), `▶ running` (green), `◆ waiting` (yellow), or `✓ done` (cyan) —
    /// clipped to `width`, or `None` when absent (the row has no agent in use).
    fn detail(self, width: usize) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(style(clip_to_width("☾ ready", width)).dim().to_string()),
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
            AgentState::Done => Some(
                style(clip_to_width("✓ done", width))
                    .cyan()
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
/// (`☾ ready` / `▶ running` / `◆ waiting` / `✓ done`).
#[allow(clippy::too_many_arguments)]
pub(super) fn worktree_row(
    worktree: &WorktreeState,
    name_width: usize,
    detail_width: usize,
    selected: bool,
    active: bool,
    in_switch: bool,
    live: bool,
    running: bool,
    waiting: bool,
    done: bool,
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

    // Line 2 spells out the agent state with its icon, or is blank when absent.
    // Only the active bar runs down to it — the `>` cursor stays a single point
    // on line 1, so the detail-line gutter ignores the cursor.
    let detail = AgentState::from_flags(live, running, waiting, done)
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
pub(super) fn root_row(
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
pub(super) fn dim_row(line: &str) -> String {
    style(console::strip_ansi_codes(line).into_owned())
        .dim()
        .to_string()
}

/// Builds the left pane: each entry spans two lines (an identity line and a
/// detail line) — the root entry first, then one per worktree (or the empty
/// message when none are recorded), trimmed to the available `rows`. `live` holds
/// the worktree paths with an embedded session (a live-but-idle one shows
/// `☾ ready`), `running` the ones working a turn (`▶ running`), `waiting` the
/// ones whose agent awaits input (`◆ waiting`), and `done` the finished ones
/// (`✓ done`); precedence is done > waiting > running > ready. When `in_switch`
/// is set (in 切替), the keyboard is on the list: the selected row shows a `>`
/// cursor and every other row is faded so the highlighted session reads first.
#[allow(clippy::too_many_arguments)]
pub(super) fn left_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
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
    // A divider separating the workspace root from the sessions below — indented
    // to start under the `root` label (past the cursor and kind-icon cells).
    let indent = " ".repeat(NAME_PREFIX);
    let inner_w = left_w.saturating_sub(NAME_PREFIX);
    lines.push(
        style(format!("{indent}{}", "─".repeat(inner_w)))
            .dim()
            .to_string(),
    );
    if list.is_empty() {
        // No sessions yet — show the empty message under the divider.
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
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
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
pub(super) fn log_line(line: &LogLine, width: usize) -> String {
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
pub(super) fn log_tail(log: &[LogLine], width: usize, rows: usize) -> Vec<String> {
    let start = log.len().saturating_sub(rows);
    log[start..]
        .iter()
        .take(rows)
        .map(|l| log_line(l, width))
        .collect()
}

/// Builds the right pane from an embedded terminal snapshot: each grid row,
/// clipped to the pane width, up to `rows` rows.
pub(super) fn terminal_pane(view: &TerminalView, right_w: usize, rows: usize) -> Vec<String> {
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
pub(super) fn focus_menu_row(info: &CommandInfo, selected: bool, width: usize) -> String {
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
pub(super) fn focus_menu(state: &HomeState, width: usize) -> Vec<String> {
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
pub(super) fn focus_prompt(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![
        style(format!("session: {}", state.focused_session_name()))
            .cyan()
            .bold()
            .to_string(),
        String::new(),
    ];
    let prompt = style("❯").red().bold();
    // Split at the caret so ←/→/Home/End move a visible caret through the prompt.
    let (before, after) = state.focus_prompt().split_at(state.focus_prompt_cursor());
    let before = style(before).cyan();
    let after = style(after).cyan();
    lines.push(clip_to_width(
        &format!("{prompt} {before}{CARET}{after}"),
        width,
    ));
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
/// live-terminal re-attach; a session with no live shell previews its 在席 action
/// menu. The header line carries the session's status and agent state.
pub(super) fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // Identify the highlighted row. `selected()` is `Some` for a real session
    // row and `None` on the root row (which carries no git status / tracked
    // path), so the match doubles as the root/session split.
    let (name, live, running, waiting, done, status) = match state.list().selected() {
        Some(w) => (
            w.branch.as_deref().unwrap_or(DETACHED).to_string(),
            state.is_live(&w.path),
            state.is_running(&w.path),
            state.is_waiting(&w.path),
            state.is_done(&w.path),
            Some(w.status),
        ),
        None => (ROOT_NAME.to_string(), false, false, false, false, None),
    };

    // Header: the name, then either the git status + agent state (a session) or
    // the workspace-root note (the root row).
    let mut header = style(clip_to_width(&name, width)).cyan().bold().to_string();
    match status {
        Some(status) => {
            header.push_str(&format!("   {}", status_label(status)));
            if let Some(agent) = AgentState::from_flags(live, running, waiting, done).detail(width)
            {
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
        // Selecting opens 在席 on this session: preview its action surface, which
        // mirrors the configured Session Action UI — a command menu or a prompt —
        // so the preview matches what focusing actually reveals.
        match state.session_action_ui() {
            SessionActionUi::Menu => {
                lines.push(style("Run a command:").dim().to_string());
                for (i, info) in state.focus_menu_commands().iter().enumerate() {
                    lines.push(focus_menu_row(info, i == 0, width));
                }
            }
            SessionActionUi::Prompt => {
                let prompt = style("❯").red().bold();
                lines.push(clip_to_width(&format!("{prompt} {CARET}"), width));
            }
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
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
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
