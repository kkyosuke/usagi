//! 枠付きダイアログ（modal）の部品。
//!
//! 角丸の枠に本文を収める [`boxed`]、それを空の画面中央に配置する
//! [`render_modal`]、既存フレームの中央に合成する [`render_over`] を持つ。
//! 幅は表示桁数（全角 2 桁）で測り、本文が枠より広ければ [`super::clip_to_width`] で切り、
//! 短ければ空白で詰めて右端を揃える。色付け（枠色）はテーマ導入時に載せるため無色で描く。

use super::{centered_padding, clip_to_width, display_width, normalize_size};
use crate::presentation::theme::{Color, Role, Style};
use unicode_width::UnicodeWidthChar;

/// 背景の ANSI スタイルを modal へ滲ませないための SGR reset。
const RESET: &str = "\u{1b}[0m";

/// `lines` を角丸の枠に収め、`title` を上辺に埋め込んだ行を返す。各行は左右 1 桁の余白を
/// 付けて `inner_width` に揃える。返す行はまだ配置されていない（[`render_modal`] が中央寄せする）。
#[must_use]
pub fn boxed(title: &str, inner_width: usize, lines: &[String]) -> Vec<String> {
    // 両角の間の桁数: 内容領域 + 左右 1 桁ずつの余白。
    let span = inner_width + 2;
    let label = if title.is_empty() {
        String::new()
    } else {
        // タイトル（`─ ` / ` ` の飾り込み）を span に切り、長いタイトルが上辺を押し出さないようにする。
        clip_to_width(&format!("─ {title} "), span)
    };
    let label_width = display_width(&label);
    let top = format!("┌{label}{}┐", "─".repeat(span.saturating_sub(label_width)));
    let bottom = format!("└{}┘", "─".repeat(span));

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(top);
    for line in lines {
        // 先に切って枠より広い行が右辺を押し出せないようにし、短い行は詰めて
        // 各行をちょうど `inner_width` にする。
        let line = clip_to_width(line, inner_width);
        let pad = inner_width.saturating_sub(display_width(&line));
        out.push(format!("│ {line}{} │", " ".repeat(pad)));
    }
    out.push(bottom);
    out
}

/// 端末幅 `width` で modal の枠が得る内側（内容）幅: `desired` を、枠（左右の枠線 2 桁 +
/// 余白 2 桁 = 4 桁）が画面を溢れないよう詰める。呼び出し側はこの幅で本文を組むと、枠の中に
/// 行が揃う。
#[must_use]
pub fn modal_inner_width(width: usize, desired: usize) -> usize {
    desired.min(width.saturating_sub(4))
}

/// Reserve `body_height` rows for a modal body.
///
/// Modal views use this after composing their state-dependent rows so a result,
/// error, or shorter list cannot move the border while the modal is open.
/// Extra rows are clipped at the body boundary; terminal-height clipping remains
/// the responsibility of [`render_modal`] and [`render_over`].
#[must_use]
pub fn fixed_body(mut body: Vec<String>, body_height: usize) -> Vec<String> {
    body.truncate(body_height);
    body.resize(body_height, String::new());
    body
}

/// The two-column left margin shared by modal body rows. Content, captions,
/// footers, and scroll indicators all indent by this so a modal reads as one
/// column no matter which view composed it.
const BODY_INDENT: &str = "  ";

/// A dim, body-indented line. Captions, empty-state notices, and footers share
/// this one style; they differ only in role and call site, so each keeps its
/// own name below while the styling lives here once.
fn dim_body_line(text: &str) -> String {
    Style::new().dim().paint(&format!("{BODY_INDENT}{text}"))
}

/// Indent `text` by the shared body margin and clip it to `inner_width`.
///
/// This folds the `clip_to_width(format!("  {line}"), inner)` idiom the list and
/// editor modals repeated. Callers pre-style their spans; the returned line is
/// only indented and clipped, never recoloured.
#[must_use]
pub fn content_line(text: &str, inner_width: usize) -> String {
    clip_to_width(&format!("{BODY_INDENT}{text}"), inner_width)
}

/// A dim section caption, such as a list's heading row.
#[must_use]
pub fn caption(text: &str) -> String {
    dim_body_line(text)
}

/// An accent, bold heading for an editor or detail modal.
#[must_use]
pub fn heading(text: &str) -> String {
    Role::Accent
        .style()
        .bold()
        .paint(&format!("{BODY_INDENT}{text}"))
}

/// A dim empty-state notice, such as `(none)` or `no pull requests`.
#[must_use]
pub fn empty_notice(text: &str) -> String {
    dim_body_line(text)
}

/// A dim footer/help row listing the modal's key hints.
#[must_use]
pub fn footer(hints: &str) -> String {
    dim_body_line(hints)
}

