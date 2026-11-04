//! Durable automatic retry of a managed update deferred by live runtimes.
//!
//! The detached worker uses the embedded installer's publication lock for every
//! attempt. It invokes the binary currently installed there, so a later update
//! or downgrade cannot be undone by an older waiting worker.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use usagi_core::domain::id::OperationId;
use usagi_core::infrastructure::ipc::BuildIdentity;
use usagi_core::infrastructure::paths::{self, DataHome};
use usagi_core::infrastructure::persistence::json_file;

use super::instance_lock::{
    PrivateLockModePolicy, PrivateLockWait, lock_private_exclusive, open_private_lock,
    verify_private_lock_path,
};
use super::{current_build, prepare_private_data_dir, reap_child};

const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const STATE_FILE: &str = "pending-update.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetryState {
    Waiting,
    Retrying,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingUpdate {
    schema_version: u16,
    request_id: OperationId,
    target: BuildIdentity,
    queued_at: DateTime<Utc>,
    state: RetryState,
    attempts: u64,
}

struct Store(PathBuf);

impl Store {
    fn new(data_dir: &Path) -> Self {
        Self(data_dir.join("daemon"))
    }

    fn lock(&self) -> io::Result<File> {
        lock_private_exclusive(
            &self.0.join("pending-update.lock"),
            "pending daemon update state",
            PrivateLockModePolicy::CrashResidue,
            PrivateLockWait::RECORD,
        )
    }

    fn worker(&self) -> io::Result<Option<File>> {
        let path = self.0.join("pending-update-worker.lock");
        let file = open_private_lock(
            &path,
            "pending daemon update worker",
            PrivateLockModePolicy::CrashResidue,
        )?;
        if worker_lock_acquired(file.try_lock_exclusive())? {
            verify_private_lock_path(&path, &file, "pending daemon update worker")?;
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    fn read(&self) -> io::Result<Option<PendingUpdate>> {
        let pending: Option<PendingUpdate> =
            json_file::read_bounded(&self.0.join(STATE_FILE), 16 * 1024)
                .map_err(io::Error::other)?;
        if pending
            .as_ref()
            .is_some_and(|pending| pending.schema_version != 1)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported pending daemon update",
            ));
        }
        Ok(pending)
    }

    fn write(&self, pending: &PendingUpdate) -> io::Result<()> {
        json_file::write_atomic(&self.0, &self.0.join(STATE_FILE), pending)
            .map_err(io::Error::other)
    }
}

