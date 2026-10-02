//! Concrete daemon-owned pseudo-terminal process adapter.
//!
//! The usecase layer deliberately depends on a small PTY port.  This adapter
//! is the sole place that uses `portable-pty`; callers get readers, writers,
//! resizing and child waiting without exposing a local terminal to clients.

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::usecase::terminal::{Geometry, PtyWriteError, PtyWriter};

/// A spawned daemon-owned shell terminal.
pub struct PtyTerminal {
    master: Box<dyn MasterPty + Send>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    writer: Mutex<AppliedPrefixWriter<Box<dyn Write + Send>, PollWritable>>,
}

/// How long PTY input may make no progress before the write fails.
///
/// Callers write input while holding daemon-wide runtime locks. A child that
/// stops reading its input (for example because it is itself blocked writing
/// output the daemon cannot drain while that lock is held) would otherwise park
/// the writer forever and every other connection behind it.
const PTY_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Waits until the PTY input side accepts at least one byte.
trait WriteReadiness {
    /// Returns `Ok(true)` once one byte can be written without blocking (or
    /// the descriptor reports a condition the write itself will surface), and
    /// `Ok(false)` when `timeout` elapsed first.
    fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<bool>;
}

/// `poll(2)` readiness of the PTY master shared with the writer.
///
/// `O_NONBLOCK` cannot bound the write instead: the writer and the output
/// reader share one open file description, so the flag would also turn the
/// reader's blocking reads into spurious `EAGAIN` failures.
struct PollWritable {
    fd: RawFd,
}

