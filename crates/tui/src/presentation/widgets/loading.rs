//! ローディング表示の部品。
//!
//! 進捗の割合が分かるときの進捗バー、割合が無いときのスピナー、そして待機中に画面が
//! 固まって見えないよう跳ねる「ローディングうさぎ」を持つ。すべて `frame`（単調増加する
//! ティック）から絵柄を選ぶ純粋関数で、連続するフレームを描けばアニメーションする。
//! 色付け（マゼンタ太字）はテーマ導入時に載せるため、ここでは無色の行を返す。

use super::{centered_padding, display_width, normalize_size};

/// ローディングうさぎの傍らで 1 ティックごとに回すブライユ点字スピナーの各コマ。
const LOADING_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// `frame` に対応するスピナーのグリフ。[`LOADING_SPINNER`] を循環する。自前の行を
/// 動かす呼び出し側（背景タスク欄など）がローディングうさぎと歩調を合わせられる。
#[must_use]
pub fn spinner_char(frame: usize) -> &'static str {
    LOADING_SPINNER[frame % LOADING_SPINNER.len()]
}

/// 括弧内が `width` 桁の固定幅進捗バー `[===>   ]`。`done / total` の割合を最も近い桁に
/// 丸めて満たす。空（`done == 0`）は全て空白、完了（`done >= total`）は全て `=`、途中は
/// 先頭に `>` を置く。尺度が無い（`total == 0`）か `width == 0` のときは空文字を返す。
#[must_use]
pub fn progress_bar(done: usize, total: usize, width: usize) -> String {
    if total == 0 || width == 0 {
        return String::new();
    }
    let done = done.min(total);
    // 満たす割合をバーの桁に丸める。病的な数でも割る前に溢れないよう u128 で計算する。
    // 値は `width` に収まるので usize に戻せる。
    let filled = ((done as u128 * width as u128 + total as u128 / 2) / total as u128) as usize;
    if filled == 0 {
        return format!("[{}]", " ".repeat(width));
    }
    if filled >= width {
        return format!("[{}]", "=".repeat(width));
    }
    // 途中: 先頭までの `=`、`>` の頭、残りの空白 — 三者の合計は常に `width`。
    format!(
        "[{}>{}]",
        "=".repeat(filled - 1),
        " ".repeat(width - filled)
    )
}

/// ローディングうさぎが循環する表情。中央の `ㅅ` が常に同じ桁に来るよう、両脇のグリフは
/// 幅 1。呼び出し側が `face_index` を進めると表情だけが変わる。
const LOADING_FACES: [&str; 6] = ["･ㅅ･", "-ㅅ-", "^ㅅ^", "oㅅo", ">ㅅ<", "=ㅅ="];

/// 跳ねる 2 行のローディングうさぎ。`hop_frame` が跳躍（とスピナー）を、`face_index` が
/// [`LOADING_FACES`] の表情を選ぶ。両行は共通のブロック幅に右詰めされる。進捗の無い待機を
/// 表すのに使い、呼び出し側は 2 つの指標を経過時間から導くと時計だけで跳ねて表情が変わる。
#[must_use]
pub fn hopping_rabbit(hop_frame: usize, face_index: usize, label: &str) -> Vec<String> {
    // 跳躍は耳と体を 1 桁ずつ一緒にずらす。
    let lead = " ".repeat(hop_frame % 2);
    let face = LOADING_FACES[face_index % LOADING_FACES.len()];
    let spinner = spinner_char(hop_frame);
    let rows = [
        format!("  {lead}∩∩"),
        format!("{lead}({face})づ{spinner} {label}"),
    ];
    let block_w = rows.iter().map(|r| display_width(r)).max().unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(display_width(&row));
            format!("{row}{}", " ".repeat(pad))
        })
        .collect()
}

/// 全画面のローディングフレーム: [`hopping_rabbit`] を `width`×`height` の画面中央で
/// 跳ねさせ、`label` を添える。遅い同期処理の代わりに表示し、固まった画面ではなく usagi が
/// 働いている様子を見せる。サイズ 0 は [`normalize_size`] で 80×24 にフォールバックする。
#[must_use]
pub fn loading_screen(
    width: usize,
    height: usize,
    hop_frame: usize,
    face_index: usize,
    label: &str,
) -> Vec<String> {
    let (height, width) = normalize_size(height, width);
    let block = hopping_rabbit(hop_frame, face_index, label);
    let block_w = block.iter().map(|r| display_width(r)).max().unwrap_or(0);
    let pad = " ".repeat(centered_padding(width, block_w));
    // 2 行ブロックを縦中央に置き、残りを空行で埋めて painter が描く全行を消去させる。
    let top = height.saturating_sub(block.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.resize(top, String::new());
    for row in block {
        lines.push(format!("{pad}{row}"));
    }
    lines.resize(height, String::new());
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        LOADING_FACES, LOADING_SPINNER, hopping_rabbit, loading_screen, progress_bar, spinner_char,
    };
    use crate::presentation::widgets::display_width;

    #[test]
    fn spinner_char_cycles_through_the_frames() {
        assert_eq!(spinner_char(0), LOADING_SPINNER[0]);
        assert_eq!(spinner_char(LOADING_SPINNER.len()), LOADING_SPINNER[0]);
        assert_eq!(spinner_char(LOADING_SPINNER.len() + 3), LOADING_SPINNER[3]);
    }

    #[test]
    fn progress_bar_edges_and_partial() {
        assert_eq!(progress_bar(0, 0, 10), ""); // total 0
        assert_eq!(progress_bar(1, 2, 0), ""); // width 0
        assert_eq!(progress_bar(0, 4, 4), "[    ]"); // 空
        assert_eq!(progress_bar(4, 4, 4), "[====]"); // 完了
        assert_eq!(progress_bar(5, 4, 4), "[====]"); // done を total に丸める
        // 途中: 括弧内はちょうど width 桁。
        let mid = progress_bar(1, 4, 4);
        assert_eq!(mid, "[>   ]");
        assert_eq!(mid.chars().filter(|c| *c != '[' && *c != ']').count(), 4);
        assert_eq!(progress_bar(2, 4, 4), "[=>  ]");
    }

    #[test]
    fn hopping_rabbit_pads_both_rows_to_a_common_width() {
        let rows = hopping_rabbit(0, 0, "作業中");
        assert_eq!(rows.len(), 2);
        let w0 = display_width(&rows[0]);
        let w1 = display_width(&rows[1]);
        assert_eq!(w0, w1); // 右詰めのため両行同じ幅
        assert!(rows[1].contains("作業中"));
        assert!(rows[1].contains(super::spinner_char(0)));
        // 表情が選ばれている。
        assert!(rows[1].contains(LOADING_FACES[0]));
    }

    #[test]
    fn hopping_rabbit_hop_shifts_by_one_column() {
        // 奇数フレームは 1 桁だけリード（跳躍）する。
        let even = hopping_rabbit(0, 0, "x");
        let odd = hopping_rabbit(1, 0, "x");
        assert!(display_width(&odd[0]) >= display_width(&even[0]));
    }

    #[test]
    fn loading_screen_centers_the_block_and_fills_the_height() {
        let lines = loading_screen(40, 10, 0, 0, "起動中");
        assert_eq!(lines.len(), 10); // 全行を高さまで埋める
        assert!(lines.iter().any(|l| l.contains("起動中")));
    }

    #[test]
    fn loading_screen_falls_back_for_zero_size() {
        let lines = loading_screen(0, 0, 0, 0, "x");
        assert_eq!(lines.len(), 24); // 0 → 80x24
    }
}
