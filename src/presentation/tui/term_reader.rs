use std::io;
use std::time::Duration;

use console::{Key, Term};

use crate::presentation::tui::screen::{Input, KeyReader, ScrollEvent};

/// How many lines one wheel notch scrolls a pane.
const WHEEL_LINES: i32 = 3;

/// Reads input from a real terminal for the interactive screens.
///
/// This is a thin wrapper over `console::Term` whose terminal I/O can only be
/// exercised against a live terminal, so it is excluded from coverage; the
/// event loops are tested with scripted [`KeyReader`] stubs instead. The pure
/// mouse-parsing logic ([`next_input`]) is unit-tested below.
pub struct TermKeyReader {
    term: Term,
}

impl TermKeyReader {
    pub fn new(term: Term) -> Self {
        Self { term }
    }
}

impl KeyReader for TermKeyReader {
    fn read_input(&mut self) -> io::Result<Input> {
        // `read_key_raw` surfaces Ctrl+C as `Key::CtrlC` instead of raising
        // SIGINT, so the event loop can quit gracefully and the alternate
        // screen guard restores the terminal on the way out.
        //
        // The alternate screen guard turns on mouse reporting so the wheel
        // can't scroll the host terminal (see `screen`), which means mouse
        // events now arrive on stdin. `console` does not understand them, so we
        // parse each report here: a wheel turn becomes a [`ScrollEvent`] (the
        // screens that scroll a pane in place act on it), and every other report
        // is swallowed so it never leaks into the key stream.
        next_input(|| self.next_key())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        // The screens that do not scroll a pane just want the next key, so drop
        // any wheel turns along the way.
        key_from_inputs(|| self.read_input())
    }

    fn read_key_timeout(&mut self, timeout: Duration) -> io::Result<Option<Key>> {
        // `console` only offers a blocking read, so use crossterm's `poll` —
        // which the embedded terminal pane already relies on — to wait at most
        // `timeout` for input to be *ready* without consuming it. This adds no
        // background thread (one would outlive its screen and steal the next
        // screen's input, or fight the embedded pane for stdin), so reads stay
        // strictly sequential. When input is ready we decode it with `console`
        // exactly as a blocking read would; a wheel turn or other mouse report
        // drains to a tick (`None`) so the caller just repaints and polls again.
        //
        // The poll **must** run in raw mode. Between its per-key raw reads
        // `console` leaves the terminal in cooked (canonical) mode (the alternate
        // screen guard only clears `ECHO`; see [`super::echo`]). A canonical-mode
        // tty is reported readable only once the line discipline has a full,
        // newline-terminated line — so a lone arrow key / `j` / `k` never looks
        // "ready", and this poll would tick to `None` forever without ever seeing
        // it. That stranded every non-`Enter` key whenever the loop animates
        // (i.e. whenever a session is live — exactly the state `Ctrl-O` out of an
        // attached pane lands in: 切替 with the just-detached session still
        // running). Entering raw mode for the poll *and* the decode makes each
        // keypress deliverable at once, mirroring the per-read raw mode `console`
        // already uses on the blocking path; the guard restores cooked mode on the
        // way out so the rest of the loop is unchanged.
        let _raw = RawModeGuard::enter()?;
        if !crossterm::event::poll(timeout)? {
            return Ok(None);
        }
        match self.read_input()? {
            Input::Key(key) => Ok(Some(key)),
            Input::Scroll(_) => Ok(None),
        }
    }
}

/// Enables crossterm raw mode for as long as it is held, restoring the prior
/// (cooked) mode on drop. Used to bracket the timeout poll so the line discipline
/// delivers each keypress immediately instead of buffering it until a newline.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl TermKeyReader {
    /// Read the next key, dropping the spurious `Key::CtrlC` that a terminal
    /// resize produces.
    ///
    /// `read_key_raw` blocks in a `select`/`poll` that any delivered signal
    /// interrupts (EINTR). The embedded terminal pane installs crossterm's
    /// `SIGWINCH` handler, so every terminal resize now interrupts this read —
    /// and `console` maps that EINTR to `Key::CtrlC`. Left untouched it would
    /// read as a real `Ctrl-C` and close the app on every resize. When the
    /// terminal size changed across the read the `CtrlC` is a resize artefact,
    /// so keep reading instead of surfacing it; the loop then repaints at the
    /// new size on the next real key.
    fn next_key(&self) -> io::Result<Key> {
        loop {
            let before = self.term.size();
            let key = self.term.read_key_raw()?;
            if matches!(key, Key::CtrlC) && self.term.size() != before {
                continue;
            }
            return Ok(key);
        }
    }
}

