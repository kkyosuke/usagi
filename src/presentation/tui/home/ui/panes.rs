//! Thin dispatcher for the home screen right pane.
//!
//! The larger pane-specific renderers live in sibling modules (`sidebar`,
//! `closeup_menu`, `tabs_hit`, `pr_popup`, `diff_render`, and
//! `markdown_render`) so this file stays focused on mode dispatch and shared
//! right-pane composition.

use console::{style, Style};

use super::super::state::{
    CreateInput, DiffView, HomeState, LineKind, LogLine, Mode, NoteEditor, NotePane, Preview,
    RenameInput, ROOT_NAME,
};
use super::super::terminal::tabs::TabStrip;
use super::super::terminal::view::TerminalView;
use super::diff_render::diff_pane;
use super::markdown_render::markdown_row;
use super::sidebar::{dim_row, status_label, AgentLifecycle};
use super::tabs_hit::header_tab_rows;
use super::{clip_to_width, pad_to_width, DETACHED, ROOT_DETAIL, STATUS_COL, TERMINAL_STARTING};
use crate::domain::settings::Sidebar;
use crate::domain::workspace_state::{BranchStatus, SessionDecision, SessionTodo};
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
/// Shared by 選択 (Overview) and 没入 (Attached) so both carry the same identity.
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
/// by 没入 (Attached), where it sits above the embedded terminal, and 集中
/// (Closeup), where it sits above the session's pane tabs.
pub(super) fn active_session_header(state: &HomeState) -> String {
    match state.list().active() {
        Some(w) => {
            let agent = AgentLifecycle::from_flags(
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

/// The label of 集中's trailing "+ new" tab — the action surface that launches a
/// pane. ASCII so the underline marker in [`tab_strip_parts`] (which measures
/// width in `chars`) lands exactly under it, as it does for the pane labels.
pub(super) const FOCUS_NEW_TAB_LABEL: &str = "+ new";

/// The label of the session's `diff` tab (the `diff` command's view). Appended to
/// the strip as the active chip while the diff is open, so it reads as a tab
/// alongside the session's panes.
pub(super) const DIFF_TAB_LABEL: &str = "diff";

/// Render the open diff view as a session tab: the session tab strip on top —
/// the live panes' chips followed by an active `diff` chip — then the diff split
/// view ([`diff_pane`]) filling the body below. Heading it with the strip (as 没入
/// and 集中 do for a live pane) is what makes the diff read as a tab in the
/// session rather than a full-pane takeover. The strip block always fills exactly
/// [`TAB_BAR_ROWS`], so the diff body lands in a stable place.
fn diff_tab_pane(state: &HomeState, diff: &DiffView, width: usize, rows: usize) -> Vec<String> {
    let mut labels = state
        .terminal_tabs()
        .map(|s| s.labels.clone())
        .unwrap_or_default();
    labels.push(DIFF_TAB_LABEL.to_string());
    let active = labels.len().saturating_sub(1);
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let mut lines = header_tab_rows(header, Some(&combined), None, width);
    lines.resize(super::TAB_BAR_ROWS, String::new());
    let body = rows.saturating_sub(lines.len());
    lines.extend(diff_pane(diff, width, body));
    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// A blank right pane of exactly `rows` rows — the pane behind a floating overlay
/// that owns the surface (the 集中 action modal), so the modal reads against an
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

/// Render the `diff` tab while its patch is still loading. This mirrors
/// [`diff_tab_pane`] so the tab appears immediately, but its body is the same
/// centered loading surface pane launches use.
fn pending_diff_tab_pane(
    state: &HomeState,
    frame: usize,
    width: usize,
    rows: usize,
) -> Vec<String> {
    let mut labels = state
        .terminal_tabs()
        .map(|s| s.labels.clone())
        .unwrap_or_default();
    labels.push(DIFF_TAB_LABEL.to_string());
    let active = labels.len().saturating_sub(1);
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let mut lines = header_tab_rows(header, Some(&combined), Some((active, frame)), width);
    lines.resize(super::TAB_BAR_ROWS, String::new());
    let body = rows.saturating_sub(lines.len());
    lines.extend(loading_tab_body!(width, body, frame));
    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// Builds the 集中 (Closeup) right pane. With no live panes it is a blank pane
/// behind the session's floating action surface — the Menu or the Prompt, both
/// composited as overlay modals by [`render_frame`] (see
/// [`HomeState::closeup_action_overlay`]). With live panes it gains a **tab strip**:
/// one chip per live pane followed by a "+ new" chip while that launch surface is
/// selected, the session identity beside it (shared with 没入), and below it the
/// selected pane's live preview — on the "+ new" tab the pane stays blank because
/// the action surface again floats as an overlay. After a zoom-out that surface
/// floats over the selected pane tab instead: the preview drawn here keeps
/// showing behind it.
fn closeup_pane(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // No live panes: the action surface (Menu or Prompt) floats as an overlay
    // modal centred over the pane (composited by [`render_frame`] when
    // [`HomeState::closeup_action_overlay`] holds), so the pane behind it stays
    // blank — neither surface renders inline.
    let Some(strip) = state.terminal_tabs().filter(|s| !s.labels.is_empty()) else {
        return blank_pane(rows);
    };

    // Live panes: the session's panes as tabs. The "+ new" tab is appended only
    // while it is the selected tab — the launch surface the user is acting on —
    // so stepping off it (e.g. `Esc` after `Ctrl-T`) drops the chip rather than
    // leaving a stale "+ new" sitting on the strip. The identity rides the
    // strip's row (as in 没入), so the body below carries no header of its own.
    let on_new = state.closeup_on_new_tab();
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
    // modal (see [`HomeState::closeup_action_overlay`]), so only the tab strip shows
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
fn overview_input_pane(
    mut lines: Vec<String>,
    hint: &str,
    width: usize,
    rows: usize,
) -> Vec<String> {
    let body_rows = rows.saturating_sub(1);
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines.push(style(clip_to_width(hint, width)).dim().to_string());
    lines
}

/// The 選択 (Overview) name input rendered in the **right pane** while creating a
/// session with the sidebar collapsed to the rail: a header, the typed name in a
/// bordered box with a block caret, the live validation error (or a hint) below
/// it, and the key hint pinned to the bottom row. At full width the input rides
/// the left pane inline instead (see [`super::overview_create_rows`]).
fn overview_create_pane(create: &CreateInput, width: usize, rows: usize) -> Vec<String> {
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
    overview_input_pane(lines, "Enter 作成 / Esc 取消", width, rows)
}

/// The 選択 (Overview) display-name input rendered in the **right pane** while
/// renaming a session with the sidebar collapsed to the rail: a header naming the
/// session, the typed label in a bordered box with a block caret, a hint, and the
/// key hint pinned to the bottom row. At full width there is room to edit on the
/// row itself, so the session's own name line becomes the editable label in place
/// (see the `rename` branch of [`worktree_row`]) and this pane is not used.
fn overview_rename_pane(rename: &RenameInput, width: usize, rows: usize) -> Vec<String> {
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
    overview_input_pane(lines, "Enter 確定 / Esc 取消", width, rows)
}

/// Most note lines the read-only 選択 note overlay shows before eliding the rest
/// with a `… (N more)` line — the full text lives in the editor (`n` / `Ctrl-E`).
const SWITCH_NOTE_MAX_LINES: usize = 6;

/// Most note lines the *editing* overlay shows at once, windowed around the
/// caret, so the box never hides the whole right pane while editing.
const EDIT_NOTE_MAX_LINES: usize = 12;

/// Most body lines an *unfocused* pane of the open editor shows. The stacked
/// note / todos / decisions boxes are all visible at once, so the two panes not
/// being edited stay compact (their overflow elided with `… (N more)`) and the
/// focused pane keeps the room.
const UNFOCUSED_NOTE_MAX_LINES: usize = 4;

/// Frame rows the three stacked editor boxes cost (a top and a bottom border
/// each); what is left of the pane height is the body budget the panes share.
const NOTE_STACK_FRAME_ROWS: usize = 6;

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
    title: &str,
    active: bool,
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
    // included) to `inner`. While the editor is open (`active`), paint the frame
    // in the accent colour so it is unmistakable — visually distinct from the
    // read-only overview note, which keeps the plain frame.
    if active {
        widgets::boxed_styled(title, inner, &body, &Style::new().accent().bold())
    } else {
        widgets::boxed(title, inner, &body)
    }
}

/// One checklist row: `[x]` / `[ ]` and the text (kept to a single line).
fn fmt_todo(todo: &SessionTodo) -> String {
    let mark = if todo.done { "x" } else { " " };
    format!("[{}] {}", mark, todo.text.replace('\n', " "))
}

/// The `note` pane box of the open editor. Focused it is the multi-line editor
/// (block caret, selection, the view windowed around the caret) with the accent
/// frame and the `note (編集中)` title; unfocused it is a plain-framed read-only
/// listing capped at `max` (overflow elided).
fn note_pane_box(editor: &NoteEditor, box_w: usize, max: usize, focused: bool) -> Vec<String> {
    if focused {
        note_box(
            editor.area().lines(),
            Some(editor.area().cursor()),
            editor.area().selection(),
            box_w,
            max,
            "note (編集中)",
            true,
        )
    } else {
        note_box(editor.area().lines(), None, None, box_w, max, "note", false)
    }
}

/// The `todos` pane box of the open editor. Focused it is the interactive
/// checklist — the highlighted row marked with `›`, and, while the add / edit
/// input is open, a one-line input row (`+`/`✎` prefixed) with a block caret —
/// under the accent frame and the `todos (編集中)` title. Unfocused it is a
/// plain-framed read-only listing (no `›`: the selection is a focus concern),
/// capped at `max`. A placeholder stands in when the checklist is empty.
fn todos_box(editor: &NoteEditor, box_w: usize, max: usize, focused: bool) -> Vec<String> {
    if !focused {
        let lines: Vec<String> = if editor.todos().is_empty() {
            vec!["(todo なし)".to_string()]
        } else {
            editor.todos().iter().map(fmt_todo).collect()
        };
        return note_box(&lines, None, None, box_w, max, "todos", false);
    }
    match editor.todo_input() {
        None => {
            let sel = editor.selected_todo();
            let lines: Vec<String> = if editor.todos().is_empty() {
                vec!["(todo なし)".to_string()]
            } else {
                editor
                    .todos()
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let marker = if i == sel { "› " } else { "  " };
                        format!("{marker}{}", fmt_todo(t))
                    })
                    .collect()
            };
            note_box(&lines, None, None, box_w, max, "todos (編集中)", true)
        }
        Some(input) => {
            // The existing rows (plain) followed by the inline input row, with the
            // caret on it.
            let mut lines: Vec<String> = editor
                .todos()
                .iter()
                .map(|t| format!("  {}", fmt_todo(t)))
                .collect();
            let label = if input.is_editing() { "✎ " } else { "+ " };
            lines.push(format!("{label}{}", input.input().value()));
            let caret_row = lines.len() - 1;
            let caret_col = label.len() + input.input().cursor();
            note_box(
                &lines,
                Some((caret_row, caret_col)),
                None,
                box_w,
                max,
                "todos (編集中)",
                true,
            )
        }
    }
}

/// The `decisions` pane box of the open editor: the read-only decision log,
/// capped at `max`. The pane never takes editing keys, so its focused title is
/// `decisions (表示中)` (viewing, not editing) over the accent frame; unfocused
/// it keeps the plain frame and the bare title.
fn decisions_box(editor: &NoteEditor, box_w: usize, max: usize, focused: bool) -> Vec<String> {
    let title = if focused {
        "decisions (表示中)"
    } else {
        "decisions"
    };
    note_box(
        &decision_lines(editor.decisions()),
        None,
        None,
        box_w,
        max,
        title,
        focused,
    )
}

/// One pane box of the open editor, dispatched by `pane` (see the per-pane
/// builders above). `max` caps the body; `focused` selects the editing surface
/// and the accent frame.
fn editor_pane_box(
    editor: &NoteEditor,
    pane: NotePane,
    box_w: usize,
    max: usize,
    focused: bool,
) -> Vec<String> {
    match pane {
        NotePane::Note => note_pane_box(editor, box_w, max, focused),
        NotePane::Todos => todos_box(editor, box_w, max, focused),
        NotePane::Decisions => decisions_box(editor, box_w, max, focused),
    }
}

/// The open editor's overlay: the `note` / `todos` / `decisions` boxes stacked
/// top to bottom in [`NotePane::all`] order, all visible at once. The pane
/// height minus the three frames ([`NOTE_STACK_FRAME_ROWS`]) is the body
/// budget: each unfocused pane gets a compact cap (a quarter of the budget, at
/// most [`UNFOCUSED_NOTE_MAX_LINES`]) and the focused pane the rest (at most
/// [`EDIT_NOTE_MAX_LINES`]). On a pane too short to give all three panes even
/// one body line, the overlay falls back to the focused pane alone — the same
/// single box the editor used to be — so a tiny terminal still edits.
fn editor_stack(editor: &NoteEditor, box_w: usize, rows: usize) -> Vec<String> {
    let budget = rows.saturating_sub(NOTE_STACK_FRAME_ROWS);
    if budget < NotePane::all().len() {
        let max = EDIT_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        return editor_pane_box(editor, editor.focus(), box_w, max, true);
    }
    let unfocused = (budget / 4).clamp(1, UNFOCUSED_NOTE_MAX_LINES);
    let focused = (budget - 2 * unfocused).min(EDIT_NOTE_MAX_LINES);
    NotePane::all()
        .into_iter()
        .flat_map(|pane| {
            let has_focus = pane == editor.focus();
            let max = if has_focus { focused } else { unfocused };
            editor_pane_box(editor, pane, box_w, max, has_focus)
        })
        .collect()
}

/// The decisions pane body: one `MM-DD HH:MM  text` line per logged decision
/// (newlines flattened so each entry stays one row), or a placeholder when none
/// have been recorded.
fn decision_lines(decisions: &[SessionDecision]) -> Vec<String> {
    if decisions.is_empty() {
        return vec!["(記録なし)".to_string()];
    }
    decisions
        .iter()
        .map(|d| {
            format!(
                "{}  {}",
                d.at.format("%m-%d %H:%M"),
                d.text.replace('\n', " ")
            )
        })
        .collect()
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
/// **editor** (when open, in any mode) wins: its `note` / `todos` / `decisions`
/// boxes stack in one top-right column (see [`editor_stack`]), the focused pane
/// marked by the accent frame and title. Otherwise the highlighted session's
/// **read-only** note shows while browsing in 選択 (see
/// [`HomeState::visible_overview_note`]). The column is narrow (see
/// [`note_box_width`]) and composited over the pane by [`right_pane_contents`],
/// so the preview underneath — the session header, the live terminal — stays
/// readable to its left and below it. `rows` caps the overlay height so the pane
/// stays partly visible behind it; `width` is the full right-pane width (the
/// boxes narrow themselves within it).
fn note_overlay(state: &HomeState, width: usize, rows: usize) -> Option<Vec<String>> {
    let box_w = note_box_width(width);
    if let Some(editor) = state.note_editor() {
        return Some(editor_stack(editor, box_w, rows));
    }
    if let Some(note) = state.visible_overview_note() {
        let cap = SWITCH_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        let note_lines: Vec<String> = note.lines().map(str::to_string).collect();
        return Some(note_box(&note_lines, None, None, box_w, cap, "note", false));
    }
    None
}

/// Witty English one-liners rested beneath the idle mascot in the 選択 preview
/// (and [`idle_quip`] picks one per session). They stand in for the 集中 menu's
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

/// The 選択 (Overview) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; an idle session with no live pane rests the mascot
/// with a light quip ([`idle_rabbit_body`]) on the menu UI, or previews its inline
/// prompt on the prompt UI. The header line carries the session's status and agent
/// state. The key hints live in the footer, so the preview uses the pane's full
/// height. The highlighted session's note is drawn over the top by [`note_overlay`]
/// (not inline), so it never pushes this preview around.
pub(super) fn overview_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
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
    // ([`overview_tab_at`]), so they are built once in [`overview_preview_header`]
    // and shared — the strip a click lands on is exactly the one drawn here.
    let (header, live) = overview_preview_header(state);
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
        // 選択 preview is where the user waits while it loads).
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
        // Selecting opens 集中's action surface (which floats as an overlay modal for
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

/// Header text for 選択's right-pane preview, plus whether the highlighted row is
/// live. `overview_preview` and `overview_tab_at` share this so tab chips are measured
/// from the same header text the renderer puts beside them.
pub(super) fn overview_preview_header(state: &HomeState) -> (String, bool) {
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
    let agent = AgentLifecycle::from_flags(live, running, waiting, done).detail(HEADER_AGENT_COL);
    (preview_header(&name, status, agent), live)
}

/// The right pane's contents, by mode. A preview of the would-be session screen
/// in 選択 (the default); the session's action surface — a menu or a prompt, per
/// [`SessionActionUi`] — in 集中; and the live embedded terminal in 没入 (a
/// starting hint until the first snapshot arrives).
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    // A momentary launch (terminal / agent spawn) blanks the right pane so the
    // centred loading rabbit (composited over the frame in [`super::frame`])
    // reads as a dedicated loading screen. Without this the 集中 action menu
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
    // regardless of mode (opened from 集中's `chat`, it captures the keyboard
    // while shown). The sidebar to its left keeps rendering as usual.
    if let Some(chat) = state.chat() {
        return crate::presentation::tui::chat::ui::pane(chat, right_w, rows);
    }
    if let Some(frame) = state.pending_diff_frame() {
        if state.mode() == Mode::Closeup {
            return pending_diff_tab_pane(state, frame, right_w, rows);
        }
        return loading_tab_body!(right_w, rows, frame);
    }
    // The diff view is a session tab: opened from 集中 (the `diff` command), it
    // reads as a `diff` tab beside the session's panes — the tab strip heads it
    // with an active `diff` chip, and the split view fills the body below. Outside
    // 集中 (there is no session strip to sit in) it takes the whole pane.
    if let Some(diff) = state.diff_view() {
        if state.mode() == Mode::Closeup {
            return diff_tab_pane(state, diff, right_w, rows);
        }
        return diff_pane(diff, right_w, rows);
    }
    // The base pane for the current mode. The session-note overlay (the editor,
    // or the read-only note while browsing in 選択) is composited over its top
    // below, so editing / reading the note never switches the screen — the
    // preview / terminal stays visible behind the floating box.
    let mut base = match state.mode() {
        Mode::Switch => {
            // Collapsed to the rail, 選択's name input has no room inline in the
            // (5-column) list, so it takes over the wide right pane; at full width
            // it rides the left pane inline and the right pane keeps previewing the
            // highlighted session.
            if state.sidebar() == Sidebar::Rail {
                if let Some(create) = state.create() {
                    return overview_create_pane(create, right_w, rows);
                }
                if let Some(rename) = state.rename() {
                    return overview_rename_pane(rename, right_w, rows);
                }
            }
            // Fade the whole preview in 選択: the keyboard is on the session list
            // to the left, so dimming the right pane signals it is not the focus —
            // the highlighted session and its tabs are browsed there, not selected
            // from here. The note box (when open) is composited bright on top below,
            // so the deliberately-opened note still reads against the faded preview.
            overview_preview(state, right_w, rows)
                .iter()
                .map(|row| dim_row(row))
                .collect()
        }
        Mode::Closeup if !state.closeup_attached() => closeup_pane(state, right_w, rows),
        Mode::Closeup => {
            // The active session's identity shares the top row with its tab chips
            // (the underline marker below them), so the header reads beside the
            // tabs just as it does in 選択. This header + tab block always fills
            // exactly `TAB_BAR_ROWS`, matching `attached_geometry`, so the embedded
            // terminal below never shifts whether or not a strip is published. A
            // starting hint stands in until the first screen snapshot arrives.
            let mut lines = Vec::with_capacity(rows);
            let header = active_session_header(state);
            let mut head = header_tab_rows(
                header,
                state.terminal_tabs(),
                // A background pane loads while the user waits in the 選択 preview,
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