/// The danger, bold `›` cursor drawn before a selected list row, or a blank
/// cell when the row is not selected. Shared with [`super::select`] so the form
/// widgets and every modal draw one cursor glyph.
#[must_use]
pub fn selection_marker(selected: bool) -> String {
    if selected {
        Role::Danger.style().bold().paint("›")
    } else {
        " ".to_string()
    }
}

/// A dim scroll indicator: `↑ N more` above the viewport or `↓ N more` below it.
fn scroll_indicator(arrow: char, hidden: usize) -> String {
    dim_body_line(&format!("{arrow} {hidden} more"))
}

/// `↑ N more` — the number of rows scrolled off the top of the viewport.
#[must_use]
pub fn scroll_above(hidden: usize) -> String {
    scroll_indicator('↑', hidden)
}

/// `↓ N more` — the number of rows below the viewport.
#[must_use]
pub fn scroll_below(hidden: usize) -> String {
    scroll_indicator('↓', hidden)
}

// ── shape composition ──────────────────────────────────────────────────────
//
// The helpers below sit one layer above the body-composition kit: they encode
// the *shape* a modal takes (a selection list, a scrolling text viewer, a
// command palette) so each view keeps only its own state, keys, and content.
// A shape owns the parts that repeat across every modal of that shape — the
// scroll viewport, the cursor row, the palette prompt — while the view decides
// what each row says.

/// The half-open window `[start, end)` of a `len`-row **selection list** that
/// keeps row `selected` visible within `capacity` rows.
///
/// The window follows the cursor: as the selection moves past the bottom edge
/// the window scrolls so the selected row sits on that edge, and it never scrolls
/// past the final row. This folds the PR list's `visible_bounds`; every
/// selection list (PR / closeup / decision) computes its viewport the same way.
#[must_use]
pub fn list_window(len: usize, selected: usize, capacity: usize) -> (usize, usize) {
    let visible = len.min(capacity);
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(len.saturating_sub(visible));
    (start, start + visible)
}

/// The half-open window `[start, end)` showing up to `capacity` rows of a
/// `len`-row **text viewer** from scroll `offset`.
///
/// Unlike [`list_window`] the anchor is the offset itself, not a selection, so
/// the reader scrolls freely; the start is clamped so a non-empty document
/// always keeps at least its last row on screen. Folds the text overlay's
/// offset math.
#[must_use]
pub fn viewport_window(len: usize, offset: usize, capacity: usize) -> (usize, usize) {
    let start = offset.min(len.saturating_sub(1));
    let end = start.saturating_add(capacity).min(len);
    (start, end)
}

/// Emit the `[start, end)` slice of `rows` bracketed by `↑ N more` / `↓ N more`
/// indicators.
///
/// This is the one scroll-viewport renderer shared by the list and text-viewer
/// shapes: rows hidden above or below the window each collapse to a single
/// indicator line, so the PR list and the text overlay no longer open-code the
/// same emission. Callers pair it with [`list_window`] or [`viewport_window`].
#[must_use]
pub fn scroll_window(rows: &[String], start: usize, end: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(end.saturating_sub(start) + 2);
    if start > 0 {
        out.push(scroll_above(start));
    }
    out.extend(rows.get(start..end).unwrap_or_default().iter().cloned());
    if end < rows.len() {
        out.push(scroll_below(rows.len() - end));
    }
    out
}

/// The **palette** input line: a danger `❯` prompt followed by the block-caret
/// rendering of `value` with the caret at byte `cursor`.
///
/// Shared by the overview and closeup palettes so every command input draws the
/// same prompt glyph and accent caret.
#[must_use]
pub fn prompt_line(value: &str, cursor: usize) -> String {
    let prompt = Role::Danger.style().bold().paint("❯");
    let body = super::block_caret(value, cursor, &Role::Accent.style());
    format!("{prompt} {body}")
}

/// A dim inline subcommand row under a palette action: a plain `›` / space
/// marker indented under its parent command. Folds the picker row the overview
/// and closeup palettes both drew; its quiet marker stays distinct from the
/// list cursor [`selection_marker`].
#[must_use]
pub fn subcommand_row(label: &str, selected: bool) -> String {
    let marker = if selected { "›" } else { " " };
    Style::new().dim().paint(&format!("      {marker} {label}"))
}

/// Reserve `body_height` rows for `lines` and render the modal centred on a
/// blank frame. The twin of [`render_body_over`]; both fold the
/// `fixed_body(…)` reserve so a view only composes its rows.
#[must_use]
pub fn render_body(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    inner_width: usize,
    body_height: usize,
    lines: Vec<String>,
) -> Vec<String> {
    render_modal(
        raw_height,
        raw_width,
        title,
        inner_width,
        &fixed_body(lines, body_height),
    )
}

