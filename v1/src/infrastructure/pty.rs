//! A live pseudo-terminal session, embedded in the workspace screen's right pane.
//!
//! The `terminal` command spawns the user's shell ([`terminal::default_shell`])
//! into a real PTY rooted at the selected worktree. A background thread streams
//! the shell's output into a [`vt100::Parser`], which maintains an in-memory
//! screen grid; the presentation layer snapshots that grid each frame and draws
//! it into the right pane (see `presentation::tui::home::terminal::pane`).
//!
//! When the shell exits on its own, [`Drop`] reaps it and — if it ended
//! abnormally (non-zero or by signal) — records the exit to the error log, so a
//! crashed agent CLI no longer looks exactly like the user typing `exit`.
//!
//! This module is pure I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs`); the pure pieces it feeds — the shell choice
//! ([`terminal`]), the grid-to-lines rendering (`home::terminal::view`), and the
//! exit-status-to-log-line decision ([`pty_exit`]) — are tested on their own.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::{pty_exit, terminal};

/// Side-channel signals the parser pulls out of the shell's output stream as it
/// processes it — sequences `vt100` does not fold into the screen grid but the
/// presentation layer still needs.
///
/// - **Audible bells (`^G`)** are counted into a shared counter. Interactive
///   agents such as Claude Code ring the terminal bell when they finish a turn
///   and wait for the user — so a rising bell count is the signal the session
///   monitor watches to flag a worktree as "waiting for input".
/// - **The cursor shape (DECSCUSR, `CSI Ps SP q`)** is captured into a shared
///   cell. `vt100` discards it, so without this an agent that picks a bar cursor
///   would leave its shape stuck on the host terminal — and switching to a tab
///   that wants a block would keep showing the previous tab's bar. The render
///   loop reads [`PtySession::cursor_shape`] and re-asserts the active pane's
///   shape, so each tab restores its own.
///
/// Public only because it appears in [`PtySession::parser`]'s return type; it
/// carries no usable surface of its own.
pub struct ScreenCallbacks {
    count: Arc<AtomicU64>,
    cursor_shape: Arc<AtomicU16>,
}

#[cfg(test)]
impl ScreenCallbacks {
    pub(crate) fn new(count: Arc<AtomicU64>, cursor_shape: Arc<AtomicU16>) -> Self {
        Self {
            count,
            cursor_shape,
        }
    }
}

impl vt100::Callbacks for ScreenCallbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    /// `vt100` routes any CSI it does not implement here. DECSCUSR is
    /// `CSI Ps SP q` — the space intermediate (`i1`) is what distinguishes it
    /// from the other `q` finals — and `Ps` selects the cursor shape (0/1 =
    /// blinking block, 2 = steady block, 3/4 = underline, 5/6 = bar). An absent
    /// `Ps` defaults to 0. Shapes outside the defined 0..=6 range are ignored so
    /// only a value we can safely re-emit is ever stored.
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if c == 'q' && i1 == Some(b' ') {
            let shape = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
            if shape <= 6 {
                self.cursor_shape.store(shape, Ordering::SeqCst);
            }
        }
    }
}

/// Cloneable handle for injecting input into a live PTY from outside the render
/// loop.
///
/// The home screen normally writes to a pane through `&mut PtySession` while the
/// pane is focused. A live MCP send is different: the `usagi mcp` process can
/// only leave a prompt on disk, and the terminal-pool watcher (a background
/// thread in the TUI process) must pick it up and type it into the already-open
/// agent pane even when that pane is detached. This handle shares the PTY input
/// writer with the owner while also exposing the parser's bracketed-paste flag
/// so the caller can encode multi-line prompts exactly as a terminal paste.
#[derive(Clone)]
pub struct PtyInputHandle {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
}

impl PtyInputHandle {
    /// Whether the running program has asked for bracketed paste mode
    /// (DECSET 2004). See [`PtySession::bracketed_paste`].
    pub fn bracketed_paste(&self) -> bool {
        self.parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .screen()
            .bracketed_paste()
    }

    /// Forward raw input bytes to the shell.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
}

