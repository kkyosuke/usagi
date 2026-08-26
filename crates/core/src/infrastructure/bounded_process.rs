//! Bounded subprocess observation for non-interactive public CLI probes.
//!
//! The runner owns the complete child lifecycle: each probe gets a fresh
//! process group, bounded output capture, a deadline, and TERM -> KILL -> reap
//! cleanup. Results are deliberately closed and never contain argv, paths,
//! environment values, credentials, raw OS errors, or failed command output.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
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
    /// The deadline elapsed and the complete process group was reaped.
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
struct Capture {
    bytes: Vec<u8>,
    exceeded: bool,
}

/// Runs one public, non-interactive CLI probe under `policy`.
///
/// The child inherits no stdin. stdout and stderr are drained concurrently so
/// either pipe can fill without deadlocking the child, while retained memory is
/// limited to `output_limit` bytes per stream.
#[must_use]
#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=normalizes_success_and_safe_failure_states,timeout_terminates_the_process_group_and_reaps_the_child
pub fn observe(program: &str, arguments: &[&str], policy: ChildPolicy) -> ChildObservation {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let Ok(mut child) = command.spawn() else {
        return ChildObservation::SpawnFailed;
    };
    let pid = child.id();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate_and_reap(&mut child, policy.terminate_grace);
        return ChildObservation::ObservationFailed;
    };
    let stdout = thread::spawn(move || {
        let mut stdout = stdout;
        capture(&mut stdout, policy.output_limit)
    });
    let stderr = thread::spawn(move || {
        let mut stderr = stderr;
        capture(&mut stderr, policy.output_limit)
    });

    let deadline = Instant::now() + policy.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildObservation::TimedOut);
            }
            Err(_) => {
                terminate_and_reap(&mut child, policy.terminate_grace);
                break Err(ChildObservation::ObservationFailed);
            }
        }
    };
    close_descendant_resources(pid, &stdout, &stderr, None, policy.terminate_grace);
    let stdout = stdout.join();
    let stderr = stderr.join();
    let Ok(status) = status else {
        return status.unwrap_err();
    };
    let (Ok(stdout), Ok(stderr)) = (stdout, stderr) else {
        return ChildObservation::ObservationFailed;
    };
    if stdout.exceeded || stderr.exceeded {
        return ChildObservation::OutputTooLarge;
    }
    if !status.success() {
        return ChildObservation::ExitFailure;
    }
    normalize_output(stdout.bytes, stderr.bytes)
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
    let input = input.to_vec();
    let writer = thread::spawn(move || stdin.write_all(&input).is_ok());
    let stdout = thread::spawn(move || {
        let mut stdout = stdout;
        capture(&mut stdout, policy.output_limit)
    });
    let stderr = thread::spawn(move || {
        let mut stderr = stderr;
        capture(&mut stderr, policy.output_limit)
    });
    let deadline = Instant::now() + policy.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
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
    let stdout = stdout.join();
    let stderr = stderr.join();
    let writer = writer.join();
    let Ok(status) = status else {
        return status.unwrap_err();
    };
    let (Ok(stdout), Ok(stderr), Ok(true)) = (stdout, stderr, writer) else {
        return ChildInputExecution::ObservationFailed;
    };
    if stdout.exceeded || stderr.exceeded {
        return ChildInputExecution::OutputTooLarge;
    }
    if status.success() {
        ChildInputExecution::Success
    } else {
        ChildInputExecution::ExitFailure
    }
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=exited_parent_cannot_leave_a_descendant_holding_capture_pipes
fn close_descendant_resources(
    pid: u32,
    stdout: &thread::JoinHandle<Capture>,
    stderr: &thread::JoinHandle<Capture>,
    writer: Option<&thread::JoinHandle<bool>>,
    grace: Duration,
) {
    let finished = || {
        stdout.is_finished()
            && stderr.is_finished()
            && writer.is_none_or(thread::JoinHandle::is_finished)
    };
    if finished() {
        return;
    }
    // A probe must not daemonize. If its main process exits while a descendant
    // still owns either pipe, close that process group instead of joining a
    // reader forever.
    signal_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if finished() {
            return;
        }
        thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    signal_group(pid, libc::SIGKILL);
}

fn capture(reader: &mut dyn Read, limit: usize) -> Capture {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let Ok(read) = reader.read(&mut buffer) else {
            return Capture {
                bytes: retained,
                exceeded: true,
            };
        };
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        exceeded |= keep < read;
    }
    Capture {
        bytes: retained,
        exceeded,
    }
}

fn normalize_output(stdout: Vec<u8>, stderr: Vec<u8>) -> ChildObservation {
    let selected = if stdout.is_empty() { stderr } else { stdout };
    let Ok(text) = String::from_utf8(selected) else {
        return ChildObservation::InvalidOutput;
    };
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return ChildObservation::EmptyOutput;
    };
    ChildObservation::Success(line.to_owned())
}

#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=timeout_terminates_the_process_group_and_reaps_the_child
fn terminate_and_reap(child: &mut std::process::Child, grace: Duration) {
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
fn signal_group(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: the child was placed in a process group whose ID is its PID;
        // a negative PID targets only that owned group. Signal errors are safe
        // to ignore because the child may have exited between try_wait and kill.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
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
    fn capture_bounds_memory_and_normalizes_read_failures() {
        let mut exact = std::io::Cursor::new(b"1234");
        let captured = capture(&mut exact, 4);
        assert_eq!(captured.bytes, b"1234");
        assert!(!captured.exceeded);

        let mut oversized = std::io::Cursor::new(b"12345");
        let captured = capture(&mut oversized, 4);
        assert_eq!(captured.bytes, b"1234");
        assert!(captured.exceeded);

        let mut failing = FailingReader {
            returned_bytes: false,
        };
        let captured = capture(&mut failing, 4);
        assert_eq!(captured.bytes, b"ok");
        assert!(captured.exceeded);
    }

    #[test]
    fn output_normalization_is_strict_and_prefers_stdout() {
        assert_eq!(
            normalize_output(b" stdout \nignored".to_vec(), b"stderr".to_vec()),
            ChildObservation::Success("stdout".to_owned())
        );
        assert_eq!(
            normalize_output(Vec::new(), b" stderr ".to_vec()),
            ChildObservation::Success("stderr".to_owned())
        );
        assert_eq!(
            normalize_output(vec![0xff], Vec::new()),
            ChildObservation::InvalidOutput
        );
        assert_eq!(
            normalize_output(b"  \n".to_vec(), Vec::new()),
            ChildObservation::EmptyOutput
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
                &["-c", "(trap '' TERM; sleep 30) & exit 0"],
                b"payload",
                16,
                policy(),
            ),
            ChildInputExecution::Success
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