/// Read inputs until one is a key, discarding any scrolls. Backs the key-only
/// reads of the screens that do not scroll a pane.
fn key_from_inputs(mut next: impl FnMut() -> io::Result<Input>) -> io::Result<Key> {
    loop {
        if let Input::Key(key) = next()? {
            return Ok(key);
        }
    }
}

/// The two mouse-report encodings a terminal may emit once reporting is on.
///
/// `console` does not recognise either, so it returns the head of the sequence
/// as a [`Key::UnknownEscSeq`] and leaves the remaining bytes to surface as
/// stray `Char` keys on subsequent reads. We classify the head here, keeping
/// whatever payload bytes `console` already consumed so the caller can resume
/// reading the rest of the report.
enum MouseSeq {
    /// SGR extended mode (DECSET 1006): `ESC [ < Cb ; Cx ; Cy (M|m)`. The
    /// numeric parameters run until the `M` (press) / `m` (release) terminator.
    Sgr,
    /// Legacy X10 mode: `ESC [ M Cb Cx Cy` — three bytes, each the value plus
    /// 32, after the `M`.
    X10,
}

/// Classify an escape sequence `console` could not decode as the head of a
/// mouse report, returning the report kind and any payload chars already in the
/// `UnknownEscSeq`. `None` for any other unknown sequence (passed through).
fn mouse_seq_kind(key: &Key) -> Option<(MouseSeq, Vec<char>)> {
    let Key::UnknownEscSeq(seq) = key else {
        return None;
    };
    match seq.as_slice() {
        // `ESC [ <` only ever begins an SGR mouse report.
        ['[', '<', rest @ ..] => Some((MouseSeq::Sgr, rest.to_vec())),
        // `ESC [ M` only ever begins an X10 mouse report.
        ['[', 'M', rest @ ..] => Some((MouseSeq::X10, rest.to_vec())),
        _ => None,
    }
}

/// Read the next input event: a key, or — for a wheel turn — a [`ScrollEvent`].
/// Mouse reports that are not wheel turns (clicks, motion) are swallowed so they
/// never reach the event loop. `read` yields successive keys from the terminal.
fn next_input(mut read: impl FnMut() -> io::Result<Key>) -> io::Result<Input> {
    loop {
        let key = read()?;
        let scroll = match mouse_seq_kind(&key) {
            Some((MouseSeq::Sgr, head)) => read_sgr(head, &mut read)?,
            Some((MouseSeq::X10, head)) => read_x10(head, &mut read)?,
            // A real key (or an unrelated escape sequence): hand it back.
            None => return Ok(Input::Key(key)),
        };
        // A wheel turn surfaces as a scroll; any other mouse report drains to
        // `None` and we loop for the next real input.
        if let Some(scroll) = scroll {
            return Ok(Input::Scroll(scroll));
        }
    }
}

/// Drain an SGR report (the parameters plus the `M`/`m` terminator), starting
/// from the `head` bytes `console` already consumed, and turn a wheel turn into
/// a [`ScrollEvent`]. Returns `None` for a non-wheel report or a malformed one.
fn read_sgr(
    head: Vec<char>,
    read: &mut impl FnMut() -> io::Result<Key>,
) -> io::Result<Option<ScrollEvent>> {
    let mut payload: String = head.into_iter().collect();
    let release = loop {
        match read()? {
            Key::Char('M') => break false,
            Key::Char('m') => break true,
            Key::Char(c) => payload.push(c),
            // An unexpected key mid-report: abandon it (swallowed).
            _ => return Ok(None),
        }
    };
    Ok(parse_sgr(&payload, release))
}

/// Parse the `Cb;Cx;Cy` parameters of an SGR report into a wheel [`ScrollEvent`].
fn parse_sgr(payload: &str, release: bool) -> Option<ScrollEvent> {
    // Wheel turns report only a press; ignore the release reports of clicks.
    if release {
        return None;
    }
    let mut params = payload.split(';');
    let cb: u32 = params.next()?.parse().ok()?;
    let cx: u32 = params.next()?.parse().ok()?;
    let cy: u32 = params.next()?.parse().ok()?;
    wheel_event(cb, cx, cy)
}

