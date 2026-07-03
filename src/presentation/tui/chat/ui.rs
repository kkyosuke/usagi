//! Pure rendering for the local-LLM chat screen: turn a [`Chat`] into a full
//! terminal frame (a `Vec<String>`, one entry per row). No terminal IO happens
//! here, so every layout branch is unit-tested.

use console::{style, Style};

use crate::presentation::theme::Palette;
use crate::presentation::tui::widgets::{self, block_caret};

use super::state::{Chat, Role};

/// Rows the header occupies at the top of the frame (title + blank).
const HEADER_ROWS: usize = 2;
/// Rows the footer occupies at the bottom (blank + input + hint).
const FOOTER_ROWS: usize = 3;

/// The prompt glyph before the composed line.
const PROMPT: &str = "❯";

/// Render the whole chat screen to `raw_height` × `raw_width` rows. `tick`
/// advances the "thinking" spinner while a reply is in flight.
pub fn render_frame(raw_height: usize, raw_width: usize, chat: &Chat, tick: usize) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    // A one-column left margin keeps text off the very edge; the content wraps
    // within the remaining width.
    let content_width = width.saturating_sub(2).max(1);

    let transcript = transcript_lines(chat, content_width, tick);
    // The transcript fills whatever is left between the header and the footer.
    let body_rows = height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let visible = window(&transcript, body_rows, chat.scroll());

    let mut lines = Vec::with_capacity(height);
    lines.push(header_line(chat.model(), width));
    lines.push(String::new());
    for row in 0..body_rows {
        match visible.get(row) {
            Some(line) => lines.push(format!(" {line}")),
            None => lines.push(String::new()),
        }
    }
    lines.push(String::new());
    lines.push(input_line(chat));
    lines.push(hint_line());
    // Clip every row to the terminal width so a long header / footer (or a
    // wide-glyph body row) can never overrun its column and corrupt the frame on
    // a narrow terminal. `clip_to_width` is ANSI-aware, so styling survives.
    for line in &mut lines {
        *line = widgets::clip_to_width(line, width);
    }
    lines
}

/// The title row: the mascot, the app, and the bound model name.
fn header_line(model: &str, width: usize) -> String {
    let title = format!("🐇 usagi chat · {model}");
    let title = widgets::clip_to_width(&title, width);
    style(title).accent().bold().to_string()
}

/// The composed-input row: a danger-coloured prompt glyph and the line with a
/// block caret, so ←/→/Home/End move a visible caret through it.
fn input_line(chat: &Chat) -> String {
    let prompt = style(PROMPT).danger().bold();
    let value = block_caret(
        chat.input().before(),
        chat.input().after(),
        &Style::new().accent(),
    );
    format!("{prompt} {value}")
}

/// The footer key hint.
fn hint_line() -> String {
    style("Enter 送信   ↑↓ スクロール   Esc 戻る")
        .dim()
        .to_string()
}

/// The transcript as styled, width-wrapped rows: each message gets a coloured
/// role label followed by its wrapped body and a blank separator. While a reply
/// is in flight a spinner line is appended; an empty transcript shows a hint.
fn transcript_lines(chat: &Chat, width: usize, tick: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if chat.messages().is_empty() && !chat.is_pending() {
        lines.push(
            style("ローカル LLM に話しかけてみましょう。")
                .dim()
                .to_string(),
        );
        return lines;
    }
    for message in chat.messages() {
        let (label, label_style) = match message.role {
            Role::User => ("You", Style::new().accent().bold()),
            Role::Assistant => (chat.model(), Style::new().success().bold()),
        };
        lines.push(label_style.apply_to(label).to_string());
        for wrapped in widgets::wrap_to_width(&message.text, width) {
            lines.push(wrapped);
        }
        lines.push(String::new());
    }
    if chat.is_pending() {
        let spinner = widgets::spinner_char(tick);
        lines.push(style(format!("{spinner} 考え中…")).warning().to_string());
    }
    lines
}

