//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — screen titles, dimmed subtitles/footers, and the modal box that overlays a
//! screen — live here so every screen renders them consistently. The usagi
//! mascot artwork and its animated renderers live in the [`rabbit`] submodule
//! (re-exported here, so callers still reach them as `widgets::rabbit_lines`),
//! and other stateful, reusable widgets (e.g. the searchable [`picker`]) live in
//! their own submodules too.

pub mod dir_picker;
pub mod picker;
mod rabbit;
pub mod text_area;
pub mod text_input;

pub use rabbit::{
    done_rabbit, farewell_lines, loading_rabbit, loading_rabbit_timed, loading_screen,
    multiplying_rabbits, rabbit_height, rabbit_lines, rabbit_lines_at, rabbit_width,
    running_rabbit, running_rabbit_width, workspace_rabbit, workspace_rabbit_rail,
    workspace_rabbit_rail_width, workspace_rabbit_reaction, workspace_rabbit_speaking,
    workspace_rabbit_width, MascotReaction, RabbitMood,
};

use crate::presentation::theme::{self, Palette};
use console::{style, Style};
use unicode_width::UnicodeWidthChar;

use chrono::{DateTime, Utc};

/// The escape (ESC, `0x1b`) that introduces an ANSI control sequence.
const ESC: char = '\u{1b}';

/// The column width of the centred content block — form fields and project rows —
/// that the full-screen forms (config / new / open) render. The single source of
/// the block width so every screen's block stays the same size and lines up.
pub const BLOCK_WIDTH: usize = 52;

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
    clip_to_width_cow(text, max).into_owned()
}

/// Like [`clip_to_width`], but borrows `text` unchanged when it already fits
/// (the overwhelmingly common case in the per-frame render path), so a row that
/// needs no clipping costs no allocation. Callers that must own the result use
/// [`clip_to_width`]; callers that only `Display` it (e.g. `style(…)`) take the
/// [`Cow`](std::borrow::Cow) and avoid the copy.
///
/// When the cut carries over any SGR escape, the clipped text is closed with a
/// [`RESET`] so the colour (or a `reverse`/background span, e.g. the caret cell
/// [`block_caret`] draws) does not bleed into whatever is butted up after it —
/// the padding and right border [`boxed`] appends, or the next `markdown_row`.
/// This matches the sibling cuts [`slice_from_width`] / [`overlay_block`], which
/// already reset. Without it a full-width (CJK) line clipped mid-caret leaves the
/// box's padding and right `│` painted in reverse video, so the frame reads as
/// broken.
pub fn clip_to_width_cow(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if console::measure_text_width(text) <= max {
        return Cow::Borrowed(text);
    }
    if max == 0 {
        return Cow::Owned(String::new());
    }
    // Reserve one column for the ellipsis.
    let budget = max - 1;
    let mut out = String::with_capacity(text.len());
    let mut width = 0usize;
    // Whether any SGR escape was carried across the cut, so the tail must be
    // closed with a [`RESET`] lest its colour bleed past the clip.
    let mut carried_escape = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
            // Copy the whole escape sequence (zero display width) so the colour
            // it selects survives the clip. The styled lines clipped here carry
            // CSI/SGR sequences — `ESC [ … final` — so copy the `[` introducer
            // and parameter bytes through to (and including) the final byte
            // (`0x40..=0x7e`, excluding the `[` introducer itself).
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
    // Close any style the cut left open so it does not bleed into the padding,
    // border, or row butted up after the clip.
    if carried_escape {
        out.push_str(RESET);
    }
    Cow::Owned(out)
}

/// Breaks `text` into lines no wider than `width` display columns, splitting
/// between characters so CJK text (which carries no spaces to break on) still
/// wraps. Plain (ANSI-free) input is assumed — the caller styles the result.
///
/// A glyph wider than `width` on its own (e.g. a width-2 CJK char on a width-1
/// line) is placed alone and overflows by that much rather than being dropped, so
/// no character is ever lost. A `width` of 0, or empty `text`, yields no lines.
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

/// Formats how long ago `from` was, relative to `now`, as a compact label:
/// `just now`, `5min ago`, `3h ago`, `2d ago`, `3w ago`, falling back to an
/// absolute `YYYY-MM-DD` date once it is over a month old. A `from` in the
/// future (clock skew) reads as `just now`.
///
/// The single source of the "last used" phrasing, shared by the project
/// selection screen and the welcome screen's recent cards.
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

/// Centres a single line of `text` by left-padding it with spaces.
///
/// The width is measured in **display columns** ([`console::measure_text_width`]),
/// not `char` count, so CJK and other wide glyphs (the app's own Japanese titles
/// and footers) centre on their true rendered width instead of drifting left. The
/// text is first clipped to `width` so a label wider than the terminal (a long
/// footer on an 80-column screen) is truncated with an ellipsis rather than
/// overrunning the row and corrupting the frame.
fn centered(width: usize, text: &str) -> String {
    let text = clip_to_width_cow(text, width);
    let padding = " ".repeat(centered_padding(width, console::measure_text_width(&text)));
    format!("{padding}{text}")
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
    //
    // Compute in `u128` so pathological counts or terminal widths cannot
    // overflow before the divide. The value is bounded by `width`, so it fits
    // back in `usize`.
    let filled = ((done as u128 * width as u128 + total as u128 / 2) / total as u128) as usize;
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

/// Right-anchors each line of `banner` onto the `lines` starting at row `top`,
/// appending it after the existing content. A row is only overlaid when its
/// current content does not reach the banner's left column, so busy rows (a
/// session card, a live terminal) are never clobbered; the banner is skipped
/// entirely when it cannot fit the width.
///
/// Shared by the home screen's top-right notices and by
/// [`FramePainter`](super::io::screen::FramePainter), which overlays the global
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

/// The SGR reset (`ESC [ 0 m`) appended to a row cut mid-style so the colour the
/// cut left open does not bleed into whatever is butted up after it.
const RESET: &str = "\u{1b}[0m";

/// Copies the leading `max` display columns of `text`, ANSI escape sequences
/// (zero display width) carried through verbatim so the kept colours survive the
/// cut. Unlike [`clip_to_width`] it appends no ellipsis — the caller butts other
/// content flush against the cut (a floating box's left edge), where a trailing
/// `…` would be wrong. A double-width glyph that would straddle the boundary is
/// dropped whole rather than split.
fn truncate_to_width(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut width = 0usize;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
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
        if width + w > max {
            break;
        }
        width += w;
        out.push(ch);
    }
    out
}

