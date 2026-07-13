//! 画面をまたいで再利用する UI 部品（widget）。v1 の `widgets/` を引き継ぎ、
//! `text_input`（キャレット編集付き 1 行入力）/ `icon`（うさぎ AA）/
//! `loading`（スピナー・進捗バー・ローディングうさぎ）/ `modal`（枠付きダイアログ）
//! を置く。特定の画面に固有の描画は [`super::views`] に置き、ここには複数 view から
//! 使い回す部品だけを置く。
//!
//! すべて実 IO を持たない純粋関数・値で、フレーム（ANSI 付き行の `Vec<String>`）を
//! 組み立てるか、その部品（`String`）を返す。色は [`super::theme`] が意味的な役割で
//! 一元管理し、view が widget の返す行に載せる。ここで直接色を選ぶことはしない
//! （widget は無色の構造・幾何・AA・編集ロジックだけを持つ）。この直下の関数は、
//! それらが共通して使うテキスト幅の測定・切り詰め・折り返しと、相対時刻の表記の
//! プリミティブである。

pub mod icon;
pub mod loading;
pub mod mascot;
pub mod modal;
pub mod select;
pub mod session_tab;
pub mod text_input;

pub use text_input::TextInput;

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthChar;

use super::theme::{Role, Style};

/// エスケープシーケンスの先頭（ESC）。表示桁数を測るとき読み飛ばす。
const ESC: char = '\u{1b}';

/// 切り詰めがスタイルを開いたまま断ち切ったとき末尾に付ける SGR リセット
/// (`ESC [ 0 m`)。開いた色が後続の内容に滲むのを防ぐ。
const RESET: &str = "\u{1b}[0m";

/// v1 の tab launch / session skeleton と同じ loading sweep。
#[must_use]
pub fn shimmer_text(text: &str, frame: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let head = frame % (chars.len() + 3);
    let mut out = String::new();
    for (index, character) in chars.into_iter().enumerate() {
        let character = character.to_string();
        if index == head || index + 1 == head {
            out.push_str(&Role::Accent.style().bold().paint(&character));
        } else {
            out.push_str(&Style::new().dim().paint(&character));
        }
    }
    out
}

/// `value` の byte-offset `cursor` に block cursor を描く。
///
/// キャレット直後の Unicode scalar を base style の reverse-video で反転し、文字を
/// 横へ押し出さない。空文字列・行末では反転空白を 1 セル描く。`cursor` は
/// [`TextInput`] が保証する char 境界を受け取るが、外部 caller にも安全なよう
/// value の範囲へ飽和する。
#[must_use]
pub fn block_caret(value: &str, cursor: usize, base: &Style) -> String {
    let mut cursor = cursor.min(value.len());
    while !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let (before, after) = value.split_at(cursor);
    let (caret, rest) = match after.chars().next() {
        Some(character) => after.split_at(character.len_utf8()),
        None => (" ", ""),
    };
    format!(
        "{}{}{}",
        if before.is_empty() {
            String::new()
        } else {
            base.paint(before)
        },
        base.reverse().paint(caret),
        if rest.is_empty() {
            String::new()
        } else {
            base.paint(rest)
        },
    )
}

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

/// `text` を表示幅 `width` にそろえる: 広ければ [`clip_to_width`] で切り、狭ければ末尾を空白で
/// 詰める。ANSI エスケープは 0 桁として扱うので、色付き行でも見た目どおりの幅になる。ペインや
/// カラムを固定幅にそろえて桁を合わせるのに使う。
#[must_use]
pub fn pad_to_width(text: &str, width: usize) -> String {
    let clipped = clip_to_width(text, width);
    let pad = width.saturating_sub(display_width(&clipped));
    format!("{clipped}{}", " ".repeat(pad))
}

/// 生の端末サイズを正規化する。非対話環境が報告する 0 を 80x24 のフォールバックに置き換える。
#[must_use]
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// `from` から `now` までの経過時間を短いラベルにする: `just now` / `5min ago` /
/// `3h ago` / `2d ago` / `3w ago`、1 か月を超えたら絶対日付 `YYYY-MM-DD` に落とす。
/// 未来の `from`（時計のずれ）は `just now` とみなす。「最終利用」の言い回しの単一情報源で、
/// welcome 画面の recent カードが使う。`now` は呼び出し側が渡す（この層は実時計を読まない）。
#[must_use]
pub fn relative_time(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - from).num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}min ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    if days < 30 {
        return format!("{}w ago", days / 7);
    }
    from.format("%Y-%m-%d").to_string()
}

