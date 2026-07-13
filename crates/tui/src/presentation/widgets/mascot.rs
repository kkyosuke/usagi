//! Home sidebar の usagi mascot と speech bubble。
//!
//! message の選択・寿命・入力はここで所有しない。呼び出し側が表示してよい行だけを
//! [`MascotSpeech`] として渡し、この widget は幅安全な装飾 block にするだけである。

use crate::presentation::theme::Role;

use super::{display_width, pad_to_width, wrap_to_width};

const INDENT: usize = 1;
const SPEECH_CHROME: usize = 4;
const BOTTOM_GAP_ROWS: usize = 1;
const RABBIT: [&str; 3] = [" (\\(\\", " (o.o)?", "o(_(\")(\")"];

/// Sidebar に渡せる、表示安全な speech の行列。
///
/// 空行だけの message は無言と同じである。ANSI は受け取る時点で除去するため、bubble
/// の warning style が後続の frame に漏れない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MascotSpeech(Vec<String>);

impl MascotSpeech {
    /// 生の候補行から表示用 message を作る。内容がなければ `None` を返す。
    #[must_use]
    pub fn new(lines: impl IntoIterator<Item = String>) -> Option<Self> {
        let lines = lines
            .into_iter()
            .map(|line| strip_ansi(&line))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        (!lines.is_empty()).then_some(Self(lines))
    }

    fn lines(&self) -> &[String] {
        &self.0
    }
}

/// mascot 本体と、その直下に必ず残す空行を含む sidebar block。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MascotBlock {
    rows: Vec<String>,
}

impl MascotBlock {
    /// 描画する mascot / bubble の行。下部予約行は含まない。
    #[must_use]
    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// mascot の後に必要な、空行を含む行数。
    #[must_use]
    pub fn reserved_rows(&self) -> usize {
        self.rows.len() + BOTTOM_GAP_ROWS
    }
}

/// `width` の sidebar に収まる mascot block を作る。
///
/// speech がない、または bubble を組めない幅では静かな mascot を返す。mascot 自体も
/// 入らない幅なら `None` を返し、呼び出し側は list viewport を優先できる。
#[must_use]
pub fn sidebar_block(
    width: usize,
    tick: u64,
    speech: Option<&MascotSpeech>,
) -> Option<MascotBlock> {
    let rabbit_width = RABBIT
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0);
    let available = width.saturating_sub(INDENT);
    if available < rabbit_width {
        return None;
    }

    let mut plain_rows = speech
        .and_then(|speech| bubble_rows(speech, available))
        .unwrap_or_default();
    let bubble_rows = plain_rows.len();
    plain_rows.extend(rabbit_rows(tick));
    let block_width = plain_rows
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0);

    let rows = plain_rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let padded = format!("{}{}", " ".repeat(INDENT), pad_to_width(&row, block_width));
            let style = if index < bubble_rows {
                Role::Warning.style().bold()
            } else {
                Role::Feature.style().bold()
            };
            pad_to_width(&style.paint(&padded), width)
        })
        .collect();
    Some(MascotBlock { rows })
}

fn rabbit_rows(tick: u64) -> [String; 3] {
    let phase = tick % 6;
    let ears = if phase == 5 { " (\\(/" } else { " (\\(\\" };
    let face = if phase == 4 { " (-.-)?" } else { " (o.o)?" };
    [ears.to_owned(), face.to_owned(), RABBIT[2].to_owned()]
}

fn bubble_rows(speech: &MascotSpeech, max_width: usize) -> Option<Vec<String>> {
    let inner = max_width.checked_sub(SPEECH_CHROME)?;
    if inner == 0 {
        return None;
    }
    let content = speech
        .lines()
        .iter()
        .flat_map(|line| wrap_to_width(line, inner))
        .collect::<Vec<_>>();
    let content_width = content.iter().map(|row| display_width(row)).max()?;
    let span = content_width + 2;
    let mut rows = Vec::with_capacity(content.len() + 2);
    rows.push(format!("╭{}╮", "─".repeat(span)));
    rows.extend(content.into_iter().map(|line| {
        let padding = " ".repeat(content_width.saturating_sub(display_width(&line)));
        format!("│ {line}{padding} │")
    }));
    let mut bottom = String::from("╰");
    for column in 0..span {
        bottom.push(if column == 2 { '┬' } else { '─' });
    }
    bottom.push('╯');
    rows.push(bottom);
    Some(rows)
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) && next != '[' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MascotSpeech, sidebar_block};
    use crate::presentation::widgets::display_width;

    fn plain(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn silent_block_has_the_three_row_mascot_and_a_bottom_reservation() {
        let block = sidebar_block(20, 0, None).expect("mascot fits");
        assert_eq!(block.rows().len(), 3);
        assert_eq!(block.reserved_rows(), 4);
        assert!(plain(block.rows()).contains("(o.o)?"));
    }

    #[test]
    fn speech_uses_a_tailed_bubble_above_the_mascot() {
        let speech = MascotSpeech::new(["ready".to_owned()]).expect("speech");
        let block = sidebar_block(20, 0, Some(&speech)).expect("mascot fits");
        let text = plain(block.rows());
        assert!(text.contains("╭───────╮"));
        assert!(text.contains("│ ready │"));
        assert!(text.contains("╰──┬────╯"));
        assert!(text.contains("(o.o)?"));
        assert_eq!(block.reserved_rows(), block.rows().len() + 1);
    }

    #[test]
    fn speech_wraps_cjk_and_removes_input_ansi_before_painting() {
        let speech = MascotSpeech::new(["\u{1b}[31mあいうえお".to_owned()]).expect("speech");
        let block = sidebar_block(11, 0, Some(&speech)).expect("mascot fits");
        assert!(plain(block.rows()).contains("あい"));
        assert!(block.rows().iter().all(|row| display_width(row) == 11));
        assert!(block.rows().iter().all(|row| row.ends_with("\u{1b}[0m")));
    }

    #[test]
    fn empty_speech_and_too_narrow_mascot_are_safely_omitted() {
        assert!(MascotSpeech::new([String::new()]).is_none());
        assert!(sidebar_block(7, 0, None).is_none());
    }
}
