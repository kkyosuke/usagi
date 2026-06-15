//! A live pseudo-terminal session, embedded in the workspace screen's right pane.
//!
//! The `terminal` command spawns the user's shell ([`terminal::default_shell`])
//! into a real PTY rooted at the selected worktree. A background thread streams
//! the shell's output into a [`vt100::Parser`], which maintains an in-memory
//! screen grid; the presentation layer snapshots that grid each frame and draws
//! it into the right pane (see `presentation::tui::home::terminal_pane`).
//!
//! This module is pure I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs`); the pure pieces it feeds — the shell choice
//! ([`terminal`]) and the grid-to-lines rendering (`home::terminal_view`) — are
//! tested on their own.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::infrastructure::terminal;

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
    reader_thread: Option<JoinHandle<()>>,
}

/// How many lines of scrolled-off output the embedded terminal keeps, so the
/// user can scroll the pane back over a command's earlier output.
const SCROLLBACK_LINES: usize = 10_000;

/// Configure `cmd` to run `command` in `shell` and then drop into an
/// interactive shell, so the launch line is passed as an argument (never echoed)
/// rather than typed into the shell's stdin. See [`PtySession::spawn`].
#[cfg(not(windows))]
fn configure_initial_command(cmd: &mut CommandBuilder, shell: &str, command: &str) {
    cmd.arg("-i");
    cmd.arg("-c");
    // `command` is already shell-quoted; chain a fresh interactive shell after
    // it so the user lands at a prompt when the agent exits.
    cmd.arg(format!("{command}; exec \"{shell}\" -i"));
}

/// Windows fallback: `cmd.exe` / PowerShell take `/c` and have no `exec`, so the
/// command runs and the shell then exits — close enough for the rare Windows
/// case, and still without echoing the launch line.
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
    /// Passed as an argument it is never echoed. The shell then execs a fresh
    /// interactive shell so the user is left at a prompt once the command exits,
    /// exactly as if they had run it themselves.
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
            reader_thread: Some(reader_thread),
        })
    }

    /// Lock the screen-grid parser to read the current contents (for rendering).
    pub fn parser(&self) -> MutexGuard<'_, vt100::Parser<BellCounter>> {
        self.parser.lock().expect("pty parser mutex poisoned")
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
        // Terminate the shell and let the reader thread finish on EOF.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
    }
}
