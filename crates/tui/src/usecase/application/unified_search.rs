//! Bounded read-only search across human-visible sources.
use super::{controller::Target, knowledge_search};
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::usecase::terminal_search::WorkspaceOutputSearch;

pub const RESULT_LIMIT: usize = 100;
pub const PROJECT_LIMIT: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    Terminal,
    Artifact,
    CurrentSession,
}

impl Scope {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Terminal,
            Self::Terminal => Self::Artifact,
            Self::Artifact => Self::CurrentSession,
            Self::CurrentSession => Self::All,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Terminal => "Terminal",
            Self::Artifact => "Artifact",
            Self::CurrentSession => "Current session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub workspace: WorkspaceId,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentSession {
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub scope: Scope,
    pub projects: Vec<Project>,
    pub current: Option<CurrentSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub workspace: WorkspaceId,
    pub project_root: PathBuf,
    pub target: Target,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    pub outputs: Vec<(WorkspaceId, Result<WorkspaceOutputSearch, String>)>,
    pub artifacts: Vec<Artifact>,
    pub knowledge: knowledge_search::Results,
    pub omitted: usize,
    pub errors: Vec<String>,
}

/// Owned by one cancellable background worker, never called by the frame thread.
pub trait UnifiedSearchPort: Send {
    fn search(&mut self, query: &Query, cancelled: Arc<AtomicBool>) -> Results;
    /// Read an exact Issue / Memory source through the existing bounded reader.
    /// # Errors
    /// Returns safe feedback for unavailable or invalid sources.
    fn read(&mut self, path: &str) -> Result<Vec<String>, String>;
}

pub struct UnavailableUnifiedSearch;

impl UnifiedSearchPort for UnavailableUnifiedSearch {
    fn search(&mut self, _: &Query, _: Arc<AtomicBool>) -> Results {
        Results {
            errors: vec!["Search sources are unavailable; refresh to retry".into()],
            ..Results::default()
        }
    }

    fn read(&mut self, _: &str) -> Result<Vec<String>, String> {
        Err("Document is unavailable".into())
    }
}

/// Rank artifact paths using the same fuzzy matcher as the other pickers.
#[must_use]
pub fn artifact_score(path: &str, query: &str) -> Option<usize> {
    crate::usecase::fuzzy::fuzzy_score(path, query)
}
