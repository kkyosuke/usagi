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
use crate::presentation::tui::widgets::{clip_to_width, clip_to_width_cow};

use chrome::{
    command_palette_body, footer_line, input_line, mode_ladder, quit_confirm_frame,
    remove_modal_body, switch_create_rows, switch_rename_rows, tab_menu_box, tab_rename_body,
    task_status_line, text_modal_body, title_bar, update_confirm_frame, waiting_notice,
    PALETTE_INNER, REMOVE_MODAL_INNER, TEXT_MODAL_INNER,
};
use panes::{group_inline_insert_line, left_pane, right_pane_contents};
// The right-pane tab strips map clicks to the tab under them through these.
pub(super) use panes::{
    attached_tab_at, attached_tab_hit, focus_tab_at, focus_tab_hit, switch_tab_at, switch_tab_hit,
};
// …a click on a sidebar session's PR badge to that session (to pin its PR popup).
pub(super) use panes::sidebar_pr_badge_at;
// …and a click anywhere to the pinned PR popup: open a `#<number>`, or dismiss it.
pub(super) use panes::{pr_popup_click, PopupClick};

use super::state::{HomeState, ModalSize, Mode, WorktreeList};
use crate::domain::resource::ResourceUsage;
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

/// Nerd Font glyph marking a session that carries a [note](crate::domain::workspace_state::SessionRecord)
/// — a yellow sticky-note shown in the otherwise-blank cell between the session
/// name and the right-edge git status on line 1, so the sessions with a memo read
/// at a glance. Needs a Nerd Font to render, like the status glyphs above.
const NOTE_ICON: char = '\u{f249}'; // nf-fa-sticky_note — the session has a memo

/// Width of the active-session marker cell on line 1: the `*` marker (or a
/// blank) plus the space that separates it from the branch name. It sits
/// between the branch name and the right-edge status field.
const ACTIVE_COL: usize = 2;

/// Chrome rows above the two-pane body: the title bar, the mode ladder, and a
/// blank separator. The body — and so the left pane's first row — starts at this
/// 0-based screen row, which is also the embedded terminal's `origin_row`.
const CHROME_TOP_ROWS: usize = 3;

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
/// its agent is doing. Each worktree entry spans the same
/// [`SESSION_ROWS`](panes::SESSION_ROWS) rows as the full sidebar (its third row
/// blank — the rail has no room for a CPU / memory figure), so toggling the
/// sidebar never shifts a session to a different row — only the width changes.
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

/// Body lines the *compact* text modal shows at once; a longer dump scrolls,
/// with a count of the hidden lines above and below. The large `man` modal scales
/// its window to the terminal instead (see [`text_modal_geometry`]).
pub const TEXT_MODAL_VISIBLE: usize = 16;

/// The geometry the text modal of `size` is drawn with for a terminal of
/// `height`×`width`: the inner content width of the box and the visible body
/// window height, as `(inner_width, visible)`.
///
/// Both the renderer (which windows the body and wraps it in a box) and the event
/// loop (whose paging step scrolls by a screenful) read this, so they size the
/// modal from one source and never disagree on how far a page scrolls. A
/// [`Normal`](ModalSize::Normal) modal is the fixed [`TEXT_MODAL_INNER`] /
/// [`TEXT_MODAL_VISIBLE`]; a [`Large`](ModalSize::Large) one scales to the screen
/// via [`widgets::large_modal_geometry`].
pub fn text_modal_geometry(height: usize, width: usize, size: ModalSize) -> (usize, usize) {
    match size {
        ModalSize::Normal => (
            widgets::modal_inner_width(width, TEXT_MODAL_INNER),
            TEXT_MODAL_VISIBLE,
        ),
        ModalSize::Large => {
            let geo = widgets::large_modal_geometry(height, width);
            (geo.inner_width, geo.visible)
        }
    }
}

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
        origin_row: CHROME_TOP_ROWS as u16,
    }
}

