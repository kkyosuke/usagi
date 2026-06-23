//! The two-pane body: the worktree list (left) and the mode-dependent right
//! pane (a switch preview, the focus menu/prompt, or the embedded terminal).
//! All functions take plain data and return styled lines.

use std::collections::HashSet;
use std::path::PathBuf;

use console::{style, Style};

use super::super::command::{CommandInfo, Hint};
use super::super::state::{
    CreateInput, HomeState, LineKind, LogLine, Mode, Preview, RenameInput, WorktreeList, ROOT_NAME,
};
use super::super::terminal_tabs::TabStrip;
use super::super::terminal_view::TerminalView;
use super::{
    clip_to_width, pad_to_width, ACTIVE_COL, DETACHED, DIRTY_ICON, EMPTY_MESSAGE, HINT_INDENT,
    HINT_MAX, LOCAL_ICON, NAME_PREFIX, NEW_ICON, PUSHED_ICON, RAIL_WIDTH, ROOT_DETAIL, STATUS_COL,
    SYNCED_ICON, TERMINAL_STARTING,
};
use crate::domain::settings::{SessionActionUi, Sidebar};
use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Span, SpanStyle};
use crate::presentation::tui::widgets;

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

/// The colour each branch lifecycle status is drawn in, shared by the full
/// sidebar's `<icon> <word>` label and the rail's single status glyph so they
/// never drift apart.
fn status_style(status: BranchStatus) -> Style {
    match status {
        BranchStatus::New => Style::new().blue(),
        BranchStatus::Dirty => Style::new().magenta(),
        BranchStatus::Local => Style::new().yellow(),
        BranchStatus::Pushed => Style::new().green(),
        BranchStatus::Synced => Style::new().cyan(),
    }
}

/// The colour-coded `<icon> <word>` label for a branch's lifecycle status. The
/// icon gives an at-a-glance read; the word keeps it legible without a Nerd
/// Font and disambiguates the colour.
pub(super) fn status_label(status: BranchStatus) -> String {
    let text = format!("{} {}", status_icon(status), status.as_str());
    status_style(status).apply_to(text).to_string()
}

