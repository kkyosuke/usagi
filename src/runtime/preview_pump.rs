//! Bounded resident worker for Home file-preview requests.
//!
//! The render thread only replaces the single pending request and drains the
//! single latest completion. Slow filesystem or Git work stays on this worker;
//! a newer request or explicit cancellation fences out an in-flight result.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use usagi_core::domain::id::OperationId;
use usagi_tui::usecase::application::controller::{PreviewFileFilter, Target};

use super::file_preview::FilePreviewError;

pub(crate) type PreviewPayload = (Vec<String>, Vec<String>);

#[derive(Debug)]
pub(crate) struct PreviewCompletion {
    pub(crate) target: Target,
    pub(crate) request_id: OperationId,
    pub(crate) path: Option<String>,
    pub(crate) filter: PreviewFileFilter,
    pub(crate) result: Result<PreviewPayload, FilePreviewError>,
}

struct PreviewJob {
    generation: u64,
    target: Target,
    request_id: OperationId,
    path: Option<String>,
    filter: PreviewFileFilter,
    root: PathBuf,
}

#[derive(Default)]
struct PreviewState {
    generation: u64,
    pending: Option<PreviewJob>,
    completed: Option<PreviewCompletion>,
    stopped: bool,
}

struct Shared {
    state: Mutex<PreviewState>,
    signal: Condvar,
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, PreviewState> {
    shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One resident, coalescing preview lane. Its queue is bounded to one pending
/// request and one completion regardless of input rate.
pub(crate) struct PreviewPump {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl PreviewPump {
    pub(crate) fn spawn<F>(mut fetch: F) -> Self
    where
        F: FnMut(
                &Path,
                Option<&str>,
                PreviewFileFilter,
            ) -> Result<PreviewPayload, FilePreviewError>
            + Send
            + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(PreviewState::default()),
            signal: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::spawn(move || {
            loop {
                let job = {
                    let mut state = lock(&worker);
                    while state.pending.is_none() && !state.stopped {
                        state = worker
                            .signal
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if state.stopped {
                        return;
                    }
                    state.pending.take().expect("pending preview job")
                };
                let result = fetch(&job.root, job.path.as_deref(), job.filter);
                let mut state = lock(&worker);
                if state.generation == job.generation && !state.stopped {
                    state.completed = Some(PreviewCompletion {
                        target: job.target,
                        request_id: job.request_id,
                        path: job.path,
                        filter: job.filter,
                        result,
                    });
                }
            }
        });
        Self {
            shared,
            handle: Some(handle),
        }
    }

    pub(crate) fn request(
        &self,
        target: Target,
        request_id: OperationId,
        path: Option<String>,
        filter: PreviewFileFilter,
        root: PathBuf,
    ) {
        let mut state = lock(&self.shared);
        state.generation = state.generation.wrapping_add(1);
        state.pending = Some(PreviewJob {
            generation: state.generation,
            target,
            request_id,
            path,
            filter,
            root,
        });
        state.completed = None;
        drop(state);
        self.shared.signal.notify_one();
    }

    pub(crate) fn cancel(&self) {
        let mut state = lock(&self.shared);
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.completed = None;
    }

    pub(crate) fn take(&self) -> Option<PreviewCompletion> {
        lock(&self.shared).completed.take()
    }
}

impl Drop for PreviewPump {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared);
            state.stopped = true;
            state.pending = None;
            state.completed = None;
        }
        self.shared.signal.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use usagi_core::domain::id::WorkspaceId;

    use super::*;

    fn take_until(pump: &PreviewPump) -> PreviewCompletion {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(completion) = pump.take() {
                return completion;
            }
            assert!(Instant::now() < deadline, "preview worker did not complete");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_request_runs_off_thread_and_returns_one_completion() {
        let pump = PreviewPump::spawn(|root, path, filter| {
            assert_eq!(filter, PreviewFileFilter::Tracked);
            Ok((
                vec![root.display().to_string()],
                vec![path.unwrap_or_default().to_owned()],
            ))
        });
        let target = Target::Root(WorkspaceId::new());
        let request_id = OperationId::new();
        pump.request(
            target,
            request_id,
            Some("README.md".to_owned()),
            PreviewFileFilter::Tracked,
            PathBuf::from("/repo"),
        );
        let completion = take_until(&pump);
        assert_eq!(completion.target, target);
        assert_eq!(completion.request_id, request_id);
        assert_eq!(completion.path.as_deref(), Some("README.md"));
        assert_eq!(completion.filter, PreviewFileFilter::Tracked);
        assert_eq!(
            completion.result.unwrap(),
            (vec!["/repo".to_owned()], vec!["README.md".to_owned()])
        );
        assert!(pump.take().is_none());
    }

    #[test]
    fn a_new_request_fences_the_in_flight_result_and_coalesces_the_queue() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut calls = 0;
        let pump = PreviewPump::spawn(move |_, path, _| {
            calls += 1;
            if calls == 1 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Ok((Vec::new(), vec![path.unwrap_or_default().to_owned()]))
        });
        let target = Target::Root(WorkspaceId::new());
        pump.request(
            target,
            OperationId::new(),
            Some("first".to_owned()),
            PreviewFileFilter::All,
            PathBuf::from("/repo"),
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        pump.request(
            target,
            OperationId::new(),
            Some("second".to_owned()),
            PreviewFileFilter::Changed,
            PathBuf::from("/repo"),
        );
        let latest_request = OperationId::new();
        pump.request(
            target,
            latest_request,
            Some("latest".to_owned()),
            PreviewFileFilter::Tracked,
            PathBuf::from("/repo"),
        );
        release_tx.send(()).unwrap();

        let completion = take_until(&pump);
        assert_eq!(completion.request_id, latest_request);
        assert_eq!(completion.path.as_deref(), Some("latest"));
        assert_eq!(completion.filter, PreviewFileFilter::Tracked);
        assert_eq!(completion.result.unwrap().1, vec!["latest"]);
    }

    #[test]
    fn cancellation_discards_pending_and_in_flight_results() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let pump = PreviewPump::spawn(move |_, _, _| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Err(FilePreviewError::FilesUnavailable)
        });
        pump.request(
            Target::Root(WorkspaceId::new()),
            OperationId::new(),
            None,
            PreviewFileFilter::All,
            PathBuf::from("/repo"),
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        pump.cancel();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        while Instant::now() < deadline {
            assert!(pump.take().is_none());
            std::thread::yield_now();
        }
    }
}
