//! 右ペインの Chrome 風 tab strip と空 pane の純粋な描画部品。
//!
//! tab の identity / selection は reducer の責務である。この widget は投影済みの
//! `selected` フラグだけを描き、label や表示順を identity として扱わない。

use crate::presentation::theme::{Color, Role, Style};

use super::icon;

/// tab strip に渡す表示専用の 1 tab。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab<'a> {
    /// 表示ラベル。
    pub label: &'a str,
    /// reducer が stable identity から決めた選択状態。
    pub selected: bool,
    /// Pending launch indicator. Only this one-cell glyph is coloured so a
    /// loading tab does not turn its whole label into a moving colour band.
    pub pending_indicator: Option<&'a str>,
}

/// The coloured runner for an in-flight pane launch.
///
/// Only the rabbit carries colour. Its leading padding advances with `frame`,
/// so it runs through the loading chip while the tab label stays dim.
const RUNNING_RABBIT: &str = "\u{f0907}";

#[must_use]
pub fn pending_indicator(frame: u64) -> String {
    format!(
        "{}{}",
        " ".repeat((frame % 4) as usize),
        Role::Feature.style().bold().paint(RUNNING_RABBIT)
    )
}

/// Chrome 風 tab strip を上段の chip と下段の active marker として描く。
///
/// 常に 2 行を返す。選択された chip は accent、直下の marker は accent の `▔` となる。
/// 幅が狭い場合も、行全体を ANSI 対応の [`super::clip_to_width`] で詰める。
#[must_use]
pub fn render(width: usize, tabs: &[Tab<'_>]) -> [String; 2] {
    render_with_prefix(width, "", tabs)
}

/// [`render`] と同じ tab strip を、先頭の表示専用ラベルの直後に配置する。
///
/// `prefix` は chip 行にだけ描画し、marker 行には同じ表示幅の空白を置く。そのため
/// session 名などの右に tab を置いても、active marker は選択した chip の真下に揃う。
#[must_use]
pub fn render_with_prefix(width: usize, prefix: &str, tabs: &[Tab<'_>]) -> [String; 2] {
    let mut chips = format!("{prefix} ");
    let mut marker = " ".repeat(super::display_width(prefix) + 1);
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            chips.push(' ');
            marker.push(' ');
        }
        let text = format!(" {}{} ", tab.pending_indicator.unwrap_or(""), tab.label);
        let chip_width = super::display_width(&text);
        if tab.selected {
            // The running rabbit owns its colour. Keep its label neutral so
            // loading looks like a rabbit crossing the chip, not a colour band.
            let chip = if tab.pending_indicator.is_some() {
                Style::new().dim().paint(&text)
            } else {
                Role::Accent.style().bold().paint(&text)
            };
            chips.push_str(&chip);
            marker.push_str(&Role::Accent.style().bold().paint(&"▔".repeat(chip_width)));
        } else {
            chips.push_str(&Style::new().dim().paint(&text));
            marker.push_str(&" ".repeat(chip_width));
        }
    }
    [
        super::pad_to_width(&super::clip_to_width(&chips, width), width),
        super::pad_to_width(&super::clip_to_width(&marker, width), width),
    ]
}

/// tab が無い右ペイン本文を、静的うさぎと案内文で中央に置く。
///
/// この関数は tick や runtime を受け取らないため、同じ geometry と message なら常に同じ
/// フレームを返す。狭すぎる場合は通常の clipping により安全に縮退する。
#[must_use]
pub fn empty_pane(width: usize, rows: usize, message: &str) -> Vec<String> {
    empty_pane_with_detail(width, rows, message, None)
}