/// Right-anchors each line of `block` onto `lines` from row `top` down, the
/// block's columns *replacing* whatever sits under them while the base content to
/// its left stays visible: each base row is cut to the columns left of the block,
/// padded out, then the block segment appended.
///
/// This is how the floating session-note box is composited over the right pane —
/// it must always show, over a sparse preview or a live terminal alike, so unlike
/// [`overlay_top_right`] (which yields to a row whose content already reaches the
/// block) it overwrites the block's columns unconditionally. A block exactly
/// `width` wide overlays the whole row (no base column survives); it is skipped
/// only when it cannot fit (`block_w > width`) or `width` is zero.
pub fn overlay_right(lines: &mut [String], top: usize, width: usize, block: &[String]) {
    let block_w = block
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w > width {
        return;
    }
    let target_left = width - block_w;
    for (offset, segment) in block.iter().enumerate() {
        let Some(base) = lines.get_mut(top + offset) else {
            break;
        };
        let kept = truncate_to_width(base, target_left);
        let kept_w = console::measure_text_width(&kept);
        let mut row = kept;
        if !row.is_empty() {
            // Close any SGR the cut left open so the block keeps its own colours.
            row.push_str(RESET);
        }
        row.push_str(&" ".repeat(target_left.saturating_sub(kept_w)));
        row.push_str(segment);
        *base = row;
    }
}

/// Returns the substring of `text` that begins at display column `at`, the
/// counterpart to [`truncate_to_width`] (which keeps the columns *before* it).
///
/// ANSI escape sequences (zero display width) before the split are collected and
/// prepended, so the slice keeps whatever colour was active there rather than
/// losing it with the dropped head; the slice is closed with a [`RESET`] so its
/// colour does not bleed past. A double-width glyph straddling the boundary is
/// dropped whole rather than split. The empty string is returned when nothing
/// remains past `at`.
///
/// Used by [`overlay_centered`] to keep the base columns to the *right* of a
/// floating box, the way `truncate_to_width` keeps those to its left.
fn slice_from_width(text: &str, at: usize) -> String {
    let mut width = 0usize;
    let mut prefix = String::new();
    let mut out = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == ESC {
            let mut seq = String::from(ch);
            for c in chars.by_ref() {
                seq.push(c);
                if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                    break;
                }
            }
            // Escapes before the split set the slice's opening colour; those
            // within it are carried through verbatim.
            if started {
                out.push_str(&seq);
            } else {
                prefix.push_str(&seq);
            }
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !started {
            if width >= at {
                started = true;
            } else if width + w > at {
                // A wide glyph straddling the boundary is dropped whole; the
                // slice resumes after it.
                width += w;
                started = true;
                continue;
            }
        }
        if started {
            out.push(ch);
        }
        width += w;
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("{prefix}{out}{RESET}")
    }
}

/// Composites the pre-[`boxed`] `block` centred over `base` — horizontally and
/// vertically — so the surrounding frame stays visible *around* the floating box
/// instead of a black backdrop. Each box row replaces only the columns it
/// spans; the base content to its left and right survives, and rows the box does
/// not cover are left untouched.
///
/// This is how the `:` command palette floats over the workspace: unlike
/// [`render_modal`] (which centres the box over an otherwise blank frame), the
/// panes behind it keep showing. `block`'s rows are assumed equal width (as
/// [`boxed`] produces). The block is skipped when it cannot fit (`block_w >
/// width`) or is empty.
pub fn overlay_centered(base: &mut [String], width: usize, block: &[String]) {
    let block_w = block
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w > width {
        return;
    }
    let left = centered_padding(width, block_w);
    // Centre vertically over the frame, the same maths [`render_modal`] uses.
    let top = base.len().saturating_sub(block.len()) / 2;
    overlay_block(base, top, left, block_w, block);
}

/// Composites `block` centred inside a rectangular display region of `base`.
///
/// `region_left` / `region_width` bound the columns, and `region_top` /
/// `region_height` bound the rows. The block is skipped when it cannot fit the
/// region or when the region sits off-screen. As with [`overlay_centered`], rows
/// are composited over the existing frame rather than blanking it, so the pane
/// behind stays visible around the floating indicator.
pub fn overlay_region_centered(
    base: &mut [String],
    width: usize,
    region_left: usize,
    region_width: usize,
    region_top: usize,
    region_height: usize,
    block: &[String],
) {
    let block_w = block
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    let block_h = block.len();
    if block_w == 0
        || block_h == 0
        || block_w > region_width
        || block_h > region_height
        || region_left >= width
        || region_top >= base.len()
    {
        return;
    }
    let left = region_left + centered_padding(region_width, block_w);
    if left + block_w > width {
        return;
    }
    let top = region_top + region_height.saturating_sub(block_h) / 2;
    if top >= base.len() {
        return;
    }
    overlay_block(base, top, left, block_w, block);
}

