use std::io;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use console::{Key, Term};

use crate::presentation::tui::io::screen::{ClickEvent, Input, KeyReader, ScrollEvent};

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

    fn read_input_timeout(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        // `console` only offers a blocking read, so we wait at most `timeout` for
        // input to be *ready* and then decode it with `console` exactly as a
        // blocking read would. This adds no background thread (one would outlive its
        // screen and steal the next screen's input, or fight the embedded pane for
        // stdin), so reads stay strictly sequential.
        //
        // The readiness wait **must not consume** the bytes it is waiting on:
        // `console`'s decode below reads the tty fd directly, so anything read out
        // from under it never reaches the key stream. An earlier version polled
        // with `crossterm::event::poll`, which reads tty bytes into crossterm's
        // *own* internal parse buffer to detect an event — bytes `console` then
        // never saw. Each keypress in the animate path was swallowed by crossterm
        // and only surfaced when the *next* press happened to wake a blocking
        // `console` read, a one-key lag that froze the task spinner, delayed
        // applying finished work, and made `c` need two presses in 切替. We instead
        // poll the fd directly ([`input_ready`]) — readiness only, never a read —
        // so every key is decoded by `console` on the first press.
        //
        // The wait **must** run in raw mode. Between its per-key raw reads
        // `console` leaves the terminal in cooked (canonical) mode (the alternate
        // screen guard only clears `ECHO`; see [`super::echo`]). A canonical-mode
        // tty is reported readable only once the line discipline has a full,
        // newline-terminated line — so a lone arrow key / `j` / `k` never looks
        // "ready", and this would tick to `None` forever without ever seeing it.
        // That stranded every non-`Enter` key whenever the loop animates (i.e.
        // whenever a session is live — exactly the state `Ctrl-O` out of an
        // attached pane lands in: 切替 with the just-detached session still
        // running). Entering raw mode for the wait *and* the decode makes each
        // keypress deliverable at once, mirroring the per-read raw mode `console`
        // already uses on the blocking path; the guard restores cooked mode on the
        // way out so the rest of the loop is unchanged.
        let _raw = RawModeGuard::enter()?;
        if !input_ready(timeout)? {
            return Ok(None);
        }
        Ok(Some(self.read_input()?))
    }

    fn read_key_timeout(&mut self, timeout: Duration) -> io::Result<Option<Key>> {
        // The key-only screens want the next key within the timeout; a wheel turn
        // or click drains to a tick (`None`) so the caller just repaints and polls
        // again. The home loop instead reads through `read_input_timeout` so a
        // click can reach it.
        Ok(match self.read_input_timeout(timeout)? {
            Some(Input::Key(key)) => Some(key),
            _ => None,
        })
    }
}

/// Whether the terminal input has a byte ready within `timeout`, **without
/// consuming it** — so the `console` decode that follows reads those same bytes.
///
/// Mirrors `console`'s own readiness check ([`unbuffered`-then-`select`/`poll`]):
/// the fd is stdin when it is a tty, else `/dev/tty`; on a macOS tty it is
/// `select`ed (a macOS tty cannot be `poll`ed), and `poll`ed everywhere else.
#[cfg(unix)]
fn input_ready(timeout: Duration) -> io::Result<bool> {
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let stdin = io::stdin();
    if unsafe { libc::isatty(stdin.as_raw_fd()) == 1 } {
        return wait_readable(stdin.as_raw_fd(), millis);
    }
    // stdin is redirected: `console` reads keys from `/dev/tty`, so wait on that.
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    wait_readable(tty.as_raw_fd(), millis)
}

/// Off Unix there is no raw fd to wait on without consuming bytes, so defer to
/// crossterm's cross-platform readiness poll. The byte-stealing concern that
/// rules `crossterm::event::poll` out on Unix does not apply here: `console`
/// reads keys through the same crossterm/Windows console layer, not a separate
/// tty fd.
#[cfg(not(unix))]
fn input_ready(timeout: Duration) -> io::Result<bool> {
    crossterm::event::poll(timeout)
}

/// Wait up to `timeout_ms` for `fd` to be readable, using `select` on a macOS
/// tty (which cannot be `poll`ed there) and `poll` otherwise. Reports readiness
/// only; it never reads, so the bytes stay queued for the decoding read.
#[cfg(unix)]
fn wait_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::isatty(fd) == 1 } {
            return select_readable(fd, timeout_ms);
        }
    }
    poll_readable(fd, timeout_ms)
}

/// `poll(2)` `fd` for `POLLIN` up to `timeout_ms` (negative blocks). Readiness
/// only — no bytes are read.
#[cfg(unix)]
fn poll_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pollfd as *mut _, 1, timeout_ms) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        // A delivered signal (e.g. crossterm's SIGWINCH handler) interrupts
        // `poll` with `EINTR`. Report "not ready" rather than surfacing the
        // error: the caller then ticks and re-waits, instead of mistaking the
        // interruption for a fatal read failure that quits the whole TUI.
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        Err(err)
    } else {
        Ok(pollfd.revents & libc::POLLIN != 0)
    }
}

