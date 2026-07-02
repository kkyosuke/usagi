//! usagi's terminal colour palette — the single place that owns the *set* of
//! colours the CLI and TUI paint with.
//!
//! Everything usagi draws styles its output through the [`console`] crate. Left
//! to itself that means raw colour calls (`.cyan()`, `.green()`, …) scattered
//! across every view, so retuning the look means hunting through a dozen files.
//! This module centralises the palette instead: views ask for a **semantic
//! role** (accent, success, danger, …) and this module decides which concrete
//! colour that role maps to. Change a mapping here and the whole UI follows.
//!
//! The roles are exposed as the [`Palette`] extension trait so call sites keep
//! `console`'s fluent chaining — `style(text).accent().bold()` — while the
//! colour choice lives in one place. Attributes that are *not* colours (`bold`,
//! `dim`, `italic`, `underlined`, `bright`, …) stay on `console`'s own methods.

use console::{Style, StyledObject};

/// Semantic colour roles mapped onto the ANSI palette.
///
/// Implemented for both [`console::Style`] (base styles built with
/// `Style::new()`) and [`console::StyledObject`] (the result of `style(value)`),
/// so either construction style can request a role the same way.
pub trait Palette: Sized {
    /// Primary accent: active/selected rows, headings, editable values, tabs.
    fn accent(self) -> Self;
    /// Positive/success: running agents, completed saves, "live" markers.
    fn success(self) -> Self;
    /// Error/danger: failures, destructive prompts, the input caret.
    fn danger(self) -> Self;
    /// Warning/attention: notices, waiting states, transient hints.
    fn warning(self) -> Self;
    /// Decorative accent: the mascot, playful highlights, secondary counts.
    fn feature(self) -> Self;
    /// Informational: brand-new items and hyperlinks.
    fn info(self) -> Self;
}

/// ANSI-256 index for the `info` role: a soft sky-blue that reads as a hyperlink
/// without the glare of the terminal's raw bright-blue. Chosen as the nearest
/// 256-cube colour to [`LINK_RGB`] (95,175,255 ≈ 102,178,255) so console-rendered
/// links (PR badges, `#123` popups, link spans, brand-new items) match the soft
/// blue the embedded terminal paints its hyperlinks with.
const INFO_256: u8 = 75;

impl Palette for Style {
    fn accent(self) -> Self {
        self.cyan()
    }
    fn success(self) -> Self {
        self.green()
    }
    fn danger(self) -> Self {
        self.red()
    }
    fn warning(self) -> Self {
        self.yellow()
    }
    fn feature(self) -> Self {
        self.magenta()
    }
    fn info(self) -> Self {
        self.color256(INFO_256)
    }
}

impl<D> Palette for StyledObject<D> {
    fn accent(self) -> Self {
        self.cyan()
    }
    fn success(self) -> Self {
        self.green()
    }
    fn danger(self) -> Self {
        self.red()
    }
    fn warning(self) -> Self {
        self.yellow()
    }
    fn feature(self) -> Self {
        self.magenta()
    }
    fn info(self) -> Self {
        self.color256(INFO_256)
    }
}

/// ANSI-256 green ramp used to fade the splash title in from dim to bright.
///
/// Each entry is a step; [`crate::presentation::tui::widgets`] adds one final
/// full-brightness step on top of these.
pub const TITLE_FADE: [u8; 4] = [22, 28, 34, 40];

/// Hyperlink colour (RGB) for text rendered inside the embedded terminal.
///
/// Kept as a plain tuple so it stays independent of the `vt100` colour type the
/// terminal renderer wraps it in. The console-side [`Palette::info`] role mirrors
/// this shade via its nearest 256-cube index ([`INFO_256`]) so links look the
/// same whether painted by `console` or the embedded terminal.
pub const LINK_RGB: (u8, u8, u8) = (102, 178, 255);

#[cfg(test)]
mod tests {
    use super::*;

    fn forced() -> Style {
        Style::new().force_styling(true)
    }

    #[test]
    fn roles_match_their_ansi_colours() {
        // The palette is a thin semantic alias over console's colours; each base
        // role must render byte-for-byte identically to the colour it stands for
        // (`info` is the exception, checked separately below).
        assert_eq!(
            forced().accent().apply_to("x").to_string(),
            forced().cyan().apply_to("x").to_string()
        );
        assert_eq!(
            forced().success().apply_to("x").to_string(),
            forced().green().apply_to("x").to_string()
        );
        assert_eq!(
            forced().danger().apply_to("x").to_string(),
            forced().red().apply_to("x").to_string()
        );
        assert_eq!(
            forced().warning().apply_to("x").to_string(),
            forced().yellow().apply_to("x").to_string()
        );
        assert_eq!(
            forced().feature().apply_to("x").to_string(),
            forced().magenta().apply_to("x").to_string()
        );
        // `info` is the one role that is not a plain ANSI alias: it maps to a
        // soft 256-cube sky-blue matching the embedded terminal's link colour.
        assert_eq!(
            forced().info().apply_to("x").to_string(),
            forced().color256(INFO_256).apply_to("x").to_string()
        );
    }

    #[test]
    fn styled_object_and_style_agree() {
        // Both entry points (`Style::new()` and `style(value)`) must resolve a
        // role to the same rendered output.
        let via_style = forced().accent().bold().apply_to("hi").to_string();
        let via_object = console::style("hi")
            .force_styling(true)
            .accent()
            .bold()
            .to_string();
        assert_eq!(via_style, via_object);
    }
}
