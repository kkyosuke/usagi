//! Rendering for the focus-mode action menu and command prompt.

use console::{style, Style};

use super::super::command::{CommandInfo, Hint};
use super::super::state::HomeState;
use super::{clip_to_width, HINT_INDENT, HINT_MAX};
use crate::domain::settings::AgentCli;
use crate::presentation::theme::Palette;
use crate::presentation::tui::widgets;

pub(super) fn menu_marker(selected: bool) -> String {
    if selected {
        style("›").danger().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// Builds one 集中 (Closeup) menu row: a `›` cursor for the highlighted command,
/// its name, and its dimmed description, clipped to `width`.
pub(super) fn closeup_menu_row(info: &CommandInfo, selected: bool, width: usize) -> String {
    menu_row(info.name, info.description, selected, width)
}

/// The shared layout for an action row: a `›` cursor when `selected`, a fixed-
/// width cyan `name`, and a dimmed `desc`, clipped to `width`. Used by the plain
/// command rows ([`closeup_menu_row`]) and the `agent` row, which substitutes a
/// "Launch <default>" description and an expand chevron.
pub(super) fn menu_row(name: &str, desc: &str, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{name:<9}")).accent().bold().to_string()
    } else {
        style(format!("{name:<9}")).accent().to_string()
    };
    let desc_budget = width.saturating_sub(HINT_INDENT + 9);
    let desc = style(clip_to_width(desc, desc_budget)).dim();
    clip_to_width(&format!("  {marker} {name}{desc}"), width)
}

/// The 集中 menu's `agent` row: like a plain command row but its description
/// names the agent a plain launch uses (the configured default) and carries an
/// expand affordance — `▾` while the picker is open, `▸` when the cursor is on
/// this row and it can open (more than one CLI installed). When more than one
/// CLI is installed but the cursor is elsewhere the slot is held with blanks so
/// the description never shifts as the cursor moves on/off the row; with a
/// single CLI (the chevron can never show) no slot is reserved.
pub(super) fn closeup_agent_command_row(state: &HomeState, selected: bool, width: usize) -> String {
    let chevron = if state.closeup_menu_agent_cursor().is_some() {
        "▾ "
    } else if state.closeup_menu_agent_can_expand() {
        "▸ "
    } else if state.installed_agents().len() > 1 {
        "  "
    } else {
        ""
    };
    let desc = format!("{chevron}Launch {}", state.default_agent().display_name());
    menu_row("agent", &desc, selected, width)
}

/// The 集中 menu's `terminal` row: like a plain command row but it can expand
/// into the `open` / `new` picker. `open` is the default and preserves the
/// existing embedded-tab behaviour. The row always reserves the same 2-column
/// chevron slot as `agent` and `close` so descriptions never shift (no CLS).
pub(super) fn closeup_terminal_command_row(
    state: &HomeState,
    selected: bool,
    width: usize,
) -> String {
    let chevron = if state.closeup_menu_terminal_expanded() {
        "▾ "
    } else if state.closeup_menu_terminal_can_expand() {
        "▸ "
    } else {
        "  "
    };
    let desc = format!("{chevron}Open a shell");
    menu_row("terminal", &desc, selected, width)
}

/// One agent-picker sub-row, indented under the expanded `agent` row: a `›`
/// cursor on the highlighted CLI, its display name, and a dimmed `(default)` tag
/// on the configured agent.
pub(super) fn closeup_agent_pick_row(
    cli: AgentCli,
    selected: bool,
    is_default: bool,
    width: usize,
) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{:<10}", cli.display_name()))
            .accent()
            .bold()
            .to_string()
    } else {
        style(format!("{:<10}", cli.display_name()))
            .accent()
            .to_string()
    };
    let tag = if is_default {
        style("(default)").dim().to_string()
    } else {
        String::new()
    };
    clip_to_width(&format!("      {marker} {name}{tag}"), width)
}

/// The 集中 menu's `close` row: like a plain command row but carries a `▾`/`▸`
/// expand affordance to open the close picker (plain close vs. close --force) —
/// `▾` while the picker is open, `▸` when the cursor is on this row (it can
/// always expand). When the cursor is elsewhere the slot is held with blanks so
/// the description never shifts as the cursor moves on/off the row (no CLS),
/// mirroring the `agent` row.
pub(super) fn closeup_close_command_row(
    state: &HomeState,
    info: &CommandInfo,
    selected: bool,
    width: usize,
) -> String {
    let chevron = if state.closeup_close_expanded() {
        "▾ "
    } else if state.closeup_close_can_expand() {
        "▸ "
    } else {
        "  "
    };
    let desc = format!("{chevron}{}", info.description);
    menu_row(info.name, &desc, selected, width)
}

