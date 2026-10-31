//! Owned background readers always cancel and join before leaving the frame loop.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

pub(super) struct Job {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Job {
    pub(super) fn spawn(task: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        Self {
            cancelled,
            worker: Some(std::thread::spawn(move || task(cancel))),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A search is current only while both its overlay epoch and query revision match.
pub(super) struct SearchJob {
    token: (u64, u64),
    worker: Job,
}

impl SearchJob {
    pub(super) fn spawn(
        token: (u64, u64),
        task: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> Self {
        Self {
            token,
            worker: Job::spawn(task),
        }
    }

    pub(super) fn cancel_unless_current(&self, token: Option<(u64, u64)>) {
        if token != Some(self.token) {
            self.worker.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn stale_search_and_closed_overlay_cancel_while_current_search_keeps_running() {
        for target in [Some((1, 1)), None] {
            let job = SearchJob::spawn((1, 0), |_| {});
            job.cancel_unless_current(Some((1, 0)));
            assert!(!job.worker.cancelled.load(Ordering::Acquire));
            job.cancel_unless_current(target);
            assert!(job.worker.cancelled.load(Ordering::Acquire));
        }
    }

    #[test]
    fn drop_cancels_and_waits_for_the_worker_to_finish() {
        let (sender, receiver) = mpsc::channel();
        let job = Job::spawn(move |cancelled| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut waiting_for_cancellation = true;
            while waiting_for_cancellation {
                assert!(Instant::now() < deadline, "drop must cancel the worker");
                std::thread::yield_now();
                waiting_for_cancellation = !cancelled.load(Ordering::Acquire);
            }
            sender.send(()).unwrap();
        });
        drop(job);
        receiver.try_recv().expect("drop must join the worker");
    }

    #[test]
    fn dropping_a_panicked_worker_does_not_panic() {
        drop(Job::spawn(|_| panic!("injected reader failure")));
    }
}
