//! Pure input translation for the embedded terminal pane (没入).
//!
//! Everything that turns a key / mouse event into a decision — which reserved
//! chord it is, how far the history should scroll, which grid cell the pointer
//! is over, and the raw bytes a key or a paste becomes for the shell — is pure
//! and lives here, unit tested. The (coverage-excluded) [`pane`] drive
//! loop only does the real terminal I/O and calls these.
//!
//! Which keys the pane **reserves** for its own navigation (rather than
//! forwarding to the shell) depends on the configured [`KeyScheme`]:
//!
//! - [`KeyScheme::Prefix`] (default) reserves only the `Ctrl-O` leader; the
//!   action is the *next* key (`Ctrl-O o/a/n/p/g/e/s/x/q`, or `Ctrl-O →`/`←`).
//!   Every other Ctrl key — `Ctrl-E`, `Ctrl-N`/`Ctrl-P`, `Ctrl-T`, … — flows to
//!   the shell, and `Ctrl-O Ctrl-O` zooms out to 選択 just like `Ctrl-O o` (a
//!   control-char second key the IME never composes). A pending leader lapses
//!   after [`PREFIX_TIMEOUT`] (and is cleared by a mouse / paste event), so a
//!   forgotten `Ctrl-O` can't capture a later key.
//! - [`KeyScheme::Alt`] reserves a single `Alt`-chord per action
//!   (`Alt-o/a/g/e/s/x/q`, `Alt-→`/`←`) and claims **no** bare Ctrl key.
//!
//! `Ctrl-^` (previous session) is a direct key in both schemes, and `Ctrl-C`
//! copies while a selection is active. `Esc` and `Ctrl-W` (the universal shell
//! "delete previous word") always flow to the shell. The scheme-aware verdict is
//! [`classify`]; the drive loop only holds the prefix-pending bit.
//!
//! [`pane`]: super::terminal::pane

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use vt100::MouseProtocolEncoding;

use crate::domain::settings::KeyScheme;

use super::terminal::selection::Cell;
use super::ui;

/// How many lines one mouse-wheel turn scrolls the history.
const WHEEL_LINES: i32 = 3;

/// Bracketed-paste start / end markers (DECSET 2004). A program that requested
/// the mode treats everything between them as one paste.
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Translate an absolute mouse position (0-based screen `col`/`row`) to a cell
/// in the terminal pane's grid, or `None` when the pointer is outside the pane.
pub(super) fn pane_cell(col: u16, row: u16, geo: ui::TerminalGeometry) -> Option<Cell> {
    let rel_col = col.checked_sub(geo.origin_col)?;
    let rel_row = row.checked_sub(geo.origin_row)?;
    if rel_col >= geo.cols || rel_row >= geo.rows {
        return None;
    }
    Some(Cell::new(rel_row, rel_col))
}

/// The shape the host terminal should give the mouse pointer over the embedded
/// pane, set via the xterm pointer-shape control (OSC 22). The pane shows a text
/// caret over the selectable terminal grid and a hand over a clickable target (a
/// URL in the grid, a tab chip, a sidebar PR badge), and restores the default
/// pointer everywhere else. Terminals that do not implement OSC 22 ignore the
/// escape, so it is a no-op there rather than leaving a stray sequence on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerShape {
    /// The terminal's own default pointer (typically an arrow).
    Default,
    /// An I-beam text caret, shown over the selectable terminal grid.
    Text,
    /// A pointing hand, shown over a clickable target.
    Hand,
}

impl PointerShape {
    /// The OSC 22 escape that sets this pointer shape: `OSC 22 ; <name> ST`, using
    /// the CSS cursor names the control standardises (`text`, `pointer`) and an
    /// empty name to restore the default. `ST` (`ESC \`) terminates it, matching
    /// the OSC 52 clipboard write the pane already emits.
    pub(super) fn osc22(self) -> &'static str {
        match self {
            PointerShape::Default => "\x1b]22;\x1b\\",
            PointerShape::Text => "\x1b]22;text\x1b\\",
            PointerShape::Hand => "\x1b]22;pointer\x1b\\",
        }
    }
}

/// The pointer shape for the cell under the mouse: a hand over anything
/// `clickable` (a URL in the grid, a tab chip, a sidebar PR badge), a text caret
/// over the selectable terminal grid (`in_pane` with nothing clickable), and the
/// default pointer everywhere else. `clickable` wins over `in_pane`, so a URL
/// cell — which is both — shows the hand.
pub(super) fn pointer_shape(in_pane: bool, clickable: bool) -> PointerShape {
    if clickable {
        PointerShape::Hand
    } else if in_pane {
        PointerShape::Text
    } else {
        PointerShape::Default
    }
}

/// The history scroll a key requests, in lines (negative scrolls up toward older
/// output), or `None` for a key the shell should receive. `Shift` distinguishes
/// the scroll keys from the `PageUp`/`PageDown`/arrows the shell expects.
pub(super) fn key_scroll_lines(key: &KeyEvent, geo: ui::TerminalGeometry) -> Option<i32> {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    // A page keeps one row of overlap for context; at least one line.
    let page = (geo.rows as i32 - 1).max(1);
    match key.code {
        KeyCode::PageUp => Some(-page),
        KeyCode::PageDown => Some(page),
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        _ => None,
    }
}

