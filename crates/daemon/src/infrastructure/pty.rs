//! Concrete daemon-owned pseudo-terminal process adapter.
//!
//! The usecase layer deliberately depends on a small PTY port.  This adapter
//! is the sole place that uses `portable-pty`; callers get readers, writers,
//! resizing and child waiting without exposing a local terminal to clients.

#![coverage(off)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::usecase::terminal::{Geometry, PtyWriteError, PtyWriter};

/// A spawned daemon-owned shell terminal.
pub struct PtyTerminal {
    master: Box<dyn MasterPty + Send>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    writer: Mutex<Box<dyn Write + Send>>,
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
        let child = pair.slave.spawn_command(command).map_err(io_error)?;
        drop(pair.slave);
        let writer = pair.master.take_writer().map_err(io_error)?;
        Ok(Self {
            master: pair.master,
            child: Mutex::new(child),
            writer: Mutex::new(writer),
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
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        self.writer
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
            .map_err(|_| PtyWriteError { applied_prefix: 0 })
    }
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::PtyTerminal;
    use crate::usecase::terminal::Geometry;
    use std::io::Read;

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
}
