//! Rendering for the home (workspace) screen's mode-aware layout.
//!
//! Top to bottom: a title bar, the engagement-ladder mode indicator, a blank
//! separator row, a body split into the worktree list (left) and a
//! mode-dependent right pane, the command input, and a footer. The right pane is
//! a preview of the highlighted session in 切替 (Switch); the session's action
//! surface (a menu or a prompt) in 在席 (Focus); and the live embedded terminal
//! in 没入 (Attached). The workspace command line is the `:` command palette
//! overlay, drawn as a centred modal over the panes. All functions take plain
//! data and return styled lines, so the layout is rendered without any terminal
//! IO.
//!
//! This module owns the shared text/layout helpers and the top-level
//! [`render_frame`] that stitches the screen together. The pane bodies live in
//! [`panes`]; the surrounding chrome (title, ladder, input, footer, the command
//! palette, modals) lives in [`chrome`].

mod chrome;
pub mod content;
mod panes;

use crate::presentation::tui::widgets;
use crate::presentation::tui::widgets::clip_to_width;

use chrome::{
    command_palette_body, footer_line, input_line, mode_ladder, quit_confirm_frame,
    remove_modal_frame, switch_create_rows, switch_rename_rows, task_status_line, text_modal_body,
    title_bar, update_banner, PALETTE_INNER, TEXT_MODAL_INNER,
};
use panes::{left_pane, right_pane_contents};
// The embedded terminal pane (没入) maps a click to the tab under it through this.
pub(super) use panes::attached_tab_at;

use super::state::{HomeState, Mode};
use crate::domain::settings::Sidebar;

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
/// a space, and the widest status word (`synced` / `pushed` / `dirty`, 6
/// columns).
const STATUS_COL: usize = 8;

/// Nerd Font (git) glyphs paired with each branch lifecycle status, for an
/// at-a-glance read of the right-edge status field. They need a patched "Nerd
/// Font" terminal font to render; without one the terminal shows a fallback box,
/// but the colour-coded word beside the icon still carries the meaning.
const NEW_ICON: char = '\u{f067}'; // nf-fa-plus — freshly cut, no work yet
const DIRTY_ICON: char = '\u{f040}'; // nf-fa-pencil — uncommitted changes
const LOCAL_ICON: char = '\u{e725}'; // nf-dev-git_branch — committed, lives only locally
const PUSHED_ICON: char = '\u{f0ee}'; // nf-fa-cloud_upload — pushed to the remote
const SYNCED_ICON: char = '\u{f00c}'; // nf-fa-check — up to date, nothing un-merged

/// Width of the active-session marker cell on line 1: the `*` marker (or a
/// blank) plus the space that separates it from the branch name. It sits
/// between the branch name and the right-edge status field.
const ACTIVE_COL: usize = 2;

/// The vertical bar (with surrounding spaces) dividing the two panes.
const SEP: &str = " │ ";

/// Visible width of [`SEP`].
const SEP_WIDTH: usize = 3;

/// Narrowest and widest the left (worktree) pane is allowed to be when the
/// sidebar is shown at its full width ([`Sidebar::Full`]).
const LEFT_MIN: usize = 16;
const LEFT_MAX: usize = 40;

/// Width of the collapsed sidebar rail ([`Sidebar::Rail`]): a gutter column for
/// the active bar plus a 2×2 glyph grid — kind + git status on row 1, agent state
/// on row 2 (under the git status). Narrow enough to hand most of the width to the
/// right pane while still showing which session is active, its git state, and what
/// its agent is doing. Each entry spans **two rows**, exactly like the full
/// sidebar, so toggling the sidebar never shifts a session to a different row —
/// only the width changes.
const RAIL_WIDTH: usize = 5;

/// Shown in the right pane between attaching the terminal and its first screen
/// snapshot arriving.
const TERMINAL_STARTING: &str = "Starting terminal…";

/// Most command-hint rows drawn above the input at once. Beyond this a
/// "… and N more" line stands in for the rest, so the hints never crowd out the
/// body on a normal terminal.
const HINT_MAX: usize = 6;

/// Display width of the command-name column in the hints.
const HINT_NAME_COL: usize = 12;

/// Columns before the name column in a hint row: `"  "` indent + the marker
/// cell + a space.
const HINT_INDENT: usize = 4;

/// Most session rows the removal modal shows at once; a longer list scrolls to
/// keep the cursor in view, with a count of the hidden rows above and below.
const REMOVE_MODAL_VISIBLE: usize = 8;

