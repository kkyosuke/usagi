//! The chrome around the body: the title bar and engagement-ladder indicator,
//! the mode-aware command input and footer, the `:` command palette overlay
//! (with its command hints), and the session-removal / quit-confirmation
//! modals. All functions take plain data and return styled lines.

use console::{style, Style};

use super::super::command::{CommandHint, Hint};
use super::super::state::{HomeState, Mode, RemoveModal, TextModal, WorktreeList};
use super::super::tasks::{TaskMark, TaskRow};
use super::panes::log_line;
use super::{
    clip_to_width, pad_to_width, HINT_INDENT, HINT_MAX, HINT_NAME_COL, REMOVE_MODAL_VISIBLE,
    TEXT_MODAL_VISIBLE,
};
use crate::presentation::tui::widgets;

/// Minimum / maximum display width of the active-session-name field in the
/// title bar. The field scales with the terminal (a quarter of its width) and
/// is clamped to this range, so a roomy window shows more of a long name while a
/// narrow one stays compact. A long name is clipped to the chosen width, a short
/// one padded out — and since the width depends only on the (per-frame constant)
/// terminal size, never the name text, the label keeps the same width every
/// frame, so the centred title never shifts as the active session changes.
const TITLE_NAME_MIN_W: usize = 12;
const TITLE_NAME_MAX_W: usize = 24;

/// The centred title bar: workspace name and session count. The count covers
/// every row in the left pane — the root row plus each session (one row per
/// session, not per repository) — so it matches what the user sees.
pub(super) fn title_bar(width: usize, list: &WorktreeList) -> String {
    let count = list.session_count();
    // The active session's name rides in the title so it is identifiable even
    // when the sidebar is collapsed to the rail (which shows no names). `▸` marks
    // it; the root row reads as the workspace itself.
    //
    // Pin the name to a fixed-width field (clipped if long, padded if short) so
    // the whole label keeps a constant width and the centred bar stays put as
    // the active session changes — a longer name no longer pushes it sideways.
    let name_w = (width / 4).clamp(TITLE_NAME_MIN_W, TITLE_NAME_MAX_W);
    let name = pad_to_width(clip_to_width(list.active_name(), name_w), name_w);
    let label = format!(
        "{} · ▸ {name} · {count} session{}",
        list.workspace_name(),
        if count == 1 { "" } else { "s" }
    );
    widgets::title_line(width, &label)
}

/// Minimum / maximum display width of the task-status label field. The field
/// scales with the terminal (a quarter of its width) and is clamped to this
/// range, so a roomy window shows more of a long session name while a narrow
/// one stays compact. A long name is clipped to the chosen width, a short one
/// padded out — and since the width depends only on the (per-frame constant)
/// terminal size, never the label text, the block stays the same size every
/// frame and never shifts as the label changes (`作成中…` → `作成完了`) or the
/// spinner ticks.
const TASK_LABEL_MIN_W: usize = 16;
const TASK_LABEL_MAX_W: usize = 32;

/// Display width of the `done/total` count field, left-padded so the progress
/// row stays right-flush. Wide enough for two-digit batches (`12/12`).
const TASK_COUNT_W: usize = 5;

