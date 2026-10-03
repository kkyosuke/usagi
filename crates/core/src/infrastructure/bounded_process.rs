//! Bounded subprocess observation for non-interactive public CLI probes.
//!
//! The runner owns the complete child lifecycle: each probe gets a fresh
//! process group, bounded output capture, a deadline, and TERM -> KILL -> reap
//! cleanup. Public observations are closed and never contain argv, paths,
//! environment values, credentials, raw OS errors, or failed command output.
//! The trusted internal execution API also preserves completed nonzero output.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Policy applied to one child observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildPolicy {
    /// Maximum time the command may run before termination begins.
    pub timeout: Duration,
    /// Time allowed after TERM before KILL is sent.
    pub terminate_grace: Duration,
    /// Maximum captured bytes across each of stdout and stderr.
    pub output_limit: usize,
}

/// Safe, typed result of one bounded command observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildObservation {
    /// Exit zero with one validated line of public output.
    Success(String),
    /// The executable could not be started.
    SpawnFailed,
    /// The child exited nonzero.
    ExitFailure,
    /// The deadline elapsed or inherited pipes could not be closed in time.
    TimedOut,
    /// stdout or stderr exceeded the configured capture bound.
    OutputTooLarge,
    /// The selected output was not valid UTF-8.
    InvalidOutput,
    /// The child produced no non-whitespace output.
    EmptyOutput,
    /// Capturing or waiting for the child failed.
    ObservationFailed,
}

/// Raw output of a successful bounded command, or a safe closed failure. Unlike
/// [`ChildObservation`], success preserves every byte (including NUL and
/// newlines) and permits empty streams, so callers can parse machine output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutputObservation {
    Success { stdout: Vec<u8>, stderr: Vec<u8> },
    SpawnFailed,
    ExitFailure,
    TimedOut,
    OutputTooLarge,
    ObservationFailed,
}

/// Captured bytes from a completed command, including a nonzero exit.
/// Callers of [`execute_command_output`] must sanitize these before publishing
/// diagnostics: unlike the public-observation API, these may include stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildCommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Closed failures of a bounded execution that did not yield complete output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildOutputError {
    SpawnFailed,
    TimedOut,
    IncompleteOutput,
    OutputTooLarge,
    ObservationFailed,
}

impl std::fmt::Display for ChildOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SpawnFailed => "command could not be started",
            Self::TimedOut => "command observation timed out",
            Self::IncompleteOutput => "command output remained incomplete after process exit",
            Self::OutputTooLarge => "command output exceeded the capture limit",
            Self::ObservationFailed => "command observation failed",
        })
    }
}

impl std::error::Error for ChildOutputError {}

/// Safe result of a bounded command fed through stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildInputExecution {
    Success,
    SpawnFailed,
    ExitFailure,
    TimedOut,
    InputTooLarge,
    OutputTooLarge,
    ObservationFailed,
}

#[derive(Debug)]
pub(crate) struct Capture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) exceeded: bool,
    pub(crate) cancelled: bool,
}

/// Runs one public, non-interactive CLI probe under `policy`.
///
/// The child inherits no stdin. stdout and stderr are drained concurrently so
/// either pipe can fill without deadlocking the child, while retained memory is
/// limited to `output_limit` bytes per stream.
#[must_use]
pub fn observe(program: &str, arguments: &[&str], policy: ChildPolicy) -> ChildObservation {
    let mut command = Command::new(program);
    command.args(arguments);
    normalize_observation(observe_command_output(command, policy))
}

fn normalize_observation(observation: ChildOutputObservation) -> ChildObservation {
    match observation {
        ChildOutputObservation::Success { stdout, stderr } => {
            normalize_output(if stdout.is_empty() { stderr } else { stdout })
        }
        ChildOutputObservation::SpawnFailed => ChildObservation::SpawnFailed,
        ChildOutputObservation::ExitFailure => ChildObservation::ExitFailure,
        ChildOutputObservation::TimedOut => ChildObservation::TimedOut,
        ChildOutputObservation::OutputTooLarge => ChildObservation::OutputTooLarge,
        ChildOutputObservation::ObservationFailed => ChildObservation::ObservationFailed,
    }
}