/// Body lines the text modal shows at once; a longer dump scrolls, with a count
/// of the hidden lines above and below. Shared with the event loop's scroll
/// clamp and paging step.
pub const TEXT_MODAL_VISIBLE: usize = 16;

/// Right-pads `content` with spaces to fill `width` display columns. Content
/// already at least that wide is returned unchanged.
fn pad_to_width(content: String, width: usize) -> String {
    let visible = console::measure_text_width(&content);
    if visible >= width {
        content
    } else {
        let mut content = content;
        // Grow the existing buffer in place rather than allocating a separate
        // `" ".repeat(..)` string only to copy it straight back out.
        content.extend(std::iter::repeat_n(' ', width - visible));
        content
    }
}

/// Splits the terminal `width` into the left pane width and the right pane
/// width, leaving room for the divider. A full sidebar is clamped to a readable
/// band; a collapsed one is the fixed-width [`RAIL_WIDTH`] rail. Either way the
/// left pane never overruns the terminal.
fn layout(width: usize, sidebar: Sidebar) -> (usize, usize) {
    let left = match sidebar {
        Sidebar::Full => (width / 3).clamp(LEFT_MIN, LEFT_MAX),
        Sidebar::Rail => RAIL_WIDTH,
    };
    let left = left.min(width.saturating_sub(SEP_WIDTH));
    let right = width.saturating_sub(left + SEP_WIDTH);
    (left, right)
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

impl TerminalGeometry {
    /// Translate the embedded terminal's `(row, col)` cursor into a 1-based
    /// screen position inside the pane.
    ///
    /// vt100 reports a *deferred wrap* by parking the cursor one column past the
    /// last cell (`col == cols`) once a row is filled exactly to its width — and
    /// a full-width (CJK) line reaches that edge at half the keystrokes, so it is
    /// hit often while typing Japanese. Placed verbatim, that column spills past
    /// the pane's right edge and the real cursor jumps to the screen edge, so the
    /// column (and, defensively, the row) is clamped back onto the last cell, the
    /// way a standalone terminal shows a pending-wrap cursor.
    pub fn cursor_screen_pos(self, row: u16, col: u16) -> (u16, u16) {
        let col = col.min(self.cols.saturating_sub(1));
        let row = row.min(self.rows.saturating_sub(1));
        let x = self.origin_col + col + 1;
        let y = self.origin_row + row + 1;
        (x, y)
    }
}

/// Computes the [`TerminalGeometry`] for a raw terminal size, matching the
/// layout [`render_frame`] draws (title + mode ladder + a blank separator above
/// the body, the left pane and divider to its left). `rows` and `cols` are at
/// least 1.
pub fn terminal_geometry(
    raw_height: usize,
    raw_width: usize,
    sidebar: Sidebar,
) -> TerminalGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, right_w) = layout(width, sidebar);
    // Chrome above the body is three rows: the title bar, the mode ladder, and a
    // blank separator. Below it sit the single-line input and the footer (two
    // more rows in the modes that show a live terminal).
    let pane_rows = height.saturating_sub(5).max(1);
    TerminalGeometry {
        rows: pane_rows.max(1) as u16,
        cols: right_w.max(1) as u16,
        origin_col: (left_w + SEP_WIDTH) as u16,
        // The body starts below the title bar, the mode ladder, and the blank
        // separator row beneath them.
        origin_row: 3,
    }
}

/// Rows the tab strip reserves at the top of the right pane in 没入 (Attached).
/// The strip lists the session's panes over two rows — a chip per pane plus an
/// underline marking the active one — and is always present once attached, even
/// for a single pane, so the embedded terminal's geometry does not jump as panes
/// are added or closed.
pub const TAB_BAR_ROWS: usize = 2;

/// The embedded terminal's geometry while 没入 (Attached): the right pane minus
/// the tab strip ([`TAB_BAR_ROWS`]) reserved above it, with the origin pushed
/// down by the same so the cursor tracks the shell below the strip. The pane and
/// the pool both size / place the live terminal through this, while the
/// tab-less previews in 切替 use [`terminal_geometry`].
pub fn attached_geometry(
    raw_height: usize,
    raw_width: usize,
    sidebar: Sidebar,
) -> TerminalGeometry {
    let geo = terminal_geometry(raw_height, raw_width, sidebar);
    TerminalGeometry {
        rows: (geo.rows as usize).saturating_sub(TAB_BAR_ROWS).max(1) as u16,
        origin_row: geo.origin_row + TAB_BAR_ROWS as u16,
        ..geo
    }
}

