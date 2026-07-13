//! Concrete daemon-owned pseudo-terminal process adapter.
//!
//! The usecase layer deliberately depends on a small PTY port.  This adapter
//! is the sole place that uses `portable-pty`; callers get readers, writers,
//! resizing and child waiting without exposing a local terminal to clients.

#![coverage(off)]

use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::usecase::terminal::{Geometry, PtyWriteError, PtyWriter};

/// A spawned daemon-owned shell terminal.
pub struct PtyTerminal {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
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
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: geometry.rows,
                cols: geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_error)?;
        Self::spawn_pair(pair, program, directory)
    }

    fn spawn_pair(pair: PtyPair, program: &str, directory: &Path) -> std::io::Result<Self> {
        let mut command = CommandBuilder::new(program);
        command.cwd(directory);
        let child = pair.slave.spawn_command(command).map_err(io_error)?;
        drop(pair.slave);
        let writer = pair.master.take_writer().map_err(io_error)?;
        Ok(Self {
            master: pair.master,
            child,
            writer,
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
    pub fn wait(&mut self) -> std::io::Result<i32> {
        self.child
            .wait()
            .map_err(io_error)
            .and_then(|status| i32::try_from(status.exit_code()).map_err(std::io::Error::other))
    }
}

impl PtyWriter for PtyTerminal {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        self.writer
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
    use crate::usecase::terminal::{Geometry, PtyWriter};

    #[test]
    fn daemon_owns_shell_pty_until_it_reaps_the_child() {
        let mut terminal = PtyTerminal::spawn(
            "/bin/sh",
            std::path::Path::new("/"),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
        terminal.write_all(b"exit\n").unwrap();
        assert_eq!(terminal.wait().unwrap(), 0);
    }
}
