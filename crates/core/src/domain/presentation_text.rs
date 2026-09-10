//! Safety rules for text which can reach a terminal presentation surface.
//!
//! Terminal-facing labels must not contain control characters or Unicode bidi
//! controls.  Keeping the predicate in core gives every admission boundary and
//! the final renderer one shared definition instead of maintaining subtly
//! different deny lists.

/// Whether one Unicode scalar can be embedded in terminal presentation text
/// without changing terminal control flow or visual direction.
#[must_use]
pub fn presentation_character_is_safe(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{206f}'
        )
}

/// Whether all Unicode scalars in `value` are safe for terminal presentation.
#[must_use]
pub fn presentation_text_is_safe(value: &str) -> bool {
    value.chars().all(presentation_character_is_safe)
}

/// Normalize one untrusted text line for terminal presentation.
///
/// Tabs become one stable cell and unsafe controls become the replacement
/// character. Callers may apply this at multiple trust boundaries while still
/// sharing one policy implementation.
#[must_use]
pub fn sanitize_presentation_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if presentation_character_is_safe(character) {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        presentation_character_is_safe, presentation_text_is_safe, sanitize_presentation_line,
    };

    #[test]
    fn ordinary_unicode_is_safe() {
        assert!(presentation_text_is_safe("workspace 日本語"));
        assert!(presentation_character_is_safe('\u{301}'));
    }

    #[test]
    fn terminal_and_direction_controls_are_unsafe() {
        for value in [
            "line\nbreak",
            "tab\tstop",
            "bell\u{7}",
            "escape\u{1b}[31m",
            "spoof\u{202e}txt",
            "isolate\u{2066}name\u{2069}",
        ] {
            assert!(!presentation_text_is_safe(value), "{value:?}");
        }
    }

    #[test]
    fn line_sanitizer_keeps_safe_unicode_and_normalizes_unsafe_cells() {
        assert_eq!(
            sanitize_presentation_line("a\tb\n日本語\u{202e}"),
            "a b\u{fffd}日本語\u{fffd}"
        );
    }
}
