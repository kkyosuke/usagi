//! 枠付きダイアログ（modal）の部品。
//!
//! 角丸の枠に本文を収める [`boxed`] と、それを画面中央に配置する [`render_modal`] を持つ。
//! 幅は表示桁数（全角 2 桁）で測り、本文が枠より広ければ [`super::clip_to_width`] で切り、
//! 短ければ空白で詰めて右端を揃える。色付け（枠色）はテーマ導入時に載せるため無色で描く。

use super::{centered_padding, clip_to_width, display_width, normalize_size};

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

#[cfg(test)]
mod tests {
    use super::{boxed, modal_inner_width, render_modal};
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
    }
}