/// Reserve `body_height` rows for `lines` and composite the modal over `base`.
///
/// On a short terminal the reserve is clamped to `height - 4` so a sliver of the
/// background stays visible above and below the box; normal terminals keep the
/// full reserve. The twin of [`render_body`].
#[must_use]
pub fn render_body_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    title: &str,
    inner_width: usize,
    body_height: usize,
    lines: Vec<String>,
) -> Vec<String> {
    let (height, _) = normalize_size(raw_height, raw_width);
    let reserved = body_height.min(height.saturating_sub(4));
    render_over(
        raw_height,
        raw_width,
        base,
        title,
        inner_width,
        &fixed_body(lines, reserved),
    )
}

/// Shared state for a two-choice confirmation modal.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmationModal {
    confirm_selected: bool,
}

impl ConfirmationModal {
    /// Create a modal focused on the affirmative choice.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            confirm_selected: true,
        }
    }

    /// Build a modal whose focus mirrors an externally owned selection. Callers
    /// that keep the Yes/No choice in their own (for example usecase-layer)
    /// state project it into this presentation widget without duplicating the
    /// selection API.
    #[must_use]
    pub const fn from_confirm_selected(confirm_selected: bool) -> Self {
        Self { confirm_selected }
    }

    /// Whether Yes is selected.
    #[must_use]
    pub const fn is_confirm_selected(self) -> bool {
        self.confirm_selected
    }

    /// Select Yes.
    pub fn select_confirm(&mut self) {
        self.confirm_selected = true;
    }

    /// Select No.
    pub fn select_cancel(&mut self) {
        self.confirm_selected = false;
    }

    /// Move focus between Yes and No.
    pub fn toggle(&mut self) {
        self.confirm_selected = !self.confirm_selected;
    }
}

impl Default for ConfirmationModal {
    fn default() -> Self {
        Self::new()
    }
}

/// The footer key hints shared by interactive Yes/No confirmations. Callers who
/// want single-key hints instead pass their own string via [`ConfirmationView`].
const CONFIRMATION_HINTS: &str = "Enter/y: yes   Esc/n: no   ←→/Tab: choose";

/// Display content and emphasis for a shared confirmation modal.
///
/// Build one with [`ConfirmationView::confirmation`], which fills the standard
/// Yes/No defaults (danger confirm, warning cancel, `[ yes ] [ no ]` buttons,
/// the [`CONFIRMATION_HINTS`] footer). Override any public field to reshape the
/// prompt, or call [`ConfirmationView::compact`] for a button-less single-key
/// variant. Every confirmation surface flows through this one component so the
/// frame, buttons, and footer stay identical across Quit, unregister, and
/// cleanup.
pub struct ConfirmationView<'a> {
    pub title: &'a str,
    pub inner_width: usize,
    pub heading: String,
    pub message: &'a str,
    /// Role (colour) of the affirmative button when it is focused.
    pub confirm_role: Role,
    /// Role (colour) of the negative button when it is focused.
    pub cancel_role: Role,
    /// Label inside the affirmative button (default `yes`).
    pub confirm_label: &'a str,
    /// Label inside the negative button (default `no`).
    pub cancel_label: &'a str,
    /// Footer key-hint string, indented and dimmed by the shared [`footer`].
    pub hints: &'a str,
    /// Whether to draw the interactive `[ yes ] [ no ]` button row. A compact
    /// prompt (no focus toggle) sets this false and relies on `hints` alone.
    pub buttons: bool,
}

impl<'a> ConfirmationView<'a> {
    /// A standard Yes/No confirmation: danger confirm, warning cancel,
    /// `[ yes ] [ no ]` buttons, and the shared Enter/y … Esc/n … ←→/Tab hints.
    /// Reshape it by overriding public fields (e.g. `confirm_role`, labels).
    #[must_use]
    pub fn confirmation(
        title: &'a str,
        inner_width: usize,
        heading: String,
        message: &'a str,
    ) -> Self {
        Self {
            title,
            inner_width,
            heading,
            message,
            confirm_role: Role::Danger,
            cancel_role: Role::Warning,
            confirm_label: "yes",
            cancel_label: "no",
            hints: CONFIRMATION_HINTS,
            buttons: true,
        }
    }

    /// Turn this into a compact prompt: `hints` as single-key guidance and no
    /// interactive `[ yes ] [ no ]` row. For confirmations whose input has no
    /// Yes/No focus toggle (for example the open-screen y/n cleanup).
    #[must_use]
    pub fn compact(mut self, hints: &'a str) -> Self {
        self.buttons = false;
        self.hints = hints;
        self
    }
}

