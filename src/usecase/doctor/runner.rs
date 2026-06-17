//! The command runner abstraction used by `doctor --fix`: a trait plus the
//! production implementation that shells out to real processes.

use super::which;

/// Runs external commands on behalf of `doctor --fix`.
///
/// Abstracted behind a trait so the remediation logic can be tested without
/// shelling out to a real package manager. Production code uses
/// [`SystemRunner`]; tests inject a fake.
pub trait CommandRunner {
    /// Whether `program` is available on the PATH (checked via `--version`,
    /// output suppressed).
    fn available(&self, program: &str) -> bool;

    /// Run an install command (`program args...`), returning whether it
    /// exited successfully. Its output is shown to the user.
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool>;

    /// Run `program args...`, feeding `input` to its standard input, returning
    /// whether it exited successfully. Used to hand a command a secret it must
    /// not appear on the process's argument list — notably the sudo password
    /// piped to `sudo -S`. The default delegates to [`run`](Self::run)
    /// (ignoring the input), which is all a test fake needs; the real runner
    /// overrides it to actually pipe the bytes.
    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> std::io::Result<bool> {
        let _ = input;
        self.run(program, args)
    }

    /// Run `program args...` quietly (stdout/stderr suppressed), returning
    /// whether it exited successfully. Used for capability probes — e.g.
    /// "is this Ollama model already pulled?" — where the command's own output
    /// should not reach the user.
    fn check(&self, program: &str, args: &[&str]) -> bool;

    /// Spawn `program args...` as a detached background process, returning as
    /// soon as it has launched (without waiting for it to exit). Used to bring
    /// up a long-running daemon — the Ollama server — that other commands
    /// depend on. Its output is discarded.
    fn spawn(&self, program: &str, args: &[&str]) -> std::io::Result<()>;
}

/// The production [`CommandRunner`], backed by [`std::process::Command`].
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn available(&self, program: &str) -> bool {
        which(program)
    }

    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        // Inherit stdio so the user sees the package manager's progress.
        std::process::Command::new(program)
            .args(args)
            .status()
            .map(|status| status.success())
    }

    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> std::io::Result<bool> {
        use std::io::Write as _;
        // Pipe the input (e.g. the sudo password) on stdin so it never reaches
        // the argument list; stdout/stderr stay inherited so progress shows.
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A trailing newline so the reading program (sudo) treats it as a
            // complete line. Dropping the handle afterwards closes the pipe, so
            // a reader waiting on EOF (e.g. `cat`) does not block `wait`. A write
            // failure (the child already exited) is ignored: `wait` then reports
            // the command's own non-zero exit.
            let _ = writeln!(stdin, "{input}");
        }
        child.wait().map(|status| status.success())
    }

    fn check(&self, program: &str, args: &[&str]) -> bool {
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn spawn(&self, program: &str, args: &[&str]) -> std::io::Result<()> {
        // Detach from our stdio so the daemon outlives this call and never
        // writes onto the user's terminal; the handle is dropped on purpose.
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
    }
}