/// A running shell attached to a pseudo-terminal, with its output parsed into a
/// terminal screen grid.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
    alive: Arc<AtomicBool>,
    /// Bumped by the reader thread after every chunk it parses, so the render
    /// loop can tell at a glance whether the screen has changed since it last
    /// drew — and wake immediately when it has, instead of on a fixed timer.
    generation: Arc<AtomicU64>,
    /// The running count of audible bells the shell has emitted, kept outside
    /// the parser mutex so the session monitor can poll it without contending
    /// with the render loop.
    bell: Arc<AtomicU64>,
    /// The cursor shape (DECSCUSR `Ps`) the running program last selected, kept
    /// outside the parser mutex so the render loop can re-assert the active
    /// pane's shape cheaply. `0` until the program picks one (the terminal
    /// default). See [`ScreenCallbacks`].
    cursor_shape: Arc<AtomicU16>,
    child: Box<dyn Child + Send + Sync>,
    exit_status: Option<ExitStatus>,
    /// The worktree this shell runs in and whether it was launched into an agent
    /// CLI — both only for the line [`Drop`] records when the shell exits
    /// abnormally on its own (see [`pty_exit::exit_log_message`]).
    worktree: PathBuf,
    is_agent: bool,
    reader_thread: Option<JoinHandle<()>>,
}

/// The stack the PTY reader thread is given. The thread only loops over a
/// blocking `read` into a heap-allocated buffer and hands the bytes to
/// `vt100::Parser` (whose own grid lives on the heap), so it needs far less than a
/// thread's 2 MiB default stack. One reader thread runs per live pane, so with
/// many sessions and panes open at once the default stacks alone reserve tens of
/// MiB of address space; a tighter stack keeps that footprint small. 256 KiB
/// leaves ample headroom over the shallow read/parse call chain.
const READER_STACK_BYTES: usize = 256 * 1024;

