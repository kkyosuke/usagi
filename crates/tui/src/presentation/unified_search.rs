//! A single cancellable reader for the unified picker.
use crate::usecase::application::unified_search::{Query, Results, UnifiedSearchPort};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Search(Query),
    Read(String),
}

pub(crate) enum Response {
    Search(Results),
    Read(Result<Vec<String>, String>),
}

pub(super) struct Completion {
    pub token: (u64, u64),
    pub response: Response,
    pub port: Box<dyn UnifiedSearchPort>,
}

pub(super) struct Job {
    token: (u64, u64),
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Job {
    pub(super) fn cancel_unless_current(&self, token: Option<(u64, u64)>) {
        if token != Some(self.token) {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn spawn(
    mut port: Box<dyn UnifiedSearchPort>,
    token: (u64, u64),
    request: Request,
    sender: Sender<Completion>,
) -> Job {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&cancelled);
    let worker = std::thread::spawn(move || {
        let response = match request {
            Request::Search(query) => Response::Search(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    port.search(&query, cancel)
                }))
                .unwrap_or_else(|_| Results {
                    errors: vec!["Search is unavailable; refresh to retry".into()],
                    ..Results::default()
                }),
            ),
            Request::Read(path) => Response::Read(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.read(&path)))
                    .unwrap_or_else(|_| Err("Document is unavailable".into())),
            ),
        };
        let _ = sender.send(Completion {
            token,
            response,
            port,
        });
    });
    Job {
        token,
        cancelled,
        worker: Some(worker),
    }
}

#[cfg(test)]
mod tests;
