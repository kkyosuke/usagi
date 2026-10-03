//! Ordered, bounded background persistence for session favorites.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;

use usagi_core::domain::id::SessionId;
use usagi_core::infrastructure::store::session_favorites::SessionFavoritesStore;
use usagi_tui::usecase::application::controller::{AppEvent, BackendEvent, Notice};
use usagi_tui::usecase::application::daemon_backend::Completions;

type Task = (Option<SessionId>, Completions);

/// One lane preserves toggle order and owns its thread until shutdown. A full
/// queue reports failure rather than blocking the frame thread or dropping an
/// accepted toggle. Closing the lane cancels queued work and reaps its worker.
pub(super) struct SessionFavoritesWorker {
    sender: Option<SyncSender<Task>>,
    worker: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl SessionFavoritesWorker {
    pub(super) fn new(workspace: &Path) -> Self {
        let store = SessionFavoritesStore::new(workspace);
        Self::with_runner(move |session, stopping| match session {
            Some(session) => store.toggle_cancellable(session, || stopping.load(Ordering::Acquire)),
            None => store.load(),
        })
    }

    fn with_runner(
        runner: impl FnMut(Option<SessionId>, &AtomicBool) -> anyhow::Result<BTreeSet<SessionId>>
        + Send
        + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(16);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stopping);
        let worker = std::thread::Builder::new()
            .name("session-favorites".to_owned())
            .spawn(move || run(&receiver, &stop_worker, runner));
        Self::from_spawn(sender, stopping, worker)
    }

    fn from_spawn(
        sender: SyncSender<Task>,
        stopping: Arc<AtomicBool>,
        worker: std::io::Result<JoinHandle<()>>,
    ) -> Self {
        match worker {
            Ok(worker) => Self {
                sender: Some(sender),
                worker: Some(worker),
                stopping,
            },
            Err(_) => Self {
                sender: None,
                worker: None,
                stopping,
            },
        }
    }

    pub(super) fn dispatch(&self, session: Option<SessionId>, completions: Completions) {
        let Some(sender) = &self.sender else {
            unavailable(&completions);
            return;
        };
        if let Err(error) = sender.try_send((session, completions)) {
            let (mpsc::TrySendError::Full((_, completions))
            | mpsc::TrySendError::Disconnected((_, completions))) = error;
            unavailable(&completions);
        }
    }
}

fn unavailable(completions: &Completions) {
    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        "Session favorites queue is unavailable; retry.",
    ))));
}

fn run(
    receiver: &Receiver<Task>,
    stopping: &AtomicBool,
    mut runner: impl FnMut(Option<SessionId>, &AtomicBool) -> anyhow::Result<BTreeSet<SessionId>>,
) {
    while let Ok((session, completions)) = receiver.recv() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        super::emit_session_favorites(runner(session, stopping), &completions);
    }
}

impl Drop for SessionFavoritesWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slow_storage_keeps_dispatch_responsive_and_toggles_ordered() {
        let (started, observed) = mpsc::channel();
        let (release, ready) = mpsc::channel();
        let mut favorites = BTreeSet::new();
        let mut first = true;
        let worker = SessionFavoritesWorker::with_runner(move |session, _| {
            if first {
                first = false;
                started.send(()).unwrap();
                ready.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if let Some(session) = session
                && !favorites.remove(&session)
            {
                favorites.insert(session);
            }
            Ok(favorites.clone())
        });
        let first = SessionId::new();
        let second = SessionId::new();
        let (completions, events) = Completions::channel();
        worker.dispatch(Some(first), completions);
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let pending = [Some(first), Some(second), None].map(|session| {
            let (completions, events) = Completions::channel();
            worker.dispatch(session, completions);
            events
        });
        assert!(events.try_recv().is_err(), "storage is still held");
        release.send(()).unwrap();
        for (events, favorites) in std::iter::once(events).chain(pending).zip([
            BTreeSet::from([first]),
            BTreeSet::new(),
            BTreeSet::from([second]),
            BTreeSet::from([second]),
        ]) {
            assert_eq!(
                events.recv_timeout(Duration::from_secs(5)).unwrap(),
                AppEvent::Backend(BackendEvent::SessionFavorites(favorites))
            );
        }
        drop(worker);
    }

    #[test]
    fn storage_and_queue_failures_return_visible_completions() {
        let worker = SessionFavoritesWorker::with_runner(|_, _| anyhow::bail!("storage error"));
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
        drop(worker);

        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = SessionFavoritesWorker {
            sender: Some(sender),
            worker: None,
            stopping: Arc::new(AtomicBool::new(false)),
        };
        let (completions, _) = Completions::channel();
        worker.dispatch(None, completions);
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
        drop(receiver);
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
    }