/// One close-picker sub-row, indented under the expanded `close` row: a `›`
/// cursor on the highlighted option, the command label, and a dimmed hint.
/// `force = false` → plain close; `force = true` → close --force.
pub(super) fn closeup_close_pick_row(force: bool, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let label = if force { "close --force" } else { "close" };
    let hint = if force {
        "(discard uncommitted changes)"
    } else {
        "(safe)"
    };
    let name = if selected {
        style(format!("{label:<14}")).cyan().bold().to_string()
    } else {
        style(format!("{label:<14}")).cyan().to_string()
    };
    let hint = style(hint).dim();
    clip_to_width(&format!("      {marker} {name}{hint}"), width)
}

/// One terminal-picker sub-row, indented under the expanded `terminal` row:
/// `open` adds an embedded usagi tab (the default), while `new` opens a native
/// terminal app rooted at the same directory.
pub(super) fn closeup_terminal_pick_row(action: &str, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{action:<10}")).cyan().bold().to_string()
    } else {
        style(format!("{action:<10}")).cyan().to_string()
    };
    let desc = if action == "new" {
        "new terminal"
    } else {
        "add tab"
    };
    let tag = if action == "open" {
        style("(default)").dim().to_string()
    } else {
        style(desc).dim().to_string()
    };
    clip_to_width(&format!("      {marker} {name}{tag}"), width)
}

/// A hard floor on the 集中 (Closeup) menu's command-area height, so even a tiny
/// menu keeps a usable window. The real target is the widest expansion (see
/// [`closeup_menu_target`]); this only guards degenerate menus with fewer rows.
pub(super) const FOCUS_MENU_MIN_VISIBLE: usize = 5;

/// The 集中 menu box's chrome rows around the windowed command area: the two box
/// borders plus the `Run a command:` label, the blank spacer, and the key hint.
/// The command window may take up to `avail_rows - FOCUS_MENU_CHROME` rows before
/// the box would overrun the right pane.
pub(super) const FOCUS_MENU_CHROME: usize = 5;

