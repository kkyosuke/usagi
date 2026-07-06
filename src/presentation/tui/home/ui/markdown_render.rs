//! Rendering for Markdown preview rows in the right pane.

use super::clip_to_width;
use crate::presentation::theme::Palette;
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Rgb, Span, SpanStyle};
use console::style;

pub(super) fn markdown_row(line: &MarkdownLine, width: usize) -> String {
    let mut out = String::new();
    if !line.prefix.is_empty() {
        let prefix = match line.style {
            LineStyle::Bullet | LineStyle::Number => style(&line.prefix).accent().to_string(),
            LineStyle::Quote => style(&line.prefix).dim().to_string(),
            _ => line.prefix.clone(),
        };
        out.push_str(&prefix);
    }
    for span in &line.spans {
        out.push_str(&styled_span(span, line.style));
    }
    clip_to_width(&out, width)
}

/// Style one inline [`Span`] for terminal display. A heading colours its whole
/// content by level; a code-block line and a quote line take a uniform style;
/// every other line styles each span by its own inline emphasis.
pub(super) fn styled_span(span: &Span, line_style: LineStyle) -> String {
    let text = span.text.as_str();
    match line_style {
        LineStyle::Heading(level) => heading_style(text, level),
        // Syntax-highlighted code carries a per-token colour; an uncoloured code
        // span (unknown highlight) falls back to the palette's success colour.
        LineStyle::Code => match span.color {
            Some(rgb) => style(text).color256(rgb_to_ansi256(rgb)).to_string(),
            None => style(text).success().to_string(),
        },
        LineStyle::Quote => style(text).dim().italic().to_string(),
        _ => match span.style {
            SpanStyle::Plain => text.to_string(),
            SpanStyle::Strong => style(text).bold().to_string(),
            SpanStyle::Emphasis => style(text).italic().to_string(),
            SpanStyle::Code => style(text).success().to_string(),
            SpanStyle::Link => style(text).info().underlined().to_string(),
        },
    }
}

/// The bold, level-coloured styling of a heading's text: magenta (h1), cyan (h2),
/// yellow (h3), and plain bold for deeper levels.
pub(super) fn heading_style(text: &str, level: u8) -> String {
    let base = style(text).bold();
    match level {
        1 => base.feature(),
        2 => base.accent(),
        3 => base.warning(),
        _ => base,
    }
    .to_string()
}

/// Map a 24-bit colour to the nearest xterm-256 palette index — the broadest
/// colour depth the `console` styling exposes. Near-grey colours snap to the
/// 24-step greyscale ramp (232–255); everything else snaps to the 6×6×6 colour
/// cube (16–231). Both choose the closest step per channel.
pub(super) fn rgb_to_ansi256(rgb: Rgb) -> u8 {
    let Rgb { r, g, b } = rgb;
    // Treat colours whose channels are within a small spread as grey so subtle
    // foregrounds use the finer-grained ramp instead of the coarse cube.
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min <= 8 {
        // The ramp runs grey 8..=238 in steps of 10 across indices 232..=255.
        let level = ((r as u16 + g as u16 + b as u16) / 3).saturating_sub(8) / 10;
        let level = level.min(23) as u8;
        return 232 + level;
    }
    let cube = |c: u8| -> u8 {
        // Cube steps sit at 0, 95, 135, 175, 215, 255; pick the nearest.
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0u8;
        let mut best_dist = u16::MAX;
        for (idx, &step) in STEPS.iter().enumerate() {
            let dist = (c as i16 - step as i16).unsigned_abs();
            if dist < best_dist {
                best_dist = dist;
                best = idx as u8;
            }
        }
        best
    };
    16 + 36 * cube(r) + 6 * cube(g) + cube(b)
}
