//! うさぎ（マスコット）を頂く全画面レイアウト。
//!
//! welcome / config など、マスコット＋タイトルを上に置き、その下のボディを垂直中央寄せし、
//! フッタを最下行に固定する全画面 view が共有する chrome。各 view はこの scaffold に
//! 「タイトル」「ボディ（画面固有の内容）」「フッタのヒント」を渡すだけでよく、マスコットの
//! 描画・中央寄せ・フッタ固定はここに集約する。これにより、どの画面でもマスコットとタイトルが
//! 同じ体裁・同じ配置で出る（画面をまたいでもうさぎが揃う）。
//!
//! ボディは端末幅に依存して組む（2 カラムの中央寄せなど）ため、`render` は正規化済みの幅を
//! 受け取るクロージャ `build_body` にボディの構築を委ね、幅の正規化を 1 か所に保つ。色は
//! [`crate::presentation::theme`] の役割で塗る。

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, icon};

/// `text` を幅 `width` に中央寄せし `style` で塗った 1 行。端末より広いテキストは
/// [`widgets::clip_to_width`] で省略記号付きに切ってから寄せる。タイトル・フッタのほか、
/// view が通知行などを中央寄せするのにも使う共通プリミティブ。
#[must_use]
pub fn centered_line(width: usize, text: &str, style: Style) -> String {
    let clipped = widgets::clip_to_width(text, width);
    let pad = widgets::centered_padding(width, widgets::display_width(&clipped));
    format!("{}{}", " ".repeat(pad), style.paint(&clipped))
}

/// マスコット＋タイトルのヘッダ行。垂直配置は [`render`] が行うので先頭余白は付けない。
/// マスコットの下に 1 行空け、タイトルを Success 太字で中央寄せする。
fn header_lines(width: usize, title: &str) -> Vec<String> {
    let mut lines = icon::centered(width);
    lines.push(String::new());
    lines.push(centered_line(width, title, Role::Success.style().bold()));
    lines
}

/// `content_lines` 行のコンテンツを、フッタ 1 行の上で `height` 行に垂直中央寄せするときの
/// 上の空行数。
fn centered_top_padding(height: usize, content_lines: usize) -> usize {
    height.saturating_sub(content_lines + 1) / 2
}

/// マスコット＋タイトルを頂く全画面フレームを組む。
///
/// ヘッダ（マスコット＋タイトル）とその下の `build_body` が返すボディを 1 塊として垂直中央寄せし、
/// `footer` を dim のヒント行として最下行に固定する。ヘッダとボディの間には 1 行の余白を挟む。
/// サイズ 0 は 80×24 にフォールバックする。`build_body` には正規化済みの端末幅を渡すので、view は
/// その幅でボディ（幅依存の中央寄せなど）を組める。
#[must_use]
pub fn render(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    footer: &str,
    build_body: impl FnOnce(usize) -> Vec<String>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut content = header_lines(width, title);
    // ヘッダとボディの間の余白。
    content.push(String::new());
    content.extend(build_body(width));
    let footer_line = centered_line(width, footer, Style::new().dim());

    let mut lines = Vec::with_capacity(height);
    let top_padding = centered_top_padding(height, content.len());
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(content);

    // フッタを最下行まで押し下げる。
    let bottom_padding = height.saturating_sub(lines.len() + 1);
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.push(footer_line);
    lines
}

#[cfg(test)]
mod tests {
    use super::{centered_line, render};
    use crate::presentation::theme::Role;
    use crate::presentation::widgets::display_width;

    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn centered_line_pads_and_styles() {
        let line = centered_line(20, "hi", Role::Warning.style());
        // 左に中央寄せの余白（(20-2)/2 = 9 桁）＋テキスト 2 桁。SGR は 0 桁。
        assert_eq!(display_width(&line), 9 + 2);
        assert!(line.starts_with(' '));
        assert!(strip(&line).contains("hi"));
        // 端末幅を超えない。
        assert!(display_width(&line) <= 20);
    }

    #[test]
    fn render_places_mascot_title_body_and_pins_footer() {
        let frame = render(40, 80, "TITLE", "hint: quit", |width| {
            vec![centered_line(width, "BODY", Role::Accent.style())]
        });
        assert_eq!(frame.len(), 40);
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        // マスコット（うさぎ AA の一部）・タイトル・ボディが出る。
        assert!(joined.contains("(\\(\\"));
        assert!(joined.contains("TITLE"));
        assert!(joined.contains("BODY"));
        // フッタは最下行。
        assert!(strip(frame.last().unwrap()).contains("hint: quit"));
        // 先頭の空行が中央寄せする。
        let top = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top > 0);
        assert!(!frame[top].is_empty());
    }

    #[test]
    fn render_falls_back_to_default_size_and_receives_normalized_width() {
        // サイズ 0 は 80×24 にフォールバックし、build_body には正規化幅 80 が渡る。
        let frame = render(0, 0, "T", "f", |width| {
            assert_eq!(width, 80);
            vec![String::new()]
        });
        assert_eq!(frame.len(), 24);
    }

    #[test]
    fn render_does_not_lose_content_on_a_short_terminal() {
        // ボディがフレームより高い端末: 中央寄せ余白は 0 に飽和し、フッタは最下行に残る。
        let frame = render(3, 80, "T", "footer", |width| {
            (0..20)
                .map(|i| centered_line(width, &i.to_string(), Role::Info.style()))
                .collect()
        });
        assert!(strip(frame.last().unwrap()).contains("footer"));
        assert!(frame.iter().any(|l| strip(l).contains('T')));
    }
}
