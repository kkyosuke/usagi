//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal`, the right pane switches to a live shell drawn
//! into the right pane while the whole workspace frame — sidebar and all — stays
//! on screen. The shell itself is owned by the [`TerminalPool`] (so it survives
//! a detach); this module borrows it and runs the render/input loop. Keystrokes
//! are forwarded to the shell as raw bytes.
//!
//! `Ctrl-O` opens the **session picker**, so the user can switch sessions
//! without losing the shell:
//!
//! - `Ctrl-O` overlays a list of every session (the root plus each worktree);
//!   `1`–`9` or `↑`/`↓` + `Enter` switches the pane to that session's terminal,
//!   staying focused. The shell just left keeps running in the pool.
//! - `Esc` closes the picker and resumes the current shell.
//! - `Ctrl-O` again **detaches** — the pane returns to the sidebar but the shell
//!   stays alive in the pool.
//!
//! Each outcome is reported to the event loop as a [`PaneExit`]; the shell
//! exiting on its own reports [`PaneExit::Closed`].
//!
//! `agent` reuses the same machinery: the pool sends the configured agent CLI to
//! the shell on first spawn, so the pane lands the user straight in the agent.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the layout geometry and frame ([`super::ui`]), the screen
//! snapshot ([`super::terminal_view`]), and the [`PaneExit`] vocabulary
//! ([`super::state`]).
//!
//! [`TerminalPool`]: super::terminal_pool::TerminalPool

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use console::Term;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;
use crate::presentation::tui::screen::diff_frame;

use super::state::{HomeState, PaneExit};
use super::terminal_pool::MonitorHandle;
use super::terminal_view::TerminalView;
use super::ui;

/// How finely the loop samples for fresh shell output while it waits for a
/// keystroke. Output, and the echo of typed keys, appear within this slice — so
/// the pane stays responsive instead of trailing a fixed redraw timer.
const POLL_SLICE: Duration = Duration::from_millis(4);

/// The longest the loop sits idle before redrawing anyway. Output and key
/// presses wake it far sooner; this is only a safety net so a terminal resize
/// is eventually noticed even while nothing else is happening.
const IDLE_REDRAW: Duration = Duration::from_millis(100);

/// How many lines one wheel notch scrolls the embedded terminal's history.
const WHEEL_LINES: i32 = 3;

/// Run the embedded terminal in the right pane, driving the pooled shell `pty`
/// until the user detaches / switches (`Ctrl-O`) or the shell exits. The PTY is
/// owned by the caller's [`TerminalPool`] so it survives a detach; here we own
/// only raw mode and the render/input loop. The reason for returning is the
/// [`PaneExit`] the event loop acts on.
///
/// [`TerminalPool`]: super::terminal_pool::TerminalPool
pub fn run(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    monitor: &MonitorHandle,
) -> Result<PaneExit> {
    enable_raw_mode().context("failed to enter raw mode for the embedded terminal")?;
    let _ = term.clear_screen();
    let result = drive(term, state, pty, monitor);
    let _ = disable_raw_mode();
    let _ = term.show_cursor();
    result
}

