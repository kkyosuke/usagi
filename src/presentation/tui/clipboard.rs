//! Copying text to the user's system clipboard from inside the TUI.
//!
//! The embedded terminal pane lets the user drag-select text (see
//! [`home::terminal_selection`](crate::presentation::tui::home::terminal_selection));
//! on release the selection is copied here. Rather than link a native clipboard
//! library, we emit an **OSC 52** escape sequence — `ESC ] 52 ; c ; <base64> BEL`
//! — which the terminal emulator interprets to set its clipboard. That works the
//! same locally and over SSH (the bytes travel down the same channel the TUI is
//! already drawing through), needs no extra dependency, and matches how the rest
//! of the TUI already speaks to the terminal in raw escapes.
//!
//! The trade-off is that the terminal must honour OSC 52 (iTerm2, kitty,
//! WezTerm, and `tmux`/`screen` with clipboard passthrough do; Apple Terminal.app
//! does not — but it also ignores mouse reporting, so drag-selection never starts
//! there anyway, and the user falls back to the terminal's own `Shift`+drag).
//!
//! Both pieces are pure string transforms, so they are unit-tested here; the
//! actual write to the terminal happens in the (coverage-excluded) terminal pane.

/// Build the OSC 52 escape sequence that asks the terminal to copy `text` to the
/// system clipboard (the `c` selection). Empty text yields an empty string, so
/// the caller can skip writing anything when there is nothing to copy.
pub fn osc52_copy(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// The standard Base64 alphabet (RFC 4648), indexed by 6-bit value.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` as standard (padded) Base64. OSC 52 payloads are Base64, and
/// this is the only place we need it, so a tiny encoder keeps the dependency
/// list unchanged.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // Pack up to three bytes into a 24-bit group, then read it back out as
        // four 6-bit indices; missing input bytes pad with `=`.
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let group = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[(group >> 18 & 0x3f) as usize] as char);
        out.push(BASE64[(group >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(group >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[(group & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_without_padding_when_aligned() {
        // "foo" is three bytes, so it encodes to exactly four chars, no padding.
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_pads_a_one_byte_remainder_with_two_equals() {
        // "f" leaves two bytes short of a group, so two `=` pads close it.
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_pads_a_two_byte_remainder_with_one_equals() {
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn base64_handles_empty_input() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_uses_the_full_alphabet_including_plus_and_slash() {
        // 0xFB 0xFF 0xFE exercises the high indices 62 (`+`) and 63 (`/`).
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn osc52_wraps_the_base64_payload_in_the_escape() {
        assert_eq!(osc52_copy("foo"), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn osc52_of_empty_text_is_empty() {
        // Nothing selected: emit nothing rather than an escape with no payload.
        assert_eq!(osc52_copy(""), "");
    }
}
