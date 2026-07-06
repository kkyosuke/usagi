//! Thin dispatcher for the home screen right pane.
//!
//! The larger pane-specific renderers live in sibling modules (`sidebar`,
//! `focus_menu`, `tabs_hit`, `pr_popup`, `diff_render`, and
//! `markdown_render`) so this file stays focused on mode dispatch and shared
//! right-pane composition.

use console::{style, Style};

use super::super::state::{
    CreateInput, HomeState, LineKind, LogLine, Mode, Preview, RenameInput, ROOT_NAME,
};
use super::super::terminal::tabs::TabStrip;
use super::super::terminal::view::TerminalView;
use super::diff_render::diff_pane;
use super::markdown_render::markdown_row;
use super::sidebar::{dim_row, status_label, AgentState};
use super::tabs_hit::header_tab_rows;
use super::{clip_to_width, pad_to_width, DETACHED, ROOT_DETAIL, STATUS_COL, TERMINAL_STARTING};
use crate::domain::settings::Sidebar;
use crate::domain::workspace_state::BranchStatus;
use crate::presentation::theme::Palette;
use crate::presentation::tui::widgets;

pub(super) fn log_line(line: &LogLine, width: usize) -> String {
    let raw = match line.kind {
        LineKind::Command => format!("❯ {}", line.text),
        _ => line.text.clone(),
    };
    let clipped = clip_to_width(&raw, width);
    match line.kind {
        LineKind::Command => style(clipped).accent().bold().to_string(),
        LineKind::Output => clipped,
        LineKind::Error => style(clipped).danger().to_string(),
        LineKind::Notice => style(clipped).warning().to_string(),
    }
}

