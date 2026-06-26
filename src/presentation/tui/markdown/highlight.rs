//! Syntax highlighting for fenced code blocks, layered on [`syntect`].
//!
//! [`render`](super::render) groups the body lines of a fenced block together
//! with the fence's info string (its language token) and hands them here.
//! [`highlight_block`] tokenises the block with `syntect` and returns one run of
//! [`Span`]s per source line, each span carrying the foreground [`Rgb`] of its
//! token. The result is still **pure data** — no terminal escapes — so the UI
//! layer (`panes`) maps the [`Rgb`] to a terminal colour when it draws.
//!
//! The bundled `syntect` syntax and theme sets are loaded once and cached. An
//! unknown or empty language token falls back to plain text (one span per line,
//! coloured by the theme's default foreground), so an unrecognised fence still
//! renders cleanly instead of failing.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::{Rgb, Span, SpanStyle};

/// The bundled syntaxes and the theme used to colour code blocks, loaded once.
fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        // A dark theme suits the terminal preview; the key is always present in
        // the bundled set, so fall back to a freshly-built default if not.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default();
        (syntaxes, theme)
    })
}

/// Pick the syntax for the fence's language token, falling back to plain text
/// when the token is empty or unrecognised.
fn syntax_for<'a>(syntaxes: &'a SyntaxSet, lang: &str) -> &'a SyntaxReference {
    syntaxes
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

/// Highlight the body `lines` of one fenced code block written in `lang` (the
/// fence info string; may be empty or unknown). Returns one [`Span`] run per
/// input line, preserving line count so the caller can emit one rendered line
/// per source line.
pub fn highlight_block(lines: &[&str], lang: &str) -> Vec<Vec<Span>> {
    let (syntaxes, theme) = assets();
    let mut highlighter = HighlightLines::new(syntax_for(syntaxes, lang), theme);

    lines
        .iter()
        .map(|line| {
            // `syntect` expects newline-terminated lines to drive multi-line
            // state (e.g. block comments). A highlighting error degrades to no
            // spans for that line rather than aborting the whole block.
            let with_nl = format!("{line}\n");
            highlighter
                .highlight_line(&with_nl, syntaxes)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(style, text)| {
                    let text = text.strip_suffix('\n').unwrap_or(text);
                    if text.is_empty() {
                        return None;
                    }
                    let fg = style.foreground;
                    let color = Rgb {
                        r: fg.r,
                        g: fg.g,
                        b: fg.b,
                    };
                    Some(Span::colored(text, SpanStyle::Code, color))
                })
                .collect()
        })
        .collect()
}