/// The render/input loop: snapshot the shell screen, draw whatever changed,
/// then wait for a keystroke or fresh output and go again. Returns the
/// [`PaneExit`] reason when the shell exits or the user detaches / switches.
fn drive(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    monitor: &MonitorHandle,
) -> Result<PaneExit> {
    // The frame drawn last pass, so we only repaint the rows that changed.
    let mut prev: Vec<String> = Vec::new();
    // How many lines the pane is scrolled back into the shell's history; `0` is
    // the live screen. The wheel and `Shift`+`PageUp`/`PageDown` move it, typing
    // snaps it back, and `set_scrollback` clamps it to the buffered output.
    let mut scrollback: usize = 0;
    loop {
        let (height, width) = term.size();
        let geo = ui::terminal_geometry(height as usize, width as usize);
        pty.resize(geo.rows, geo.cols);
        // Apply the scroll position and re-read what the parser actually allows,
        // so an over-scroll past the oldest line settles at the top.
        scrollback = pty.set_scrollback(scrollback);

        // Note the output seen before snapshotting, so the wait below redraws
        // again if more arrives between here and then.
        let drawn_gen = pty.generation();
        let view = TerminalView::from_screen(pty.parser().screen());
        // The cursor belongs to the live screen, so hide it while the user is
        // viewing scrolled-back history.
        let cursor = if scrollback == 0 { view.cursor() } else { None };
        state.set_terminal_view(view);
        // Refresh the sidebar's waiting and live-agent markers so other
        // background sessions flagged while we are attached here show up in the
        // next repaint.
        state.set_waiting(monitor.waiting());
        state.set_live(monitor.live());
        render(term, state, cursor, geo, &mut prev)?;

        // The shell closed (e.g. the user typed `exit`): leave the pane.
        if !pty.is_alive() {
            return Ok(PaneExit::Closed);
        }

        match wait(pty, drawn_gen)? {
            // New output (or the idle timer): loop and redraw it.
            Wake::Output => {}
            // Input is queued: forward every pending key (or scroll the
            // history), then redraw.
            Wake::Input => {
                if let Some(exit) = pump_input(term, state, pty, geo, &mut scrollback, &mut prev)? {
                    return Ok(exit);
                }
            }
        }
    }
}

/// Why a [`wait`] ended: input is queued, or the shell produced output (or the
/// idle timer elapsed) and the pane should redraw.
enum Wake {
    Input,
    Output,
}

/// Block until a key (or other input event) is queued, the shell's output moves
/// past `drawn_gen`, or the idle timer elapses — whichever comes first.
fn wait(pty: &PtySession, drawn_gen: u64) -> Result<Wake> {
    let start = Instant::now();
    loop {
        // Fresh output (or the shell exiting, which also bumps the counter).
        if pty.generation() != drawn_gen {
            return Ok(Wake::Output);
        }
        if event::poll(POLL_SLICE)? {
            return Ok(Wake::Input);
        }
        if start.elapsed() >= IDLE_REDRAW {
            return Ok(Wake::Output);
        }
    }
}

