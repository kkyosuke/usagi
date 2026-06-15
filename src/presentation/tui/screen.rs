use std::fmt::Write as _;
use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::echo::EchoGuard;

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
}

/// Enter the alternate screen.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
/// Leave the alternate screen, restoring the prior contents.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";
/// Enable mouse reporting: normal tracking (DECSET 1000) plus SGR extended
/// coordinates (DECSET 1006). Claiming the mouse this way does two jobs at once:
///
/// - it stops the terminal from acting on the wheel itself — which is what would
///   otherwise scroll the host viewport and reveal the pre-launch scrollback
///   "behind" the TUI — and
/// - it hands wheel and click events to us as escape sequences instead, which
///   `term_reader` decodes: a wheel turn becomes a [`ScrollEvent`] (swallowed by
///   the management screens, acted on by the embedded pane), everything else is
///   dropped.
///
/// We deliberately do *not* enable alternate scroll (DECSET 1007). Alternate
/// scroll makes the wheel masquerade as cursor-key presses, and those are
/// indistinguishable from real arrow keys — so on a terminal that does not report
/// the wheel as a mouse event the wheel would silently move the lists (and never
/// reach the pane as a scroll). Relying on mouse reporting alone keeps the TUI
/// itself unscrollable on every terminal; where the wheel is not reported, the
/// embedded pane still scrolls via `Shift`+`PageUp`/`PageDown`.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
/// Disable mouse reporting, restoring the terminal's normal wheel / selection
/// behaviour once the TUI exits. Reset in the reverse order of [`ENABLE_MOUSE`].
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1000l";

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

/// Write the wheel-capture input mode — mouse reporting ([`ENABLE_MOUSE`]) — so
/// the wheel is reported to us (and swallowed) rather than scrolling the host
/// terminal's own viewport and revealing the pre-launch scrollback behind the
/// TUI.
///
/// Set once when the alternate screen is entered, and re-asserted after the
/// embedded terminal pane hands control back: that pane toggles `crossterm`'s
/// raw mode around itself, so re-asserting here keeps the capture intact no
/// matter what the pane (or the shell it ran) left behind.
pub(crate) fn write_input_modes(term: &Term) -> Result<()> {
    term.write_str(ENABLE_MOUSE)?;
    Ok(())
}

impl AlternateScreenGuard {
    pub fn new(term: Term) -> Result<Self> {
        let echo = EchoGuard::new();
        term.write_str(ENTER_ALT_SCREEN)?;
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
#[derive(Default)]
pub struct FramePainter {
    prev: Vec<String>,
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

    /// Draw `frame`, rewriting only the rows that changed since the previous
    /// paint, then remember it for the next diff.
    pub fn paint(&mut self, term: &Term, frame: Vec<String>) -> Result<()> {
        term.write_str(&diff_frame(&self.prev, &frame))?;
        term.flush()?;
        self.prev = frame;
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
