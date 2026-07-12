//! `usagi daemon`: control the per-machine background daemon.
//!
//! The daemon is the process that (in later work) owns the agent PTYs and
//! session monitoring so agents keep running after the TUI closes. This command
//! is its control surface:
//!
//! - `status` — report whether a daemon is running.
//! - `start`  — launch one if none is running.
//! - `stop`   — ask a running one to exit (or clean up a stale record).
//! - `serve`  — run the daemon loop itself (hidden; launched by `start`, not by
//!   hand).
//!
//! The decisions live in [`crate::usecase::daemon`]; this layer only parses the
//! subcommand, renders the outcome, and dispatches `serve` to the injected loop.
//! The process table (`alive`), the detached spawn (`spawn`), and the daemon loop
//! (`serve`) are all injected so the whole command is unit-tested without a real
//! daemon.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::domain::daemon::DaemonState;
use crate::infrastructure::daemon_sessions_store;
use crate::usecase::daemon::{self, StartOutcome, StopOutcome};

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Show whether the usagi daemon is running
    Status,
    /// Start the usagi daemon if it is not already running
    Start,
    /// Ask the running usagi daemon to stop
    Stop,
    /// Run the daemon loop (launched by `start`; not invoked by hand)
    #[command(hide = true)]
    Serve,
}

/// Run a `usagi daemon` subcommand against the daemon directory `dir`.
///
/// `alive` checks the live process table, `spawn` launches the detached daemon
/// process, and `serve` runs the daemon loop in the foreground (used by the
/// `serve` subcommand in the spawned child). Output is written to `out`.
pub fn run<W: Write>(
    command: DaemonCommand,
    dir: &Path,
    alive: &dyn Fn(u32) -> bool,
    spawn: &dyn Fn() -> Result<()>,
    serve: &dyn Fn() -> Result<()>,
    out: &mut W,
) -> Result<()> {
    match command {
        DaemonCommand::Status => {
            let state = daemon::status(dir, alive)?;
            writeln!(out, "{}", describe(state)).context("failed to write status")?;
            // When a daemon is running, list the sessions it is monitoring from
            // the snapshot it maintains (see `usecase::daemon::monitor_tick`). A
            // stale/absent snapshot reads as no sessions.
            if matches!(state, DaemonState::Running { .. }) {
                for session in daemon_sessions_store::read(dir)? {
                    let activity = session.activity.map_or("-", |a| a.as_str());
                    writeln!(out, "  {}  {activity}", session.name)
                        .context("failed to write session line")?;
                }
            }
            Ok(())
        }
        DaemonCommand::Start => {
            let line = match daemon::start(dir, alive, spawn)? {
                StartOutcome::Started => "started usagi daemon".to_string(),
                StartOutcome::AlreadyRunning { pid } => {
                    format!("usagi daemon is already running (pid {pid})")
                }
            };
            writeln!(out, "{line}").context("failed to write start result")
        }
        DaemonCommand::Stop => {
            let line = match daemon::stop(dir, alive)? {
                StopOutcome::Stopping { pid } => {
                    format!("stopping usagi daemon (pid {pid})")
                }
                StopOutcome::RemovedStale { pid } => {
                    format!("removed stale usagi daemon record (pid {pid})")
                }
                StopOutcome::NotRunning => "usagi daemon is not running".to_string(),
            };
            writeln!(out, "{line}").context("failed to write stop result")
        }
        DaemonCommand::Serve => serve(),
    }
}

