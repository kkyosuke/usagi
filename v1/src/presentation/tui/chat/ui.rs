//! Pure rendering for the local-LLM chat, drawn into the home screen's **right
//! pane** (the same rectangle the embedded terminal / agent use). [`pane`]
//! returns exactly `rows` lines, each clipped to `width`, so the home renderer
//! can drop it straight into the pane. No terminal IO happens here, so every
//! layout branch is unit-tested.

use console::{style, Style};

use crate::presentation::theme::Palette;
use crate::presentation::tui::widgets::{self, block_caret};

use super::state::{Chat, Role};

/// Rows the header occupies at the top of the pane (the model title).
const HEADER_ROWS: usize = 1;
/// Rows the footer occupies at the bottom (the input line + the key hint).
const FOOTER_ROWS: usize = 2;

/// The prompt glyph before the composed line.
const PROMPT: &str = "❯";

/// Render the chat into a `width` × `rows` right-pane rectangle. The transcript
/// fills the space between the one-row header and the two-row footer (input +
/// hint); a "thinking" spinner (advanced via [`Chat::advance_tick`]) shows while
/// a reply is in flight.
pub fn pane(chat: &Chat, width: usize, rows: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(rows);
    lines.push(header_line(chat.model(), width));

    let body_rows = rows.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let transcript = transcript_lines(chat, width);
    let visible = window(&transcript, body_rows, chat.scroll());
    for row in 0..body_rows {
        match visible.get(row) {
            Some(line) => lines.push(line.clone()),
            None => lines.push(String::new()),
        }
    }

    // Only draw the footer if the pane has room for it (a very short pane keeps
    // just the header + whatever transcript rows fit).
    if rows >= HEADER_ROWS + FOOTER_ROWS {
        lines.push(input_line(chat));
        lines.push(hint_line());
    }

    // Exactly `rows` lines: pad a short pane (a tiny one with no room for the
    // footer) and clamp an over-full one.
    lines.resize(rows, String::new());
    // Clip every row to the pane width so a long header / footer or a wide-glyph
    // body row never overruns its column. `clip_to_width` is ANSI-aware.
    for line in &mut lines {
        *line = widgets::clip_to_width(line, width);
    }
    lines
}

/// The title row: the mascot and the bound model name.
fn header_line(model: &str, width: usize) -> String {
    let title = format!("🐇 chat · {model}");
    style(widgets::clip_to_width(&title, width))
        .accent()
        .bold()
        .to_string()
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
    style("Enter 送信  ↑↓ スクロール  Esc 戻る")
        .dim()
        .to_string()
}

/// The transcript as styled, width-wrapped rows: each message gets a coloured
/// role label followed by its wrapped body and a blank separator. While a reply
/// is in flight a spinner line is appended; an empty transcript shows a hint.
fn transcript_lines(chat: &Chat, width: usize) -> Vec<String> {
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
        lines.extend(display_lines(&message.text, width));
        lines.push(String::new());
    }
    if chat.is_pending() {
        let spinner = widgets::spinner_char(chat.tick());
        lines.push(style(format!("{spinner} 考え中…")).warning().to_string());
    }
    lines
}

/// Turn a message body into display rows: split on newlines (a model reply is
/// usually multi-line), sanitise each raw line, then width-wrap it. Splitting
/// first is essential — a raw `\n` left inside a frame line would move the cursor
/// mid-row and corrupt the diff-painted screen. Blank source lines are preserved
/// as blank rows so paragraph spacing survives.
fn display_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let clean = sanitize(raw);
        let wrapped = widgets::wrap_to_width(&clean, width);
        if wrapped.is_empty() {
            out.push(String::new());
        } else {
            out.extend(wrapped);
        }
    }
    out
}

