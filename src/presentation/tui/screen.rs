use std::fmt::Write as _;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::echo::EchoGuard;
use crate::presentation::tui::install_task::{self, InstallHandle};

/// A mouse-wheel scroll, decoded from a terminal mouse report.
///
/// No management screen acts on one: the TUI itself never scrolls (the embedded
/// terminal pane has its own history scroll, handled separately via
/// `crossterm`). [`KeyReader::read_key`] drops scrolls, so they are read and
/// swallowed rather than reaching the host terminal's own viewport and
/// revealing the pre-launch scrollback. This type stays the unit a decoded wheel
/// turn drains through in [`term_reader`](crate::presentation::tui::term_reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollEvent {
    /// Lines to scroll: negative scrolls up (toward older content), positive
    /// scrolls down (toward the newest).
    pub lines: i32,
    /// The 0-based column the wheel was reported over, used to tell which pane
    /// the cursor was in.
    pub col: u16,
    /// The 0-based row the wheel was reported over.
    pub row: u16,
}

/// One unit of terminal input: a key press, or a mouse-wheel scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Scroll(ScrollEvent),
}

/// Source of input driving an interactive screen.
///
/// Abstracting the read lets event loops be exercised without a real terminal:
/// tests supply a scripted sequence of inputs.
///
/// Implementors provide [`read_key`](KeyReader::read_key). The default
/// [`read_input`](KeyReader::read_input) reports every read as a key, which is
/// all most screens and their test stubs need; the real terminal's reader
/// overrides it to decode (and swallow) mouse reports so a wheel turn never
/// leaks into the key stream.
pub trait KeyReader {
    /// The next key press, discarding any scrolls along the way.
    fn read_key(&mut self) -> io::Result<Key>;

    /// The next input event (a key, or a mouse-wheel scroll). Defaults to a
    /// key, so screens and their test stubs need not implement it. The real
    /// terminal decodes mouse reports here; [`read_key`](Self::read_key) then
    /// drops the scrolls, since no screen scrolls the TUI in place.
    fn read_input(&mut self) -> io::Result<Input> {
        Ok(Input::Key(self.read_key()?))
    }

    /// The next key, or `None` if `timeout` elapses with nothing pressed. Lets a
    /// screen wake periodically to repaint a time-based overlay (the background
    /// install rabbit) while still waiting on the user. The default blocks like
    /// [`read_key`](Self::read_key) and always returns a key — all the screens'
    /// test stubs inherit this, so their behaviour is unchanged; only the real
    /// terminal reader overrides it to honour the timeout.
    fn read_key_timeout(&mut self, _timeout: Duration) -> io::Result<Option<Key>> {
        Ok(Some(self.read_key()?))
    }
}

/// Read the next key, keeping the background-install overlay animating while it
/// waits. While `handle` reports an active install, the read wakes every
/// [`install_task::ANIM_TICK`] to repaint the overlay — advancing its hop and
/// expression on the clock, independent of any progress — instead of blocking;
/// once a key arrives (or no install is in flight) it returns. Read errors
/// propagate unchanged, so each screen's existing error handling still applies.
pub fn animated_read(
    reader: &mut dyn KeyReader,
    term: &Term,
    painter: &mut FramePainter,
    handle: &InstallHandle,
) -> io::Result<Key> {
    loop {
        if !handle.is_active(Instant::now()) {
            return reader.read_key();
        }
        match reader.read_key_timeout(install_task::ANIM_TICK)? {
            Some(key) => return Ok(key),
            // A tick with no key: repaint so the overlay's time-based animation
            // moves, then keep waiting.
            None => {
                let _ = painter.tick(term);
            }
        }
    }
}

