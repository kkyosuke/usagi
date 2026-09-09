//! Application-owned persistence boundaries.
//!
//! Usecases depend on these contracts rather than file-store implementations.
//! Infrastructure adapters retain their lock for the duration of each
//! transaction callback, keeping read-modify-write operations atomic without
//! exposing a concrete lock type to the application layer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::issue::{Issue, IssueSummary};
use crate::domain::memory::{Memory, MemorySummary};
use crate::domain::recent::Unite;
use crate::domain::workspace::Workspace;
use crate::domain::workspace_state::WorkspaceState;

/// One parseable issue and the filename identity observed with it.
#[derive(Debug, Clone)]
pub struct IssueSource {
    pub issue: Issue,
    pub file: String,
    pub filename_number: Option<u32>,
}

impl IssueSource {
    #[must_use]
    pub fn summary(self) -> IssueSummary {
        self.issue.summary(&self.file)
    }
}

/// One consistent issue-source observation used for readiness calculation.
#[derive(Debug, Clone, Default)]
pub struct IssueSourceSnapshot {
    pub sources: Vec<IssueSource>,
    pub claims: BTreeMap<u32, Vec<PathBuf>>,
}

/// Operations available while an issue repository transaction is locked.
pub trait IssueTransaction {
    /// Read a single issue from the locked source set.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is ambiguous or cannot be read.
    fn get(&self, number: u32) -> Result<Option<Issue>>;

    /// Capture all parseable sources and filename claims under the same lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the source directory cannot be enumerated.
    fn source_snapshot(&self) -> Result<IssueSourceSnapshot>;

    /// Reserve the next workspace-wide issue number.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable sequence cannot be updated.
    fn reserve_next_number(&self) -> Result<u32>;

    /// Commit one issue source.
    ///
    /// # Errors
    ///
    /// Returns an error when the source cannot be committed.
    fn save(&self, issue: &Issue) -> Result<()>;
}

/// Persistence required by issue CRUD usecases.
pub trait IssueRepository {
    /// Run `operation` while holding the repository's write lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired or `operation` fails.
    fn transact<T>(&self, operation: impl FnOnce(&dyn IssueTransaction) -> Result<T>) -> Result<T>;

    /// Read one issue outside a larger transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the issue cannot be read unambiguously.
    fn get(&self, number: u32) -> Result<Option<Issue>>;

    /// Read issue summaries.
    ///
    /// # Errors
    ///
    /// Returns an error when neither the derived index nor sources can be read.
    fn summaries(&self) -> Result<Vec<IssueSummary>>;

    /// Capture one consistent source snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the source directory cannot be observed.
    fn source_snapshot(&self) -> Result<IssueSourceSnapshot>;

    /// Delete one issue.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is ambiguous or removal fails.
    fn delete(&self, number: u32) -> Result<bool>;
}

/// One memory source and the exact filename from which it was read.
#[derive(Debug, Clone)]
pub struct MemorySource {
    pub memory: Memory,
    pub file: String,
}

/// Operations available while a memory repository transaction is locked.
pub trait MemoryTransaction {
    /// Read one memory from the locked source set.
    ///
    /// # Errors
    ///
    /// Returns an error when the name or source is invalid.
    fn get(&self, name: &str) -> Result<Option<Memory>>;

    /// Commit one memory source.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is unsafe or the source cannot be written.
    fn save(&self, memory: &Memory) -> Result<()>;
}

/// Persistence required by memory CRUD usecases.
pub trait MemoryRepository {
    /// Run `operation` while holding the repository's write lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired or `operation` fails.
    fn transact<T>(&self, operation: impl FnOnce(&dyn MemoryTransaction) -> Result<T>)
    -> Result<T>;

    /// Read one memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the name or source is invalid.
    fn get(&self, name: &str) -> Result<Option<Memory>>;

    /// Read all memory summaries.
    ///
    /// # Errors
    ///
    /// Returns an error when neither the derived index nor sources can be read.
    fn summaries(&self) -> Result<Vec<MemorySummary>>;

    /// Read source-backed memories while preserving their filenames.
    ///
    /// # Errors
    ///
    /// Returns an error when the source directory cannot be observed.
    fn sources(&self) -> Result<Vec<MemorySource>>;

    /// Delete one memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is unsafe or removal fails.
    fn delete(&self, name: &str) -> Result<bool>;
}

/// Operations available while a workspace-state transaction is locked.
pub trait WorkspaceStateTransaction {
    /// Load the current state, if present.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be read or parsed.
    fn load(&self) -> Result<Option<WorkspaceState>>;

    /// Persist the complete state.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be written.
    fn save(&self, state: &WorkspaceState) -> Result<()>;
}

/// Persistence required by workspace note and environment usecases.
pub trait WorkspaceStateRepository {
    /// Load state outside a larger transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be read or parsed.
    fn load(&self) -> Result<Option<WorkspaceState>>;

    /// Run `operation` while holding the state write lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired or `operation` fails.
    fn transact<T>(
        &self,
        operation: impl FnOnce(&dyn WorkspaceStateTransaction) -> Result<T>,
    ) -> Result<T>;
}

/// Operations available while the global workspace registry is locked.
pub trait WorkspaceRegistryTransaction {
    /// Load registered workspaces.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    fn load_workspaces(&self) -> Result<Vec<Workspace>>;

    /// Persist registered workspaces.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be written.
    fn save_workspaces(&self, workspaces: &[Workspace]) -> Result<()>;

    /// Load saved Unite entries.
    ///
    /// # Errors
    ///
    /// Returns an error when Unite storage cannot be read.
    fn load_unites(&self) -> Result<Vec<Unite>>;

    /// Persist saved Unite entries.
    ///
    /// # Errors
    ///
    /// Returns an error when Unite storage cannot be written.
    fn save_unites(&self, unites: &[Unite]) -> Result<()>;
}

/// Persistence required by workspace registry and recent-list usecases.
pub trait WorkspaceRepository {
    /// Run `operation` while holding the global registry lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired or `operation` fails.
    fn transact<T>(
        &self,
        operation: impl FnOnce(&dyn WorkspaceRegistryTransaction) -> Result<T>,
    ) -> Result<T>;

    /// Load registered workspaces.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    fn load_workspaces(&self) -> Result<Vec<Workspace>>;

    /// Load optional Unite recents.
    ///
    /// # Errors
    ///
    /// Returns an error when Unite storage cannot be read.
    fn load_unites(&self) -> Result<Vec<Unite>>;

    /// Load repository-local state for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when state cannot be read or parsed.
    fn workspace_state(&self, root: &Path) -> Result<Option<WorkspaceState>>;

    /// Load issue summaries for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when issue sources cannot be read.
    fn issue_summaries(&self, root: &Path) -> Result<Vec<IssueSummary>>;
}
