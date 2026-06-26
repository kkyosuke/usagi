//! Pure input translation for the embedded terminal pane (没入).
//!
//! Everything that turns a key / mouse event into a decision — which reserved
//! chord it is, how far the history should scroll, which grid cell the pointer
//! is over, and the raw bytes a key or a paste becomes for the shell — is pure
//! and lives here, unit tested. The (coverage-excluded) [`pane`] drive
//! loop only does the real terminal I/O and calls these.
//!
//! The **reserved keys** are `Ctrl-O`, `Ctrl-N`/`Ctrl-P`, `Ctrl-T`/`Ctrl-G`,
//! `Ctrl-^`, `Ctrl-B`, `Ctrl-E`, and `Ctrl-Q`; everything else, including `Esc`
//! **and `Ctrl-W`** (the universal shell "delete previous word"), flows to the
//! shell.
//!
//! [`pane`]: super::terminal::pane

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use vt100::MouseProtocolEncoding;

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

/// Whether this key is the reserved `Ctrl-O` leader (zoom out one engagement
/// level), as the raw `0x0f` (SI) char or `'o'` + `CONTROL`.
pub(super) fn is_leader(key: &KeyEvent) -> bool {
    chord(key, '\u{0f}', 'o')
}

/// Whether this key is `Ctrl` + the arrow `code`. The home-screen surfaces move
/// tabs with the bare arrows (via `console`, which can't see the modifier), so
/// 没入 adds the `Ctrl` qualifier to keep plain arrows flowing to the shell.
fn is_ctrl_arrow(key: &KeyEvent, code: KeyCode) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == code
}

/// Whether this key moves to the next tab: `Ctrl-N` (the raw `0x0e` (SO) char or
/// `'n'` + `CONTROL`) or `Ctrl-→`.
pub(super) fn is_next_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{0e}', 'n') || is_ctrl_arrow(key, KeyCode::Right)
}

/// Whether this key is `Ctrl-E` (open the note editor), as the raw `0x05` (ENQ)
/// char or `'e'` + `CONTROL`.
pub(super) fn is_open_note(key: &KeyEvent) -> bool {
    chord(key, '\u{05}', 'e')
}

/// Whether this key moves to the previous tab: `Ctrl-P` (the raw `0x10` (DLE)
/// char or `'p'` + `CONTROL`) or `Ctrl-←`.
pub(super) fn is_prev_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{10}', 'p') || is_ctrl_arrow(key, KeyCode::Left)
}

/// Whether this key is `Ctrl-T` (zoom out to 在席 / Focus), as the raw `0x14`
/// (DC4) char or `'t'` + `CONTROL`.
pub(super) fn is_to_focus(key: &KeyEvent) -> bool {
    chord(key, '\u{14}', 't')
}

/// Whether this key is `Ctrl-G` (add an agent tab), as the raw `0x07` (BEL) char
/// or `'g'` + `CONTROL`.
pub(super) fn is_new_agent_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{07}', 'g')
}

/// Whether this key is `Ctrl-B` (toggle the left sidebar), as the raw `0x02`
/// (STX) char or `'b'` + `CONTROL`.
pub(super) fn is_toggle_sidebar(key: &KeyEvent) -> bool {
    chord(key, '\u{02}', 'b')
}

/// Whether this key is `Ctrl-^` (jump to the previously focused session), as the
/// raw `0x1e` (RS) char or `'^'` + `CONTROL`.
pub(super) fn is_prev_session(key: &KeyEvent) -> bool {
    chord(key, '\u{1e}', '^')
}

/// Whether this key is the copy shortcut (`Ctrl-C`). It only copies when a
/// selection is active; otherwise the caller forwards it to the shell as the
/// usual interrupt. `Ctrl+Shift+C` is left to the shell unchanged.
pub(super) fn is_copy(key: &KeyEvent) -> bool {
    key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c')
}

