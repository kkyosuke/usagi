//! The chrome around the body: the title bar and engagement-ladder indicator,
//! the mode-aware command input and footer, the Overview command hints, and the
//! session-removal / quit-confirmation modals. All functions take plain data
//! and return styled lines.

use console::style;

use crate::domain::version::Version;

use super::super::command::{CommandHint, Hint};
use super::super::state::{HomeState, Mode, RemoveModal, TextModal, WorktreeList};
use super::panes::log_line;
use super::{
    clip_to_width, pad_to_width, CARET, HINT_INDENT, HINT_MAX, HINT_NAME_COL, REMOVE_MODAL_VISIBLE,
    TEXT_MODAL_VISIBLE,
};
use crate::presentation::tui::widgets;

/// The centred title bar: workspace name and session count. The count covers
/// every row in the left pane — the root row plus each session (one row per
/// session, not per repository) — so it matches what the user sees.
pub(super) fn title_bar(width: usize, list: &WorktreeList) -> String {
    let count = list.session_count();
    let label = format!(
        "{} · {count} session{}",
        list.workspace_name(),
        if count == 1 { "" } else { "s" }
    );
    widgets::title_line(width, &label)
}

/// The top-right "update available" notice: the usagi mascot beside a short
/// note that a release newer than the running build (`latest`) has been
/// published. Shown only while the background update check reports one.
///
/// Each returned line pairs a mascot row with its message and is right-padded to
/// a common block width and styled yellow-bold, so the block right-aligns
/// cleanly when [`overlay_top_right`](super::overlay_top_right) anchors it to the
/// top rows.
pub(super) fn update_banner(latest: &Version) -> Vec<String> {
    let art = widgets::rabbit_art();
    let art_w = art
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    // One message per mascot row; the last row carries only the mascot's feet.
    let messages = [
        "最新版があります".to_string(),
        format!("v{latest}"),
        String::new(),
    ];
    let rows: Vec<String> = art
        .iter()
        .zip(messages.iter())
        .map(|(line, message)| {
            let cell = pad_to_width((*line).to_string(), art_w);
            if message.is_empty() {
                cell
            } else {
                format!("{cell}  {message}")
            }
        })
        .collect();
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            style(pad_to_width(row, block_w))
                .yellow()
                .bold()
                .to_string()
        })
        .collect()
}

/// The engagement-ladder indicator drawn just under the title bar: the four
/// modes in order with the current one highlighted (cyan-bold) and the rest
/// dimmed, so the screen always shows which step the keys act on. Centred for
/// the terminal width.
pub(super) fn mode_ladder(width: usize, current: Mode) -> String {
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

/// The advisory hint lines drawn just above the command input in 統括: the
/// matching commands while the command word is typed, or the usage and examples
/// once a known command is given arguments. Empty outside Overview.
pub(super) fn hint_lines(state: &HomeState, width: usize) -> Vec<String> {
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
pub(super) fn input_line(state: &HomeState) -> String {
    match state.mode() {
        Mode::Overview => format!(" {}", overview_input_content(state)),
        Mode::Switch => style(
            " Pick a session — ↑↓ session, ←→ tab, Enter focus, t new, c new, r rename".to_string(),
        )
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

/// The 統括 (Overview) command input rendered as a bordered field — an
/// HTML-input-like box — so it reads clearly as *where you type*, set apart from
/// the hints above it and the results band below. Spans the full `width` (three
/// rows: top border, the `❯ <input>▏` line, bottom border).
pub(super) fn overview_input_box(state: &HomeState, width: usize) -> Vec<String> {
    let content = overview_input_content(state);
    // `boxed` adds the two borders and one space of padding on each side, so the
    // inner content area is the width less those four columns.
    widgets::boxed("", width.saturating_sub(4), &[content])
}

/// The Overview command line as `❯ <text>` with the caret drawn at the editing
/// position (the byte offset from [`HomeState::cursor`]), so ←/→/Home/End move a
/// visible caret through the text instead of always sitting at the end.
fn overview_input_content(state: &HomeState) -> String {
    let prompt = style("❯").red().bold();
    let input = state.input();
    let (before, after) = input.split_at(state.cursor());
    let before = style(before).cyan();
    let after = style(after).cyan();
    format!("{prompt} {before}{CARET}{after}")
}

/// The footer help line, aware of the current mode. It leads with a mode tag so
/// it is always clear which engagement level the keys act on.
pub(super) fn footer_line(width: usize, state: &HomeState) -> String {
    let help = match state.mode() {
        Mode::Overview => {
            "[overview]  Tab: complete / ↑↓: history / Enter: run / \"session switch\": pick session"
                .to_string()
        }
        Mode::Switch => {
            "[switch]  ↑↓ session / ←→ tab / Enter focus / t new / c new / r rename / Esc back / Ctrl-O overview"
                .to_string()
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
/// naming a new session: `+ new: <before>▏<after>`, with the caret drawn at the
/// editing position (`cursor`, a byte offset into `input`) and an inline error
/// below it. The rows are clipped to the pane width.
pub(super) fn switch_create_rows(
    input: &str,
    cursor: usize,
    error: Option<&str>,
    left_w: usize,
) -> Vec<String> {
    let (before, after) = input.split_at(cursor);
    let label = clip_to_width(&format!("+ new: {before}{CARET}{after}"), left_w);
    let mut rows = vec![style(label).green().bold().to_string()];
    if let Some(err) = error {
        rows.push(style(clip_to_width(err, left_w)).red().to_string());
    }
    rows
}

/// Builds the inline rename row appended to the left pane in 切替 (Switch) while
/// editing a session's sidebar label: `rename <target>: <input>▏`, clipped to the
/// pane width. The label is cosmetic, so there is no validation row.
pub(super) fn switch_rename_rows(target: &str, input: &str, left_w: usize) -> Vec<String> {
    let label = clip_to_width(&format!("rename {target}: {input}{CARET}"), left_w);
    vec![style(label).cyan().bold().to_string()]
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

/// Builds the centred quit-confirmation modal, shown when the user presses
/// `Ctrl-C` while a session is still live: it names how many sessions are still
/// running and asks whether to close anyway.
pub(super) fn quit_confirm_frame(raw_height: usize, raw_width: usize, live: usize) -> Vec<String> {
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

/// Builds the centred text modal: a scrollable window over a text-dumping
/// command's output (`man` / `history` / `session list`), coloured by line kind,
/// with `↑`/`↓` more-counts and the dismiss hint below.
pub(super) fn text_modal_frame(
    raw_height: usize,
    raw_width: usize,
    modal: &TextModal,
) -> Vec<String> {
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