/// [`empty_pane`] に、画面が提供する安全な補足を加える。
///
/// 補足は feedback のように renderer がすでに表示安全と保証した文字列だけを渡す。
#[must_use]
pub fn empty_pane_with_detail(
    width: usize,
    rows: usize,
    message: &str,
    detail: Option<&str>,
) -> Vec<String> {
    // White + dim is deliberately explicit rather than inheriting the terminal
    // foreground: both the rabbit and every caption remain neutral gray.
    // Paint only after clipping so every styled line owns its final reset.
    let gray = Style::new().fg(Color::White).dim();
    let mut block = icon::centered(width)
        .into_iter()
        .map(|line| gray.paint(&super::pad_to_width(&line, width)))
        .collect::<Vec<_>>();
    block.push(String::new());
    for text in std::iter::once(message).chain(detail) {
        let caption = super::clip_to_width(text, width);
        let centered = format!(
            "{}{}",
            " ".repeat(super::centered_padding(
                width,
                super::display_width(&caption)
            )),
            caption
        );
        block.push(gray.paint(&super::pad_to_width(&centered, width)));
    }

    let top = rows.saturating_sub(block.len()) / 2;
    let mut lines = vec![String::new(); top];
    lines.extend(block);
    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        RUNNING_RABBIT, Tab, empty_pane, empty_pane_with_detail, pending_indicator, render,
    };
    use crate::presentation::widgets::display_width;

    fn strip(text: &str) -> String {
        let mut plain = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for byte in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&byte) && byte != '[' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        plain
    }

    #[test]
    fn selected_tab_has_an_accent_marker_below_its_chip() {
        let rows = render(
            40,
            &[
                Tab {
                    label: "Terminal",
                    selected: false,
                    pending_indicator: None,
                },
                Tab {
                    label: "Agent",
                    selected: true,
                    pending_indicator: None,
                },
            ],
        );
        assert!(strip(&rows[0]).contains(" Terminal   Agent "));
        assert!(!strip(&rows[1]).starts_with('▔'));
        assert!(strip(&rows[1]).contains('▔'));
        assert!(rows.iter().all(|row| display_width(row) == 40));
    }

    #[test]
    fn chrome_clips_ansi_styled_long_labels_at_narrow_widths() {
        let rows = render(
            6,
            &[Tab {
                label: "Terminal (resolving)",
                selected: true,
                pending_indicator: None,
            }],
        );
        assert!(rows.iter().all(|row| display_width(row) == 6));
        assert!(rows[0].ends_with("\u{1b}[0m") || rows[0].ends_with(' '));
    }

    #[test]
    fn pending_indicator_is_a_coloured_rabbit_that_runs_across_frames() {
        let indicator = pending_indicator(0);
        let later = pending_indicator(3);
        assert!(indicator.contains(RUNNING_RABBIT));
        assert_eq!(display_width(&indicator), 1);
        assert_ne!(indicator, later);
        assert_eq!(display_width(&later), 4);
        assert!(indicator.ends_with("\u{1b}[0m"));
        let tab = render(
            40,
            &[Tab {
                label: "Agent (starting)",
                selected: true,
                pending_indicator: Some(&indicator),
            }],
        );
        assert!(tab[0].contains(RUNNING_RABBIT));
        assert!(tab[0].contains("\u{1b}[2m"));
    }

    #[test]
    fn empty_pane_centers_static_rabbit_and_message() {
        let rows = empty_pane(30, 11, "No tabs stirring yet. Enter starts one.");
        let plain = rows.iter().map(|row| strip(row)).collect::<Vec<_>>();
        assert_eq!(rows.len(), 11);
        assert!(plain.iter().any(|row| row.contains("(='-')")));
        assert!(plain.iter().any(|row| row.contains("No tabs stirring yet")));
        assert!(rows.iter().all(|row| display_width(row) <= 30));
        assert!(
            rows.iter()
                .filter(|row| row.contains("(='-')"))
                .all(|row| row.starts_with("\u{1b}[2;37m") && row.ends_with("\u{1b}[0m"))
        );
        assert_eq!(
            rows,
            empty_pane(30, 11, "No tabs stirring yet. Enter starts one.")
        );
    }

    #[test]
    fn empty_pane_keeps_a_safe_detail_below_the_invitation() {
        let rows = empty_pane_with_detail(60, 12, "No tabs stirring yet.", Some("feedback: safe"));
        let plain = rows.iter().map(|row| strip(row)).collect::<Vec<_>>();
        assert!(
            plain
                .iter()
                .any(|row| row.contains("No tabs stirring yet."))
        );
        assert!(plain.iter().any(|row| row.contains("feedback: safe")));
    }

    #[test]
    fn empty_pane_centers_each_caption_and_closes_gray_style_when_clipped() {
        let rows = empty_pane_with_detail(13, 9, "launch pane", Some("safe detail"));
        let plain = rows.iter().map(|row| strip(row)).collect::<Vec<_>>();
        let caption = plain.iter().find(|row| row.contains("launch")).unwrap();
        assert!(
            caption.starts_with(' '),
            "caption should be pane-centered: {caption:?}"
        );
        assert!(
            rows.iter()
                .filter(|row| row.contains("launch") || row.contains("safe"))
                .all(|row| row.starts_with("\u{1b}[2;37m") && row.ends_with("\u{1b}[0m"))
        );

        let narrow = empty_pane(1, 7, "wide message");
        assert!(narrow.iter().all(|row| display_width(row) <= 1));
        assert!(
            narrow
                .iter()
                .filter(|row| row.contains("\u{1b}[2;37m"))
                .all(|row| row.ends_with("\u{1b}[0m"))
        );
    }
}