/// Two fixed-width Yes/No buttons shared by confirmation modals. Labels are
/// padded to a common width so focus never changes the bracket geometry;
/// selection uses role-coloured text and bold weight.
#[must_use]
pub fn confirmation_buttons(
    confirm_selected: bool,
    confirm_role: Role,
    cancel_role: Role,
    confirm_label: &str,
    cancel_label: &str,
) -> String {
    // Pad both labels to a common width so the two buttons stay the same size
    // regardless of which word is longer (`yes`/`no` → `[ yes ]` / `[ no  ]`).
    let label_width = confirm_label
        .chars()
        .count()
        .max(cancel_label.chars().count());
    let confirm_text = format!("[ {confirm_label:<label_width$} ]");
    let cancel_text = format!("[ {cancel_label:<label_width$} ]");
    let selected = |role: Role| role.style().bold();
    // `dim` alone inherits the terminal's current foreground colour. Give idle
    // labels an explicit white base so focus changes cannot leave a stale
    // success/danger colour behind.
    let idle = Style::new().fg(Color::White).dim();
    let (yes, no) = if confirm_selected {
        (
            selected(confirm_role).paint(&confirm_text),
            idle.paint(&cancel_text),
        )
    } else {
        (
            idle.paint(&confirm_text),
            selected(cancel_role).paint(&cancel_text),
        )
    };
    format!("  {yes}  {no}")
}

/// Render a confirmation over an existing frame through the shared component.
///
/// The button row is drawn only when `view.buttons` is set; a compact prompt
/// keeps the heading, message, and footer hints alone. `state` selects the
/// focused button and is unused by a compact prompt.
#[must_use]
pub fn render_confirmation_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: ConfirmationModal,
    view: ConfirmationView<'_>,
) -> Vec<String> {
    let mut body = vec![
        view.heading,
        Style::new().fg(Color::White).paint(view.message),
        String::new(),
    ];
    if view.buttons {
        body.push(confirmation_buttons(
            state.is_confirm_selected(),
            view.confirm_role,
            view.cancel_role,
            view.confirm_label,
            view.cancel_label,
        ));
    }
    body.push(footer(view.hints));
    render_over(
        raw_height,
        raw_width,
        base,
        view.title,
        view.inner_width,
        &body,
    )
}

/// `body` を中央寄せの [`boxed`] modal に収めたフレームを返す。枠は水平・垂直とも中央に置き、
/// 残りは空行で埋めるので、イベントループはフルスクリーン画面と同じ手順で描き直せる。
/// サイズ 0 は [`normalize_size`] で 80×24 にフォールバックする。
#[must_use]
pub fn render_modal(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    // 左右の枠線と余白だけで 4 桁必要。枠が収まらない幅では
    // 端末外へはみ出すより、空のフレームを返す。
    if width < 4 {
        return vec![String::new(); height];
    }
    // 枠は `inner_width + 4` 桁必要。狭い端末で溢れないよう内側幅を詰める（boxed が各行と
    // タイトルを収まるよう切る）。
    let inner_width = modal_inner_width(width, inner_width);
    let box_lines = boxed(title, inner_width, body);
    let pad = " ".repeat(centered_padding(width, inner_width + 4));

    let mut lines = Vec::with_capacity(height);
    let top_padding = height.saturating_sub(box_lines.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    for line in &box_lines {
        lines.push(format!("{pad}{line}"));
    }
    while lines.len() < height {
        lines.push(String::new());
    }
    // 端末高に収める: 枠が非常に低い端末より高いと top_padding が 0 になり枠だけで height を
    // 超えるので、溢れを切る（painter が最終行より下を描いて崩すのを防ぐ）。
    lines.truncate(height);
    lines
}

/// ANSI escape の終端かどうか。CSI 導入子 `[` は final byte ではない。
fn is_escape_final(ch: char) -> bool {
    ('\u{40}'..='\u{7e}').contains(&ch) && ch != '['
}

/// `text` から表示列 `start..start + width` を取り出し、ちょうど `width` 桁に
/// そろえる。ANSI escape は 0 桁として保存する。境界が全角文字の 2 桁の中間に
/// 入った場合は、片側だけを描けないため重なる列を空白にする。
fn columns(text: &str, start: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let end = start.saturating_add(width);
    let mut out = String::new();
    let mut escapes_before = String::new();
    let mut column = 0usize;
    let mut selected = false;
    let mut carries_style = false;
    let mut chars = text.chars();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            let mut sequence = String::from(ch);
            for next in chars.by_ref() {
                sequence.push(next);
                if is_escape_final(next) {
                    break;
                }
            }
            if selected && column < end {
                out.push_str(&sequence);
                carries_style = true;
            } else if !selected && column <= start {
                // suffix は行の途中から始まる。そこまでの SGR を再生すれば、
                // modal の手前で reset しても suffix の元の色を復元できる。
                escapes_before.push_str(&sequence);
            }
            continue;
        }

        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width == 0 {
            if selected && column <= end {
                out.push(ch);
            }
            continue;
        }

        let char_end = column.saturating_add(char_width);
        if char_end <= start {
            column = char_end;
            continue;
        }
        if column >= end {
            break;
        }
        if !selected {
            out.push_str(&escapes_before);
            carries_style = !escapes_before.is_empty();
            selected = true;
        }

        if column < start || char_end > end {
            // 2 桁文字を半分だけ残すことはできない。その文字のうち
            // 要求範囲と重なるセル数だけ空白にし、後続の列位置を保つ。
            let overlap_start = column.max(start);
            let overlap_end = char_end.min(end);
            out.push_str(&" ".repeat(overlap_end.saturating_sub(overlap_start)));
        } else {
            out.push(ch);
        }
        column = char_end;
    }

    let padding = width.saturating_sub(display_width(&out));
    out.push_str(&" ".repeat(padding));
    if carries_style {
        out.push_str(RESET);
    }
    out
}

