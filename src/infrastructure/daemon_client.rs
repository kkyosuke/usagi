//! The TUI's attach client for a daemon-owned terminal.
//!
//! A [`DaemonTerminal`] is the remote counterpart of a directly-owned
//! [`PtySession`](crate::infrastructure::pty::PtySession), exposing the same
//! surface the terminal pool and the pane render loop drive — a bounded local
//! [`vt100::Parser`] for the current viewport, generation / bell / liveness
//! counters, input and resize — while the process and full scrollback live in
//! the daemon. It connects to the daemon's Unix socket, performs the
//! `Spawn`→`Spawned` and `Attach`→`Attached` handshake, and then a background
//! thread folds daemon `Screen` viewport snapshots and live raw `Output` deltas
//! into the local parser. Historical scrolling sends `Scrollback` requests and
//! receives bounded snapshots instead of replaying or retaining full history in
//! the TUI process.
//!
//! Dropping a `DaemonTerminal` only detaches (the daemon keeps the terminal
//! running — that is the point of daemon ownership); [`DaemonTerminal::kill`]
//! is the explicit teardown for a user closing the pane.
//!
//! This module is pure socket and thread IO, so it is excluded from coverage
//! (cf. `pty.rs`); the protocol decisions it drives — handshake reply matching
//! and screen-feed folding — live in [`crate::usecase::daemon_attach`] and the
//! message/framing shapes in [`crate::domain::daemon_ipc`], all unit-tested.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::domain::daemon_ipc::{ClientMessage, FrameDecoder, ServerMessage, TerminalId};
use crate::infrastructure::daemon_ipc::{decode_message, encode_message, socket_path};
use crate::infrastructure::daemon_store;
use crate::infrastructure::pty::ScreenCallbacks;
use crate::usecase::daemon_attach::{
    attach_reply, drain_buffered_frames, spawn_reply, AttachReply, DrainOutcome, ScreenSink,
    SpawnReply,
};

/// How long a connect keeps retrying while a daemon record exists (the daemon
/// was just autospawned and is still binding its socket). Without a record the
/// first failure is final, so a machine with no daemon falls back instantly.
const CONNECT_DEADLINE: Duration = Duration::from_secs(3);
/// How long the `Spawn` / `Attach` handshake waits for its reply. Generous: the
/// daemon answers within one of its fast ticks, but it may be busy spawning.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
/// How long an explicit `Kill` waits for the daemon's `Killed` acknowledgement
/// before reporting teardown failure to the caller.
const KILL_ACK_DEADLINE: Duration = Duration::from_secs(2);
/// The pause between connect retries, and the read timeout the handshake polls
/// the socket with.
const RETRY_PAUSE: Duration = Duration::from_millis(50);
/// The reader thread's stack, for the same reason as the PTY reader's: it only
/// loops over a blocking `read` and hands bytes to the decoder/parser.
const READER_STACK_BYTES: usize = 256 * 1024;

/// Cloneable handle for injecting input into a daemon terminal from outside the
/// render loop — the remote counterpart of
/// [`PtyInputHandle`](crate::infrastructure::pty::PtyInputHandle), used by the
/// terminal-pool watcher to type MCP-sent prompts into a detached agent pane.
#[derive(Clone)]
pub struct DaemonInputHandle {
    stream: Arc<Mutex<UnixStream>>,
    terminal: TerminalId,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
}

impl DaemonInputHandle {
    /// Whether the program running in the daemon terminal has asked for
    /// bracketed paste mode (DECSET 2004) — read from the local replayed
    /// parser, which saw the same enabling sequence the daemon's did.
    pub fn bracketed_paste(&self) -> bool {
        self.parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .screen()
            .bracketed_paste()
    }

    /// Forward raw input bytes to the daemon terminal as a `Keys` message.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        send(
            &self.stream,
            &ClientMessage::Keys {
                terminal: self.terminal,
                data: bytes.to_vec(),
            },
        )
    }
}

