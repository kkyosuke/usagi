//! Deadline-bounded subprocess execution for v1's short-lived command calls.
//!
//! The runner owns the child, its process group, and its pipe workers as one
//! lifecycle.  Its timeout is an end-to-end wall-clock budget: normal execution,
//! graceful termination, forced termination, reap polling, and pipe collection
//! all fit inside that budget.  Cleanup that cannot finish in time is moved to a
//! detached reaper so the caller is never turned into the blocking cleanup path.

use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Runtime and cleanup budgets for one subprocess.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// End-to-end wall-clock budget, including termination, reap, and drain.
    pub timeout: Duration,
    /// Portion of `timeout` reserved for a graceful process-group termination.
    pub terminate_grace: Duration,
    /// Portion of `timeout` reserved for force-kill reap and final pipe drain.
    pub reap_grace: Duration,
    /// Maximum delay between lifecycle polls.
    pub poll_interval: Duration,
    /// Maximum stdout bytes retained. Remaining bytes are drained and discarded.
    pub stdout_cap: usize,
    /// Maximum stderr bytes retained. Remaining bytes are drained and discarded.
    pub stderr_cap: usize,
}

/// Bytes retained from one pipe and whether further bytes were discarded.
#[derive(Debug, Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Output of a child that exited and whose pipes closed within the deadline.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: io::Result<Captured>,
    pub stderr: io::Result<Captured>,
}

/// Result of a bounded subprocess run.
#[derive(Debug)]
pub enum Outcome {
    Exited(Output),
    /// The execution or pipe drain exhausted the deadline. Cleanup may continue
    /// on a detached reaper, but the caller is no longer coupled to it.
    TimedOut(TimeoutDiagnostic),
}

/// Best-effort cleanup information retained for logs and caller diagnostics.
#[derive(Debug, Default)]
pub struct TimeoutDiagnostic {
    pub pid: u32,
    pub terminate_error: Option<String>,
    pub force_kill_error: Option<String>,
    pub detached_reaper: bool,
    pub stdout_drain_pending: bool,
    pub stderr_drain_pending: bool,
}

/// Spawn and supervise `command` under one end-to-end deadline.
///
/// On Unix the child starts a new process group, which is sent `SIGTERM` and
/// then `SIGKILL`. On Windows a new process group is created, but stable std does
/// not provide a job object/tree termination primitive; cleanup therefore kills
/// the direct child and detached descendants may outlive the call.
pub fn run(mut command: Command, stdin: Option<Vec<u8>>, limits: Limits) -> io::Result<Outcome> {
    configure_process_group(&mut command);
    let started = Instant::now();
    let deadline = started.checked_add(limits.timeout).unwrap_or(started);
    let mut child = command.spawn()?;
    let pid = child.id();

    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }

    let stdout = spawn_drain(child.stdout.take(), limits.stdout_cap);
    let stderr = spawn_drain(child.stderr.take(), limits.stderr_cap);
    Ok(supervise(
        OsChild { child, pid },
        stdout,
        stderr,
        deadline,
        limits,
    ))
}

type DrainResult = io::Result<Captured>;

fn spawn_drain(pipe: Option<impl Read + Send + 'static>, cap: usize) -> Receiver<DrainResult> {
    let (tx, rx) = mpsc::channel();
    match pipe {
        Some(mut pipe) => {
            std::thread::spawn(move || {
                let _ = tx.send(read_capped(&mut pipe, cap));
            });
        }
        None => {
            let _ = tx.send(Ok(Captured::default()));
        }
    }
    rx
}

