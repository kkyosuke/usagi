//! The command runner abstraction used by `doctor --fix`: a trait plus the
//! production implementation that shells out to real processes.

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

    /// Like [`run`](Self::run) but with stdout/stderr suppressed. Used when the
    /// caller drives its own progress UI — the TUI's background install paints a
    /// loading rabbit, and the command's raw output (e.g. `ollama pull`'s
    /// `pulling manifest …`) would otherwise corrupt the screen. The default
    /// delegates to [`run`](Self::run) (a test fake cannot observe the suppressed
    /// streams anyway, so the recorded command is identical); the real runner
    /// overrides it to null both streams.
    fn run_quiet(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        self.run(program, args)
    }

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

    /// Like [`run_with_input`](Self::run_with_input) but with stdout/stderr
    /// suppressed, for the same reason as [`run_quiet`](Self::run_quiet) — the
    /// TUI install pipes the sudo password to `sudo -S` while painting its own
    /// progress. The default delegates to [`run_with_input`](Self::run_with_input);
    /// the real runner overrides it to null both streams (still piping `input`).
    fn run_with_input_quiet(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
    ) -> std::io::Result<bool> {
        self.run_with_input(program, args, input)
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

/// Whether `name` is installed and runnable, probed by searching the PATH
/// for an executable file. The basis of [`SystemRunner::available`].
fn which(name: &str) -> bool {
    let path_var = match std::env::var_os("PATH") {
        Some(v) => v,
        None => return false,
    };

    #[cfg(target_os = "windows")]
    let extensions = {
        let pathext = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| std::ffi::OsString::from(".EXE;.BAT;.CMD;.COM"));
        std::env::split_paths(&pathext).collect::<Vec<_>>()
    };

    for path in std::env::split_paths(&path_var) {
        let candidate = path.join(name);

        #[cfg(target_os = "windows")]
        {
            if candidate.is_file() {
                return true;
            }
            for ext in &extensions {
                if let Some(ext_str) = ext.to_str() {
                    let mut name_with_ext = name.to_string();
                    name_with_ext.push_str(ext_str);
                    if path.join(name_with_ext).is_file() {
                        return true;
                    }
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(metadata) = candidate.metadata() {
                    if metadata.is_file() && (metadata.permissions().mode() & 0o111) != 0 {
                        return true;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                if candidate.is_file() {
                    return true;
                }
            }
        }
    }
    false
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

    fn run_quiet(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        // Same as `run`, but with stdout/stderr discarded so the command's
        // progress cannot paint over a TUI that is drawing its own indicator.
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
    }

    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> std::io::Result<bool> {
        // Output stays inherited so progress shows; the secret is piped on stdin.
        self.run_with_input_inner(program, args, input, false)
    }

    fn run_with_input_quiet(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
    ) -> std::io::Result<bool> {
        // As `run_with_input`, but with stdout/stderr discarded for the TUI path.
        self.run_with_input_inner(program, args, input, true)
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

impl SystemRunner {
    /// Shared body of [`run_with_input`](CommandRunner::run_with_input) and its
    /// quiet variant: pipe `input` on stdin, optionally discarding stdout/stderr.
    fn run_with_input_inner(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
        quiet: bool,
    ) -> std::io::Result<bool> {
        use std::io::Write as _;
        let mut command = std::process::Command::new(program);
        command.args(args).stdin(std::process::Stdio::piped());
        if quiet {
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let mut child = command.spawn()?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A runner that implements only the required methods, so the trait's
    /// default `run_with_input` / `run_quiet` / `run_with_input_quiet` (each
    /// delegating to a louder counterpart) are exercised here rather than left
    /// to the production overrides that shell out.
    #[derive(Default)]
    struct DefaultFake {
        calls: RefCell<Vec<String>>,
    }

    impl CommandRunner for DefaultFake {
        fn available(&self, _program: &str) -> bool {
            true
        }
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(true)
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn quiet_and_input_variants_default_to_run() {
        let fake = DefaultFake::default();
        assert!(fake.run_quiet("a", &["1"]).unwrap());
        assert!(fake.run_with_input("b", &["2"], "secret").unwrap());
        assert!(fake.run_with_input_quiet("c", &["3"], "secret").unwrap());
        // Every default ultimately routes through `run`, ignoring the input.
        assert_eq!(*fake.calls.borrow(), vec!["a 1", "b 2", "c 3"]);
        // The fake's other required methods round out its trait surface.
        assert!(fake.available("x"));
        assert!(fake.check("x", &[]));
        assert!(fake.spawn("x", &[]).is_ok());
    }

    #[test]
    fn test_system_runner_available() {
        let runner = SystemRunner;
        // git is definitely installed on the host
        assert!(runner.available("git"));
        // nonexistent command should not be available
        assert!(!runner.available("nonexistent-command-xyz"));
    }

    #[test]
    fn test_which_fails_when_path_is_missing() {
        // Backup original PATH
        let original_path = std::env::var_os("PATH");
        std::env::remove_var("PATH");

        assert!(!which("git"));

        // Restore original PATH
        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        }
    }

    #[test]
    fn test_which_skips_non_executable_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("dummy-command");
        // Create a non-executable file
        std::fs::write(&file_path, "not executable").unwrap();

        // Backup original PATH and set it to temp_dir
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", temp_dir.path());

        // The dummy-command is a file but not executable (on Unix), so which should return false.
        assert!(!which("dummy-command"));

        // Restore PATH
        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        }
    }
}
