//! A live pseudo-terminal session, embedded in the workspace screen's right pane.
//!
//! The `terminal` command spawns the user's shell ([`terminal::default_shell`])
//! into a real PTY rooted at the selected worktree. A background thread streams
//! the shell's output into a [`vt100::Parser`], which maintains an in-memory
//! screen grid; the presentation layer snapshots that grid each frame and draws
//! it into the right pane (see `presentation::tui::home::terminal_pane`).
//!
//! When the shell exits on its own, [`Drop`] reaps it and — if it ended
//! abnormally (non-zero or by signal) — records the exit to the error log, so a
//! crashed agent CLI no longer looks exactly like the user typing `exit`.
//!
//! This module is pure I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs`); the pure pieces it feeds — the shell choice
//! ([`terminal`]), the grid-to-lines rendering (`home::terminal_view`), and the
//! exit-status-to-log-line decision ([`pty_exit`]) — are tested on their own.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::{pty_exit, terminal};

/// Counts the audible bells (`^G`) the shell emits, recorded into a shared
/// counter as the parser processes output.
///
/// Interactive agents such as Claude Code ring the terminal bell when they
/// finish a turn and wait for the user — so a rising bell count is the signal
/// the session monitor watches to flag a worktree as "waiting for input".
///
/// Public only because it appears in [`PtySession::parser`]'s return type; it
/// carries no usable surface of its own.
pub struct BellCounter {
    count: Arc<AtomicU64>,
}

impl vt100::Callbacks for BellCounter {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// A running shell attached to a pseudo-terminal, with its output parsed into a
/// terminal screen grid.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser<BellCounter>>>,
    alive: Arc<AtomicBool>,
    /// Bumped by the reader thread after every chunk it parses, so the render
    /// loop can tell at a glance whether the screen has changed since it last
    /// drew — and wake immediately when it has, instead of on a fixed timer.
    generation: Arc<AtomicU64>,
    /// The running count of audible bells the shell has emitted, kept outside
    /// the parser mutex so the session monitor can poll it without contending
    /// with the render loop.
    bell: Arc<AtomicU64>,
    child: Box<dyn Child + Send + Sync>,
    /// The worktree this shell runs in and whether it was launched into an agent
    /// CLI — both only for the line [`Drop`] records when the shell exits
    /// abnormally on its own (see [`pty_exit::exit_log_message`]).
    worktree: PathBuf,
    is_agent: bool,
    reader_thread: Option<JoinHandle<()>>,
}

/// How many lines of scrolled-off output the embedded terminal keeps, so the
/// user can scroll the pane back over a command's earlier output.
const SCROLLBACK_LINES: usize = 10_000;

/// Configure `cmd` to run `command` in `shell` and then exit, so the launch
/// line is passed as an argument (never echoed) rather than typed into the
/// shell's stdin. The shell exits when `command` does, so leaving the agent
/// drops the pane back to 在席 (Focus) instead of a bare shell prompt. See
/// [`PtySession::spawn`].
#[cfg(not(windows))]
fn configure_initial_command(cmd: &mut CommandBuilder, _shell: &str, command: &str) {
    cmd.arg("-i");
    cmd.arg("-c");
    // `command` is already shell-quoted; run it and let the `-c` shell exit when
    // it finishes (no trailing `exec`), so the agent exiting closes the pane.
    cmd.arg(command);
}

/// Windows fallback: `cmd.exe` / PowerShell take `/c` and run the command, then
/// the shell exits — the same end behaviour as the Unix path, still without
/// echoing the launch line.
#[cfg(windows)]
fn configure_initial_command(cmd: &mut CommandBuilder, _shell: &str, command: &str) {
    cmd.arg("/c");
    cmd.arg(command);
}

impl PtySession {
    /// Spawn the default shell into a fresh PTY of `rows`×`cols`, rooted at
    /// `dir`. The shell's output is streamed into a [`vt100::Parser`] on a
    /// background thread until it closes (the reader sees EOF), at which point
    /// the session is marked no longer [`alive`](Self::is_alive).
    ///
    /// When `command` is `Some`, it is run as a shell argument rather than typed
    /// into the shell's stdin: a typed command is echoed back by the shell's
    /// line editor, which would splash the long `:agent` launch line (with its
    /// `--append-system-prompt`) across the pane before the agent draws over it.
    /// Passed as an argument it is never echoed. The shell exits once the
    /// command does, so leaving the agent returns to 在席 (Focus) rather than
    /// dropping the user at a bare shell prompt.
    pub fn spawn(dir: &Path, rows: u16, cols: u16, command: Option<&str>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open a pseudo-terminal")?;

        let shell = terminal::default_shell();
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(dir);
        if let Some(command) = command {
            configure_initial_command(&mut cmd, &shell, command);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn the shell in the pseudo-terminal")?;
        // The child now holds the slave end; drop ours so the reader below sees
        // EOF once the shell exits.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to read from the pseudo-terminal")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to write to the pseudo-terminal")?;

        let bell = Arc::new(AtomicU64::new(0));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_LINES,
            BellCounter {
                count: Arc::clone(&bell),
            },
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));