/// `base` の背景を残したまま、`body` を枠付き modal として中央に合成する。
///
/// 返すフレームは常に正規化後の端末高と同じ行数で、各行は端末幅ちょうどに
/// そろえる。ANSI escape は 0 桁、全角文字は 2 桁として扱う。背景が短い行や
/// 行数不足の場合は空白で埋める。幅 4 桁未満では枠自体が収まらないため、
/// modal は描かず正規化した背景だけを返す。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    title: &str,
    inner_width: usize,
    body: &[String],
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    let mut frame: Vec<String> = (0..height)
        .map(|row| columns(base.get(row).map_or("", String::as_str), 0, width))
        .collect();

    // 左右の枠線と余白だけで 4 桁必要。それ未満では背景を守る。
    if width < 4 {
        return frame;
    }

    let inner_width = modal_inner_width(width, inner_width);
    let box_lines = boxed(title, inner_width, body);
    let box_width = inner_width + 4;
    let left = centered_padding(width, box_width);
    let top = height.saturating_sub(box_lines.len()) / 2;

    for (offset, box_line) in box_lines.iter().enumerate() {
        let row = top + offset;
        if row >= height {
            break;
        }
        let background = &frame[row];
        let prefix = columns(background, 0, left);
        let suffix_start = left + box_width;
        let suffix = columns(background, suffix_start, width.saturating_sub(suffix_start));
        // A modal line may contain coloured title, copy, or button text. Close
        // every style before restoring the background suffix so no SGR state
        // leaks into the rest of the row or subsequent redraws.
        frame[row] = format!("{prefix}{box_line}{RESET}{suffix}");
    }

    frame
}

#[cfg(test)]
mod tests {
    use super::{
        ConfirmationModal, ConfirmationView, boxed, caption, columns, confirmation_buttons,
        content_line, empty_notice, fixed_body, footer, heading, list_window, modal_inner_width,
        prompt_line, render_body, render_body_over, render_confirmation_over, render_modal,
        render_over, scroll_above, scroll_below, scroll_window, selection_marker, subcommand_row,
        viewport_window,
    };
    use crate::presentation::theme::{Role, Style};
    use crate::presentation::widgets::clip_to_width;

    #[test]
    fn confirmation_buttons_mark_the_selected_choice() {
        let ok_selected = confirmation_buttons(true, Role::Success, Role::Warning, "yes", "no");
        let cancel_selected = confirmation_buttons(false, Role::Danger, Role::Warning, "yes", "no");
        assert_eq!(display_width(&ok_selected), display_width(&cancel_selected));
        assert_eq!(display_width(&ok_selected), 18);
        assert!(ok_selected.contains("[ yes ]"));
        assert!(cancel_selected.contains("[ no  ]"));
        assert!(ok_selected.contains("\u{1b}[1;32m"));
        assert!(cancel_selected.contains("\u{1b}[1;33m"));
        assert!(ok_selected.contains("\u{1b}[2;37m"));
        assert!(cancel_selected.contains("\u{1b}[2;37m"));
    }

    #[test]
    fn confirmation_buttons_pad_custom_labels_to_a_common_width() {
        // Labels are caller-supplied; the shorter one is padded so both buttons
        // keep the same bracket geometry no matter which word is longer.
        let buttons = confirmation_buttons(true, Role::Danger, Role::Success, "remove", "keep");
        assert!(buttons.contains("[ remove ]"));
        assert!(buttons.contains("[ keep   ]"));
        // The affirmative carries the danger SGR; the idle negative stays white.
        assert!(buttons.contains("\u{1b}[1;31m[ remove ]"));
        assert!(buttons.contains("\u{1b}[2;37m[ keep   ]"));
    }