fn read_capped(reader: &mut impl Read, cap: usize) -> io::Result<Captured> {
    let mut bytes = Vec::new();
    reader.take(cap as u64 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > cap;
    if truncated {
        bytes.truncate(cap);
        let _ = io::copy(reader, &mut io::sink());
    }
    Ok(Captured { bytes, truncated })
}

trait ChildControl: Send + 'static {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn terminate(&mut self) -> io::Result<()>;
    fn force_kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

struct OsChild {
    child: Child,
    pid: u32,
}

impl ChildControl for OsChild {
    fn id(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate(&mut self) -> io::Result<()> {
        terminate_process_group(&mut self.child, self.pid, false)
    }

    fn force_kill(&mut self) -> io::Result<()> {
        terminate_process_group(&mut self.child, self.pid, true)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

fn supervise<C: ChildControl>(
    mut child: C,
    stdout_rx: Receiver<DrainResult>,
    stderr_rx: Receiver<DrainResult>,
    deadline: Instant,
    limits: Limits,
) -> Outcome {
    let cleanup = limits.terminate_grace.saturating_add(limits.reap_grace);
    let execution_deadline = deadline.checked_sub(cleanup).unwrap_or(deadline);
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;

    while Instant::now() < execution_deadline {
        poll_child(&mut child, &mut status);
        poll_drain(&stdout_rx, &mut stdout);
        poll_drain(&stderr_rx, &mut stderr);
        if let Some(output) = take_complete_output(&mut status, &mut stdout, &mut stderr) {
            return Outcome::Exited(output);
        }
        sleep_until(execution_deadline, limits.poll_interval);
    }

    let mut diagnostic = TimeoutDiagnostic {
        pid: child.id(),
        ..TimeoutDiagnostic::default()
    };
    if let Err(error) = child.terminate() {
        diagnostic.terminate_error = Some(error.to_string());
    }

    let terminate_deadline = deadline.checked_sub(limits.reap_grace).unwrap_or(deadline);
    poll_until(
        &mut child,
        &mut status,
        &stdout_rx,
        &mut stdout,
        &stderr_rx,
        &mut stderr,
        terminate_deadline,
        limits.poll_interval,
    );

    if let Err(error) = child.force_kill() {
        diagnostic.force_kill_error = Some(error.to_string());
    }
    poll_until(
        &mut child,
        &mut status,
        &stdout_rx,
        &mut stdout,
        &stderr_rx,
        &mut stderr,
        deadline,
        limits.poll_interval,
    );

    if status.is_none() {
        diagnostic.detached_reaper = true;
        spawn_reaper(child, diagnostic.pid);
    }
    diagnostic.stdout_drain_pending = stdout.is_none();
    diagnostic.stderr_drain_pending = stderr.is_none();
    Outcome::TimedOut(diagnostic)
}

fn take_complete_output(
    status: &mut Option<ExitStatus>,
    stdout: &mut Option<DrainResult>,
    stderr: &mut Option<DrainResult>,
) -> Option<Output> {
    match (status.take(), stdout.take(), stderr.take()) {
        (Some(status), Some(stdout), Some(stderr)) => Some(Output {
            status,
            stdout,
            stderr,
        }),
        (pending_status, pending_stdout, pending_stderr) => {
            *status = pending_status;
            *stdout = pending_stdout;
            *stderr = pending_stderr;
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_until<C: ChildControl>(
    child: &mut C,
    status: &mut Option<ExitStatus>,
    stdout_rx: &Receiver<DrainResult>,
    stdout: &mut Option<DrainResult>,
    stderr_rx: &Receiver<DrainResult>,
    stderr: &mut Option<DrainResult>,
    deadline: Instant,
    poll_interval: Duration,
) {
    while Instant::now() < deadline {
        poll_child(child, status);
        poll_drain(stdout_rx, stdout);
        poll_drain(stderr_rx, stderr);
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            return;
        }
        sleep_until(deadline, poll_interval);
    }
}

fn poll_child(child: &mut impl ChildControl, status: &mut Option<ExitStatus>) {
    if status.is_none() {
        if let Ok(Some(exited)) = child.try_wait() {
            *status = Some(exited);
        }
    }
}

fn poll_drain(receiver: &Receiver<DrainResult>, slot: &mut Option<DrainResult>) {
    if slot.is_none() {
        match receiver.try_recv() {
            Ok(result) => *slot = Some(result),
            Err(TryRecvError::Disconnected) => {
                *slot = Some(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "subprocess pipe worker disconnected",
                )));
            }
            Err(TryRecvError::Empty) => {}
        }
    }
}

fn sleep_until(deadline: Instant, poll_interval: Duration) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(poll_interval.min(remaining));
}

fn spawn_reaper<C: ChildControl>(mut child: C, pid: u32) {
    std::thread::spawn(move || {
        eprintln!("usagi: subprocess pid {pid} exceeded cleanup deadline; detached reaper active");
        loop {
            match child.wait() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(_) => break,
                Err(error) => {
                    eprintln!("usagi: detached subprocess reaper failed for pid {pid}: {error}");
                    break;
                }
            }
        }
    });
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_group(_child: &mut Child, pid: u32, force: bool) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child pid exceeds i32"))?;
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn terminate_process_group(child: &mut Child, _pid: u32, _force: bool) -> io::Result<()> {
    child.kill()
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_group(child: &mut Child, _pid: u32, _force: bool) -> io::Result<()> {
    child.kill()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn limits(timeout: Duration) -> Limits {
        Limits {
            timeout,
            terminate_grace: Duration::from_millis(30),
            reap_grace: Duration::from_millis(30),
            poll_interval: Duration::from_millis(2),
            stdout_cap: 32,
            stderr_cap: 32,
        }
    }

    #[cfg(unix)]
    fn exited() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    struct NeverExitChild {
        released: Arc<AtomicBool>,
        reaped: Arc<AtomicBool>,
    }

    #[cfg(unix)]
    impl ChildControl for NeverExitChild {
        fn id(&self) -> u32 {
            42
        }

        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self.released.load(Ordering::SeqCst).then(exited))
        }

        fn terminate(&mut self) -> io::Result<()> {
            Err(io::Error::other("terminate failed"))
        }

        fn force_kill(&mut self) -> io::Result<()> {
            Err(io::Error::other("force failed"))
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            while !self.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.reaped.store(true, Ordering::SeqCst);
            Ok(exited())
        }
    }

    fn completed_drain() -> Receiver<DrainResult> {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(Captured::default())).unwrap();
        rx
    }

    #[cfg(unix)]
    #[test]
    fn kill_failures_and_never_exit_are_bounded_and_detached_reaped() {
        let released = Arc::new(AtomicBool::new(false));
        let reaped = Arc::new(AtomicBool::new(false));
        let timeout = Duration::from_millis(80);
        let started = Instant::now();
        let outcome = supervise(
            NeverExitChild {
                released: Arc::clone(&released),
                reaped: Arc::clone(&reaped),
            },
            completed_drain(),
            completed_drain(),
            started + timeout,
            limits(timeout),
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        let Outcome::TimedOut(diagnostic) = outcome else {
            panic!("expected timeout");
        };
        assert_eq!(
            diagnostic.terminate_error.as_deref(),
            Some("terminate failed")
        );
        assert_eq!(diagnostic.force_kill_error.as_deref(), Some("force failed"));
        assert!(diagnostic.detached_reaper);

        released.store(true, Ordering::SeqCst);
        let eventual = Instant::now() + Duration::from_secs(1);
        while !reaped.load(Ordering::SeqCst) && Instant::now() < eventual {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(reaped.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn normal_success_collects_large_output_with_a_cap() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "yes x | head -c 4096"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let Outcome::Exited(output) = run(command, None, limits(Duration::from_secs(2))).unwrap()
        else {
            panic!("command timed out");
        };
        assert!(output.status.success());
        let stdout = output.stdout.unwrap();
        assert_eq!(stdout.bytes.len(), 32);
        assert!(stdout.truncated);
        assert!(output.stderr.unwrap().bytes.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn normal_success_writes_stdin_and_collects_complete_output() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let Outcome::Exited(output) = run(
            command,
            Some(b"hello bounded process".to_vec()),
            limits(Duration::from_secs(2)),
        )
        .unwrap() else {
            panic!("command timed out");
        };
        assert!(output.status.success());
        let stdout = output.stdout.unwrap();
        assert_eq!(stdout.bytes, b"hello bounded process");
        assert!(!stdout.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_stdout_holder_is_killed_without_exceeding_wall_clock_bound() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 30 & echo $! > \"$1\"; exit 0",
                "sh",
                pid_file.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        assert!(matches!(
            run(command, None, limits(timeout)).unwrap(),
            Outcome::TimedOut(_)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));

        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let eventual = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < eventual {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[cfg(unix)]
    #[test]
    fn term_ignoring_child_is_force_killed_and_reaped_within_deadline() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        let Outcome::TimedOut(diagnostic) = run(command, None, limits(timeout)).unwrap() else {
            panic!("expected timeout");
        };
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!diagnostic.detached_reaper);
        assert_eq!(unsafe { libc::kill(diagnostic.pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[cfg(windows)]
    #[test]
    fn windows_direct_child_timeout_is_bounded() {
        let mut command = Command::new("cmd");
        command
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        assert!(matches!(
            run(command, None, limits(timeout)).unwrap(),
            Outcome::TimedOut(_)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