/// How close two left clicks on the same session row must fall to count as a
/// double click — the threshold separating a single click (select / arm) from a
/// double click (confirm, like `Enter`). Shared by 選択's `overview_click` and the
/// 没入 pane so a sidebar double click confirms a row the same way whether or not a
/// session is attached.
pub(super) const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Whether a left click on session `row` at `now` completes a double click — a
/// previous click on the **same** row no more than `threshold` ago. Threads the
/// pending click through `last_click`: a completing click clears it (so a third
/// click starts a fresh sequence rather than re-confirming), and any other click
/// records `(row, now)` as the new pending one. The shared core of 選択's
/// `overview_click` and the 没入 sidebar double-click-to-switch.
pub(super) fn is_double_click(
    last_click: &mut Option<(usize, Instant)>,
    row: usize,
    now: Instant,
    threshold: Duration,
) -> bool {
    let doubled = matches!(
        *last_click,
        Some((prev, at)) if prev == row && now.duration_since(at) <= threshold
    );
    *last_click = if doubled { None } else { Some((row, now)) };
    doubled
}

/// The history scroll a mouse wheel turn requests, in lines, or `None` for a
/// non-wheel mouse event.
pub(super) fn wheel_delta(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollUp => Some(-WHEEL_LINES),
        MouseEventKind::ScrollDown => Some(WHEEL_LINES),
        _ => None,
    }
}

/// The arrow-key bytes a wheel notch of `delta` lines sends to an alternate-screen
/// program (alternate-scroll emulation): one arrow per line, `Up` for a scroll up
/// (`delta < 0`) and `Down` for a scroll down. The encoding follows the program's
/// cursor-key mode (DECCKM) — `ESC O A`/`ESC O B` in application mode (what an
/// agent's full-screen UI typically sets), `ESC [ A`/`ESC [ B` otherwise — so the
/// program reads them as the same arrows the wheel would feed it standalone.
pub(super) fn wheel_arrows(delta: i32, application_cursor: bool) -> String {
    let arrow = match (delta < 0, application_cursor) {
        (true, true) => "\x1bOA",
        (true, false) => "\x1b[A",
        (false, true) => "\x1bOB",
        (false, false) => "\x1b[B",
    };
    arrow.repeat(delta.unsigned_abs() as usize)
}

/// The bytes a wheel notch sends to a program that enabled mouse reporting
/// (DECSET 1000/1002/1003), so it scrolls its own viewport — exactly what the
/// wheel does over such a program in a standalone terminal. Some full-screen
/// agents (`claude`) draw into the **primary** buffer rather than the alternate
/// screen, so the [`wheel_arrows`] alternate-scroll path never fires for them;
/// forwarding the wheel as a real mouse report is what lets them scroll.
///
/// `up` is a scroll-up notch; `cell` is the 0-based pane-relative cell under the
/// pointer (reports are 1-based, so each axis is offset by one). One report per
/// notch — the program decides how far to scroll — matching real terminals. The
/// wire shape follows the program's chosen [`MouseProtocolEncoding`]:
///
/// - `Sgr`: `CSI < Cb ; Cx ; Cy M` (the `M` marks a press; wheel notches are
///   always reported as presses).
/// - `Default` (X10) / `Utf8`: `CSI M Cb Cx Cy` with every field offset by 32 —
///   differing only in how a field past 95 is emitted (one byte capped at 255
///   for `Default`, its UTF-8 encoding for `Utf8`).
///
/// `Cb` carries the 64 ("wheel") flag: 64 for a scroll up, 65 for a scroll down.
pub(super) fn encode_mouse_wheel(up: bool, cell: Cell, encoding: MouseProtocolEncoding) -> Vec<u8> {
    let button: u32 = if up { 64 } else { 65 };
    let col = cell.col as u32 + 1;
    let row = cell.row as u32 + 1;
    match encoding {
        MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{col};{row}M").into_bytes(),
        enc => {
            let mut bytes = b"\x1b[M".to_vec();
            for field in [button, col, row] {
                let v = field + 32;
                match char::from_u32(v) {
                    Some(c) if enc == MouseProtocolEncoding::Utf8 => {
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                    // X10: one printable byte per field, capped at the 255 a
                    // single byte can hold (panes never reach that width/height).
                    _ => bytes.push(v.min(255) as u8),
                }
            }
            bytes
        }
    }
}

/// Move the scrollback offset by `delta` lines (negative scrolls up toward
/// older output). The upper bound is enforced by `set_scrollback` on the next
/// redraw, so this only has to keep the offset from underflowing past the live
/// screen.
pub(super) fn apply_scroll(scrollback: &mut usize, delta: i32) {
    *scrollback = if delta < 0 {
        scrollback.saturating_add(delta.unsigned_abs() as usize)
    } else {
        scrollback.saturating_sub(delta as usize)
    };
}

/// Only forward real key presses (and auto-repeats), never key releases (which
/// some platforms / the kitty protocol report).
pub(super) fn is_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// Whether `key` is the Ctrl chord for `letter`, accepting both forms crossterm
/// may report — so a chord binds the same regardless of how the terminal /
/// keyboard protocol delivers it:
///
/// - `letter` + `CONTROL` — crossterm's usual decoding of `Ctrl-<letter>`.
/// - the bare control codepoint `raw` — some terminals/keyboard protocols
///   deliver the chord as the raw control char with no `CONTROL` modifier (this
///   is how `console` reports it on the other home-screen surfaces). The raw
///   control char only ever comes from that chord, so it is accepted regardless
///   of the reported modifiers — otherwise [`encode_key`] would forward the raw
///   byte to the agent, which renders the unprintable control char as a
///   `?`-like placeholder instead of acting on the chord.
///
/// `Ctrl+Shift+<letter>` is deliberately *not* matched: its `Char` is the
/// uppercase letter, so it flows through to the agent unchanged.
fn chord(key: &KeyEvent, raw: char, letter: char) -> bool {
    key.code == KeyCode::Char(raw)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(letter))
}