/// The single colour-coded git-status glyph shown on the collapsed rail (the icon
/// from [`status_label`] without the word), in the same colour.
fn rail_status_glyph(status: BranchStatus) -> String {
    status_style(status)
        .apply_to(status_icon(status))
        .to_string()
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

    /// The single, colour-matched glyph for the collapsed rail — the same icon
    /// the [`detail`](Self::detail) label leads with (`☾`/`▶`/`◆`/`✓`), or `None`
    /// when no agent is in use (the rail then falls back to the worktree's kind
    /// dot, so the row is never blank).
    fn rail_icon(self) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(style("☾").dim().to_string()),
            AgentState::Running => Some(style("▶").green().bold().to_string()),
            AgentState::Waiting => Some(style("◆").yellow().bold().to_string()),
            AgentState::Done => Some(style("✓").cyan().bold().to_string()),
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
    label: &str,
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
    // The session's sidebar label (its custom display name, or the branch when
    // unset); a detached worktree with no label falls back to the placeholder.
    let name = if label.is_empty() {
        worktree.branch.as_deref().unwrap_or(DETACHED)
    } else {
        label
    };
    let branch = name_cell(name, name_width, active || selected);
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

/// The worktree's kind dot — primary (`●`, magenta) or ordinary (`○`, dim).
fn kind_dot(worktree: &WorktreeState) -> String {
    if worktree.primary {
        style("●").magenta().to_string()
    } else {
        style("○").dim().to_string()
    }
}

/// Builds one collapsed-rail **entry** as the same two lines a full-sidebar entry
/// spans, so toggling the sidebar never moves a session to a different row (no
/// layout shift) — only the width changes. The glyphs form a 2×2 grid beside the
/// gutter:
///
/// ```text
/// ▎ <kind> <git>     row 1: identity dot (⌂/●/○) + git-status glyph
/// ▎       <agent>    row 2: agent-state glyph (▶/◆/☾/✓), under the git column
/// ```
///
/// `git` is blank on the root (no git status); `agent` is blank when no agent is
/// in use. The active `▎` bar runs down both rows; the 切替 `>` cursor stays a
/// point on row 1, matching the full sidebar.
fn rail_entry(
    selected: bool,
    active: bool,
    in_switch: bool,
    kind: &str,
    git: Option<&str>,
    agent: Option<&str>,
) -> (String, String) {
    let gutter = gutter_cell(selected, active, in_switch);
    let bar = gutter_cell(false, active, in_switch);
    // Columns: gutter @0, kind @2, git/agent @4 — so the agent glyph sits under
    // the git glyph and the column under the kind dot stays blank.
    let top = pad_to_width(
        format!("{gutter} {kind} {}", git.unwrap_or(" ")),
        RAIL_WIDTH,
    );
    let detail = pad_to_width(format!("{bar}   {}", agent.unwrap_or(" ")), RAIL_WIDTH);
    (top, detail)
}

/// Builds the collapsed-rail sidebar ([`Sidebar::Rail`]): the root entry first, a
/// divider, then one entry per worktree — each the same two rows as the full
/// sidebar (kind glyph on row 1, agent state on row 2), so the rail and the full
/// list share the exact same row layout and toggling between them only changes
/// the width. The active session keeps its green `▎` gutter bar (down both rows)
/// and, in 切替, the `>` cursor and the dimming of the other entries, so the rail
/// still shows which session is selected, its git state, and what its agent is
/// doing without spelling out their names.
#[allow(clippy::too_many_arguments)]
fn rail_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
    rows: usize,
    in_switch: bool,
) -> Vec<String> {
    let root_glyph = style("⌂").magenta().to_string();
    let (mut root_top, mut root_detail) = rail_entry(
        list.root_selected(),
        list.root_active(),
        in_switch,
        &root_glyph,
        None,
        None,
    );
    if in_switch && !list.root_selected() {
        root_top = dim_row(&root_top);
        root_detail = dim_row(&root_detail);
    }
    let mut lines = vec![root_top, root_detail];
    lines.push(style("─".repeat(RAIL_WIDTH)).dim().to_string());
    for (i, w) in list.worktrees().iter().enumerate() {
        // The root occupies the first entry, so worktree `i` sits at selectable
        // row i + 1.
        let row = i + 1;
        let selected = row == list.selected_index();
        let active = row == list.active_index();
        let kind = kind_dot(w);
        let git = rail_status_glyph(w.status);
        let agent = AgentState::from_flags(
            live.contains(&w.path),
            running.contains(&w.path),
            waiting.contains(&w.path),
            done.contains(&w.path),
        )
        .rail_icon();
        let (mut top, mut detail) = rail_entry(
            selected,
            active,
            in_switch,
            &kind,
            Some(&git),
            agent.as_deref(),
        );
        if in_switch && !selected {
            top = dim_row(&top);
            detail = dim_row(&detail);
        }
        lines.push(top);
        lines.push(detail);
    }
    if list.is_empty() {
        // Mirror the full sidebar's single empty-message row so the row count
        // matches and toggling never shifts the layout.
        lines.push(pad_to_width(String::new(), RAIL_WIDTH));
    }
    lines.truncate(rows);
    lines
}

