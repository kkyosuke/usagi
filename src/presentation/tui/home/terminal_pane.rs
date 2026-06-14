//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal`, the right pane switches to a live shell: this
//! module spawns the PTY ([`crate::infrastructure::pty`]), then runs a small
//! render/input loop that keeps the whole workspace frame on screen — sidebar
//! and all — with the shell's output drawn into the right pane. Keystrokes are
//! forwarded to the shell as raw bytes; `Ctrl-O` detaches and closes it, as does
//! the shell exiting on its own.
//!
//! `agent` reuses the same machinery: it spawns the shell and then sends an
//! `initial` command line (the configured agent CLI, e.g. `claude`) so the pane
//! lands the user straight in the agent — exactly as if they had run `terminal`
//! and typed it themselves.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the layout geometry and frame ([`super::ui`]) and the
//! screen snapshot ([`super::terminal_view`]).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use console::Term;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;

use super::state::HomeState;
use super::terminal_view::TerminalView;
use super::ui;

/// How long to wait for a keystroke before redrawing, so the pane reflects the
/// shell's output even while the user is idle (~30 fps).
const REFRESH: Duration = Duration::from_millis(33);

/// Run the embedded terminal in the right pane, rooted at `dir`, until the user
/// detaches (`Ctrl-O`) or the shell exits. The right-pane mode is set by the
/// caller; here we own the PTY, raw mode, and the render/input loop.
///
/// When `initial` is `Some`, that command line is sent to the shell on start
/// (followed by a carriage return) — this is how `agent` lands the user in the
/// configured agent CLI, just as if they had typed it into a fresh terminal.
pub fn run(term: &Term, state: &mut HomeState, dir: &Path, initial: Option<&str>) -> Result<()> {
    let (height, width) = term.size();
    let geo = ui::terminal_geometry(height as usize, width as usize);
    let mut pty = PtySession::spawn(dir, geo.rows, geo.cols)?;
    if let Some(command) = initial {
        // The shell buffers its input, so writing immediately is fine: it runs
        // the command once it has started up.
        pty.write(format!("{command}\r").as_bytes())?;
    }

    enable_raw_mode().context("failed to enter raw mode for the embedded terminal")?;
    let _ = term.clear_screen();
    let result = drive(term, state, &mut pty);
    let _ = disable_raw_mode();
    let _ = term.show_cursor();
    result
}

/// The render/input loop: snapshot the shell screen, draw the frame, then
/// forward one keystroke (or time out and redraw). Returns when the shell exits
/// or the user detaches.
fn drive(term: &Term, state: &mut HomeState, pty: &mut PtySession) -> Result<()> {
    loop {
        let (height, width) = term.size();
        let geo = ui::terminal_geometry(height as usize, width as usize);
        pty.resize(geo.rows, geo.cols);

        let view = TerminalView::from_screen(pty.parser().screen());
        let cursor = view.cursor();
        state.set_terminal_view(view);
        render(term, state, cursor, geo)?;

        // The shell closed (e.g. the user typed `exit`): leave the pane.
        if !pty.is_alive() {
            return Ok(());
        }

        if event::poll(REFRESH)? {
            if let Event::Key(key) = event::read()? {
                if !is_press(key) {
                    continue;
                }
                if is_detach(&key) {
                    return Ok(());
                }
                let bytes = encode_key(&key);
                if !bytes.is_empty() {
                    pty.write(&bytes)?;
                }
            }
        }
    }
}

/// Draw the workspace frame (sidebar + terminal pane), then park the real cursor
/// over the shell's cursor cell so it tracks the embedded terminal.
fn render(
    term: &Term,
    state: &HomeState,
    cursor: Option<(u16, u16)>,
    geo: ui::TerminalGeometry,
) -> Result<()> {
    let (height, width) = term.size();
    let frame = ui::render_frame(height as usize, width as usize, state);

    term.hide_cursor()?;
    for (row, line) in frame.iter().enumerate() {
        term.move_cursor_to(0, row)?;
        term.clear_line()?;
        term.write_str(line)?;
    }

    match cursor {
        Some((row, col)) => {
            let x = geo.origin_col as usize + col as usize;
            let y = geo.origin_row as usize + row as usize;
            term.move_cursor_to(x, y)?;
            term.show_cursor()?;
        }
        None => term.hide_cursor()?,
    }
    term.flush()?;
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