/// A daemon-owned terminal this process is attached to: a live view (and input
/// path) onto a shell whose process belongs to the daemon.
pub struct DaemonTerminal {
    terminal: TerminalId,
    pid: u32,
    stream: Arc<Mutex<UnixStream>>,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
    alive: Arc<AtomicBool>,
    killed_ack: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    bell: Arc<AtomicU64>,
    cursor_shape: Arc<AtomicU16>,
    scrollback: Arc<AtomicUsize>,
    /// Set once [`kill`](Self::kill) ran, so `Drop` neither detaches (the
    /// terminal is gone) nor leaves the terminal running.
    killed: bool,
    reader_thread: Option<JoinHandle<()>>,
}

/// Encode `message` and write it to the shared stream under its lock.
fn send(stream: &Arc<Mutex<UnixStream>>, message: &ClientMessage) -> Result<()> {
    let bytes = encode_message(message)?;
    let mut stream = stream
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stream
        .write_all(&bytes)
        .context("writing to the daemon socket")
}

/// Connect to the daemon socket under `daemon_dir`. Retries while a daemon
/// record exists (a just-spawned daemon is still coming up); with no record the
/// first failure is final so the caller can fall back to a local PTY at once.
fn connect(daemon_dir: &Path) -> Result<UnixStream> {
    let socket = socket_path(daemon_dir);
    let deadline = Instant::now() + CONNECT_DEADLINE;
    loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                let registered = daemon_store::read(daemon_dir).ok().flatten().is_some();
                if !registered || Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!("connecting to the usagi daemon at {}", socket.display())
                    });
                }
                std::thread::sleep(RETRY_PAUSE);
            }
        }
    }
}