/// Re-renders an already-styled row uniformly dimmed: strips its colours and
/// wraps the plain text in `dim`. Used to fade the rows the cursor is *not* on
/// in 切替 (Switch), so the highlighted session stands out without a box.
pub(super) fn dim_row(line: &str) -> String {
    // `strip_ansi_codes` borrows the input when it carries no escapes (the common
    // case for a plain session row), so styling the `Cow` directly avoids the
    // extra owned copy `into_owned` would force before the single styled string is
    // built.
    style(console::strip_ansi_codes(line)).dim().to_string()
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
///
/// When `sidebar` is [`Sidebar::Rail`] the list collapses to the compact rail
/// ([`rail_pane`]) instead, and `left_w` is the rail width.
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
    sidebar: Sidebar,
) -> Vec<String> {
    if sidebar == Sidebar::Rail {
        return rail_pane(list, live, running, waiting, done, rows, in_switch);
    }
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
                list.display_label(i),
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

/// Builds the tab strip's two raw (unclipped) rows: one ` N label ` chip per
/// pane (the active one reversed and bold, the rest dimmed) and the underline
/// marker beneath the active chip. Each chip is numbered (1-based) to match the
/// `←`/`→` tab order. The rows are laid beside the preview header on a shared row
/// by [`header_tab_rows`], which re-indents the marker to stay under the chips.
fn tab_strip_parts(strip: &TabStrip) -> (String, String) {
    // Gap between chips on the top row (and under it on the marker row), so the
    // chips read as separate tabs without a hard separator glyph.
    const GAP: &str = "  ";
    let mut chips = String::new();
    let mut marker = String::new();
    for (i, label) in strip.labels.iter().enumerate() {
        if i > 0 {
            chips.push_str(GAP);
            marker.push_str(&" ".repeat(GAP.chars().count()));
        }
        let text = format!(" {} {label} ", i + 1);
        let width = text.chars().count();
        if i == strip.active {
            chips.push_str(&style(&text).reverse().bold().to_string());
            marker.push_str(&style("▔".repeat(width)).cyan().bold().to_string());
        } else {
            chips.push_str(&style(&text).dim().to_string());
            marker.push_str(&" ".repeat(width));
        }
    }
    (chips, marker)
}

/// The divider drawn between the fixed-width header identity and the tab strip,
/// so the session's identity (name / status / agent) reads as a distinct block
/// from its tabs. It reuses the pane divider glyph ([`SEP`](super::SEP)), dimmed.
const HEADER_TAB_DIVIDER: &str = " │ ";

/// Lays the preview `header` (the fixed-width identity from [`preview_header`])
/// and the pane tab strip on a single row: the identity, a dim divider, then the
/// numbered chips, with the active-tab underline marker on the row below
/// re-indented to sit under the chips. Because the identity is a constant width,
/// the divider and the chips land in the same column whichever session is shown,
/// so the row does not jitter as the 切替 cursor moves between sessions. With no
/// `strip` (or an empty one) the identity stands alone on one row. Used by both
/// the 切替 (Switch) preview and 没入 (Attached).
pub(super) fn header_tab_rows(
    header: String,
    strip: Option<&TabStrip>,
    width: usize,
) -> Vec<String> {
    let Some(strip) = strip.filter(|s| !s.labels.is_empty()) else {
        return vec![clip_to_width(&header, width)];
    };
    let (chips, marker) = tab_strip_parts(strip);
    let divider = style(HEADER_TAB_DIVIDER).dim().to_string();
    // Push the marker right past the identity and the divider so it lands under
    // the chips on the row above. The identity is a fixed width, so this indent
    // is the same for every session.
    let indent = console::measure_text_width(&header) + HEADER_TAB_DIVIDER.chars().count();
    vec![
        clip_to_width(&format!("{header}{divider}{chips}"), width),
        clip_to_width(&format!("{}{marker}", " ".repeat(indent)), width),
    ]
}

/// Column widths for the fixed-width header identity. The session name is clipped
/// and every field is padded to a constant width, so the identity block is the
/// same size for every session: a long name or status is clipped (with an
/// ellipsis) instead of shoving the divider and tabs sideways, and the 切替
/// preview does not jitter as the cursor moves between sessions.
const HEADER_NAME_COL: usize = 16;
const HEADER_AGENT_COL: usize = 9;
/// The status + agent block that follows the name: the right-edge status field
/// ([`STATUS_COL`]) and the agent label, with a two-space gap between them.
const HEADER_DETAIL_COL: usize = STATUS_COL + 2 + HEADER_AGENT_COL;

/// Builds the fixed-width right-pane header identity shown above a session's
/// preview / terminal: the session `name` (cyan, bold, clipped to
/// [`HEADER_NAME_COL`]), then either its git `status` label and `agent` state (a
/// real session) or the workspace-root note (the root row, no status). Each field
/// is padded to a constant width so the block is the same size for every session.
/// Shared by 切替 (Switch) and 没入 (Attached) so both carry the same identity.
fn preview_header(name: &str, status: Option<BranchStatus>, agent: Option<String>) -> String {
    let name = pad_to_width(
        style(clip_to_width(name, HEADER_NAME_COL))
            .cyan()
            .bold()
            .to_string(),
        HEADER_NAME_COL,
    );
    let detail = match status {
        Some(status) => {
            let status = pad_to_width(status_label(status), STATUS_COL);
            let agent = pad_to_width(agent.unwrap_or_default(), HEADER_AGENT_COL);
            format!("{status}  {agent}")
        }
        // The root row carries no git status / agent, only its note — clipped to
        // the same detail width so the identity block stays a constant size.
        None => pad_to_width(
            style(clip_to_width(ROOT_DETAIL, HEADER_DETAIL_COL))
                .dim()
                .to_string(),
            HEADER_DETAIL_COL,
        ),
    };
    format!("{name}  {detail}")
}

/// The header line for the active (focused) session: its name, git status, and
/// agent state — or the workspace-root note when the root row is active. Shared
/// by 没入 (Attached), where it sits above the embedded terminal, and 在席
/// (Focus), where it sits above the session's pane tabs.
fn active_session_header(state: &HomeState) -> String {
    match state.list().active() {
        Some(w) => {
            let agent = AgentState::from_flags(
                state.is_live(&w.path),
                state.is_running(&w.path),
                state.is_waiting(&w.path),
                state.is_done(&w.path),
            )
            .detail(HEADER_AGENT_COL);
            preview_header(
                w.branch.as_deref().unwrap_or(DETACHED),
                Some(w.status),
                agent,
            )
        }
        None => preview_header(ROOT_NAME, None, None),
    }
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

/// The `session: <name>` header line shown above the 在席 (Focus) action surface
/// when the session has no live panes (an idle session, no tab strip). With live
/// panes the identity rides the tab strip ([`active_session_header`]) instead.
fn focus_session_header(state: &HomeState) -> String {
    style(format!("session: {}", state.focused_session_name()))
        .cyan()
        .bold()
        .to_string()
}

/// The body of the 在席 (Focus) menu (no identity header): the `Run a command:`
/// label, one row per Session-scope command (`›` cursor on the highlighted one),
/// and a key hint. Shared by the idle-session [`focus_menu`] and the "+ new" tab.
fn focus_menu_body(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![style("Run a command:").dim().to_string()];
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

/// The body of the 在席 (Focus) prompt surface (no identity header): the
/// session-scoped command line (`❯ <input>▏`) and the Session-scope hint below
/// it. Shared by the idle-session [`focus_prompt`] and the "+ new" tab.
fn focus_prompt_body(state: &HomeState, width: usize) -> Vec<String> {
    let prompt = style("❯").red().bold();
    // Split at the caret so ←/→/Home/End move a visible block caret through the prompt.
    let (before, after) = state.focus_prompt().split_at(state.focus_prompt_cursor());
    let value = widgets::block_caret(before, after, &Style::new().cyan());
    let mut lines = vec![clip_to_width(&format!("{prompt} {value}"), width)];
    lines.push(String::new());
    lines.extend(focus_hint_lines(state.focus_prompt_hint(), width));
    lines
}

/// Builds the 在席 (Focus) menu: a short header, one row per Session-scope
/// command (`›` cursor on the highlighted one), and a key hint.
pub(super) fn focus_menu(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![focus_session_header(state), String::new()];
    lines.extend(focus_menu_body(state, width));
    lines
}

/// Builds the 在席 (Focus) prompt surface: a header, the session-scoped command
/// line (`❯ <input>▏`), and the Session-scope hint below it.
pub(super) fn focus_prompt(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![focus_session_header(state), String::new()];
    lines.extend(focus_prompt_body(state, width));
    lines
}

/// The label of 在席's trailing "+ new" tab — the action surface that launches a
/// pane. ASCII so the underline marker in [`tab_strip_parts`] (which measures
/// width in `chars`) lands exactly under it, as it does for the pane labels.
const FOCUS_NEW_TAB_LABEL: &str = "+ new";

/// Builds the 在席 (Focus) right pane. With no live panes it is the session's
/// action surface alone — the menu or prompt with its own `session:` header,
/// exactly as before. With live panes it gains a **tab strip**: one chip per live
/// pane followed by a "+ new" chip, the session identity beside it (shared with
/// 没入), and below it either the selected pane's live preview or — on the "+ new"
/// tab — the action surface (header-less, the identity already rides the strip).
fn focus_pane(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // No live panes: the action surface fills the pane, just as it did before
    // tabs existed (its own `session:` header, no strip).
    let Some(strip) = state.terminal_tabs().filter(|s| !s.labels.is_empty()) else {
        let mut lines = match state.session_action_ui() {
            SessionActionUi::Menu => focus_menu(state, width),
            SessionActionUi::Prompt => focus_prompt(state, width),
        };
        lines.truncate(rows);
        lines.resize(rows, String::new());
        return lines;
    };

    // Live panes: the session's panes as tabs, then a trailing "+ new" tab. The
    // identity rides the strip's row (as in 没入), so the body below carries no
    // header of its own.
    let on_new = state.focus_on_new_tab();
    let mut labels = strip.labels.clone();
    labels.push(FOCUS_NEW_TAB_LABEL.to_string());
    let active = if on_new {
        labels.len() - 1
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let mut lines = header_tab_rows(header, Some(&combined), width);

    if on_new {
        // The "+ new" tab: the action surface that launches the next pane.
        lines.push(String::new());
        match state.session_action_ui() {
            SessionActionUi::Menu => lines.extend(focus_menu_body(state, width)),
            SessionActionUi::Prompt => lines.extend(focus_prompt_body(state, width)),
        }
    } else {
        // A pane tab: preview the pane's live screen (the snapshot taken before
        // painting), so the selection shows what re-attaching reveals. Fall back
        // to a label until the first snapshot is available.
        match state.terminal_view() {
            Some(view) => {
                let body = rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").green().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    }

    lines.truncate(rows);
    lines.resize(rows, String::new());
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

/// Pad `lines` to fill the right pane and pin `hint` to its bottom row. Shared by
/// the rail's create / rename right-pane inputs, whose `Enter` / `Esc` hint stays
/// in view beneath the input.
fn switch_input_pane(mut lines: Vec<String>, hint: &str, width: usize, rows: usize) -> Vec<String> {
    let body_rows = rows.saturating_sub(1);
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines.push(style(clip_to_width(hint, width)).dim().to_string());
    lines
}

/// The 切替 (Switch) name input rendered in the **right pane** while creating a
/// session with the sidebar collapsed to the rail: a header, the typed name in a
/// bordered box with a block caret, the live validation error (or a hint) below
/// it, and the key hint pinned to the bottom row. At full width the input rides
/// the left pane inline instead (see [`super::switch_create_rows`]).
fn switch_create_pane(create: &CreateInput, width: usize, rows: usize) -> Vec<String> {
    // The box draws two borders and a space of padding on each side, so its
    // content area is the pane width less those four columns.
    let inner = width.saturating_sub(4).max(1);
    let (before, after) = create.value().split_at(create.cursor());
    let value = widgets::block_caret(before, after, &Style::new().cyan());
    let mut lines = vec![style("+ new session").green().bold().to_string()];
    lines.extend(widgets::boxed("", inner, &[value]));
    // Keep the row count stable whether or not the name is currently invalid: an
    // error replaces the dim hint in place rather than adding a row.
    match create.error() {
        Some(err) => lines.push(style(clip_to_width(err, width)).red().to_string()),
        None => lines.push(
            style("空文字・重複・\"/\" は作成できません")
                .dim()
                .to_string(),
        ),
    }
    switch_input_pane(lines, "Enter 作成 / Esc 取消", width, rows)
}

/// The 切替 (Switch) display-name input rendered in the **right pane** while
/// renaming a session with the sidebar collapsed to the rail: a header naming the
/// session, the typed label in a bordered box with a block caret, a hint, and the
/// key hint pinned to the bottom row. At full width it rides the left pane inline
/// instead (see [`super::switch_rename_rows`]).
fn switch_rename_pane(rename: &RenameInput, width: usize, rows: usize) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let value = widgets::block_caret(rename.value(), "", &Style::new().cyan());
    let mut lines = vec![clip_to_width(
        &style(format!("rename {}", rename.target()))
            .cyan()
            .bold()
            .to_string(),
        width,
    )];
    lines.extend(widgets::boxed("", inner, &[value]));
    lines.push(style("空にすると既定の表示名に戻す").dim().to_string());
    switch_input_pane(lines, "Enter 確定 / Esc 取消", width, rows)
}

/// Most note lines the read-only 切替 note overlay shows before eliding the rest
/// with a `… (N more)` line — the full text lives in the editor (`n` / `Ctrl-E`).
const SWITCH_NOTE_MAX_LINES: usize = 6;

/// Most note lines the *editing* overlay shows at once, windowed around the
/// caret, so the box never hides the whole right pane while editing.
const EDIT_NOTE_MAX_LINES: usize = 12;

/// The narrowest the floating note box renders.
const NOTE_BOX_MIN_WIDTH: usize = 28;
/// The widest the floating note box renders — past this the box stops growing so
/// it stays a top-right column rather than swallowing a wide right pane.
const NOTE_BOX_MAX_WIDTH: usize = 48;
/// Columns kept clear to the left of the box (so the preview underneath — the
/// session header, the live terminal — stays readable beside it) when the pane
/// is wide enough to spare them.
const NOTE_PREVIEW_KEEP: usize = 12;

/// Width of the floating note box for a right pane `pane_w` columns wide: capped
/// to [`NOTE_BOX_MAX_WIDTH`] and kept a [`NOTE_PREVIEW_KEEP`]-column margin off
/// the pane's left so it reads as a top-right column over the preview. On a pane
/// too narrow to spare that margin it floors at [`NOTE_BOX_MIN_WIDTH`] (clamped
/// to the pane), and on a pane narrower still it takes the whole row.
fn note_box_width(pane_w: usize) -> usize {
    pane_w
        .saturating_sub(NOTE_PREVIEW_KEEP)
        .clamp(NOTE_BOX_MIN_WIDTH, NOTE_BOX_MAX_WIDTH)
        .min(pane_w)
}

/// Build the floating `note: <name>` box overlaid on the right pane. With `caret`
/// set it is the **editor** (a block caret on the cursor line, the view windowed
/// around it); with `None` it is the **read-only** note (capped, the overflow
/// elided with `… (N more)`). `max` caps the body so the box always leaves part of
/// the right pane visible underneath. Returned rows are the bordered box.
fn note_box(
    name: &str,
    lines: &[String],
    caret: Option<(usize, usize)>,
    width: usize,
    max: usize,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let max = max.max(1);
    let title = format!("note: {name}");
    let body: Vec<String> = match caret {
        // Read-only: the first `max` lines, then a `… (N more)` line when longer.
        None => {
            let shown = lines.len().min(max);
            let mut body = lines[..shown].to_vec();
            if lines.len() > shown {
                body.push(format!("… ({} more)", lines.len() - shown));
            }
            body
        }
        // Editing: a `max`-line window around the caret, with a block caret drawn
        // on the cursor line so editing happens where it shows.
        Some((caret_row, caret_col)) => {
            let start = caret_row.saturating_sub(max.saturating_sub(1));
            let base = Style::new();
            lines
                .iter()
                .enumerate()
                .skip(start)
                .take(max)
                .map(|(i, line)| {
                    if i == caret_row {
                        let (before, after) = line.split_at(caret_col);
                        widgets::block_caret(before, after, &base)
                    } else {
                        line.clone()
                    }
                })
                .collect()
        }
    };
    // `boxed` clips each line (and the block-caret one, ANSI included) to `inner`.
    widgets::boxed(&title, inner, &body)
}

/// The floating note overlay for the right pane, or `None` when none applies. The
/// **editor** (when open, in any mode) wins; otherwise the highlighted session's
/// **read-only** note shows while browsing in 切替 (until `Esc` dismisses it, see
/// [`HomeState::switch_note_visible`]). The box is a narrow top-right column (see
/// [`note_box_width`]) composited over the pane by [`right_pane_contents`], so
/// the preview underneath — the session header, the live terminal — stays
/// readable to its left and below it. `rows` caps the box height so the pane
/// stays partly visible behind it; `width` is the full right-pane width (the box
/// narrows itself within it).
fn note_overlay(state: &HomeState, width: usize, rows: usize) -> Option<Vec<String>> {
    let box_w = note_box_width(width);
    if let Some(editor) = state.note_editor() {
        let cap = EDIT_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        return Some(note_box(
            editor.target(),
            editor.area().lines(),
            Some(editor.area().cursor()),
            box_w,
            cap,
        ));
    }
    if state.switch_note_visible() {
        if let Some(note) = state.selected_session_note() {
            let name = state
                .list()
                .selected()
                .and_then(|w| w.branch.as_deref())
                .unwrap_or(DETACHED)
                .to_string();
            let cap = SWITCH_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
            let note_lines: Vec<String> = note.lines().map(str::to_string).collect();
            return Some(note_box(&name, &note_lines, None, box_w, cap));
        }
    }
    None
}

/// The 切替 (Switch) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; a session with no live shell previews its 在席 action
/// menu. The header line carries the session's status and agent state. The key
/// hints live in the footer, so the preview uses the pane's full height. The
/// highlighted session's note is drawn over the top by [`note_overlay`] (not
/// inline), so it never pushes this preview around.
pub(super) fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    let body_rows = rows;
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
        None => {
            // The root row carries no worktree, but its embedded session is
            // keyed by the workspace root path, so match it against the same
            // live / running / waiting / done sets — otherwise a running root
            // agent never previews live here (it only re-appears once selected).
            let root = state.root_path();
            (
                ROOT_NAME.to_string(),
                state.is_live(root),
                state.is_running(root),
                state.is_waiting(root),
                state.is_done(root),
                None,
            )
        }
    };

    // Header: the name, then either the git status + agent state (a session) or
    // the workspace-root note (the root row). A live session's tabs share the
    // header's row (the `←`/`→` targets), so the identity and the tabs read
    // together on one line; the preview below mirrors the active pane.
    let agent = AgentState::from_flags(live, running, waiting, done).detail(HEADER_AGENT_COL);
    let header = preview_header(&name, status, agent);
    let mut lines = header_tab_rows(
        header,
        if live { state.terminal_tabs() } else { None },
        width,
    );

    if live {
        // Selecting re-attaches the running shell / agent: preview its actual
        // screen (the live snapshot taken before painting) so the choice shows
        // what re-attaching reveals. Fall back to a label until the first
        // snapshot is available.
        match state.terminal_view() {
            Some(view) => {
                let body = body_rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").green().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    } else {
        // Selecting opens 在席 on this session: preview its action surface, which
        // mirrors the configured Session Action UI — a command menu or a prompt —
        // so the preview matches what focusing actually reveals. A blank row keeps
        // the header clear of the surface below it.
        lines.push(String::new());
        match state.session_action_ui() {
            SessionActionUi::Menu => {
                lines.push(style("Run a command:").dim().to_string());
                for (i, info) in state.focus_menu_commands().iter().enumerate() {
                    lines.push(focus_menu_row(info, i == 0, width));
                }
            }
            SessionActionUi::Prompt => {
                let prompt = style("❯").red().bold();
                let value = widgets::block_caret("", "", &Style::new().cyan());
                lines.push(clip_to_width(&format!("{prompt} {value}"), width));
            }
        }
        lines.push(String::new());
        lines.push(style("Enter で開く").dim().to_string());
    }

    // Trim the body to its budget and pad up so the pane is always full-height.
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines
}

/// The right pane's contents, by mode. Blank in 統括 (the user is on the command
/// line); a preview of the would-be session screen in 切替; the session's action
/// surface — a menu or a prompt, per [`SessionActionUi`] — in 在席; and the live
/// embedded terminal in 没入 (a starting hint until the first snapshot arrives).
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    // The Markdown preview, when open, takes over the right pane regardless of
    // mode (it is opened from 統括 and captures the keyboard while shown).
    if let Some(preview) = state.preview() {
        return preview_pane(preview, right_w, rows);
    }
    // The base pane for the current mode. The session-note overlay (the editor,
    // or the read-only note while browsing in 切替) is composited over its top
    // below, so editing / reading the note never switches the screen — the
    // preview / terminal stays visible behind the floating box.
    let mut base = match state.mode() {
        Mode::Overview => Vec::new(),
        Mode::Switch => {
            // Collapsed to the rail, 切替's name input has no room inline in the
            // (5-column) list, so it takes over the wide right pane; at full width
            // it rides the left pane inline and the right pane keeps previewing the
            // highlighted session.
            if state.sidebar() == Sidebar::Rail {
                if let Some(create) = state.create() {
                    return switch_create_pane(create, right_w, rows);
                }
                if let Some(rename) = state.rename() {
                    return switch_rename_pane(rename, right_w, rows);
                }
            }
            switch_preview(state, right_w, rows)
        }
        Mode::Focus => focus_pane(state, right_w, rows),
        Mode::Attached => {
            // The active session's identity shares the top row with its tab chips
            // (the underline marker below them), so the header reads beside the
            // tabs just as it does in 切替. This header + tab block always fills
            // exactly `TAB_BAR_ROWS`, matching `attached_geometry`, so the embedded
            // terminal below never shifts whether or not a strip is published. A
            // starting hint stands in until the first screen snapshot arrives.
            let mut lines = Vec::with_capacity(rows);
            let header = active_session_header(state);
            let mut head = header_tab_rows(header, state.terminal_tabs(), right_w);
            head.resize(super::TAB_BAR_ROWS, String::new());
            lines.extend(head);
            let body = rows.saturating_sub(lines.len());
            match state.terminal_view() {
                Some(view) => lines.extend(terminal_pane(view, right_w, body)),
                None => lines.push(
                    style(clip_to_width(TERMINAL_STARTING, right_w))
                        .dim()
                        .to_string(),
                ),
            }
            lines
        }
    };
    // Composite the floating note box onto the top-right of the base pane: only
    // the box's own columns are overwritten, so the preview / terminal to its
    // left and below stays put (no CLS), and the session header on the top row
    // keeps its leading columns beside the box.
    if let Some(overlay) = note_overlay(state, right_w, rows) {
        // Grow the base to hold the whole box when the pane beneath is shorter
        // than it (no live snapshot yet, or a partial one), so the box's bottom
        // border always lands instead of being clipped as the note grows with
        // each newline. Rows the box does not cover are left blank.
        if overlay.len() > base.len() {
            base.resize(overlay.len(), String::new());
        }
        widgets::overlay_right(&mut base, 0, right_w, &overlay);
    }
    base
}

/// Render the right-pane Markdown preview: a one-row header (the file path, plus
/// a `start-end/total` position once it scrolls) over a window of rendered
/// Markdown lines. The window is clamped so the last line stays in view, matching
/// the event loop's scroll clamp, and each row is clipped to the pane width — so
/// the preview never overruns the pane or shifts the layout (no CLS).
pub(super) fn preview_pane(preview: &Preview, width: usize, rows: usize) -> Vec<String> {
    let total = preview.lines.len();
    let body_h = rows.saturating_sub(1);
    let max_start = total.saturating_sub(body_h);
    let start = preview.scroll.min(max_start);
    let end = (start + body_h).min(total);

    let header = if total > body_h {
        format!("📄 {}  ({}-{}/{})", preview.title, start + 1, end, total)
    } else {
        format!("📄 {}", preview.title)
    };

    let mut lines = Vec::with_capacity(rows);
    lines.push(style(clip_to_width(&header, width)).bold().to_string());
    // A one-row pane (or none) shows just the header.
    if rows <= 1 {
        lines.truncate(rows);
        return lines;
    }
    for i in 0..body_h {
        match preview.lines.get(start + i) {
            Some(line) => lines.push(markdown_row(line, width)),
            None => lines.push(String::new()),
        }
    }
    lines
}

/// Render one [`MarkdownLine`] to a styled, width-clipped row: its prefix marker
/// coloured by block kind, then its inline spans styled by emphasis.
fn markdown_row(line: &MarkdownLine, width: usize) -> String {
    let mut out = String::new();
    if !line.prefix.is_empty() {
        let prefix = match line.style {
            LineStyle::Bullet | LineStyle::Number => style(&line.prefix).cyan().to_string(),
            LineStyle::Quote => style(&line.prefix).dim().to_string(),
            _ => line.prefix.clone(),
        };
        out.push_str(&prefix);
    }
    for span in &line.spans {
        out.push_str(&styled_span(span, line.style));
    }
    clip_to_width(&out, width)
}

/// Style one inline [`Span`] for terminal display. A heading colours its whole
/// content by level; a code-block line and a quote line take a uniform style;
/// every other line styles each span by its own inline emphasis.
fn styled_span(span: &Span, line_style: LineStyle) -> String {
    let text = span.text.as_str();
    match line_style {
        LineStyle::Heading(level) => heading_style(text, level),
        LineStyle::Code => style(text).green().to_string(),
        LineStyle::Quote => style(text).dim().italic().to_string(),
        _ => match span.style {
            SpanStyle::Plain => text.to_string(),
            SpanStyle::Strong => style(text).bold().to_string(),
            SpanStyle::Emphasis => style(text).italic().to_string(),
            SpanStyle::Code => style(text).green().to_string(),
            SpanStyle::Link => style(text).blue().underlined().to_string(),
        },
    }
}

/// The bold, level-coloured styling of a heading's text: magenta (h1), cyan (h2),
/// yellow (h3), and plain bold for deeper levels.
fn heading_style(text: &str, level: u8) -> String {
    let base = style(text).bold();
    match level {
        1 => base.magenta(),
        2 => base.cyan(),
        3 => base.yellow(),
        _ => base,
    }
    .to_string()
}