/// Forward every queued key press to the shell, or — for the wheel and
/// `Shift`+`PageUp`/`PageDown` — scroll the pane's history via `scrollback`.
/// `Ctrl-O` opens the session picker instead. Returns `Some(exit)` when the user
/// switches or detaches; other events are ignored so the next redraw picks up
/// any new size.
fn pump_input(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    geo: ui::TerminalGeometry,
    scrollback: &mut usize,
    prev: &mut Vec<String>,
) -> Result<Option<PaneExit>> {
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) => {
                if !is_press(key) {
                    continue;
                }
                // Scroll keys move the history view in place rather than going
                // to the shell.
                if let Some(delta) = key_scroll_lines(&key, geo) {
                    apply_scroll(scrollback, delta);
                    continue;
                }
                if is_leader(&key) {
                    // `Ctrl-O` opens the picker; it returns the pane action to
                    // leave on (switch / detach), or `None` to resume the shell.
                    if let Some(exit) = run_session_picker(term, state, geo, prev)? {
                        return Ok(Some(exit));
                    }
                    continue;
                }
                let bytes = encode_key(&key);
                if !bytes.is_empty() {
                    // Typing returns to the live screen, like a real terminal.
                    *scrollback = 0;
                    pty.write(&bytes)?;
                }
            }
            // The wheel scrolls the history when it is over the terminal pane.
            Event::Mouse(mouse) => {
                if let Some(delta) = wheel_delta(mouse.kind) {
                    if (mouse.column as usize) >= geo.origin_col as usize {
                        apply_scroll(scrollback, delta);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// The history scroll a key requests, in lines (negative scrolls up toward older
/// output), or `None` for a key the shell should receive. `Shift` distinguishes
/// the scroll keys from the `PageUp`/`PageDown`/arrows the shell expects.
fn key_scroll_lines(key: &KeyEvent, geo: ui::TerminalGeometry) -> Option<i32> {
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
fn wheel_delta(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollUp => Some(-WHEEL_LINES),
        MouseEventKind::ScrollDown => Some(WHEEL_LINES),
        _ => None,
    }
}

/// Move the scrollback offset by `delta` lines (negative scrolls up toward
/// older output). The upper bound is enforced by `set_scrollback` on the next
/// redraw, so this only has to keep the offset from underflowing past the live
/// screen.
fn apply_scroll(scrollback: &mut usize, delta: i32) {
    *scrollback = if delta < 0 {
        scrollback.saturating_add(delta.unsigned_abs() as usize)
    } else {
        scrollback.saturating_sub(delta as usize)
    };
}

/// Run the in-pane session picker (`Ctrl-O`): overlay the session list and read
/// keys until the user switches to a session (`1`-`9` or `↑`/`↓` + `Enter`),
/// cancels (`Esc`), or detaches (`Ctrl-O` again). Returns the [`PaneExit`] to
/// leave the pane on, or `None` to resume the current shell. The picker is drawn
/// over the live frame, so it shares the caller's `prev` diff buffer.
fn run_session_picker(
    term: &Term,
    state: &mut HomeState,
    geo: ui::TerminalGeometry,
    prev: &mut Vec<String>,
) -> Result<Option<PaneExit>> {
    state.open_session_picker();
    // The shell cursor has no place under the overlay.
    let _ = term.hide_cursor();
    let exit = loop {
        render(term, state, None, geo, prev)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !is_press(key) {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.session_picker_move_up(),
            KeyCode::Down | KeyCode::Char('j') => state.session_picker_move_down(),
            // A 1-based session number jumps to and selects that row.
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if state.session_picker_select_number(c as usize - '0' as usize) {
                    break Some(PaneExit::Switch);
                }
            }
            KeyCode::Enter => break Some(PaneExit::Switch),
            KeyCode::Esc => break None,
            // A second `Ctrl-O` detaches, leaving the shell alive in the pool.
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break Some(PaneExit::Detach)
            }
            _ => {}
        }
    };
    // `Switch` commits the highlighted session; everything else just closes.
    if exit == Some(PaneExit::Switch) {
        state.confirm_session_picker();
    } else {
        state.cancel_session_picker();
    }
    Ok(exit)
}

/// Draw the workspace frame (sidebar + terminal pane), repainting only the rows
/// that changed since `prev` and batching them into a single write — so an
/// update lands in one pass without the flicker of clearing the whole screen.
/// Finally, park the real cursor over the shell's cursor cell so it tracks the
/// embedded terminal.
fn render(
    term: &Term,
    state: &HomeState,
    cursor: Option<(u16, u16)>,
    geo: ui::TerminalGeometry,
    prev: &mut Vec<String>,
) -> Result<()> {
    let (height, width) = term.size();
    let frame = ui::render_frame(height as usize, width as usize, state);

    // Repaint only the changed rows (see [`diff_frame`]); the cursor is hidden
    // for the repaint and re-shown below over the shell's cell.
    let mut buf = diff_frame(prev, &frame);

    if let Some((row, col)) = cursor {
        // Translate the pane-relative cursor to a 1-based screen position and
        // reveal it there.
        let x = geo.origin_col as usize + col as usize + 1;
        let y = geo.origin_row as usize + row as usize + 1;
        let _ = write!(buf, "\x1b[{y};{x}H\x1b[?25h");
    }

    term.write_str(&buf)?;
    term.flush()?;
    *prev = frame;
    Ok(())
}

/// Only forward real key presses (and auto-repeats), never key releases (which
/// some platforms / the kitty protocol report).
fn is_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// `Ctrl-O` opens the session picker (see [`run_session_picker`]).
fn is_leader(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o')
}

/// Translate a key event into the bytes a shell expects on its input. Unknown
/// keys map to nothing (an empty slice), so they are simply dropped.
fn encode_key(key: &KeyEvent) -> Vec<u8> {
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