/// A reserved 没入 (Attached) navigation action — what the embedded pane handles
/// itself instead of forwarding the key to the shell / agent. Which key triggers
/// each one depends on the active [`KeyScheme`] (see [`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reserved {
    /// Zoom out to 選択 (Overview), leaving every pane alive in the pool.
    Detach,
    /// Open the Focus modal inside Closeup, the session's action menu / prompt.
    ToFocus,
    /// Switch to the next tab in place.
    NextTab,
    /// Switch to the previous tab in place.
    PrevTab,
    /// Move the active tab one slot to the right, keeping it active.
    SwapTabRight,
    /// Move the active tab one slot to the left, keeping it active.
    SwapTabLeft,
    /// Add a fresh agent tab without leaving 没入.
    NewAgentTab,
    /// Close the active tab in place, killing its shell. Mirrors 選択's `x`, so
    /// closing a tab no longer means zooming out first.
    CloseTab,
    /// Open the session-note editor over the pane.
    OpenNote,
    /// Collapse / expand the left session sidebar in place.
    ToggleSidebar,
    /// Jump to the previously focused session.
    PrevSession,
    /// Leave 没入 to quit usagi (raises the quit-confirmation modal).
    Quit,
}

/// What the pane should do with a key, given the active [`KeyScheme`] and (for
/// the prefix scheme) whether a leader press is pending. The decision is pure so
/// the coverage-excluded drive loop only has to hold the one-bit pending state
/// and act on the verdict; all the keymap logic is unit-tested here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyAction {
    /// Run this reserved navigation action; any pending prefix is cleared.
    Reserved(Reserved),
    /// (Prefix scheme) The leader was pressed with nothing pending — start
    /// waiting for the second key, swallowing the leader itself.
    BeginPrefix,
    /// Forward the key to the shell (via [`encode_key`]); clears any pending
    /// prefix.
    Forward,
    /// Swallow the key without acting (an unrecognised key right after the
    /// leader); clears any pending prefix.
    Swallow,
}

/// Whether this key is the `Ctrl-O` leader of the prefix scheme (the raw `0x0f`
/// (SI) char or `'o'` + `CONTROL`).
fn is_prefix(key: &KeyEvent) -> bool {
    chord(key, '\u{0f}', 'o')
}

/// How long a `Ctrl-O` leader press waits for its action key before it lapses.
/// Without it, a leader left pending (the user pressed `Ctrl-O` then got
/// distracted) would make the *next* key — pressed much later as a fresh command
/// — its action key: a later `Ctrl-O` would zoom out to 選択 by surprise, and a
/// plain key meant for the shell would be swallowed (or fire a chord). One second
/// is long enough to type the second key deliberately, short enough that a
/// forgotten prefix expires before it can capture a later press.
pub(super) const PREFIX_TIMEOUT: Duration = Duration::from_millis(1000);

/// Whether a leader pressed at `since` is still awaiting its action key at `now`
/// — i.e. within [`PREFIX_TIMEOUT`]. `None` (no leader pending) is never alive.
/// The drive loop stamps the leader press with the instant it arrived and reads
/// this back so a stale prefix lapses instead of swallowing an unrelated later
/// key (and so its footer hint clears). Pure so the keymap stays unit-tested;
/// the coverage-excluded loop only supplies the clock.
pub(super) fn prefix_alive(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|t| now.saturating_duration_since(t) < PREFIX_TIMEOUT)
}

/// Whether this key is `Ctrl-^` (jump to the previously focused session), as the
/// raw `0x1e` (RS) char or `'^'` + `CONTROL`. A dedicated direct key in *both*
/// schemes — `Ctrl-^` is rarely bound in shells, so it needs no prefix / `Alt`.
fn is_prev_session(key: &KeyEvent) -> bool {
    chord(key, '\u{1e}', '^')
}

/// Whether this key is `Ctrl+Shift+letter`. crossterm reports the chord as the
/// **uppercase** `Char` (Shift having already cased the letter) carrying the
/// `CONTROL` modifier — so matching the uppercase `Char` + `CONTROL` keeps it
/// distinct from the bare `Ctrl-<lowercase>` that still flows to the shell / the
/// prefix scheme. `letter` is given lowercase; its uppercase is what we match.
/// Terminals that cannot report Ctrl combined with Shift simply never produce
/// this, leaving the old `Ctrl-N`/`Ctrl-P` bytes untouched.
fn ctrl_shift_letter(key: &KeyEvent, letter: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char(letter.to_ascii_uppercase())
}

/// The reserved action a single `Alt`-chord triggers in the [`KeyScheme::Alt`]
/// scheme, or `None` for a key the shell should receive. Only `Alt` letters and
/// arrows readline does **not** bind by default are claimed — so `Alt-b`/`Alt-f`
/// (word motion), `Alt-d` (delete word), `Alt-t` (transpose), `Alt-n`/`Alt-p`
/// (history search) all still reach the shell.
fn alt_action(key: &KeyEvent) -> Option<Reserved> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('o') => Reserved::Detach,
        KeyCode::Char('a') => Reserved::ToFocus,
        KeyCode::Char('g') => Reserved::NewAgentTab,
        KeyCode::Char('e') => Reserved::OpenNote,
        KeyCode::Char('s') => Reserved::ToggleSidebar,
        KeyCode::Char('x') => Reserved::CloseTab,
        KeyCode::Char('q') => Reserved::Quit,
        KeyCode::Right => Reserved::NextTab,
        KeyCode::Left => Reserved::PrevTab,
        _ => return None,
    })
}

/// The reserved action the key *after* the `Ctrl-O` leader triggers in the
/// [`KeyScheme::Prefix`] scheme, or `None` for an unrecognised key (swallowed).
/// The second key is a plain letter / arrow with no modifier — the shell only
/// ever sees keys while no prefix is pending, so these never collide with it.
fn prefix_action(key: &KeyEvent) -> Option<Reserved> {
    Some(match key.code {
        KeyCode::Char('o') => Reserved::Detach,
        KeyCode::Char('a') => Reserved::ToFocus,
        KeyCode::Char('n') | KeyCode::Right => Reserved::NextTab,
        KeyCode::Char('p') | KeyCode::Left => Reserved::PrevTab,
        KeyCode::Char('g') => Reserved::NewAgentTab,
        KeyCode::Char('e') => Reserved::OpenNote,
        KeyCode::Char('s') => Reserved::ToggleSidebar,
        KeyCode::Char('x') => Reserved::CloseTab,
        KeyCode::Char('q') => Reserved::Quit,
        _ => return None,
    })
}