/// Composites the pre-sized `block` onto `base` with its top-left at `(top,
/// left)`, **clamped** so the whole block stays on screen: `left` is pulled back
/// so the block's right edge fits `width`, and `top` is pulled up so its bottom
/// fits `base`'s height. Each box row replaces only the columns it spans (the base
/// content to its left and right survives), the same compositing
/// [`overlay_centered`] does — but at an arbitrary anchor, for a floating tooltip
/// (the sidebar's PR hover popup) rather than a centred modal. The block is
/// skipped when empty or wider than the screen.
pub fn overlay_at(base: &mut [String], width: usize, top: usize, left: usize, block: &[String]) {
    let block_w = block
        .iter()
        .map(|line| console::measure_text_width(line))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w > width {
        return;
    }
    // Pull the anchor back so the block never spills past the right edge or below
    // the last row — a popup anchored near the bottom-right still shows in full.
    let left = left.min(width - block_w);
    let top = top.min(base.len().saturating_sub(block.len()));
    overlay_block(base, top, left, block_w, block);
}

/// Shared compositor for the floating overlays: writes each `block` row onto
/// `base` from `top` down, its left edge at column `left`, keeping the base
/// columns to the block's left and right. `block_w` is the block's display width
/// (its rows are assumed that wide, as [`boxed`] produces).
fn overlay_block(base: &mut [String], top: usize, left: usize, block_w: usize, block: &[String]) {
    let right_start = left + block_w;
    for (offset, segment) in block.iter().enumerate() {
        let Some(row) = base.get_mut(top + offset) else {
            break;
        };
        let kept_left = truncate_to_width(row, left);
        let kept_left_w = console::measure_text_width(&kept_left);
        let kept_right = slice_from_width(row, right_start);

        let mut composed = kept_left;
        if kept_left_w > 0 {
            // Close any SGR the left cut left open so the box keeps its colours.
            composed.push_str(RESET);
        }
        composed.push_str(&" ".repeat(left.saturating_sub(kept_left_w)));
        composed.push_str(segment);
        composed.push_str(&kept_right);
        *row = composed;
    }
}

/// A centred screen title in the palette's success colour.
pub fn title_line(width: usize, title: &str) -> String {
    style(centered(width, title)).success().bold().to_string()
}

/// The number of fade steps [`faded_title_line`] accepts: one per
/// [`theme::TITLE_FADE`] shade plus the final success-bold step that matches
/// [`title_line`]. The ramp itself lives in the central [`theme`] palette.
pub const TITLE_FADE_STEPS: usize = theme::TITLE_FADE.len() + 1;

/// A centred title faded to `step` of [`TITLE_FADE_STEPS`].
///
/// `step` 0 is a blank line (the title not shown yet), so a screen can reserve
/// the title's row before it appears without the layout shifting. Intermediate
/// steps ramp the title from dim to bright through [`theme::TITLE_FADE`], and
/// the final step (and anything past it) is the canonical success-bold
/// [`title_line`] — so
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
        .color256(theme::TITLE_FADE[step - 1])
        .to_string()
}

/// A centred, dimmed line — used for subtitles and footers.
pub fn dim_line(width: usize, text: &str) -> String {
    style(centered(width, text)).dim().to_string()
}

/// The centred mascot + title (+ optional subtitle) block every full-screen form
/// (welcome / config / new / open) puts at the top.
///
/// [`rabbit_lines`] draws the mascot, a blank row separates it from the `title`,
/// and `subtitle` — when present — is added as a [`dim_line`] below it. Vertical
/// placement is the caller's `render_frame`'s job, so this adds no leading
/// padding. The single source of the header layout so the screens stay aligned.
pub fn header_lines(width: usize, title: &str, subtitle: Option<&str>) -> Vec<String> {
    let mut lines = rabbit_lines(width);
    lines.push(String::new());
    lines.push(title_line(width, title));
    if let Some(subtitle) = subtitle {
        lines.push(dim_line(width, subtitle));
    }
    lines
}

