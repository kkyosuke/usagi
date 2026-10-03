//! Concrete daemon-owned pseudo-terminal process adapter.
//!
//! The usecase layer deliberately depends on a small PTY port.  This adapter
//! is the sole place that uses `portable-pty`; callers get readers, writers,
//! resizing and child waiting without exposing a local terminal to clients.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::usecase::terminal::{Geometry, PtyWriteError, PtyWriter};

/// A spawned daemon-owned shell terminal.
pub struct PtyTerminal {
    master: Box<dyn MasterPty + Send>,
    master_fd: RawFd,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    writer: Mutex<StallBoundedWriter<Box<dyn Write + Send>, PollReadiness>>,
}

/// Maximum duration of one PTY input write, including partial progress.
///
/// Callers write input while holding daemon-wide runtime locks. A child that
/// stops reading its input (for example because it is itself blocked writing
/// output the daemon cannot drain while that lock is held) would otherwise park
/// the writer forever and every other connection behind it.
const PTY_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Waits for `poll(2)` readiness of one descriptor.
trait Readiness {
    /// Returns `Ok(true)` once `events` is ready (or the descriptor reports a
    /// condition the next read or write will surface), and `Ok(false)` when
    /// `timeout` elapsed first. `None` waits without a bound.
    fn wait(&mut self, events: libc::c_short, timeout: Option<Duration>) -> std::io::Result<bool>;
}

/// `poll(2)` on one descriptor the caller keeps open for every wait.
struct PollReadiness {
    fd: RawFd,
}

impl Readiness for PollReadiness {
    fn wait(&mut self, events: libc::c_short, timeout: Option<Duration>) -> std::io::Result<bool> {
        let timeout = timeout.map_or(-1, |timeout| {
            libc::c_int::try_from(timeout.as_nanos().div_ceil(1_000_000))
                .unwrap_or(libc::c_int::MAX)
        });
        let mut descriptor = libc::pollfd {
            fd: self.fd,
            events,
            revents: 0,
        };
        // SAFETY: `descriptor` is one valid, writable `pollfd` for the call.
        let ready = unsafe { libc::poll(&raw mut descriptor, 1, timeout) };
        poll_outcome(ready, std::io::Error::last_os_error())
    }
}

fn poll_outcome(ready: libc::c_int, error: std::io::Error) -> std::io::Result<bool> {
    match ready {
        -1 => Err(error),
        0 => Ok(false),
        _ => Ok(true),
    }
}

/// Puts the master's open file description in non-blocking mode.
///
/// The writer and every reader share that description, so the flag applies to
/// all of them: writes return what fit instead of parking (see
/// [`StallBoundedWriter`]) and reads are restored to blocking behavior by
/// [`ReadinessReader`].
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: `fcntl` only inspects and updates the status flags of `fd`.
    let flags = fcntl_outcome(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: as above; the new flags keep every existing status bit.
    fcntl_outcome(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(())
}

fn fcntl_outcome(result: libc::c_int) -> std::io::Result<libc::c_int> {
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(result)
}

/// Writes PTY input without ever parking while the input queue is full.
///
/// Each write offers the whole remaining input, so a key sequence or a paste
/// marker reaches the child in one piece whenever the queue has room for it.
/// The entire call has a `stall_timeout` budget: partial progress and interrupted
/// waits never renew it. A full queue waits only for the remaining budget.
/// A terminal that stalled once is then failed at once until a write completes
/// again, so a child that stopped reading costs one bounded wait instead of
/// one per keystroke while the caller's lock is held.
struct StallBoundedWriter<W, R> {
    inner: W,
    readiness: R,
    stall_timeout: Duration,
    stalled: bool,
}

impl<W: Write, R: Readiness> PtyWriter for StallBoundedWriter<W, R> {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        let deadline = Instant::now() + self.stall_timeout;
        self.write_with_budget(bytes, &mut || {
            deadline.saturating_duration_since(Instant::now())
        })
    }
}