    #[test]
    fn confirmation_modal_defaults_to_yes_and_can_select_no() {
        let mut modal = ConfirmationModal::new();
        assert!(modal.is_confirm_selected());
        modal.toggle();
        assert!(!modal.is_confirm_selected());
        modal.select_confirm();
        assert!(modal.is_confirm_selected());
        modal.select_cancel();
        assert!(!modal.is_confirm_selected());

        assert!(ConfirmationModal::default().is_confirm_selected());

        // A caller that owns the selection elsewhere can project it in directly.
        assert!(ConfirmationModal::from_confirm_selected(true).is_confirm_selected());
        assert!(!ConfirmationModal::from_confirm_selected(false).is_confirm_selected());
    }

    #[test]
    fn confirmation_renderer_uses_yes_no_copy_and_shortcuts() {
        let frame = render_confirmation_over(
            12,
            60,
            &vec![String::new(); 12],
            ConfirmationModal::new(),
            ConfirmationView::confirmation("Confirm", 52, "Proceed?".to_owned(), "This is a test."),
        )
        .join("\n");
        assert!(frame.contains("[ yes ]"));
        assert!(frame.contains("[ no  ]"));
        // The footer flows through the shared `footer` helper (#372), so its
        // dim, body-indented rendering appears verbatim in the frame.
        assert!(frame.contains(&footer(super::CONFIRMATION_HINTS)));
        assert!(frame.contains("Enter/y: yes"));
        assert!(frame.contains("Esc/n: no"));
        assert!(frame.contains("←→/Tab: choose"));
    }

    #[test]
    fn compact_confirmation_drops_the_buttons_and_uses_single_key_hints() {
        // A compact prompt (no Yes/No focus toggle) keeps the heading, message,
        // and single-key footer hints, but never draws the button row.
        let frame = render_confirmation_over(
            12,
            60,
            &vec![String::new(); 12],
            ConfirmationModal::new(),
            ConfirmationView::confirmation(
                "Clean up registry",
                52,
                "Remove missing registry entries?".to_owned(),
                "Registry entries whose folder is gone are removed.",
            )
            .compact("y: remove   n/Esc: cancel"),
        )
        .join("\n");
        assert!(frame.contains("Remove missing registry entries?"));
        assert!(frame.contains("y: remove   n/Esc: cancel"));
        assert!(!frame.contains("[ yes ]"));
        assert!(!frame.contains("[ no  ]"));
        assert!(!frame.contains("←→/Tab: choose"));
    }
    use crate::presentation::widgets::display_width;

    #[test]
    fn boxed_without_title_has_plain_top_border() {
        let out = boxed("", 4, &["ab".to_string()]);
        assert_eq!(out[0], "┌──────┐"); // span = inner+2 = 6
        assert_eq!(out[2], "└──────┘");
        // 本文行は inner_width=4 に詰められ、左右に余白と枠線。
        assert_eq!(out[1], "│ ab   │");
    }

    #[test]
    fn boxed_embeds_the_title_in_the_top_border() {
        let out = boxed("題", 8, &[]);
        assert!(out[0].starts_with("┌─ 題 "));
        assert!(out[0].ends_with('┐'));
        // 上辺全体の表示幅は span + 両角 = (8+2) + 2 = 12。
        assert_eq!(display_width(&out[0]), 12);
    }

    #[test]
    fn boxed_clips_a_line_wider_than_the_box() {
        let out = boxed("", 4, &["abcdefgh".to_string()]);
        // 内容部は inner_width=4 に切られ `…` が付く。
        assert!(out[1].contains('…'));
        assert_eq!(display_width(&out[1]), 8); // │ + 空白 + 4 + 空白 + │
    }

    #[test]
    fn modal_inner_width_clamps_to_the_screen() {
        assert_eq!(modal_inner_width(80, 40), 40); // 収まる
        assert_eq!(modal_inner_width(10, 40), 6); // 10 - 4
        assert_eq!(modal_inner_width(2, 40), 0); // 飽和
    }

    #[test]
    fn fixed_body_reserves_rows_and_clips_overflow() {
        assert_eq!(fixed_body(vec!["one".into()], 3), vec!["one", "", ""]);
        assert_eq!(fixed_body(vec!["one".into(), "two".into()], 1), vec!["one"]);
    }