/// Make one raw line safe to draw: drop ANSI escapes (so widths are measured on
/// visible text only), turn tabs into a single space, and strip the remaining
/// control characters (`\r`, other C0) that would otherwise misalign or corrupt
/// the row. The caller has already split on `\n`, so none survive here.
fn sanitize(line: &str) -> String {
    console::strip_ansi_codes(line)
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
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

    /// The plain (ANSI-stripped) text of a rendered pane, for readable
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
        let out = pane(&chat, 40, 12);
        let text = plain(&out);
        // The pane is exactly `rows` lines.
        assert_eq!(out.len(), 12);
        // Header names the model.
        assert!(text[0].contains("chat"));
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
        let out = pane(&chat, 16, 20);
        let text = plain(&out);
        assert!(text.iter().any(|l| l.contains("You")));
        assert!(text.iter().any(|l| l.trim() == "ai"));
        assert!(text.iter().any(|l| l.contains("hi")));
        // Every row fits the pane width (the reply wrapped).
        assert!(out
            .iter()
            .all(|l| console::measure_text_width(&console::strip_ansi_codes(l)) <= 16));
    }

    #[test]
    fn a_pending_reply_shows_the_spinner_line() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        chat.advance_tick();
        let out = pane(&chat, 40, 12);
        assert!(plain(&out).iter().any(|l| l.contains("考え中")));
    }

    #[test]
    fn display_lines_splits_multiline_and_never_leaves_a_raw_newline() {
        let rows = display_lines("first line\nsecond line\n\nlast", 40);
        // Each source line becomes its own row; the blank line is preserved.
        assert_eq!(rows, vec!["first line", "second line", "", "last"]);
        // No row contains a raw newline (which would corrupt the frame).
        assert!(rows.iter().all(|r| !r.contains('\n')));
    }

    #[test]
    fn display_lines_wraps_japanese_by_display_width() {
        // Width-8 pane, width-2 CJK glyphs: four per row.
        let rows = display_lines("あいうえおか", 8);
        assert!(rows.len() >= 2);
        assert!(rows.iter().all(|r| console::measure_text_width(r) <= 8));
        // Nothing is lost across the wrap.
        assert_eq!(rows.concat(), "あいうえおか");
    }

    #[test]
    fn display_lines_strips_control_chars_and_ansi() {
        // Tabs become a space; carriage returns / other controls and ANSI escapes
        // are dropped so widths stay honest and the row never corrupts.
        let rows = display_lines("a\tb\r\n\x1b[31mred\x1b[0m", 40);
        assert_eq!(rows, vec!["a b", "red"]);
    }

    #[test]
    fn a_multiline_japanese_reply_renders_within_the_pane() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        chat.finish(Ok(
            "一行目です。\n二行目はもう少し長い日本語の返信です。".to_string()
        ));
        let out = pane(&chat, 20, 16);
        // Every row fits the pane and carries no raw newline.
        assert!(out.iter().all(|l| {
            let plain = console::strip_ansi_codes(l);
            console::measure_text_width(&plain) <= 20 && !l.contains('\n')
        }));
        // Both source lines are present (wrapped).
        let text = plain(&out).join("");
        assert!(text.contains("一行目です。"));
        assert!(text.contains("二行目"));
    }

    #[test]
    fn window_tails_the_transcript_and_clamps_scroll() {
        let lines: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(window(&lines, 3, 0), &["7", "8", "9"]);
        assert_eq!(window(&lines, 3, 2), &["5", "6", "7"]);
        assert_eq!(window(&lines, 3, 999), &["0", "1", "2"]);
        assert_eq!(window(&lines, 20, 0), lines.as_slice());
        assert!(window(&lines, 0, 0).is_empty());
    }

    #[test]
    fn scrolled_view_shows_older_messages() {
        let mut chat = Chat::new("m");
        for turn in 0..5 {
            for c in format!("q{turn}").chars() {
                chat.input_mut().insert(c);
            }
            chat.submit().unwrap();
            chat.finish(Ok(format!("a{turn}")));
        }
        for _ in 0..40 {
            chat.scroll_up();
        }
        let out = pane(&chat, 40, 10);
        assert!(plain(&out).iter().any(|l| l.contains("q0")));
    }

    #[test]
    fn a_tiny_pane_keeps_the_header_without_a_footer() {
        // Too short for the footer: only the header (and any transcript rows that
        // fit) are drawn, still exactly `rows` lines.
        let chat = Chat::new("m");
        let out = pane(&chat, 20, 2);
        assert_eq!(out.len(), 2);
        assert!(plain(&out)[0].contains("chat"));
        // No input prompt row in a footerless pane.
        assert!(!plain(&out).iter().any(|l| l.contains(PROMPT)));
    }
}