/// Classify a key in 没入 (Attached) under the active `scheme`, given whether a
/// prefix press is `pending` (always `false` in the `Alt` scheme). This is the
/// single source of truth for the 没入 keymap; the drive loop owns only the
/// `pending` bit and acts on the returned [`KeyAction`].
pub(super) fn classify(scheme: KeyScheme, pending: bool, key: &KeyEvent) -> KeyAction {
    // `Ctrl-^` jumps to the previous session in either scheme — a low-conflict
    // direct key, so it never needs the leader or an `Alt` modifier.
    if is_prev_session(key) {
        return KeyAction::Reserved(Reserved::PrevSession);
    }
    // `Ctrl+Shift+N/P` reorder the active tab in either scheme. These chords are
    // only available when the terminal reports Shift distinctly from Ctrl; when
    // it does not, the old `Ctrl-N/P` bytes keep flowing exactly as before.
    if ctrl_shift_letter(key, 'n') {
        return KeyAction::Reserved(Reserved::SwapTabRight);
    }
    if ctrl_shift_letter(key, 'p') {
        return KeyAction::Reserved(Reserved::SwapTabLeft);
    }
    match scheme {
        KeyScheme::Alt => match alt_action(key) {
            Some(action) => KeyAction::Reserved(action),
            None => KeyAction::Forward,
        },
        KeyScheme::Prefix if pending => {
            // `Ctrl-O Ctrl-O` zooms out to 選択, the same as `Ctrl-O o` — a second
            // leader is two control chars (never an `o` the IME composes into
            // kana), so 選択 stays reachable with a Japanese IME left on. (The
            // `alt` scheme keeps bare `Ctrl-O` flowing to the shell for those who
            // want its readline binding.)
            if is_prefix(key) {
                KeyAction::Reserved(Reserved::Detach)
            } else {
                match prefix_action(key) {
                    Some(action) => KeyAction::Reserved(action),
                    None => KeyAction::Swallow,
                }
            }
        }
        KeyScheme::Prefix => {
            if is_prefix(key) {
                KeyAction::BeginPrefix
            } else {
                KeyAction::Forward
            }
        }
    }
}

/// Whether this key is the copy shortcut (`Ctrl-C`). It only copies when a
/// selection is active; otherwise the caller forwards it to the shell as the
/// usual interrupt. `Ctrl+Shift+C` is left to the shell unchanged.
pub(super) fn is_copy(key: &KeyEvent) -> bool {
    key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c')
}

/// Encode a paste for the shell. When the running program asked for bracketed
/// paste (`bracketed`), wrap the text in the start/end markers so it lands as a
/// single block — the agent inserts the multi-line text rather than submitting
/// on each newline. Otherwise forward the raw bytes (the program never opted in,
/// so there is nothing to wrap).
///
/// In the bracketed case any [`PASTE_END`] marker the pasted text itself contains
/// is stripped first: leaving it in would let pasted content close the paste
/// early and have its tail run as live keystrokes (paste injection), so — like
/// real terminals — we neutralise the embedded terminator.
pub(super) fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let body = text.replace(PASTE_END, "");
    let mut bytes = Vec::with_capacity(PASTE_START.len() + body.len() + PASTE_END.len());
    bytes.extend_from_slice(PASTE_START.as_bytes());
    bytes.extend_from_slice(body.as_bytes());
    bytes.extend_from_slice(PASTE_END.as_bytes());
    bytes
}

/// Encode a prompt injected by MCP into a live agent pane.
///
/// The prompt itself is pasted (respecting the program's bracketed-paste mode
/// when enabled) and then submitted with a terminal Enter (`\r`). This mirrors
/// the user's "paste task, press Enter" workflow while keeping multi-line prompts
/// from turning into multiple submits for agents that request bracketed paste.
pub(super) fn encode_prompt_submit(prompt: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = encode_paste(prompt, bracketed);
    bytes.push(b'\r');
    bytes
}