/// The `>` cursor a selectable row shows when `selected` (red-bold), or a single
/// blank space when not — so the marker column stays aligned whether or not the
/// row is the cursor. The single source of the selection-cursor glyph and its
/// styling, shared by every screen's row / menu / button rendering.
pub fn cursor_marker(selected: bool) -> String {
    if selected {
        style(">").danger().bold().to_string()
    } else {
        " ".to_string()
    }
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
            styled.warning().bold()
        } else if focused {
            styled.accent().bold()
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
/// it marked (see [`FramePainter`](super::super::io::screen::FramePainter)).
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

/// Renders one editor line that a selection runs through: the bytes in
/// `[start, end)` are drawn in **reverse video** (the selection highlight) and
/// the rest in `base`. When the caret sits on this line, `caret` is its byte
/// column — always `start` or `end`, the selection's moving edge — and a
/// zero-width [`CARET_MARK`] is embedded there so the frame painter parks the
/// real cursor on it, exactly as [`block_caret`] does. `newline_selected`
/// appends a reversed space, showing that the line break after this line is
/// itself inside the selection (so a multi-line span reads as one block).
///
/// `start` and `end` must be `char` boundaries within `line` (the caret columns
/// always are). Like [`block_caret`], the recoloured cells never shift the text
/// sideways, so the highlight tracks the selection without reflowing the line.
pub fn block_selection(
    line: &str,
    start: usize,
    end: usize,
    caret: Option<usize>,
    newline_selected: bool,
    base: &Style,
) -> String {
    let before = &line[..start];
    let mid = &line[start..end];
    let after = &line[end..];
    // The caret marks at most one edge: its column is `start` or `end`, so place
    // a single [`CARET_MARK`] there (never both, even for an empty span).
    let (mark_start, mark_end) = match caret {
        Some(c) if c == start => (CARET_MARK, ""),
        Some(c) if c == end => ("", CARET_MARK),
        _ => ("", ""),
    };
    let trailing = if newline_selected {
        base.clone().reverse().apply_to(" ").to_string()
    } else {
        String::new()
    };
    format!(
        "{}{mark_start}{}{mark_end}{}{trailing}",
        base.apply_to(before),
        base.clone().reverse().apply_to(mid),
        base.apply_to(after),
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
    // A plain frame: `Style::new()` carries no attributes, so `apply_to` emits no
    // ANSI and the output is byte-for-byte the unstyled box.
    boxed_styled(title, inner_width, lines, &Style::new())
}

/// Like [`boxed`] but paints the frame — the border glyphs and the embedded
/// title — in `border`. The content lines keep whatever styling they already
/// carry; only the box outline is recoloured. This lets a box signal state
/// (e.g. an open editor) by its frame colour without touching the text inside.
pub fn boxed_styled(
    title: &str,
    inner_width: usize,
    lines: &[String],
    border: &Style,
) -> Vec<String> {
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
    // The side border is one styled glyph reused on both edges of every row.
    let edge = border.apply_to("│").to_string();

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(border.apply_to(top).to_string());
    for line in lines {
        // Clip first so a line wider than the box can never push the right
        // border out; then pad short lines so every row is exactly `inner_width`.
        let line = clip_to_width(line, inner_width);
        let pad = inner_width.saturating_sub(console::measure_text_width(&line));
        out.push(format!("{edge} {line}{} {edge}", " ".repeat(pad)));
    }
    out.push(border.apply_to(bottom).to_string());
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
    // Clamp to the terminal height: when the box is taller than a very short
    // terminal `top_padding` collapses to 0 and the box rows alone exceed
    // `height`, so trim the overflow rather than letting the diff painter address
    // rows past the bottom (which the terminal clamps onto the last line, garbling
    // it). The box loses its lower rows on a cramped screen, but never overruns.
    lines.truncate(height);
    lines
}

/// The inner (content) width a modal box gets for terminal `width`: the `desired`
/// width clamped so the box — `desired` plus the two borders and a space of
/// padding on each side — never overruns the screen. Callers compute the body
/// with this width so its lines match the box [`overlay_modal`] / [`render_modal`]
/// draw around them.
pub fn modal_inner_width(width: usize, desired: usize) -> usize {
    desired.min(width.saturating_sub(4))
}

/// Columns the large modal keeps blank on each side of the box, and rows it
/// keeps above and below it, so the terminal-filling box still floats clear of
/// the screen edges instead of butting against them.
const LARGE_MODAL_MARGIN_X: usize = 2;
const LARGE_MODAL_MARGIN_Y: usize = 1;

/// The widest the large modal's content grows, regardless of terminal width, so
/// the short help lines stay readable instead of stretching across a very wide
/// screen. Public so content built for this modal (the `issue gantt` chart) can
/// size its line budget to the same cap.
pub const LARGE_MODAL_MAX_INNER: usize = 100;

/// The geometry of the large, terminal-filling text modal (the `man` help): the
/// inner content width of the box and how many body window lines are visible at
/// once, both scaled to the terminal and inset so the box keeps a margin from
/// the screen edges.
///
/// Returned together so the renderer (which windows the body and wraps it in a
/// box) and the event loop (whose paging step scrolls by a screenful) size the
/// modal from one source — they never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargeModalGeometry {
    /// Inner content width of the box, before its borders and the space of
    /// padding [`boxed`] adds on each side.
    pub inner_width: usize,
    /// Body window lines shown at once — the scroll window height the renderer
    /// and the paging step share.
    pub visible: usize,
}

/// Computes the [`LargeModalGeometry`] for a terminal of `height`×`width`.
///
/// The width reserves the box overhead (two borders + a space of padding on each
/// side = 4 columns) plus a side margin, then caps at [`LARGE_MODAL_MAX_INNER`].
/// The visible-line budget is the height minus the top/bottom margins, the two
/// borders, the two scroll-hint rows (one above, one below the window) and the
/// two-row dismiss footer that the body always carries — so the whole box,
/// hints and footer included, always fits the screen. Both are floored at 1 so a
/// tiny terminal still yields a drawable (if cramped) box.
pub fn large_modal_geometry(height: usize, width: usize) -> LargeModalGeometry {
    let inner_width = width
        .saturating_sub(4 + LARGE_MODAL_MARGIN_X * 2)
        .clamp(1, LARGE_MODAL_MAX_INNER);
    let visible = height
        .saturating_sub(LARGE_MODAL_MARGIN_Y * 2 + 2 + 2 + 2)
        .max(1);
    LargeModalGeometry {
        inner_width,
        visible,
    }
}

/// Composites a titled modal box centred over `base`, the floating sibling of
/// [`render_modal`]: it wraps `body` in a [`boxed`] frame and overlays it with
/// [`overlay_centered`], so the screen behind it stays visible instead of a black
/// backdrop. The shared path for floating modals (the `:` command palette, the
/// text modal). `inner_width` is clamped the same way [`modal_inner_width`] does,
/// so passing a body built with that width lines the rows up inside the box.
pub fn overlay_modal(
    base: &mut [String],
    width: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) {
    let inner = modal_inner_width(width, inner_width);
    overlay_centered(base, width, &boxed(title, inner, body));
}

/// Composites a titled modal box centred inside a rectangular **region** of
/// `base`, the region-scoped sibling of [`overlay_modal`]: it wraps `body` in a
/// [`boxed`] frame and overlays it with [`overlay_region_centered`], so the box
/// floats within one pane (e.g. the right pane) while the columns outside the
/// region — the sidebar — stay visible around it.
///
/// `region_left` / `region_width` bound the columns and `region_top` /
/// `region_height` the rows the box is centred within. `inner_width` is clamped
/// to the region (not the whole screen), so a body built with
/// [`modal_inner_width`]`(region_width, inner_width)` lines its rows up inside the
/// box. The box is skipped when it cannot fit the region (see
/// [`overlay_region_centered`]).
#[allow(clippy::too_many_arguments)]
pub fn overlay_region_modal(
    base: &mut [String],
    width: usize,
    region_left: usize,
    region_width: usize,
    region_top: usize,
    region_height: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) {
    let inner = modal_inner_width(region_width, inner_width);
    overlay_region_centered(
        base,
        width,
        region_left,
        region_width,
        region_top,
        region_height,
        &boxed(title, inner, body),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_to_width_breaks_cjk_text_between_characters() {
        // No spaces to break on: the line splits between glyphs, each wrapped line
        // staying within the width, and no character lost.
        let lines = wrap_to_width("アップデートがあるぴょん", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| console::measure_text_width(l) <= 12));
        assert_eq!(lines.concat(), "アップデートがあるぴょん");
    }

    #[test]
    fn wrap_to_width_keeps_short_text_on_one_line() {
        assert_eq!(wrap_to_width("v0.2.0", 12), vec!["v0.2.0".to_string()]);
    }

    #[test]
    fn wrap_to_width_yields_nothing_for_zero_width_or_empty() {
        assert!(wrap_to_width("text", 0).is_empty());
        assert!(wrap_to_width("", 8).is_empty());
    }

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
    fn large_modal_geometry_scales_with_the_terminal() {
        // A roomy terminal: the box reserves its overhead (4) plus a side margin
        // (2 each side) horizontally, and the borders/hints/footer/margin
        // vertically — so it grows with the screen but never touches the edges.
        let geo = large_modal_geometry(40, 90);
        assert_eq!(geo.inner_width, 90 - 8);
        assert_eq!(geo.visible, 40 - 8);
    }

    #[test]
    fn large_modal_geometry_caps_width_and_floors_at_one() {
        // Very wide: the content width is capped so the short help stays readable.
        assert_eq!(
            large_modal_geometry(60, 400).inner_width,
            LARGE_MODAL_MAX_INNER
        );
        // Degenerate sizes never yield a zero-dimension box.
        let tiny = large_modal_geometry(1, 1);
        assert_eq!(tiny.inner_width, 1);
        assert_eq!(tiny.visible, 1);
    }

    #[test]
    fn normalize_size_keeps_nonzero_values() {
        assert_eq!(normalize_size(30, 100), (30, 100));
    }

    #[test]
    fn title_line_contains_the_title() {
        assert!(title_line(80, "USAGI").contains("USAGI"));
    }

    #[test]
    fn centered_clips_a_line_wider_than_the_terminal() {
        // A footer/title longer than the terminal must be truncated (with an
        // ellipsis), never overrun the row — the diff painter would otherwise
        // wrap or corrupt the line. Measured by display width, ANSI stripped.
        let long = "a / b / c / d / e / f / g / h / i / j / k / l / m / n / o / p";
        let line = dim_line(20, long);
        assert!(console::measure_text_width(&line) <= 20);
        assert!(line.contains('…'));
    }

    #[test]
    fn centered_centres_cjk_by_display_width_not_char_count() {
        // Each CJK char is two display columns, so a 4-char title is 8 columns
        // wide and its left padding is (20 - 8) / 2 = 6 — not (20 - 4) / 2 = 8,
        // which a char-count centring would (mis)compute.
        let line = title_line(20, "セッション一覧"); // 7 wide chars = 14 cols
        let lead = line.chars().take_while(|c| *c == ' ').count();
        assert_eq!(lead, centered_padding(20, 14));
    }

    #[test]
    fn render_modal_clamps_to_a_short_terminal_height() {
        // A box taller than a very short terminal must not return more rows than
        // the screen has, or the painter addresses lines past the bottom.
        let frame = render_modal(
            3,
            40,
            "Confirm",
            30,
            &[
                "line a".to_string(),
                "line b".to_string(),
                "line c".to_string(),
            ],
        );
        assert_eq!(frame.len(), 3);
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
        // The final step (and anything past it) is exactly the success-bold
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
    fn progress_bar_math_is_overflow_safe_for_huge_counts() {
        // Counts near `usize::MAX` must not overflow the internal
        // `done * width + total / 2` before the divide. Half done rounds to half
        // the bar, matching ordinary-sized inputs.
        assert_eq!(
            progress_bar((usize::MAX - 1) / 2, usize::MAX - 1, 8),
            "[===>    ]"
        );
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
    fn overlay_right_keeps_left_content_and_replaces_the_right_columns() {
        // The block is anchored to the right; the base row keeps the columns left
        // of it (cut to fit), padded out so the block lands flush right.
        let mut lines = vec!["session alpha is live and busy".to_string()];
        overlay_right(&mut lines, 0, 30, &["[note]".to_string()]);
        let row = console::strip_ansi_codes(&lines[0]);
        assert_eq!(
            console::measure_text_width(&row),
            30,
            "the row fills the width"
        );
        assert!(
            row.starts_with("session alpha"),
            "the left content survives"
        );
        assert!(row.ends_with("[note]"), "the block lands flush right");
    }

    #[test]
    fn overlay_right_overwrites_the_full_row_for_a_block_as_wide_as_the_pane() {
        // A block exactly the pane width leaves no left column: it replaces the row.
        let mut lines = vec!["busy preview line".to_string()];
        overlay_right(&mut lines, 0, 6, &["[note]".to_string()]);
        assert_eq!(console::strip_ansi_codes(&lines[0]), "[note]");
    }

    #[test]
    fn overlay_right_overlays_an_empty_base_row_without_a_stray_reset() {
        // An empty base row pads straight to the block (no SGR to close), so the
        // row is just spaces then the block.
        let mut lines = vec![String::new()];
        overlay_right(&mut lines, 0, 10, &["abc".to_string()]);
        assert_eq!(lines[0], format!("{}abc", " ".repeat(7)));
    }

    #[test]
    fn overlay_right_is_skipped_when_too_wide_or_empty_and_stops_past_the_last_row() {
        // A block wider than the pane is dropped, an empty block is a no-op, and a
        // block taller than what remains stops at the end instead of panicking.
        let mut lines = vec!["keep".to_string(), String::new()];
        overlay_right(&mut lines, 0, 3, &["ABCDE".to_string()]);
        overlay_right(&mut lines, 0, 80, &[]);
        assert_eq!(lines[0], "keep");
        assert!(lines[1].is_empty());
        overlay_right(&mut lines, 1, 80, &["A".to_string(), "B".to_string()]);
        assert!(lines[1].ends_with('A'));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn slice_from_width_keeps_the_tail_with_its_colour() {
        // Plain slice: the columns before `at` are dropped, the rest kept.
        assert_eq!(
            console::strip_ansi_codes(&slice_from_width("abcdef", 2)).into_owned(),
            "cdef",
        );
        // Nothing remains past the end → the empty string.
        assert_eq!(slice_from_width("abc", 5), "");
        // A double-width glyph straddling the split is dropped whole, not halved.
        assert_eq!(
            console::strip_ansi_codes(&slice_from_width("あい", 1)).into_owned(),
            "い",
        );
        // The colour active before the split is reapplied so the tail keeps it.
        let styled = "\u{1b}[31mabcd\u{1b}[0m";
        let tail = slice_from_width(styled, 2);
        assert_eq!(console::strip_ansi_codes(&tail).into_owned(), "cd");
        assert!(
            tail.contains("\u{1b}[31m"),
            "the colour before the split is carried onto the tail",
        );
    }

    #[test]
    fn overlay_centered_floats_a_box_keeping_the_surrounding_content() {
        // The box is centred over the base; on the rows it spans the base columns
        // to its left and right survive, and the rows it does not span are left
        // untouched.
        let mut base = vec!["L".repeat(20); 5];
        let block = vec!["┌──┐".to_string(), "│xy│".to_string(), "└──┘".to_string()];
        // block_w = 4, left = (20-4)/2 = 8; top = (5-3)/2 = 1, so rows 1..4 carry
        // the box and rows 0 and 4 are untouched.
        overlay_centered(&mut base, 20, &block);
        assert_eq!(
            base[0],
            "L".repeat(20),
            "the row above the box is untouched"
        );
        assert_eq!(
            base[4],
            "L".repeat(20),
            "the row below the box is untouched"
        );
        for row in &base[1..4] {
            let plain = console::strip_ansi_codes(row);
            assert_eq!(
                console::measure_text_width(&plain),
                20,
                "the row stays full width: {plain}",
            );
            assert!(plain.starts_with("LLLLLLLL"), "left content survives");
            assert!(plain.ends_with("LLLLLLLL"), "right content survives");
        }
        assert!(console::strip_ansi_codes(&base[2]).contains("│xy│"));
    }

    #[test]
    fn overlay_centered_pads_an_empty_base_row_up_to_the_box() {
        // A blank base row has no left content to keep, so it pads straight to the
        // box's left edge (no stray SGR reset) — covering the empty-left branch.
        let mut base = vec![String::new()];
        overlay_centered(&mut base, 10, &["XX".to_string()]);
        // block_w = 2, left = (10-2)/2 = 4: four spaces then the box segment.
        assert_eq!(base[0], format!("{}XX", " ".repeat(4)));
    }

    #[test]
    fn overlay_centered_is_skipped_when_too_wide_or_empty_and_stops_past_the_last_row() {
        // A block wider than the width is dropped, an empty block is a no-op, and a
        // block taller than what remains stops at the end instead of panicking.
        let mut base = vec!["keep".to_string()];
        overlay_centered(&mut base, 3, &["WIDE".to_string()]);
        overlay_centered(&mut base, 80, &[]);
        assert_eq!(base[0], "keep");
        // Three box rows over a single base row: row 0 is overlaid, the rest stop
        // at the end of the base.
        overlay_centered(
            &mut base,
            80,
            &["A".to_string(), "B".to_string(), "C".to_string()],
        );
        assert!(console::strip_ansi_codes(&base[0]).contains('A'));
        assert_eq!(base.len(), 1);
    }

    #[test]
    fn overlay_region_centered_floats_inside_the_given_rectangle() {
        // A 2×2 block centred inside the 10-column region starting at col 5 and
        // the 4-row region starting at row 1 lands at col 9, row 2.
        let mut base = vec![".".repeat(20); 6];
        overlay_region_centered(
            &mut base,
            20,
            5,
            10,
            1,
            4,
            &["XX".to_string(), "YY".to_string()],
        );
        assert_eq!(base[0], ".".repeat(20));
        assert_eq!(base[1], ".".repeat(20));
        assert_eq!(base[4], ".".repeat(20));
        assert_eq!(base[5], ".".repeat(20));

        let first = console::strip_ansi_codes(&base[2]).into_owned();
        let second = console::strip_ansi_codes(&base[3]).into_owned();
        assert_eq!(&first[9..11], "XX");
        assert_eq!(&second[9..11], "YY");
    }

    #[test]
    fn overlay_region_modal_boxes_the_body_and_floats_it_within_the_region() {
        // A titled box floats centred inside the right-hand region (cols 10..30 of
        // a 30-wide frame), leaving the columns to its left — the "sidebar" — intact.
        let mut base = vec![".".repeat(30); 8];
        overlay_region_modal(
            &mut base,
            30,
            10, // region_left
            20, // region_width
            1,  // region_top
            6,  // region_height
            "T",
            8, // inner_width
            &["hi".to_string()],
        );
        let joined = console::strip_ansi_codes(&base.join("\n")).into_owned();
        // The body and the box frame (with its embedded title) both landed.
        assert!(joined.contains("hi"));
        assert!(joined.contains("┌─ T"));
        assert!(joined.contains('└'));
        // Every row keeps its leading sidebar columns (cols 0..10) untouched.
        for row in &base {
            let plain = console::strip_ansi_codes(row).into_owned();
            assert_eq!(&plain[..10], "..........");
        }
    }

    #[test]
    fn overlay_region_modal_clamps_the_inner_width_to_the_region() {
        // A desired inner width wider than the region is clamped so the box still
        // fits inside it (rather than being skipped for overrunning).
        let mut base = vec![" ".repeat(30); 6];
        overlay_region_modal(&mut base, 30, 10, 16, 0, 6, "", 999, &["x".to_string()]);
        let joined = console::strip_ansi_codes(&base.join("\n")).into_owned();
        assert!(joined.contains('┌'));
        assert!(joined.contains('x'));
    }

    #[test]
    fn overlay_region_centered_is_skipped_when_it_cannot_fit_the_region() {
        let mut base = vec!["keep".to_string(); 2];
        overlay_region_centered(&mut base, 20, 5, 1, 0, 2, &["XX".to_string()]);
        overlay_region_centered(
            &mut base,
            20,
            5,
            10,
            0,
            1,
            &["X".to_string(), "Y".to_string()],
        );
        overlay_region_centered(&mut base, 20, 20, 10, 0, 2, &["X".to_string()]);
        assert_eq!(base, vec!["keep".to_string(); 2]);
    }

    #[test]
    fn overlay_region_centered_is_skipped_when_centering_would_leave_the_frame() {
        let mut base = vec!["keep".to_string(); 2];
        // The region itself may extend beyond the frame even though its left/top
        // edge is visible. In that case the centred block would spill, so it is
        // skipped instead of clamped into a misleading position.
        overlay_region_centered(&mut base, 10, 8, 10, 0, 1, &["XXXXXX".to_string()]);
        overlay_region_centered(&mut base, 10, 0, 1, 1, 4, &["X".to_string()]);
        assert_eq!(base, vec!["keep".to_string(); 2]);
    }

    #[test]
    fn overlay_at_anchors_a_box_at_the_given_cell_keeping_the_surrounding_content() {
        // The box is placed with its top-left at (1, 8): rows 1..4 carry it, and on
        // those rows the base columns to its left and right survive.
        let mut base = vec!["L".repeat(20); 5];
        let block = vec!["┌──┐".to_string(), "│xy│".to_string(), "└──┘".to_string()];
        overlay_at(&mut base, 20, 1, 8, &block);
        assert_eq!(
            base[0],
            "L".repeat(20),
            "the row above the box is untouched"
        );
        assert_eq!(
            base[4],
            "L".repeat(20),
            "the row below the box is untouched"
        );
        let mid = console::strip_ansi_codes(&base[2]);
        assert!(
            mid.starts_with("LLLLLLLL"),
            "the eight columns left of the box survive"
        );
        assert!(mid.contains("│xy│"));
        assert_eq!(
            console::measure_text_width(&mid),
            20,
            "the row stays full width"
        );
    }

    #[test]
    fn overlay_at_clamps_the_anchor_so_the_box_stays_on_screen() {
        // An anchor past the right / bottom edge is pulled back so the whole box
        // still shows: width 10, a 4-wide box asked for at column 9 lands at 6
        // (10 - 4); three box rows asked for at row 4 of a 5-row base land at row 2.
        let mut base = vec!["..........".to_string(); 5];
        let block = vec!["[xx]".to_string(), "[yy]".to_string(), "[zz]".to_string()];
        overlay_at(&mut base, 10, 4, 9, &block);
        // Rows 2..5 carry the box; row 1 is untouched.
        assert_eq!(base[1], "..........");
        assert!(console::strip_ansi_codes(&base[2]).ends_with("[xx]"));
        assert!(console::strip_ansi_codes(&base[4]).ends_with("[zz]"));
    }

    #[test]
    fn overlay_at_is_skipped_when_empty_or_wider_than_the_screen() {
        let mut base = vec!["keep".to_string()];
        overlay_at(&mut base, 3, 0, 0, &["WIDE".to_string()]);
        overlay_at(&mut base, 80, 0, 0, &[]);
        assert_eq!(base[0], "keep");
    }

    #[test]
    fn truncate_to_width_keeps_colours_and_drops_a_straddling_wide_glyph() {
        // ANSI escapes are zero-width and carried through; a double-width glyph
        // that would overflow the budget is dropped whole rather than split.
        // (An explicit SGR sequence, since `console` suppresses colour off a TTY.)
        let styled = "\u{1b}[31mab\u{1b}[0m";
        let kept = truncate_to_width(styled, 2);
        assert_eq!(console::strip_ansi_codes(&kept), "ab");
        assert!(
            kept.contains("\u{1b}[31m"),
            "the colour escape is carried through"
        );
        // "あ" is two columns wide: it fits a budget of 2 but not of 1.
        assert_eq!(truncate_to_width("あい", 2), "あ");
        assert_eq!(truncate_to_width("あい", 1), "");
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
    fn clip_to_width_closes_a_style_the_cut_left_open() {
        // A styled line cut mid-span must be closed with a RESET so its colour —
        // or a reverse/background span — does not bleed into what is butted up
        // after it (the padding and right border `boxed` appends).
        let styled = Style::new()
            .force_styling(true)
            .red()
            .apply_to("hello world")
            .to_string();
        let clipped = clip_to_width(&styled, 6);
        assert!(clipped.ends_with(RESET), "clip must reset: {clipped:?}");
        // Plain (unstyled) text carries no escape, so no stray reset is appended.
        let plain = clip_to_width("hello world", 6);
        assert!(!plain.contains(RESET));
        assert!(plain.ends_with('…'));
    }

    #[test]
    fn boxed_does_not_bleed_a_clipped_caret_span_onto_its_border() {
        // A full-width (CJK) input with a reverse-video caret, clipped to the box:
        // the reverse must not carry past the clip onto the padding and right `│`,
        // so once `boxed` closes the row the border reads as a plain frame.
        let base = Style::new().force_styling(true).accent();
        let line = block_caret("あいうえおかきくけこ", "さしすせそ", &base);
        let boxed = boxed("t", 12, &[line]);
        // Every row stays the same display width — the clip never shifts the frame.
        let width = console::measure_text_width(&boxed[0]);
        for row in &boxed {
            assert_eq!(console::measure_text_width(row), width, "row: {row:?}");
        }
        // The content row's tail (padding + right border) carries no open SGR: the
        // last escape in it is a reset, not the reverse/colour the caret opened.
        let content = &boxed[1];
        let last_esc = content.rfind(ESC).expect("row carries styling");
        assert!(
            content[last_esc..].starts_with(RESET),
            "border tail must be reset, not bled style: {content:?}",
        );
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
    fn block_selection_reverses_the_span_and_parks_the_caret_at_its_edge() {
        let base = Style::new().force_styling(true);
        // "ab[cd]" with the caret at the selection's right edge (end == 4).
        let line = block_selection("abcd", 2, 4, Some(4), false, &base);
        assert_eq!(&*console::strip_ansi_codes(&line), "abcd");
        let reversed = base.clone().reverse().apply_to("cd").to_string();
        assert!(line.contains(&reversed), "the span is reversed");
        assert!(line.contains(CARET_MARK), "the caret edge is marked");
        // No trailing cell when the line break is not part of the selection.
        assert_eq!(console::measure_text_width(&line), 4);
    }

    #[test]
    fn block_selection_marks_the_left_edge_and_shows_a_selected_newline() {
        let base = Style::new().force_styling(true);
        // Whole line selected with the caret at the left edge (start == 0) and the
        // trailing line break included: a reversed space stands in for the newline.
        let line = block_selection("ab", 0, 2, Some(0), true, &base);
        assert_eq!(&*console::strip_ansi_codes(&line), "ab ");
        assert!(line.contains(CARET_MARK));
        let reversed_space = base.clone().reverse().apply_to(" ").to_string();
        assert!(
            line.contains(&reversed_space),
            "the newline reads as selected"
        );
    }

    #[test]
    fn block_selection_without_the_caret_on_the_line_omits_the_marker() {
        let base = Style::new().force_styling(true);
        // A line fully inside a multi-line selection but not holding the caret.
        let line = block_selection("mid", 0, 3, None, true, &base);
        assert_eq!(&*console::strip_ansi_codes(&line), "mid ");
        assert!(!line.contains(CARET_MARK));
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
    fn boxed_styled_paints_the_frame_but_not_the_content() {
        let border = Style::new().force_styling(true).cyan();
        let lines = boxed_styled("Title", 10, &["hi".to_string()], &border);
        // The border rows carry the colour; strip it and the plain box remains.
        assert!(lines[0].contains("\u{1b}["), "the top border is coloured");
        assert!(
            lines.last().unwrap().contains("\u{1b}["),
            "the bottom border is coloured"
        );
        assert!(console::strip_ansi_codes(&lines[0]).starts_with('┌'));
        // The side glyphs are coloured but the content text between them is not.
        assert!(lines[1].starts_with("\u{1b}["), "the left edge is coloured");
        let plain = console::strip_ansi_codes(&lines[1]);
        assert!(plain.starts_with("│ hi"), "content follows the left edge");
        assert!(plain.ends_with("│"), "content is closed by the right edge");
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

    #[test]
    fn modal_inner_width_clamps_to_fit_the_borders_and_padding() {
        // A roomy terminal keeps the desired width …
        assert_eq!(modal_inner_width(80, 60), 60);
        // … a narrow one clamps so the box (inner + 4) never overruns.
        assert_eq!(modal_inner_width(20, 60), 16);
    }

    #[test]
    fn overlay_modal_floats_a_titled_box_over_the_base_keeping_content() {
        // Unlike `render_modal`, the box is composited over the live frame, so the
        // surrounding rows stay visible instead of a black backdrop.
        let mut base = vec!["abcdefghijklmnopqrstuvwxyz".to_string(); 10];
        overlay_modal(&mut base, 26, "Help", 10, &["row".to_string()]);
        let joined = base.join("\n");
        // The titled box and its body are drawn …
        assert!(joined.contains("Help"));
        assert!(joined.contains("row"));
        assert!(joined.contains('┌'));
        // … and a row outside the box still carries its original content (the
        // frame shows around the float, not blanked).
        assert!(base[0].contains('a') || base.last().unwrap().contains('a'));
    }
}