/// Drain an X10 report (three bytes, each value + 32), starting from the `head`
/// bytes `console` already consumed, and turn a wheel turn into a
/// [`ScrollEvent`]. Returns `None` for a non-wheel or malformed report.
///
/// Best-effort: we always request SGR coordinates (DECSET 1006), so a compliant
/// terminal never sends X10 and this path is effectively a fallback. It assumes
/// each coordinate byte is a single `char`; columns/rows past 95 (byte ≥ 128)
/// would mis-frame, but at worst the report is dropped — it cannot move a list.
fn read_x10(
    head: Vec<char>,
    read: &mut impl FnMut() -> io::Result<Key>,
) -> io::Result<Option<ScrollEvent>> {
    let mut bytes = head;
    while bytes.len() < 3 {
        match read()? {
            Key::Char(c) => bytes.push(c),
            _ => return Ok(None),
        }
    }
    // Every X10 value is offset by 32; the coordinates are otherwise the same
    // 1-based column/row as SGR.
    let cb = (bytes[0] as u32).saturating_sub(32);
    let cx = (bytes[1] as u32).saturating_sub(32);
    let cy = (bytes[2] as u32).saturating_sub(32);
    Ok(wheel_event(cb, cx, cy))
}

/// Build a [`ScrollEvent`] from a decoded button code and 1-based coordinates,
/// or `None` when the button is not a vertical wheel turn.
fn wheel_event(cb: u32, cx: u32, cy: u32) -> Option<ScrollEvent> {
    // Wheel turns set bit 6 (64); the low two bits pick the axis and direction:
    // 0 = up, 1 = down (2 / 3 are the horizontal wheel, which we ignore).
    if cb & 0x40 == 0 {
        return None;
    }
    let lines = match cb & 0b11 {
        0 => -WHEEL_LINES,
        1 => WHEEL_LINES,
        _ => return None,
    };
    Some(ScrollEvent {
        lines,
        col: cx.saturating_sub(1).min(u16::MAX as u32) as u16,
        row: cy.saturating_sub(1).min(u16::MAX as u32) as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Feed `next_input` a scripted run of keys.
    fn drive(keys: Vec<Key>) -> Input {
        let mut queue: VecDeque<Key> = keys.into();
        next_input(|| {
            queue
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "drained"))
        })
        .unwrap()
    }

    /// The SGR wheel report `ESC [ < cb ; cx ; cy M` as `console` decodes it: a
    /// `[< <first-digit>` head, then each remaining byte as a `Char`.
    fn sgr(cb: u32, cx: u32, cy: u32) -> Vec<Key> {
        let digits = format!("{cb};{cx};{cy}");
        let mut chars = digits.chars();
        let first = chars.next().unwrap();
        let mut keys = vec![Key::UnknownEscSeq(vec!['[', '<', first])];
        keys.extend(chars.map(Key::Char));
        keys.push(Key::Char('M'));
        keys
    }

    #[test]
    fn plain_key_passes_through() {
        assert_eq!(drive(vec![Key::Char('j')]), Input::Key(Key::Char('j')));
    }

    #[test]
    fn unrelated_escape_sequence_passes_through() {
        // Not a mouse report (e.g. an unmapped function key): leave it alone.
        let seq = Key::UnknownEscSeq(vec!['[', '3', '0']);
        assert_eq!(drive(vec![seq.clone()]), Input::Key(seq));
    }

    #[test]
    fn sgr_wheel_up_becomes_a_scroll_up_at_its_position() {
        // Wheel up (cb 64) at column 10, row 20 (both 1-based).
        let input = drive(sgr(64, 10, 20));
        assert_eq!(
            input,
            Input::Scroll(ScrollEvent {
                lines: -WHEEL_LINES,
                col: 9,
                row: 19,
            })
        );
    }

    #[test]
    fn sgr_wheel_down_becomes_a_scroll_down() {
        let input = drive(sgr(65, 5, 5));
        assert_eq!(
            input,
            Input::Scroll(ScrollEvent {
                lines: WHEEL_LINES,
                col: 4,
                row: 4,
            })
        );
    }

    #[test]
    fn sgr_non_wheel_report_is_swallowed_then_next_key_returned() {
        // A left-click press (cb 0) is not a wheel turn, so it is dropped and the
        // following key is returned.
        let mut keys = sgr(0, 1, 1);
        keys.push(Key::Enter);
        assert_eq!(drive(keys), Input::Key(Key::Enter));
    }

    #[test]
    fn sgr_horizontal_wheel_is_ignored() {
        // cb 66 / 67 are the horizontal wheel; we only scroll vertically.
        let mut keys = sgr(66, 1, 1);
        keys.push(Key::Char('q'));
        assert_eq!(drive(keys), Input::Key(Key::Char('q')));
    }

    #[test]
    fn sgr_release_report_is_swallowed() {
        // SGR release reports end with a lowercase `m`; never a wheel turn.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '0']),
            Key::Char(';'),
            Key::Char('5'),
            Key::Char(';'),
            Key::Char('5'),
            Key::Char('m'),
            Key::Char('q'),
        ];
        assert_eq!(drive(keys), Input::Key(Key::Char('q')));
    }

    #[test]
    fn sgr_unexpected_key_mid_report_is_abandoned() {
        // A non-Char interrupting the parameters bails out of the report; the
        // following key is returned.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '6']),
            Key::ArrowUp, // unexpected mid-report
            Key::Char('k'),
        ];
        assert_eq!(drive(keys), Input::Key(Key::Char('k')));
    }

    #[test]
    fn sgr_malformed_parameters_are_swallowed() {
        // Too few parameters: parsing fails and the report is dropped.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '6']),
            Key::Char('4'),
            Key::Char('M'),
            Key::Char('z'),
        ];
        assert_eq!(drive(keys), Input::Key(Key::Char('z')));
    }

    #[test]
    fn x10_wheel_up_becomes_a_scroll() {
        // `ESC [ M Cb Cx Cy`: wheel up is cb 64 (+32 = '`'); column/row 1 are
        // (1 + 32) = '!'. console yields the head plus two stray coordinate
        // chars.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', '`']),
            Key::Char('!'),
            Key::Char('!'),
        ];
        assert_eq!(
            drive(keys),
            Input::Scroll(ScrollEvent {
                lines: -WHEEL_LINES,
                col: 0,
                row: 0,
            })
        );
    }

    #[test]
    fn x10_non_wheel_report_drops_exactly_two_trailing_bytes() {
        // A non-wheel X10 report is swallowed, and its two coordinate chars must
        // not reach the event loop.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', ' ']), // cb 0 (space = 32)
            Key::Char('!'),
            Key::Char('"'),
            Key::ArrowDown,
        ];
        assert_eq!(drive(keys), Input::Key(Key::ArrowDown));
    }

    #[test]
    fn x10_unexpected_key_mid_report_is_abandoned() {
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', '`']),
            Key::Enter, // unexpected before the coordinates complete
            Key::Char('x'),
        ];
        assert_eq!(drive(keys), Input::Key(Key::Char('x')));
    }

    #[test]
    fn consecutive_wheel_reports_each_surface() {
        // A fast wheel spin produces several reports back to back; the first
        // turn is returned and the rest wait for the next read.
        let mut keys = sgr(64, 1, 1);
        keys.extend(sgr(64, 1, 1));
        assert!(matches!(drive(keys), Input::Scroll(_)));
    }

    #[test]
    fn read_error_propagates() {
        let mut queue: VecDeque<Key> = VecDeque::new();
        let err =
            next_input(|| queue.pop_front().ok_or_else(|| io::Error::other("boom"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn key_from_inputs_drops_scrolls_and_returns_the_next_key() {
        // The key-only reads of the non-scrolling screens skip a wheel turn and
        // return the following key.
        let mut inputs: VecDeque<Input> = VecDeque::from(vec![
            Input::Scroll(ScrollEvent {
                lines: -WHEEL_LINES,
                col: 1,
                row: 1,
            }),
            Input::Key(Key::Char('a')),
        ]);
        let key = key_from_inputs(|| {
            inputs
                .pop_front()
                .ok_or_else(|| io::Error::other("drained"))
        })
        .unwrap();
        assert_eq!(key, Key::Char('a'));
    }

    #[test]
    fn key_from_inputs_propagates_a_read_error() {
        let err = key_from_inputs(|| Err::<Input, _>(io::Error::other("boom"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }
}