/// Read framed [`ServerMessage`]s from `stream` until `judge` settles on one,
/// or the handshake deadline passes. Bytes past the settling frame stay in
/// `decoder` for the reader thread to continue from.
fn await_reply<T>(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    mut judge: impl FnMut(&ServerMessage) -> Option<T>,
) -> Result<T> {
    stream
        .set_read_timeout(Some(RETRY_PAUSE))
        .context("configuring the daemon socket")?;
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    while Instant::now() < deadline {
        while let Some(frame) = decoder.next_frame()? {
            let message: ServerMessage = decode_message(&frame)?;
            if let Some(settled) = judge(&message) {
                return Ok(settled);
            }
        }
        match stream.read(&mut buf) {
            Ok(0) => bail!("the daemon closed the connection during the handshake"),
            Ok(n) => decoder.feed(&buf[..n]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => {
                return Err(error).context("reading the daemon's handshake reply");
            }
        }
    }
    bail!("timed out waiting for the daemon's handshake reply")
}

/// The reader thread's sink: folds the daemon's screen feed into the shared
/// parser and wakes the render loop, mirroring what the PTY reader thread does
/// for a locally-owned session. Holds the parser weakly so a dropped terminal
/// frees its grid even while this thread is blocked in `read`.
struct ParserSink {
    parser: Weak<Mutex<vt100::Parser<ScreenCallbacks>>>,
    alive: Arc<AtomicBool>,
    scrollback: Arc<AtomicUsize>,
    /// Set when the parser has been dropped — the thread's signal to stop.
    orphaned: bool,
}

impl ParserSink {
    fn process(&mut self, bytes: &[u8]) {
        match self.parser.upgrade() {
            Some(parser) => {
                if let Ok(mut parser) = parser.lock() {
                    parser.process(bytes);
                }
            }
            None => self.orphaned = true,
        }
    }
}

impl ScreenSink for ParserSink {
    fn replace_screen(&mut self, contents: &[u8], scrollback: usize) {
        // A snapshot begins with a full clear, so processing it repaints the
        // grid without accumulating stale rows.
        self.scrollback.store(scrollback, Ordering::SeqCst);
        self.process(contents);
    }

    fn apply_output(&mut self, data: &[u8]) {
        self.scrollback.store(0, Ordering::SeqCst);
        self.process(data);
    }

    fn exited(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    fn orphaned(&self) -> bool {
        self.orphaned
    }
}

impl DaemonTerminal {
    /// Ask the daemon under `daemon_dir` to spawn a new terminal in `worktree`
    /// (running `command` when given, a plain shell otherwise) and attach to
    /// it. `env` is the resolved workspace environment for the child process;
    /// `scrollback` caps both the daemon's and this client's local scrollback.
    pub fn spawn(
        daemon_dir: &Path,
        worktree: &Path,
        rows: u16,
        cols: u16,
        command: Option<&str>,
        scrollback: usize,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut stream = connect(daemon_dir)?;
        let mut decoder = FrameDecoder::new();
        let spawn = ClientMessage::Spawn {
            worktree: worktree.to_path_buf(),
            command: command.map(str::to_string),
            env: env.clone(),
            cols,
            rows,
            scrollback,
        };
        stream
            .write_all(&encode_message(&spawn)?)
            .context("sending the spawn request to the daemon")?;
        let (terminal, pid) =
            await_reply(&mut stream, &mut decoder, |message| {
                match spawn_reply(message) {
                    SpawnReply::Ready { terminal, pid } => Some(Ok((terminal, pid))),
                    SpawnReply::Rejected(reason) => {
                        Some(Err(anyhow!("the daemon refused to spawn: {reason}")))
                    }
                    SpawnReply::NotYet => None,
                }
            })??;
        Self::attach_over(
            stream, decoder, worktree, terminal, pid, rows, cols, scrollback,
        )
    }

    /// Attach to the already-running daemon terminal `terminal` in `worktree` —
    /// the restore path for a pane whose persisted snapshot recorded the id.
    /// Fails when the daemon does not know the id (it restarted, or the
    /// terminal exited), in which case the caller spawns afresh.
    pub fn attach(
        daemon_dir: &Path,
        worktree: &Path,
        terminal: TerminalId,
        rows: u16,
        cols: u16,
        scrollback: usize,
    ) -> Result<Self> {
        let stream = connect(daemon_dir)?;
        let decoder = FrameDecoder::new();
        Self::attach_over(
            stream, decoder, worktree, terminal, 0, rows, cols, scrollback,
        )
    }

    /// The shared attach tail of [`spawn`](Self::spawn) and
    /// [`attach`](Self::attach): subscribe to the terminal's screen feed, adopt
    /// this client's geometry, and hand the connection to the reader thread.
    #[allow(clippy::too_many_arguments)]
    fn attach_over(
        mut stream: UnixStream,
        mut decoder: FrameDecoder,
        worktree: &Path,
        terminal: TerminalId,
        spawned_pid: u32,
        rows: u16,
        cols: u16,
        _scrollback: usize,
    ) -> Result<Self> {
        stream
            .write_all(&encode_message(&ClientMessage::Attach {
                terminal,
                worktree: worktree.to_path_buf(),
            })?)
            .context("sending the attach request to the daemon")?;
        let pid = await_reply(&mut stream, &mut decoder, |message| {
            match attach_reply(message, terminal) {
                AttachReply::Ready { pid } => Some(Ok(pid)),
                AttachReply::Rejected(reason) => {
                    Some(Err(anyhow!("the daemon refused the attach: {reason}")))
                }
                AttachReply::NotYet => None,
            }
        })??;
        let pid = if pid == 0 { spawned_pid } else { pid };

        // Adopt this client's geometry: an attach to an already-running
        // terminal inherits whatever size it last had, which rarely matches
        // this pane. (Both sides resize their parser; the application repaints
        // via SIGWINCH.)
        stream
            .write_all(&encode_message(&ClientMessage::Resize {
                terminal,
                cols,
                rows,
            })?)
            .context("sending the initial resize to the daemon")?;

        // Back to blocking reads for the reader thread (the handshake polled
        // with a short timeout; the option is shared with the clone below).
        stream
            .set_read_timeout(None)
            .context("configuring the daemon socket")?;

        let bell = Arc::new(AtomicU64::new(0));
        let cursor_shape = Arc::new(AtomicU16::new(0));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            0,
            ScreenCallbacks::new(Arc::clone(&bell), Arc::clone(&cursor_shape)),
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));
        let scrollback_state = Arc::new(AtomicUsize::new(0));
        let killed_ack = Arc::new(AtomicBool::new(false));

        let reader_stream = stream
            .try_clone()
            .context("cloning the daemon socket for the reader thread")?;
        let reader_thread = {
            let mut sink = ParserSink {
                parser: Arc::downgrade(&parser),
                alive: Arc::clone(&alive),
                scrollback: Arc::clone(&scrollback_state),
                orphaned: false,
            };
            let generation = Arc::clone(&generation);
            let alive = Arc::clone(&alive);
            let killed_ack = Arc::clone(&killed_ack);
            std::thread::Builder::new()
                .name("usagi-daemon-attach".to_string())
                .stack_size(READER_STACK_BYTES)
                .spawn(move || {
                    // Mark the terminal dead and wake the render loop on any
                    // exit — clean EOF (the daemon went away, taking the
                    // terminal with it), a read error, or a decode panic —
                    // mirroring the PTY reader's drop guard.
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
                    let mut stream = reader_stream;
                    let mut buf = vec![0u8; 64 * 1024];
                    // Drain before blocking: the attach handshake can pull the
                    // daemon's initial `Screen` snapshot (sent right behind
                    // `Attached`) into the decoder, and an idle terminal sends
                    // nothing further to flush it out — a read-first loop
                    // would leave the pane blank.
                    while drain_buffered_frames(
                        &mut decoder,
                        terminal,
                        &mut sink,
                        &mut |payload| {
                            let message = decode_message::<ServerMessage>(payload).ok()?;
                            if matches!(
                                message,
                                ServerMessage::Killed { terminal: id } if id == terminal
                            ) {
                                killed_ack.store(true, Ordering::SeqCst);
                                return None;
                            }
                            Some(message)
                        },
                        &mut || {
                            generation.fetch_add(1, Ordering::SeqCst);
                        },
                    ) == DrainOutcome::NeedMoreBytes
                    {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => decoder.feed(&buf[..n]),
                        }
                    }
                })
                .context("failed to spawn the daemon attach reader thread")?
        };

        Ok(Self {
            terminal,
            pid,
            stream: Arc::new(Mutex::new(stream)),
            parser,
            alive,
            killed_ack,
            generation,
            bell,
            cursor_shape,
            scrollback: scrollback_state,
            killed: false,
            reader_thread: Some(reader_thread),
        })
    }

    /// The daemon-assigned id of this terminal — what a persisted pane snapshot
    /// records so the next TUI run can re-attach instead of respawning.
    pub fn terminal_id(&self) -> TerminalId {
        self.terminal
    }

    /// Lock the locally replayed screen-grid parser to read the current
    /// contents (for rendering). Poison-recovering for the same reason as
    /// [`PtySession::parser`](crate::infrastructure::pty::PtySession::parser).
    pub fn parser(&self) -> MutexGuard<'_, vt100::Parser<ScreenCallbacks>> {
        self.parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the program in the daemon terminal has asked for bracketed
    /// paste mode (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.parser().screen().bracketed_paste()
    }

    /// The running count of audible bells, replayed from the daemon's output
    /// stream. Counts only bells rung since this client attached.
    pub fn bell_count(&self) -> u64 {
        self.bell.load(Ordering::SeqCst)
    }

    /// A shared handle to the bell counter for the pool watcher.
    pub fn bell_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.bell)
    }

    /// A shared handle to the local parser for the pool watcher's off-loop
    /// scans (PR URLs).
    pub fn parser_handle(&self) -> Arc<Mutex<vt100::Parser<ScreenCallbacks>>> {
        Arc::clone(&self.parser)
    }

    /// A shared handle to the update generation counter.
    pub fn generation_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// The daemon-side shell's pid — the root the resource sampler totals the
    /// session's process tree from. `None` when it is unknown (`0`).
    pub fn process_id(&self) -> Option<u32> {
        (self.pid != 0).then_some(self.pid)
    }

    /// The cursor shape (DECSCUSR `Ps`) last selected, replayed from the output
    /// stream; `0` until the program picks one after this client attached.
    pub fn cursor_shape(&self) -> u16 {
        self.cursor_shape.load(Ordering::SeqCst)
    }

    /// A shared handle to the liveness flag for the pool watcher.
    pub fn alive_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// A cloneable handle that can write to the daemon terminal without
    /// borrowing this session — the watcher's prompt-injection path.
    pub fn input_handle(&self) -> DaemonInputHandle {
        DaemonInputHandle {
            stream: Arc::clone(&self.stream),
            terminal: self.terminal,
            parser: Arc::clone(&self.parser),
        }
    }

    /// Forward raw input bytes to the daemon terminal.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.input_handle().write(bytes)
    }

    /// Resize the daemon terminal (and the local grid) to `rows`×`cols`. The
    /// local grid resizes immediately so the next frame draws at the pane's
    /// size; the daemon resizes its PTY and authoritative parser on receipt.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = send(
            &self.stream,
            &ClientMessage::Resize {
                terminal: self.terminal,
                cols,
                rows,
            },
        );
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
    }

    /// Scroll the local screen `offset` lines back into the replayed history
    /// (`0` is the live screen), returning the offset actually applied.
    pub fn set_scrollback(&mut self, offset: usize) -> usize {
        let _ = send(
            &self.stream,
            &ClientMessage::Scrollback {
                terminal: self.terminal,
                offset,
            },
        );
        self.scrollback.store(offset, Ordering::SeqCst);
        offset
    }

    /// The scroll offset currently applied to the replayed history.
    pub fn scrollback(&self) -> usize {
        self.scrollback.load(Ordering::SeqCst)
    }

    /// Whether the daemon terminal is still running, as far as this client
    /// knows: `false` once the daemon reported its exit, it was killed, or the
    /// connection to the daemon was lost (the daemon dying kills its
    /// terminals, so a lost connection means the terminal is gone too).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// A counter bumped for every screen update folded in (and once more on
    /// exit), for the render loop's redraw check.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Kill the daemon-owned terminal — the explicit teardown for a user
    /// closing the pane, as opposed to `Drop`'s detach-and-leave-running.
    pub fn kill(&mut self) -> bool {
        if self.killed {
            return true;
        }
        if send(
            &self.stream,
            &ClientMessage::Kill {
                terminal: self.terminal,
            },
        )
        .is_err()
        {
            return false;
        }
        self.killed = true;
        let deadline = Instant::now() + KILL_ACK_DEADLINE;
        while Instant::now() < deadline {
            if self.killed_ack.load(Ordering::SeqCst) || !self.alive.load(Ordering::SeqCst) {
                self.alive.store(false, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(RETRY_PAUSE);
        }
        false
    }
}

impl Drop for DaemonTerminal {
    fn drop(&mut self) {
        // Detach, never kill: the terminal (and the agent inside it) belongs to
        // the daemon and keeps running for the next attach. After an explicit
        // `kill` there is nothing to detach from.
        if !self.killed {
            let _ = send(
                &self.stream,
                &ClientMessage::Detach {
                    terminal: self.terminal,
                },
            );
        }
        // Unblock the reader thread's `read` so the join below returns: closing
        // this end makes its (shared) socket yield EOF.
        if let Ok(stream) = self.stream.lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
    }
}
