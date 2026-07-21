//! Production adapters assembled at the application boundary.

use std::path::Path;

use anyhow::Result;

use crate::domain::workspace_state::SessionRecord;
use crate::usecase::session::{SessionInventory, SetupCommandRunner};

#[derive(Debug, Clone, Copy)]
pub struct ProductionSetupRunner;

impl SetupCommandRunner for ProductionSetupRunner {
    fn run(&self, cwd: &Path, command: &str) -> Result<()> {
        crate::infrastructure::setup_runner::run(cwd, command)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProductionSessionInventory;

impl SessionInventory for ProductionSessionInventory {
    fn workspace_root(&self, start: &Path) -> std::path::PathBuf {
        crate::infrastructure::session_inventory::workspace_root(start)
    }

    fn sessions(&self, workspace_root: &Path) -> Result<Vec<SessionRecord>> {
        crate::infrastructure::session_inventory::sessions(workspace_root)
    }
}

pub fn agent_snapshot(worktree: &Path) -> Result<crate::domain::workspace_state::SessionAgent> {
    crate::usecase::session::agent_snapshot(&ProductionSessionInventory, worktree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::AgentCli;
    use crate::domain::workspace_state::{
        SessionAgent, SessionOrigin, SessionRecord, WorkspaceState,
    };
    use crate::infrastructure::workspace_store::WorkspaceStore;
    use chrono::Utc;

    #[test]
    fn production_inventory_composition_reads_the_saved_session_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".usagi/sessions/work");
        std::fs::create_dir_all(&root).unwrap();
        let expected = SessionAgent {
            cli: Some(AgentCli::Claude),
            model: Some("production-model".into()),
        };
        WorkspaceStore::new(workspace.path())
            .save(&WorkspaceState {
                sessions: vec![SessionRecord {
                    name: "work".into(),
                    display_name: None,
                    note: None,
                    todos: Vec::new(),
                    decisions: Vec::new(),
                    label_id: None,
                    agent: expected.clone(),
                    origin: SessionOrigin::Mcp,
                    started_from: None,
                    root: root.clone(),
                    worktrees: Vec::new(),
                    worktree_provenance: Vec::new(),
                    created_at: Utc::now(),
                    last_active: None,
                }],
                ..WorkspaceState::default()
            })
            .unwrap();

        assert_eq!(agent_snapshot(&root).unwrap(), expected);
    }
}
