//! Config 画面（設定）。
//!
//! welcome から開く設定画面。マスコット＋タイトル＋フッタの配置は共通の
//! [`mascot_screen`] レイアウトに任せ、この view はボディだけを組む。設定項目はまだ無く、
//! ボディは「設定項目が無い」ことを示す 1 行のプレースホルダだけを中央寄せで出す。設定が
//! 増えたらここにフィールドの状態と描画を足す。
//!
//! [`render`] は端末 IO を持たない純粋関数で、フレーム（ANSI 付き行の `Vec<String>`）を返す。

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::Style;

/// 画面上部に置くタイトル。
const TITLE: &str = "Config";
/// 最下行に固定するキー操作ヒント。
const FOOTER: &str = "Esc: back";
/// 設定項目が無いことを示すボディのプレースホルダ。
const EMPTY_BODY: &str = "No settings";

/// 生の端末サイズ `raw_height`×`raw_width` に対する config 画面 1 フレーム分の行。
/// マスコット・タイトル・フッタの配置は共通の [`mascot_screen`] レイアウトに任せる。設定項目が
/// 無いので、ボディは中央寄せした 1 行のプレースホルダ（dim）だけを出す。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, FOOTER, |width| {
        vec![mascot_screen::centered_line(
            width,
            EMPTY_BODY,
            Style::new().dim(),
        )]
    })
}

#[cfg(test)]
mod tests {
    use super::render;
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
    fn render_shows_the_mascot_title_placeholder_and_footer() {
        let frame = render(24, 80);
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        // マスコット（うさぎ AA の一部）・タイトル・空設定のプレースホルダ。
        assert!(joined.contains("(\\(\\"));
        assert!(joined.contains("Config"));
        assert!(joined.contains("No settings"));
        // フッタは最下行。
        assert!(strip(frame.last().unwrap()).contains("Esc: back"));
    }

    #[test]
    fn render_fills_the_terminal_and_fits_its_width() {
        let frame = render(30, 80);
        assert_eq!(frame.len(), 30);
        assert!(frame.iter().all(|l| display_width(l) <= 80));
    }

    #[test]
    fn render_falls_back_for_a_zero_size() {
        // サイズ 0 は 80×24 にフォールバック。
        assert_eq!(render(0, 0).len(), 24);
    }
}