/// Runs a caller-built command with bounded output, deadline, and complete
/// process-group cleanup while preserving successful stdout and stderr bytes.
/// Failure states never expose command output.
///
/// The caller may set a trusted cwd and scrub environment variables before
/// passing the command; this function owns stdin/stdout/stderr and process-group
/// configuration from that point onward.
#[must_use]
pub fn observe_command_output(command: Command, policy: ChildPolicy) -> ChildOutputObservation {
    public_output(execute_command_output(command, policy))
}

fn public_output(result: Result<ChildCommandOutput, ChildOutputError>) -> ChildOutputObservation {
    match result {
        Ok(output) if output.success => ChildOutputObservation::Success {
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Ok(_) => ChildOutputObservation::ExitFailure,
        Err(ChildOutputError::SpawnFailed) => ChildOutputObservation::SpawnFailed,
        Err(ChildOutputError::TimedOut | ChildOutputError::IncompleteOutput) => {
            ChildOutputObservation::TimedOut
        }
        Err(ChildOutputError::OutputTooLarge) => ChildOutputObservation::OutputTooLarge,
        Err(ChildOutputError::ObservationFailed) => ChildOutputObservation::ObservationFailed,
    }
}

/// Executes a trusted command with bounded time, capture, and pipe cleanup.
/// Nonzero exits retain their captured output for internal adapters such as Git;
/// public readiness probes should use [`observe_command_output`] instead.
///
/// # Errors
///
/// Returns a closed failure when spawning, waiting, capture, or cleanup fails.
#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=preserves_machine_output_bytes,normalizes_success_and_safe_failure_states,escaped_descendant_cannot_hold_capture_or_input_workers
pub fn execute_command_output(
    mut command: Command,
    policy: ChildPolicy,
) -> Result<ChildCommandOutput, ChildOutputError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|_| ChildOutputError::SpawnFailed)?;
    let pid = child.id();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate_and_reap(&mut child, policy.terminate_grace);
        return Err(ChildOutputError::ObservationFailed);
    };
    if nonblocking(stdout.as_raw_fd())
        .and_then(|()| nonblocking(stderr.as_raw_fd()))
        .is_err()
    {
        terminate_and_reap(&mut child, policy.terminate_grace);
        return Err(ChildOutputError::ObservationFailed);
    }
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = Arc::clone(&output_exceeded);
    let stdout_cancelled = Arc::clone(&cancelled);
    let stdout = thread::spawn(move || {
        let mut stdout = stdout;
        capture(
            &mut stdout,
            policy.output_limit,
            &stdout_exceeded,
            &stdout_cancelled,
        )
    });
    let stderr_exceeded = Arc::clone(&output_exceeded);
    let stderr_cancelled = Arc::clone(&cancelled);
    let stderr = thread::spawn(move || {
        let mut stderr = stderr;
        capture(
            &mut stderr,
            policy.output_limit,
            &stderr_exceeded,
            &stderr_cancelled,
        )
    });

    let deadline = Instant::now() + policy.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if output_exceeded.load(Ordering::Acquire) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildOutputError::OutputTooLarge);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildOutputError::TimedOut);
            }
            Err(_) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildOutputError::ObservationFailed);
            }
        }
    };
    close_descendant_resources(pid, &stdout, &stderr, None, policy.terminate_grace);
    cancelled.store(true, Ordering::Release);
    let stdout = stdout.join();
    let stderr = stderr.join();
    let status = status?;
    let (Ok(stdout), Ok(stderr)) = (stdout, stderr) else {
        return Err(ChildOutputError::ObservationFailed);
    };
    if stdout.exceeded || stderr.exceeded {
        return Err(ChildOutputError::OutputTooLarge);
    }
    if stdout.cancelled || stderr.cancelled {
        return Err(ChildOutputError::IncompleteOutput);
    }
    Ok(ChildCommandOutput {
        success: status.success(),
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

/// Runs a non-interactive command with bounded input, output, lifetime, and
/// complete process-group cleanup.
#[must_use]
#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=bounded_input_execution_writes_and_times_out_safely
pub fn write_stdin_bounded(
    program: &str,
    arguments: &[&str],
    input: &[u8],
    input_limit: usize,
    policy: ChildPolicy,
) -> ChildInputExecution {
    if input.len() > input_limit {
        return ChildInputExecution::InputTooLarge;
    }
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let Ok(mut child) = command.spawn() else {
        return ChildInputExecution::SpawnFailed;
    };
    let pid = child.id();
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        terminate_and_reap(&mut child, policy.terminate_grace);
        return ChildInputExecution::ObservationFailed;
    };
    if nonblocking(stdin.as_raw_fd())
        .and_then(|()| nonblocking(stdout.as_raw_fd()))
        .and_then(|()| nonblocking(stderr.as_raw_fd()))
        .is_err()
    {
        terminate_and_reap(&mut child, policy.terminate_grace);
        return ChildInputExecution::ObservationFailed;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let writer_cancelled = Arc::clone(&cancelled);
    let input = input.to_vec();
    let writer = thread::spawn(move || write_input(&mut stdin, &input, &writer_cancelled));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = Arc::clone(&output_exceeded);
    let stdout_cancelled = Arc::clone(&cancelled);
    let stdout = thread::spawn(move || {
        let mut stdout = stdout;
        capture(
            &mut stdout,
            policy.output_limit,
            &stdout_exceeded,
            &stdout_cancelled,
        )
    });
    let stderr_exceeded = Arc::clone(&output_exceeded);
    let stderr_cancelled = Arc::clone(&cancelled);
    let stderr = thread::spawn(move || {
        let mut stderr = stderr;
        capture(
            &mut stderr,
            policy.output_limit,
            &stderr_exceeded,
            &stderr_cancelled,
        )
    });
    let deadline = Instant::now() + policy.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if output_exceeded.load(Ordering::Acquire) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildInputExecution::OutputTooLarge);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Ok(None) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildInputExecution::TimedOut);
            }
            Err(_) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildInputExecution::ObservationFailed);
            }
        }
    };
    close_descendant_resources(pid, &stdout, &stderr, Some(&writer), policy.terminate_grace);
    cancelled.store(true, Ordering::Release);
    let stdout = stdout.join();
    let stderr = stderr.join();
    let writer = writer.join();
    let Ok(status) = status else {
        return status.unwrap_err();
    };
    let (Ok(stdout), Ok(stderr), Ok(writer)) = (stdout, stderr, writer) else {
        return ChildInputExecution::ObservationFailed;
    };
    normalize_input_completion(status.success(), &stdout, &stderr, writer)
}