    #[test]
    fn body_composition_helpers_pin_the_pre_refactor_idioms() {
        // Each helper reproduces byte-for-byte the inline expression the views
        // previously repeated, so migrating a view cannot move a glyph.
        assert_eq!(content_line("row", 20), clip_to_width("  row", 20));
        assert_eq!(
            caption("Pull requests"),
            Style::new().dim().paint("  Pull requests")
        );
        assert_eq!(empty_notice("(none)"), Style::new().dim().paint("  (none)"));
        assert_eq!(
            footer("↑↓ select   Esc: close"),
            Style::new().dim().paint("  ↑↓ select   Esc: close")
        );
        assert_eq!(
            heading("Decision"),
            Role::Accent.style().bold().paint("  Decision")
        );
        assert_eq!(scroll_above(3), Style::new().dim().paint("  ↑ 3 more"));
        assert_eq!(scroll_below(1), Style::new().dim().paint("  ↓ 1 more"));
    }

    #[test]
    fn list_window_follows_the_selection_and_clamps_to_the_list() {
        // Fewer rows than the capacity: the whole list is the window.
        assert_eq!(list_window(3, 2, 6), (0, 3));
        // Selection inside the first page keeps the window at the top.
        assert_eq!(list_window(10, 0, 6), (0, 6));
        assert_eq!(list_window(10, 5, 6), (0, 6));
        // Past the bottom edge the window scrolls to keep the selection visible.
        assert_eq!(list_window(10, 6, 6), (1, 7));
        assert_eq!(list_window(10, 9, 6), (4, 10));
        // An empty list yields an empty window rather than panicking.
        assert_eq!(list_window(0, 0, 6), (0, 0));
    }

    #[test]
    fn viewport_window_anchors_on_the_scroll_offset() {
        // Offset zero shows the first `capacity` rows.
        assert_eq!(viewport_window(20, 0, 5), (0, 5));
        // A mid-document offset is the window start.
        assert_eq!(viewport_window(20, 12, 5), (12, 17));
        // The start clamps so the last row always stays on screen.
        assert_eq!(viewport_window(20, 100, 5), (19, 20));
        // A short document shows only what it has.
        assert_eq!(viewport_window(3, 0, 5), (0, 3));
        // An empty document yields an empty window.
        assert_eq!(viewport_window(0, 0, 5), (0, 0));
    }

    #[test]
    fn scroll_window_brackets_the_slice_with_more_indicators() {
        let rows: Vec<String> = (0..6).map(|n| format!("row {n}")).collect();
        // A window flush with both ends emits no indicators.
        assert_eq!(scroll_window(&rows, 0, 6), rows);
        // Hidden rows above and below each collapse to one indicator line.
        let windowed = scroll_window(&rows, 2, 4);
        assert_eq!(windowed.first(), Some(&scroll_above(2)));
        assert_eq!(windowed.last(), Some(&scroll_below(2)));
        assert!(windowed.contains(&"row 2".to_string()));
        assert!(windowed.contains(&"row 3".to_string()));
        assert!(!windowed.contains(&"row 1".to_string()));
        // An out-of-range window degrades to just its indicators.
        assert_eq!(scroll_window(&rows, 8, 8), vec![scroll_above(8)]);
    }

    #[test]
    fn prompt_line_draws_a_danger_prompt_and_accent_caret() {
        let line = prompt_line("cmd", 3);
        assert!(line.starts_with(&Role::Danger.style().bold().paint("❯")));
        assert!(line.contains("cmd"));
        // The caret past the value is the shared block caret's inverse space.
        assert!(line.contains("\u{1b}[7"));
    }

    #[test]
    fn subcommand_row_marks_the_selected_picker_row() {
        assert_eq!(
            subcommand_row("open", true),
            Style::new().dim().paint("      › open")
        );
        assert_eq!(
            subcommand_row("new", false),
            Style::new().dim().paint("        new")
        );
    }

    #[test]
    fn content_line_clips_a_wide_row_to_the_inner_width() {
        let clipped = content_line("0123456789", 6);
        assert!(display_width(&clipped) <= 6);
        assert!(clipped.starts_with("  "));
        assert!(clipped.contains('…'));
    }

    #[test]
    fn selection_marker_is_a_danger_cursor_when_selected_and_blank_otherwise() {
        assert_eq!(
            selection_marker(true),
            Role::Danger.style().bold().paint("›")
        );
        assert_eq!(selection_marker(false), " ");
        // The form widgets reuse this exact cursor for their focused row.
        assert!(
            crate::presentation::widgets::select::render("Theme", "dark", true, false)
                .starts_with(&selection_marker(true))
        );
    }