/// The selectable left-pane row a left click at the 0-based screen (`col`, `row`)
/// lands on, or `None` when the click is not on a session row. Row 0 is the first
/// group's root row (`⌂ root`); each subsequent flat row maps to a worktree or a
/// later group's root, matching
/// [`WorktreeList::focus_index`](crate::presentation::tui::home::state::WorktreeList).
///
/// Defers the layout walk to [`panes::sidebar_row_at_line_for_sidebar`], which
/// replays exactly what [`left_pane`](panes::left_pane) draws — in both
/// single-workspace mode and 統合(unite) mode (full-sidebar group headers,
/// inter-workspace gaps, root pairs, dividers, and
/// [`SESSION_ROWS`](panes::SESSION_ROWS) rows per worktree) — so a click maps back
/// to its row without the renderer and the hit test ever disagreeing. Returns
/// `None` for a click in the right pane (past `left_w`), in the chrome above or
/// below the body, on a header / divider, or below the last session.
pub(super) fn left_pane_session_at(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, _right) = layout(width, state.sidebar());
    let (col, row) = (col as usize, row as usize);
    // A click in the right pane (or on the divider column) is not a row select.
    if col >= left_w {
        return None;
    }
    // The body's 0-based line under the click; `None` for the chrome above it.
    let line = row.checked_sub(CHROME_TOP_ROWS)?;
    if line >= body_rows_for(height) {
        return None;
    }
    panes::sidebar_row_at_line_for_sidebar(state.list(), line, state.sidebar())
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

/// The workspace-total resource line shown beside the resting mascot's feet —
/// the icon-led ` 23%   512MB` — or `None` when nothing is live (idle), so the
/// rabbit rests without a number. The CPU and memory figures are each tinted by
/// their load band (dim / yellow / red) via
/// [`resource_inline_label_tinted`](panes::resource_inline_label_tinted), so a
/// heavy figure stands out beside the mascot.
fn workspace_total_label(total: ResourceUsage) -> Option<String> {
    (!total.is_idle()).then(|| panes::resource_inline_label_tinted(total))
}

/// Append the workspace resource `total` to the mascot's feet row (its last line
/// — ears, face, **feet**), so the label rests on the rabbit's foot line. Skipped
/// when nothing is live, or when the label would not fit the sidebar beside the
/// art (so the row never overruns `left_w` and pushes the right pane out of
/// line). `rabbit` is the already-indented mascot block.
fn append_total_beside_mascot(rabbit: &mut [String], total: ResourceUsage, left_w: usize) {
    let Some(label) = workspace_total_label(total) else {
        return;
    };
    // The feet are the block's last row; a two-row rail chibi carries no total,
    // so guard against blocks too short to be the full three-row mascot.
    if rabbit.len() < 3 {
        return;
    }
    let feet = rabbit.len().saturating_sub(1);
    let needed =
        console::measure_text_width(&rabbit[feet]) + 2 + console::measure_text_width(&label);
    if needed <= left_w {
        rabbit[feet].push_str(&format!("  {label}"));
    }
}