impl<W: Write, R: Readiness> StallBoundedWriter<W, R> {
    fn write_with_budget(
        &mut self,
        bytes: &[u8],
        remaining: &mut dyn FnMut() -> Duration,
    ) -> Result<(), PtyWriteError> {
        let mut applied_prefix = 0;
        while applied_prefix < bytes.len() {
            let budget = remaining();
            if budget.is_zero() {
                self.stalled = true;
                return Err(PtyWriteError { applied_prefix });
            }
            match self.inner.write(&bytes[applied_prefix..]) {
                Ok(0) => return Err(PtyWriteError { applied_prefix }),
                Ok(written) => {
                    // A later call may wait again once the child is reading.
                    // Progress never extends this call's absolute deadline.
                    self.stalled = false;
                    applied_prefix += written;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if self.stalled {
                        return Err(PtyWriteError { applied_prefix });
                    }
                    let budget = remaining();
                    if budget.is_zero() {
                        self.stalled = true;
                        return Err(PtyWriteError { applied_prefix });
                    }
                    match self
                        .readiness
                        .wait(libc::POLLOUT, Some(budget.min(self.stall_timeout)))
                    {
                        Ok(true) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Ok(false) => {
                            self.stalled = true;
                            return Err(PtyWriteError { applied_prefix });
                        }
                        Err(_) => return Err(PtyWriteError { applied_prefix }),
                    }
                }
                Err(_) => return Err(PtyWriteError { applied_prefix }),
            }
        }
        Ok(())
    }
}

/// Restores blocking reads on the non-blocking master description.
///
/// Linux reports a closed slave as `EIO` on the master; like the
/// `portable-pty` reader this replaces, that is the end of output.
struct ReadinessReader<I, R> {
    inner: I,
    readiness: R,
}

impl<I: Read, R: Readiness> Read for ReadinessReader<I, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match self.readiness.wait(libc::POLLIN, None) {
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                outcome => return outcome,
            }
        }
    }
}

impl PtyTerminal {
    /// Opens an interactive shell under a new pseudo-terminal in `directory`.
    /// The profile resolver, not an IPC client, chooses the shell program.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot allocate the PTY or
    /// start the selected trusted program.
    pub fn spawn(program: &str, directory: &Path, geometry: Geometry) -> std::io::Result<Self> {
        Self::spawn_with(program, &[], &[], directory, geometry)
    }

    /// Opens a pseudo-terminal running `program` with a rendered argument vector
    /// and environment allowlist values in `directory`. Agent adapters render
    /// the argv/environment once; this adapter never parses a shell command.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot allocate the PTY or
    /// start the selected trusted program.
    pub fn spawn_with(
        program: &str,
        args: &[String],
        environment: &[(String, String)],
        directory: &Path,
        geometry: Geometry,
    ) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: geometry.rows,
                cols: geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_error)?;
        Self::spawn_pair(pair, program, args, environment, directory)
    }

    fn spawn_pair(
        pair: PtyPair,
        program: &str,
        args: &[String],
        environment: &[(String, String)],
        directory: &Path,
    ) -> std::io::Result<Self> {
        // The descriptor is made non-blocking before the child exists, so a
        // failure here leaves no process to reap.
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or(std::io::Error::other("PTY master has no descriptor"))?;
        set_nonblocking(master_fd)?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        // CommandBuilder starts with a snapshot of the daemon environment.
        // The PTY boundary is the final authority: discard that ambient state
        // before rebuilding the child environment from explicit live inputs.
        command.env_clear();
        for (name, value) in environment {
            command.env(name, value);
        }
        command.cwd(directory);
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| io_error_with_context("PTY child spawn failed", error))?;
        drop(pair.slave);
        let writer = pair.master.take_writer().map_err(io_error)?;
        Ok(Self {
            master: pair.master,
            master_fd,
            child: Mutex::new(child),
            // The writer lives beside the master, which keeps `master_fd` open.
            writer: Mutex::new(StallBoundedWriter {
                inner: writer,
                readiness: PollReadiness { fd: master_fd },
                stall_timeout: PTY_INPUT_STALL_TIMEOUT,
                stalled: false,
            }),
        })
    }

    /// Returns a separate reader for the PTY master.  A daemon actor drains it
    /// into its bounded journal before broadcasting output.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system cannot duplicate the PTY
    /// reader.
    pub fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
        // The reader can outlive this terminal, so it waits on a descriptor it
        // owns rather than on `master_fd`, which may be closed and reused.
        // SAFETY: `self.master` keeps `master_fd` open for this borrow.
        let owned = unsafe { BorrowedFd::borrow_raw(self.master_fd) }.try_clone_to_owned()?;
        let reader = std::fs::File::from(owned);
        Ok(Box::new(ReadinessReader {
            readiness: PollReadiness {
                fd: reader.as_raw_fd(),
            },
            inner: reader,
        }))
    }

    /// Returns the child PID observed directly from the freshly spawned PTY.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.lock().ok()?.process_id()
    }

    /// Applies a terminal size change to the daemon-owned master.
    ///
    /// # Errors
    ///
    /// Returns an error when the PTY master rejects the requested geometry.
    pub fn resize(&self, geometry: Geometry) -> std::io::Result<()> {
        self.master
            .resize(PtySize {
                rows: geometry.rows,
                cols: geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_error)
    }

    /// Reaps the child.  This is invoked by the daemon lifecycle worker, never
    /// by a detached client.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be waited for or reports an exit
    /// code outside the supported range.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_real_pty
    pub fn wait(&self) -> std::io::Result<i32> {
        self.child
            .lock()
            .map_err(|_| std::io::Error::other("PTY child lock poisoned"))?
            .wait()
            .map_err(io_error)
            .and_then(|status| i32::try_from(status.exit_code()).map_err(std::io::Error::other))
    }

    /// Reports the child's exit code once it has exited, without waiting.
    ///
    /// # Errors
    ///
    /// Returns an error when the child lock or the status query fails, or the
    /// exit code is outside the supported range.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=waiting_for_exit_leaves_the_terminal_lock_free
    pub fn try_wait(&self) -> std::io::Result<Option<i32>> {
        self.child
            .lock()
            .map_err(|_| std::io::Error::other("PTY child lock poisoned"))?
            .try_wait()?
            .map(|status| i32::try_from(status.exit_code()).map_err(std::io::Error::other))
            .transpose()
    }

    /// Terminates and reaps this daemon-owned child. Used only to compensate a
    /// failed admission commit after the process has already been spawned.
    ///
    /// # Errors
    ///
    /// Returns an error when the child lock, termination, or wait fails.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_real_pty
    pub fn terminate_reap(&self) -> std::io::Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| std::io::Error::other("PTY child lock poisoned"))?;
        child.kill().map_err(io_error)?;
        child.wait().map_err(io_error)?;
        Ok(())
    }
}

