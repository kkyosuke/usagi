//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — the usagi mascot, screen titles, dimmed subtitles/footers, and the modal
//! box that overlays a screen — live here so every screen renders them
//! consistently. Stateful, reusable widgets (e.g. the searchable [`picker`])
//! live in submodules.

pub mod dir_picker;
pub mod picker;
pub mod text_input;

use console::{style, Style};
use unicode_width::UnicodeWidthChar;

/// The usagi mascot artwork (raw, unstyled lines).
const RABBIT: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

/// The escape (ESC, `0x1b`) that introduces an ANSI control sequence.
const ESC: char = '\u{1b}';

/// Shortens `text` to at most `max` display columns, appending an ellipsis when
/// it has to cut (the head of the text is the most informative part).
///
/// A single forward pass accumulates display width and copies characters until
/// the next visible one would overflow — O(n), not the O(n²) of re-measuring a
/// growing clone each step. ANSI escape sequences (the SGR colours a styled line
/// carries) have zero display width and are copied verbatim, matching
/// [`console::measure_text_width`], so the clipped text keeps its colours and
/// never counts an escape against the budget.
///
/// The shared truncation primitive: panes clip rows to their column, and
/// [`render_modal`] clips modal content to the box so nothing ever overruns its
/// bounds.
pub fn clip_to_width(text: &str, max: usize) -> String {
    if console::measure_text_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis.
    let budget = max - 1;
    let mut out = String::with_capacity(text.len());
    let mut width = 0usize;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
            // Copy the whole escape sequence (zero display width) so the colour
            // it selects survives the clip. The styled lines clipped here carry
            // CSI/SGR sequences — `ESC [ … final` — so copy the `[` introducer
            // and parameter bytes through to (and including) the final byte
            // (`0x40..=0x7e`, excluding the `[` introducer itself).
            out.push(ch);
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
    out
}

/// Left padding that horizontally centres content of `content_width` columns
/// within a terminal `term_width` columns wide. Saturates to 0 when the content
/// is wider than the terminal.
pub fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// Normalises a raw terminal size, substituting an 80x24 fallback for the
/// zeroes that non-interactive environments report.
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// Centres a single line of `text` by left-padding it with spaces.
fn centered(width: usize, text: &str) -> String {
    let padding = " ".repeat(centered_padding(width, text.chars().count()));
    format!("{padding}{text}")
}