    #[test]
    fn a_failed_worker_spawn_returns_a_visible_completion() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let worker = SessionFavoritesWorker::from_spawn(
            sender,
            Arc::new(AtomicBool::new(false)),
            Err(std::io::Error::other("thread unavailable")),
        );
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
    }

    #[test]
    fn leaving_a_workspace_cancels_an_active_store_lock_wait_and_reaps_the_worker() {
        use std::cell::Cell;
        use usagi_core::infrastructure::paths::project_data_dir;
        use usagi_core::infrastructure::persistence::store_lock::StoreLock;

        let workspace = tempfile::tempdir().unwrap();
        let held = StoreLock::acquire(&project_data_dir(workspace.path())).unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let (started, waiting) = mpsc::channel();
        let worker = SessionFavoritesWorker::with_runner(move |session, stopping| {
            let probes = Cell::new(0);
            store.toggle_cancellable(session.unwrap(), || {
                probes.set(probes.get() + 1);
                if probes.get() == 2 {
                    started.send(()).unwrap();
                }
                stopping.load(Ordering::Acquire)
            })
        });
        let (completions, events) = Completions::channel();
        worker.dispatch(Some(SessionId::new()), completions);
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished, joined) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(worker);
            finished.send(()).unwrap();
        });
        let stopped = joined.recv_timeout(Duration::from_secs(1));
        drop(held);
        shutdown.join().unwrap();
        assert!(
            stopped.is_ok(),
            "workspace drop must cancel lock contention"
        );
        assert!(matches!(
            events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_store_lock_fails_without_a_writer_and_workspace_drop_reaps_the_worker() {
        use std::ffi::CString;
        use std::fs::{self, File};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use usagi_core::infrastructure::paths::project_data_dir;
        use usagi_core::infrastructure::persistence::store_lock::StoreLock;

        let workspace = tempfile::tempdir().unwrap();
        let dir = project_data_dir(workspace.path());
        fs::create_dir_all(&dir).unwrap();
        let path = StoreLock::path(&dir);
        let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o400) }, 0);

        let worker = SessionFavoritesWorker::new(workspace.path());
        let (completions, events) = Completions::channel();
        worker.dispatch(Some(SessionId::new()), completions);
        let event = events.recv_timeout(Duration::from_secs(1));
        let (finished, joined) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(worker);
            let _ = finished.send(());
        });
        let stopped = joined.recv_timeout(Duration::from_secs(1));

        // Release a regressing read-only open before asserting, keeping both
        // FIFO ends alive until the worker and its shutdown thread have joined.
        let release = if event.is_err() || stopped.is_err() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            Some(
                File::options()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)
                    .unwrap(),
            )
        } else {
            None
        };
        shutdown.join().unwrap();
        drop(release);

        assert!(
            matches!(event, Ok(AppEvent::Backend(BackendEvent::Notice(_)))),
            "FIFO store lock must report failure without a writer"
        );
        assert!(
            stopped.is_ok(),
            "workspace drop must reap its favorites worker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_preferences_fail_without_a_writer_and_workspace_drop_reaps_the_worker() {
        use std::ffi::CString;
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Instant;
        use usagi_core::infrastructure::paths::project_data_dir;

        let workspace = tempfile::tempdir().unwrap();
        let dir = project_data_dir(workspace.path());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-favorites.json");
        let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        // A failing implementation blocks in open/read. Release it before any
        // assertion, then join both the cleanup writer and shutdown thread.
        let (release, requested) = mpsc::channel();
        let cleanup = std::thread::spawn(move || -> std::io::Result<()> {
            if !requested.recv().unwrap_or(false) {
                return Ok(());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)
                {
                    Ok(mut writer) => return writer.write_all(br#"{"sessions":[]}"#),
                    Err(error)
                        if error.raw_os_error() == Some(libc::ENXIO)
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            }
        });
        let worker = SessionFavoritesWorker::new(workspace.path());
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        let event = events.recv_timeout(Duration::from_secs(1));
        let (finished, joined) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(worker);
            let _ = finished.send(());
        });
        let stopped = joined.recv_timeout(Duration::from_secs(1));
        let release_sent = release.send(event.is_err() || stopped.is_err());
        let released = cleanup.join();
        let shutdown = shutdown.join();

        release_sent.unwrap();
        released.unwrap().unwrap();
        shutdown.unwrap();
        assert!(
            matches!(event, Ok(AppEvent::Backend(BackendEvent::Notice(_)))),
            "FIFO preferences must report failure without a writer"
        );
        assert!(
            stopped.is_ok(),
            "workspace drop must reap its favorites worker"
        );
    }

    #[test]
    fn shutdown_cancels_queued_work_and_reaps_the_running_worker() {
        let (started, observed) = mpsc::channel();
        let (release, ready) = mpsc::channel();
        let worker = SessionFavoritesWorker::with_runner(move |_, _| {
            started.send(()).unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(BTreeSet::new())
        });
        let (completions, events) = Completions::channel();
        worker.dispatch(None, completions);
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let (completions, cancelled) = Completions::channel();
        worker.dispatch(Some(SessionId::new()), completions);
        let stopping = Arc::clone(&worker.stopping);
        let (stopped, joined) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(worker);
            stopped.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !stopping.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(joined.try_recv().is_err(), "running work must be reaped");
        release.send(()).unwrap();
        joined.recv_timeout(Duration::from_secs(5)).unwrap();
        shutdown.join().unwrap();
        assert!(observed.recv().is_err(), "queued work was cancelled");
        assert_eq!(
            events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::SessionFavorites(BTreeSet::new()))
        );
        assert!(events.recv().is_err());
        assert!(cancelled.recv().is_err());
    }
}