/// Session sidebar 用の簡潔な相対時刻。`now` / `5m ago` / `3h ago` と表す。
#[must_use]
pub fn relative_session_time(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - from).num_seconds();
    if secs < 60 {
        return "now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    if days < 30 {
        return format!("{}w ago", days / 7);
    }
    from.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        block_caret, centered_padding, clip_to_width, display_width, normalize_size,
        relative_session_time, relative_time, wrap_to_width,
    };
    use crate::presentation::theme::Role;
    use chrono::{DateTime, Duration, Utc};

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn display_width_counts_full_width_as_two_and_skips_ansi() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("あい"), 4); // 全角 2 文字 = 4 桁
        // SGR カラー（赤）は 0 桁。見た目の "hi" は 2 桁。
        assert_eq!(display_width("\u{1b}[31mhi\u{1b}[0m"), 2);
    }

    #[test]
    fn block_caret_reverses_the_edit_cell_without_shifting_ascii_or_cjk_text() {
        let accent = Role::Accent.style().bold();
        let ascii = block_caret("abcd", 2, &accent);
        assert_eq!(display_width(&ascii), 4);
        assert!(ascii.contains("\u{1b}[1;7;36mc\u{1b}[0m"));

        let cjk = block_caret("あいう", "あ".len(), &accent);
        assert_eq!(display_width(&cjk), 6);
        assert!(cjk.contains("\u{1b}[1;7;36mい\u{1b}[0m"));

        // Public callers cannot split a scalar even if they pass a non-boundary byte offset.
        let clamped = block_caret("あ", 2, &accent);
        assert_eq!(display_width(&clamped), 2);
        assert!(clamped.contains("\u{1b}[1;7;36mあ\u{1b}[0m"));
    }

    #[test]
    fn block_caret_draws_a_reversed_space_for_empty_and_end_positions() {
        let accent = Role::Accent.style();
        assert_eq!(block_caret("", 0, &accent), "\u{1b}[7;36m \u{1b}[0m");
        assert_eq!(
            block_caret("ab", usize::MAX, &accent),
            "\u{1b}[36mab\u{1b}[0m\u{1b}[7;36m \u{1b}[0m"
        );
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
    fn pad_to_width_pads_and_clips() {
        use super::pad_to_width;
        // 狭い: 末尾を空白で詰める。
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        // ちょうど: そのまま。
        assert_eq!(pad_to_width("abc", 3), "abc");
        // 広い: 省略記号付きに切る（表示幅は width 以内）。
        assert!(display_width(&pad_to_width("abcdef", 4)) <= 4);
        // ANSI は 0 桁: 色付き "hi" を 4 桁に詰めると末尾に空白 2。
        let padded = pad_to_width("\u{1b}[31mhi\u{1b}[0m", 4);
        assert_eq!(display_width(&padded), 4);
        assert!(padded.ends_with("  "));
    }

    #[test]
    fn normalize_size_substitutes_fallback_for_zeroes() {
        assert_eq!(normalize_size(0, 0), (24, 80));
        assert_eq!(normalize_size(30, 100), (30, 100));
    }

    #[test]
    fn relative_time_scales_the_label_by_age() {
        let now = at("2026-06-25T12:00:00Z");
        assert_eq!(relative_time(now, now), "just now");
        // 未来（時計のずれ）も just now。
        assert_eq!(relative_time(now + Duration::minutes(5), now), "just now");
        assert_eq!(relative_time(now - Duration::seconds(59), now), "just now");
        assert_eq!(relative_time(now - Duration::minutes(11), now), "11min ago");
        assert_eq!(relative_time(now - Duration::hours(3), now), "3h ago");
        assert_eq!(relative_time(now - Duration::days(2), now), "2d ago");
        assert_eq!(relative_time(now - Duration::days(20), now), "2w ago");
    }

    #[test]
    fn relative_session_time_uses_the_compact_sidebar_vocabulary() {
        let now = at("2026-06-25T12:00:00Z");
        assert_eq!(relative_session_time(now, now), "now");
        assert_eq!(
            relative_session_time(now - Duration::seconds(59), now),
            "now"
        );
        assert_eq!(
            relative_session_time(now - Duration::minutes(11), now),
            "11m ago"
        );
        assert_eq!(
            relative_session_time(now - Duration::hours(3), now),
            "3h ago"
        );
        assert_eq!(
            relative_session_time(now - Duration::days(2), now),
            "2d ago"
        );
        assert_eq!(
            relative_session_time(now - Duration::days(20), now),
            "2w ago"
        );
        assert_eq!(
            relative_session_time(at("2026-05-01T00:00:00Z"), now),
            "2026-05-01"
        );
    }

    #[test]
    fn relative_time_falls_back_to_an_absolute_date_after_a_month() {
        let now = at("2026-06-25T12:00:00Z");
        // 1 か月を超えると絶対日付になる。
        assert_eq!(relative_time(at("2026-05-01T00:00:00Z"), now), "2026-05-01");
    }
}