        let reader_thread = {
            let parser = Arc::clone(&parser);
            let alive = Arc::clone(&alive);
            let generation = Arc::clone(&generation);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut parser) = parser.lock() {
                                parser.process(&buf[..n]);
                            }
                            // Announce the new output so a waiting render loop
                            // redraws it without waiting out its idle timer.
                            generation.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
                alive.store(false, Ordering::SeqCst);
                generation.fetch_add(1, Ordering::SeqCst);
            })
        };

        Ok(Self {
            master: pair.master,
            writer,
            parser,
            alive,
            generation,
            bell,
            child,
            // A launch command means this pane runs an agent CLI; its absence a
            // plain terminal. Recorded for the exit log line built in Drop.
            worktree: dir.to_path_buf(),
            is_agent: command.is_some(),
            reader_thread: Some(reader_thread),
        })
    }

    /// Lock the screen-grid parser to read the current contents (for rendering).
    pub fn parser(&self) -> MutexGuard<'_, vt100::Parser<BellCounter>> {
        self.parser.lock().expect("pty parser mutex poisoned")
    }

    /// Whether the running program has asked for bracketed paste mode
    /// (DECSET 2004). When it has, pasted text must be wrapped in the
    /// `ESC [ 200~` / `ESC [ 201~` markers so the program treats the whole block
    /// as a single paste (newlines insert rather than submit each line).
    pub fn bracketed_paste(&self) -> bool {
        self.parser().screen().bracketed_paste()
    }

    /// The running count of audible bells the shell has emitted so far. The
    /// session monitor compares it against a baseline to tell when an embedded
    /// agent has rung the bell to ask for input.
    pub fn bell_count(&self) -> u64 {
        self.bell.load(Ordering::SeqCst)
    }

    /// A shared handle to the bell counter, so a background watcher can poll it
    /// without owning (or borrowing) the session.
    pub fn bell_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.bell)
    }

    /// A shared handle to the liveness flag, so a background watcher can tell
    /// when the shell has exited and stop tracking it.
    pub fn alive_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// Forward raw input bytes to the shell.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY (and its screen grid) to `rows`×`cols`. Best-effort: a
    /// failure to inform the kernel is ignored, the grid is resized regardless.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
    }

    /// Scroll the screen `offset` lines back into the buffered history (`0` is
    /// the live screen), returning the offset actually applied — vt100 clamps it
    /// to the buffered lines, so the caller can use the result to stop scrolling
    /// past the oldest output.
    pub fn set_scrollback(&mut self, offset: usize) -> usize {
        if let Ok(mut parser) = self.parser.lock() {
            let screen = parser.screen_mut();
            screen.set_scrollback(offset);
            screen.scrollback()
        } else {
            0
        }
    }

    /// Whether the shell is still running (the reader has not hit EOF).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// A counter bumped each time the shell's output is parsed (and once more
    /// when it exits). The render loop compares it against the value at its last
    /// draw to decide whether the screen needs redrawing.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.is_alive() {
            // The shell is still running, so dropping the session is a deliberate
            // teardown (the user closed the pane or left the screen): terminate
            // it and reap. `Child::kill` escalates SIGHUP → SIGKILL, guaranteeing
            // the reader thread then sees EOF. A deliberate close is not a
            // failure, so nothing is logged.
            let _ = self.child.kill();
            let _ = self.child.wait();
        } else {
            // The shell exited on its own (the reader hit EOF): reap it and, if it
            // ended abnormally, record it — an agent CLI crashing or exiting
            // non-zero is otherwise indistinguishable from the user typing `exit`.
            // We own the child and reap exactly once here, so there is no
            // pid-reuse window from signalling an already-reaped process.
            if let Ok(status) = self.child.wait() {
                if let Some(message) =
                    pty_exit::exit_log_message(&self.worktree, self.is_agent, &status)
                {
                    ErrorLog::record(&message);
                }
            }
        }
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
    }
}
