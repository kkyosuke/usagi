//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — the usagi mascot, screen titles, dimmed subtitles/footers, and the modal
//! box that overlays a screen — live here so every screen renders them
//! consistently. Stateful, reusable widgets (e.g. the searchable [`picker`])
//! live in submodules.

pub mod dir_picker;
pub mod picker;
pub mod text_area;
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

/// The mascot waving goodbye, drawn inside the farewell box: the usagi from
/// [`RABBIT`] with a raised paw (`ﾉ`) and its parting words alongside.
const FAREWELL_ART: [&str; 3] = ["  (\\(\\", " ( ^ω^)ﾉ  またね、ぴょん！", " o(_(\")(\")"];
/// Spaces padding the art from the box's side borders.
const FAREWELL_PAD: usize = 2;

/// The rounded box bidding the user farewell — shown both when usagi tears down
/// the alternate screen on exit and when the `quit`/`exit` command runs, so the
/// two share one look.
///
/// The box is sized to the widest art row ([`console::measure_text_width`],
/// matching how the rest of the TUI counts columns) and every row is padded to
/// that width, so the right edge lines up despite the art's mix of half- and
/// full-width characters. The frame is dim and the rabbit cyan — a soft pairing
/// that echoes the TUI's accent palette without shouting. The embedded ANSI
/// survives both the raw exit write and the log pane's pass-through rendering of
/// `Output` lines.
pub fn farewell_lines() -> Vec<String> {
    let content = FAREWELL_ART
        .iter()
        .map(|l| console::measure_text_width(l))
        .max()
        .unwrap_or(0);
    let inner = content + FAREWELL_PAD * 2;
    let rule = "─".repeat(inner);
    let frame = Style::new().dim();
    let rabbit = Style::new().cyan();

    let mut lines = Vec::with_capacity(FAREWELL_ART.len() + 2);
    lines.push(frame.apply_to(format!("╭{rule}╮")).to_string());
    for art in FAREWELL_ART {
        let right = inner - FAREWELL_PAD - console::measure_text_width(art);
        lines.push(format!(
            "{}{}{}",
            frame.apply_to(format!("│{}", " ".repeat(FAREWELL_PAD))),
            rabbit.apply_to(art),
            frame.apply_to(format!("{}│", " ".repeat(right))),
        ));
    }
    lines.push(frame.apply_to(format!("╰{rule}╯")).to_string());
    lines
}

/// Braille spinner frames cycled beside the loading rabbit, one per tick.
const LOADING_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The braille spinner glyph for `frame`, wrapping the [`LOADING_SPINNER`]
/// cycle. Shared so callers that animate their own rows (the background-task
/// panel) spin in step with the loading rabbit without owning the frame table.
pub fn spinner_char(frame: usize) -> &'static str {
    LOADING_SPINNER[frame % LOADING_SPINNER.len()]
}

/// A fixed-width `[===>   ]` progress bar `width` columns wide (inside the
/// brackets), filled to the `done / total` fraction rounded to the nearest
/// column. Unlike a per-task spinner this is a **real** ratio — the share of a
/// batch of background tasks that has finished — so the bar grows as each one
/// completes. An empty bar (`done == 0`) is all blanks, a complete one
/// (`done >= total`) all `=`, and a partial one marks its leading edge with `>`.
/// Returns the empty string when there is nothing to scale against (`total == 0`
/// or `width == 0`); the caller styles the result.
pub fn progress_bar(done: usize, total: usize, width: usize) -> String {
    if total == 0 || width == 0 {
        return String::new();
    }
    let done = done.min(total);
    // Round the filled fraction onto the bar's columns.
    let filled = (done * width + total / 2) / total;
    if filled == 0 {
        return format!("[{}]", " ".repeat(width));
    }
    if filled >= width {
        return format!("[{}]", "=".repeat(width));
    }
    // Partial: `=` up to the leading edge, a `>` head, then blanks — the three
    // spans always sum to `width`.
    format!(
        "[{}>{}]",
        "=".repeat(filled - 1),
        " ".repeat(width - filled)
    )
}

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