/// Translate a key event into the bytes a shell expects on its input. Unknown
/// keys map to nothing (an empty slice), so they are simply dropped.
pub(super) fn encode_key(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) => {
            let mut bytes = Vec::new();
            if alt {
                bytes.push(0x1b);
            }
            if ctrl {
                // Control characters: map the letter to its 0x00–0x1f code.
                bytes.push((c.to_ascii_uppercase() as u8) & 0x1f);
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            bytes
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// A pane geometry with the given origin and size, for the pointer hit-test.
    fn geo(origin_col: u16, origin_row: u16, cols: u16, rows: u16) -> ui::TerminalGeometry {
        ui::TerminalGeometry {
            rows,
            cols,
            origin_col,
            origin_row,
        }
    }

    #[test]
    fn is_double_click_confirms_a_second_click_on_the_same_row_within_the_threshold() {
        let threshold = Duration::from_millis(400);
        let t0 = Instant::now();
        let mut last = None;
        // The first click only arms: it records the row and returns false.
        assert!(!is_double_click(&mut last, 3, t0, threshold));
        assert_eq!(last, Some((3, t0)));
        // A second click on the same row within the threshold confirms and clears
        // the pending click, so a third click starts a fresh sequence.
        let t1 = t0 + Duration::from_millis(200);
        assert!(is_double_click(&mut last, 3, t1, threshold));
        assert_eq!(last, None);
    }

    #[test]
    fn is_double_click_rearms_on_a_different_row_or_after_the_threshold() {
        let threshold = Duration::from_millis(400);
        let t0 = Instant::now();
        // A second click on a *different* row does not confirm; it re-arms on the
        // new row instead.
        let mut last = Some((3, t0));
        let t1 = t0 + Duration::from_millis(100);
        assert!(!is_double_click(&mut last, 4, t1, threshold));
        assert_eq!(last, Some((4, t1)));
        // A second click on the same row but past the threshold does not confirm;
        // it re-arms (its own click becomes the new pending one).
        let mut last = Some((3, t0));
        let t2 = t0 + Duration::from_millis(500);
        assert!(!is_double_click(&mut last, 3, t2, threshold));
        assert_eq!(last, Some((3, t2)));
    }

    #[test]
    fn pane_cell_maps_a_pointer_inside_the_pane_relative_to_its_origin() {
        let geo = geo(10, 3, 80, 24);
        // The pane's top-left corner maps to cell (0, 0).
        assert_eq!(pane_cell(10, 3, geo), Some(Cell::new(0, 0)));
        // An interior pointer maps relative to the origin.
        assert_eq!(pane_cell(15, 5, geo), Some(Cell::new(2, 5)));
        // The bottom-right corner is still inside.
        assert_eq!(pane_cell(89, 26, geo), Some(Cell::new(23, 79)));
    }

    #[test]
    fn pane_cell_rejects_a_pointer_outside_the_pane() {
        let geo = geo(10, 3, 80, 24);
        // Left of / above the origin underflow to `None`.
        assert_eq!(pane_cell(9, 5, geo), None);
        assert_eq!(pane_cell(15, 2, geo), None);
        // One column / row past the pane is outside.
        assert_eq!(pane_cell(90, 5, geo), None);
        assert_eq!(pane_cell(15, 27, geo), None);
    }

    #[test]
    fn pointer_shape_picks_hand_text_or_default() {
        // A clickable target (a URL / tab chip / PR badge) shows a hand — even
        // inside the grid, where a plain cell would be a text caret.
        assert_eq!(pointer_shape(true, true), PointerShape::Hand);
        assert_eq!(pointer_shape(false, true), PointerShape::Hand);
        // The selectable grid with nothing clickable under the pointer is a caret.
        assert_eq!(pointer_shape(true, false), PointerShape::Text);
        // Off the grid and off any target restores the default pointer.
        assert_eq!(pointer_shape(false, false), PointerShape::Default);
    }

    #[test]
    fn pointer_shape_osc22_sets_css_names_with_an_st_terminator() {
        // The standardised CSS cursor names, terminated by ST (`ESC \`).
        assert_eq!(PointerShape::Text.osc22(), "\x1b]22;text\x1b\\");
        assert_eq!(PointerShape::Hand.osc22(), "\x1b]22;pointer\x1b\\");
        // An empty name resets to the terminal's own pointer.
        assert_eq!(PointerShape::Default.osc22(), "\x1b]22;\x1b\\");
    }

    #[test]
    fn key_scroll_lines_pages_and_steps_only_with_shift() {
        let geo = geo(0, 0, 80, 24);
        let shift = KeyModifiers::SHIFT;
        // A page keeps one row of overlap for context (rows - 1).
        assert_eq!(
            key_scroll_lines(&key(KeyCode::PageUp, shift), geo),
            Some(-23)
        );
        assert_eq!(
            key_scroll_lines(&key(KeyCode::PageDown, shift), geo),
            Some(23)
        );
        // Arrows step one line.
        assert_eq!(key_scroll_lines(&key(KeyCode::Up, shift), geo), Some(-1));
        assert_eq!(key_scroll_lines(&key(KeyCode::Down, shift), geo), Some(1));
        // A non-scroll key with Shift is not a scroll.
        assert_eq!(key_scroll_lines(&key(KeyCode::Char('a'), shift), geo), None);
    }

    #[test]
    fn key_scroll_lines_without_shift_is_left_to_the_shell() {
        let geo = geo(0, 0, 80, 24);
        assert_eq!(
            key_scroll_lines(&key(KeyCode::PageUp, KeyModifiers::NONE), geo),
            None
        );
        assert_eq!(
            key_scroll_lines(&key(KeyCode::Up, KeyModifiers::NONE), geo),
            None
        );
    }

    #[test]
    fn key_scroll_lines_keeps_a_one_row_pane_paging_at_least_one_line() {
        // A degenerate one-row pane still pages a whole line, never zero.
        let geo = geo(0, 0, 80, 1);
        assert_eq!(
            key_scroll_lines(&key(KeyCode::PageUp, KeyModifiers::SHIFT), geo),
            Some(-1)
        );
    }

    #[test]
    fn wheel_delta_scrolls_three_lines_per_turn() {
        assert_eq!(wheel_delta(MouseEventKind::ScrollUp), Some(-3));
        assert_eq!(wheel_delta(MouseEventKind::ScrollDown), Some(3));
        // A non-wheel mouse event does not scroll.
        assert_eq!(wheel_delta(MouseEventKind::Moved), None);
    }

    #[test]
    fn wheel_arrows_sends_one_arrow_per_line_in_the_program_cursor_mode() {
        // A scroll up (negative delta) sends `Up` arrows, a scroll down `Down`,
        // one per line, and follows the program's DECCKM: `ESC O _` in application
        // cursor mode (what a full-screen agent UI sets), `ESC [ _` otherwise.
        assert_eq!(wheel_arrows(-WHEEL_LINES, false), "\x1b[A".repeat(3));
        assert_eq!(wheel_arrows(-WHEEL_LINES, true), "\x1bOA".repeat(3));
        assert_eq!(wheel_arrows(WHEEL_LINES, false), "\x1b[B".repeat(3));
        assert_eq!(wheel_arrows(WHEEL_LINES, true), "\x1bOB".repeat(3));
        // The count tracks the magnitude, so a single-line delta sends one arrow.
        assert_eq!(wheel_arrows(-1, false), "\x1b[A");
    }

    #[test]
    fn encode_mouse_wheel_sgr_reports_one_press_per_notch_at_the_one_based_cell() {
        // SGR: `CSI < Cb ; Cx ; Cy M`. The wheel flag is 64 (up) / 65 (down), the
        // cell is reported 1-based (the 0-based pane cell + 1 on each axis), and
        // the trailing `M` marks the press a wheel notch always reports as.
        assert_eq!(
            encode_mouse_wheel(true, Cell::new(4, 9), MouseProtocolEncoding::Sgr),
            b"\x1b[<64;10;5M"
        );
        assert_eq!(
            encode_mouse_wheel(false, Cell::new(0, 0), MouseProtocolEncoding::Sgr),
            b"\x1b[<65;1;1M"
        );
    }

    #[test]
    fn encode_mouse_wheel_x10_offsets_every_field_by_32() {
        // Default (X10): `CSI M Cb Cx Cy`, each field a single byte offset by 32.
        // Up at cell (0,0) → button 64+32=96 (0x60), col/row 1+32=33 (0x21).
        assert_eq!(
            encode_mouse_wheel(true, Cell::new(0, 0), MouseProtocolEncoding::Default),
            b"\x1b[M\x60\x21\x21"
        );
        // Down at cell (4,9) → button 65+32=97, col 10+32=42, row 5+32=37.
        assert_eq!(
            encode_mouse_wheel(false, Cell::new(4, 9), MouseProtocolEncoding::Default),
            b"\x1b[M\x61\x2a\x25"
        );
    }

    #[test]
    fn encode_mouse_wheel_x10_caps_a_far_field_at_one_byte() {
        // A column past 255-32 can't fit one byte; X10 saturates at 255 rather
        // than overflowing (real panes never get this wide, but the math is safe).
        let encoded = encode_mouse_wheel(true, Cell::new(0, 400), MouseProtocolEncoding::Default);
        assert_eq!(encoded, b"\x1b[M\x60\xff\x21");
    }

    #[test]
    fn encode_mouse_wheel_utf8_encodes_a_far_field_as_utf8() {
        // UTF-8 encoding emits a field past 95 as its UTF-8 sequence rather than a
        // raw byte. Col cell 200 → 201+32 = 233 ('é'), a two-byte UTF-8 char; the
        // small fields (button, row) stay single bytes, matching X10.
        let encoded = encode_mouse_wheel(true, Cell::new(0, 200), MouseProtocolEncoding::Utf8);
        let mut expected = b"\x1b[M\x60".to_vec();
        expected.extend_from_slice('é'.to_string().as_bytes());
        expected.push(0x21);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn apply_scroll_moves_up_and_down_and_saturates_at_the_live_screen() {
        let mut s = 5usize;
        // Negative scrolls up (further into history).
        apply_scroll(&mut s, -3);
        assert_eq!(s, 8);
        // Positive scrolls back down toward the live screen.
        apply_scroll(&mut s, 2);
        assert_eq!(s, 6);
        // Scrolling down past the live screen clamps at zero (no underflow).
        apply_scroll(&mut s, 100);
        assert_eq!(s, 0);
    }

    #[test]
    fn is_press_forwards_presses_and_repeats_but_not_releases() {
        let mut press = key(KeyCode::Char('a'), KeyModifiers::NONE);
        press.kind = KeyEventKind::Press;
        assert!(is_press(press));

        let mut repeat = key(KeyCode::Char('a'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert!(is_press(repeat));

        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!is_press(release));
    }

    /// `Alt` + `ch`, the shape crossterm reports an `Alt`-chord as.
    fn alt(ch: char) -> KeyEvent {
        key(KeyCode::Char(ch), KeyModifiers::ALT)
    }

    #[test]
    fn prefix_scheme_claims_only_the_leader_then_maps_the_second_key() {
        use KeyScheme::Prefix;
        // With nothing pending the leader (either reported form) begins a prefix
        // sequence and is swallowed; every other key flows to the shell.
        assert_eq!(
            classify(
                Prefix,
                false,
                &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            KeyAction::BeginPrefix
        );
        assert_eq!(
            classify(
                Prefix,
                false,
                &key(KeyCode::Char('\u{0f}'), KeyModifiers::NONE)
            ),
            KeyAction::BeginPrefix
        );
        // The conflicting bare-Ctrl keys are no longer claimed — they reach the
        // shell (the whole point of the prefix scheme). `Ctrl-X` is among them: a
        // readline prefix (`Ctrl-X Ctrl-E` …), so closing a tab is `Ctrl-O x`, not
        // a bare chord that would steal it.
        for ch in ['e', 'n', 'p', 't', 'g', 'b', 'x', 'q'] {
            assert_eq!(
                classify(
                    Prefix,
                    false,
                    &key(KeyCode::Char(ch), KeyModifiers::CONTROL)
                ),
                KeyAction::Forward,
                "Ctrl-{ch} must flow to the shell in the prefix scheme"
            );
        }
        // After the leader, each second key maps to its action.
        let second = |ch: char| classify(Prefix, true, &key(KeyCode::Char(ch), KeyModifiers::NONE));
        assert_eq!(second('o'), KeyAction::Reserved(Reserved::Detach));
        assert_eq!(second('a'), KeyAction::Reserved(Reserved::ToFocus));
        assert_eq!(second('n'), KeyAction::Reserved(Reserved::NextTab));
        assert_eq!(second('p'), KeyAction::Reserved(Reserved::PrevTab));
        assert_eq!(second('g'), KeyAction::Reserved(Reserved::NewAgentTab));
        assert_eq!(second('e'), KeyAction::Reserved(Reserved::OpenNote));
        assert_eq!(second('s'), KeyAction::Reserved(Reserved::ToggleSidebar));
        assert_eq!(second('x'), KeyAction::Reserved(Reserved::CloseTab));
        assert_eq!(second('q'), KeyAction::Reserved(Reserved::Quit));
        // Arrows after the leader move tabs too.
        assert_eq!(
            classify(Prefix, true, &key(KeyCode::Right, KeyModifiers::NONE)),
            KeyAction::Reserved(Reserved::NextTab)
        );
        assert_eq!(
            classify(Prefix, true, &key(KeyCode::Left, KeyModifiers::NONE)),
            KeyAction::Reserved(Reserved::PrevTab)
        );
    }

    #[test]
    fn ctrl_shift_n_and_p_reorder_tabs_in_both_schemes() {
        let ctrl = KeyModifiers::CONTROL;
        for scheme in KeyScheme::ALL {
            for pending in [false, true] {
                assert_eq!(
                    classify(scheme, pending, &key(KeyCode::Char('N'), ctrl)),
                    KeyAction::Reserved(Reserved::SwapTabRight)
                );
                assert_eq!(
                    classify(scheme, pending, &key(KeyCode::Char('P'), ctrl)),
                    KeyAction::Reserved(Reserved::SwapTabLeft)
                );
            }
            assert_eq!(
                classify(scheme, false, &key(KeyCode::Char('n'), ctrl)),
                KeyAction::Forward,
                "bare Ctrl-n stays unchanged; terminals without Ctrl+Shift reporting keep the old behaviour"
            );
        }
    }

    #[test]
    fn prefix_alive_only_within_the_timeout_window() {
        let t = Instant::now();
        // No leader pending is never alive.
        assert!(!prefix_alive(None, t));
        // Just pressed, and anywhere short of the timeout, is still alive.
        assert!(prefix_alive(Some(t), t));
        assert!(prefix_alive(
            Some(t),
            t + PREFIX_TIMEOUT - Duration::from_millis(1)
        ));
        // At or past the timeout the leader has lapsed.
        assert!(!prefix_alive(Some(t), t + PREFIX_TIMEOUT));
        assert!(!prefix_alive(
            Some(t),
            t + PREFIX_TIMEOUT + Duration::from_secs(5)
        ));
    }

    #[test]
    fn prefix_double_leader_zooms_to_overview_and_unknown_second_key_is_swallowed() {
        use KeyScheme::Prefix;
        // `Ctrl-O Ctrl-O` zooms out to 選択 like `Ctrl-O o`, in both control-char
        // forms crossterm may report (with `CONTROL`, or the raw `0x0f`) — so 選択
        // stays reachable with a Japanese IME left on, which would compose a plain
        // `o` into kana before usagi ever saw it.
        assert_eq!(
            classify(
                Prefix,
                true,
                &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            KeyAction::Reserved(Reserved::Detach)
        );
        assert_eq!(
            classify(
                Prefix,
                true,
                &key(KeyCode::Char('\u{0f}'), KeyModifiers::NONE)
            ),
            KeyAction::Reserved(Reserved::Detach)
        );
        // An unrecognised key right after the leader is swallowed (tmux-style),
        // not sent to the shell.
        assert_eq!(
            classify(Prefix, true, &key(KeyCode::Char('z'), KeyModifiers::NONE)),
            KeyAction::Swallow
        );
    }

    #[test]
    fn alt_scheme_claims_single_alt_chords_and_leaves_the_rest_to_the_shell() {
        use KeyScheme::Alt;
        // Each action is one Alt-chord; `pending` is irrelevant in this scheme.
        assert_eq!(
            classify(Alt, false, &alt('o')),
            KeyAction::Reserved(Reserved::Detach)
        );
        assert_eq!(
            classify(Alt, false, &alt('a')),
            KeyAction::Reserved(Reserved::ToFocus)
        );
        assert_eq!(
            classify(Alt, false, &alt('g')),
            KeyAction::Reserved(Reserved::NewAgentTab)
        );
        assert_eq!(
            classify(Alt, false, &alt('e')),
            KeyAction::Reserved(Reserved::OpenNote)
        );
        assert_eq!(
            classify(Alt, false, &alt('s')),
            KeyAction::Reserved(Reserved::ToggleSidebar)
        );
        assert_eq!(
            classify(Alt, false, &alt('x')),
            KeyAction::Reserved(Reserved::CloseTab)
        );
        assert_eq!(
            classify(Alt, false, &alt('q')),
            KeyAction::Reserved(Reserved::Quit)
        );
        assert_eq!(
            classify(Alt, false, &key(KeyCode::Right, KeyModifiers::ALT)),
            KeyAction::Reserved(Reserved::NextTab)
        );
        assert_eq!(
            classify(Alt, false, &key(KeyCode::Left, KeyModifiers::ALT)),
            KeyAction::Reserved(Reserved::PrevTab)
        );
        // No bare Ctrl key is claimed — `Ctrl-O` and friends reach the shell.
        assert_eq!(
            classify(Alt, false, &key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            KeyAction::Forward
        );
        // Alt chords readline binds (Alt-b/f/d/t/n/p) are deliberately NOT claimed.
        for ch in ['b', 'f', 'd', 't', 'n', 'p'] {
            assert_eq!(
                classify(Alt, false, &alt(ch)),
                KeyAction::Forward,
                "Alt-{ch} must reach the shell"
            );
        }
        // A plain key flows to the shell.
        assert_eq!(
            classify(Alt, false, &key(KeyCode::Char('x'), KeyModifiers::NONE)),
            KeyAction::Forward
        );
    }

    #[test]
    fn ctrl_caret_jumps_to_the_previous_session_in_both_schemes() {
        // `Ctrl-^` is a direct previous-session key in either scheme (and even
        // mid-prefix), reported as `'^'` + CONTROL or the bare 0x1e (RS) char.
        for scheme in KeyScheme::ALL {
            for pending in [false, true] {
                assert_eq!(
                    classify(
                        scheme,
                        pending,
                        &key(KeyCode::Char('^'), KeyModifiers::CONTROL)
                    ),
                    KeyAction::Reserved(Reserved::PrevSession)
                );
                assert_eq!(
                    classify(
                        scheme,
                        pending,
                        &key(KeyCode::Char('\u{1e}'), KeyModifiers::NONE)
                    ),
                    KeyAction::Reserved(Reserved::PrevSession)
                );
            }
        }
    }

    #[test]
    fn is_prev_session_matches_both_forms_of_ctrl_caret() {
        // crossterm's usual decoding, and the bare 0x1e (RS) most terminals send.
        assert!(is_prev_session(&key(
            KeyCode::Char('^'),
            KeyModifiers::CONTROL
        )));
        assert!(is_prev_session(&key(
            KeyCode::Char('\u{1e}'),
            KeyModifiers::NONE
        )));
        // A plain caret or the wrong chord flows to the shell.
        assert!(!is_prev_session(&key(
            KeyCode::Char('^'),
            KeyModifiers::NONE
        )));
        assert!(!is_prev_session(&key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn is_copy_matches_only_plain_ctrl_c() {
        // `Ctrl-C` is the copy shortcut (only meaningful with a selection).
        assert!(is_copy(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        // A bare `c`, a different letter, or `Ctrl+Shift+C` is not the shortcut
        // and flows to the shell unchanged.
        assert!(!is_copy(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_copy(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
        assert!(!is_copy(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn encode_paste_passes_raw_bytes_when_not_bracketed() {
        // No bracketed-paste mode: forward the text unwrapped, verbatim.
        assert_eq!(encode_paste("ls -la\n", false), b"ls -la\n".to_vec());
    }

    #[test]
    fn encode_paste_wraps_in_bracketed_markers() {
        assert_eq!(
            encode_paste("hi", true),
            [PASTE_START, "hi", PASTE_END].concat().into_bytes(),
        );
    }

    #[test]
    fn encode_paste_strips_an_embedded_end_marker() {
        // Paste-injection guard: an end marker inside the pasted text would
        // otherwise close the paste early and run its tail as live keystrokes.
        let malicious = format!("safe{PASTE_END}rm -rf ~\n");
        let encoded = encode_paste(&malicious, true);
        let expected = [PASTE_START, "saferm -rf ~\n", PASTE_END]
            .concat()
            .into_bytes();
        assert_eq!(encoded, expected);
        // The terminator appears exactly once — only the wrapper's own trailer.
        let needle = PASTE_END.as_bytes();
        let hits = encoded
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count();
        assert_eq!(hits, 1);
    }

    #[test]
    fn encode_prompt_submit_pastes_then_enters_without_bracketed_mode() {
        assert_eq!(
            encode_prompt_submit("do this\nwith context", false),
            b"do this\nwith context\r".to_vec()
        );
    }

    #[test]
    fn encode_prompt_submit_wraps_bracketed_prompt_before_enter() {
        assert_eq!(
            encode_prompt_submit("hi", true),
            [PASTE_START, "hi", PASTE_END, "\r"].concat().into_bytes()
        );
    }

    #[test]
    fn encode_key_maps_plain_and_modified_chars() {
        // A plain char is its UTF-8 bytes (multi-byte chars included).
        assert_eq!(
            encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            b"a"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('あ'), KeyModifiers::NONE)),
            "あ".as_bytes()
        );
        // Ctrl maps the letter to its 0x00–0x1f control code (Ctrl-C → 0x03),
        // case-insensitively.
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        // Alt prefixes an ESC (meta).
        assert_eq!(
            encode_key(&key(KeyCode::Char('b'), KeyModifiers::ALT)),
            vec![0x1b, b'b']
        );
    }

    #[test]
    fn encode_key_maps_the_named_keys_to_their_escape_sequences() {
        assert_eq!(encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
        assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE)), b"\t");
        assert_eq!(
            encode_key(&key(KeyCode::BackTab, KeyModifiers::NONE)),
            b"\x1b[Z"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE)),
            vec![0x7f]
        );
        assert_eq!(
            encode_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
            vec![0x1b]
        );
        assert_eq!(
            encode_key(&key(KeyCode::Left, KeyModifiers::NONE)),
            b"\x1b[D"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Right, KeyModifiers::NONE)),
            b"\x1b[C"
        );
        assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE)), b"\x1b[A");
        assert_eq!(
            encode_key(&key(KeyCode::Down, KeyModifiers::NONE)),
            b"\x1b[B"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Home, KeyModifiers::NONE)),
            b"\x1b[H"
        );
        assert_eq!(
            encode_key(&key(KeyCode::End, KeyModifiers::NONE)),
            b"\x1b[F"
        );
        assert_eq!(
            encode_key(&key(KeyCode::PageUp, KeyModifiers::NONE)),
            b"\x1b[5~"
        );
        assert_eq!(
            encode_key(&key(KeyCode::PageDown, KeyModifiers::NONE)),
            b"\x1b[6~"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Insert, KeyModifiers::NONE)),
            b"\x1b[2~"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Delete, KeyModifiers::NONE)),
            b"\x1b[3~"
        );
    }

    #[test]
    fn encode_key_drops_keys_it_does_not_map() {
        // An unmapped key (e.g. a function key) produces no bytes.
        assert!(encode_key(&key(KeyCode::F(1), KeyModifiers::NONE)).is_empty());
    }
}
