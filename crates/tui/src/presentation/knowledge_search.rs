//! One background reader, returned to the frame loop after each request.
use crate::usecase::application::knowledge_search::{KnowledgeSearchPort, Results};
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Search(String),
    Read(String),
}

pub(crate) enum Response {
    Search(Results),
    Read(Result<Vec<String>, String>),
}

pub(super) struct Completion {
    pub token: (u64, u64),
    pub response: Response,
    pub port: Box<dyn KnowledgeSearchPort>,
}

pub(super) struct Job(Option<std::thread::JoinHandle<()>>);

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn spawn(
    mut port: Box<dyn KnowledgeSearchPort>,
    token: (u64, u64),
    request: Request,
    sender: Sender<Completion>,
) -> Job {
    Job(Some(std::thread::spawn(move || {
        let response = match request {
            Request::Search(query) => Response::Search(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.search(&query)))
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
    })))
}

#[cfg(test)]
mod tests;
