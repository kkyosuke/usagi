use std::io;

use console::{Key, Term};

use crate::presentation::tui::screen::KeyReader;

/// Reads keys from a real terminal for the interactive screens.
///
/// This is a thin wrapper over `console::Term` whose terminal I/O can only be
/// exercised against a live terminal, so it is excluded from coverage; the
/// event loops are tested with scripted [`KeyReader`] stubs instead. The pure
/// mouse-filtering logic ([`next_non_mouse_key`]) is unit-tested below.
pub struct TermKeyReader {
    term: Term,
}

impl TermKeyReader {
    pub fn new(term: Term) -> Self {
        Self { term }
    }
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        // `read_key_raw` surfaces Ctrl+C as `Key::CtrlC` instead of raising
        // SIGINT, so the event loop can quit gracefully and the alternate
        // screen guard restores the terminal on the way out.
        //
        // The alternate screen guard turns on mouse reporting so the wheel
        // can't scroll the terminal (see `screen`), which means stray mouse
        // events now arrive on stdin. `console` does not understand them, so we
        // drain each report and hand back only the next real key.
        next_non_mouse_key(|| self.term.read_key_raw())
    }
}

/// The two mouse-report encodings a terminal may emit once reporting is on.
///
/// `console` does not recognise either, so it returns the head of the sequence
/// as a [`Key::UnknownEscSeq`] and leaves the remaining bytes to surface as
/// stray keys on subsequent reads. We classify the head here so the caller can
/// drain exactly the trailing bytes that belong to the report.
enum MouseSeq {
    /// SGR extended mode (DECSET 1006): `ESC [ < params... (M|m)`. `console`
    /// has consumed `[ < <first-digit>`; the rest runs through the `M`/`m`
    /// terminator.
    Sgr,
    /// Legacy X10 mode: `ESC [ M Cb Cx Cy` — always exactly three bytes after
    /// `M`. `console` has consumed `[ M Cb`; two coordinate bytes remain.
    X10,
}

/// Classify an escape sequence `console` could not decode as the head of a
/// mouse report, or `None` if it is some other unknown sequence we should pass
/// through untouched.
fn mouse_seq_kind(key: &Key) -> Option<MouseSeq> {
    let Key::UnknownEscSeq(seq) = key else {
        return None;
    };
    match seq.as_slice() {
        // `ESC [ <` only ever begins an SGR mouse report.
        ['[', '<', ..] => Some(MouseSeq::Sgr),
        // `ESC [ M` only ever begins an X10 mouse report.
        ['[', 'M', ..] => Some(MouseSeq::X10),
        _ => None,
    }
}

/// Read keys until one is not part of a mouse report, discarding any reports in
/// between. `read` yields successive keys from the terminal (each call reads the
/// next key, blocking for real input once the buffered report bytes are gone).
fn next_non_mouse_key(mut read: impl FnMut() -> io::Result<Key>) -> io::Result<Key> {
    loop {
        let key = read()?;
        match mouse_seq_kind(&key) {
            // SGR: swallow the remaining bytes through the `M`/`m` terminator.
            Some(MouseSeq::Sgr) => loop {
                if matches!(read()?, Key::Char('M') | Key::Char('m')) {
                    break;
                }
            },
            // X10: the report is a fixed length — drop the two coordinate bytes.
            Some(MouseSeq::X10) => {
                let _ = read()?;
                let _ = read()?;
            }
            // A real key (or an unrelated escape sequence): hand it back.
            None => return Ok(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Feed `next_non_mouse_key` a scripted run of keys.
    fn drive(keys: Vec<Key>) -> Key {
        let mut queue: VecDeque<Key> = keys.into();
        next_non_mouse_key(|| {
            queue
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "drained"))
        })
        .unwrap()
    }

    #[test]
    fn plain_key_passes_through() {
        assert_eq!(drive(vec![Key::Char('j')]), Key::Char('j'));
    }

    #[test]
    fn unrelated_escape_sequence_passes_through() {
        // Not a mouse report (e.g. an unmapped function key): leave it alone.
        let seq = Key::UnknownEscSeq(vec!['[', '3', '0']);
        assert_eq!(drive(vec![seq.clone()]), seq);
    }

    #[test]
    fn sgr_report_is_swallowed_then_next_key_returned() {
        // `ESC [ < 6 4 ; 1 0 ; 2 0 M` wheel-up, as console decodes it: the head
        // is an UnknownEscSeq and the tail leaks as individual chars up to `M`.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '6']),
            Key::Char('4'),
            Key::Char(';'),
            Key::Char('1'),
            Key::Char('0'),
            Key::Char(';'),
            Key::Char('2'),
            Key::Char('0'),
            Key::Char('M'),
            Key::Enter,
        ];
        assert_eq!(drive(keys), Key::Enter);
    }

    #[test]
    fn sgr_release_terminator_is_also_swallowed() {
        // SGR release reports end with a lowercase `m`.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '0']),
            Key::Char(';'),
            Key::Char('5'),
            Key::Char(';'),
            Key::Char('5'),
            Key::Char('m'),
            Key::Char('q'),
        ];
        assert_eq!(drive(keys), Key::Char('q'));
    }

    #[test]
    fn x10_report_drops_exactly_two_trailing_bytes() {
        // `ESC [ M Cb Cx Cy`: console yields the head plus two stray coordinate
        // chars, which must not reach the event loop.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', '`']),
            Key::Char('!'),
            Key::Char('"'),
            Key::ArrowDown,
        ];
        assert_eq!(drive(keys), Key::ArrowDown);
    }

    #[test]
    fn consecutive_reports_are_all_swallowed() {
        // A fast wheel spin produces several reports back to back.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', '<', '6']),
            Key::Char('5'),
            Key::Char('M'),
            Key::UnknownEscSeq(vec!['[', '<', '6']),
            Key::Char('5'),
            Key::Char('M'),
            Key::Char('k'),
        ];
        assert_eq!(drive(keys), Key::Char('k'));
    }

    #[test]
    fn read_error_propagates() {
        let mut queue: VecDeque<Key> = VecDeque::new();
        let err = next_non_mouse_key(|| queue.pop_front().ok_or_else(|| io::Error::other("boom")))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }
}
