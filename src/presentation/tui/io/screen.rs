use std::fmt::Write as _;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::install_task::{self, InstallHandle};
use crate::presentation::tui::io::echo::EchoGuard;
use crate::presentation::tui::widgets;

/// A mouse-wheel scroll, decoded from a terminal mouse report.
///
/// No management screen acts on one: the TUI itself never scrolls (the embedded
/// terminal pane has its own history scroll, handled separately via
/// `crossterm`). [`KeyReader::read_key`] drops scrolls, so they are read and
/// swallowed rather than reaching the host terminal's own viewport and
/// revealing the pre-launch scrollback. This type stays the unit a decoded wheel
/// turn drains through in [`term_reader`](crate::presentation::tui::io::term_reader).
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

/// A mouse click, decoded from a terminal mouse report.
///
/// Management screens mostly act on left clicks (sidebar/session selection,
/// links, tab chips), while right clicks open context menus such as the home
/// tab menu. Keeping the coordinates in one small DTO lets tests synthesize both
/// paths without a real terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickEvent {
    /// The 0-based column the click was reported at.
    pub col: u16,
    /// The 0-based row the click was reported at.
    pub row: u16,
}

/// One unit of terminal input: a key press, a mouse-wheel scroll, a click, or a
/// bare pointer move (hover).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Scroll(ScrollEvent),
    Click(ClickEvent),
    RightClick(ClickEvent),
    /// The pointer moved with no button held — its current cell (the same
    /// `(col, row)` shape as a [`ClickEvent`]). The home loop uses it to show the
    /// sidebar's PR hover popup; every key-only screen drops it.
    Hover(ClickEvent),
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

    /// The next input event (key, scroll, or click), or `None` if `timeout`
    /// elapses with nothing ready — the input-aware counterpart of
    /// [`read_key_timeout`](Self::read_key_timeout). The home loop reads through
    /// this so a click can reach it (to make the mascot react) while idle ticks
    /// still wake it to animate. Defaults to wrapping
    /// [`read_key_timeout`](Self::read_key_timeout) as a key, so every screen and
    /// its test stub inherits its existing timeout behaviour unchanged; only the
    /// real terminal reader overrides it to surface scrolls and clicks.
    fn read_input_timeout(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        Ok(self.read_key_timeout(timeout)?.map(Input::Key))
    }
}

/// Read the next key, keeping the background-install overlay animating while it
/// waits. While `handle` reports an active install, the read wakes every
/// [`install_task::ANIM_TICK`] to repaint the overlay — advancing its hop and
/// expression on the clock, independent of any progress — instead of blocking;
/// once a key arrives (or no install is in flight) it returns. Read errors
/// propagate unchanged, so each screen's existing error handling still applies.
///
/// A terminal resize while animating returns `Key::Unknown` instead of ticking
/// in place: the tick would repaint the caller's *old-size* frame (freshly laid
/// out only on the caller's loop pass), so the screen would keep its stale
/// layout until the next real key. Returning the same artefact a blocking read
/// surfaces for a resize (see `term_reader`) sends the caller around its loop,
/// which re-reads the size and re-renders — a full clear + redraw, since the
/// painter discards its diff base on the same size change.
pub fn animated_read(
    reader: &mut dyn KeyReader,
    term: &Term,
    painter: &mut FramePainter,
    handle: &InstallHandle,
) -> io::Result<Key> {
    animated_read_sized(reader, term, painter, handle, &mut || term.size())
}

