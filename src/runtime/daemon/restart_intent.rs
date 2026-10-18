//! A new CLI's durable continuation intent, understood by a new successor even
//! when the old owner writes a pre-continue restart transaction.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use usagi_core::domain::agent::DaemonRestartAgentPlan;
use usagi_core::domain::id::{AgentRuntimeRef, DaemonGeneration};
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_core::infrastructure::persistence::json_file;

use super::{AGENT_RUNTIME_LIMIT, PendingDaemonAgentRestart};

const FILE: &str = "agent-restart-intent.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestartIntent {
    schema_version: u16,
    operation_id: String,
    from_generation: DaemonGeneration,
    runtimes: Vec<AgentRuntimeRef>,
}

impl RestartIntent {
    pub(super) fn new(operation_id: &str, plan: &DaemonRestartAgentPlan) -> Option<Self> {
        Some(Self {
            schema_version: 1,
            operation_id: operation_id.to_owned(),
            from_generation: plan.agents.first()?.runtime.terminal.daemon_generation,
            runtimes: plan
                .agents
                .iter()
                .map(|agent| agent.runtime.clone())
                .collect(),
        })
    }

    pub(super) fn apply(&self, pending: &mut PendingDaemonAgentRestart) -> io::Result<bool> {
        if self.operation_id != pending.operation_id {
            return Ok(false);
        }
        let mut expected = self.runtimes.clone();
        let mut actual = pending
            .agents
            .iter()
            .map(|item| item.agent.runtime.clone())
            .collect::<Vec<_>>();
        expected.sort_by_key(|runtime| runtime.agent_runtime_id.as_str());
        actual.sort_by_key(|runtime| runtime.agent_runtime_id.as_str());
        if self.schema_version != 1
            || expected.is_empty()
            || expected.len() > AGENT_RUNTIME_LIMIT
            || self.from_generation != pending.from_generation
            || expected != actual
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Agent restart intent does not match the exact stopped runtimes",
            ));
        }
        let mut changed = false;
        for item in &mut pending.agents {
            if item.completed {
                continue;
            }
            let continue_work =
                matches!(item.agent.phase, AgentPhase::Running | AgentPhase::Waiting);
            changed |= item.continue_work != continue_work;
            item.continue_work = continue_work;
        }
        Ok(changed)
    }
}

pub(super) fn read(data_dir: &Path) -> io::Result<Option<RestartIntent>> {
    json_file::read_bounded(&data_dir.join("daemon").join(FILE), 256 * 1024)
        .map_err(io::Error::other)
}

pub(super) fn write(data_dir: &Path, intent: &RestartIntent) -> io::Result<()> {
    json_file::write_atomic(
        &data_dir.join("daemon"),
        &data_dir.join("daemon").join(FILE),
        intent,
    )
    .map_err(io::Error::other)
}

pub(super) fn clear(data_dir: &Path, operation_id: &str) -> io::Result<bool> {
    if read(data_dir)?.is_none_or(|intent| intent.operation_id != operation_id) {
        return Ok(false);
    }
    std::fs::remove_file(data_dir.join("daemon").join(FILE))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::daemon::tests::daemon_restart_plan;

    #[test]
    fn only_the_matching_intent_adds_continue_to_an_uncompleted_legacy_item() {
        let directory = tempfile::tempdir().unwrap();
        let plan = daemon_restart_plan(usagi_core::domain::id::WorkspaceId::new());
        let operation = super::super::OperationId("restart".to_owned());
        let mut pending = PendingDaemonAgentRestart::new(
            &operation,
            plan.agents[0].runtime.terminal.daemon_generation,
            directory.path().to_path_buf(),
            &plan,
        );
        let mut legacy = serde_json::to_value(&pending).unwrap();
        legacy["agents"][0]
            .as_object_mut()
            .unwrap()
            .remove("continue_work");
        pending = serde_json::from_value(legacy).unwrap();
        assert!(!pending.agents[0].continue_work);
        assert!(
            RestartIntent::new("empty", &DaemonRestartAgentPlan { agents: Vec::new() }).is_none()
        );
        let mut intent = RestartIntent::new("other", &plan).unwrap();
        assert!(!intent.apply(&mut pending).unwrap());
        assert!(!pending.agents[0].continue_work);
        intent.operation_id = operation.0.clone();
        intent.schema_version = 2;
        assert!(intent.apply(&mut pending).is_err());
        intent.schema_version = 1;
        let generation = intent.from_generation;
        intent.from_generation = DaemonGeneration::new();
        assert!(intent.apply(&mut pending).is_err());
        intent.from_generation = generation;
        let runtime = intent.runtimes[0].clone();
        intent.runtimes[0].agent_runtime_id = usagi_core::domain::id::AgentRuntimeId::new();
        assert!(intent.apply(&mut pending).is_err());
        intent.runtimes[0] = runtime;
        assert!(intent.apply(&mut pending).unwrap());
        assert!(pending.agents[0].continue_work);
        assert!(!intent.apply(&mut pending).unwrap());
        pending.agents[0].continue_work = false;
        pending.agents[0].completed = true;
        assert!(!intent.apply(&mut pending).unwrap());
        pending.agents[0].completed = false;
        pending.agents[0].agent.phase = AgentPhase::Ready;
        assert!(!intent.apply(&mut pending).unwrap());
        write(directory.path(), &intent).unwrap();
        assert!(read(directory.path()).unwrap().is_some());
        assert!(!clear(directory.path(), "other").unwrap());
        assert!(clear(directory.path(), &operation.0).unwrap());
        assert!(!clear(directory.path(), &operation.0).unwrap());
        std::fs::create_dir(directory.path().join("daemon").join(FILE)).unwrap();
        assert!(write(directory.path(), &intent).is_err());
        assert!(read(directory.path()).is_err());
        assert!(clear(directory.path(), &operation.0).is_err());
    }

    #[test]
    fn exact_runtime_matching_ignores_item_order_and_preserves_idle_conversations() {
        let workspace = usagi_core::domain::id::WorkspaceId::new();
        let mut plan = daemon_restart_plan(workspace);
        let mut second = plan.agents[0].clone();
        second.runtime.agent_runtime_id = usagi_core::domain::id::AgentRuntimeId::new();
        second.target.runtime_id = second.runtime.agent_runtime_id;
        second.phase = AgentPhase::Waiting;
        plan.agents.push(second);
        let operation = super::super::OperationId("ordered".to_owned());
        let mut pending = PendingDaemonAgentRestart::new(
            &operation,
            plan.agents[0].runtime.terminal.daemon_generation,
            std::path::PathBuf::from("/workspace"),
            &plan,
        );
        let intent = RestartIntent::new(&operation.0, &plan).unwrap();
        pending.agents.reverse();
        for item in &mut pending.agents {
            item.continue_work = false;
        }
        assert!(intent.apply(&mut pending).unwrap());
        assert!(pending.agents.iter().all(|item| item.continue_work));
        pending.agents[0].agent.phase = AgentPhase::Ended;
        pending.agents[1].agent.phase = AgentPhase::Ready;
        assert!(intent.apply(&mut pending).unwrap());
        assert!(pending.agents.iter().all(|item| !item.continue_work));
    }
}