impl WriteReadiness for PollWritable {
    fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<bool> {
        let timeout = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        let mut descriptor = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
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

struct AppliedPrefixWriter<W, R> {
    inner: W,
    readiness: R,
    stall_timeout: Duration,
}

impl<W: Write, R: WriteReadiness> PtyWriter for AppliedPrefixWriter<W, R> {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        let mut applied_prefix = 0;
        while applied_prefix < bytes.len() {
            match self.readiness.wait_writable(self.stall_timeout) {
                Ok(true) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(false) | Err(_) => return Err(PtyWriteError { applied_prefix }),
            }
            // Readiness guarantees room for one byte only. A longer blocking
            // write would park on whatever does not fit, so the stall bound
            // holds only while each write is limited to that byte.
            match self.inner.write(&bytes[applied_prefix..=applied_prefix]) {
                Ok(0) => return Err(PtyWriteError { applied_prefix }),
                Ok(written) => applied_prefix += written,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(PtyWriteError { applied_prefix }),
            }
        }
        Ok(())
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
        // A master without a descriptor cannot become writable; `poll(2)`
        // ignores a negative descriptor, so its writes fail after the stall
        // timeout instead of blocking.
        let readiness = PollWritable {
            fd: pair.master.as_raw_fd().unwrap_or(-1),
        };
        Ok(Self {
            master: pair.master,
            child: Mutex::new(child),
            writer: Mutex::new(AppliedPrefixWriter {
                inner: writer,
                readiness,
                stall_timeout: PTY_INPUT_STALL_TIMEOUT,
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
        self.master.try_clone_reader().map_err(io_error)
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

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn io_error_with_context(context: &str, error: impl std::fmt::Display) -> std::io::Error {
    let error = io_error(error);
    std::io::Error::other(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{AppliedPrefixWriter, PollWritable, PtyTerminal, WriteReadiness, poll_outcome};
    use crate::usecase::terminal::{Geometry, InputAck, InputRequest, PtyWriter, TerminalRegistry};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::collections::VecDeque;
    use std::io::{Error, ErrorKind, Read, Write};
    use std::time::Duration;
    use usagi_core::domain::id::{
        ClientId, ConnectionId, DaemonGeneration, RequestId, SessionId, TerminalId, TerminalRef,
        WorkspaceId, WorktreeId,
    };

    enum WriteStep {
        Bytes(usize),
        Interrupted,
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
        timeouts: Vec<Duration>,
    }

    impl ScriptedReadiness {
        fn ready() -> Self {
            Self::new([])
        }

        fn new(answers: impl IntoIterator<Item = std::io::Result<bool>>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                timeouts: Vec::new(),
            }
        }
    }

    impl WriteReadiness for ScriptedReadiness {
        fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<bool> {
            self.timeouts.push(timeout);
            self.answers.pop_front().unwrap_or(Ok(true))
        }
    }

    fn scripted(
        steps: impl IntoIterator<Item = WriteStep>,
        readiness: ScriptedReadiness,
    ) -> AppliedPrefixWriter<ScriptedWriter, ScriptedReadiness> {
        AppliedPrefixWriter {
            inner: ScriptedWriter::new(steps),
            readiness,
            stall_timeout: Duration::from_millis(7),
        }
    }

    #[test]
    fn partial_writes_report_the_exact_applied_prefix() {
        let mut writer = scripted(
            [
                WriteStep::Bytes(1),
                WriteStep::Bytes(1),
                WriteStep::Bytes(1),
                WriteStep::Error,
            ],
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
                WriteStep::Bytes(1),
                WriteStep::Bytes(1),
                WriteStep::Interrupted,
                WriteStep::Bytes(1),
                WriteStep::Bytes(1),
                WriteStep::Bytes(1),
            ],
            ScriptedReadiness::ready(),
        );

        assert_eq!(writer.write_all(b"hello"), Ok(()));
        assert_eq!(writer.inner.written, b"hello");
        assert_eq!(writer.inner.calls, 6);
    }

    #[test]
    fn write_zero_reports_the_prefix_already_applied() {
        let mut writer = scripted(
            [WriteStep::Bytes(1), WriteStep::Bytes(1), WriteStep::Zero],
            ScriptedReadiness::ready(),
        );

        assert_eq!(
            writer.write_all(b"hello"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 2 })
        );
        assert_eq!(writer.inner.written, b"he");
    }

    #[test]
    fn each_write_is_limited_to_the_byte_readiness_guarantees() {
        let mut writer = scripted(
            (0..5).map(|_| WriteStep::Bytes(1)),
            ScriptedReadiness::ready(),
        );

        assert_eq!(writer.write_all(b"hello"), Ok(()));
        assert_eq!(writer.inner.written, b"hello");
        assert_eq!(writer.inner.calls, 5);
        assert_eq!(writer.readiness.timeouts, vec![Duration::from_millis(7); 5]);
        assert!(writer.inner.flush().is_ok());
    }

    #[test]
    fn a_stalled_input_fails_with_the_prefix_applied_before_the_stall() {
        let mut writer = scripted(
            [WriteStep::Bytes(1), WriteStep::Bytes(1)],
            ScriptedReadiness::new([Ok(true), Ok(true), Ok(false)]),
        );

        assert_eq!(
            writer.write_all(b"hello"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 2 })
        );
        assert_eq!(writer.inner.written, b"he");
        assert_eq!(writer.inner.calls, 2);
    }

    #[test]
    fn interrupted_readiness_waits_again_and_other_failures_fail_closed() {
        let mut interrupted = scripted(
            [WriteStep::Bytes(1)],
            ScriptedReadiness::new([Err(Error::from(ErrorKind::Interrupted))]),
        );
        assert_eq!(interrupted.write_all(b"h"), Ok(()));
        assert_eq!(interrupted.readiness.timeouts.len(), 2);

        let mut failed = scripted([], ScriptedReadiness::new([Err(Error::other("poll"))]));
        assert_eq!(
            failed.write_all(b"h"),
            Err(crate::usecase::terminal::PtyWriteError { applied_prefix: 0 })
        );
        assert_eq!(failed.inner.calls, 0);
    }

    #[test]
    fn poll_outcome_maps_failure_timeout_and_readiness() {
        assert_eq!(
            poll_outcome(-1, Error::other("poll failed"))
                .unwrap_err()
                .to_string(),
            "poll failed"
        );
        assert!(!poll_outcome(0, Error::other("unused")).unwrap());
        assert!(poll_outcome(1, Error::other("unused")).unwrap());
    }

    #[test]
    fn real_pty_input_to_a_child_that_never_reads_fails_instead_of_blocking() {
        // The deadlock this bounds: an Agent stops reading input while the
        // daemon writes to it under a runtime lock. Without the stall bound
        // this write parks forever once the PTY input queue is full.
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sleep");
        command.arg("30");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut writer = AppliedPrefixWriter {
            inner: pair.master.take_writer().unwrap(),
            readiness: PollWritable {
                fd: pair.master.as_raw_fd().unwrap(),
            },
            stall_timeout: Duration::from_millis(200),
        };
        let input = b"usagi\n".repeat(64 * 1024);

        let started = std::time::Instant::now();
        let error = writer.write_all(&input).unwrap_err();

        assert!(error.applied_prefix < input.len());
        assert!(started.elapsed() < Duration::from_secs(20));
        child.kill().unwrap();
        child.wait().unwrap();
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
            [WriteStep::Bytes(1), WriteStep::Bytes(1), WriteStep::Error],
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