/// Enter the alternate screen.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
/// Leave the alternate screen, restoring the prior contents.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";
/// Enable mouse reporting: normal tracking (DECSET 1000), button-event /
/// drag tracking (DECSET 1002), and SGR extended coordinates (DECSET 1006).
///
/// What keeps the pre-launch scrollback hidden is the **alternate screen**
/// ([`ENTER_ALT_SCREEN`]) — that scrollback lives in the primary buffer, which
/// the alternate screen replaces, so on every mainstream terminal (including
/// Apple Terminal.app, which ignores mouse reporting entirely) the wheel cannot
/// reveal it. Mouse reporting then does two further jobs on terminals that
/// honour it:
///
/// - it makes the terminal hand wheel and click events to us as escape sequences
///   rather than acting on the wheel itself, which `term_reader` decodes: a wheel
///   turn becomes a [`ScrollEvent`] (swallowed by the management screens, acted on
///   by the embedded pane), everything else is dropped.
/// - button-event tracking (DECSET 1002) additionally reports motion while a
///   button is held, so the embedded terminal pane can follow a drag and select
///   text (see `home::terminal_selection`). The management screens see these
///   motion reports too, but `term_reader` drops every non-wheel report, so they
///   are harmless there.
///
/// We deliberately do *not* enable alternate scroll (DECSET 1007). Alternate
/// scroll makes the wheel masquerade as cursor-key presses, and those are
/// indistinguishable from real arrow keys — so on a terminal that does not report
/// the wheel as a mouse event the wheel would silently move the lists (and never
/// reach the pane as a scroll). Relying on mouse reporting alone keeps the TUI
/// itself unscrollable on every terminal. The cost is that on a terminal that
/// ignores mouse reporting (Apple Terminal.app) the wheel does nothing in the
/// embedded pane either; there the pane scrolls via `Shift`+`↑`/`↓` (and
/// `Shift`+`PageUp`/`PageDown` where the terminal does not bind them itself).
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// Disable mouse reporting, restoring the terminal's normal wheel / selection
/// behaviour once the TUI exits. Reset in the reverse order of [`ENABLE_MOUSE`].
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// RAII guard that activates the terminal alternate screen and restores it on drop.
pub struct AlternateScreenGuard {
    term: Term,
    farewell: bool,
    /// Disables terminal echo while the TUI is up so the mouse-report flood that
    /// arrives once [`ENABLE_MOUSE`] is set is not echoed to the screen between
    /// `console`'s per-key raw reads. Dropped (echo restored) after this guard's
    /// own `drop` body runs.
    _echo: EchoGuard,
}

/// The escape sequences [`write_input_modes`] writes, as one string: the
/// **alternate screen** ([`ENTER_ALT_SCREEN`]) first, then mouse reporting
/// ([`ENABLE_MOUSE`]). Pulled out as a pure function so the exact bytes can be
/// asserted in a unit test — the `Term` write itself goes to a real terminal and
/// is not capturable.
fn input_mode_sequence() -> String {
    format!("{ENTER_ALT_SCREEN}{ENABLE_MOUSE}")
}

/// Write the input modes that keep the TUI unscrollable — the **alternate
/// screen** ([`ENTER_ALT_SCREEN`]) and mouse reporting ([`ENABLE_MOUSE`]) — so
/// the wheel is reported to us (and swallowed) rather than scrolling the host
/// terminal's own viewport and revealing the pre-launch scrollback behind the
/// TUI.
///
/// Written when the alternate screen is entered, and re-asserted after the
/// embedded terminal pane hands control back: that pane toggles `crossterm`'s
/// raw mode around itself and runs a full-screen child (an agent CLI resets the
/// terminal on its way out), so re-asserting **both** modes here keeps them
/// intact no matter what the pane (or the shell it ran) left behind.
///
/// Re-asserting the alternate screen — not only mouse capture — is what fixes a
/// whole-TUI scroll: the alternate screen is the *only* thing hiding the
/// scrollback on terminals that ignore mouse reporting (Apple Terminal.app), and
/// it would otherwise be entered just once at startup. A single stray leave
/// (`?1049l`) anywhere would then be unrecoverable and leave the whole TUI
/// scrollable. The caller repaints in full afterwards
/// ([`FramePainter::reset`]), so re-entering the alternate screen is harmless
/// even when it was already active.
pub(crate) fn write_input_modes(term: &Term) -> Result<()> {
    term.write_str(&input_mode_sequence())?;
    Ok(())
}