/// Builds the full home-screen frame for a raw terminal size.
pub fn render_frame(raw_height: usize, raw_width: usize, state: &HomeState) -> Vec<String> {
    // The quit-confirmation modal, when open, overlays everything else.
    if state.quit_confirm() {
        return quit_confirm_frame(raw_height, raw_width, state.live_count());
    }
    // The update-confirmation modal (raised by clicking the mascot's update
    // notice) likewise overlays everything. It is only ever opened while an update
    // is available, so `update()` is `Some` here; fall through defensively if not.
    if state.update_confirm() {
        if let Some(latest) = state.update() {
            return update_confirm_frame(raw_height, raw_width, &latest);
        }
    }
    // The session-removal modal is *not* a full-screen overlay: like the `:`
    // command palette and the text modal it floats as a centred box over the live
    // workspace frame (built below) so the panes stay visible around it, rather
    // than a black backdrop. It is composited last, alongside them.
    //
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

    // Build the worktree column (list + 切替's inline create / rename input) and
    // rest the mode-aware mascot at its foot. Both steps are shared with
    // [`mascot_hit_rect`], so the drawn rabbit and the click target stay one
    // computation.
    let mut left = left_column(state, sidebar, left_w, body_rows);
    let _ = place_mascot(&mut left, state, sidebar, left_w, body_rows);
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
        // Clip each cell to its pane width so the composed row never overruns the
        // terminal. The left row's fixed cells (gutter + status + badges) emit a
        // minimum width even when the name cell shrinks to nothing, and some
        // right-pane content (a menu line, a preview header) isn't pre-sized to a
        // very narrow pane — either can otherwise shove the divider (and the rest
        // of the row) past the right edge, a layout shift the fixed-column design
        // forbids. The left cell is clipped then padded so the `│` divider stays
        // pinned to column `left_w`; the right cell is clipped to `right_w`. At
        // normal widths both already fit, so these clips are no-ops.
        let cell = left_rows.next().unwrap_or_default();
        let mut line = pad_to_width(clip_to_width(&cell, left_w), left_w);
        line.push_str(SEP);
        let right_cell = right_rows.next().unwrap_or_default();
        // The right cell fits its pane at normal widths (the clip is a no-op),
        // so borrow it through `clip_to_width_cow` and append in place — only a
        // row actually too wide allocates a clipped copy. The owned `clip_to_width`
        // here allocated a fresh string for every body row each paint just to copy
        // it straight back out.
        line.push_str(&clip_to_width_cow(&right_cell, right_w));
        lines.push(line);
    }

    // Float the pinned PR popup beside the session whose badge was clicked. The
    // placement is only ever produced on the full sidebar, for a PR-bearing session,
    // with its anchor already clamped to where this overlay lands (see
    // [`panes::pr_popup_placement`] — the click hit-test shares it so a click on a
    // `#<number>` opens exactly the link the user sees). It lists the session's PRs
    // — the row itself folds them to an `<icon> <count>` badge — anchored at the
    // session's first row and pushed just past the divider into the right pane, so
    // it never hides the sidebar it describes. Composited now, while `lines` holds
    // only the body rows, so the box stays within the panes and never spills onto
    // the input / footer below.
    if let Some((popup, top, left)) = panes::pr_popup_placement(state, raw_height, raw_width) {
        widgets::overlay_at(&mut lines, width, top, left, &popup);
    }

    if let Some(menu) = state.tab_menu() {
        widgets::overlay_at(
            &mut lines,
            width,
            menu.row() as usize,
            menu.col() as usize,
            &tab_menu_box(menu),
        );
    }

    lines.extend(input_lines);
    lines.push(footer_line(width, state));

    // Overlay the top-right corner, in priority order: a momentary blocking
    // action (terminal / agent launch) shows the loading rabbit; otherwise any
    // in-flight background session work (create / remove) shows the task status
    // line; otherwise a `◆ N waiting` notice appears while at least one session
    // is waiting for the user's input. The loading rabbit anchors to the top of
    // the right pane (the rows below the title bar and mode ladder); the task
    // status and waiting notice ride the header rows. The "update available"
    // notice is no longer a corner overlay — the sidebar mascot speaks it
    // (above) instead.
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
    } else {
        widgets::overlay_top_right(
            &mut lines,
            0,
            width,
            &waiting_notice(state.waiting_paths().len()),
        );
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
        let (inner, visible) = text_modal_geometry(height, width, modal.size);
        let body = text_modal_body(modal, inner, visible);
        widgets::overlay_modal(&mut lines, width, &modal.title, inner, &body);
    }

    // Float the session-removal checklist as a centred box over the assembled
    // frame too, so the workspace shows around it instead of a black backdrop.
    if let Some(modal) = state.remove_modal() {
        let inner = widgets::modal_inner_width(width, REMOVE_MODAL_INNER);
        let body = remove_modal_body(modal, inner);
        widgets::overlay_modal(&mut lines, width, "Remove sessions", inner, &body);
    }

    if let Some(input) = state.tab_rename() {
        const TAB_RENAME_INNER: usize = 44;
        let inner = widgets::modal_inner_width(width, TAB_RENAME_INNER);
        let body = tab_rename_body(input.value(), input.cursor(), inner);
        widgets::overlay_modal(&mut lines, width, "Rename tab", inner, &body);
    }

    lines
}

/// Header rows above the two-pane body: the title bar, the mode ladder, and the
/// blank separator. The body — and so the sidebar mascot — starts just below them,
/// so a body-row index lifts into a screen row by adding this.
const HEADER_ROWS: usize = 3;

/// The mascot art's left indent inside the sidebar, so its left edge lines up with
/// the bottom input line's content (the `● live terminal` indicator carries a
/// single leading space) rather than sitting flush against the pane edge.
const RABBIT_INDENT: usize = 1;