/// The window of `lines` to show in `rows` rows, scrolled `scroll` lines up from
/// the newest. `scroll` is clamped to the real overflow so scrolling past the
/// top is a no-op; when everything fits, the whole transcript is returned.
fn window(lines: &[String], rows: usize, scroll: usize) -> &[String] {
    if rows == 0 {
        return &[];
    }
    let overflow = lines.len().saturating_sub(rows);
    let scroll = scroll.min(overflow);
    let end = lines.len() - scroll;
    let start = end.saturating_sub(rows);
    &lines[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain (ANSI-stripped) text of a rendered frame, for readable
    /// assertions that ignore styling.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect()
    }

    #[test]
    fn empty_chat_shows_the_header_prompt_and_a_starter_hint() {
        let chat = Chat::new("qwen2.5-coder:7b");
        let frame = render_frame(12, 40, &chat, 0);
        let text = plain(&frame);
        // The frame is exactly the terminal height.
        assert_eq!(frame.len(), 12);
        // Header names the model.
        assert!(text[0].contains("usagi chat"));
        assert!(text[0].contains("qwen2.5-coder:7b"));
        // The starter hint sits in the transcript body.
        assert!(text.iter().any(|l| l.contains("話しかけて")));
        // The input prompt and footer hint are pinned to the bottom.
        assert!(text[text.len() - 2].contains(PROMPT));
        assert!(text[text.len() - 1].contains("Enter 送信"));
    }

    #[test]
    fn a_conversation_renders_both_roles_and_wraps_long_text() {
        let mut chat = Chat::new("ai");
        for c in "hi".chars() {
            chat.input_mut().insert(c);
        }
        chat.submit().unwrap();
        chat.finish(Ok("a fairly long reply that must wrap".to_string()));
        let frame = render_frame(20, 16, &chat, 0);
        let text = plain(&frame);
        // Both role labels are shown.
        assert!(text.iter().any(|l| l.contains("You")));
        assert!(text.iter().any(|l| l.trim() == "ai"));
        // The user message body is present.
        assert!(text.iter().any(|l| l.contains("hi")));
        // The reply wrapped across multiple rows (no row exceeds the width).
        assert!(frame
            .iter()
            .all(|l| console::measure_text_width(&console::strip_ansi_codes(l)) <= 16));
    }

    #[test]
    fn a_pending_reply_shows_the_spinner_line() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        let frame = render_frame(12, 40, &chat, 3);
        let text = plain(&frame);
        assert!(text.iter().any(|l| l.contains("考え中")));
    }

    #[test]
    fn window_tails_the_transcript_and_clamps_scroll() {
        let lines: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        // Pinned to the bottom: the last three lines.
        assert_eq!(window(&lines, 3, 0), &["7", "8", "9"]);
        // Scrolled up two: the window moves back by two.
        assert_eq!(window(&lines, 3, 2), &["5", "6", "7"]);
        // Scrolling past the top clamps to the first `rows` lines.
        assert_eq!(window(&lines, 3, 999), &["0", "1", "2"]);
        // Everything fits: the whole slice is returned.
        assert_eq!(window(&lines, 20, 0), lines.as_slice());
        // Zero rows yields nothing.
        assert!(window(&lines, 0, 0).is_empty());
    }

    #[test]
    fn scrolled_view_shows_older_messages() {
        let mut chat = Chat::new("m");
        // Build a transcript taller than a short body.
        for turn in 0..5 {
            for c in format!("q{turn}").chars() {
                chat.input_mut().insert(c);
            }
            chat.submit().unwrap();
            chat.finish(Ok(format!("a{turn}")));
        }
        // Scroll up far enough to reveal the very first turn.
        for _ in 0..40 {
            chat.scroll_up();
        }
        let frame = render_frame(10, 40, &chat, 0);
        let text = plain(&frame);
        assert!(text.iter().any(|l| l.contains("q0")));
    }

    #[test]
    fn a_zero_size_terminal_falls_back_without_panicking() {
        // Non-interactive environments report 0×0; the frame still renders.
        let chat = Chat::new("m");
        let frame = render_frame(0, 0, &chat, 0);
        assert_eq!(frame.len(), 24);
    }
}