    #[test]
    fn render_body_and_over_fold_the_fixed_body_reserve() {
        // Centred: reserves the full body height, then render_modal clips to the
        // terminal, matching the old render_modal(…, &fixed_body(lines, h)).
        let lines = vec!["one".to_string(), "two".to_string()];
        let centred = render_body(24, 80, "T", 20, 6, lines.clone());
        assert_eq!(
            centred,
            render_modal(24, 80, "T", 20, &fixed_body(lines.clone(), 6))
        );

        // Over a background: the reserve clamps to height - 4 on a short
        // terminal so a sliver of the base survives above and below the box.
        let base = vec!["background".to_string(); 8];
        let over = render_body_over(8, 80, &base, "T", 20, 6, lines.clone());
        let reserved = 6usize.min(8usize.saturating_sub(4));
        assert_eq!(
            over,
            render_over(8, 80, &base, "T", 20, &fixed_body(lines, reserved))
        );
        assert_eq!(over.len(), 8);
        assert!(over[0].starts_with("background"));
    }

    #[test]
    #[coverage(off)]
    fn render_modal_centers_the_box_over_a_blank_frame() {
        let lines = render_modal(10, 40, "T", 10, &["hi".to_string()]);
        assert_eq!(lines.len(), 10);
        // 枠は 3 行（上辺・本文・下辺）。上下に空行が入る。
        let non_blank = lines.iter().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(non_blank, 3);
        assert!(lines.iter().any(|l| l.contains("hi")));
        assert!(lines.iter().any(|l| l.contains('T')));
    }

    #[test]
    fn render_modal_falls_back_and_truncates_to_height() {
        // 高さ 0 → 24 にフォールバック。
        assert_eq!(render_modal(0, 0, "", 10, &[]).len(), 24);
        // 枠より低い端末では高さに切り詰める（溢れさせない）。
        let body: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let lines = render_modal(3, 40, "", 10, &body);
        assert_eq!(lines.len(), 3);

        // 枠の最小 4 桁が収まらない端末でも、行を幅外へ出さない。
        for width in 1..4 {
            let lines = render_modal(2, width, "T", 10, &["body".to_string()]);
            assert_eq!(lines.len(), 2);
            assert!(lines.iter().all(|line| display_width(line) <= width));
        }
    }

    #[test]
    fn render_over_preserves_the_background_outside_the_centered_box() {
        let base: Vec<String> = (0..9)
            .map(|row| format!("row-{row}-{}", ".".repeat(33)))
            .collect();
        let lines = render_over(9, 40, &base, "T", 8, &["body".to_string()]);

        assert_eq!(lines.len(), 9);
        assert!(lines.iter().all(|line| display_width(line) == 40));
        // 3 行の box は row 3..=5 に置かれ、その外は背景のまま。
        assert!(lines[0].starts_with("row-0-"));
        assert!(lines[8].starts_with("row-8-"));
        // box の左右にも背景が残る。
        assert!(lines[3].starts_with("row-3-"));
        assert!(lines[3].contains("┌─ T "));
        assert!(lines[3].trim_end().ends_with("...."));
        assert!(lines[4].contains("body"));
    }

    #[test]
    fn render_over_keeps_ansi_and_full_width_cells_aligned() {
        // 色付き全角文字の中間に box 境界が入る（left=5）ケース。
        let background = format!("\u{1b}[31m{}\u{1b}[0m", "界".repeat(10));
        let base = vec![background; 5];
        let lines = render_over(5, 20, &base, "題", 6, &["中身".to_string()]);

        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|line| display_width(line) == 20));
        let top = &lines[1];
        assert!(top.contains("\u{1b}[31m"));
        // prefix の色は modal 枠前で閉じ、suffix で再現される。
        assert!(top.contains("\u{1b}[0m┌"));
        assert!(top.matches("\u{1b}[31m").count() >= 2);
        assert!(top.contains("─ 題 "));
    }

    #[test]
    fn render_over_handles_tiny_terminals_without_overflow() {
        for width in 1..=4 {
            let base = vec!["abcdef".to_string(); 2];
            let lines = render_over(2, width, &base, "title", 20, &["body".to_string()]);
            assert_eq!(lines.len(), 2);
            assert!(lines.iter().all(|line| display_width(line) == width));
            if width < 4 {
                assert!(!lines.iter().any(|line| line.contains('┌')));
            } else {
                assert!(lines.iter().any(|line| line.contains('┌')));
            }
        }
    }

    #[test]
    fn render_over_normalizes_missing_rows_and_zero_size() {
        let lines = render_over(0, 0, &["background".to_string()], "T", 10, &[]);
        assert_eq!(lines.len(), 24);
        assert!(lines.iter().all(|line| display_width(line) == 80));
        assert!(lines[0].starts_with("background"));
    }

    #[test]
    fn column_slice_keeps_zero_width_combining_characters() {
        let sliced = columns("a\u{301}b", 0, 2);
        assert_eq!(display_width(&sliced), 2);
        assert!(sliced.contains('\u{301}'));
    }
}