/// Faces the time-based loading rabbit ([`loading_rabbit_timed`]) cycles
/// through. Each is a three-cell `XㅅX` mask whose side glyphs are width-1, so
/// the centre `ㅅ` always lands in the same display column and the ears stay
/// over the head no matter which face shows. They convey no progress — the
/// caller advances `face_index` on a wall-clock timer, so the expression simply
/// changes on its own while a background task runs.
const LOADING_FACES: [&str; 6] = ["･ㅅ･", "-ㅅ-", "^ㅅ^", "oㅅo", ">ㅅ<", "=ㅅ="];

/// A two-line loading rabbit whose **bounce and face advance on separate axes**:
/// `hop_frame` drives the hop (and the braille spinner), while `face_index`
/// picks the [`LOADING_FACES`] expression. Used by the background-install
/// overlay, where there is no progress to report — the caller derives both
/// indices from elapsed time, so the rabbit hops and changes expression purely
/// with the clock.
///
/// Like [`loading_rabbit`], both rows are padded to a common block width and
/// styled magenta-bold so the block right-aligns cleanly when
/// [`overlay_top_right`] anchors it to the top-right corner.
pub fn loading_rabbit_timed(hop_frame: usize, face_index: usize, label: &str) -> Vec<String> {
    // The hop shifts the ears and body together by one column, exactly as the
    // progress-driven `loading_rabbit` poses do, so the bounce reads the same.
    let lead = " ".repeat(hop_frame % 2);
    let face = LOADING_FACES[face_index % LOADING_FACES.len()];
    let spinner = LOADING_SPINNER[hop_frame % LOADING_SPINNER.len()];
    let rows = [
        format!("  {lead}∩∩"),
        format!("{lead}({face})づ{spinner} {label}"),
    ];
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

/// A two-line "finished" rabbit for the background-install overlay: a resting
/// usagi with a happy (`^ㅅ^`) or dejected (`>ㅅ<`) face and the outcome
/// `message`. No spinner — the work is done. Padded and styled like
/// [`loading_rabbit_timed`] so it drops into the same corner.
pub fn done_rabbit(ok: bool, message: &str) -> Vec<String> {
    let face = if ok { "^ㅅ^" } else { ">ㅅ<" };
    let mark = if ok { "✓" } else { "✗" };
    let rows = ["  ∩∩".to_string(), format!("({face})づ{mark} {message}")];
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

/// The running usagi's two content rows — `(ears, body)` — by travel direction.
/// Speed lines (`ﾐ`) trail *behind* the run — on the left when heading right, on
/// the right when heading left — so the rabbit reads as dashing that way while
/// the face keeps its single `ㅅ` nose. The head's `ㅅ` is width-2 like every
/// other usagi face, and the ears sit centred over it (each direction pads the
/// ears so they stay above the head). [`running_rabbit`] draws these as a
/// three-row block that bobs up and down so a rabbit translated across the
/// screen reads as bounding.
const RUNNER_RIGHT: [&str; 2] = ["   ∩∩", "ﾐ(･ㅅ･)"];
const RUNNER_LEFT: [&str; 2] = ["  ∩∩", "(･ㅅ･)ﾐ"];

/// The display width of the running usagi sprite, so a caller can bound the
/// rabbit's horizontal travel against the terminal width (the rightmost column
/// it may start at is `width - running_rabbit_width()`).
pub fn running_rabbit_width() -> usize {
    RUNNER_RIGHT
        .iter()
        .chain(RUNNER_LEFT.iter())
        .map(|row| console::measure_text_width(row))
        .max()
        .unwrap_or(0)
}

/// A three-row running usagi at horizontal offset `col`, facing right
/// (`face_right`) or left, drawn mid-hop (`airborne`) or grounded. The two
/// content rows ride the top two rows of the block when airborne and the bottom
/// two when grounded, so toggling `airborne` between frames makes the rabbit
/// bound; advancing `col` carries it across the screen. Styled magenta-bold like
/// the mascot. Used by the startup [`splash`](super::super::tui::splash) screen,
/// which owns the motion (the bounce between the screen edges and the per-frame
/// hop) and calls this purely to draw a frame.
pub fn running_rabbit(col: usize, face_right: bool, airborne: bool) -> Vec<String> {
    let [ears, body] = if face_right {
        RUNNER_RIGHT
    } else {
        RUNNER_LEFT
    };
    let pad = " ".repeat(col);
    let ears = format!("{pad}{ears}");
    let body = format!("{pad}{body}");
    let rows = if airborne {
        [ears, body, String::new()]
    } else {
        [String::new(), ears, body]
    };
    rows.into_iter()
        .map(|row| style(row).magenta().bold().to_string())
        .collect()
}

/// One usagi "segment" of the multiplying conga line, as `(ears, face, feet)`.
/// Each row is exactly six display columns wide — using only width-1 glyphs (no
/// zero-width sound marks) — so the three rows tile into an aligned block no
/// matter how many rabbits line up.
const MULTIPLY_EARS: &str = " n_n  ";
const MULTIPLY_FACE: &str = "(｡･-･)";
const MULTIPLY_FEET: &str = " └┘   ";

/// A three-row line of `count` usagi standing shoulder to shoulder — the
/// "multiplying" rabbits. Each rabbit is a fixed-width segment, so the rows tile
/// into an aligned block; growing `count` between frames reads as the warren
/// filling up. The block is **anchored to the left edge**: the first rabbit
/// always holds column zero and each new one extends the line rightward, so the
/// rabbits already on screen never shift sideways as the warren grows (no layout
/// jump). Styled magenta-bold (the mascot's colour). A `count` of zero yields
/// three blank rows.
pub fn multiplying_rabbits(count: usize) -> Vec<String> {
    let rows = [
        MULTIPLY_EARS.repeat(count),
        MULTIPLY_FACE.repeat(count),
        MULTIPLY_FEET.repeat(count),
    ];
    rows.into_iter()
        .map(|row| style(row).magenta().bold().to_string())
        .collect()
}

/// Right-anchors each line of `banner` onto the `lines` starting at row `top`,
/// appending it after the existing content. A row is only overlaid when its
/// current content does not reach the banner's left column, so busy rows (a
/// session card, a live terminal) are never clobbered; the banner is skipped
/// entirely when it cannot fit the width.
///
/// Shared by the home screen's top-right notices and by
/// [`FramePainter`](super::screen::FramePainter), which overlays the global
/// background-install rabbit onto whatever screen is showing.
pub fn overlay_top_right(lines: &mut [String], top: usize, width: usize, banner: &[String]) {
    let block_w = banner
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w >= width {
        return;
    }
    let target_left = width - block_w;
    for (offset, segment) in banner.iter().enumerate() {
        let Some(base) = lines.get_mut(top + offset) else {
            break;
        };
        let base_w = console::measure_text_width(base);
        if base_w <= target_left {
            base.push_str(&" ".repeat(target_left - base_w));
            base.push_str(segment);
        }
    }
}

/// A centred, green-bold screen title.
pub fn title_line(width: usize, title: &str) -> String {
    style(centered(width, title)).green().bold().to_string()
}

/// xterm-256 green shades from dim to bright — the brightness ramp the splash
/// fades the title up through before it settles on the welcome screen's
/// green-bold title.
const TITLE_FADE: [u8; 4] = [22, 28, 34, 40];

/// The number of fade steps [`faded_title_line`] accepts: one per [`TITLE_FADE`]
/// shade plus the final green-bold step that matches [`title_line`].
pub const TITLE_FADE_STEPS: usize = TITLE_FADE.len() + 1;

/// A centred title faded to `step` of [`TITLE_FADE_STEPS`].
///
/// `step` 0 is a blank line (the title not shown yet), so a screen can reserve
/// the title's row before it appears without the layout shifting. Intermediate
/// steps ramp the green from dim to bright through [`TITLE_FADE`], and the final
/// step (and anything past it) is the canonical green-bold [`title_line`] — so
/// the splash can fade the title in and hand off to the welcome screen with no
/// visible jump.
pub fn faded_title_line(width: usize, title: &str, step: usize) -> String {
    if step == 0 {
        return String::new();
    }
    if step >= TITLE_FADE_STEPS {
        return title_line(width, title);
    }
    style(centered(width, title))
        .color256(TITLE_FADE[step - 1])
        .to_string()
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

/// An invisible marker [`block_caret`] embeds at the caret's column so the frame
/// painter can park the **real** terminal cursor there.
///
/// The block caret is only a recoloured cell — the hardware cursor stays hidden
/// and wherever the last write left it. An OS IME draws its in-progress
/// (preedit) text at the hardware cursor, so without parking it the composing
/// Japanese (or any IME) text surfaces at the bottom of the screen instead of in
/// the input field — and exactly where varies by terminal. The painter strips
/// this marker from every row before drawing and moves the cursor to the column
/// it marked (see [`FramePainter`](super::super::screen::FramePainter)).
///
/// It is a zero-width CSI sequence ([`console::measure_text_width`] and
/// [`clip_to_width`] both treat it as a no-op escape, so embedding it never
/// shifts the surrounding layout while the frame is assembled) that no styling
/// ever emits, so the painter can locate it unambiguously. The painter strips it
/// before writing; should one ever leak, `CSI 0 n` is an inert device-status
/// form a terminal ignores.
pub(crate) const CARET_MARK: &str = "\u{1b}[0n";

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
/// A zero-width [`CARET_MARK`] is embedded at the caret's column so the frame
/// painter can park the real terminal cursor there (the block caret alone leaves
/// the hardware cursor — and thus an IME's preedit text — misplaced).
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
        "{}{CARET_MARK}{}{}",
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
    fn farewell_lines_are_an_aligned_box_around_the_rabbit() {
        let lines = farewell_lines();
        // A top and bottom rule frame every art row.
        assert_eq!(lines.len(), FAREWELL_ART.len() + 2);
        // The parting words sit inside the box.
        assert!(lines.iter().any(|l| l.contains("またね、ぴょん！")));
        // Strip the ANSI colours to inspect the box's shape.
        let plain: Vec<String> = lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        // Top and bottom are rounded corners; every row between has side borders.
        assert!(plain[0].starts_with('╭') && plain[0].ends_with('╮'));
        assert!(plain.last().unwrap().starts_with('╰') && plain.last().unwrap().ends_with('╯'));
        assert!(plain[1..plain.len() - 1]
            .iter()
            .all(|l| l.starts_with('│') && l.ends_with('│')));
        // Every row is the same display width, so the right edge lines up.
        let width = console::measure_text_width(&plain[0]);
        assert!(plain
            .iter()
            .all(|l| console::measure_text_width(l) == width));
    }

    #[test]
    fn title_line_contains_the_title() {
        assert!(title_line(80, "USAGI").contains("USAGI"));
    }

    #[test]
    fn faded_title_line_is_blank_at_step_zero() {
        // Step 0 reserves the title's row without showing it, so a screen can
        // fade the title in later without the surrounding layout shifting.
        assert_eq!(faded_title_line(80, "USAGI", 0), "");
    }

    #[test]
    fn faded_title_line_shows_the_title_once_it_starts_fading() {
        // Every step past the delay carries the title text (the colour is what
        // ramps), and the same centring as the final title.
        for step in 1..=TITLE_FADE_STEPS {
            let line = faded_title_line(80, "USAGI", step);
            assert!(console::strip_ansi_codes(&line).contains("USAGI"));
        }
    }

    #[test]
    fn faded_title_line_settles_on_the_canonical_title() {
        // The final step (and anything past it) is exactly the green-bold
        // title_line, so the splash hands off to the welcome screen with no jump.
        assert_eq!(
            faded_title_line(80, "USAGI", TITLE_FADE_STEPS),
            title_line(80, "USAGI"),
        );
        assert_eq!(
            faded_title_line(80, "USAGI", TITLE_FADE_STEPS + 9),
            title_line(80, "USAGI"),
        );
    }

    #[test]
    fn spinner_char_wraps_the_braille_cycle() {
        assert_eq!(spinner_char(0), "⠋");
        // The frame index wraps around the ten-glyph cycle.
        assert_eq!(spinner_char(10), spinner_char(0));
        assert_eq!(spinner_char(11), spinner_char(1));
    }

    #[test]
    fn progress_bar_is_empty_with_nothing_to_scale_against() {
        assert_eq!(progress_bar(0, 0, 8), "");
        assert_eq!(progress_bar(1, 3, 0), "");
    }

    #[test]
    fn progress_bar_fills_none_some_and_all() {
        // No work done: an empty track.
        assert_eq!(progress_bar(0, 3, 8), "[        ]");
        // Everything done: a full track, no `>` head.
        assert_eq!(progress_bar(3, 3, 8), "[========]");
        // Past the total clamps to full rather than overflowing.
        assert_eq!(progress_bar(9, 3, 8), "[========]");
    }

    #[test]
    fn progress_bar_marks_a_partial_edge_with_a_head() {
        // 1/3 of 8 rounds to ~3 columns: two `=` then the `>` head, blanks after.
        assert_eq!(progress_bar(1, 3, 8), "[==>     ]");
        // 2/3 of 8 rounds to ~5 columns.
        assert_eq!(progress_bar(2, 3, 8), "[====>   ]");
        // Every partial bar is exactly `width` columns inside the brackets.
        for done in 1..4 {
            let bar = progress_bar(done, 4, 10);
            assert_eq!(console::measure_text_width(&bar), 12);
        }
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
    fn loading_rabbit_timed_carries_the_label_face_and_spinner() {
        let lines = loading_rabbit_timed(0, 0, "LLM 導入中…");
        assert_eq!(lines.len(), 2);
        let plain = console::strip_ansi_codes(&lines.join("\n")).into_owned();
        assert!(plain.contains("LLM 導入中…"));
        // The first face and the frame-0 braille spinner show.
        assert!(plain.contains("(･ㅅ･)"));
        assert!(plain.contains('⠋'));
    }

    #[test]
    fn loading_rabbit_timed_changes_face_with_the_face_index_alone() {
        // The expression advances on its own axis: holding the hop frame fixed
        // and bumping only the face index swaps the face — so the rabbit's mood
        // changes purely on the clock, independent of any progress.
        let a = console::strip_ansi_codes(&loading_rabbit_timed(0, 0, "x").join("\n")).into_owned();
        let b = console::strip_ansi_codes(&loading_rabbit_timed(0, 1, "x").join("\n")).into_owned();
        assert!(a.contains("(･ㅅ･)"));
        assert!(b.contains("(-ㅅ-)"));
    }

    #[test]
    fn loading_rabbit_timed_faces_wrap_and_cover_every_expression() {
        // Indexing wraps modulo the face set, and every face is reachable.
        for (i, face) in LOADING_FACES.iter().enumerate() {
            let plain =
                console::strip_ansi_codes(&loading_rabbit_timed(0, i, "x").join("\n")).into_owned();
            assert!(plain.contains(&format!("({face})")));
        }
        let wrapped = console::strip_ansi_codes(
            &loading_rabbit_timed(0, LOADING_FACES.len(), "x").join("\n"),
        )
        .into_owned();
        assert!(wrapped.contains(&format!("({})", LOADING_FACES[0])));
    }

    #[test]
    fn loading_rabbit_timed_rows_share_one_block_width() {
        let lines = loading_rabbit_timed(1, 2, "導入中…");
        assert_eq!(
            console::measure_text_width(&lines[0]),
            console::measure_text_width(&lines[1]),
        );
    }

    #[test]
    fn loading_rabbit_timed_keeps_the_ears_over_the_head_through_the_hop() {
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for hop in [0usize, 1] {
            let lines = loading_rabbit_timed(hop, 0, "x");
            assert_eq!(
                col_of(&lines[0], '∩'),
                col_of(&lines[1], 'ㅅ'),
                "ears must sit over the head on hop frame {hop}",
            );
        }
    }

    #[test]
    fn done_rabbit_shows_the_outcome_face_and_message() {
        let ok = console::strip_ansi_codes(&done_rabbit(true, "完了").join("\n")).into_owned();
        assert!(ok.contains("(^ㅅ^)"));
        assert!(ok.contains('✓'));
        assert!(ok.contains("完了"));

        let fail = console::strip_ansi_codes(&done_rabbit(false, "失敗").join("\n")).into_owned();
        assert!(fail.contains("(>ㅅ<)"));
        assert!(fail.contains('✗'));
        assert!(fail.contains("失敗"));
    }

    #[test]
    fn done_rabbit_rows_share_one_block_width() {
        let lines = done_rabbit(true, "qwen2.5:7b を導入しました");
        assert_eq!(
            console::measure_text_width(&lines[0]),
            console::measure_text_width(&lines[1]),
        );
    }

    #[test]
    fn running_rabbit_faces_its_direction_of_travel() {
        // Speed lines trail behind: on the left heading right, on the right
        // heading left. The face keeps its single `ㅅ` nose either way.
        let right =
            console::strip_ansi_codes(&running_rabbit(0, true, true).join("\n")).into_owned();
        assert!(right.contains("ﾐ(･ㅅ･)"));
        let left =
            console::strip_ansi_codes(&running_rabbit(0, false, true).join("\n")).into_owned();
        assert!(left.contains("(･ㅅ･)ﾐ"));
    }

    #[test]
    fn running_rabbit_is_three_rows_and_carries_the_offset() {
        // Always a three-row block; a larger `col` indents the art further so it
        // travels rightward across the screen.
        let near = running_rabbit(2, true, true);
        let far = running_rabbit(20, true, true);
        assert_eq!(near.len(), 3);
        assert_eq!(far.len(), 3);
        let lead = |line: &str| {
            console::strip_ansi_codes(line)
                .chars()
                .take_while(|c| *c == ' ')
                .count()
        };
        assert!(lead(&far[0]) > lead(&near[0]));
    }

    #[test]
    fn running_rabbit_bobs_between_the_top_and_bottom_rows() {
        // Airborne: the art rides the top two rows, leaving the last blank.
        // Grounded: it drops to the bottom two rows, leaving the first blank. So
        // toggling `airborne` between frames bounces the rabbit.
        let air = running_rabbit(0, true, true);
        assert!(console::strip_ansi_codes(&air[0]).contains('∩'));
        assert!(console::strip_ansi_codes(&air[2]).trim().is_empty());

        let ground = running_rabbit(0, true, false);
        assert!(console::strip_ansi_codes(&ground[0]).trim().is_empty());
        assert!(console::strip_ansi_codes(&ground[2]).contains('ㅅ'));
    }

    #[test]
    fn running_rabbit_keeps_the_ears_over_the_head_in_both_directions() {
        // The first ear must sit over the head centre (`ㅅ`) regardless of which
        // way the rabbit faces, so the ears never drift off the head.
        fn col_of(line: &str, target: char) -> usize {
            let plain = console::strip_ansi_codes(line).into_owned();
            let byte = plain.find(target).expect("glyph present");
            console::measure_text_width(&plain[..byte])
        }
        for face_right in [true, false] {
            let rows = running_rabbit(3, face_right, true);
            assert_eq!(
                col_of(&rows[0], '∩'),
                col_of(&rows[1], 'ㅅ'),
                "ears must sit over the head (face_right={face_right})",
            );
        }
    }

    #[test]
    fn running_rabbit_width_spans_the_widest_sprite_row() {
        // The bound a caller uses for the rabbit's travel matches the actual art:
        // the widest content row (`ﾐ(･ㅅ･)` / `(･ㅅ･)ﾐ`, seven columns).
        assert_eq!(running_rabbit_width(), 7);
    }

    #[test]
    fn multiplying_rabbits_lines_up_count_usagi() {
        // The face appears once per rabbit, so the warren grows with `count`.
        let plain = console::strip_ansi_codes(&multiplying_rabbits(3).join("\n")).into_owned();
        assert_eq!(plain.matches("(｡･-･)").count(), 3);
    }

    #[test]
    fn multiplying_rabbits_rows_stay_aligned_as_a_block() {
        // All three rows tile to the same width, so the ears/face/feet line up no
        // matter how many rabbits stand together.
        let lines = multiplying_rabbits(4);
        assert_eq!(lines.len(), 3);
        let w0 = console::measure_text_width(&lines[0]);
        assert!(lines.iter().all(|l| console::measure_text_width(l) == w0));
    }

    #[test]
    fn multiplying_rabbits_grow_wider_with_the_count() {
        // One more rabbit is one more fixed-width segment, so the block widens.
        let two = console::measure_text_width(&multiplying_rabbits(2)[1]);
        let five = console::measure_text_width(&multiplying_rabbits(5)[1]);
        assert!(five > two);
    }

    #[test]
    fn multiplying_rabbits_zero_count_is_blank() {
        // No rabbits yet: three empty rows (the animation starts from nothing).
        let lines = multiplying_rabbits(0);
        assert!(lines
            .iter()
            .all(|l| console::strip_ansi_codes(l).trim().is_empty()));
    }

    #[test]
    fn multiplying_rabbits_anchor_left_so_growth_never_shifts_them() {
        // The block is anchored to the left edge and a growing warren only appends
        // to the right: each row of the larger count starts with the row of the
        // smaller count, so the rabbits already on screen never jump sideways (no
        // layout shift). The first rabbit's face is flush left (column zero).
        let one = console::strip_ansi_codes(&multiplying_rabbits(1).join("\n")).into_owned();
        let three = console::strip_ansi_codes(&multiplying_rabbits(3).join("\n")).into_owned();
        for (small, big) in one.lines().zip(three.lines()) {
            assert!(big.starts_with(small), "growth must extend rightward only");
        }
        // The face row leads with the first rabbit's face, no centring padding.
        assert!(three.lines().nth(1).unwrap().starts_with("(｡･-･)"));
    }

    #[test]
    fn overlay_top_right_skips_a_row_whose_content_reaches_the_banner_column() {
        // The first line already fills the width, so the banner cannot be placed
        // on it; a later, empty line still receives its segment.
        let mut lines = vec!["X".repeat(100), String::new()];
        let banner = vec!["AB".to_string(), "CD".to_string()];
        overlay_top_right(&mut lines, 0, 100, &banner);
        assert_eq!(console::measure_text_width(&lines[0]), 100);
        assert!(lines[1].ends_with("CD"));
    }

    #[test]
    fn overlay_top_right_stops_when_the_banner_runs_past_the_last_row() {
        // The banner has more rows than remain from `top`, so placement stops at
        // the end of `lines` instead of panicking.
        let mut lines = vec![String::new()];
        let banner = vec!["AB".to_string(), "CD".to_string(), "EF".to_string()];
        overlay_top_right(&mut lines, 0, 100, &banner);
        assert!(lines[0].ends_with("AB"));
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn overlay_top_right_is_skipped_when_too_narrow_or_empty() {
        // A banner wider than the width is dropped rather than clobbering rows,
        // and an empty banner is a no-op.
        let mut lines = vec![String::new(), String::new()];
        overlay_top_right(&mut lines, 0, 3, &["ABCDE".to_string()]);
        overlay_top_right(&mut lines, 0, 80, &[]);
        assert!(lines.iter().all(|l| l.is_empty()));
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
    fn caret_mark_is_zero_width_and_survives_clipping() {
        // The marker must not shift layout while embedded in a frame: both the
        // width measurement that pads modal/box rows and the truncation primitive
        // treat it as a no-op escape.
        assert_eq!(console::measure_text_width(CARET_MARK), 0);
        let clipped = clip_to_width(&format!("ab{CARET_MARK}cd"), 10);
        assert_eq!(console::strip_ansi_codes(&clipped), "abcd");
        assert!(clipped.contains(CARET_MARK));
    }

    #[test]
    fn block_caret_embeds_the_caret_marker_for_cursor_parking() {
        // The painter locates the real cursor by this marker, placed at the caret
        // column; it carries no display width, so the visible text is unchanged.
        let base = Style::new().force_styling(true);
        let line = block_caret("あ", "い", &base);
        assert!(line.contains(CARET_MARK));
        assert_eq!(&*console::strip_ansi_codes(&line), "あい");
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
