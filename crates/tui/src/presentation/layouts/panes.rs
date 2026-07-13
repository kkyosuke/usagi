//! 左右 2 ペインのレイアウト。全幅を「左ペイン＋縦区切り 1 桁＋右ペイン」に割り、各ペインの
//! 行を横に結合して 1 フレームにする。workspace 画面の session menu（左）と closeup（右）が使う。
//!
//! レイアウトは幅の割り当てと結合（矩形の算出）に徹し、各ペインの中身は view が組む。結合時は
//! [`crate::presentation::widgets::pad_to_width`] で各行を自ペインの幅にそろえるので、色付き行でも
//! 桁がずれない。

use crate::presentation::theme::{Color, Style};
use crate::presentation::widgets;

/// 縦区切り線の桁数。
const DIVIDER_WIDTH: usize = 1;
/// 下端の footer 領域として縦区切りを引かない行数。
const FOOTER_ROWS: usize = 2;

/// 全幅を割った左右ペインの表示幅。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// 左ペインの幅。
    pub left: usize,
    /// 右ペインの幅。
    pub right: usize,
}

/// 全幅 `width` を、左ペイン `desired_left`・縦区切り 1 桁・右ペインに割る。左は右に最低 1 桁
/// 残せるところまでに詰め、右は残り。`width` が狭すぎる場合は左右とも 0 に飽和する。
#[must_use]
pub fn split(width: usize, desired_left: usize) -> Panes {
    // 右に最低 1 桁残す（区切り＋右 1 桁 = 2 桁を確保）。
    let max_left = width.saturating_sub(DIVIDER_WIDTH + 1);
    let left = desired_left.min(max_left);
    let right = width.saturating_sub(left + DIVIDER_WIDTH);
    Panes { left, right }
}

/// 左ペイン `left`・右ペイン `right` の行を白い縦区切りで横に結合し、`height` 行の
/// フレームにする。下端 2 行は共通 footer 領域のため divider を引かない。各行は
/// [`widgets::pad_to_width`] で自ペインの幅にそろえ、行が尽きたペインは空白で埋める。
#[must_use]
pub fn join(height: usize, left: &[String], right: &[String], panes: Panes) -> Vec<String> {
    // The divider is structural chrome, rather than secondary metadata.  Keep it
    // visibly brighter than the dim sidebar copy so the two panes remain legible
    // through the intentionally generous blank space in the home screen.
    let divider = Style::new().fg(Color::White).paint("│");
    (0..height)
        .map(|i| {
            let l = widgets::pad_to_width(left.get(i).map_or("", String::as_str), panes.left);
            let r = widgets::pad_to_width(right.get(i).map_or("", String::as_str), panes.right);
            let separator = if i + FOOTER_ROWS < height {
                divider.as_str()
            } else {
                " "
            };
            format!("{l}{separator}{r}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Panes, join, split};
    use crate::presentation::widgets::display_width;

    #[test]
    fn split_allocates_the_left_then_the_rest_to_the_right() {
        // 全幅 80、左 30 希望 → 左 30・区切り 1・右 49。
        let panes = split(80, 30);
        assert_eq!(
            panes,
            Panes {
                left: 30,
                right: 49
            }
        );
        assert_eq!(panes.left + 1 + panes.right, 80);
        // derive された Debug / Clone / PartialEq も触れる。
        assert_eq!(panes, panes);
        assert!(format!("{panes:?}").contains("Panes"));
    }

    #[test]
    fn split_clamps_the_left_to_keep_a_column_for_the_right() {
        // 左希望が全幅を超える → 右に 1 桁残るところまで詰める。
        let panes = split(10, 100);
        assert_eq!(panes.right, 1);
        assert_eq!(panes.left + 1 + panes.right, 10);
        // 極端に狭い端末では両方 0 に飽和する。
        assert_eq!(split(1, 5), Panes { left: 0, right: 0 });
    }

    #[test]
    fn join_places_both_panes_around_the_divider() {
        let panes = split(20, 8); // 左 8・区切り 1・右 11
        let left = vec!["L".to_string()];
        let right = vec!["R".to_string(), "R2".to_string()];
        let rows = join(3, &left, &right, panes);
        assert_eq!(rows.len(), 3);
        // 各行はちょうど全幅。
        assert!(rows.iter().all(|r| display_width(r) == 20));
        assert!(rows[0].contains('│'));
        // 行が尽きた側は空白で埋まる（左は 1 行だけ）。
        assert!(rows[1].starts_with(' '));
        assert!(rows[1].contains("R2"));
        assert!(rows[0].contains("\u{1b}[37m│\u{1b}[0m"));
        assert!(!rows[1].contains('│'));
        assert!(!rows[2].contains('│'));
    }
}