/// Build the worktree column exactly as [`render_frame`] does, before the mascot
/// is rested at its foot: the session list, plus 切替's inline create / rename
/// input when one is open at full width (collapsed to the rail there is no room,
/// so the input renders in the right pane instead). Shared with
/// [`mascot_hit_rect`] so the two always agree on the column the mascot sits in.
fn left_column(
    state: &HomeState,
    sidebar: Sidebar,
    left_w: usize,
    body_rows: usize,
) -> Vec<String> {
    let mut left = left_pane(
        state.list(),
        state.live_paths(),
        state.running_paths(),
        state.waiting_paths(),
        state.done_paths(),
        state.resource_usages(),
        left_w,
        body_rows,
        // In 切替 the keyboard is on the list: fade the rows the cursor is not on.
        state.mode() == Mode::Switch,
        sidebar,
        state.now(),
    );
    if sidebar == Sidebar::Full {
        // While naming a new session in 切替, insert the inline create row(s) into
        // the selected workspace's own block: directly after that workspace's
        // session rows in the regular sidebar flow. In 統合(unite) mode this keeps
        // the "+ new" input attached to the workspace that `c` targets, instead
        // of drifting to another workspace or to the whole column's foot.
        if let Some(create) = state.create() {
            // `left_pane` always draws the persistent "+ new session" affordance at
            // the list foot; while the input is open it *becomes* that input, so
            // drop the affordance row first and let the input take its slot.
            let persistent = group_inline_insert_line(
                state.list(),
                state.list().group_count().saturating_sub(1),
            );
            if persistent < left.len() {
                left.remove(persistent);
            }
            let rows = switch_create_rows(create.value(), create.cursor(), create.error(), left_w);
            place_create_rows(&mut left, state.list(), rows);
            left.truncate(body_rows);
        }
        // While renaming a session's sidebar label in 切替, append the inline rename
        // row (trimmed back if it would overflow).
        if let Some(rename) = state.rename() {
            for row in switch_rename_rows(rename.target(), rename.value(), rename.cursor(), left_w)
            {
                left.push(row);
            }
            left.truncate(body_rows);
        }
    }
    left
}

/// Insert `rows` into `column` at `line`, padding with blanks when the target line
/// is below the rows currently built. This preserves the normal sidebar flow for
/// inline inputs: they sit after the targeted workspace's list, not at the bottom
/// of the viewport.
fn splice_rows(column: &mut Vec<String>, line: usize, rows: Vec<String>) {
    if line >= column.len() {
        column.resize(line, String::new());
        column.extend(rows);
    } else {
        column.splice(line..line, rows);
    }
}

/// Place 切替's inline create rows inside the selected workspace block without
/// moving the workspaces below it. In 統合(unite) mode every following workspace
/// already has a fixed two-row gap before its header; while creating in a group
/// that has such a follower, reuse those gap rows as the input slot instead of
/// inserting new rows. The follower's header therefore stays on the same screen
/// line (no CLS). The last group has no lower workspace to protect, so it keeps
/// the single-workspace behaviour and appends the input after that group.
fn place_create_rows(column: &mut Vec<String>, list: &WorktreeList, rows: Vec<String>) {
    // The create row lives at the foot of the last group, so a cursor resting on
    // it targets that group; every other cursor targets its own group.
    let group = if list.create_row_selected() {
        list.group_count().saturating_sub(1)
    } else {
        list.selected_group()
    };
    let line = group_inline_insert_line(list, group);
    if group + 1 < list.group_count() {
        replace_rows(column, line, rows);
    } else {
        splice_rows(column, line, rows);
    }
}

/// Replace rows in-place without pushing later rows down. Unlike [`splice_rows`],
/// this never grows the column; it draws temporary inline UI only in already
/// reserved blank space.
fn replace_rows(column: &mut [String], line: usize, rows: Vec<String>) {
    for (slot, row) in column.iter_mut().skip(line).zip(rows) {
        *slot = row;
    }
}

/// Where the sidebar mascot's clickable body landed within the left column: the
/// animal's top body-row index, how many rows it spans, and its left column and
/// width (in cells).
struct MascotSpot {
    animal_top: usize,
    animal_rows: usize,
    left: usize,
    width: usize,
}