/// [`animated_read`] with the terminal-size read injected, so the
/// resize-while-animating branch is exercised in tests (a real `Term`'s size
/// cannot be changed from a unit test).
fn animated_read_sized(
    reader: &mut dyn KeyReader,
    term: &Term,
    painter: &mut FramePainter,
    handle: &InstallHandle,
    size: &mut dyn FnMut() -> (u16, u16),
) -> io::Result<Key> {
    let entry_size = size();
    loop {
        if !handle.is_active(Instant::now()) {
            return reader.read_key();
        }
        match reader.read_key_timeout(install_task::ANIM_TICK)? {
            Some(key) => return Ok(key),
            // A tick with no key. On a resize (which interrupts the wait via
            // SIGWINCH → EINTR, surfacing as this same tick) hand the caller the
            // resize artefact so it re-renders at the new size; otherwise repaint
            // so the overlay's time-based animation moves, then keep waiting.
            None => {
                if size() != entry_size {
                    return Ok(Key::Unknown);
                }
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
///   turn becomes a [`ScrollEvent`] (swallowed by the management screens; the
///   embedded pane scrolls its own history on the primary screen, or forwards the
///   wheel as arrow keys to a full-screen program on the alternate screen — see
///   `home::terminal::pane`), everything else is dropped.
/// - button-event tracking (DECSET 1002) additionally reports motion while a
///   button is held, so the embedded terminal pane can follow a drag and select
///   text (see `home::terminal::selection`).
/// - any-event tracking (DECSET 1003) additionally reports bare motion (no button
///   held), which `term_reader` surfaces as an [`Input::Hover`] so the home loop
///   can show the sidebar's PR hover popup. Key-only screens drop it like any
///   other non-key report, so it is harmless there.
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
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
/// Disable mouse reporting, restoring the terminal's normal wheel / selection
/// behaviour once the TUI exits. Reset in the reverse order of [`ENABLE_MOUSE`].
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// The full escape sequence that returns the host terminal to its pre-TUI state
/// on an exit that runs no `Drop`: leave the alternate screen
/// ([`LEAVE_ALT_SCREEN`]), disable every mouse-reporting mode ([`DISABLE_MOUSE`])
/// and bracketed paste (`?2004l`), and show the cursor (`?25h`).
///
/// Held as one `const` byte slice (rather than composed at runtime) so the
/// SIGINT / SIGTERM / SIGHUP handlers in [`super::signals`] can write it straight
/// to the stdout fd without allocating — the only work that is async-signal-safe.
/// [`write_terminal_restore`] writes the same bytes on the panic path
/// (`app::install_panic_hook`), and a unit test pins them to
/// [`LEAVE_ALT_SCREEN`] / [`DISABLE_MOUSE`] so the abrupt-exit restore never
/// drifts from what the guard's `Drop` resets.
pub(crate) const TERMINAL_RESTORE: &[u8] =
    b"\x1b[?1049l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?25h";

/// Write the terminal-restore sequence ([`TERMINAL_RESTORE`]) to `out` and flush.
///
/// The panic hook uses this with real stdout; taking an `impl Write` keeps the
/// bytes assertable in a unit test (a `Term`/fd write is not capturable). The
/// signal handlers cannot take an `impl Write` in an async-signal-safe context,
/// so they write the same `TERMINAL_RESTORE` bytes to the raw stdout fd instead.
pub(crate) fn write_terminal_restore(out: &mut impl io::Write) -> io::Result<()> {
    out.write_all(TERMINAL_RESTORE)?;
    out.flush()
}

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
pub(crate) fn input_mode_sequence() -> String {
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
        // Backstop for the embedded terminal's OSC 22 mouse pointer shape. The
        // pane restores the default when it returns to the home UI, but a panic
        // or early process exit could bypass that boundary; never leave the
        // user's shell with usagi's text-caret / hand pointer.
        let _ = self.term.write_str("\x1b]22;\x1b\\");
        let _ = self.term.write_str(DISABLE_MOUSE);
        let _ = self.term.write_str(LEAVE_ALT_SCREEN);
        let _ = self.term.show_cursor();
        if self.farewell {
            for line in widgets::farewell_lines() {
                let _ = self.term.write_line(&line);
            }
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
/// paint clears the screen before drawing every row; after
/// [`reset`](FramePainter::reset) it instead rewrites every remembered row with
/// per-row clears, so leftover content from another screen can't show through
/// without a whole-screen flash.
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
    /// Optional column-diff layout for the last base frame. Home uses this to
    /// keep the body sidebar and right pane repaint scopes independent; all other
    /// screens leave it `None` and keep the row-diff behaviour.
    columns: Option<ColumnDiff>,
    /// A reusable scratch frame the overlay is composed into each flush, then
    /// swapped with `prev`. Holding it across flushes lets the per-paint compose
    /// reuse its row allocations (via `clone_from`) instead of allocating a fresh
    /// frame every time, so a steady stream of repaints does no heap work for the
    /// frame buffer itself.
    scratch: Vec<String>,
    /// Escape bytes to emit at the very front of the next flush, before the diff.
    /// Set by [`reset_with_prefix`](Self::reset_with_prefix) so a mode
    /// re-assertion after the embedded pane (re-entering the alternate screen,
    /// which *clears* it) rides the same terminal write as the forced full
    /// repaint — never a separate flush that leaves the cleared screen visible as
    /// a one-frame black flash. Drained (cleared) on the flush that emits it.
    prefix: String,
    /// The terminal size at the last flush, so a resize between flushes is
    /// detected and the remembered frame discarded
    /// ([`invalidate_on_resize`](Self::invalidate_on_resize)).
    last_size: Option<(u16, u16)>,
}

impl FramePainter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force the next [`paint`](Self::paint) to repaint every row **without a
    /// whole-screen clear**. Call this after another screen (a modal, the embedded
    /// terminal, the settings screen) has drawn over ours and left the remembered
    /// frame stale.
    ///
    /// Rather than forgetting the frame (which would make [`diff_frame`] emit a
    /// `\x1b[2J` whole-screen clear on the next paint — a visible flash), this
    /// blanks every remembered row while keeping the row count: each blanked row
    /// then differs from the new content and is rewritten with its own per-row
    /// clear (`\x1b[2K`), and a shorter new frame still clears the rows it no
    /// longer fills. The result is a flicker-free full repaint that overwrites
    /// whatever was on screen row by row.
    pub fn reset(&mut self) {
        for line in &mut self.prev {
            line.clear();
        }
    }

    /// Like [`reset`](Self::reset) (a flicker-free full repaint on the next
    /// paint), but also queue `prefix` to be written at the front of that next
    /// flush, ahead of the diff.
    ///
    /// Called when the embedded pane hands control back: the alternate-screen and
    /// mouse-mode re-assertion (and the pointer reset) go here rather than being
    /// written and flushed on their own. Re-entering the alternate screen
    /// (`\x1b[?1049h`) clears it, so a standalone flush would show a blank screen
    /// for the frame between it and the event loop's next repaint — the black
    /// flash. Riding the same write as the forced full repaint below makes the
    /// clear and the redraw land in one terminal update, so nothing blank is ever
    /// shown.
    pub fn reset_with_prefix(&mut self, prefix: String) {
        self.reset();
        self.prefix = prefix;
    }

    /// Draw `frame` (overlaying any in-flight install), rewriting only the rows
    /// that changed since the previous paint, then remember it for the next diff.
    pub fn paint(&mut self, term: &Term, frame: Vec<String>) -> Result<()> {
        self.base = frame;
        self.columns = None;
        self.flush(term)
    }

    /// Draw `frame` with a column-aware diff for the given body region. Rows
    /// outside the region (header, input/footer, and full-screen/modal overlays)
    /// still use the normal row diff, while body rows are diffed as
    /// left/separator/right fixed-width cells so a sidebar-only change never
    /// rewrites the right pane (and vice versa).
    pub(crate) fn paint_columns(
        &mut self,
        term: &Term,
        frame: Vec<String>,
        columns: ColumnDiff,
    ) -> Result<()> {
        self.base = frame;
        self.columns = Some(columns);
        self.flush(term)
    }

    /// Re-apply the install overlay to the last painted frame and repaint. Called
    /// while waiting on a key so the overlay's time-based animation keeps moving
    /// even when nothing else on screen changed.
    pub fn tick(&mut self, term: &Term) -> Result<()> {
        self.flush(term)
    }

    /// Discard the diff base when the terminal size changed since the last
    /// flush. A resize reflows / truncates what is actually on screen, so the
    /// remembered frame no longer matches reality: diffing against it would
    /// skip "unchanged" rows and leave the disturbed content in place.
    /// Forgetting the frame entirely — not just blanking its rows, as
    /// [`reset`](Self::reset) does — makes the next diff clear the whole screen
    /// (`\x1b[2J`) before redrawing every row: the flicker-free per-row path is
    /// pointless here (the resize already disturbed the screen), and only a
    /// whole-screen clear also wipes reflow leftovers outside the new frame.
    fn invalidate_on_resize(&mut self, size: (u16, u16)) {
        if self.last_size.is_some_and(|last| last != size) {
            self.prev.clear();
        }
        self.last_size = Some(size);
    }

    /// Overlay the global install (if any) onto the base frame and diff-paint it.
    fn flush(&mut self, term: &Term) -> Result<()> {
        let size = term.size();
        // A resize since the last flush invalidates everything on screen: drop
        // the diff base so this paint is a full clear + redraw at the new size.
        self.invalidate_on_resize(size);
        let (_, width) = size;
        // Compose into the reused scratch buffer rather than a fresh clone: copy
        // the base into it (reusing its rows' allocations) and overlay any install
        // on top. `prev` is still untouched, so it remains the correct diff base.
        self.scratch.clone_from(&self.base);
        install_task::overlay(
            &mut self.scratch,
            width as usize,
            install_task::snapshot().as_ref(),
        );
        // A text-input screen marks its caret column with a zero-width sentinel
        // ([`widgets::CARET_MARK`]); pull it out (and strip it from every row)
        // before diffing so the marker never reaches the terminal and the
        // diff/`prev` stay clean.
        let caret = take_caret(&mut self.scratch);
        // Emit any queued prefix (a mode re-assertion after the embedded pane)
        // ahead of the diff, so it rides this same terminal write instead of a
        // separate flush. Drained here so it is written exactly once.
        let mut out = std::mem::take(&mut self.prefix);
        out.push_str(&diff_frame_with_columns(
            &self.prev,
            &self.scratch,
            self.columns,
        ));
        // Park the real cursor over the caret (showing it) so an OS IME draws its
        // preedit text in the input field rather than wherever the cursor was left
        // — otherwise composing Japanese surfaces at the bottom of the screen. With
        // no input field the cursor stays hidden, as `diff_frame` left it.
        if let Some((row, col)) = caret {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
        }
        term.write_str(&out)?;
        term.flush()?;
        // The scratch is now the painted frame: make it `prev` for the next diff
        // and reclaim the old `prev` as the next scratch, so neither is reallocated.
        std::mem::swap(&mut self.prev, &mut self.scratch);
        Ok(())
    }
}

/// Finds the caret marker ([`widgets::CARET_MARK`]) a text-input screen embedded
/// in its frame, returning the `(row, col)` of the caret's display column and
/// stripping the marker from every row so it never reaches the terminal.
///
/// The column is the display width of the row's content up to the marker, so a
/// caret sitting after full-width (CJK) text parks at the right place. Only the
/// first marker positions the cursor; any others are still stripped defensively.
fn take_caret(frame: &mut [String]) -> Option<(usize, usize)> {
    let mut caret = None;
    for (row, line) in frame.iter_mut().enumerate() {
        let Some(idx) = line.find(widgets::CARET_MARK) else {
            continue;
        };
        if caret.is_none() {
            caret = Some((row, console::measure_text_width(&line[..idx])));
        }
        *line = line.replace(widgets::CARET_MARK, "");
    }
    caret
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
#[cfg(test)]
fn diff_frame(prev: &[String], frame: &[String]) -> String {
    diff_frame_with_columns(prev, frame, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnDiff {
    pub row_start: usize,
    pub row_count: usize,
    pub left_width: usize,
    pub separator_width: usize,
    pub right_width: usize,
}

impl ColumnDiff {
    fn contains_row(self, row: usize) -> bool {
        row >= self.row_start && row < self.row_start.saturating_add(self.row_count)
    }
}

pub(crate) fn diff_frame_with_columns(
    prev: &[String],
    frame: &[String],
    columns: Option<ColumnDiff>,
) -> String {
    let fresh = prev.is_empty();
    // Hide the cursor while repainting so it does not flicker across the rows.
    let mut buf = String::from("\x1b[?25l");
    if fresh {
        // Nothing remembered: clear whatever another screen left behind.
        buf.push_str("\x1b[2J");
    }
    for (row, line) in frame.iter().enumerate() {
        if let Some(columns) = columns.filter(|columns| columns.contains_row(row)) {
            diff_column_row(&mut buf, fresh, row, prev.get(row), line, columns);
            continue;
        }
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

fn diff_column_row(
    buf: &mut String,
    fresh: bool,
    row: usize,
    prev: Option<&String>,
    line: &str,
    columns: ColumnDiff,
) {
    let left = fixed_segment(line, 0, columns.left_width);
    let separator_col = columns.left_width;
    let separator = fixed_segment(line, separator_col, columns.separator_width);
    let right_col = separator_col + columns.separator_width;
    let right = fixed_segment(line, right_col, columns.right_width);

    let prev_left = prev.map(|line| fixed_segment(line, 0, columns.left_width));
    let prev_separator =
        prev.map(|line| fixed_segment(line, separator_col, columns.separator_width));
    let prev_right = prev.map(|line| fixed_segment(line, right_col, columns.right_width));

    write_segment(
        buf,
        row,
        0,
        columns.left_width,
        &left,
        fresh || prev_left.as_ref() != Some(&left),
    );
    write_segment(
        buf,
        row,
        separator_col,
        columns.separator_width,
        &separator,
        fresh || prev_separator.as_ref() != Some(&separator),
    );
    write_segment(
        buf,
        row,
        right_col,
        columns.right_width,
        &right,
        fresh || prev_right.as_ref() != Some(&right),
    );
}

fn write_segment(
    buf: &mut String,
    row: usize,
    col: usize,
    width: usize,
    segment: &str,
    changed: bool,
) {
    if !changed || width == 0 {
        return;
    }
    // Move to a 1-based row/column and overwrite exactly this segment's fixed
    // width. Do not clear-to-EOL: that would erase the neighbouring pane.
    let _ = write!(buf, "\x1b[{};{}H", row + 1, col + 1);
    buf.push_str(segment);
}

fn fixed_segment(line: &str, start: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut clipped = if start == 0 {
        widgets::truncate_to_width(line, width)
    } else {
        widgets::truncate_to_width(&widgets::slice_from_width(line, start), width)
    };
    if clipped.contains('\u{1b}') {
        clipped.push_str("\u{1b}[0m");
    }
    pad_to_width(clipped, width)
}

fn pad_to_width(mut content: String, width: usize) -> String {
    let visible = console::measure_text_width(&content);
    if visible < width {
        content.extend(std::iter::repeat_n(' ', width - visible));
    }
    content
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
    fn terminal_restore_reuses_the_leave_and_mouse_disable_sequences() {
        // The abrupt-exit restore must undo exactly what the guard set up: it
        // starts by leaving the alternate screen and includes the mouse-disable
        // sequence, so it stays in step with LEAVE_ALT_SCREEN / DISABLE_MOUSE (a
        // change to either must be reflected here). It then clears bracketed
        // paste (?2004l) and shows the cursor (?25h).
        let restore = std::str::from_utf8(TERMINAL_RESTORE).unwrap();
        assert!(restore.starts_with(LEAVE_ALT_SCREEN));
        assert!(restore.contains(DISABLE_MOUSE));
        assert_eq!(
            TERMINAL_RESTORE,
            b"\x1b[?1049l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?25h"
        );
    }

    #[test]
    fn write_terminal_restore_writes_the_full_sequence_and_flushes() {
        // The panic hook shares this with the signal handlers' raw-fd write; the
        // bytes must be exactly TERMINAL_RESTORE.
        let mut out = Vec::new();
        write_terminal_restore(&mut out).unwrap();
        assert_eq!(out, TERMINAL_RESTORE);
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
    fn take_caret_finds_the_marked_column_by_display_width_and_strips_it() {
        // The caret column is the *display* width before the marker, so a caret
        // after full-width (CJK) text parks at the right place; the marker is
        // removed so it never reaches the terminal.
        let mut frame = lines(&["", &format!("❯ あい{}う", widgets::CARET_MARK)]);
        // "❯ あい" is 1 + 1 + 2 + 2 = 6 display columns.
        assert_eq!(take_caret(&mut frame), Some((1, 6)));
        assert_eq!(frame[1], "❯ あいう");
    }

    #[test]
    fn take_caret_is_none_and_a_no_op_without_a_marker() {
        let mut frame = lines(&["plain", "rows"]);
        assert_eq!(take_caret(&mut frame), None);
        assert_eq!(frame, lines(&["plain", "rows"]));
    }

    #[test]
    fn take_caret_strips_every_marker_but_parks_on_the_first() {
        // Only the first marker positions the cursor, but any stray markers are
        // still scrubbed so none leak to the terminal.
        let m = widgets::CARET_MARK;
        let mut frame = lines(&[&format!("a{m}"), &format!("bb{m}")]);
        assert_eq!(take_caret(&mut frame), Some((0, 1)));
        assert_eq!(frame, lines(&["a", "bb"]));
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
    fn diff_frame_with_columns_rewrites_only_the_changed_left_cell() {
        let columns = ColumnDiff {
            row_start: 0,
            row_count: 1,
            left_width: 4,
            separator_width: 3,
            right_width: 5,
        };
        let prev = lines(&["left │ right"]);
        let frame = lines(&["LEFT │ right"]);

        let out = diff_frame_with_columns(&prev, &frame, Some(columns));

        assert!(!out.contains("\x1b[2K"), "column diff must not clear rows");
        assert!(out.contains("\x1b[1;1HLEFT"));
        assert!(!out.contains("\x1b[1;5H"), "separator is unchanged");
        assert!(
            !out.contains("\x1b[1;8H"),
            "right pane is unchanged and must not be rewritten: {out:?}"
        );
    }

    #[test]
    fn diff_frame_with_columns_rewrites_only_the_changed_right_cell() {
        let columns = ColumnDiff {
            row_start: 0,
            row_count: 1,
            left_width: 4,
            separator_width: 3,
            right_width: 5,
        };
        let prev = lines(&["left │ right"]);
        let frame = lines(&["left │ RIGHT"]);

        let out = diff_frame_with_columns(&prev, &frame, Some(columns));

        assert!(!out.contains("\x1b[2K"), "column diff must not clear rows");
        assert!(!out.contains("\x1b[1;1H"), "left pane is unchanged");
        assert!(!out.contains("\x1b[1;5H"), "separator is unchanged");
        assert!(out.contains("\x1b[1;8HRIGHT"));
    }

    #[test]
    fn diff_frame_with_columns_still_row_diffs_outside_body_region() {
        let columns = ColumnDiff {
            row_start: 1,
            row_count: 1,
            left_width: 4,
            separator_width: 3,
            right_width: 5,
        };
        let prev = lines(&["old header", "left │ right"]);
        let frame = lines(&["new header", "left │ RIGHT"]);

        let out = diff_frame_with_columns(&prev, &frame, Some(columns));

        assert!(out.contains("\x1b[1;1H\x1b[2Knew header"));
        assert!(out.contains("\x1b[2;8HRIGHT"));
        assert!(!out.contains("\x1b[2;1H"), "body left cell is unchanged");
    }

    #[test]
    fn diff_frame_with_columns_skips_zero_width_segments() {
        let columns = ColumnDiff {
            row_start: 0,
            row_count: 1,
            left_width: 0,
            separator_width: 3,
            right_width: 5,
        };
        let prev = lines(&[" │ right"]);
        let frame = lines(&[" │ RIGHT"]);

        let out = diff_frame_with_columns(&prev, &frame, Some(columns));

        assert!(!out.contains("\x1b[1;1H"));
        assert!(out.contains("\x1b[1;4HRIGHT"));
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
    fn default_read_input_timeout_wraps_the_key_timeout() {
        // The default surfaces each timed read as a key, so a stub that only scripts
        // keys keeps working when the loop reads inputs (clicks come only from the
        // real terminal reader, which overrides this).
        let mut reader = OneKey(Key::Char('z'));
        assert_eq!(
            reader.read_input_timeout(Duration::ZERO).unwrap(),
            Some(Input::Key(Key::Char('z')))
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
    fn animated_read_surfaces_a_resize_while_animating_as_unknown() {
        // A resize during the animated wait must not just re-tick the old-size
        // frame: it returns the resize artefact (`Key::Unknown`) so the caller's
        // loop re-renders at the new size without waiting for a real key.
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        let handle = InstallHandle::new();
        handle.begin_at("m", Instant::now());
        let mut reader = TickReader {
            timeouts: std::collections::VecDeque::from(vec![Ok(None)]),
            blocking: Default::default(),
        };
        let mut sizes = std::collections::VecDeque::from(vec![(24u16, 80u16)]);
        let mut size = move || sizes.pop_front().unwrap_or((30, 100));
        let key =
            animated_read_sized(&mut reader, &term, &mut painter, &handle, &mut size).unwrap();
        assert_eq!(key, Key::Unknown);
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
        // After a reset the remembered rows are blanked (not forgotten): the row
        // count is kept so the repaint clears row by row rather than whole-screen.
        painter.reset();
        assert_eq!(painter.prev, lines(&["", ""]));
        // The forced repaint rewrites every row (each differs from the blank) and
        // emits NO whole-screen clear — the flicker-free path.
        let out = diff_frame(&painter.prev, &lines(&["a", "b"]));
        assert!(!out.contains("\x1b[2J"));
        assert!(out.contains("\x1b[1;1H\x1b[2Ka"));
        assert!(out.contains("\x1b[2;1H\x1b[2Kb"));
        painter.paint(&term, lines(&["a", "b"])).unwrap();
        assert_eq!(painter.prev, lines(&["a", "b"]));
    }

    #[test]
    fn a_resize_between_flushes_discards_the_diff_base() {
        let mut painter = FramePainter::new();
        painter.prev = lines(&["old", "frame"]);
        // The first flush records the size without touching the base…
        painter.invalidate_on_resize((24, 80));
        assert_eq!(painter.prev, lines(&["old", "frame"]));
        // …and so does a flush at the same size.
        painter.invalidate_on_resize((24, 80));
        assert_eq!(painter.prev, lines(&["old", "frame"]));
        // A resize forgets the frame entirely (not just blanking rows): the
        // next diff then clears the whole screen and redraws every row,
        // wiping whatever the terminal's reflow left behind.
        painter.invalidate_on_resize((30, 100));
        assert!(painter.prev.is_empty());
        assert_eq!(painter.last_size, Some((30, 100)));
        let out = diff_frame(&painter.prev, &lines(&["new"]));
        assert!(out.contains("\x1b[2J"));
        assert!(out.contains("\x1b[1;1H\x1b[2Knew"));
    }

    #[test]
    fn reset_with_prefix_keeps_the_flicker_free_repaint_base() {
        let mut painter = FramePainter::new();
        painter.prev = lines(&["old", "frame"]);

        painter.reset_with_prefix("prefix".to_string());

        // It is still the reset path: rows are blanked, not forgotten, so the next
        // diff rewrites row-by-row instead of emitting a whole-screen clear.
        assert_eq!(painter.prev, lines(&["", ""]));
        assert_eq!(painter.prefix, "prefix");
        let out = diff_frame(&painter.prev, &lines(&["new", "frame"]));
        assert!(!out.contains("\x1b[2J"));
        assert!(out.contains("\x1b[1;1H\x1b[2Knew"));
        assert!(out.contains("\x1b[2;1H\x1b[2Kframe"));
    }
}