fn worker_lock_acquired(result: io::Result<()>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn schedule_deferred_update() -> io::Result<()> {
    let data_dir = prepare_private_data_dir()?;
    enqueue(&Store::new(&data_dir), current_build(), &mut || {
        spawn_worker(&std::env::current_exe()?, &data_dir)
    })
}

/// A later managed synchronization can complete while an earlier worker is no
/// longer alive. The installer still owns publication custody here, so every
/// reservation preceding this successful synchronization is satisfied.
pub(crate) fn complete_deferred_update(data_dir: &Path) -> io::Result<()> {
    let store = Store::new(data_dir);
    let _state = store.lock()?;
    match std::fs::remove_file(store.0.join(STATE_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Publication and the worker's final empty check share the state lock. The
/// worker releases its lease *inside* that final section, so a newly queued
/// update either wakes the existing worker or launches a successor.
fn enqueue(
    store: &Store,
    target: BuildIdentity,
    spawn: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let _state = store.lock()?;
    let mut pending = PendingUpdate {
        schema_version: 1,
        request_id: OperationId::new(),
        target,
        queued_at: Utc::now(),
        state: RetryState::Waiting,
        attempts: 0,
    };
    store.write(&pending)?;
    if let Some(available) = store.worker()? {
        drop(available);
        if let Err(error) = spawn() {
            pending.state = RetryState::Failed;
            store.write(&pending)?;
            return Err(error);
        }
    }
    Ok(())
}

fn worker_command(
    executable: &Path,
    data_dir: &Path,
    mode: paths::RuntimeMode,
) -> io::Result<Command> {
    // The worker deliberately leaves the invoking worktree. Freeze both halves
    // of the selected data home before changing cwd so a relative USAGI_HOME
    // cannot select another state directory or another installation afterward.
    let data_home = DataHome::from_selected(std::path::absolute(data_dir)?, mode);
    let mut command = Command::new(executable);
    command
        .args(["daemon", "continue-update"])
        .current_dir(data_home.selected())
        .env(paths::DATA_DIR_ENV, data_home.base())
        .env(paths::RUNTIME_MODE_ENV, data_home.mode().as_env_value())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

fn spawn_worker(executable: &Path, data_dir: &Path) -> io::Result<()> {
    let child = worker_command(executable, data_dir, paths::runtime_mode())?.spawn()?;
    // Reap when the caller remains alive, while letting the detached worker
    // outlive the update command or its TUI when that caller exits first.
    reap_child(child);
    Ok(())
}

pub(super) fn continue_update() -> io::Result<()> {
    let data_dir = prepare_private_data_dir()?;
    let store = Store::new(&data_dir);
    let Some(worker) = store.worker()? else {
        return Ok(());
    };
    record_worker_pid(&worker, std::process::id())?;
    drain(&store, &worker, &mut retry_installed_binary, &mut || {
        std::thread::sleep(RETRY_INTERVAL);
    })
}

fn record_worker_pid(mut worker: &File, pid: u32) -> io::Result<()> {
    worker.set_len(0)?;
    writeln!(worker, "{pid}")
}

fn retry_installed_binary() -> io::Result<Option<i32>> {
    let request = usagi_cli::cli::commands::update::installer_request(false);
    let script = synchronization_script(&request)?;
    let mut child = Command::new("bash")
        .args(["-s", "--", "--sync-daemon-only"])
        .env("USAGI_MANAGED_UPDATE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = child
        .stdin
        .take()
        .expect("installer stdin is piped")
        .write_all(script);
    let status = child.wait()?;
    result?;
    Ok(status.code())
}

fn synchronization_script(request: &usagi_cli::cli::InstallerRequest) -> io::Result<&'static [u8]> {
    match request.verified_script() {
        Some(script) => Ok(script),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid embedded synchronization installer",
        )),
    }
}

fn drain(
    store: &Store,
    worker: &File,
    attempt: &mut dyn FnMut() -> io::Result<Option<i32>>,
    wait: &mut dyn FnMut(),
) -> io::Result<()> {
    loop {
        let state = store.lock()?;
        let Some(selected) = store.read()? else {
            fs2::FileExt::unlock(worker)?;
            return Ok(());
        };
        drop(state);
        // No lifecycle or bootstrap lock is held while waiting between attempts.
        let result = attempt();
        let state = store.lock()?;
        if let Some(mut current) = store.read()?
            && current.request_id == selected.request_id
        {
            current.attempts = current.attempts.saturating_add(1);
            match &result {
                Ok(Some(0)) => std::fs::remove_file(store.0.join(STATE_FILE))?,
                Ok(Some(2)) => {
                    current.state = RetryState::Failed;
                    store.write(&current)?;
                    fs2::FileExt::unlock(worker)?;
                    return Err(io::Error::other(
                        "installed binary does not support deferred daemon synchronization",
                    ));
                }
                other => {
                    current.state = if matches!(other, Ok(Some(3))) {
                        RetryState::Waiting
                    } else {
                        RetryState::Retrying
                    };
                    store.write(&current)?;
                }
            }
        }
        drop(state);
        if !matches!(result, Ok(Some(0))) {
            wait();
        }
    }
}

pub(super) fn report_pending_update(out: &mut dyn Write, data_dir: &Path) -> io::Result<()> {
    match Store::new(data_dir).read() {
        Ok(Some(pending)) => writeln!(
            out,
            "  pending daemon update: {}; {}",
            pending.target.label(),
            match pending.state {
                RetryState::Waiting =>
                    "automatic retry every 5s; waiting for live Agent/terminal runtimes to finish",
                RetryState::Retrying =>
                    "automatic retry every 5s; synchronization has not completed",
                RetryState::Failed => "automatic retry stopped; run 'usagi update' again",
            }
        ),
        Ok(None) => Ok(()),
        Err(_) => writeln!(out, "  pending daemon update: state unavailable"),
    }
}

#[cfg(test)]
mod tests;