/// Whether this key is `Ctrl-Q` (quit usagi), as the raw `0x11` (DC1) char or
/// `'q'` + `CONTROL`. It is the dedicated global quit chord: 没入 claims it from
/// the shell/agent so quitting works without first zooming out, and the home loop
/// raises the quit-confirmation modal when the pane hands this back.
pub(super) fn is_quit(key: &KeyEvent) -> bool {
    chord(key, '\u{11}', 'q')
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

    #[test]
    fn is_leader_matches_both_forms_of_ctrl_o() {
        // crossterm's usual decoding: lowercase `'o'` + `CONTROL`.
        assert!(is_leader(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
        // Some terminals deliver the bare `0x0F` (SI) codepoint instead, with
        // no `CONTROL` modifier reported — still the leader, so it must not
        // reach the agent (where it would render as `?`).
        assert!(is_leader(&key(KeyCode::Char('\u{0f}'), KeyModifiers::NONE)));
        assert!(is_leader(&key(
            KeyCode::Char('\u{0f}'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn is_leader_leaves_ctrl_shift_o_alone() {
        // `Ctrl+Shift+O` is not the leader: it flows to the agent unchanged.
        assert!(!is_leader(&key(KeyCode::Char('O'), KeyModifiers::CONTROL)));
        assert!(!is_leader(&key(
            KeyCode::Char('O'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn is_leader_rejects_non_leader_keys() {
        // No `Ctrl` modifier, or a different letter, is not the leader.
        assert!(!is_leader(&key(KeyCode::Char('o'), KeyModifiers::NONE)));
        assert!(!is_leader(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn tab_chords_match_both_forms_and_reject_others() {
        // Ctrl-N (next) / Ctrl-P (prev): crossterm's `'n'`/`'p'` + CONTROL, and
        // the bare control char some terminals deliver instead (0x0e / 0x10).
        assert!(is_next_tab(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)));
        assert!(is_next_tab(&key(
            KeyCode::Char('\u{0e}'),
            KeyModifiers::NONE
        )));
        assert!(is_prev_tab(&key(KeyCode::Char('p'), KeyModifiers::CONTROL)));
        assert!(is_prev_tab(&key(
            KeyCode::Char('\u{10}'),
            KeyModifiers::NONE
        )));
        // Ctrl-→ (next) / Ctrl-← (prev) are accepted alongside the chords.
        assert!(is_next_tab(&key(KeyCode::Right, KeyModifiers::CONTROL)));
        assert!(is_prev_tab(&key(KeyCode::Left, KeyModifiers::CONTROL)));
        // Plain letters, the wrong modifier, and the other chord are rejected.
        assert!(!is_next_tab(&key(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert!(!is_prev_tab(&key(KeyCode::Char('p'), KeyModifiers::NONE)));
        assert!(!is_next_tab(&key(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_prev_tab(&key(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL
        )));
        // Bare arrows (no Ctrl) stay with the shell, and the axes don't cross.
        assert!(!is_next_tab(&key(KeyCode::Right, KeyModifiers::NONE)));
        assert!(!is_prev_tab(&key(KeyCode::Left, KeyModifiers::NONE)));
        assert!(!is_next_tab(&key(KeyCode::Left, KeyModifiers::CONTROL)));
        assert!(!is_prev_tab(&key(KeyCode::Right, KeyModifiers::CONTROL)));
    }

    #[test]
    fn pane_chords_match_both_forms_and_reject_others() {
        // Ctrl-T (zoom out to 在席) / Ctrl-G (add agent): crossterm's letter +
        // CONTROL, and the bare control char some terminals deliver instead
        // (0x14 / 0x07).
        assert!(is_to_focus(&key(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        assert!(is_to_focus(&key(
            KeyCode::Char('\u{14}'),
            KeyModifiers::NONE
        )));
        assert!(is_new_agent_tab(&key(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL
        )));
        assert!(is_new_agent_tab(&key(
            KeyCode::Char('\u{07}'),
            KeyModifiers::NONE
        )));
        // Plain letters and the wrong chord are rejected (they flow to the shell).
        assert!(!is_to_focus(&key(KeyCode::Char('t'), KeyModifiers::NONE)));
        assert!(!is_new_agent_tab(&key(
            KeyCode::Char('g'),
            KeyModifiers::NONE
        )));
        // Ctrl-W is no longer a pane chord: it flows to the shell ("delete previous
        // word") rather than closing a tab. Nothing here should treat it specially.
        assert!(!is_to_focus(&key(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_new_agent_tab(&key(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn is_toggle_sidebar_matches_both_forms_of_ctrl_b() {
        // crossterm's usual decoding, and the bare 0x02 (STX) some terminals send.
        assert!(is_toggle_sidebar(&key(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL
        )));
        assert!(is_toggle_sidebar(&key(
            KeyCode::Char('\u{02}'),
            KeyModifiers::NONE
        )));
        // A plain `b` or the wrong chord flows to the shell.
        assert!(!is_toggle_sidebar(&key(
            KeyCode::Char('b'),
            KeyModifiers::NONE
        )));
        assert!(!is_toggle_sidebar(&key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn is_open_note_matches_both_forms_of_ctrl_e() {
        // crossterm's usual decoding, and the bare 0x05 (ENQ) some terminals send.
        assert!(is_open_note(&key(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL
        )));
        assert!(is_open_note(&key(
            KeyCode::Char('\u{05}'),
            KeyModifiers::NONE
        )));
        // A plain letter, the wrong chord, or Ctrl+Shift+E flows to the shell.
        assert!(!is_open_note(&key(KeyCode::Char('e'), KeyModifiers::NONE)));
        assert!(!is_open_note(&key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_open_note(&key(
            KeyCode::Char('E'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
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
    fn is_quit_matches_both_forms_of_ctrl_q() {
        // crossterm's usual decoding, and the bare 0x11 (DC1) most terminals send.
        assert!(is_quit(&key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        assert!(is_quit(&key(KeyCode::Char('\u{11}'), KeyModifiers::NONE)));
        // A bare `q`, the wrong chord, or `Ctrl+Shift+Q` flows to the shell.
        assert!(!is_quit(&key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(!is_quit(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
        assert!(!is_quit(&key(
            KeyCode::Char('Q'),
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