/// Configure `cmd` to run `command` in `shell` and then exit, so the launch
/// line is passed as an argument (never echoed) rather than typed into the
/// shell's stdin. The shell exits when `command` does, so leaving the agent
/// drops the pane back to 集中 (Closeup) instead of a bare shell prompt. See
/// [`PtySession::spawn`].
#[cfg(not(windows))]
fn configure_initial_command(cmd: &mut CommandBuilder, _shell: &str, command: &str) {
    cmd.arg("-i");
    cmd.arg("-c");
    // `command` is already shell-quoted; run it and explicitly exit with the
    // same status so shells that stay interactive after `-i -c` still close the
    // pane when the agent exits.
    cmd.arg(format!("{command}\nexit $?"));
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
    /// command does, so leaving the agent returns to 集中 (Closeup) rather than
    /// dropping the user at a bare shell prompt.
    ///
    /// `env` is a map of effective (global plus workspace-local) secret
    /// environment variables already resolved by the caller. The values are
    /// injected through the child process environment, never through the launch
    /// command line.
    ///
    /// `scrollback` caps how many scrolled-off lines the embedded terminal keeps
    /// for the user to scroll back over (the `vt100` parser grows the buffer
    /// lazily up to this bound). It is the configured
    /// [`Settings::terminal_scrollback_lines`](crate::domain::settings::Settings)
    /// value, threaded down from the pool, so a smaller cap trades scroll depth
    /// for a smaller per-pane memory footprint when many panes are open.
    pub fn spawn(
        dir: &Path,
        rows: u16,
        cols: u16,
        command: Option<&str>,
        scrollback: usize,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        Self::spawn_inner(dir, rows, cols, command, scrollback, env)
    }

    fn spawn_inner(
        dir: &Path,
        rows: u16,
        cols: u16,
        command: Option<&str>,
        scrollback: usize,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
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
        for (name, value) in env {
            cmd.env(name, value);
        }
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
        let writer = Arc::new(Mutex::new(writer));

        let bell = Arc::new(AtomicU64::new(0));
        let cursor_shape = Arc::new(AtomicU16::new(0));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            scrollback,
            ScreenCallbacks {
                count: Arc::clone(&bell),
                cursor_shape: Arc::clone(&cursor_shape),
            },
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));

        let reader_thread = {
            // A *weak* handle, so this thread never keeps the parser (a large
            // scrollback grid) alive on its own. If a descendant escapes the
            // process group and holds the slave fd open, the `read` below never
            // sees EOF and `Drop` detaches this thread rather than hang the UI;
            // capturing `Weak` means the session's last strong ref still frees
            // the grid the moment the session drops, instead of the grid living
            // as long as the orphaned thread stays blocked (a memory leak that
            // grew with every closed session).
            let parser = Arc::downgrade(&parser);
            let alive = Arc::clone(&alive);
            let generation = Arc::clone(&generation);
            std::thread::Builder::new()
                .name("usagi-pty-reader".to_string())
                .stack_size(READER_STACK_BYTES)
                .spawn(move || {
                    // Mark the session dead and wake the render loop on *any* exit from
                    // this thread — clean EOF, a read error, or a panic in
                    // `parser.process` (which parses untrusted shell output). A drop
                    // guard does it so the panic path is covered too: were `alive` left
                    // `true` after a panic, `is_alive()` would report the session live
                    // forever and it would linger in the UI as a zombie that never
                    // closes (and `Drop`'s teardown would wait on a thread that is gone).
                    struct DeathBell {
                        alive: Arc<AtomicBool>,
                        generation: Arc<AtomicU64>,
                    }
                    impl Drop for DeathBell {
                        fn drop(&mut self) {
                            self.alive.store(false, Ordering::SeqCst);
                            self.generation.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let _death = DeathBell {
                        alive,
                        generation: Arc::clone(&generation),
                    };
                    // A 64 KiB read buffer (heap, so the reader thread's stack is
                    // untouched): during an output flood each `read` returns up to
                    // this much at once, so a burst is drained in ~1/8 the read
                    // syscalls, parser-lock acquisitions, and generation bumps that
                    // an 8 KiB buffer took — cutting contention with the render loop
                    // that locks the same parser. `parser.process` handles any chunk
                    // size, so this only changes throughput, not correctness.
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => match parser.upgrade() {
                                // Session still live: parse the output into its grid.
                                Some(parser) => {
                                    if let Ok(mut parser) = parser.lock() {
                                        parser.process(&buf[..n]);
                                    }
                                    // Announce the new output so a waiting render
                                    // loop redraws it without waiting out its idle
                                    // timer.
                                    generation.fetch_add(1, Ordering::SeqCst);
                                }
                                // The session was dropped while we were blocked in
                                // `read`: its grid is already freed, so there is
                                // nowhere to put this output. Stop the thread.
                                None => break,
                            },
                        }
                    }
                })
                .context("failed to spawn the pseudo-terminal reader thread")?
        };

        Ok(Self {
            master: pair.master,
            writer,
            parser,
            alive,
            generation,
            bell,
            cursor_shape,
            child,
            exit_status: None,
            // A launch command means this pane runs an agent CLI; its absence a
            // plain terminal. Recorded for the exit log line built in Drop.
            worktree: dir.to_path_buf(),
            is_agent: command.is_some(),
            reader_thread: Some(reader_thread),
        })
    }

    /// Lock the screen-grid parser to read the current contents (for rendering).
    ///
    /// Recovers the guard rather than panicking if the lock was poisoned: this
    /// runs on the render path, and the reader thread holds the same lock around
    /// `parser.process` (which parses untrusted shell output), so a panic there
    /// would poison the mutex and an `expect` here would escalate it into a crash
    /// of the whole TUI — leaving the terminal in raw mode. A possibly-stale
    /// screen grid beats taking the UI down.
    pub fn parser(&self) -> MutexGuard<'_, vt100::Parser<ScreenCallbacks>> {
        self.parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    /// A shared handle to the screen-grid parser, so a background watcher can
    /// scan the pane's output (e.g. for pull-request URLs) without owning — or
    /// blocking the render loop that borrows — the session. The watcher locks it
    /// only briefly and off its own state lock (see the terminal pool's watcher).
    pub fn parser_handle(&self) -> Arc<Mutex<vt100::Parser<ScreenCallbacks>>> {
        Arc::clone(&self.parser)
    }

    /// A shared handle to the output generation counter (see
    /// [`generation`](Self::generation)), so a background watcher can tell whether
    /// the pane produced new output since it last scanned it — and skip the
    /// (whole-grid) scan when it did not — without owning the session.
    pub fn generation_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// The shell's process id — also its process-group id, since portable-pty
    /// launches it as a session leader (`setsid`), so it is the root a resource
    /// sampler walks to total the session's whole process tree (the shell and any
    /// agent CLI beneath it). `None` once the child has been reaped.
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// The cursor shape (DECSCUSR `Ps`) the running program last selected, or `0`
    /// (the terminal default) until it picks one. The embedded-pane render loop
    /// re-emits `CSI Ps SP q` for the active pane so switching tabs restores each
    /// pane's own cursor shape instead of leaking the previous tab's.
    pub fn cursor_shape(&self) -> u16 {
        self.cursor_shape.load(Ordering::SeqCst)
    }

    /// A shared handle to the liveness flag, so a background watcher can tell
    /// when the shell has exited and stop tracking it.
    pub fn alive_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// A cloneable handle that can write to this PTY without borrowing the
    /// owning [`PtySession`]. Used by the terminal-pool watcher to inject
    /// MCP-sent prompts into a backgrounded agent pane.
    pub fn input_handle(&self) -> PtyInputHandle {
        PtyInputHandle {
            writer: Arc::clone(&self.writer),
            parser: Arc::clone(&self.parser),
        }
    }

    /// Forward raw input bytes to the shell.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.input_handle().write(bytes)
    }

    /// Resize the PTY (and its screen grid) to `rows`×`cols`. Best-effort: a
    /// failure to inform the kernel is ignored, the grid is resized regardless.
    ///
    /// The kernel resize and the grid `set_size` happen under the parser lock as
    /// one step. The reader thread parses PTY output while holding that same
    /// lock, so informing the kernel *before* taking it would let reflowed output
    /// (produced for the new size) be parsed into the still-old grid in the
    /// window between the two calls, corrupting the display until the next
    /// refresh. Holding the lock across both keeps the reader from observing a
    /// size mismatch.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if let Ok(mut parser) = self.parser.lock() {
            let _ = self.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
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

    /// The scroll offset currently applied to the buffered history (`0` is the
    /// live screen). Output streaming in while the pane is scrolled back advances
    /// this on its own — the vendored `vt100`'s `scroll_up` bumps the offset as
    /// lines enter the scrollback so the viewed region stays pinned — so the
    /// render loop reads it back to keep its tracked offset in step (otherwise a
    /// later wheel notch would scroll relative to a stale value).
    pub fn scrollback(&self) -> usize {
        self.parser().screen().scrollback()
    }

    /// Whether the shell is still running (the reader has not hit EOF).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Poll the child directly and return whether it has exited.
    ///
    /// The reader thread normally clears [`alive`](Self::alive) when it observes
    /// EOF from the PTY, but EOF is not the only reliable child-death signal: a
    /// shell can be reaped before the reader gets scheduled to observe the
    /// closed slave side.
    pub fn poll_exit(&mut self) -> bool {
        if !self.is_alive() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_status = Some(status);
                self.alive.store(false, Ordering::SeqCst);
                self.generation.fetch_add(1, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// A counter bumped each time the shell's output is parsed (and once more
    /// when it exits). The render loop compares it against the value at its last
    /// draw to decide whether the screen needs redrawing.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Terminate the shell **and everything it spawned** on a deliberate
    /// teardown.
    ///
    /// `Child::kill` here is `std::process::Child::kill` (via portable-pty),
    /// which sends a single `SIGKILL` to the shell pid alone — it does *not*
    /// signal the process group. An agent CLI the shell launched would then be
    /// reparented to init and keep running, holding the worktree open *and*
    /// keeping the PTY slave fd open — which would stop the reader thread from
    /// ever seeing EOF and deadlock the `wait`/`join` in [`Drop`].
    ///
    /// portable-pty makes the shell a session leader (`setsid`), so its pid is
    /// also its process-group id; signalling that group (via `killpg`) reaches
    /// the agent and any other descendant. We still call `child.kill` afterward
    /// as the Windows fallback (no process groups there) and as a harmless
    /// no-op once the group is already gone.
    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // SAFETY: `killpg` with a valid signal has no memory effects; a
            // stale or unknown group just yields `ESRCH`, which we ignore.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    /// Reap the child without blocking the caller indefinitely, returning its
    /// exit status when it is collected within [`TEARDOWN_TIMEOUT`].
    ///
    /// In the normal case the child has already exited (or `killpg` just reaped
    /// the whole group) and `try_wait` returns immediately. But if a descendant
    /// escaped the process group — it re-`setsid`'d, or this is Windows where
    /// there is no group kill — the child can linger, and a plain blocking
    /// `wait()` here would freeze the UI thread that runs `Drop`. Polling with a
    /// deadline bounds that worst case: an un-reaped child becomes a short-lived
    /// zombie until the process exits, which is far better than a frozen UI.
    fn reap_within_timeout(&mut self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(TEARDOWN_POLL),
                _ => return None,
            }
        }
    }
}

