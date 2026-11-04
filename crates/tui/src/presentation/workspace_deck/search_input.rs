//! Shared query editing for the asynchronous search pickers.

use super::Key;
use crate::presentation::widgets::TextInput;

/// Apply bounded, single-line editing and report whether the query changed.
pub(super) fn edit(input: &mut TextInput, key: &Key, limit: usize) -> bool {
    let before = input.value().to_owned();
    match key {
        Key::Char(ch) if !ch.is_control() && input.value().chars().count() < limit => {
            input.insert(*ch);
        }
        Key::Paste(text) => {
            let remaining = limit.saturating_sub(input.value().chars().count());
            input.insert_str(
                &text
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(remaining)
                    .collect::<String>(),
            );
        }
        Key::Backspace => {
            input.backspace();
        }
        Key::Delete => {
            input.delete_forward();
        }
        Key::Left => input.move_left(),
        Key::Right => input.move_right(),
        Key::Home | Key::LineStart => input.move_home(),
        Key::End | Key::LineEnd => input.move_end(),
        _ => {}
    }
    before != input.value()
}