/// The top-right background-task status block: two fixed-width rows showing the
/// in-flight (or just-finished) session create / remove work. The first row is a
/// mark plus a representative label; the second, indented under the label, is a
/// batch-progress bar plus a `done/total` count. The mark leads with a spinning
/// braille glyph (cyan) while anything runs, or `✓` (green) / `✗` (red) once
/// everything has settled. Returns no lines when nothing is tracked, so the
/// corner falls back to the update notice.
///
/// Anchored to the **two header rows** (the centred title bar and mode ladder,
/// whose right columns are blank) by [`overlay_top_right`](super::overlay_top_right)
/// rather than over the body, so it never collides with the right pane's preview
/// / menu / live terminal. Splitting onto two rows lets the label field use more
/// of the terminal's width than a single corner line could. The bar is a real
/// ratio — the share of the tracked tasks that have finished — not a per-task
/// percentage git cannot report. Both rows are the same width (the icon column
/// plus the label field) so the block right-aligns cleanly and never changes
/// size frame to frame.
pub(super) fn task_status_line(rows: &[TaskRow], width: usize) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    // The representative row: the first still-running task, else the last
    // finished one (so once a batch completes the line settles on its outcome).
    let lead = rows
        .iter()
        .find(|row| matches!(row.mark, TaskMark::Running(_)))
        .or_else(|| rows.last())
        .expect("rows is non-empty");
    let (icon, icon_style) = match lead.mark {
        TaskMark::Running(frame) => (
            widgets::spinner_char(frame).to_string(),
            Style::new().cyan().bold(),
        ),
        TaskMark::Done(true) => ("✓".to_string(), Style::new().green().bold()),
        TaskMark::Done(false) => ("✗".to_string(), Style::new().red().bold()),
    };
    // Scale the label field with the terminal, clamped so the block still tucks
    // into the blank gap beside the centred title / mode ladder. Constant for
    // the whole frame, so the right-anchored block never shifts.
    let label_w = (width / 4).clamp(TASK_LABEL_MIN_W, TASK_LABEL_MAX_W);
    let label = pad_to_width(clip_to_width(&lead.label, label_w), label_w);
    let done = rows
        .iter()
        .filter(|row| matches!(row.mark, TaskMark::Done(_)))
        .count();
    let total = rows.len();
    // The progress row spans the label field exactly: the bracketed bar
    // (`inner` + 2 brackets), a space, then the right-flush count sum to
    // `label_w`, so the second row lines up under the first and shares its width.
    let bar_inner = label_w.saturating_sub(2 + 1 + TASK_COUNT_W);
    let bar = widgets::progress_bar(done, total, bar_inner);
    // Left-pad the count to a fixed field so the row stays right-flush.
    let count = format!("{done}/{total}");
    let count = format!(
        "{}{count}",
        " ".repeat(TASK_COUNT_W.saturating_sub(count.len()))
    );
    // Row 1: mark + label. Row 2: two-space indent (under the label, past the
    // `icon + space` column) + bar + count.
    let line1 = format!("{} {label}", icon_style.apply_to(&icon));
    let line2 = format!("  {} {}", style(bar).dim(), style(count).dim());
    vec![line1, line2]
}