impl AlternateScreenGuard {
    pub fn new(term: Term) -> Result<Self> {
        let echo = EchoGuard::new();
        write_input_modes(&term)?;
        term.hide_cursor()?;
        Ok(Self {
            term,
            farewell: true,
            _echo: echo,
        })
    }

    /// Suppresses the farewell message on drop (e.g. when exiting with an error).
    pub fn dismiss(&mut self) {
        self.farewell = false;
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.term.write_str(DISABLE_MOUSE);
        let _ = self.term.write_str(LEAVE_ALT_SCREEN);
        let _ = self.term.show_cursor();
        if self.farewell {
            let _ = self.term.write_line("USAGI run away ( ^-^)ノ");
        }
    }
}

/// Repaints a screen by rewriting only the rows that changed since the last
/// frame, batched into a single terminal write — so an update lands in one pass
/// without the flicker of clearing the whole screen and redrawing every row on
/// each keystroke.
///
/// A *frame* is the `Vec<String>` an `ui::render_frame` returns: one styled line
/// per terminal row. The painter remembers the frame it last drew and, on the
/// next paint, moves to and rewrites only the rows whose text differs. The first
/// paint — and any paint after [`reset`](FramePainter::reset) — clears the
/// screen first, so leftover content from another screen can't show through.
///
/// Before diffing, the painter overlays the global background-install rabbit
/// (when one is in flight) onto the screen's frame, so every screen surfaces the
/// install without rendering it itself. It keeps the screen's `base` frame
/// separate from the `prev` overlaid frame so [`tick`](Self::tick) can re-apply
/// the (time-based) overlay between key presses and animate it.
#[derive(Default)]
pub struct FramePainter {
    /// The last frame a screen handed to [`paint`](Self::paint), before the
    /// install overlay is applied.
    base: Vec<String>,
    /// The last frame actually drawn (base + overlay), for diffing.
    prev: Vec<String>,
    /// A reusable scratch frame the overlay is composed into each flush, then
    /// swapped with `prev`. Holding it across flushes lets the per-paint compose
    /// reuse its row allocations (via `clone_from`) instead of allocating a fresh
    /// frame every time, so a steady stream of repaints does no heap work for the
    /// frame buffer itself.
    scratch: Vec<String>,
}

impl FramePainter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the last frame so the next [`paint`](Self::paint) clears the
    /// screen and repaints every row. Call this after another screen (a modal,
    /// the embedded terminal, the settings screen) has drawn over ours and left
    /// the remembered frame stale.
    pub fn reset(&mut self) {
        self.prev.clear();
    }

    /// Draw `frame` (overlaying any in-flight install), rewriting only the rows
    /// that changed since the previous paint, then remember it for the next diff.
    pub fn paint(&mut self, term: &Term, frame: Vec<String>) -> Result<()> {
        self.base = frame;
        self.flush(term)
    }

    /// Re-apply the install overlay to the last painted frame and repaint. Called
    /// while waiting on a key so the overlay's time-based animation keeps moving
    /// even when nothing else on screen changed.
    pub fn tick(&mut self, term: &Term) -> Result<()> {
        self.flush(term)
    }

    /// Overlay the global install (if any) onto the base frame and diff-paint it.
    fn flush(&mut self, term: &Term) -> Result<()> {
        let (_, width) = term.size();
        // Compose into the reused scratch buffer rather than a fresh clone: copy
        // the base into it (reusing its rows' allocations) and overlay any install
        // on top. `prev` is still untouched, so it remains the correct diff base.
        self.scratch.clone_from(&self.base);
        install_task::overlay(
            &mut self.scratch,
            width as usize,
            install_task::snapshot().as_ref(),
        );
        term.write_str(&diff_frame(&self.prev, &self.scratch))?;
        term.flush()?;
        // The scratch is now the painted frame: make it `prev` for the next diff
        // and reclaim the old `prev` as the next scratch, so neither is reallocated.
        std::mem::swap(&mut self.prev, &mut self.scratch);
        Ok(())
    }
}