/// The longest `Drop` waits in total — across reaping the child **and** joining
/// the reader thread — before giving up rather than blocking the UI thread. The
/// two waits share one deadline so a pathological close cannot stack them into a
/// double-length stall.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// How often [`PtySession::reap_within_timeout`] re-polls within that window.
const TEARDOWN_POLL: Duration = Duration::from_millis(10);

impl Drop for PtySession {
    fn drop(&mut self) {
        // One deadline shared by both waits below, so the reap and the reader-join
        // can never stack into a double-length UI stall on a pathological close.
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        let deliberate = self.is_alive();
        if deliberate {
            // The shell is still running, so dropping the session is a deliberate
            // teardown (the user closed the pane or left the screen): kill the
            // whole process group (shell + any agent it spawned) so the reader
            // thread then sees EOF. A deliberate close is not a failure, so the
            // reaped status is discarded rather than logged.
            self.terminate();
            let _ = self.reap_within_timeout(deadline);
        } else if let Some(status) = self
            .exit_status
            .take()
            .or_else(|| self.reap_within_timeout(deadline))
        {
            // The shell exited on its own (the reader hit EOF): reap it and, if it
            // ended abnormally, record it — an agent CLI crashing or exiting
            // non-zero is otherwise indistinguishable from the user typing `exit`.
            // We own the child and reap exactly once here, so there is no
            // pid-reuse window from signalling an already-reaped process.
            if let Some(message) =
                pty_exit::exit_log_message(&self.worktree, self.is_agent, &status)
            {
                ErrorLog::record(&message);
            }
        }
        // Join the reader only once it has actually left (its drop guard clears
        // `alive` on EOF/error/panic). Wait briefly for that, then join — it is
        // instant once the thread has exited. If the thread is still blocked in
        // `read` because an escaped descendant holds the slave fd open, detach it
        // rather than hang the UI; it ends when EOF finally arrives or with the
        // process.
        if let Some(thread) = self.reader_thread.take() {
            while self.is_alive() && Instant::now() < deadline {
                std::thread::sleep(TEARDOWN_POLL);
            }
            if !self.is_alive() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a parser wired with [`ScreenCallbacks`] over `bytes` and read back
    /// the shape the callback captured — the same path the reader thread feeds.
    fn shape_after(bytes: &[u8]) -> u16 {
        let cursor_shape = Arc::new(AtomicU16::new(0));
        let mut parser = vt100::Parser::new_with_callbacks(
            4,
            10,
            0,
            ScreenCallbacks {
                count: Arc::new(AtomicU64::new(0)),
                cursor_shape: Arc::clone(&cursor_shape),
            },
        );
        parser.process(bytes);
        cursor_shape.load(Ordering::SeqCst)
    }

    #[test]
    fn decscusr_captures_each_defined_cursor_shape() {
        // 0..=6 are the defined DECSCUSR shapes (block / underline / bar, each
        // blinking or steady); every one round-trips into the shared cell.
        for ps in 0..=6u16 {
            assert_eq!(shape_after(format!("\x1b[{ps} q").as_bytes()), ps);
        }
    }

    #[test]
    fn decscusr_with_no_param_is_the_default_shape() {
        // `CSI SP q` with no `Ps` selects shape 0 (the terminal default).
        assert_eq!(shape_after(b"\x1b[ q"), 0);
    }

    #[test]
    fn out_of_range_shape_is_ignored() {
        // A program first picks a bar (6), then emits an undefined shape (9): the
        // junk value is dropped so only a re-emittable shape is ever stored.
        assert_eq!(shape_after(b"\x1b[6 q\x1b[9 q"), 6);
    }

    #[test]
    fn a_q_without_the_space_intermediate_is_not_decscusr() {
        // `CSI Ps q` (no space) is not DECSCUSR, so it must not move the shape off
        // its default.
        assert_eq!(shape_after(b"\x1b[5q"), 0);
    }

    #[test]
    fn an_audible_bell_still_counts_alongside_shape_capture() {
        // The bell side-channel keeps working now that the callbacks also track
        // the cursor shape.
        let count = Arc::new(AtomicU64::new(0));
        let mut parser = vt100::Parser::new_with_callbacks(
            4,
            10,
            0,
            ScreenCallbacks {
                count: Arc::clone(&count),
                cursor_shape: Arc::new(AtomicU16::new(0)),
            },
        );
        parser.process(b"\x07\x07");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// Lines scrolled out of a **top-anchored** DECSTBM region (`ESC[1;Nr`) must
    /// still land in the scrollback. Inline full-screen TUIs (e.g. the Codex CLI)
    /// reserve their composer with such a region and scroll the transcript inside
    /// it; upstream `vt100` only fed the scrollback when *no* region was active,
    /// so the embedded pane had nothing to wheel back through. This guards the
    /// vendored patch in `third_party/vt100`.
    #[test]
    fn top_anchored_scroll_region_still_feeds_the_scrollback() {
        let mut parser = vt100::Parser::new(6, 20, 100);
        // Reserve the bottom two rows (a composer) by anchoring the region to the
        // top, then scroll ten lines of transcript through the upper region.
        parser.process(b"\x1b[1;4r");
        for i in 0..10 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        let screen = parser.screen_mut();
        screen.set_scrollback(100);
        assert!(
            screen.scrollback() > 0,
            "a top-anchored region must feed the scrollback (got {})",
            screen.scrollback()
        );
    }

    /// A region that does **not** start at the top (`ESC[2;Nr`) keeps upstream
    /// behaviour: its scrolled-out lines are dropped, matching xterm (only a
    /// top margin of the first row saves lines).
    #[test]
    fn an_offset_scroll_region_does_not_feed_the_scrollback() {
        let mut parser = vt100::Parser::new(6, 20, 100);
        parser.process(b"\x1b[2;5r");
        for i in 0..10 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        let screen = parser.screen_mut();
        screen.set_scrollback(100);
        assert_eq!(
            screen.scrollback(),
            0,
            "an offset region must not feed the scrollback"
        );
    }

    /// While the pane is scrolled back, output streaming in advances the parser's
    /// own offset so the viewed region stays pinned to the same lines. The render
    /// loop (`terminal/pane.rs`) relies on this to re-read the offset and keep its
    /// tracked value in step — otherwise a later wheel notch scrolls relative to a
    /// stale offset and the view jumps by however many lines streamed in. This
    /// guards the offset auto-advance in the vendored `vt100` `scroll_up`.
    #[test]
    fn streaming_output_advances_the_offset_while_scrolled_back() {
        let mut parser = vt100::Parser::new(6, 20, 1000);
        for i in 0..50 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        // Scroll back a few lines into the history.
        parser.screen_mut().set_scrollback(3);
        assert_eq!(parser.screen().scrollback(), 3);
        // Ten more lines stream in with no further scroll input.
        for i in 50..60 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        // The offset advanced by the ten streamed lines so the same content stays
        // in view — the value the render loop must adopt.
        assert_eq!(
            parser.screen().scrollback(),
            13,
            "the offset must track the lines that streamed in while scrolled back"
        );
    }
}