/// The command-area height the 集中 menu reserves: the row count with the *most
/// sub-menu-heavy* picker fully expanded — the command rows plus the largest
/// picker's sub-rows. Fixing the window to this height means every picker opens in
/// place with no `↑/↓ N more` clipping and no layout shift, whatever is expanded.
/// Each command contributes its own picker's sub-row count when it can expand:
/// `agent` the installed CLIs (only when more than one, so a picker actually
/// opens), `terminal` its open/new actions, `close` its two options.
pub(super) fn closeup_menu_target(state: &HomeState, commands: &[CommandInfo]) -> usize {
    let widest_picker = commands
        .iter()
        .map(|info| match info.name {
            "agent" if state.installed_agents().len() > 1 => state.installed_agents().len(),
            "terminal" => state.closeup_menu_terminal_actions().len(),
            "close" => 2,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    commands.len() + widest_picker
}

/// How many command rows the 集中 menu window shows for a pane `avail_rows` tall,
/// given the `target` height (the widest expansion, see [`closeup_menu_target`]).
/// It reserves the full `target` — so every picker opens without scrolling and the
/// box never resizes as pickers open and close — capping only when the pane cannot
/// hold it (then the window scrolls), and never below [`FOCUS_MENU_MIN_VISIBLE`].
pub(super) fn closeup_menu_visible(target: usize, avail_rows: usize) -> usize {
    let max_fit = avail_rows.saturating_sub(FOCUS_MENU_CHROME);
    target.min(max_fit).max(FOCUS_MENU_MIN_VISIBLE)
}

/// Windows the 集中 menu's command `rows` to exactly `visible` output rows,
/// scrolled so the `active` row (the cursor, or the highlighted picker sub-row)
/// stays on screen. Rows hidden past an edge are summarised with a dim `↑ N` /
/// `↓ N` marker on the window's top / bottom row; rows that already fit are padded
/// with blanks. Either way the result is `visible` rows, so the box keeps the same
/// height whether or not an inline picker is expanded.
pub(super) fn closeup_menu_window(rows: Vec<String>, active: usize, visible: usize) -> Vec<String> {
    if rows.len() <= visible {
        let mut out = rows;
        out.resize(visible, String::new());
        return out;
    }
    let total = rows.len();
    // Reserve a marker row on each edge; the content window is what remains. The
    // clamp keeps `active` inside that window (centring it where there is room).
    let content = visible.saturating_sub(2).max(1);
    let start = active.saturating_sub(content / 2).min(total - content);
    let end = start + content;
    let mut out = Vec::with_capacity(visible);
    out.push(closeup_menu_overflow(start, true));
    out.extend(rows[start..end].iter().cloned());
    out.push(closeup_menu_overflow(total - end, false));
    out
}

/// A dim overflow marker for the windowed 集中 menu: `↑ N more` (`above`) or
/// `↓ N more` (below) when `hidden` rows sit past the window edge, or a blank row
/// when none do — so the window keeps its fixed height at the ends of the scroll.
pub(super) fn closeup_menu_overflow(hidden: usize, above: bool) -> String {
    if hidden == 0 {
        return String::new();
    }
    let arrow = if above { '↑' } else { '↓' };
    style(format!("  {arrow} {hidden} more")).dim().to_string()
}

/// The body of the 集中 (Closeup) menu (no identity header): the `Run a command:`
/// label, one row per Session-scope command (`›` cursor on the highlighted one),
/// and a key hint. The command rows are windowed ([`closeup_menu_window`]) to a
/// height that grows to fill the `avail_rows`-tall right pane, so a long picker
/// shows as many rows as fit before it scrolls rather than collapsing straight
/// into `↑/↓ N more` markers. Rendered as the body of the floating menu overlay
/// modal (see [`super::render_frame`] and [`HomeState::closeup_action_overlay`]); the
/// `session:` identity rides the modal's title rather than a header line here.
pub(super) fn closeup_menu_body(state: &HomeState, width: usize, avail_rows: usize) -> Vec<String> {
    let cursor = state.closeup_menu_cursor();
    let expanded = state.closeup_menu_expanded();
    let close_expanded = state.closeup_close_expanded();
    let commands = state.closeup_menu_commands();

    // Build the whole command area first — one row per command plus any expanded
    // picker's sub-rows — tracking the index of the *active* row (the cursor, or
    // the highlighted picker sub-row) so the window below can keep it on screen.
    let mut rows: Vec<String> = Vec::new();
    let mut active = 0usize;
    for (i, info) in commands.iter().enumerate() {
        let selected = i == cursor;
        if info.name == "agent" {
            // The `agent` row names the default CLI; when expanded, its installed
            // alternatives follow as indented picker sub-rows (案A).
            let agent_cursor = state.closeup_menu_agent_cursor();
            if agent_cursor.is_none() && selected {
                active = rows.len();
            }
            rows.push(closeup_agent_command_row(state, selected, width));
            if agent_cursor.is_some() {
                let default = state.default_agent();
                for (j, &cli) in state.installed_agents().iter().enumerate() {
                    if Some(j) == agent_cursor {
                        active = rows.len();
                    }
                    rows.push(closeup_agent_pick_row(
                        cli,
                        Some(j) == agent_cursor,
                        cli == default,
                        width,
                    ));
                }
            }
        } else if info.name == "close" {
            // The `close` row carries a chevron affordance; when expanded the two
            // options (plain close and close --force) follow as sub-rows.
            if !close_expanded && selected {
                active = rows.len();
            }
            rows.push(closeup_close_command_row(state, info, selected, width));
            if close_expanded {
                let close_cursor = state.closeup_close_cursor();
                for j in 0..2usize {
                    if Some(j) == close_cursor {
                        active = rows.len();
                    }
                    rows.push(closeup_close_pick_row(
                        j == 1,
                        Some(j) == close_cursor,
                        width,
                    ));
                }
            }
        } else if info.name == "terminal" {
            let terminal_expanded = state.closeup_menu_terminal_expanded();
            if !terminal_expanded && selected {
                active = rows.len();
            }
            rows.push(closeup_terminal_command_row(state, selected, width));
            if terminal_expanded {
                let terminal_cursor = state.closeup_menu_terminal_cursor();
                for (j, &action) in state.closeup_menu_terminal_actions().iter().enumerate() {
                    if Some(j) == terminal_cursor {
                        active = rows.len();
                    }
                    rows.push(closeup_terminal_pick_row(
                        action,
                        Some(j) == terminal_cursor,
                        width,
                    ));
                }
            }
        } else {
            if selected {
                active = rows.len();
            }
            rows.push(closeup_menu_row(info, selected, width));
        }
    }

    // A `/` filter that matches nothing leaves the command area blank; a dim
    // placeholder keeps the box from reading as a bug (an empty menu). Worded like
    // the Open Project picker's "No projects match the filter." for consistency.
    if commands.is_empty() {
        rows.push(style("  No commands match the filter.").dim().to_string());
    }

    // Window the command area to the widest expansion's height, so every picker
    // opens in place — its sub-rows shown, not collapsed into `↑/↓ N more` — and
    // the box never resizes as pickers open and close. Capped to the right pane so
    // it never overruns; only when the pane is too short does the window scroll.
    let visible = closeup_menu_visible(closeup_menu_target(state, &commands), avail_rows);
    let mut lines = vec![closeup_menu_filter_line(state, width)];
    lines.extend(closeup_menu_window(rows, active, visible));
    lines.push(String::new());
    // The hint is contextual: picker-navigation keys while any picker is open,
    // the `/` filter's own keys while it is live, a row-specific expand affordance
    // while the cursor can open one, else base.
    let hint = if close_expanded {
        "↑↓ move   Enter run   ← back".to_string()
    } else if expanded {
        "↑↓ move   Enter launch   ← back".to_string()
    } else if state.closeup_menu_filtering() {
        "↑↓ move   Enter run   ⌫ edit   Esc clear".to_string()
    } else if state.closeup_menu_agent_can_expand() {
        "↑↓ move   Enter run   → pick agent   / filter   t terminal   a agent".to_string()
    } else if state.closeup_menu_terminal_can_expand() {
        "↑↓ move   Enter run   → pick terminal   / filter   t terminal   a agent".to_string()
    } else if state.closeup_close_can_expand() {
        "↑↓ move   Enter run   → expand   / filter   t terminal   a agent".to_string()
    } else {
        "↑↓ move   Enter run   / filter   t terminal   a agent".to_string()
    };
    lines.push(style(hint).dim().to_string());
    lines
}

/// Rows the 集中 prompt reserves for its Session-scope hint, always filled to this
/// height (padded with blanks) so the box keeps a **fixed height** as the hint
/// changes while typing — no layout shift, the prompt sibling of the menu's
/// widest-expansion window ([`closeup_menu_target`]) and the palette's
/// [`PALETTE_HINT_ROWS`](super::chrome). Sized to the tallest hint: a `usage` line
/// plus up to [`HINT_MAX`] examples.
pub(super) const FOCUS_PROMPT_HINT_ROWS: usize = HINT_MAX + 1;

/// The 集中 prompt box's chrome rows around the reserved hint block: the two box
/// borders plus the `❯` command line and its blank spacer. The hint block is
/// capped to `avail_rows - FOCUS_PROMPT_CHROME` so a short pane never overruns.
pub(super) const FOCUS_PROMPT_CHROME: usize = 4;

/// The 集中 menu's first line: the dim `Run a command:` label normally, or — while
/// a `/` filter is live — a `Filter: <query>` line that shows the typed text as the
/// list narrows beneath it. Mirrors the Open Project picker's filter bar (see
/// [`crate::presentation::tui::open`]) so the two search affordances read alike.
pub(super) fn closeup_menu_filter_line(state: &HomeState, width: usize) -> String {
    let Some(query) = state.closeup_menu_filter() else {
        return style("Run a command:").dim().to_string();
    };
    let label = style("Filter:").dim();
    let value = if query.is_empty() {
        style(" type to filter").dim().to_string()
    } else {
        format!(" {}", style(query).accent())
    };
    clip_to_width(&format!("{label}{value}"), width)
}

/// The body of the 集中 (Closeup) prompt surface (no identity header): the
/// session-scoped command line (`❯ <input>▏`) and a **fixed-height** Session-scope
/// hint block below it, so the box never resizes as the hint changes while typing.
/// Rendered as the body of the floating prompt overlay modal (the prompt sibling
/// of [`closeup_menu_body`]; see [`super::render_frame`] and
/// [`HomeState::closeup_action_overlay`]); the `session:` identity rides the modal's
/// title rather than a header line here. `avail_rows` caps the reserved block so a
/// short right pane never overruns (mirroring the menu's `avail_rows` window).
pub(super) fn closeup_prompt_body(
    state: &HomeState,
    width: usize,
    avail_rows: usize,
) -> Vec<String> {
    let prompt = style("❯").danger().bold();
    // Split at the caret so ←/→/Home/End move a visible block caret through the prompt.
    let (before, after) = state
        .closeup_prompt()
        .split_at(state.closeup_prompt_cursor());
    let value = widgets::block_caret(before, after, &Style::new().accent());
    let mut lines = vec![clip_to_width(&format!("{prompt} {value}"), width)];
    lines.push(String::new());
    // Reserve a fixed number of hint rows (padded with blanks), so the box keeps
    // one height whatever the hint is — commands, usage/examples, or none. Capped
    // to what the pane can hold so a short pane never pushes the box past it.
    let hint_rows = FOCUS_PROMPT_HINT_ROWS.min(avail_rows.saturating_sub(FOCUS_PROMPT_CHROME));
    let mut hints = closeup_hint_lines(state.closeup_prompt_hint(), width);
    hints.truncate(hint_rows);
    hints.resize(hint_rows, String::new());
    lines.extend(hints);
    lines
}

pub(super) fn closeup_hint_lines(hint: Hint, width: usize) -> Vec<String> {
    match hint {
        Hint::Commands(hints) => hints
            .iter()
            .take(HINT_MAX)
            .map(|h| {
                let name = style(format!("{:<9}", h.name)).accent().to_string();
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
                style(usage).accent()
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
