//! Bounded SGR mouse recovery after crossterm splits off the leading Escape.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Parse a mouse tail without its Escape. `None` is a valid unfinished prefix;
/// an error tells the caller to replay the original keyboard events.
pub(super) fn parse_report(report: &str) -> Result<Option<MouseEvent>, ()> {
    if report == "[" {
        return Ok(None);
    }
    let body = report.strip_prefix("[<").ok_or(())?;
    let complete = body.ends_with(['M', 'm']);
    let parameters = if complete {
        &body[..body.len() - 1]
    } else {
        body
    };
    let mut values = [0_u16; 3];
    let mut count = 0;
    for (index, field) in parameters.split(';').enumerate() {
        if index >= values.len() || field.len() > 5 {
            return Err(());
        }
        count += 1;
        if field.is_empty() && !complete && parameters.ends_with(';') {
            // Only the last field may be empty in a valid partial report.
            if index + 1 == parameters.split(';').count() {
                return Ok(None);
            }
        }
        if field.is_empty() && !complete && parameters.is_empty() {
            return Ok(None);
        }
        if !field.bytes().all(|byte| byte.is_ascii_digit()) || field.is_empty() {
            return Err(());
        }
        values[index] = field.parse().map_err(|_| ())?;
    }
    let [button, column, row] = values;
    let button = u8::try_from(button).map_err(|_| ())?;
    if !complete {
        return Ok(None);
    }
    if count != 3 {
        return Err(());
    }
    Ok(Some(MouseEvent {
        kind: mouse_kind(button, body.ends_with('m'))?,
        column: column.checked_sub(1).ok_or(())?,
        row: row.checked_sub(1).ok_or(())?,
        modifiers: mouse_modifiers(button),
    }))
}

fn mouse_kind(code: u8, released: bool) -> Result<MouseEventKind, ()> {
    let button = match code & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        _ => MouseButton::Right,
    };
    // Remove the Shift/Alt/Control bits, retaining button and motion bits.
    Ok(match code & 0b1110_0011 {
        0..=2 if released => MouseEventKind::Up(button),
        0..=2 => MouseEventKind::Down(button),
        3 => MouseEventKind::Up(MouseButton::Left),
        32..=34 => MouseEventKind::Drag(button),
        35 | 96 | 97 => MouseEventKind::Moved,
        64 => MouseEventKind::ScrollUp,
        65 => MouseEventKind::ScrollDown,
        66 => MouseEventKind::ScrollLeft,
        67 => MouseEventKind::ScrollRight,
        _ => return Err(()),
    })
}

fn mouse_modifiers(code: u8) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (4, KeyModifiers::SHIFT),
        (8, KeyModifiers::ALT),
        (16, KeyModifiers::CONTROL),
    ] {
        if code & bit != 0 {
            modifiers |= modifier;
        }
    }
    modifiers
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;

    #[test]
    fn reports_preserve_buttons_motion_wheel_coordinates_and_modifiers() {
        let cases = [
            (0, 'M', MouseEventKind::Down(MouseButton::Left)),
            (1, 'M', MouseEventKind::Down(MouseButton::Middle)),
            (2, 'M', MouseEventKind::Down(MouseButton::Right)),
            (0, 'm', MouseEventKind::Up(MouseButton::Left)),
            (1, 'm', MouseEventKind::Up(MouseButton::Middle)),
            (2, 'm', MouseEventKind::Up(MouseButton::Right)),
            (3, 'M', MouseEventKind::Up(MouseButton::Left)),
            (32, 'M', MouseEventKind::Drag(MouseButton::Left)),
            (33, 'M', MouseEventKind::Drag(MouseButton::Middle)),
            (34, 'M', MouseEventKind::Drag(MouseButton::Right)),
            (35, 'M', MouseEventKind::Moved),
            (96, 'M', MouseEventKind::Moved),
            (97, 'M', MouseEventKind::Moved),
            (64, 'M', MouseEventKind::ScrollUp),
            (65, 'M', MouseEventKind::ScrollDown),
            (66, 'M', MouseEventKind::ScrollLeft),
            (67, 'M', MouseEventKind::ScrollRight),
        ];
        for (button, suffix, kind) in cases {
            for (modifier_bits, modifiers) in [
                (0, KeyModifiers::NONE),
                (4, KeyModifiers::SHIFT),
                (8, KeyModifiers::ALT),
                (16, KeyModifiers::CONTROL),
                (
                    28,
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ),
            ] {
                assert_eq!(
                    parse_report(&format!("[<{};42;65535{suffix}", button | modifier_bits)),
                    Ok(Some(MouseEvent {
                        kind,
                        column: 41,
                        row: 65534,
                        modifiers,
                    }))
                );
            }
        }
        assert_eq!(
            mouse_modifiers(28),
            KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL
        );
    }

    #[test]
    fn prefixes_wait_for_the_complete_report() {
        for report in ["[<64;42;13M", "[<00065;00001;00001m"] {
            for end in 1..report.len() {
                assert_eq!(parse_report(&report[..end]), Ok(None), "{report:?}");
            }
            assert!(parse_report(report).unwrap().is_some());
        }
    }

    #[test]
    fn invalid_or_unrelated_text_is_never_claimed_as_a_mouse_report() {
        for report in [
            "",
            "x",
            "[x",
            "[<x",
            "[<+64;1;1M",
            "[<;1;1M",
            "[<64;;1M",
            "[<64;;",
            "[<64;1;M",
            "[<64M",
            "[<64;1M",
            "[<64;1;1;2M",
            "[<64;1;1;",
            "[<256;1;1M",
            "[<65536;1;1M",
            "[<000064;1;1M",
            "[<64;65536;1M",
            "[<64;0;1M",
            "[<64;1;0M",
            "[<128;1;1M",
            "[<64;1;1z",
            "[<64;1;あM",
        ] {
            assert_eq!(parse_report(report), Err(()), "{report:?}");
        }
    }
}