/// The number of two-pane body rows for a normalized terminal `height` — the
/// rows between the title/ladder/blank chrome above (3 rows) and the input /
/// footer chrome below (2 rows). Mode-independent now that every base mode uses
/// the single-line [`input_line`] (the workspace command line is the `:` palette
/// overlay), so the preview's scroll clamp agrees with what is actually drawn.
fn body_rows_for(height: usize) -> usize {
    height.saturating_sub(5).max(1)
}

/// How many Markdown lines the right-pane preview shows at once for a raw terminal
/// size: the body rows less the preview's one-row header. Used by the event loop
/// to clamp and page the preview's scroll so the last line stays in view.
pub fn preview_visible(raw_height: usize, raw_width: usize, _state: &HomeState) -> usize {
    let (height, _width) = widgets::normalize_size(raw_height, raw_width);
    body_rows_for(height).saturating_sub(1).max(1)
}

/// Maps the home screen's engagement [`Mode`] onto the resting mascot's
/// [`RabbitMood`](widgets::RabbitMood), so the sidebar rabbit's expression tracks
/// what the user is doing without coupling the widget to the screen's enum.
fn rabbit_mood(mode: Mode) -> widgets::RabbitMood {
    match mode {
        Mode::Switch => widgets::RabbitMood::Browsing,
        Mode::Focus => widgets::RabbitMood::Attentive,
        Mode::Attached => widgets::RabbitMood::Working,
    }
}

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
    // The text modal (a text-dumping command's output: `man` / `history` /
    // `session list`) is *not* a full-screen overlay: like the `:` command
    // palette it floats as a centred box over the live workspace frame (built
    // below) so the panes stay visible around it, rather than a black backdrop.
    // It is composited last, alongside the palette.
    //
    // The workspace command palette (`:`) is *not* a full-screen overlay: it
    // floats as a centred box over the live workspace frame (built below) so the
    // panes stay visible around it, rather than a black backdrop. It is composited
    // last, after the frame and its top-right notices are assembled.
    //
    // The session-note editor is *not* a full-screen overlay either: it renders in the
    // right pane (see [`panes::right_pane_contents`]) so the sidebar and chrome
    // stay put and the screen never switches — matching the read-only note shown
    // there while browsing.

    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    // The left sidebar honours the `Ctrl-B` toggle in every mode — 切替 (Switch)
    // included, so the picker works collapsed to the rail (the cursor `>` and the
    // dimming still render there). 切替's inline create / rename name input needs
    // room: at full width it rides the left pane inline (below), but collapsed to
    // the rail there is none, so it moves to the right pane instead (see
    // [`right_pane_contents`]). The pool sizes the 切替 preview to this same state,
    // so the previewed terminal always fills the pane it is drawn into.
    let sidebar = state.sidebar();
    let (left_w, right_w) = layout(width, sidebar);

    // Every base mode uses a single-line status input (the workspace command line
    // is the `:` palette overlay, drawn as a centred modal instead of a resident
    // box). The footer and the input are one row each.
    let input_lines = vec![input_line(state)];

    // Chrome: title + mode ladder + a blank separator on top, the input line +
    // footer at the bottom. Everything between is the two-pane body.
    let body_rows = body_rows_for(height);

    let mut left = left_pane(
        state.list(),
        state.live_paths(),
        state.running_paths(),
        state.waiting_paths(),
        state.done_paths(),
        left_w,
        body_rows,
        // In 切替 the keyboard is on the list: fade the rows the cursor is not on.
        state.mode() == Mode::Switch,
        sidebar,
    );
    // 切替's inline create / rename input rides the left pane — but only at full
    // width. Collapsed to the rail (5 columns) there is no room for the name, so
    // the input renders in the right pane instead (see [`right_pane_contents`]).
    if sidebar == Sidebar::Full {
        // While naming a new session in 切替, append the inline create row(s) to
        // the left pane (trimmed back to the session-list area if it overflows).
        if let Some(create) = state.create() {
            for row in switch_create_rows(create.value(), create.cursor(), create.error(), left_w) {
                left.push(row);
            }
            left.truncate(body_rows);
        }
        // While renaming a session's sidebar label in 切替, append the inline
        // rename row to the left pane (trimmed back if it would overflow).
        if let Some(rename) = state.rename() {
            for row in switch_rename_rows(rename.target(), rename.value(), rename.cursor(), left_w)
            {
                left.push(row);
            }
            left.truncate(body_rows);
        }
    }
    // Rest the mascot at the bottom of the (full) sidebar, below the session
    // list, when there is room for it. Its face and colour follow the current
    // mode — browsing in 切替, attentive in 在席, heads-down in 没入 — so the
    // rabbit reflects what the user is doing. With a list (or an inline create /
    // rename input) long enough to reach those rows, or a sidebar collapsed to
    // the narrow rail / too narrow to hold the art, it politely hides rather than
    // overlapping the list.
    if sidebar == Sidebar::Full && left_w >= widgets::workspace_rabbit_width() {
        let rabbit = widgets::workspace_rabbit(rabbit_mood(state.mode()));
        if body_rows >= rabbit.len() && left.len() <= body_rows - rabbit.len() {
            left.resize(body_rows - rabbit.len(), String::new());
            left.extend(rabbit);
        }
    }
    let right = right_pane_contents(state, right_w, body_rows);

    let mut lines = Vec::with_capacity(height);
    lines.push(title_bar(width, state.list()));
    lines.push(mode_ladder(width, state.mode()));
    // A blank separator row between the mode ladder and the body, so the
    // engagement-ladder header reads as its own band set apart from the panes.
    lines.push(pad_to_width(String::new(), width));
    let body_start = lines.len();
    // `left` / `right` are not read past this loop, so consume them by value: each
    // row's owned cell text moves straight into the composed line instead of being
    // cloned out of the borrowed vecs. The line reuses the padded left cell's
    // allocation (pushing the separator and right cell onto it) rather than letting
    // `format!` allocate a fresh string per body row.
    let mut left_rows = left.into_iter();
    let mut right_rows = right.into_iter();
    for _ in 0..body_rows {
        let mut line = pad_to_width(left_rows.next().unwrap_or_default(), left_w);
        line.push_str(SEP);
        line.push_str(&right_rows.next().unwrap_or_default());
        lines.push(line);
    }

    lines.extend(input_lines);
    lines.push(footer_line(width, state));

    // Overlay the top-right corner, in priority order: a momentary blocking
    // action (terminal / agent launch) shows the loading rabbit; otherwise any
    // in-flight background session work (create / remove) shows the task status
    // line; otherwise the "update available" notice shows when the background
    // check has found a newer release than this build. The loading rabbit anchors
    // to the top of the right pane (the rows below the title bar and mode ladder);
    // the update notice rides the header rows (like the task status block), since
    // the 切替 preview now occupies the right pane and would otherwise collide.
    if let Some(loading) = state.loading() {
        // The transient launch indicator is deliberate and short-lived, so it
        // takes the corner even over a live pane.
        widgets::overlay_top_right(
            &mut lines,
            body_start,
            width,
            &widgets::loading_rabbit(loading.frame(), loading.label()),
        );
    } else if !state.tasks().is_empty() {
        // Background session work (create / remove) running off the event-loop
        // thread. It rides the two header rows (row 0 the title bar, row 1 the
        // mode ladder), whose centred content leaves the right columns blank —
        // so it never collides with the right pane (preview / menu / live
        // terminal) the way the old body-row panel did, and needs no
        // live-terminal suppression. Two rows give the label more width.
        widgets::overlay_top_right(
            &mut lines,
            0,
            width,
            &task_status_line(state.tasks(), width),
        );
    } else if let Some(latest) = state.update() {
        // The update notice rides the two header rows (the title bar and mode
        // ladder, whose centred content leaves the right columns free), like the
        // task status block, so it never collides with the right pane's preview /
        // menu / live terminal — which the 切替 default now occupies. The compact
        // two-line block (message + version) fits the gap the fixed-width title
        // name leaves.
        widgets::overlay_top_right(&mut lines, 0, width, &update_banner(&latest));
    }

    // Float the `:` command palette as a centred box over the assembled frame, so
    // the workspace shows around it instead of a black backdrop. A fixed-height
    // body (see [`command_palette_body`]) centred over a constant-height frame
    // keeps the same position and size as the user types and runs commands — no
    // jump.
    if state.command_palette_open() {
        let inner = widgets::modal_inner_width(width, PALETTE_INNER);
        let body = command_palette_body(state, inner);
        widgets::overlay_modal(&mut lines, width, "Command", inner, &body);
    }

    // Float the text modal (a text-dumping command's output) as a centred box
    // over the assembled frame too, so the workspace shows around it instead of
    // a black backdrop.
    if let Some(modal) = state.text_modal() {
        let inner = widgets::modal_inner_width(width, TEXT_MODAL_INNER);
        let body = text_modal_body(modal, inner);
        widgets::overlay_modal(&mut lines, width, &modal.title, inner, &body);
    }

    lines
}

#[cfg(test)]
mod tests;