/// Fixed display width of the session name in the right-pane header.
const HEADER_NAME_COL: usize = 16;
/// Wide enough for the longest agent label with its leading [AI glyph](AGENT_ICON)
/// and phase icon: `<robot> ▶ running` measures 11 columns.
const HEADER_AGENT_COL: usize = 11;
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
    // Measure by plain display width (matching [`name_cell`]) so a name carrying
    // wide characters keeps the identity block a constant width instead of pushing
    // the status / agent fields — and the tab strip laid beside it — sideways.
    let name = pad_to_width(
        style(clip_to_width(name, HEADER_NAME_COL))
            .accent()
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
pub(super) fn active_session_header(state: &HomeState) -> String {
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

/// The label of 在席's trailing "+ new" tab — the action surface that launches a
/// pane. ASCII so the underline marker in [`tab_strip_parts`] (which measures
/// width in `chars`) lands exactly under it, as it does for the pane labels.
pub(super) const FOCUS_NEW_TAB_LABEL: &str = "+ new";

/// A blank right pane of exactly `rows` rows — the pane behind a floating overlay
/// that owns the surface (the 在席 action modal), so the modal reads against an
/// empty pane rather than stale content showing through.
fn blank_pane(rows: usize) -> Vec<String> {
    vec![String::new(); rows]
}

macro_rules! loading_tab_body {
    ($width:expr, $rows:expr, $frame:expr) => {{
        let width = $width;
        let rows = $rows;
        let mut pane = vec![String::new(); rows];
        let mut block = launch_loading_block!($frame, width);
        if !block.is_empty() {
            let mut block_w = 0;
            for line in &block {
                block_w = block_w.max(console::measure_text_width(line));
            }
            block.push(super::center_row(
                &style("起動中…").dim().to_string(),
                block_w,
            ));
            widgets::overlay_region_centered(&mut pane, width, 0, width, 0, rows, &block);
        }
        pane
    }};
}

/// Builds the 在席 (Focus) right pane. With no live panes it is a blank pane
/// behind the session's floating action surface — the Menu or the Prompt, both
/// composited as overlay modals by [`render_frame`] (see
/// [`HomeState::focus_action_overlay`]). With live panes it gains a **tab strip**:
/// one chip per live pane followed by a "+ new" chip while that launch surface is
/// selected, the session identity beside it (shared with 没入), and below it the
/// selected pane's live preview — on the "+ new" tab the pane stays blank because
/// the action surface again floats as an overlay. After a zoom-out that surface
/// floats over the selected pane tab instead: the preview drawn here keeps
/// showing behind it.
fn focus_pane(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // No live panes: the action surface (Menu or Prompt) floats as an overlay
    // modal centred over the pane (composited by [`render_frame`] when
    // [`HomeState::focus_action_overlay`] holds), so the pane behind it stays
    // blank — neither surface renders inline.
    let Some(strip) = state.terminal_tabs().filter(|s| !s.labels.is_empty()) else {
        return blank_pane(rows);
    };

    // Live panes: the session's panes as tabs. The "+ new" tab is appended only
    // while it is the selected tab — the launch surface the user is acting on —
    // so stepping off it (e.g. `Esc` after `Ctrl-T`) drops the chip rather than
    // leaving a stale "+ new" sitting on the strip. The identity rides the
    // strip's row (as in 没入), so the body below carries no header of its own.
    let on_new = state.focus_on_new_tab();
    let mut labels = strip.labels.clone();
    let active = if on_new {
        labels.push(FOCUS_NEW_TAB_LABEL.to_string());
        labels.len().saturating_sub(1)
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    // A background pane loading in this session animates its chip here (the
    // resolving placeholder chip is appended to the published strip by the loop; a
    // spawned pane's chip sits at its pool index).
    let loading_body = state.terminal_loading_body_frame();
    let mut lines = header_tab_rows(
        header,
        Some(&combined),
        loading_body.and_then(|_| state.loading_tab()),
        width,
    );

    // On the "+ new" tab the launch surface (Menu or Prompt) floats as an overlay
    // modal (see [`HomeState::focus_action_overlay`]), so only the tab strip shows
    // behind it here. On a pane tab, preview the pane's live screen (the snapshot
    // taken before painting) so the selection shows what re-attaching reveals,
    // falling back to a label until the first snapshot is available.
    if let Some(frame) = loading_body {
        let body = rows.saturating_sub(lines.len());
        lines.extend(loading_tab_body!(width, body, frame));
    } else if !on_new {
        match state.terminal_view() {
            Some(view) => {
                let body = rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").success().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    }

    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// Pad `lines` to fill the right pane and pin `hint` to its bottom row.
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
    let value = widgets::block_caret(before, after, &Style::new().accent());
    let mut lines = vec![style("+ new session").success().bold().to_string()];
    lines.extend(widgets::boxed("", inner, &[value]));
    // Keep the row count stable whether or not the name is currently invalid: an
    // error replaces the dim hint in place rather than adding a row.
    match create.error() {
        Some(err) => lines.push(style(clip_to_width(err, width)).danger().to_string()),
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
/// key hint pinned to the bottom row. At full width there is room to edit on the
/// row itself, so the session's own name line becomes the editable label in place
/// (see the `rename` branch of [`worktree_row`]) and this pane is not used.
fn switch_rename_pane(rename: &RenameInput, width: usize, rows: usize) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let value = widgets::block_caret(rename.value(), "", &Style::new().accent());
    let mut lines = vec![clip_to_width(
        &style(format!("rename {}", rename.target()))
            .accent()
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

/// Build the floating `note` box overlaid on the right pane. With `caret` set it
/// is the **editor** (a block caret on the cursor line, the view windowed around
/// it, and the selected span — if any — reversed); with `None` it is the
/// **read-only** note (capped, the overflow elided with `… (N more)`). `max` caps
/// the body so the box always leaves part of the right pane visible underneath.
/// Returned rows are the bordered box. The session is already named in the pane
/// header, so the title is just `note` (no session name).
fn note_box(
    lines: &[String],
    caret: Option<(usize, usize)>,
    selection: Option<((usize, usize), (usize, usize))>,
    width: usize,
    max: usize,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let max = max.max(1);
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
        // Editing: a `max`-line window around the caret. The selected span is
        // reversed; the caret is a block on the cursor line (drawn by
        // `block_selection` at the selection's edge when a span is active, else by
        // `block_caret`), so editing happens where it shows.
        Some((caret_row, caret_col)) => {
            let start = caret_row.saturating_sub(max.saturating_sub(1));
            let base = Style::new();
            lines
                .iter()
                .enumerate()
                .skip(start)
                .take(max)
                .map(
                    |(i, line)| match selection_on_line(selection, i, line.len()) {
                        Some((sel_start, sel_end, newline_selected)) => {
                            let caret = (i == caret_row).then_some(caret_col);
                            widgets::block_selection(
                                line,
                                sel_start,
                                sel_end,
                                caret,
                                newline_selected,
                                &base,
                            )
                        }
                        None if i == caret_row => {
                            let (before, after) = line.split_at(caret_col);
                            widgets::block_caret(before, after, &base)
                        }
                        None => line.clone(),
                    },
                )
                .collect()
        }
    };
    // `boxed`/`boxed_styled` clip each line (and the block-caret one, ANSI
    // included) to `inner`. While editing, paint the frame in the accent colour
    // and mark the title so the open editor is unmistakable — visually distinct
    // from the read-only note, which keeps the plain frame.
    match caret {
        Some(_) => {
            widgets::boxed_styled("note (編集中)", inner, &body, &Style::new().accent().bold())
        }
        None => widgets::boxed("note", inner, &body),
    }
}

/// The byte span `[start, end)` of `selection` that lies on line `row` (whose
/// content is `len` bytes), plus whether the line break after it is selected too
/// (the span continues onto a later line, so the renderer shows the newline as a
/// reversed trailing cell). `None` when the line is outside the selection.
fn selection_on_line(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    len: usize,
) -> Option<(usize, usize, bool)> {
    let ((sr, sc), (er, ec)) = selection?;
    if row < sr || row > er {
        return None;
    }
    let start = if row == sr { sc } else { 0 };
    let end = if row == er { ec } else { len };
    Some((start, end, row < er))
}

/// The floating note overlay for the right pane, or `None` when none applies. The
/// **editor** (when open, in any mode) wins; otherwise the highlighted session's
/// **read-only** note shows while browsing in 切替 (see
/// [`HomeState::visible_switch_note`]). The box is a narrow top-right column (see
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
            editor.area().lines(),
            Some(editor.area().cursor()),
            editor.area().selection(),
            box_w,
            cap,
        ));
    }
    if let Some(note) = state.visible_switch_note() {
        let cap = SWITCH_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        let note_lines: Vec<String> = note.lines().map(str::to_string).collect();
        return Some(note_box(&note_lines, None, None, box_w, cap));
    }
    None
}

/// Witty English one-liners rested beneath the idle mascot in the 切替 preview
/// (and [`idle_quip`] picks one per session). They stand in for the 在席 menu's
/// choices — which selecting reveals only as a floating modal, never inline — so
/// the preview says "nothing is running here yet" without promising a surface that
/// never appears. Each nods to the usagi and to `Enter` as the way in.
const IDLE_QUIPS: [&str; 6] = [
    "Quiet as a sleeping bunny — press Enter to begin.",
    "This burrow is empty. Hop in with Enter.",
    "No tabs stirring yet. Enter starts one.",
    "A fresh warren awaits. Enter to dig in.",
    "Nothing running here. Enter wakes it up.",
    "Still and cozy — press Enter to get going.",
];

/// The [`IDLE_QUIPS`] line for the highlighted row, chosen by its name so the quip
/// stays stable per session (and differs between them). The root row keys off
/// [`ROOT_NAME`]; a detached session off [`DETACHED`].
fn idle_quip(state: &HomeState) -> &'static str {
    let name = match state.list().selected() {
        Some(w) => w.branch.as_deref().unwrap_or(DETACHED),
        None => ROOT_NAME,
    };
    let seed: usize = name.bytes().map(usize::from).sum();
    IDLE_QUIPS[seed % IDLE_QUIPS.len()]
}

/// The idle-session body: the mascot ([`widgets::rabbit_lines`]) centred in the
/// middle of a `width`×`rows` region with `quip` on a dim row below it. The block
/// (mascot + blank + caption) is centred both horizontally — each row already is —
/// and vertically, so the usagi sits in the centre of the pane whatever its
/// height. Always returns exactly `rows` rows so the pane stays full-height.
fn idle_rabbit_body(quip: &str, width: usize, rows: usize) -> Vec<String> {
    let mut block = widgets::rabbit_lines(width);
    block.push(String::new());
    block.push(widgets::dim_line(width, quip));
    let top = rows.saturating_sub(block.len()) / 2;
    let mut lines = vec![String::new(); top];
    lines.extend(block);
    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// The 切替 (Switch) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; an idle session with no live pane rests the mascot
/// with a light quip ([`idle_rabbit_body`]) on the menu UI, or previews its inline
/// prompt on the prompt UI. The header line carries the session's status and agent
/// state. The key hints live in the footer, so the preview uses the pane's full
/// height. The highlighted session's note is drawn over the top by [`note_overlay`]
/// (not inline), so it never pushes this preview around.
pub(super) fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    if state.list().create_row_selected() {
        let mut lines = vec![
            style(clip_to_width("+ new session", width))
                .green()
                .bold()
                .to_string(),
            style(clip_to_width(
                "Type a name here, or press Enter, to create a session.",
                width,
            ))
            .dim()
            .to_string(),
            style(clip_to_width(
                "Esc cancels the input after it opens.",
                width,
            ))
            .dim()
            .to_string(),
        ];
        lines.truncate(rows);
        return lines;
    }

    let body_rows = rows;
    // The highlighted row's identity plus whether it has a live pane. The header
    // and the `live` flag drive both this preview and the tab hit-test
    // ([`switch_tab_at`]), so they are built once in [`switch_preview_header`]
    // and shared — the strip a click lands on is exactly the one drawn here.
    let (header, live) = switch_preview_header(state);
    let loading_body = state.terminal_loading_body_frame();
    let live_or_loading = live || loading_body.is_some();

    // A live session's tabs share the header's row (the `←`/`→` — and click —
    // targets), so the identity and the tabs read together on one line; the
    // preview below mirrors the active pane.
    let mut lines = header_tab_rows(
        header,
        if live_or_loading {
            state.terminal_tabs()
        } else {
            None
        },
        // A background pane starting in this session animates its chip here (the
        // 切替 preview is where the user waits while it loads).
        loading_body.and_then(|_| state.loading_tab()),
        width,
    );

    if let Some(frame) = loading_body {
        let body = body_rows.saturating_sub(lines.len());
        lines.extend(loading_tab_body!(width, body, frame));
    } else if live {
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
                lines.push(style("● live terminal").success().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    } else {
        // An idle session (no live pane, so no tab) has no screen to mirror.
        // Selecting opens 在席's action surface (which floats as an overlay modal for
        // both Menu and Prompt), so nothing is drawn inline. Previewing it here would
        // promise an inline surface that never appears, so rest the mascot in the
        // middle of the pane with a light English quip instead.
        let body = body_rows.saturating_sub(lines.len());
        lines.extend(idle_rabbit_body(idle_quip(state), width, body));
    }

    // Trim the body to its budget and pad up so the pane is always full-height.
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines
}

/// Header text for 切替's right-pane preview, plus whether the highlighted row is
/// live. `switch_preview` and `switch_tab_at` share this so tab chips are measured
/// from the same header text the renderer puts beside them.
pub(super) fn switch_preview_header(state: &HomeState) -> (String, bool) {
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
    let agent = AgentState::from_flags(live, running, waiting, done).detail(HEADER_AGENT_COL);
    (preview_header(&name, status, agent), live)
}

/// The right pane's contents, by mode. A preview of the would-be session screen
/// in 切替 (the default); the session's action surface — a menu or a prompt, per
/// [`SessionActionUi`] — in 在席; and the live embedded terminal in 没入 (a
/// starting hint until the first snapshot arrives).
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    // A momentary launch (terminal / agent spawn) blanks the right pane so the
    // centred loading rabbit (composited over the frame in [`super::frame`])
    // reads as a dedicated loading screen. Without this the 在席 action menu
    // (agent / terminal / … の選択肢) stays painted behind the rabbit, so the
    // choices would show through while the spawn blocks. Return an empty pane of
    // the right height and let the overlay own the whole surface.
    if state.loading().is_some() && state.terminal_tabs().is_none_or(|s| s.labels.is_empty()) {
        return vec![String::new(); rows];
    }
    // The Markdown preview, when open, takes over the right pane regardless of
    // mode (it is opened from the `:` palette and captures the keyboard while
    // shown).
    if let Some(preview) = state.preview() {
        return preview_pane(preview, right_w, rows);
    }
    // The local-LLM chat, when open, likewise takes over the right pane
    // regardless of mode (opened from 在席's `chat`, it captures the keyboard
    // while shown). The sidebar to its left keeps rendering as usual.
    if let Some(chat) = state.chat() {
        return crate::presentation::tui::chat::ui::pane(chat, right_w, rows);
    }
    // The diff view likewise takes over the right pane (opened from the `:`
    // palette, capturing the keyboard while shown).
    if let Some(diff) = state.diff_view() {
        return diff_pane(diff, right_w, rows);
    }
    // The base pane for the current mode. The session-note overlay (the editor,
    // or the read-only note while browsing in 切替) is composited over its top
    // below, so editing / reading the note never switches the screen — the
    // preview / terminal stays visible behind the floating box.
    let mut base = match state.mode() {
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
            // Fade the whole preview in 切替: the keyboard is on the session list
            // to the left, so dimming the right pane signals it is not the focus —
            // the highlighted session and its tabs are browsed there, not selected
            // from here. The note box (when open) is composited bright on top below,
            // so the deliberately-opened note still reads against the faded preview.
            switch_preview(state, right_w, rows)
                .iter()
                .map(|row| dim_row(row))
                .collect()
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
            let mut head = header_tab_rows(
                header,
                state.terminal_tabs(),
                // A background pane loads while the user waits in the 切替 preview,
                // not while 没入; so the attached strip carries no loading chip.
                None,
                right_w,
            );
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
