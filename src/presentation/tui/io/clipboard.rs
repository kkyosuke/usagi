//! Copying text to the user's system clipboard from inside the TUI.
//!
//! The embedded terminal pane lets the user drag-select text (see
//! [`home::terminal::selection`](crate::presentation::tui::home::terminal::selection));
//! on release (or `Ctrl-C`) the selection is copied here, by two complementary
//! routes:
//!
//! - **The local system clipboard**, by piping the text to the platform's
//!   clipboard tool ([`system_copy_commands`] → `pbcopy` / `wl-copy` / `xclip` /
//!   `clip`). This is what makes copy work on terminals that ignore OSC 52 —
//!   notably **Apple Terminal.app**, where it is the only route that copies.
//! - **An OSC 52 escape sequence** ([`osc52_copy`] → `ESC ] 52 ; c ; <base64> ST`),
//!   which a supporting terminal interprets to set *its* clipboard. This is what
//!   reaches the user's machine over SSH, where the local tool would only write
//!   the remote's clipboard. It needs no dependency and rides the channel the TUI
//!   already draws through.
//!
//! The escape is terminated with **ST** (`ESC \`) rather than BEL: a terminal
//! that doesn't implement OSC 52 (again, Terminal.app) silently discards an
//! ST-terminated string, whereas a trailing BEL rings the bell.
//!
//! The string transforms (the OSC sequence, the Base64, the command table) are
//! pure and unit-tested here; the terminal write and the process spawn happen in
//! the (coverage-excluded) terminal pane.

/// Build the OSC 52 escape sequence that asks the terminal to copy `text` to the
/// system clipboard (the `c` selection). Empty text yields an empty string, so
/// the caller can skip writing anything when there is nothing to copy.
///
/// The sequence ends with **ST** (`ESC \`), not BEL: terminals that don't
/// support OSC 52 (e.g. Apple Terminal.app) discard an ST-terminated string
/// quietly, while a BEL terminator makes them beep.
pub fn osc52_copy(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    format!("\x1b]52;c;{}\x1b\\", base64_encode(text.as_bytes()))
}

/// The candidate clipboard-write commands for the current platform, in
/// preference order; each is an argv whose **stdin** receives the text to copy.
/// The caller tries them in turn and stops at the first that runs. This is the
/// local-clipboard route used alongside [`osc52_copy`], and the only one that
/// copies on terminals that ignore OSC 52. An empty slice means the platform is
/// unrecognised and only the OSC 52 route is available.
pub fn system_copy_commands() -> &'static [&'static [&'static str]] {
    #[cfg(target_os = "macos")]
    {
        &[&["pbcopy"]]
    }
    #[cfg(target_os = "windows")]
    {
        &[&["clip"]]
    }
    // Linux / BSD: Wayland first, then the two common X11 tools.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        &[]
    }
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
    fn osc52_wraps_the_base64_payload_and_ends_with_st() {
        // The payload is Base64, and the sequence is terminated by ST (`ESC \`)
        // so terminals without OSC 52 stay quiet instead of beeping.
        assert_eq!(osc52_copy("foo"), "\x1b]52;c;Zm9v\x1b\\");
    }

    #[test]
    fn osc52_of_empty_text_is_empty() {
        // Nothing selected: emit nothing rather than an escape with no payload.
        assert_eq!(osc52_copy(""), "");
    }

    #[test]
    fn system_copy_commands_name_a_runnable_tool_for_this_platform() {
        // Every platform we build for offers at least one command, and each is
        // a non-empty argv (a binary, then its flags) the caller can spawn.
        let cmds = system_copy_commands();
        assert!(!cmds.is_empty());
        for argv in cmds {
            assert!(!argv.is_empty());
        }
        #[cfg(target_os = "macos")]
        assert_eq!(cmds, [["pbcopy"]]);
    }
}
