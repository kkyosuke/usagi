//! 画面をまたいで再利用する UI 部品（widget）。v1 の `widgets/` を引き継ぎ、
//! `text_input`（キャレット編集付き 1 行入力）/ `icon`（うさぎ AA）/
//! `loading`（スピナー・進捗バー・ローディングうさぎ）/ `modal`（枠付きダイアログ）
//! を置く。特定の画面に固有の描画は [`super::views`] に置き、ここには複数 view から
//! 使い回す部品だけを置く。
//!
//! すべて実 IO を持たない純粋関数・値で、フレーム（ANSI 付き行の `Vec<String>`）を
//! 組み立てるか、その部品（`String`）を返す。色付け（テーマ）は未導入のため、
//! ここでは無色の構造・幾何・AA・編集ロジックだけを持つ。この直下の関数は、
//! それらが共通して使うテキスト幅の測定・切り詰め・折り返しのプリミティブである。

pub mod icon;
pub mod loading;
pub mod modal;
pub mod text_input;

pub use text_input::TextInput;

use unicode_width::UnicodeWidthChar;

/// エスケープシーケンスの先頭（ESC）。表示桁数を測るとき読み飛ばす。
const ESC: char = '\u{1b}';

/// 切り詰めがスタイルを開いたまま断ち切ったとき末尾に付ける SGR リセット
/// (`ESC [ 0 m`)。開いた色が後続の内容に滲むのを防ぐ。
const RESET: &str = "\u{1b}[0m";

/// `text` の表示桁数（端末に描かれる列数）を返す。全角（CJK など）は 2 桁、
/// ANSI エスケープシーケンス（SGR カラー）は 0 桁として数えるので、色付き行でも
/// 見た目どおりの幅になる。
#[must_use]
pub fn display_width(text: &str) -> usize {
    let mut width = 0usize;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
            // エスケープシーケンス（`ESC [ … final`）を末尾の final バイト
            // (`0x40..=0x7e`。ただし `[` 導入子を除く) まで読み飛ばす。
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                    break;
                }
            }
            continue;
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    width
}

/// `text` を表示桁数 `max` に収まるよう切り詰める。溢れるときは末尾 1 桁を使って
/// `…` を付ける（先頭側が情報量が多いので頭を残す）。ANSI エスケープは 0 桁として
/// そのまま持ち越し、切断が色を開いたままにするときは末尾を [`RESET`] で閉じる。
#[must_use]
pub fn clip_to_width(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // 省略記号 `…` に 1 桁を残す。
    let budget = max - 1;
    let mut out = String::with_capacity(text.len());
    let mut width = 0usize;
    // 切断がスタイル（SGR エスケープ）を持ち越したか。持ち越したら末尾を
    // [`RESET`] で閉じ、開いた色が後続へ滲まないようにする。
    let mut carried_escape = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
            out.push(ch);
            carried_escape = true;
            for c in chars.by_ref() {
                out.push(c);
                if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        out.push(ch);
    }
    out.push('…');
    if carried_escape {
        out.push_str(RESET);
    }
    out
}

/// `text` を表示桁数 `width` 以下の行に折り返す。空白を持たない CJK でも折れるよう、
/// 文字の境目で分割する。単体で `width` を超える 1 文字（幅 1 の行に幅 2 の全角など）は
/// その行に単独で置いて溢れさせ、文字を落とさない。`width == 0` か空文字は 0 行を返す。
#[must_use]
pub fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + w > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += w;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// 幅 `term_width` の端末に、幅 `content_width` の内容を水平中央寄せするときの左パディング。
/// 内容が端末より広いときは 0 に飽和する。
#[must_use]
pub fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// 生の端末サイズを正規化する。非対話環境が報告する 0 を 80x24 のフォールバックに置き換える。
#[must_use]
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

#[cfg(test)]
mod tests {
    use super::{centered_padding, clip_to_width, display_width, normalize_size, wrap_to_width};

    #[test]
    fn display_width_counts_full_width_as_two_and_skips_ansi() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("あい"), 4); // 全角 2 文字 = 4 桁
        // SGR カラー（赤）は 0 桁。見た目の "hi" は 2 桁。
        assert_eq!(display_width("\u{1b}[31mhi\u{1b}[0m"), 2);
    }

    #[test]
    fn clip_to_width_returns_text_unchanged_when_it_fits() {
        assert_eq!(clip_to_width("abc", 3), "abc");
        assert_eq!(clip_to_width("abc", 10), "abc");
    }

    #[test]
    fn clip_to_width_zero_is_empty() {
        assert_eq!(clip_to_width("abc", 0), "");
    }

    #[test]
    fn clip_to_width_truncates_with_ellipsis() {
        // 5 桁に収める: 頭 4 文字 + `…`。
        assert_eq!(clip_to_width("abcdefg", 5), "abcd…");
    }

    #[test]
    fn clip_to_width_steps_whole_full_width_chars() {
        // 全角は 2 桁。max=3 なら budget=2 で 1 文字だけ入り、`…` が付く。
        assert_eq!(clip_to_width("あいう", 3), "あ…");
    }

    #[test]
    fn clip_to_width_carries_ansi_and_closes_with_reset() {
        // 色付きの長い行を切ると、色を持ち越しつつ末尾を RESET で閉じる。
        let clipped = clip_to_width("\u{1b}[31mabcdef", 4);
        assert!(clipped.starts_with("\u{1b}[31m"));
        assert!(clipped.ends_with("\u{1b}[0m"));
        assert!(clipped.contains('…'));
        // 見た目の桁数は max に収まる（色と reset は 0 桁）。
        assert!(display_width(&clipped) <= 4);
    }

    #[test]
    fn wrap_to_width_zero_yields_no_lines() {
        assert!(wrap_to_width("abc", 0).is_empty());
        assert!(wrap_to_width("", 5).is_empty());
    }

    #[test]
    fn wrap_to_width_breaks_between_characters() {
        assert_eq!(wrap_to_width("abcde", 2), vec!["ab", "cd", "e"]);
        // 幅 1 の行に全角: 各文字が単独行で溢れる（落とさない）。
        assert_eq!(wrap_to_width("あい", 1), vec!["あ", "い"]);
    }

    #[test]
    fn centered_padding_centers_and_saturates() {
        assert_eq!(centered_padding(10, 4), 3);
        assert_eq!(centered_padding(4, 10), 0); // 内容が広いと 0
    }

    #[test]
    fn normalize_size_substitutes_fallback_for_zeroes() {
        assert_eq!(normalize_size(0, 0), (24, 80));
        assert_eq!(normalize_size(30, 100), (30, 100));
    }
}
