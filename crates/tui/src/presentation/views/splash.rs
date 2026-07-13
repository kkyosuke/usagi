//! Welcome の前に一度だけ出す起動スプラッシュ。
//!
//! うさぎを先に出し、`USAGI` を暗い緑から通常の Success 太字へフェードインする。
//! title の行は最初から確保するため、フェード中にレイアウトは動かない。

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Role, Style, TITLE_FADE};
use crate::presentation::widgets::{self, icon};

/// 1 フレームの表示間隔は 110ms。
pub const ANIM_TICK: std::time::Duration = std::time::Duration::from_millis(110);

/// タイトルを隠したまま、うさぎだけを表示するフレーム数。
const TITLE_DELAY: usize = 5;
/// フェード完了後、Welcome へ渡す前に表示を保つフレーム数。
const TITLE_HOLD: usize = 4;
/// 起動スプラッシュ全体のフレーム数。
pub const FRAMES: usize = TITLE_DELAY + TITLE_FADE.len() + 1 + TITLE_HOLD;

const TITLE: &str = "USAGI";

/// `frame` に対応するタイトルのフェード段階。0 は title を描かない。
#[must_use]
fn title_fade_step(frame: usize) -> usize {
    if frame < TITLE_DELAY {
        0
    } else {
        (frame - TITLE_DELAY + 1).min(TITLE_FADE.len() + 1)
    }
}

fn faded_title_line(width: usize, step: usize) -> String {
    if step == 0 {
        return String::new();
    }
    if step > TITLE_FADE.len() {
        return mascot_screen::centered_line(width, TITLE, Role::Success.style().bold());
    }
    mascot_screen::centered_line(
        width,
        TITLE,
        Style::new().fg(crate::presentation::theme::Color::Ansi256(
            TITLE_FADE[step - 1],
        )),
    )
}

/// スプラッシュの 1 フレームを組み立てる。マスコットは固定し、title だけを変える。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, frame: usize) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let mut header: Vec<String> = icon::centered(width)
        .iter()
        .map(|line| Role::Feature.style().paint(line))
        .collect();
    header.push(String::new());
    header.push(faded_title_line(width, title_fade_step(frame)));

    // Welcome と同じ mascot-screen の通常位置（端末高の約 1/5）に置く。
    let mut lines = vec![String::new(); height / 5];
    lines.append(&mut header);
    lines.resize(height, String::new());
    lines
}

#[cfg(test)]
mod tests {
    use super::{FRAMES, TITLE, TITLE_DELAY, render, title_fade_step};

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
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn title_waits_then_fades_and_holds_at_full_brightness() {
        assert_eq!(title_fade_step(0), 0);
        assert_eq!(title_fade_step(TITLE_DELAY - 1), 0);
        assert_eq!(title_fade_step(TITLE_DELAY), 1);
        assert_eq!(title_fade_step(FRAMES - 1), 5);
    }

    #[test]
    fn mascot_is_visible_before_the_title_and_stays_fixed() {
        let before = render(24, 80, 0);
        let after = render(24, 80, FRAMES - 1);
        let before_text = before.iter().map(|line| strip(line)).collect::<Vec<_>>();
        let after_text = after.iter().map(|line| strip(line)).collect::<Vec<_>>();
        assert!(before_text.iter().any(|line| line.contains("(='-')")));
        assert!(!before_text.iter().any(|line| line.contains(TITLE)));
        assert!(after_text.iter().any(|line| line.contains(TITLE)));
        assert_eq!(before_text[4..7], after_text[4..7]);
    }

    #[test]
    fn final_title_matches_welcome_success_style() {
        let frame = render(24, 80, FRAMES - 1);
        let title = frame.iter().find(|line| line.contains(TITLE)).unwrap();
        assert!(title.contains("1;32m"));
    }
}