/// Builds the escape sequence that turns the `prev` frame into `frame` on
/// screen. Hides the cursor for the repaint, rewrites each row whose text
/// changed (moving to it and clearing it first), and clears any trailing rows a
/// shorter new frame leaves behind. When `prev` is empty — the first paint, or
/// after a [`FramePainter::reset`] — the whole screen is cleared first and every
/// row is drawn.
///
/// Exposed to the crate so the embedded terminal pane — which also parks the
/// real cursor over the shell after the repaint — can share the same diff.
pub(crate) fn diff_frame(prev: &[String], frame: &[String]) -> String {
    let fresh = prev.is_empty();
    // Hide the cursor while repainting so it does not flicker across the rows.
    let mut buf = String::from("\x1b[?25l");
    if fresh {
        // Nothing remembered: clear whatever another screen left behind.
        buf.push_str("\x1b[2J");
    }
    for (row, line) in frame.iter().enumerate() {
        if fresh || prev.get(row) != Some(line) {
            // Move to the row (1-based), clear it, then write the new content.
            let _ = write!(buf, "\x1b[{};1H\x1b[2K", row + 1);
            buf.push_str(line);
        }
    }
    // A shorter frame than last time (e.g. after a resize) leaves stale rows
    // below; clear them.
    for row in frame.len()..prev.len() {
        let _ = write!(buf, "\x1b[{};1H\x1b[2K", row + 1);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that yields one scripted key, used to exercise the default
    /// [`KeyReader::read_input`] (the real terminal overrides it; the stubbed
    /// screens only implement [`KeyReader::read_key`]).
    struct OneKey(Key);

    impl KeyReader for OneKey {
        fn read_key(&mut self) -> io::Result<Key> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn default_read_input_wraps_a_key() {
        // The default implementation reports each read as a key, never a scroll.
        let mut reader = OneKey(Key::Char('a'));
        assert_eq!(reader.read_input().unwrap(), Input::Key(Key::Char('a')));
    }

    #[test]
    fn input_modes_reassert_both_the_alternate_screen_and_mouse_capture() {
        // Re-asserting the input modes must re-enter the alternate screen — the
        // sole defence against a whole-TUI scroll on terminals that ignore mouse
        // reporting — and not only mouse capture; otherwise a stray leave is
        // unrecoverable and the scrollback gets exposed (#50). The alternate
        // screen comes first so the mouse modes apply within it.
        let seq = input_mode_sequence();
        let alt = seq
            .find(ENTER_ALT_SCREEN)
            .expect("re-enters the alt screen");
        let mouse = seq.find(ENABLE_MOUSE).expect("re-enables mouse capture");
        assert!(alt < mouse);
    }

    #[test]
    fn guard_writes_farewell_when_not_dismissed() {
        let guard = AlternateScreenGuard::new(Term::stdout()).unwrap();
        // Dropping without dismissing takes the farewell branch.
        drop(guard);
    }

    #[test]
    fn dismiss_suppresses_farewell() {
        let mut guard = AlternateScreenGuard::new(Term::stdout()).unwrap();
        guard.dismiss();
        // Dropping after dismiss skips the farewell branch.
        drop(guard);
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn diff_frame_clears_and_draws_every_row_on_the_first_paint() {
        let out = diff_frame(&[], &lines(&["a", "b"]));
        // Hide cursor, clear the screen, then place and write both rows.
        assert!(out.starts_with("\x1b[?25l\x1b[2J"));
        assert!(out.contains("\x1b[1;1H\x1b[2Ka"));
        assert!(out.contains("\x1b[2;1H\x1b[2Kb"));
    }

    #[test]
    fn diff_frame_rewrites_only_the_changed_rows() {
        let prev = lines(&["a", "b", "c"]);
        let out = diff_frame(&prev, &lines(&["a", "B", "c"]));
        // No full-screen clear once a frame is remembered.
        assert!(!out.contains("\x1b[2J"));
        // Only row 2 (1-based) is repainted; the unchanged rows are skipped.
        assert!(out.contains("\x1b[2;1H\x1b[2KB"));
        assert!(!out.contains("\x1b[1;1H"));
        assert!(!out.contains("\x1b[3;1H"));
    }

    #[test]
    fn diff_frame_clears_rows_a_shorter_frame_leaves_behind() {
        let prev = lines(&["a", "b", "c"]);
        let out = diff_frame(&prev, &lines(&["a", "b"]));
        // Row 3 is gone from the new frame, so it is cleared but not rewritten.
        assert!(out.contains("\x1b[3;1H\x1b[2K"));
        assert!(!out.contains("\x1b[3;1H\x1b[2Kc"));
    }

    /// A reader scripting both blocking reads and timeout reads, so the two
    /// paths of [`animated_read`] can be exercised independently.
    struct TickReader {
        timeouts: std::collections::VecDeque<io::Result<Option<Key>>>,
        blocking: std::collections::VecDeque<io::Result<Key>>,
    }

    impl KeyReader for TickReader {
        fn read_key(&mut self) -> io::Result<Key> {
            self.blocking.pop_front().unwrap_or(Ok(Key::Escape))
        }
        fn read_key_timeout(&mut self, _timeout: Duration) -> io::Result<Option<Key>> {
            self.timeouts.pop_front().unwrap_or(Ok(Some(Key::Escape)))
        }
    }

    #[test]
    fn default_read_key_timeout_blocks_and_returns_a_key() {
        // The trait default ignores the timeout and yields the next key, so the
        // screens' blocking stubs keep their behaviour.
        let mut reader = OneKey(Key::Char('z'));
        assert_eq!(
            reader.read_key_timeout(Duration::ZERO).unwrap(),
            Some(Key::Char('z'))
        );
    }

    #[test]
    fn animated_read_blocks_when_no_install_is_active() {
        // With an idle install the read is a plain blocking read.
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        let handle = InstallHandle::new();
        let mut reader = TickReader {
            timeouts: Default::default(),
            blocking: std::collections::VecDeque::from(vec![Ok(Key::Char('a'))]),
        };
        let key = animated_read(&mut reader, &term, &mut painter, &handle).unwrap();
        assert_eq!(key, Key::Char('a'));
    }

    #[test]
    fn animated_read_polls_and_repaints_while_an_install_runs() {
        // An active install switches to timeout reads: a tick with no key
        // repaints the overlay (advancing its animation), then the next key
        // returns.
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        painter.paint(&term, lines(&["base"])).unwrap();
        let handle = InstallHandle::new();
        handle.begin_at("m", Instant::now());
        let mut reader = TickReader {
            timeouts: std::collections::VecDeque::from(vec![Ok(None), Ok(Some(Key::Enter))]),
            blocking: Default::default(),
        };
        let key = animated_read(&mut reader, &term, &mut painter, &handle).unwrap();
        assert_eq!(key, Key::Enter);
    }

    #[test]
    fn animated_read_propagates_a_timeout_read_error() {
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        let handle = InstallHandle::new();
        handle.begin_at("m", Instant::now());
        let mut reader = TickReader {
            timeouts: std::collections::VecDeque::from(vec![Err(io::Error::other("boom"))]),
            blocking: Default::default(),
        };
        let err = animated_read(&mut reader, &term, &mut painter, &handle).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn frame_painter_repaints_in_full_after_a_reset() {
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        // First paint remembers the frame.
        painter.paint(&term, lines(&["a", "b"])).unwrap();
        // An identical frame now diffs to nothing but the cursor-hide prefix.
        assert_eq!(diff_frame(&painter.prev, &lines(&["a", "b"])), "\x1b[?25l");
        // After a reset the remembered frame is forgotten, forcing a full repaint.
        painter.reset();
        assert!(painter.prev.is_empty());
        painter.paint(&term, lines(&["a", "b"])).unwrap();
        assert_eq!(painter.prev, lines(&["a", "b"]));
    }
}
