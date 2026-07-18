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

/// Display content and emphasis for a shared confirmation modal.
pub struct ConfirmationView<'a> {
    pub title: &'a str,
    pub inner_width: usize,
    pub heading: String,
    pub message: &'a str,
    pub confirm_role: Role,
}

/// Two fixed-width Yes/No buttons shared by confirmation modals. Selection
/// uses role-coloured text and bold weight; focus never changes the bracket
/// geometry.
#[must_use]
pub fn confirmation_buttons(confirm_selected: bool, confirm_role: Role) -> String {
    let selected = |role: Role| role.style().bold();
    // `dim` alone inherits the terminal's current foreground colour. Give idle
    // labels an explicit white base so focus changes cannot leave a stale
    // success/danger colour behind.
    let idle = Style::new().fg(Color::White).dim();
    let (yes, no) = if confirm_selected {
        (
            selected(confirm_role).paint("[ yes ]"),
            idle.paint("[ no  ]"),
        )
    } else {
        (
            idle.paint("[ yes ]"),
            selected(Role::Warning).paint("[ no  ]"),
        )
    };
    format!("  {yes}  {no}")
}

/// Render a standard Yes/No confirmation over an existing frame.
#[must_use]
pub fn render_confirmation_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: ConfirmationModal,
    view: ConfirmationView<'_>,
) -> Vec<String> {
    render_over(
        raw_height,
        raw_width,
        base,
        view.title,
        view.inner_width,
        &[
            view.heading,
            Style::new().fg(Color::White).paint(view.message),
            String::new(),
            confirmation_buttons(state.is_confirm_selected(), view.confirm_role),
            Style::new()
                .dim()
                .paint("  Enter/y: yes   Esc/n: no   ←→/Tab: choose"),
        ],
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
        ConfirmationModal, ConfirmationView, boxed, columns, confirmation_buttons, fixed_body,
        modal_inner_width, render_confirmation_over, render_modal, render_over,
    };
    use crate::presentation::theme::Role;

    #[test]
    fn confirmation_buttons_mark_the_selected_choice() {
        let ok_selected = confirmation_buttons(true, Role::Success);
        let cancel_selected = confirmation_buttons(false, Role::Danger);
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
    }

    #[test]
    fn confirmation_renderer_uses_yes_no_copy_and_shortcuts() {
        let frame = render_confirmation_over(
            12,
            60,
            &vec![String::new(); 12],
            ConfirmationModal::new(),
            ConfirmationView {
                title: "Confirm",
                inner_width: 40,
                heading: "Proceed?".to_owned(),
                message: "This is a test.",
                confirm_role: Role::Danger,
            },
        )
        .join("\n");
        assert!(frame.contains("[ yes ]"));
        assert!(frame.contains("[ no  ]"));
        assert!(frame.contains("Enter/y: yes"));
        assert!(frame.contains("Esc/n: no"));
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