/// Rest the mode-aware mascot at the foot of the left `column` (mutating it in
/// place) and report where its clickable body landed, or `None` when there is no
/// room. Shared by [`render_frame`] (which draws it) and [`mascot_hit_rect`]
/// (which hit-tests clicks against it), so the drawn rabbit and the click target
/// are one computation.
///
/// The full-width sidebar rests the mood mascot — its face and colour follow the
/// current mode (browsing in 切替, attentive in 在席, heads-down in 没入), it
/// *speaks* the update notice from a bubble above when one is pending, and it plays
/// a click reaction in the foreground while one is in flight. The collapsed rail
/// shows a tiny static chibi instead. A blank row sits above the art (so it reads
/// apart from the list) and one below (so it floats clear of the input line); with
/// a list long enough to reach those rows, or a pane too narrow to hold the art, it
/// politely hides. Only the bottom [`rabbit_height`](widgets::rabbit_height) rows —
/// the animal itself — are the click target, so a click on the speech bubble above
/// it does not count.
fn place_mascot(
    column: &mut Vec<String>,
    state: &HomeState,
    sidebar: Sidebar,
    left_w: usize,
    body_rows: usize,
) -> Option<MascotSpot> {
    let (mascot, animal_rows, width) = match sidebar {
        Sidebar::Full if left_w >= widgets::workspace_rabbit_width() + RABBIT_INDENT => {
            let mood = rabbit_mood(state.mode());
            // The CPU load drives how strained and busy the rabbit looks (and how
            // fast it animates), from the same workspace total shown beside it.
            let load = state.resource_total().cpu_load();
            // `blinking` is set for the frames just after the user interacts, and
            // `tick` advances on the live loop so the 没入 Working paw pumps — both
            // from the state the event loop refreshes each frame.
            let (blinking, tick) = (state.mascot_blinking(), state.mascot_tick());
            let art = match state.mascot_reaction() {
                // A click reaction plays in the foreground over the resting /
                // speaking mascot for its brief window.
                Some(reaction) => {
                    widgets::workspace_rabbit_reaction(reaction, state.mascot_reaction_phase())
                }
                None => match state.update() {
                    Some(latest) => widgets::workspace_rabbit_speaking(
                        mood,
                        load,
                        &["アップデートがあるぴょん".to_string(), format!("v{latest}")],
                        // Leave room for the indent so the bubble still fits the pane.
                        left_w - RABBIT_INDENT,
                        blinking,
                        tick,
                    ),
                    None => widgets::workspace_rabbit(mood, load, blinking, tick),
                },
            };
            (
                art,
                widgets::rabbit_height(),
                widgets::workspace_rabbit_width(),
            )
        }
        Sidebar::Rail if left_w >= widgets::workspace_rabbit_rail_width() + RABBIT_INDENT => {
            let art = widgets::workspace_rabbit_rail();
            let rows = art.len();
            (art, rows, widgets::workspace_rabbit_rail_width())
        }
        _ => return None,
    };
    let mut rabbit: Vec<String> = mascot
        .into_iter()
        .map(|row| format!("{}{row}", " ".repeat(RABBIT_INDENT)))
        .collect();
    // The workspace CPU / memory total rests beside the full mascot's feet; the
    // rail chibi is too small (and the reaction art has no settled face), so they
    // carry none — `append_total_beside_mascot` no-ops on a block under three rows.
    if sidebar == Sidebar::Full {
        append_total_beside_mascot(&mut rabbit, state.resource_total(), left_w);
    }
    // Reserve a blank row above the art and one below it.
    let reserved = rabbit.len() + 2;
    if body_rows >= reserved && column.len() <= body_rows - reserved {
        column.resize(body_rows.saturating_sub(rabbit.len() + 1), String::new());
        column.extend(rabbit);
        // The animal's body is the bottom `animal_rows` of the placed block; its
        // feet sit on the second-to-last body row, so the body's top is here.
        Some(MascotSpot {
            animal_top: body_rows - 1 - animal_rows,
            animal_rows,
            left: RABBIT_INDENT,
            width,
        })
    } else {
        None
    }
}

/// The screen rectangle the sidebar mascot's clickable body occupies, in 0-based
/// terminal cells. The home loop hit-tests a click against it to decide whether to
/// play a reaction.
pub(super) struct MascotHit {
    top: usize,
    rows: usize,
    left: usize,
    width: usize,
}

impl MascotHit {
    /// Whether the 0-based `(col, row)` click cell lands on the mascot's body.
    pub(super) fn contains(&self, col: u16, row: u16) -> bool {
        let (col, row) = (col as usize, row as usize);
        row >= self.top
            && row < self.top + self.rows
            && col >= self.left
            && col < self.left + self.width
    }
}

/// The screen rectangle the sidebar mascot's body occupies for a raw terminal
/// size and `state`, or `None` when no mascot is shown. Recomputes the same left
/// column and mascot placement [`render_frame`] draws, then lifts the placement
/// into screen coordinates, so a click is hit-tested against exactly where the
/// rabbit was painted. Purely geometric — the caller ([`super::event`]) gates out
/// the modes and overlays where a click should be ignored.
pub(super) fn mascot_hit_rect(
    raw_height: usize,
    raw_width: usize,
    state: &HomeState,
) -> Option<MascotHit> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let sidebar = state.sidebar();
    let (left_w, _right_w) = layout(width, sidebar);
    let body_rows = body_rows_for(height);
    let mut column = left_column(state, sidebar, left_w, body_rows);
    let spot = place_mascot(&mut column, state, sidebar, left_w, body_rows)?;
    Some(MascotHit {
        top: HEADER_ROWS + spot.animal_top,
        rows: spot.animal_rows,
        left: spot.left,
        width: spot.width,
    })
}

#[cfg(test)]
mod tests;