/// The one-line human description of a [`DaemonState`] for `status`.
fn describe(state: DaemonState) -> String {
    match state {
        DaemonState::Running { pid } => format!("usagi daemon is running (pid {pid})"),
        DaemonState::Stale { pid } => {
            format!("usagi daemon is not running (stale record for pid {pid})")
        }
        DaemonState::NotRunning => "usagi daemon is not running".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::daemon_store::{self, DaemonRecord};

    fn dead(_: u32) -> bool {
        false
    }
    fn live(_: u32) -> bool {
        true
    }
    /// A spawn/serve that succeeds with no side effects. One shared function
    /// stands in for both injected closures across the tests: it is exercised by
    /// the start/serve success cases and merely passed (never reached) by the
    /// status/stop cases, so every case is covered without a per-test closure.
    fn noop() -> Result<()> {
        Ok(())
    }

    fn output(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn status_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Status,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "usagi daemon is not running\n");
    }

    #[test]
    fn status_running() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 55 }).unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Status,
            tmp.path(),
            &live,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "usagi daemon is running (pid 55)\n");
    }

    #[test]
    fn status_running_lists_monitored_sessions() {
        use crate::domain::daemon::{SessionActivity, SessionSnapshot};
        use std::path::PathBuf;
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 12 }).unwrap();
        daemon_sessions_store::write(
            tmp.path(),
            &[
                SessionSnapshot {
                    workspace: PathBuf::from("/repo"),
                    name: "work-a".to_string(),
                    worktree: None,
                    activity: Some(SessionActivity::Waiting),
                },
                SessionSnapshot {
                    workspace: PathBuf::from("/repo"),
                    name: "fix-b".to_string(),
                    worktree: None,
                    activity: None,
                },
            ],
        )
        .unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Status,
            tmp.path(),
            &live,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            output(out),
            "usagi daemon is running (pid 12)\n  work-a  waiting\n  fix-b  -\n"
        );
    }

    #[test]
    fn status_stale() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 55 }).unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Status,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            output(out),
            "usagi daemon is not running (stale record for pid 55)\n"
        );
    }

    #[test]
    fn start_spawns_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        // `noop` stands in for the detached spawn; the "started" line is only
        // printed on the Started outcome, which `daemon::start` returns after
        // spawn ran.
        run(
            DaemonCommand::Start,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "started usagi daemon\n");
    }

    #[test]
    fn start_reports_already_running() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 7 }).unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Start,
            tmp.path(),
            &live,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "usagi daemon is already running (pid 7)\n");
    }

    #[test]
    fn stop_signals_running() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 9 }).unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Stop,
            tmp.path(),
            &live,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "stopping usagi daemon (pid 9)\n");
    }

    #[test]
    fn stop_removes_stale() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 9 }).unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Stop,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "removed stale usagi daemon record (pid 9)\n");
    }

    #[test]
    fn stop_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        run(
            DaemonCommand::Stop,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert_eq!(output(out), "usagi daemon is not running\n");
    }

    #[test]
    fn serve_dispatches_to_the_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        // Serve delegates to the injected loop and prints nothing itself; `noop`
        // standing in for the loop returns Ok, and no output is written.
        run(
            DaemonCommand::Serve,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut out,
        )
        .unwrap();
        assert!(output(out).is_empty());
    }

    #[test]
    fn serve_propagates_its_error() {
        let tmp = tempfile::tempdir().unwrap();
        let serve = || Err(anyhow::anyhow!("loop failed"));
        let mut out = Vec::new();
        let err = run(
            DaemonCommand::Serve,
            tmp.path(),
            &dead,
            &noop,
            &serve,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("loop failed"));
    }

    #[test]
    fn write_failure_surfaces() {
        // A sink that always fails to write turns into the contextual error, so a
        // broken stdout is reported rather than silently dropped.
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // The always-Ok flush is part of the sink's contract even though
        // `writeln!` never calls it; exercise it directly so the impl is covered.
        assert!(FailWriter.flush().is_ok());
        let tmp = tempfile::tempdir().unwrap();
        let err = run(
            DaemonCommand::Status,
            tmp.path(),
            &dead,
            &noop,
            &noop,
            &mut FailWriter,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to write status"));
    }

    #[test]
    fn session_line_write_failure_surfaces() {
        use crate::domain::daemon::{SessionActivity, SessionSnapshot};
        use std::path::PathBuf;
        // A writer that lets the whole status line through — however many write
        // calls `writeln!` splits it into — then fails once that line's newline
        // has landed, so the failure lands on the following session line and is
        // reported distinctly. Keying on the newline (not a write count) is robust
        // to `write_fmt` fragmenting the line.
        struct FailAfterFirstLine {
            line_done: bool,
        }
        impl Write for FailAfterFirstLine {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.line_done {
                    return Err(std::io::Error::other("broken pipe"));
                }
                if buf.contains(&b'\n') {
                    self.line_done = true;
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // The always-Ok flush is part of the sink's contract; `writeln!` never
        // calls it, so exercise it directly to cover the impl.
        assert!(FailAfterFirstLine { line_done: true }.flush().is_ok());
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 12 }).unwrap();
        daemon_sessions_store::write(
            tmp.path(),
            &[SessionSnapshot {
                workspace: PathBuf::from("/repo"),
                name: "s".to_string(),
                worktree: None,
                activity: Some(SessionActivity::Running),
            }],
        )
        .unwrap();
        let err = run(
            DaemonCommand::Status,
            tmp.path(),
            &live,
            &noop,
            &noop,
            &mut FailAfterFirstLine { line_done: false },
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to write session line"));
    }
}
