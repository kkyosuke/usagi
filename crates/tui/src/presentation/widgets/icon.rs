//! usagi マスコットの AA（アイコン）。
//!
//! 静的なマスコット絵柄と、それを画面上に配置する幾何を持つ。色付け（マゼンタ太字など）は
//! テーマ導入時に載せるため、ここでは無色の行を返す。アニメーションするローディングうさぎは
//! [`super::loading`] にある。

use super::centered_padding;

/// usagi マスコットの絵柄（生の無色行）。
const RABBIT: [&str; 3] = ["  (\\(\\ ", "  (='-') ", " o(_(\")(\")"];

/// マスコットの表示幅 — [`RABBIT`] の最も広い行の桁数。中央寄せ配置が基準にする。
#[must_use]
pub fn width() -> usize {
    RABBIT.iter().map(|l| l.chars().count()).max().unwrap_or(0)
}

/// マスコットが占める行数。呼び出し側がちょうどの行を確保（または消去）できる。
#[must_use]
pub fn height() -> usize {
    RABBIT.len()
}

/// マスコットの各行を列 `col` から始まるよう字下げして返す。ブロック全体で同じ字下げを
/// 共有するので絵柄の桁が揃う。
#[must_use]
pub fn lines_at(col: usize) -> Vec<String> {
    let padding = " ".repeat(col);
    RABBIT
        .iter()
        .map(|line| format!("{padding}{line}"))
        .collect()
}

/// 幅 `term_width` の端末に対して中央寄せしたマスコットの行。
#[must_use]
pub fn centered(term_width: usize) -> Vec<String> {
    lines_at(centered_padding(term_width, width()))
}

#[cfg(test)]
mod tests {
    use super::{RABBIT, centered, height, lines_at, width};

    #[test]
    fn dimensions_match_the_art() {
        assert_eq!(height(), 3);
        // 最も広い行の桁数。
        let widest = RABBIT.iter().map(|l| l.chars().count()).max().unwrap();
        assert_eq!(width(), widest);
    }

    #[test]
    fn lines_at_indents_every_row_by_the_same_column() {
        let lines = lines_at(4);
        assert_eq!(lines.len(), 3);
        for (line, raw) in lines.iter().zip(RABBIT.iter()) {
            assert_eq!(line, &format!("    {raw}"));
        }
    }

    #[test]
    fn lines_at_zero_is_the_bare_art() {
        assert_eq!(
            lines_at(0),
            RABBIT.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn centered_indents_by_the_centering_padding() {
        // 幅 20 の端末では左パディング (20 - width) / 2 で中央寄せ。
        let pad = (20 - width()) / 2;
        let lines = centered(20);
        assert!(lines[0].starts_with(&" ".repeat(pad)));
        // 端末が絵柄より狭いとパディング 0（飽和）。
        let narrow = centered(1);
        assert_eq!(narrow, lines_at(0));
    }
}
