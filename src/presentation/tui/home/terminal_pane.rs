//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal`, the right pane switches to a live shell: this
//! module spawns the PTY ([`crate::infrastructure::pty`]), then runs a small
//! render/input loop that keeps the whole workspace frame on screen — sidebar
//! and all — with the shell's output drawn into the right pane. Keystrokes are
//! forwarded to the shell as raw bytes; `Ctrl-O` detaches and closes it, as does
//! the shell exiting on its own.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the layout geometry and frame ([`super::ui`]) and the
//! screen snapshot ([`super::terminal_view`]).

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use console::Term;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;

use super::state::HomeState;
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

/// Run the embedded terminal in the right pane, rooted at `dir`, until the user
/// detaches (`Ctrl-O`) or the shell exits. The right-pane mode is set by the
/// caller; here we own the PTY, raw mode, and the render/input loop.
pub fn run(term: &Term, state: &mut HomeState, dir: &Path) -> Result<()> {
    let (height, width) = term.size();
    let geo = ui::terminal_geometry(height as usize, width as usize);
    let mut pty = PtySession::spawn(dir, geo.rows, geo.cols)?;

    enable_raw_mode().context("failed to enter raw mode for the embedded terminal")?;
    let _ = term.clear_screen();
    let result = drive(term, state, &mut pty);
    let _ = disable_raw_mode();
    let _ = term.show_cursor();
    result
}

/// The render/input loop: snapshot the shell screen, draw whatever changed,
/// then wait for a keystroke or fresh output and go again. Returns when the
/// shell exits or the user detaches.
fn drive(term: &Term, state: &mut HomeState, pty: &mut PtySession) -> Result<()> {
    // The frame drawn last pass, so we only repaint the rows that changed.
    let mut prev: Vec<String> = Vec::new();
    loop {
        let (height, width) = term.size();
        let geo = ui::terminal_geometry(height as usize, width as usize);
        pty.resize(geo.rows, geo.cols);

        // Note the output seen before snapshotting, so the wait below redraws
        // again if more arrives between here and then.
        let drawn_gen = pty.generation();
        let view = TerminalView::from_screen(pty.parser().screen());
        let cursor = view.cursor();
        state.set_terminal_view(view);
        render(term, state, cursor, geo, &mut prev)?;

        // The shell closed (e.g. the user typed `exit`): leave the pane.
        if !pty.is_alive() {
            return Ok(());
        }

        match wait(pty, drawn_gen)? {
            // New output (or the idle timer): loop and redraw it.
            Wake::Output => {}
            // Input is queued: forward every pending key, then redraw.
            Wake::Input => {
                if pump_input(pty)? {
                    return Ok(());
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

/// Forward every queued key press to the shell. Returns `true` if the user asked
/// to detach (`Ctrl-O`); non-key events (resize, …) are ignored so the next
/// redraw simply picks up the new size.
fn pump_input(pty: &mut PtySession) -> Result<bool> {
    while event::poll(Duration::ZERO)? {
        if let Event::Key(key) = event::read()? {
            if !is_press(key) {
                continue;
            }
            if is_detach(&key) {
                return Ok(true);
            }
            let bytes = encode_key(&key);
            if !bytes.is_empty() {
                pty.write(&bytes)?;
            }
        }
    }
    Ok(false)
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

    let mut buf = String::from("\x1b[?25l"); // hide the cursor while repainting
    for (row, line) in frame.iter().enumerate() {
        if prev.get(row) != Some(line) {
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

/// `Ctrl-O` detaches from the embedded terminal and closes it.
fn is_detach(key: &KeyEvent) -> bool {
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