/// `select(2)` `fd` for readability up to `timeout_ms` (negative blocks). Used in
/// place of `poll` on a macOS tty, where `poll` does not report tty readiness.
#[cfg(target_os = "macos")]
fn select_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    use std::mem;
    unsafe {
        let mut read_fd_set: libc::fd_set = mem::zeroed();
        let mut timeout_val;
        let timeout = if timeout_ms < 0 {
            std::ptr::null_mut()
        } else {
            timeout_val = libc::timeval {
                tv_sec: (timeout_ms / 1000) as _,
                tv_usec: ((timeout_ms % 1000) * 1000) as _,
            };
            &mut timeout_val
        };
        libc::FD_ZERO(&mut read_fd_set);
        libc::FD_SET(fd, &mut read_fd_set);
        let ret = libc::select(
            fd + 1,
            &mut read_fd_set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            timeout,
        );
        if ret < 0 {
            let err = io::Error::last_os_error();
            // `EINTR` (a delivered signal, e.g. SIGWINCH) is not a read failure:
            // report "not ready" so the caller ticks and re-waits. See
            // [`poll_readable`].
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            Err(err)
        } else {
            Ok(libc::FD_ISSET(fd, &read_fd_set))
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

/// Read the next input event: a key, a wheel turn ([`ScrollEvent`]), or a left /
/// right-button click ([`ClickEvent`]). Other mouse reports (motion, drags, other
/// buttons, releases) are swallowed so they never reach the event loop. `read`
/// yields successive keys from the terminal.
fn next_input(mut read: impl FnMut() -> io::Result<Key>) -> io::Result<Input> {
    loop {
        let key = read()?;
        // A modified cursor key (e.g. `Shift`+arrow) arrives as `CSI 1 ; <mod>
        // <letter>`, which `console` only half-decodes — the `1 ;` head becomes an
        // `UnknownEscSeq` and the `<mod><letter>` tail leaks as stray `Char`s.
        // Reassemble the whole sequence into one key so the event loop sees a
        // single press instead of those strays (the note editor maps the
        // `Shift` ones to a selection; see `home::event::handlers`).
        if is_modified_key_head(&key) {
            return Ok(Input::Key(read_modified_key(&mut read)?));
        }
        let event = match mouse_seq_kind(&key) {
            Some((MouseSeq::Sgr, head)) => read_sgr(head, &mut read)?,
            Some((MouseSeq::X10, head)) => read_x10(head, &mut read)?,
            // A real key (or an unrelated escape sequence): hand it back.
            None => return Ok(Input::Key(key)),
        };
        // A wheel turn or button click surfaces as its event; any other mouse
        // report drains to `None` and we loop for the next real input.
        if let Some(event) = event {
            return Ok(event);
        }
    }
}

/// Whether `key` is the head `console` emits for a modified cursor key: the
/// `[ 1 ;` prefix of a `CSI 1 ; <mod> <letter>` sequence (the `<mod><letter>`
/// tail is still unread). Shift / Ctrl / Alt + an arrow, `Home`, or `End` all
/// take this form.
fn is_modified_key_head(key: &Key) -> bool {
    matches!(key, Key::UnknownEscSeq(seq) if seq.as_slice() == ['[', '1', ';'])
}

/// Reassemble a modified cursor key into one [`Key::UnknownEscSeq`] holding the
/// whole `[ 1 ; <mod> <letter>` sequence, draining the `<mod><letter>` tail (the
/// modifier digits, then the terminating letter) that `console` left unread.
///
/// A non-`Char` interrupting the tail ends the (incomplete) sequence early and is
/// itself consumed and dropped — the same way [`read_sgr`] / [`read_x10`] abandon
/// a mouse report interrupted mid-stream. This only arises on corrupted or
/// interleaved input: a real terminal emits `CSI 1 ; <mod> <letter>` atomically,
/// so the tail is never interrupted in practice. The event loop ignores the
/// returned incomplete sequence just like any complete one it does not recognise.
fn read_modified_key(read: &mut impl FnMut() -> io::Result<Key>) -> io::Result<Key> {
    let mut seq = vec!['[', '1', ';'];
    // Drain the `<mod><letter>` tail: digits accumulate, and the first non-digit
    // `Char` is the terminating letter. A non-`Char` ends the (incomplete) tail;
    // it is consumed by this `read()` and dropped (see the doc above).
    while let Key::Char(c) = read()? {
        seq.push(c);
        if !c.is_ascii_digit() {
            break;
        }
    }
    Ok(Key::UnknownEscSeq(seq))
}

/// Drain an SGR report (the parameters plus the `M`/`m` terminator), starting
/// from the `head` bytes `console` already consumed, and turn a wheel turn into a
/// scroll or a left / right-button press into a click. Returns `None` for any other
/// report (motion, other buttons, releases) or a malformed one.
fn read_sgr(
    head: Vec<char>,
    read: &mut impl FnMut() -> io::Result<Key>,
) -> io::Result<Option<Input>> {
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

/// Parse the `Cb;Cx;Cy` parameters of an SGR report (with its press/release flag)
/// into a wheel scroll or a left / right click.
fn parse_sgr(payload: &str, release: bool) -> Option<Input> {
    let mut params = payload.split(';');
    let cb: u32 = params.next()?.parse().ok()?;
    let cx: u32 = params.next()?.parse().ok()?;
    let cy: u32 = params.next()?.parse().ok()?;
    mouse_event(cb, cx, cy, release)
}

/// Drain an X10 report (three bytes, each value + 32), starting from the `head`
/// bytes `console` already consumed, and turn a wheel turn into a scroll or a
/// left / right-button press into a click. Returns `None` for any other or malformed
/// report.
///
/// Best-effort: we always request SGR coordinates (DECSET 1006), so a compliant
/// terminal never sends X10 and this path is effectively a fallback. It assumes
/// each coordinate byte is a single `char`; columns/rows past 95 (byte ≥ 128)
/// would mis-frame, but at worst the report is dropped — it cannot move a list.
/// X10 has no separate release flag (a button release is its own code, `3`), so
/// the report is always treated as a press.
fn read_x10(
    head: Vec<char>,
    read: &mut impl FnMut() -> io::Result<Key>,
) -> io::Result<Option<Input>> {
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
    Ok(mouse_event(cb, cx, cy, false))
}

/// Build an [`Input`] from a decoded button code, 1-based coordinates, and the
/// press/release flag: a wheel turn becomes a [`ScrollEvent`], a left / right
/// press a [`ClickEvent`], a bare pointer move an [`Input::Hover`], and anything
/// else `None` (so it is swallowed).
fn mouse_event(cb: u32, cx: u32, cy: u32, release: bool) -> Option<Input> {
    let col = cx.saturating_sub(1).min(u16::MAX as u32) as u16;
    let row = cy.saturating_sub(1).min(u16::MAX as u32) as u16;
    // Wheel turns set bit 6 (64); the low two bits pick the axis and direction:
    // 0 = up, 1 = down (2 / 3 are the horizontal wheel, which we ignore). A wheel
    // reports only a press, so a (spurious) release is dropped.
    if cb & 0x40 != 0 {
        if release {
            return None;
        }
        let lines = match cb & 0b11 {
            0 => -WHEEL_LINES,
            1 => WHEEL_LINES,
            _ => return None,
        };
        return Some(Input::Scroll(ScrollEvent { lines, col, row }));
    }
    // A plain button click: not the wheel (handled above), not a motion report
    // (bit 5, 32), and the low two bits select the button (0 = left, 2 = right).
    // Fire on the press (`M`) and drop the matching release (`m`), so one click is
    // one event.
    if !release && cb & 0x20 == 0 {
        return match cb & 0b11 {
            0 => Some(Input::Click(ClickEvent { col, row })),
            2 => Some(Input::RightClick(ClickEvent { col, row })),
            _ => None,
        };
    }
    // A bare pointer move (DECSET 1003 any-event tracking): the motion bit (32) is
    // set with no button held — the low two bits read as the "released / no button"
    // code (3). Surface it as a hover so the home loop can drive the sidebar PR
    // popup. A drag (motion *with* a held button) keeps the low bits of that button
    // and so falls through to `None`, dropped like before.
    if cb & 0x20 != 0 && cb & 0b11 == 0b11 {
        return Some(Input::Hover(ClickEvent { col, row }));
    }
    None
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

    /// The fragments `console` produces for a `CSI 1 ; <mod> <letter>` key: the
    /// `[ 1 ;` head as an `UnknownEscSeq`, then the modifier digit(s) and the
    /// terminating letter as stray `Char`s.
    fn modified_key(modifier: &str, letter: char) -> Vec<Key> {
        let mut keys = vec![Key::UnknownEscSeq(vec!['[', '1', ';'])];
        keys.extend(modifier.chars().map(Key::Char));
        keys.push(Key::Char(letter));
        keys
    }

    #[test]
    fn modified_cursor_key_is_reassembled_into_one_key() {
        // Shift+Left (`CSI 1 ; 2 D`) comes back as a single `UnknownEscSeq`
        // carrying the whole sequence, not the stray `2` / `D` console leaks.
        let input = drive(modified_key("2", 'D'));
        assert_eq!(
            input,
            Input::Key(Key::UnknownEscSeq(vec!['[', '1', ';', '2', 'D']))
        );
    }

    #[test]
    fn modified_key_with_a_multi_digit_modifier_is_reassembled() {
        // Modifiers can be two digits (e.g. 16 = Ctrl+Alt+Shift): all are kept.
        let input = drive(modified_key("16", 'F'));
        assert_eq!(
            input,
            Input::Key(Key::UnknownEscSeq(vec!['[', '1', ';', '1', '6', 'F']))
        );
    }

    #[test]
    fn a_non_char_interrupting_a_modified_key_ends_it_and_is_dropped() {
        // A non-`Char` mid-tail ends the (incomplete) sequence, returned first.
        // Like the SGR/X10 mouse readers, that interrupting key is consumed and
        // dropped — it does not resurface on the next read. (Unreachable on real
        // terminals, which emit `CSI 1;<mod><letter>` atomically; this only pins
        // the corrupted/interleaved-input contract.)
        let mut queue: VecDeque<Key> = vec![
            Key::UnknownEscSeq(vec!['[', '1', ';']),
            Key::Char('2'),
            Key::Enter, // interrupts before the letter
        ]
        .into();
        let mut read = || {
            queue
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "drained"))
        };
        // First read: the reassembled (incomplete) modified key.
        assert_eq!(
            next_input(&mut read).unwrap(),
            Input::Key(Key::UnknownEscSeq(vec!['[', '1', ';', '2']))
        );
        // The interrupting Enter was dropped, so the next read finds nothing left.
        assert!(next_input(&mut read).is_err());
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
    fn sgr_left_click_press_becomes_a_click_at_its_position() {
        // A left-button press (cb 0) at column 6, row 9 (1-based) surfaces as a
        // click at the 0-based cell — what the home loop hit-tests the mascot with.
        let input = drive(sgr(0, 6, 9));
        assert_eq!(input, Input::Click(ClickEvent { col: 5, row: 8 }));
    }

    #[test]
    fn sgr_right_button_press_becomes_a_right_click() {
        // A right-button press (cb 2) at column 3, row 4 (1-based) opens context
        // menus on screens that support them.
        let input = drive(sgr(2, 3, 4));
        assert_eq!(input, Input::RightClick(ClickEvent { col: 2, row: 3 }));
    }

    #[test]
    fn sgr_drag_report_is_swallowed() {
        // A drag (motion bit 32 with the left button held, low bits 0) is dropped on
        // the management screens, and the following key is returned.
        let mut keys = sgr(32, 1, 1);
        keys.push(Key::Char('q'));
        assert_eq!(drive(keys), Input::Key(Key::Char('q')));
    }

    #[test]
    fn sgr_bare_motion_becomes_a_hover_at_its_position() {
        // A bare pointer move (motion bit 32 with no button held, low bits 3 → cb
        // 35) at column 6, row 9 (1-based) surfaces as a hover at the 0-based cell.
        let input = drive(sgr(35, 6, 9));
        assert_eq!(input, Input::Hover(ClickEvent { col: 5, row: 8 }));
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
        // SGR release reports end with a lowercase `m`; a click's release is
        // dropped (the press already fired), so only the following key surfaces.
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
    fn x10_left_click_becomes_a_click() {
        // `ESC [ M Cb Cx Cy`: left press is cb 0 (+32 = ' '); column/row 1 are
        // (1 + 32) = '!'. The report surfaces as a click at the 0-based cell.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', ' ']), // cb 0 (space = 32)
            Key::Char('!'),
            Key::Char('!'),
        ];
        assert_eq!(drive(keys), Input::Click(ClickEvent { col: 0, row: 0 }));
    }

    #[test]
    fn x10_bare_motion_becomes_a_hover() {
        // `ESC [ M Cb Cx Cy`: a bare move is cb 35 (+32 = 'C'); column/row 1 are
        // (1 + 32) = '!'. The report surfaces as a hover at the 0-based cell.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', 'C']), // cb 35 (67 = 'C')
            Key::Char('!'),
            Key::Char('!'),
        ];
        assert_eq!(drive(keys), Input::Hover(ClickEvent { col: 0, row: 0 }));
    }

    #[test]
    fn x10_right_click_becomes_a_right_click() {
        // A right-button X10 report (cb 2, '"') surfaces and consumes its two
        // coordinate chars.
        let keys = vec![
            Key::UnknownEscSeq(vec!['[', 'M', '"']), // cb 2 (34 = '"')
            Key::Char('!'),
            Key::Char('"'),
        ];
        assert_eq!(
            drive(keys),
            Input::RightClick(ClickEvent { col: 0, row: 1 })
        );
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