impl PtyWriter for PtyTerminal {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_real_pty
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        self.writer
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
    }
}

/// How often [`wait_for_exit`] re-checks a child whose output already ended.
pub const PTY_EXIT_POLL: Duration = Duration::from_millis(50);

/// Waits for the child of a shared terminal to exit without holding the
/// terminal's lock while it waits.
///
/// A blocking `waitpid` under that lock would park every input, resize and
/// close of the terminal, all of which run under daemon-wide runtime locks,
/// for as long as the child lives. That includes the close that would end it.
/// Output can end before the child does, for example when the child closes its
/// terminal descriptors, so the lock is taken only to check, then released.
///
/// # Errors
///
/// Returns an error when the terminal lock is poisoned or the status query
/// fails.
pub fn wait_for_exit(terminal: &Mutex<PtyTerminal>, poll: Duration) -> std::io::Result<i32> {
    loop {
        let Ok(terminal_guard) = terminal.lock() else {
            return Err(std::io::Error::other("PTY terminal lock poisoned"));
        };
        let exited = terminal_guard.try_wait()?;
        drop(terminal_guard);
        if let Some(code) = exited {
            return Ok(code);
        }
        std::thread::sleep(poll);
    }
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn io_error_with_context(context: &str, error: impl std::fmt::Display) -> std::io::Error {
    let error = io_error(error);
    std::io::Error::other(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        PollReadiness, PtyTerminal, Readiness, ReadinessReader, StallBoundedWriter, fcntl_outcome,
        poll_outcome, set_nonblocking, wait_for_exit,
    };
    use crate::usecase::terminal::{Geometry, InputAck, InputRequest, PtyWriter, TerminalRegistry};
    use std::collections::VecDeque;
    use std::io::{Error, ErrorKind, Read, Write};
    use std::sync::Mutex;
    use std::time::Duration;
    use usagi_core::domain::id::{
        ClientId, ConnectionId, DaemonGeneration, RequestId, SessionId, TerminalId, TerminalRef,
        WorkspaceId, WorktreeId,
    };

    enum WriteStep {
        Bytes(usize),
        Interrupted,
        WouldBlock,
        Error,
        Zero,
    }

    struct ScriptedWriter {
        steps: VecDeque<WriteStep>,
        written: Vec<u8>,
        calls: usize,
    }

    impl ScriptedWriter {
        fn new(steps: impl IntoIterator<Item = WriteStep>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                written: Vec::new(),
                calls: 0,
            }
        }
    }

    impl Write for ScriptedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            match self.steps.pop_front().expect("scripted write step") {
                WriteStep::Bytes(count) => {
                    assert!(count <= bytes.len());
                    self.written.extend_from_slice(&bytes[..count]);
                    Ok(count)
                }
                WriteStep::Interrupted => Err(Error::from(ErrorKind::Interrupted)),
                WriteStep::WouldBlock => Err(Error::from(ErrorKind::WouldBlock)),
                WriteStep::Error => Err(Error::other("scripted failure")),
                WriteStep::Zero => Ok(0),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn reference() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn input(
        subscription: u64,
        connection: ConnectionId,
        client: ClientId,
        request: RequestId,
    ) -> InputRequest {
        InputRequest {
            subscription,
            connection,
            client,
            request,
            input_seq: 0,
            operation: None,
        }
    }

    fn run_with_ambient_sentinel(test_name: &str) -> bool {
        if std::env::var_os("USAGI_PTY_TEST_HELPER").is_some() {
            return true;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env("USAGI_PTY_TEST_HELPER", "1")
            .env("USAGI_PTY_SENTINEL", "must-not-leak")
            .status()
            .unwrap();
        assert!(status.success());
        false
    }

    fn output(terminal: &PtyTerminal) -> String {
        let mut output = String::new();
        terminal
            .reader()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        output
    }

    #[test]
    fn daemon_owns_shell_pty_until_it_reaps_the_child() {
        let terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &["-c".to_owned(), "exit 0".to_owned()],
            &[],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    #[test]
    fn spawn_convenience_uses_the_real_pty_boundary() {
        let terminal = PtyTerminal::spawn(
            "/usr/bin/true",
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    #[test]
    fn spawn_failure_preserves_the_pty_stage_and_os_reason() {
        let error = PtyTerminal::spawn(
            "/usagi-test/missing-agent-executable",
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .err()
        .expect("missing executable must be rejected");

        let message = error.to_string();
        assert!(message.contains("PTY child spawn failed"));
        assert!(message.len() > "PTY child spawn failed: ".len());
    }

    #[test]
    fn spawn_with_applies_rendered_argv_and_reaps_the_status() {
        let terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "test \"$USAGI_AGENT\" = 1 || exit 8; exit 7".to_owned(),
            ],
            &[("USAGI_AGENT".to_owned(), "1".to_owned())],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        assert_eq!(terminal.wait().unwrap(), 7);
    }

    #[test]
    fn generic_child_receives_only_its_explicit_public_environment() {
        if !run_with_ambient_sentinel(
            "infrastructure::pty::tests::generic_child_receives_only_its_explicit_public_environment",
        ) {
            return;
        }
        let terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "printf '%s|%s|%s|%s' \"${USAGI_PTY_SENTINEL-unset}\" \"$PATH\" \"$HOME\" \"$TERM\""
                    .to_owned(),
            ],
            &[
                ("PATH".to_owned(), "/allowed/bin".to_owned()),
                ("HOME".to_owned(), "/allowed/home".to_owned()),
                ("TERM".to_owned(), "xterm-256color".to_owned()),
            ],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();

        assert_eq!(
            output(&terminal),
            "unset|/allowed/bin|/allowed/home|xterm-256color"
        );
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    #[test]
    fn empty_environment_does_not_restore_ambient_values() {
        if !run_with_ambient_sentinel(
            "infrastructure::pty::tests::empty_environment_does_not_restore_ambient_values",
        ) {
            return;
        }
        let terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &["-c".to_owned(), "env".to_owned()],
            &[],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();

        let child_output = output(&terminal);
        assert!(!child_output.contains("USAGI_PTY_SENTINEL="));
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    #[test]
    fn duplicate_environment_names_use_the_last_explicit_value() {
        let terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &["-c".to_owned(), "printf %s \"$USAGI_PRIORITY\"".to_owned()],
            &[
                ("USAGI_PRIORITY".to_owned(), "profile".to_owned()),
                ("USAGI_PRIORITY".to_owned(), "provision".to_owned()),
            ],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();

        assert_eq!(output(&terminal), "provision");
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    /// Readiness answers consumed in order; once exhausted it stays ready.
    struct ScriptedReadiness {
        answers: VecDeque<std::io::Result<bool>>,
        waits: Vec<(libc::c_short, Option<Duration>)>,
    }

    impl ScriptedReadiness {
        fn ready() -> Self {
            Self::new([])
        }

        fn new(answers: impl IntoIterator<Item = std::io::Result<bool>>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                waits: Vec::new(),
            }
        }
    }

    impl Readiness for ScriptedReadiness {
        fn wait(
            &mut self,
            events: libc::c_short,
            timeout: Option<Duration>,
        ) -> std::io::Result<bool> {
            self.waits.push((events, timeout));
            self.answers.pop_front().unwrap_or(Ok(true))
        }
    }

    const STALL: Duration = Duration::from_millis(7);

    fn scripted(
        steps: impl IntoIterator<Item = WriteStep>,
        readiness: ScriptedReadiness,
    ) -> StallBoundedWriter<ScriptedWriter, ScriptedReadiness> {
        StallBoundedWriter {
            inner: ScriptedWriter::new(steps),
            readiness,
            stall_timeout: STALL,
            stalled: false,
        }
    }

    #[test]
    fn input_is_offered_whole_so_key_sequences_stay_in_one_write() {
        let mut writer = scripted([WriteStep::Bytes(3)], ScriptedReadiness::ready());

        assert_eq!(writer.write_all(b"\x1b[A"), Ok(()));
        assert_eq!(writer.inner.written, b"\x1b[A");
        assert_eq!(writer.inner.calls, 1);
        assert!(writer.readiness.waits.is_empty());
        assert!(writer.inner.flush().is_ok());
    }

    #[test]
    fn partial_writes_report_the_exact_applied_prefix() {
        let mut writer = scripted(
            [WriteStep::Bytes(2), WriteStep::Bytes(1), WriteStep::Error],
            ScriptedReadiness::ready(),
        );

        assert_eq!(
            writer.write_all(b"hello"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 3 })
        );
        assert_eq!(writer.inner.written, b"hel");
    }

    #[test]
    fn interrupted_write_retries_without_losing_progress() {
        let mut writer = scripted(
            [
                WriteStep::Bytes(2),
                WriteStep::Interrupted,
                WriteStep::Bytes(3),
            ],
            ScriptedReadiness::ready(),
        );

        assert_eq!(writer.write_all(b"hello"), Ok(()));
        assert_eq!(writer.inner.written, b"hello");
        assert_eq!(writer.inner.calls, 3);
    }

    #[test]
    fn write_zero_reports_the_prefix_already_applied() {
        let mut writer = scripted(
            [WriteStep::Bytes(2), WriteStep::Zero],
            ScriptedReadiness::ready(),
        );

        assert_eq!(
            writer.write_all(b"hello"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 2 })
        );
        assert_eq!(writer.inner.written, b"he");
    }

    #[test]
    fn a_full_queue_waits_for_room_and_then_writes_the_rest() {
        let mut writer = scripted(
            [
                WriteStep::Bytes(2),
                WriteStep::WouldBlock,
                WriteStep::Bytes(3),
            ],
            ScriptedReadiness::ready(),
        );

        assert_eq!(writer.write_all(b"hello"), Ok(()));
        assert_eq!(writer.inner.written, b"hello");
        assert_eq!(writer.readiness.waits.len(), 1);
        let (events, budget) = writer.readiness.waits[0];
        assert_eq!(events, libc::POLLOUT);
        assert!(budget.is_some_and(|budget| !budget.is_zero() && budget <= STALL));
    }

    #[test]
    fn a_stall_fails_with_its_prefix_and_later_writes_fail_without_waiting() {
        let mut writer = scripted(
            [
                WriteStep::Bytes(2),
                WriteStep::WouldBlock,
                WriteStep::WouldBlock,
            ],
            ScriptedReadiness::new([Ok(false)]),
        );

        assert_eq!(
            writer.write_all(b"hello"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 2 })
        );
        assert_eq!(
            writer.write_all(b"x"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 0 })
        );
        assert_eq!(writer.readiness.waits.len(), 1);
        let (events, budget) = writer.readiness.waits[0];
        assert_eq!(events, libc::POLLOUT);
        assert!(budget.is_some_and(|budget| !budget.is_zero() && budget <= STALL));
    }

    #[test]
    fn progress_clears_the_stall_so_a_later_full_queue_waits_again() {
        let mut writer = scripted(
            [
                WriteStep::WouldBlock,
                WriteStep::Bytes(1),
                WriteStep::WouldBlock,
                WriteStep::Bytes(1),
            ],
            ScriptedReadiness::new([Ok(false)]),
        );

        assert!(writer.write_all(b"a").is_err());
        assert!(writer.stalled);
        // A partial write that then meets a full queue waits again instead of
        // dropping the rest of a paste the child has started consuming.
        assert_eq!(writer.write_all(b"bc"), Ok(()));
        assert!(!writer.stalled);
        assert_eq!(writer.inner.written, b"bc");
        assert_eq!(writer.readiness.waits.len(), 2);
    }

    #[test]
    fn partial_progress_and_interruptions_do_not_renew_the_write_budget() {
        let mut writer = scripted(
            [
                WriteStep::WouldBlock,
                WriteStep::Bytes(1),
                WriteStep::WouldBlock,
                WriteStep::Bytes(1),
                WriteStep::WouldBlock,
            ],
            ScriptedReadiness::ready(),
        );
        let mut budgets = [7, 7, 5, 3, 3, 1, 0].into_iter();
        assert_eq!(
            writer.write_with_budget(b"abc", &mut || {
                Duration::from_millis(budgets.next().unwrap())
            }),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 2 })
        );
        assert_eq!(writer.inner.written, b"ab");
        assert_eq!(
            writer.readiness.waits,
            vec![
                (libc::POLLOUT, Some(STALL)),
                (libc::POLLOUT, Some(Duration::from_millis(3)))
            ]
        );
        assert!(writer.stalled);
        assert!(writer.write_all(b"c").is_err());
        assert_eq!(writer.readiness.waits.len(), 2);

        let mut interrupted = scripted([WriteStep::Interrupted], ScriptedReadiness::ready());
        let mut budgets = [1, 0].into_iter();
        assert_eq!(
            interrupted.write_with_budget(b"x", &mut || {
                Duration::from_millis(budgets.next().unwrap())
            }),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 0 })
        );
        assert!(interrupted.stalled);

        let mut exhausted = scripted([WriteStep::WouldBlock], ScriptedReadiness::ready());
        let mut budgets = [1, 0].into_iter();
        assert_eq!(
            exhausted.write_with_budget(b"x", &mut || {
                Duration::from_millis(budgets.next().unwrap())
            }),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 0 })
        );
        assert!(exhausted.stalled);
        assert!(exhausted.readiness.waits.is_empty());
    }

    #[test]
    fn interrupted_readiness_waits_again_and_other_failures_fail_closed() {
        let mut interrupted = scripted(
            [
                WriteStep::WouldBlock,
                WriteStep::WouldBlock,
                WriteStep::Bytes(1),
            ],
            ScriptedReadiness::new([Err(Error::from(ErrorKind::Interrupted))]),
        );
        assert_eq!(interrupted.write_all(b"h"), Ok(()));
        assert_eq!(interrupted.readiness.waits.len(), 2);

        let mut failed = scripted(
            [WriteStep::WouldBlock],
            ScriptedReadiness::new([Err(Error::other("poll"))]),
        );
        assert_eq!(
            failed.write_all(b"h"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 0 })
        );
        assert!(!failed.stalled);
    }

    enum ReadStep {
        Data(&'static [u8]),
        WouldBlock,
        Hangup,
        Error,
    }

    struct ScriptedReader(VecDeque<ReadStep>);

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front().expect("scripted read step") {
                ReadStep::Data(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                ReadStep::WouldBlock => Err(Error::from(ErrorKind::WouldBlock)),
                ReadStep::Hangup => Err(Error::from_raw_os_error(libc::EIO)),
                ReadStep::Error => Err(Error::other("scripted read failure")),
            }
        }
    }

    fn reader(
        steps: impl IntoIterator<Item = ReadStep>,
        readiness: ScriptedReadiness,
    ) -> ReadinessReader<ScriptedReader, ScriptedReadiness> {
        ReadinessReader {
            inner: ScriptedReader(steps.into_iter().collect()),
            readiness,
        }
    }

    #[test]
    fn reads_wait_without_a_bound_instead_of_reporting_would_block() {
        let mut reader = reader(
            [
                ReadStep::WouldBlock,
                ReadStep::WouldBlock,
                ReadStep::Data(b"ok"),
            ],
            ScriptedReadiness::new([Err(Error::from(ErrorKind::Interrupted))]),
        );
        let mut buffer = [0; 4];

        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"ok");
        assert_eq!(
            reader.readiness.waits,
            vec![(libc::POLLIN, None), (libc::POLLIN, None)]
        );
    }

    #[test]
    fn a_closed_slave_ends_the_output_instead_of_failing_the_read() {
        let mut reader = reader([ReadStep::Hangup], ScriptedReadiness::ready());

        assert_eq!(reader.read(&mut [0; 4]).unwrap(), 0);
    }

    #[test]
    fn read_and_readiness_failures_reach_the_reader_caller() {
        let mut buffer = [0; 4];
        let mut failed_read = reader([ReadStep::Error], ScriptedReadiness::ready());
        assert_eq!(
            failed_read.read(&mut buffer).unwrap_err().to_string(),
            "scripted read failure"
        );

        let mut failed_wait = reader(
            [ReadStep::WouldBlock],
            ScriptedReadiness::new([Err(Error::other("poll"))]),
        );
        assert_eq!(
            failed_wait.read(&mut buffer).unwrap_err().to_string(),
            "poll"
        );
    }

    #[test]
    fn poll_and_fcntl_outcomes_map_failures() {
        assert_eq!(
            poll_outcome(-1, Error::other("poll failed"))
                .unwrap_err()
                .to_string(),
            "poll failed"
        );
        assert!(!poll_outcome(0, Error::other("unused")).unwrap());
        assert!(poll_outcome(1, Error::other("unused")).unwrap());
        assert!(fcntl_outcome(-1).is_err());
        assert_eq!(fcntl_outcome(3).unwrap(), 3);
        assert!(set_nonblocking(-1).is_err());
    }

    #[test]
    fn pty_error_conversion_preserves_the_failure_details() {
        let converted = super::io_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "terminal access denied",
        ));
        assert_eq!(converted.kind(), std::io::ErrorKind::Other);
        assert_eq!(converted.to_string(), "terminal access denied");
        let contextual =
            super::io_error_with_context("allocate PTY", anyhow::anyhow!("no terminal available"));
        assert_eq!(contextual.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            contextual.to_string(),
            "allocate PTY: no terminal available"
        );
    }

    #[test]
    fn real_poll_readiness_times_out_and_reports_ready() {
        let (read, write) = std::io::pipe().unwrap();
        let mut readable = PollReadiness {
            fd: std::os::fd::AsRawFd::as_raw_fd(&read),
        };
        assert!(
            !readable
                .wait(libc::POLLIN, Some(Duration::from_millis(1)))
                .unwrap()
        );
        let mut writable = PollReadiness {
            fd: std::os::fd::AsRawFd::as_raw_fd(&write),
        };
        assert!(writable.wait(libc::POLLOUT, None).unwrap());
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=real_pty_input_to_a_child_that_never_reads_fails_instead_of_blocking
    fn configure_raw_input(fd: std::os::fd::RawFd) -> std::io::Result<()> {
        // Canonical line discipline can discard overflow on Linux. Raw input
        // with echo disabled makes the unread queue apply pressure.
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: the caller owns this open terminal descriptor; tcgetattr
        // initializes mode before it is read by cfmakeraw.
        fcntl_outcome(unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) })?;
        // SAFETY: tcgetattr succeeded, initializing this termios value.
        let mut mode = unsafe { mode.assume_init() };
        // SAFETY: mode was initialized by tcgetattr and is writable.
        unsafe { libc::cfmakeraw(&raw mut mode) };
        // SAFETY: this owned terminal and initialized mode remain valid.
        fcntl_outcome(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const mode) })?;
        Ok(())
    }

    #[test]
    fn real_pty_input_to_a_child_that_never_reads_fails_instead_of_blocking() {
        // The deadlock this bounds: an Agent stops reading input while the
        // daemon writes to it under a runtime lock. A blocking write parks
        // forever once the PTY input queue is full, even after the child dies.
        let mut terminal = PtyTerminal::spawn_with(
            "/bin/sleep",
            &["30".to_owned()],
            &[],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        // Fill the kernel flip buffers as well as the line discipline queue:
        // Linux can accept hundreds of KiB without the slave reading input.
        let input = vec![b'x'; 8 * 1024 * 1024];
        let observations = configure_raw_input(terminal.master_fd).map(|()| {
            terminal.writer.lock().unwrap().stall_timeout = Duration::from_millis(200);

            let started = std::time::Instant::now();
            let first = terminal.write_all(&input);
            let first_elapsed = started.elapsed();
            let started = std::time::Instant::now();
            let second = terminal.write_all(b"x");
            (first, first_elapsed, second, started.elapsed())
        });
        // Reap before asserting either outcome, including a failed setup, so
        // a failing regression never leaves its sleep child running.
        terminal.terminate_reap().unwrap();

        let (first, first_elapsed, second, second_elapsed) = observations.unwrap();
        let error = first.unwrap_err();
        assert!(error.applied_prefix > 0);
        assert!(error.applied_prefix < input.len());
        assert!(first_elapsed >= Duration::from_millis(200));

        assert_eq!(
            second.unwrap_err().applied_prefix,
            0,
            "a stalled terminal fails later input without waiting again"
        );
        assert!(second_elapsed < Duration::from_millis(200));
    }

    #[test]
    fn real_pty_round_trips_input_through_the_non_blocking_master() {
        let mut terminal = PtyTerminal::spawn_with(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "read line; printf '<%s>' \"$line\"".to_owned(),
            ],
            &[],
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        let mut reader = terminal.reader().unwrap();
        terminal.write_all(b"usagi\n").unwrap();
        let mut output = Vec::new();
        let mut buffer = [0; 256];
        while !String::from_utf8_lossy(&output).contains("<usagi>") {
            let read = reader.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "output ended before the echoed line");
            output.extend_from_slice(&buffer[..read]);
        }
        assert_eq!(terminal.wait().unwrap(), 0);
    }

    #[test]
    fn waiting_for_exit_leaves_the_terminal_lock_free() {
        let terminal = Mutex::new(
            PtyTerminal::spawn_with(
                "/bin/sh",
                &["-c".to_owned(), "read line; exit 3".to_owned()],
                &[],
                std::path::Path::new("/"),
                Geometry { cols: 80, rows: 24 },
            )
            .unwrap(),
        );

        // As in the daemon, output is drained: on macOS the child's last
        // terminal close waits for unread output.
        let mut reader = terminal.lock().unwrap().reader().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
            let waiter = scope.spawn(|| wait_for_exit(&terminal, Duration::from_millis(5)));
            // The child exits only after this input, which needs the lock the
            // waiter would have held for the whole wait.
            std::thread::sleep(Duration::from_millis(30));
            terminal.lock().unwrap().write_all(b"done\n").unwrap();
            assert_eq!(waiter.join().unwrap().unwrap(), 3);
        });
    }

    #[test]
    fn waiting_for_exit_reports_a_poisoned_terminal_lock() {
        let terminal = Mutex::new(
            PtyTerminal::spawn(
                "/usr/bin/true",
                std::path::Path::new("/"),
                Geometry { cols: 80, rows: 24 },
            )
            .unwrap(),
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = terminal.lock().unwrap();
            panic!("poison the terminal lock");
        }));

        assert!(wait_for_exit(&terminal, Duration::from_millis(5)).is_err());
        let poisoned = terminal.into_inner().err().unwrap();
        assert_eq!(poisoned.into_inner().wait().unwrap(), 0);
    }

    #[test]
    fn real_pty_write_path_preserves_safe_and_ambiguous_operation_replay() {
        let terminal = reference();
        let mut registry = TerminalRegistry::new(4, 2);
        registry
            .register(terminal.clone(), Geometry { cols: 80, rows: 24 })
            .unwrap();
        let connection = ConnectionId::new();
        let client = ClientId::new();
        let subscription = registry.attach(&terminal, connection).unwrap().subscription;

        let ambiguous_request = RequestId::new();
        let ambiguous_input = input(subscription, connection, client, ambiguous_request);
        let mut partial = scripted(
            [WriteStep::Bytes(2), WriteStep::Error],
            ScriptedReadiness::ready(),
        );
        assert_eq!(
            registry
                .write_input(&terminal, ambiguous_input, b"hello", 0, &mut partial)
                .unwrap(),
            InputAck::Ambiguous { applied_prefix: 2 }
        );
        assert_eq!(partial.inner.written, b"he");
        assert_eq!(
            registry
                .write_input(&terminal, ambiguous_input, b"hello", 0, &mut partial)
                .unwrap(),
            InputAck::Cached(Box::new(InputAck::Ambiguous { applied_prefix: 2 }))
        );
        assert_eq!(partial.inner.written, b"he");

        let mut safe_registry = TerminalRegistry::new(4, 2);
        safe_registry
            .register(terminal.clone(), Geometry { cols: 80, rows: 24 })
            .unwrap();
        let safe_subscription = safe_registry
            .attach(&terminal, connection)
            .unwrap()
            .subscription;
        let safe_request = RequestId::new();
        let safe_input = input(safe_subscription, connection, client, safe_request);
        let mut failed = scripted([WriteStep::Error], ScriptedReadiness::ready());
        assert_eq!(
            safe_registry
                .write_input(&terminal, safe_input, b"hello", 0, &mut failed)
                .unwrap(),
            InputAck::Failed
        );
        assert_eq!(
            safe_registry
                .write_input(&terminal, safe_input, b"hello", 0, &mut failed)
                .unwrap(),
            InputAck::Cached(Box::new(InputAck::Failed))
        );
        assert!(failed.inner.written.is_empty());
    }
}
