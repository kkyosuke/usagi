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
        Self::with_runner(move |session| match session {
            Some(session) => store.toggle(session),
            None => store.load(),
        })
    }

    fn with_runner(
        runner: impl FnMut(Option<SessionId>) -> anyhow::Result<BTreeSet<SessionId>> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(16);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stopping);
        let worker = std::thread::spawn(move || run(&receiver, &stop_worker, runner));
        Self {
            sender: Some(sender),
            worker: Some(worker),
            stopping,
        }
    }

    pub(super) fn dispatch(&self, session: Option<SessionId>, completions: Completions) {
        if let Err(error) = self
            .sender
            .as_ref()
            .expect("favorite worker owns its sender until drop")
            .try_send((session, completions))
        {
            let (mpsc::TrySendError::Full((_, completions))
            | mpsc::TrySendError::Disconnected((_, completions))) = error;
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "Session favorites queue is unavailable; retry.",
            ))));
        }
    }
}

fn run(
    receiver: &Receiver<Task>,
    stopping: &AtomicBool,
    mut runner: impl FnMut(Option<SessionId>) -> anyhow::Result<BTreeSet<SessionId>>,
) {
    while let Ok((session, completions)) = receiver.recv() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        super::emit_session_favorites(runner(session), &completions);
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
        let worker = SessionFavoritesWorker::with_runner(move |session| {
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
        let worker = SessionFavoritesWorker::with_runner(|_| anyhow::bail!("storage error"));
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
    fn shutdown_cancels_queued_work_and_reaps_the_running_worker() {
        let (started, observed) = mpsc::channel();
        let (release, ready) = mpsc::channel();
        let worker = SessionFavoritesWorker::with_runner(move |_| {
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