fn normalize_input_completion(
    success: bool,
    stdout: &Capture,
    stderr: &Capture,
    writer: Option<bool>,
) -> ChildInputExecution {
    if stdout.exceeded || stderr.exceeded {
        return ChildInputExecution::OutputTooLarge;
    }
    if stdout.cancelled || stderr.cancelled || writer.is_none() {
        return ChildInputExecution::TimedOut;
    }
    if writer == Some(false) {
        return ChildInputExecution::ObservationFailed;
    }
    if success {
        ChildInputExecution::Success
    } else {
        ChildInputExecution::ExitFailure
    }
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=exited_parent_cannot_leave_a_descendant_holding_capture_pipes
pub(crate) fn close_descendant_resources<T>(
    pid: u32,
    stdout: &thread::JoinHandle<T>,
    stderr: &thread::JoinHandle<T>,
    writer: Option<&thread::JoinHandle<Option<bool>>>,
    grace: Duration,
) -> bool {
    let finished = || {
        stdout.is_finished()
            && stderr.is_finished()
            && writer.is_none_or(thread::JoinHandle::is_finished)
    };
    if finished() {
        return false;
    }
    // A probe must not daemonize. If its main process exits while a descendant
    // still owns either pipe, close that process group instead of joining a
    // reader forever.
    let mut signalled = signal_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if finished() {
            return signalled;
        }
        thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    signalled |= signal_group(pid, libc::SIGKILL);
    // Even a descendant that escaped the original group must not hold a join.
    // Allow an EOF from cooperative descendants before cancelling the workers.
    let deadline = Instant::now() + grace;
    while !finished() && Instant::now() < deadline {
        thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    signalled
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=escaped_descendant_cannot_hold_capture_or_input_workers
pub(crate) fn nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fcntl only reads and changes flags on the owned open descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn write_input(writer: &mut dyn Write, mut input: &[u8], cancelled: &AtomicBool) -> Option<bool> {
    while !input.is_empty() {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        match writer.write(input) {
            Ok(0) => return Some(false),
            Ok(count) => input = &input[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Some(false),
        }
    }
    Some(true)
}

pub(crate) fn capture(
    reader: &mut dyn Read,
    limit: usize,
    exceeded_signal: &AtomicBool,
    cancelled: &AtomicBool,
) -> Capture {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    let incomplete = loop {
        // Cancellation must not turn an already closed pipe into a timeout
        // merely because its reader was scheduled after cleanup completed.
        // Nonblocking reads can still establish EOF and retain ready bytes.
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if cancelled.load(Ordering::Acquire)
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
            {
                break true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => {
                exceeded_signal.store(true, Ordering::Release);
                return Capture {
                    bytes: retained,
                    exceeded: true,
                    cancelled: false,
                };
            }
        };
        if read == 0 {
            break false;
        }
        let remaining = limit.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        exceeded |= keep < read;
        if exceeded {
            exceeded_signal.store(true, Ordering::Release);
            // An escaped descendant may keep producing immediately readable
            // output forever. The capture limit bounds draining after cancel.
            if cancelled.load(Ordering::Acquire) {
                break true;
            }
        }
    };
    Capture {
        bytes: retained,
        exceeded,
        cancelled: incomplete,
    }
}

fn normalize_output(output: Vec<u8>) -> ChildObservation {
    let Ok(text) = String::from_utf8(output) else {
        return ChildObservation::InvalidOutput;
    };
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return ChildObservation::EmptyOutput;
    };
    ChildObservation::Success(line.to_owned())
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=timeout_terminates_the_process_group_and_reaps_the_child
pub(crate) fn terminate_and_reap(child: &mut std::process::Child, grace: Duration) {
    signal_group(child.id(), libc::SIGTERM);
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) | Err(_) => break,
        }
    }
    signal_group(child.id(), libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=timeout_terminates_the_process_group_and_reaps_the_child
pub(crate) fn signal_group(pid: u32, signal: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: the child was placed in a process group whose ID is its PID;
    // a negative PID targets only that owned group. A missing group is normal
    // when all processes exited before the reader workers were scheduled.
    unsafe { libc::kill(-pid, signal) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingReader {
        returned_bytes: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.returned_bytes {
                return Err(std::io::Error::other("injected read failure"));
            }
            self.returned_bytes = true;
            buffer[..2].copy_from_slice(b"ok");
            Ok(2)
        }
    }

    fn policy() -> ChildPolicy {
        ChildPolicy {
            timeout: Duration::from_secs(1),
            terminate_grace: Duration::from_millis(20),
            output_limit: 16,
        }
    }

    #[test]
    fn public_output_drops_failed_command_bytes_and_maps_closed_errors() {
        assert_eq!(
            public_output(Ok(ChildCommandOutput {
                success: false,
                stdout: b"secret".to_vec(),
                stderr: b"credential".to_vec(),
            })),
            ChildOutputObservation::ExitFailure
        );
        for (error, expected) in [
            (
                ChildOutputError::SpawnFailed,
                ChildOutputObservation::SpawnFailed,
            ),
            (ChildOutputError::TimedOut, ChildOutputObservation::TimedOut),
            (
                ChildOutputError::IncompleteOutput,
                ChildOutputObservation::TimedOut,
            ),
            (
                ChildOutputError::OutputTooLarge,
                ChildOutputObservation::OutputTooLarge,
            ),
            (
                ChildOutputError::ObservationFailed,
                ChildOutputObservation::ObservationFailed,
            ),
        ] {
            assert!(!error.to_string().is_empty());
            assert_eq!(public_output(Err(error)), expected);
        }
        let mut command = Command::new("sh");
        command.args(["-c", "printf diagnostic >&2; exit 7"]);
        assert_eq!(
            execute_command_output(command, policy()).unwrap(),
            ChildCommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"diagnostic".to_vec(),
            }
        );
    }

    #[test]
    fn input_completion_never_accepts_overflow_or_cancelled_io() {
        let complete = Capture {
            bytes: Vec::new(),
            exceeded: false,
            cancelled: false,
        };
        for (success, writer, expected) in [
            (true, Some(true), ChildInputExecution::Success),
            (false, Some(true), ChildInputExecution::ExitFailure),
            (true, Some(false), ChildInputExecution::ObservationFailed),
            (true, None, ChildInputExecution::TimedOut),
        ] {
            assert_eq!(
                normalize_input_completion(success, &complete, &complete, writer),
                expected
            );
        }
        let overflow = Capture {
            bytes: Vec::new(),
            exceeded: true,
            cancelled: false,
        };
        let cancelled = Capture {
            bytes: Vec::new(),
            exceeded: false,
            cancelled: true,
        };
        for (aborted, expected) in [
            (&overflow, ChildInputExecution::OutputTooLarge),
            (&cancelled, ChildInputExecution::TimedOut),
        ] {
            assert_eq!(
                normalize_input_completion(true, aborted, &complete, Some(true)),
                expected
            );
            assert_eq!(
                normalize_input_completion(true, &complete, aborted, Some(true)),
                expected
            );
        }
    }

    #[test]
    fn capture_and_input_retry_nonblocking_io_but_honor_cancellation() {
        use std::collections::VecDeque;
        struct Reader(VecDeque<std::io::Result<Vec<u8>>>);
        impl Read for Reader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let bytes = self.0.pop_front().unwrap()?;
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
        struct Writer(VecDeque<std::io::Result<usize>>);
        impl Write for Writer {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0.pop_front().unwrap()
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut reader = Reader(
            [
                Err(std::io::ErrorKind::Interrupted.into()),
                Err(std::io::ErrorKind::WouldBlock.into()),
                Ok(b"ok".to_vec()),
                Ok(Vec::new()),
            ]
            .into(),
        );
        let captured = capture(
            &mut reader,
            4,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(captured.bytes, b"ok");
        assert!(!captured.cancelled);
        let mut writer = Writer(
            [
                Err(std::io::ErrorKind::Interrupted.into()),
                Err(std::io::ErrorKind::WouldBlock.into()),
                Ok(1),
                Ok(1),
            ]
            .into(),
        );
        assert_eq!(
            write_input(&mut writer, b"ok", &AtomicBool::new(false)),
            Some(true)
        );
        assert!(writer.flush().is_ok());
        for step in [Ok(0), Err(std::io::Error::other("write failed"))] {
            assert_eq!(
                write_input(&mut Writer([step].into()), b"x", &AtomicBool::new(false)),
                Some(false)
            );
        }
        assert_eq!(
            write_input(&mut Writer(VecDeque::new()), b"x", &AtomicBool::new(true)),
            None
        );
    }

    #[test]
    fn cancelled_capture_preserves_ready_bytes_and_closed_pipes() {
        // The reader starts only after the owner has cancelled cleanup, as
        // happens when a successful child's reader is delayed by scheduling.
        for bytes in [b"".as_slice(), b"ok".as_slice(), b"full".as_slice()] {
            let exceeded = AtomicBool::new(false);
            let captured = capture(
                &mut std::io::Cursor::new(bytes),
                4,
                &exceeded,
                &AtomicBool::new(true),
            );
            assert_eq!(captured.bytes, bytes);
            assert!(!captured.cancelled);
            assert!(!captured.exceeded);
            assert!(!exceeded.load(Ordering::Acquire));
        }
    }

    #[test]
    fn cancelled_capture_bounds_open_and_continuously_readable_pipes() {
        struct OpenReader {
            first: Option<Vec<u8>>,
            error: std::io::ErrorKind,
            reads: usize,
        }
        impl Read for OpenReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if let Some(bytes) = self.first.take() {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    return Ok(bytes.len());
                }
                Err(self.error.into())
            }
        }
        struct EndlessReader(usize);
        impl Read for EndlessReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                buffer[0] = b'x';
                Ok(1)
            }
        }

        for error in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::Interrupted,
        ] {
            for first in [Vec::new(), b"full".to_vec()] {
                let mut reader = OpenReader {
                    first: (!first.is_empty()).then(|| first.clone()),
                    error,
                    reads: 0,
                };
                let captured = capture(
                    &mut reader,
                    4,
                    &AtomicBool::new(false),
                    &AtomicBool::new(true),
                );
                assert_eq!(captured.bytes, first);
                assert!(captured.cancelled);
                assert!(!captured.exceeded);
                assert_eq!(reader.reads, usize::from(!first.is_empty()) + 1);
            }
        }
        let mut reader = EndlessReader(0);
        let exceeded = AtomicBool::new(false);
        let captured = capture(&mut reader, 4, &exceeded, &AtomicBool::new(true));
        assert_eq!(captured.bytes, b"xxxx");
        assert!(captured.cancelled);
        assert!(captured.exceeded);
        assert!(exceeded.load(Ordering::Acquire));
        assert_eq!(reader.0, 5);
        let captured = capture(
            &mut OpenReader {
                first: None,
                error: std::io::ErrorKind::Other,
                reads: 0,
            },
            4,
            &AtomicBool::new(false),
            &AtomicBool::new(true),
        );
        assert!(captured.exceeded);
        assert!(!captured.cancelled);
    }

    #[test]
    #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=escaped_descendant_cannot_hold_capture_or_input_workers
    fn escaped_descendant_probe() {
        use std::os::fd::FromRawFd;
        struct PendingEscape(libc::pid_t);
        #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=escaped_descendant_cannot_hold_capture_or_input_workers
        impl Drop for PendingEscape {
            fn drop(&mut self) {
                // SAFETY: this guard owns the exact forked child until the
                // PID file hands cleanup to the invoking test's guard.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    while libc::waitpid(self.0, std::ptr::null_mut(), 0) < 0 {
                        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                        {
                            break;
                        }
                    }
                }
            }
        }
        let Some(path) = std::env::var_os("USAGI_BOUNDED_PIPE_HELPER") else {
            return;
        };
        let mut ready = [0; 2];
        // SAFETY: all child-side work after fork uses only async-signal-safe
        // libc calls; the child is explicitly killed by the test's cleanup.
        unsafe {
            assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::close(ready[0]);
                if libc::setsid() < 0 {
                    libc::_exit(71);
                }
                libc::write(ready[1], b"r".as_ptr().cast(), 1);
                libc::close(ready[1]);
                loop {
                    libc::pause();
                }
            }
            let _child_cleanup = PendingEscape(pid);
            libc::close(ready[1]);
            let mut read = std::fs::File::from_raw_fd(ready[0]);
            read.read_exact(&mut [0]).unwrap();
            std::fs::write(path, pid.to_string()).unwrap();
            std::process::exit(0);
        }
    }

    #[test]
    fn escaped_descendant_cannot_hold_capture_or_input_workers() {
        struct KillEscaped(std::path::PathBuf);
        #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=escaped_descendant_cannot_hold_capture_or_input_workers
        impl Drop for KillEscaped {
            fn drop(&mut self) {
                if let Some(pid) = std::fs::read_to_string(&self.0)
                    .ok()
                    .and_then(|pid| pid.parse::<libc::pid_t>().ok())
                {
                    // SAFETY: the helper recorded exactly the escaped child's PID.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let test = "infrastructure::bounded_process::tests::escaped_descendant_probe";
        let bounded = ChildPolicy {
            timeout: Duration::from_secs(2),
            output_limit: 4096,
            ..policy()
        };
        for with_input in [false, true] {
            let path = temp.path().join(if with_input {
                "input.pid"
            } else {
                "capture.pid"
            });
            let _cleanup = KillEscaped(path.clone());
            let started = Instant::now();
            if with_input {
                let binding = format!("USAGI_BOUNDED_PIPE_HELPER={}", path.display());
                let input = vec![b'x'; 1024 * 1024];
                let result = write_stdin_bounded(
                    "env",
                    &[
                        &binding,
                        executable.to_str().unwrap(),
                        "--exact",
                        test,
                        "--nocapture",
                    ],
                    &input,
                    input.len(),
                    bounded,
                );
                assert_eq!(result, ChildInputExecution::TimedOut);
            } else {
                let mut command = Command::new(&executable);
                command
                    .args(["--exact", test, "--nocapture"])
                    .env("USAGI_BOUNDED_PIPE_HELPER", &path);
                let result = execute_command_output(command, bounded);
                assert_eq!(result, Err(ChildOutputError::IncompleteOutput));
                assert_eq!(public_output(result), ChildOutputObservation::TimedOut);
            }
            assert!(
                path.exists(),
                "the helper must have escaped before the probe returned"
            );
            assert!(started.elapsed() < Duration::from_secs(4));
        }
    }

    #[test]
    fn normalizes_success_and_safe_failure_states() {
        assert_eq!(
            observe("sh", &["-c", "printf ' tool 1.2\\nmore\\n'"], policy()),
            ChildObservation::Success("tool 1.2".to_owned())
        );
        assert_eq!(
            observe("sh", &["-c", "printf 'stderr 2.0\\n' >&2"], policy()),
            ChildObservation::Success("stderr 2.0".to_owned())
        );
        assert_eq!(
            observe("sh", &["-c", "printf secret >&2; exit 7"], policy()),
            ChildObservation::ExitFailure
        );
        assert_eq!(
            observe("definitely-not-a-usagi-command", &[], policy()),
            ChildObservation::SpawnFailed
        );
        assert_eq!(
            observe("sh", &["-c", "printf '   \\n'"], policy()),
            ChildObservation::EmptyOutput
        );
    }

    #[test]
    fn rejects_invalid_or_oversized_output() {
        assert_eq!(
            observe("sh", &["-c", "printf '\\377'"], policy()),
            ChildObservation::InvalidOutput
        );
        assert_eq!(
            observe("sh", &["-c", "printf 12345678901234567"], policy()),
            ChildObservation::OutputTooLarge
        );
        assert_eq!(
            observe("sh", &["-c", "printf 12345678901234567 >&2"], policy()),
            ChildObservation::OutputTooLarge
        );
    }

    #[test]
    fn output_overflow_terminates_before_the_provider_deadline() {
        let started = Instant::now();
        let result = observe(
            "sh",
            &["-c", "trap '' TERM; yes oversized"],
            ChildPolicy {
                timeout: Duration::from_secs(5),
                ..policy()
            },
        );
        assert_eq!(result, ChildObservation::OutputTooLarge);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn capture_bounds_memory_and_normalizes_read_failures() {
        let exceeded = AtomicBool::new(false);
        let mut exact = std::io::Cursor::new(b"1234");
        let captured = capture(&mut exact, 4, &exceeded, &AtomicBool::new(false));
        assert_eq!(captured.bytes, b"1234");
        assert!(!captured.exceeded);
        assert!(!exceeded.load(Ordering::Acquire));

        let exceeded = AtomicBool::new(false);
        let mut oversized = std::io::Cursor::new(b"12345");
        let captured = capture(&mut oversized, 4, &exceeded, &AtomicBool::new(false));
        assert_eq!(captured.bytes, b"1234");
        assert!(captured.exceeded);
        assert!(exceeded.load(Ordering::Acquire));

        let exceeded = AtomicBool::new(false);
        let mut failing = FailingReader {
            returned_bytes: false,
        };
        let captured = capture(&mut failing, 4, &exceeded, &AtomicBool::new(false));
        assert_eq!(captured.bytes, b"ok");
        assert!(captured.exceeded);
        assert!(exceeded.load(Ordering::Acquire));
    }

    #[test]
    fn output_normalization_is_strict_and_prefers_stdout() {
        assert_eq!(
            normalize_output(b" stdout \nignored".to_vec()),
            ChildObservation::Success("stdout".to_owned())
        );
        assert_eq!(
            normalize_output(vec![0xff]),
            ChildObservation::InvalidOutput
        );
        assert_eq!(
            normalize_output(b"  \n".to_vec()),
            ChildObservation::EmptyOutput
        );
        assert_eq!(
            normalize_observation(ChildOutputObservation::ObservationFailed),
            ChildObservation::ObservationFailed
        );
    }

    #[test]
    fn preserves_machine_output_bytes() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'one\\000two\\nthree'"]);
        assert_eq!(
            observe_command_output(command, policy()),
            ChildOutputObservation::Success {
                stdout: b"one\0two\nthree".to_vec(),
                stderr: Vec::new(),
            }
        );

        let mut empty = Command::new("sh");
        empty.args(["-c", "exit 0"]);
        assert_eq!(
            observe_command_output(empty, policy()),
            ChildOutputObservation::Success {
                stdout: Vec::new(),
                stderr: Vec::new(),
            }
        );
    }

    #[test]
    fn timeout_terminates_the_process_group_and_reaps_the_child() {
        let started = Instant::now();
        let result = observe(
            "sh",
            &["-c", "trap '' TERM; (trap '' TERM; sleep 30) & wait"],
            ChildPolicy {
                timeout: Duration::from_millis(30),
                ..policy()
            },
        );
        assert_eq!(result, ChildObservation::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn exited_parent_cannot_leave_a_descendant_holding_capture_pipes() {
        let started = Instant::now();
        let result = observe(
            "sh",
            &["-c", "(trap '' TERM; sleep 30) & printf done"],
            policy(),
        );
        assert_eq!(result, ChildObservation::Success("done".to_owned()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_input_execution_writes_and_times_out_safely() {
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "test \"$(cat)\" = payload"],
                b"payload",
                16,
                policy()
            ),
            ChildInputExecution::Success
        );
        assert_eq!(
            write_stdin_bounded("sh", &["-c", "cat >/dev/null"], b"too large", 4, policy()),
            ChildInputExecution::InputTooLarge
        );
        let started = Instant::now();
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "trap '' TERM; sleep 30"],
                b"payload",
                16,
                ChildPolicy {
                    timeout: Duration::from_millis(30),
                    ..policy()
                },
            ),
            ChildInputExecution::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        let started = Instant::now();
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "cat >/dev/null; trap '' TERM; yes oversized"],
                b"payload",
                16,
                ChildPolicy {
                    timeout: Duration::from_secs(5),
                    ..policy()
                },
            ),
            ChildInputExecution::OutputTooLarge
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_input_normalizes_nonzero_broken_pipe_and_descendant_cleanup() {
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "cat >/dev/null; exit 7"],
                b"payload",
                16,
                policy()
            ),
            ChildInputExecution::ExitFailure
        );

        let oversized_pipe_write = vec![b'x'; 1024 * 1024];
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "exec 0<&-; sleep 0.05"],
                &oversized_pipe_write,
                oversized_pipe_write.len(),
                policy(),
            ),
            ChildInputExecution::ObservationFailed
        );

        let started = Instant::now();
        assert_eq!(
            write_stdin_bounded(
                "sh",
                &["-c", "cat >/dev/null; (trap '' TERM; sleep 30) & exit 0",],
                b"payload",
                16,
                policy(),
            ),
            ChildInputExecution::Success
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