/// The usagi mascot, centred for the terminal width and styled magenta-bold.
///
/// The whole block shares a single padding so the art stays aligned.
pub fn rabbit_lines(width: usize) -> Vec<String> {
    let rabbit_width = RABBIT.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let padding = " ".repeat(centered_padding(width, rabbit_width));
    RABBIT
        .iter()
        .map(|line| {
            style(format!("{padding}{line}"))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// The raw (unstyled) lines of the usagi mascot, for callers that place the art
/// themselves rather than centring it (e.g. the home screen's top-right update
/// notice).
pub fn rabbit_art() -> [&'static str; 3] {
    RABBIT
}

/// Braille spinner frames cycled beside the loading rabbit, one per tick.
const LOADING_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The hopping rabbit's poses as `(ears, body)`. The ears sit centred over the
/// head (the `∩∩` lands on the `ㅅ`), and each "hop" pose shifts the ears *and*
/// the body together by one column so they bounce as a unit without the ears
/// drifting off the head. The blink (`-ㅅ-`) lands on the third pose, so cycling
/// the poses reads as a rabbit hopping in place.
const LOADING_POSES: [(&str, &str); 4] = [
    ("  ∩∩", "(･ㅅ･)づ"),
    ("   ∩∩", " (･ㅅ･)づ"),
    ("  ∩∩", "(-ㅅ-)づ"),
    ("   ∩∩", " (･ㅅ･)づ"),
];

/// A two-line "loading" rabbit for the home screen's top-right corner: a hopping
/// usagi with a braille spinner and a short `label` (e.g. `削除中… 2/5`). `frame`
/// is a monotonically advancing tick — the pose and spinner are picked from it,
/// so painting successive frames animates the rabbit.
///
/// Both rows are padded to a common block width and styled magenta-bold (the
/// mascot's colour), so the block right-aligns cleanly when
/// [`overlay_top_right`](super::super::tui::home::ui) anchors it to the top rows
/// — exactly like the [`update_banner`](super::super::tui::home::ui) notice it
/// shares that corner with.
pub fn loading_rabbit(frame: usize, label: &str) -> Vec<String> {
    let (ears, body) = LOADING_POSES[frame % LOADING_POSES.len()];
    let spinner = LOADING_SPINNER[frame % LOADING_SPINNER.len()];
    let rows = [ears.to_string(), format!("{body}{spinner} {label}")];
    let block_w = rows
        .iter()
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|row| {
            let pad = block_w.saturating_sub(console::measure_text_width(&row));
            style(format!("{row}{}", " ".repeat(pad)))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// A centred, green-bold screen title.
pub fn title_line(width: usize, title: &str) -> String {
    style(centered(width, title)).green().bold().to_string()
}

/// A centred, dimmed line — used for subtitles and footers.
pub fn dim_line(width: usize, text: &str) -> String {
    style(centered(width, text)).dim().to_string()
}

/// A left/right value chooser — the shared rendering primitive for every
/// settings field that cycles through choices.
///
/// The value is always wrapped in chevrons — `< Dark >` — so every field reads
/// as a left/right selector and the chevrons line up in a single column down
/// the screen. Colour conveys state: the `focused` row is bright (cyan-bold),
/// the rest are dimmed.
///
/// `changed` marks a value that differs from what is saved on disk: it is
/// painted yellow (taking priority over the focused/idle colours) so unsaved
/// edits stand out at a glance.
pub fn chooser(value: &str, focused: bool, changed: bool) -> String {
    let paint = |text: &str| {
        let styled = style(text.to_string());
        if changed {
            styled.yellow().bold()
        } else if focused {
            styled.cyan().bold()
        } else {
            styled.dim()
        }
        .to_string()
    };

    format!("{} {} {}", paint("<"), paint(value), paint(">"))
}

/// Renders the two halves of a [`text_input::TextInput`] — the text `before` the
/// caret and the text `after` it — as one line carrying a **block caret**.
///
/// The character under the caret (the first of `after`) is drawn in reverse
/// video so it reads as a solid block sitting *on* that character, the way a
/// terminal or Claude's prompt shows its cursor. At the end of the line, where
/// there is no character to highlight, a reversed space stands in. Because the
/// block only recolours an existing cell instead of inserting a glyph, the text
/// never shifts sideways as the caret moves through it.
///
/// `base` paints the line; the caret cell reuses that style reversed, so it
/// inherits the field's colour. Styling follows the terminal's colour support
/// (tests can force it with [`console::Style::force_styling`]).
pub fn block_caret(before: &str, after: &str, base: &Style) -> String {
    let (caret, rest) = match after.chars().next() {
        Some(first) => after.split_at(first.len_utf8()),
        None => (" ", ""),
    };
    format!(
        "{}{}{}",
        base.apply_to(before),
        base.clone().reverse().apply_to(caret),
        base.apply_to(rest),
    )
}

/// Wraps `lines` in a single-bordered box `inner_width` columns wide, with
/// `title` embedded in the top border.
///
/// Each content line is padded — by *display* width, so text carrying ANSI
/// styling still aligns — to `inner_width`, with one space of breathing room on
/// each side. The returned rows are not yet placed; [`render_modal`] centres
/// them. A shared primitive so every modal dialog shares one frame.
pub fn boxed(title: &str, inner_width: usize, lines: &[String]) -> Vec<String> {
    // Columns between the two corner glyphs: the content area plus one space of
    // padding on each side.
    let span = inner_width + 2;
    let label = if title.is_empty() {
        String::new()
    } else {
        // Clip the title (with its `─ ` / ` ` framing) to the span so a long
        // title never pushes the top border past the box edge.
        clip_to_width(&format!("─ {title} "), span)
    };
    let label_width = console::measure_text_width(&label);
    let top = format!("┌{label}{}┐", "─".repeat(span.saturating_sub(label_width)));
    let bottom = format!("└{}┘", "─".repeat(span));

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(top);
    for line in lines {
        // Clip first so a line wider than the box can never push the right
        // border out; then pad short lines so every row is exactly `inner_width`.
        let line = clip_to_width(line, inner_width);
        let pad = inner_width.saturating_sub(console::measure_text_width(&line));
        out.push(format!("│ {line}{} │", " ".repeat(pad)));
    }
    out.push(bottom);
    out
}

/// Renders `body` inside a centred [`boxed`] modal for a raw terminal size.
///
/// The box is centred both horizontally and vertically over an otherwise blank
/// frame, mirroring how the full-screen screens build their frames so the event
/// loop can clear and redraw it the same way.
pub fn render_modal(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    // The box needs `inner_width + 4` columns (two borders + a space of padding
    // on each side). Clamp the inner width so the box never overruns a narrow
    // terminal; `boxed` then clips each line and the title to fit.
    let inner_width = inner_width.min(width.saturating_sub(4));
    let box_lines = boxed(title, inner_width, body);
    // The box is `inner_width` plus the two spaces of padding and two borders.
    let pad = " ".repeat(centered_padding(width, inner_width + 4));

    let mut lines = Vec::with_capacity(height);
    let top_padding = height.saturating_sub(box_lines.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    for line in &box_lines {
        lines.push(format!("{pad}{line}"));
    }
    while lines.len() < height {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_padding_centers_content() {
        assert_eq!(centered_padding(80, 10), 35);
        assert_eq!(centered_padding(81, 10), 35);
    }

    #[test]
    fn centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(5, 10), 0);
    }

    #[test]
    fn normalize_size_substitutes_fallbacks_for_zero() {
        assert_eq!(normalize_size(0, 0), (24, 80));
    }

    #[test]
    fn normalize_size_keeps_nonzero_values() {
        assert_eq!(normalize_size(30, 100), (30, 100));
    }

    #[test]
    fn rabbit_lines_are_three_centered_mascot_rows() {
        let lines = rabbit_lines(80);
        assert_eq!(lines.len(), 3);
        // The mascot face appears, and the block is indented (centred).
        assert!(lines.iter().any(|l| l.contains("(='-')")));
        assert!(lines[0].starts_with(' '));
    }

    #[test]
    fn title_line_contains_the_title() {
        assert!(title_line(80, "USAGI").contains("USAGI"));
    }

    #[test]
    fn loading_rabbit_carries_the_label_and_a_spinner_frame() {
        let lines = loading_rabbit(2, "削除中… 2/5");
        assert_eq!(lines.len(), 2);
        let plain = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        // The label rides the body row, and the blink pose shows on this frame.
        assert!(plain.contains("削除中… 2/5"));
        assert!(plain.contains("(-ㅅ-)"));
        // The braille spinner for frame 2 is present.
        assert!(plain.contains('⠹'));
    }

    #[test]
    fn loading_rabbit_rows_share_one_block_width() {
        // Both rows pad to the widest, so the block right-aligns as a rectangle
        // when anchored to the top-right corner.
        let lines = loading_rabbit(0, "読み込み中…");
        let w0 = console::measure_text_width(&lines[0]);
        let w1 = console::measure_text_width(&lines[1]);
        assert_eq!(w0, w1);
    }

    #[test]
    fn loading_rabbit_animates_across_frames() {
        // Advancing the frame cycles the spinner glyph, so successive paints move.
        let a = console::strip_ansi_codes(&loading_rabbit(0, "x").join("\n")).into_owned();
        let b = console::strip_ansi_codes(&loading_rabbit(1, "x").join("\n")).into_owned();
        assert_ne!(a, b);
    }

    #[test]
    fn loading_rabbit_keeps_the_ears_over_the_head_through_the_hop() {
        // The display column of the first ear must line up with the head centre
        // (`ㅅ`) on both the resting (frame 0) and hopped (frame 1) poses, so the
        // ears never drift off the head as the rabbit bounces.
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for frame in [0usize, 1] {
            let lines = loading_rabbit(frame, "x");
            assert_eq!(
                col_of(&lines[0], '∩'),
                col_of(&lines[1], 'ㅅ'),
                "ears must sit over the head on frame {frame}",
            );
        }
    }

    #[test]
    fn dim_line_contains_the_text() {
        assert!(dim_line(80, "hint").contains("hint"));
    }

    #[test]
    fn chooser_always_brackets_the_value() {
        // Chevrons show whether focused or not, so every field reads as a
        // selector and the chevrons align down the column.
        for focused in [true, false] {
            let rendered = chooser("Dark", focused, false);
            assert!(rendered.contains("Dark"));
            assert!(rendered.contains('<'));
            assert!(rendered.contains('>'));
        }
    }

    #[test]
    fn chooser_keeps_the_value_aligned_across_focus() {
        // Focus changes only the colour, not the layout, so the visible width is
        // identical and the column never jumps.
        let focused = console::strip_ansi_codes(&chooser("On", true, false)).into_owned();
        let idle = console::strip_ansi_codes(&chooser("On", false, false)).into_owned();
        assert_eq!(focused, idle);
    }

    #[test]
    fn chooser_marks_changed_values() {
        // A changed value still renders its text; the colour difference is what
        // signals the unsaved edit, and it applies whether focused or not.
        assert!(chooser("Gemini", true, true).contains("Gemini"));
        assert!(chooser("Gemini", false, true).contains("Gemini"));
    }

    #[test]
    fn block_caret_highlights_the_character_under_the_caret() {
        // Force styling so the reverse-video codes are emitted whether or not the
        // test's stdout is a terminal.
        let base = Style::new().force_styling(true);
        let line = block_caret("ab", "cd", &base);
        // The caret recolours 'c' (the first char after it) without inserting a
        // cell, so the visible text is still "abcd".
        assert_eq!(&*console::strip_ansi_codes(&line), "abcd");
        let reversed = base.clone().reverse().apply_to("c").to_string();
        assert!(
            line.contains(&reversed),
            "the char under the caret is reversed"
        );
    }

    #[test]
    fn block_caret_at_the_end_reverses_a_trailing_space() {
        let base = Style::new().force_styling(true);
        let line = block_caret("ab", "", &base);
        // With no character to sit on, the caret is a reversed space — the one
        // cell the line grows by, marking where the next character lands.
        assert_eq!(&*console::strip_ansi_codes(&line), "ab ");
        let reversed = base.clone().reverse().apply_to(" ").to_string();
        assert!(line.contains(&reversed));
    }

    #[test]
    fn block_caret_sits_on_whole_multibyte_characters() {
        // The caret lands on a whole multi-byte char, never splitting one.
        let base = Style::new().force_styling(true);
        let line = block_caret("あ", "いう", &base);
        assert_eq!(&*console::strip_ansi_codes(&line), "あいう");
        let reversed = base.clone().reverse().apply_to("い").to_string();
        assert!(line.contains(&reversed));
    }

    #[test]
    fn boxed_frames_the_lines_with_a_title_and_borders() {
        let lines = boxed("Title", 10, &["hi".to_string(), "world".to_string()]);
        // Two content rows plus the top and bottom borders.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains("Title"));
        assert!(lines[0].ends_with('┐'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(lines.last().unwrap().ends_with('┘'));
        // Content rows are bordered and equal width (padded to the inner width).
        assert!(lines[1].contains("hi"));
        assert!(lines[2].contains("world"));
        assert_eq!(
            console::measure_text_width(&lines[1]),
            console::measure_text_width(&lines[2]),
        );
    }

    #[test]
    fn boxed_without_a_title_is_all_dashes_on_top() {
        let lines = boxed("", 4, &["x".to_string()]);
        // No title segment: the top border is corners plus a run of dashes.
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains('─'));
        assert!(!lines[0].contains(' '));
    }

    #[test]
    fn render_modal_centers_a_box_over_a_full_frame() {
        let frame = render_modal(24, 80, "Pick", 20, &["row".to_string()]);
        assert_eq!(frame.len(), 24);
        let joined = frame.join("\n");
        assert!(joined.contains("Pick"));
        assert!(joined.contains("row"));
        // The box is offset from the left edge (horizontally centred).
        let box_row = frame.iter().find(|l| l.contains("Pick")).unwrap();
        assert!(box_row.starts_with(' '));
        // Blank rows above the box (vertically centred).
        assert!(frame[0].is_empty());
    }

    #[test]
    fn boxed_clips_a_line_wider_than_the_inner_width() {
        // A body line longer than the box must be truncated (with an ellipsis),
        // never pushing the right border out — every row stays the same width.
        let lines = boxed("T", 6, &["short".to_string(), "way too long".to_string()]);
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| console::measure_text_width(l))
            .collect();
        assert!(widths.iter().all(|&w| w == widths[0]));
        assert!(lines[2].contains('…'));
    }

    #[test]
    fn boxed_clips_a_title_wider_than_the_box() {
        // A long title is truncated so the top border never overruns the box.
        let lines = boxed("An extremely long modal title", 4, &["x".to_string()]);
        assert_eq!(
            console::measure_text_width(&lines[0]),
            console::measure_text_width(lines.last().unwrap()),
        );
        assert!(lines[0].ends_with('┐'));
    }

    #[test]
    fn render_modal_never_overflows_a_narrow_terminal() {
        // The requested inner width (40) is far wider than the terminal (20);
        // the box must be clamped and every row must fit within the width.
        let width = 20;
        let frame = render_modal(
            24,
            width,
            "Local LLM",
            40,
            &["ローカル LLM をインストールします".to_string()],
        );
        for line in &frame {
            assert!(
                console::measure_text_width(line) <= width,
                "row overflows {width} cols: {line:?}",
            );
        }
        // The box is still drawn (a border row is present).
        assert!(frame.iter().any(|l| l.contains('┌')));
    }
}
