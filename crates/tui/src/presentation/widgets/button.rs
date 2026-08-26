//! One-line buttons whose painted cells and clickable width come from one value.

use crate::presentation::theme::Style;

use super::{clip_to_width, display_width};

/// A compact inline button with one clickable cell of horizontal padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineButton<'a> {
    label: &'a str,
}

impl<'a> InlineButton<'a> {
    #[must_use]
    pub const fn new(label: &'a str) -> Self {
        Self { label }
    }

    /// Full display width, including the clickable padding on both sides.
    #[must_use]
    pub fn width(self) -> usize {
        display_width(self.label).saturating_add(2)
    }

    /// Paint the cells which form the button, clipped as one component.
    #[must_use]
    pub fn render(self, available: usize, style: Style) -> RenderedButton {
        let plain = clip_to_width(&format!(" {} ", self.label), available);
        let width = display_width(&plain);
        RenderedButton {
            line: style.paint(&plain),
            width,
        }
    }
}

/// Painted button material and its exact clickable span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedButton {
    pub line: String,
    pub width: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_button_keeps_both_padding_cells_inside_its_width() {
        let button = InlineButton::new("+ Open");
        let rendered = button.render(80, Style::new());
        assert_eq!(rendered.line, " + Open ");
        assert_eq!(button.width(), 8);
        assert_eq!(rendered.width, 8);
    }

    #[test]
    fn inline_button_clips_as_one_bounded_component() {
        let rendered = InlineButton::new("日本語").render(5, Style::new());
        assert!(display_width(&rendered.line) <= 5);
        assert_eq!(rendered.width, display_width(&rendered.line));
    }
}
