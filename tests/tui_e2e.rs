//! End-to-end tests that drive the real `usagi` binary inside a pseudo-terminal.
//!
//! Each screen's event loop is already unit-tested with a scripted `KeyReader`
//! (see `src/presentation/tui/**`). These tests close the remaining gap: they
//! launch the actual binary in a PTY, send real key bytes, and assert on the
//! screen parsed back out of the terminal with `vt100` — exercising the full
//! path from key decoding, through the screen-graph navigation, to the painted
//! frame, exactly as a user at a terminal would.
//!
//! The session is isolated via `$USAGI_HOME` (pointed at a tempdir) so the test
//! never reads or writes the developer's real `~/.usagi`. Only screens that
//! need no external tooling (git, a shell, a network) are exercised here:
//! welcome → config → back → quit.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Terminal size the TUI is given. Wide and tall enough that the centred
/// welcome body and the config form both fit without truncation.
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// How long to wait for an expected string to appear before failing. Generous
/// so a slow CI box (cold binary, loaded scheduler) does not flake.
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Polling interval while waiting for the screen to settle or the child to exit.
const POLL: Duration = Duration::from_millis(20);

/// A `usagi` process running inside a pseudo-terminal, with its output fed into
/// a `vt100` parser so tests can assert on what a user would see on screen.
struct PtySession {
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Held so the master end of the PTY stays open for the writer and the
    // reader thread; dropping it would close them.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _reader: thread::JoinHandle<()>,
}

impl PtySession {
    /// Spawns `usagi <args...>` in a fresh PTY with `$USAGI_HOME` isolated to
    /// `usagi_home`, streaming its output into a `vt100` screen.
    fn spawn(args: &[&str], usagi_home: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty");

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_usagi"));
        for arg in args {
            cmd.arg(arg);
        }
        cmd.env("USAGI_HOME", usagi_home);
        // A known terminal type so `vt100` and the TUI agree on capabilities.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("failed to spawn usagi");
        // Release the parent's handle to the slave; the child keeps its own.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("failed to clone reader");
        let writer = pair.master.take_writer().expect("failed to take writer");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let sink = Arc::clone(&parser);
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().process(&buf[..n]),
                }
            }
        });

        PtySession {
            writer,
            parser,
            child,
            _master: pair.master,
            _reader: handle,
        }
    }

    /// Writes raw bytes (key presses) to the terminal.
    fn send(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .expect("failed to write to pty");
        self.writer.flush().expect("failed to flush pty");
    }

    /// The current visible screen as plain text (styling stripped by `vt100`).
    fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Blocks until the visible screen contains `needle`, or fails with a dump
    /// of the last screen seen if the timeout elapses.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            let screen = self.screen();
            if screen.contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}.\n--- screen ---\n{screen}\n--------------"
                );
            }
            thread::sleep(POLL);
        }
    }

    /// Blocks until the child process exits and asserts it exited successfully.
    fn wait_for_clean_exit(&mut self) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            match self.child.try_wait().expect("failed to poll child") {
                Some(status) => {
                    assert!(status.success(), "usagi exited unsuccessfully: {status:?}");
                    return;
                }
                None if Instant::now() >= deadline => {
                    panic!(
                        "timed out waiting for usagi to exit.\n--- screen ---\n{}",
                        self.screen()
                    );
                }
                None => thread::sleep(POLL),
            }
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Never leave a stray TUI process behind if a test panics mid-flow.
        let _ = self.child.kill();
    }
}

/// Drives the welcome menu into the global Config screen, back out, and then
/// quits — proving real key input flows through the screen graph and each
/// destination renders.
#[test]
fn hop_navigates_welcome_to_config_and_back_then_quits() {
    let home = tempfile::tempdir().expect("failed to create temp USAGI_HOME");
    let mut session = PtySession::spawn(&["hop"], home.path());

    // The welcome screen paints its title and menu before reading any key.
    session.wait_for("USAGI");
    session.wait_for("Enter or shortcut letter: select");

    // 'c' opens the global Config screen (shortcut for the "Config" entry).
    session.send(b"c");
    session.wait_for("Config");
    session.wait_for("Adjust your global preferences");

    // 'q' on the Config screen returns to the welcome menu (Back).
    session.send(b"q");
    session.wait_for("USAGI");

    // 'q' on the welcome menu quits the application cleanly.
    session.send(b"q");
    session.wait_for_clean_exit();
}

/// The fastest path: quitting straight from the welcome menu ends the session
/// with a success status.
#[test]
fn hop_quits_from_welcome() {
    let home = tempfile::tempdir().expect("failed to create temp USAGI_HOME");
    let mut session = PtySession::spawn(&["hop"], home.path());

    session.wait_for("USAGI");
    session.send(b"q");
    session.wait_for_clean_exit();
}