/// The engagement-ladder indicator drawn just under the title bar: the three
/// modes in order with the current one highlighted (cyan-bold) and the rest
/// dimmed, so the screen always shows which step the keys act on. Centred for
/// the terminal width.
pub(super) fn mode_ladder(width: usize, current: Mode) -> String {
    const STEPS: [(Mode, &str); 3] = [
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

/// Renders one command-hint row: a `›` marker for the highlighted best match,
/// the command name with its already-typed prefix emphasised, and the dimmed
/// description, clipped to `width`.
pub(super) fn command_hint_row(
    hint: &CommandHint,
    typed_len: usize,
    selected: bool,
    width: usize,
) -> String {
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

/// The advisory hint lines drawn in the command palette (`:`): the matching
/// commands while the command word is typed, or the usage and examples once a
/// known command is given arguments. Empty while the palette is closed.
pub(super) fn hint_lines(state: &HomeState, width: usize) -> Vec<String> {
    if !state.command_palette_open() {
        return Vec::new();
    }
    match state.hint() {
        Hint::Commands(hints) => {
            let typed = state.input().trim_start();
            // Only point a marker at a best match once something is typed; a
            // bare prompt shows the whole menu with nothing pre-selected.
            let highlight = !typed.is_empty();
            // The palette line is always workspace-scoped; a partial match just
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

/// The command input line, by mode: a left-pane hint in 切替 (Switch), the
/// focused session in 在席 (Focus), and a live-terminal status in 没入
/// (Attached). The workspace command line is the `:` palette overlay, not this
/// resident line.
pub(super) fn input_line(state: &HomeState) -> String {
    match state.mode() {
        Mode::Switch => style(" Pick a session".to_string()).dim().to_string(),
        Mode::Focus => style(format!(
            " Operating session: {}",
            state.focused_session_name()
        ))
        .dim()
        .to_string(),
        Mode::Attached => style(" ● live terminal".to_string()).green().to_string(),
    }
}

/// The command palette line as `❯ <text>` with the caret drawn at the editing
/// position (the byte offset from [`HomeState::cursor`]), so ←/→/Home/End move a
/// visible caret through the text instead of always sitting at the end.
fn command_input_content(state: &HomeState) -> String {
    let prompt = style("❯").red().bold();
    let input = state.input();
    let (before, after) = input.split_at(state.cursor());
    let value = widgets::block_caret(before, after, &Style::new().cyan());
    format!("{prompt} {value}")
}

/// Inner (content) width of the command palette box, before the borders and the
/// space of padding [`widgets::boxed`] adds on each side.
pub(super) const PALETTE_INNER: usize = 60;

/// Rows the palette reserves for the advisory hints, always filled to this height
/// (padded with blanks): a header plus up to [`HINT_MAX`] matches plus an
/// `… and N more` overflow line. Reserving a fixed block keeps the box the same
/// height as the match count changes while typing, so it never jumps (no layout
/// shift).
const PALETTE_HINT_ROWS: usize = HINT_MAX + 2;

/// Rows the palette reserves for the latest command's response, always filled to
/// this height (padded with blanks): an `↑ N more` overflow line plus up to
/// [`PALETTE_RESPONSE_MAX`] of the newest output lines. Fixed, like the hint
/// block, so running a command does not resize the box.
const PALETTE_RESPONSE_ROWS: usize = 6;

/// Most response lines the palette shows at once; older lines are elided behind
/// an `↑ N more` summary so the (fixed-height) response block never overflows.
const PALETTE_RESPONSE_MAX: usize = PALETTE_RESPONSE_ROWS - 1;

/// Builds the body of the workspace command palette (`:`) at a **fixed height**:
/// the `❯ <input>` command line (with a block caret), a fixed-height block of
/// advisory command hints, a fixed-height block of the latest command's response
/// (capped, with an `↑ N more` line when longer), and a key-hint footer. Every
/// region is padded to a constant number of rows so the box keeps the same size
/// as the user types and runs commands — it never grows or shrinks.
pub(super) fn command_palette_body(state: &HomeState, inner: usize) -> Vec<String> {
    let mut body = Vec::with_capacity(PALETTE_HINT_ROWS + PALETTE_RESPONSE_ROWS + 5);
    body.push(command_input_content(state));
    body.push(String::new());

    // The advisory hints (matching commands, or the usage of a known command),
    // padded to a fixed height so a changing match count never resizes the box.
    let mut hints = hint_lines(state, inner);
    hints.truncate(PALETTE_HINT_ROWS);
    pad_block(&mut body, hints, PALETTE_HINT_ROWS);

    body.push(String::new());

    // The latest command's response, capped so a long dump does not swallow the
    // box (the overflow is summarised with an `↑ N more` line) and padded to a
    // fixed height so running a command never resizes the box.
    let response = state.response_lines();
    let mut rows = Vec::new();
    if !response.is_empty() {
        let total = response.len();
        let start = total.saturating_sub(PALETTE_RESPONSE_MAX);
        if start > 0 {
            rows.push(style(format!("  ↑ {start} more")).dim().to_string());
        }
        for line in &response[start..] {
            rows.push(log_line(line, inner));
        }
    }
    rows.truncate(PALETTE_RESPONSE_ROWS);
    pad_block(&mut body, rows, PALETTE_RESPONSE_ROWS);

    body.push(String::new());
    body.push(
        style("Enter: run   Tab: complete   ↑↓ history   Esc: close")
            .dim()
            .to_string(),
    );
    body
}

/// Appends `rows` to `body`, then pads with blank lines up to `height` so the
/// region always occupies a fixed number of rows. `rows` is expected to already
/// be no longer than `height`.
fn pad_block(body: &mut Vec<String>, rows: Vec<String>, height: usize) {
    let filled = rows.len();
    body.extend(rows);
    for _ in filled..height {
        body.push(String::new());
    }
}

/// The footer help line, aware of the current mode. It leads with a mode tag so
/// it is always clear which engagement level the keys act on.
pub(super) fn footer_line(width: usize, state: &HomeState) -> String {
    // The note editor captures the keyboard while open (rendered in the right
    // pane, so the screen never switches), so its controls take over the footer.
    if state.note_editor().is_some() {
        return widgets::dim_line(
            width,
            "[note]  Ctrl-S: save / Esc: cancel / Enter: newline / ←→↑↓: move / Shift+←→↑↓: select",
        );
    }
    // The preview captures the keyboard, so its controls take over the footer
    // regardless of the underlying mode.
    if state.preview().is_some() {
        return widgets::dim_line(
            width,
            "[preview]  ↑↓ scroll / PgUp/PgDn page / Esc / q: close",
        );
    }
    // The command palette captures the keyboard while open, so its controls take
    // over the footer regardless of the underlying mode.
    if state.command_palette_open() {
        return widgets::dim_line(
            width,
            "[command]  Tab: complete / ↑↓: history / Enter: run / Esc: close",
        );
    }
    let help = match state.mode() {
        Mode::Switch => {
            // While the highlighted session's note is showing, `Esc` first hides
            // it (a second `Esc` then backs out), so the footer names that.
            let esc = if state.switch_note_visible() {
                "Esc close note"
            } else {
                "Esc back"
            };
            format!(
                "[switch]  ↑↓ session / ←→ tab / Enter focus / c new / r rename / n/Ctrl-E note / x close tab / : commands / {esc}"
            )
        }
        Mode::Focus => {
            format!(
                "[session: {}]  Ctrl-N/P: tab / Enter: open/run / Ctrl-O: switch / Ctrl-^: last / Ctrl-E: note / : commands / Esc: switch",
                state.focused_session_name()
            )
        }
        Mode::Attached => {
            "[attached]  Ctrl-O: switch / Ctrl-T: focus / Ctrl-^: last / Ctrl-N/P: tab / Ctrl-G: agent / Ctrl-E: note"
                .to_string()
        }
    };
    widgets::dim_line(width, &help)
}

/// Builds the inline create row appended to the left pane in 切替 (Switch) while
/// naming a new session: `+ new: <name>` with a block caret on the character
/// being edited (`cursor`, a byte offset into `input`) and an inline error below
/// it. The rows are clipped to the pane width.
pub(super) fn switch_create_rows(
    input: &str,
    cursor: usize,
    error: Option<&str>,
    left_w: usize,
) -> Vec<String> {
    let base = Style::new().green().bold();
    let (before, after) = input.split_at(cursor);
    let value = widgets::block_caret(before, after, &base);
    let label = clip_to_width(&format!("{}{value}", base.apply_to("+ new: ")), left_w);
    let mut rows = vec![label];
    if let Some(err) = error {
        rows.push(style(clip_to_width(err, left_w)).red().to_string());
    }
    rows
}

/// Builds the inline rename row appended to the left pane in 切替 (Switch) while
/// editing a session's sidebar label: `rename <target>: <input>` with a block
/// caret on the character being edited (`cursor`, a byte offset into `input`),
/// clipped to the pane width. The label is cosmetic, so there is no validation
/// row.
pub(super) fn switch_rename_rows(
    target: &str,
    input: &str,
    cursor: usize,
    left_w: usize,
) -> Vec<String> {
    let base = Style::new().cyan().bold();
    let (before, after) = input.split_at(cursor);
    let value = widgets::block_caret(before, after, &base);
    let label = clip_to_width(
        &format!("{}{value}", base.apply_to(format!("rename {target}: "))),
        left_w,
    );
    vec![label]
}

/// Builds one removal-modal row: a `>` cursor for the highlighted entry, a
/// `[x]` / `[ ]` checkbox for its selection, and the (clipped) session name.
/// The cursored row is emphasised, a checked row stays bright, and the rest are
/// dimmed.
pub(super) fn remove_modal_row(name: &str, cursor: bool, selected: bool, inner: usize) -> String {
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
pub(super) fn remove_modal_frame(
    raw_height: usize,
    raw_width: usize,
    modal: &RemoveModal,
) -> Vec<String> {
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

/// Builds the centred quit-confirmation modal. `Ctrl-C` raises it only when a
/// session is still live (naming how many are running and warning they will be
/// stopped); `Ctrl-Q` always raises it, so with nothing live (`live == 0`) it
/// asks a plain "quit?" instead of warning about agents that are not running.
pub(super) fn quit_confirm_frame(raw_height: usize, raw_width: usize, live: usize) -> Vec<String> {
    // Wide enough for the longest body line ("Close anyway? Running agents
    // will be stopped." = 45 columns) so it does not overflow the box.
    const INNER: usize = 46;
    let body = if live == 0 {
        vec![
            style("No sessions are running.").dim().to_string(),
            String::new(),
            style("Quit usagi?").to_string(),
            String::new(),
            style("y / Enter: quit   n / Esc: cancel").dim().to_string(),
        ]
    } else {
        vec![
            style(format!("{live} session(s) still running."))
                .dim()
                .to_string(),
            String::new(),
            style("Close anyway? Running agents will be stopped.").to_string(),
            String::new(),
            style("y / Enter: close   n / Esc: cancel")
                .dim()
                .to_string(),
        ]
    };
    widgets::render_modal(raw_height, raw_width, "Quit usagi?", INNER, &body)
}

/// Inner (content) width of the text modal box, before the borders and the
/// space of padding [`widgets::boxed`] adds on each side.
pub(super) const TEXT_MODAL_INNER: usize = 60;

/// Builds the body of the text modal: a scrollable window over a text-dumping
/// command's output (`man` / `history` / `session list`), coloured by line kind,
/// with `↑`/`↓` more-counts and the dismiss hint below.
///
/// Like the `:` command palette, this is only the body (no border): `inner` is
/// the box's content width, and [`render_frame`](super::render_frame) wraps it
/// and floats it over the live workspace with [`widgets::overlay_modal`] so the
/// panes stay visible around it, rather than a black backdrop.
pub(super) fn text_modal_body(modal: &TextModal, inner: usize) -> Vec<String> {
    let total = modal.lines.len();
    let start = modal.scroll.min(total.saturating_sub(TEXT_MODAL_VISIBLE));
    let end = (start + TEXT_MODAL_VISIBLE).min(total);

    let mut body = Vec::new();
    if start > 0 {
        body.push(style(format!("  ↑ {start} more")).dim().to_string());
    }
    for line in &modal.lines[start..end] {
        body.push(log_line(line, inner));
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
    body
}
