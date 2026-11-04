//! Read-only Issue/Memory discovery boundary for the human TUI.
use std::sync::{Arc, atomic::AtomicBool};

/// Maximum displayed results per store. A full store scan can return more.
pub const RESULT_LIMIT: usize = 100;
/// Maximum query length in Unicode scalar values.
pub const QUERY_LIMIT: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Issue,
    Memory,
}

/// Exact source identity, independent of an issue number or memory slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub kind: Kind,
    pub path: String,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    pub documents: Vec<Document>,
    pub omitted: usize,
    pub errors: Vec<String>,
}

/// Moved into a worker; neither method runs on the frame thread.
pub trait KnowledgeSearchPort: Send {
    /// Observe cancellation during lock waits and between stores. A cancelled
    /// response retains its request token and is discarded by the frame loop.
    fn search(&mut self, query: &str, cancelled: Arc<AtomicBool>) -> Results;
    /// Read the current exact source using the bounded preview policy.
    /// # Errors
    /// Returns safe feedback when the source is missing or cannot be previewed.
    fn read(&mut self, path: &str) -> Result<Vec<String>, String>;
}

pub struct UnavailableKnowledgeSearch;

impl KnowledgeSearchPort for UnavailableKnowledgeSearch {
    fn search(&mut self, _: &str, _: Arc<AtomicBool>) -> Results {
        Results {
            errors: vec!["Issue / Memory search is unavailable".into()],
            ..Results::default()
        }
    }

    fn read(&mut self, _: &str) -> Result<Vec<String>, String> {
        Err("Document is unavailable".into())
    }
}
