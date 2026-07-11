//! Durable worker lifecycle events and best-effort owner wake-ups.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::orchestrator::{Event, EventKind};
use crate::infrastructure::{
    agent_live_pane_store, agent_live_prompt_store, agent_prompt_store, json_file,
    orchestrator_store::OrchestratorStore, resource,
};

const BINDING: &str = ".usagi/orchestrator-worker.json";

/// Durable address attached to a worker when a generation is delegated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerBinding {
    pub workspace: PathBuf,
    pub plan: String,
    pub issue: u64,
    pub generation: u64,
    pub owner_worktree: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeChannel {
    Live,
    Launch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitResult {
    pub created: bool,
    pub wake_channel: WakeChannel,
}

/// Atomically records the generation identity before its agent can start.
pub fn register(worker_worktree: &Path, binding: &WorkerBinding) -> Result<()> {
    let dir = worker_worktree.join(".usagi");
    json_file::write_atomic(&dir, &worker_worktree.join(BINDING), binding)
}

pub fn binding(worker_worktree: &Path) -> Result<Option<WorkerBinding>> {
    json_file::read(&worker_worktree.join(BINDING))
}

/// Persist an event first, then use an owner queue only as a wake-up signal.
/// A queue error is returned while the event remains available for a retry.
pub fn emit(
    worker_worktree: &Path,
    kind: EventKind,
    terminal_revision: u64,
    observed_at: DateTime<Utc>,
) -> Result<Option<EmitResult>> {
    emit_with_liveness(
        worker_worktree,
        kind,
        terminal_revision,
        observed_at,
        resource::process_alive,
    )
}

fn emit_with_liveness(
    worker_worktree: &Path,
    kind: EventKind,
    terminal_revision: u64,
    observed_at: DateTime<Utc>,
    pid_alive: fn(u32) -> bool,
) -> Result<Option<EmitResult>> {
    let Some(binding) = binding(worker_worktree)? else {
        return Ok(None);
    };
    let id = Event::deterministic_id(
        &binding.plan,
        binding.issue,
        binding.generation,
        &kind,
        terminal_revision,
    );
    let event = Event {
        id: id.clone(),
        plan: binding.plan.clone(),
        issue: binding.issue,
        generation: binding.generation,
        kind,
        terminal_revision,
        observed_at,
    };
    let created = OrchestratorStore::new(&binding.workspace).append_event(&event)?;
    let prompt = format!(
        "Orchestrator event {id} is pending. Reconcile plan {:?} and acknowledge the event after its effect is durably saved.",
        binding.plan
    );
    let wake_channel = if agent_live_pane_store::is_live(&binding.owner_worktree, pid_alive) {
        agent_live_prompt_store::append(&binding.owner_worktree, &prompt)
            .context("event was saved, but the owner live wake-up could not be queued")?;
        WakeChannel::Live
    } else {
        agent_prompt_store::set(&binding.owner_worktree, &prompt)
            .context("event was saved, but the owner launch wake-up could not be queued")?;
        WakeChannel::Launch
    };
    Ok(Some(EmitResult {
        created,
        wake_channel,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::{
        agent_live_pane_store, agent_live_prompt_store, agent_prompt_store, storage,
    };

    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn with_fixture(body: impl FnOnce(&Path, &Path, &Path)) {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let worker = tempfile::tempdir().unwrap();
        let owner = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, data.path());
        register(
            worker.path(),
            &WorkerBinding {
                workspace: workspace.path().into(),
                plan: "p".into(),
                issue: 184,
                generation: 7,
                owner_worktree: owner.path().into(),
            },
        )
        .unwrap();
        body(workspace.path(), worker.path(), owner.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn absent_owner_gets_launch_wakeup_and_duplicate_event_is_one_file() {
        with_fixture(|workspace, worker, owner| {
            let first = emit_with_liveness(worker, EventKind::Succeeded, 3, now(), |_| false)
                .unwrap()
                .unwrap();
            let second = emit_with_liveness(worker, EventKind::Succeeded, 3, now(), |_| false)
                .unwrap()
                .unwrap();
            assert!(first.created);
            assert!(!second.created);
            assert_eq!(first.wake_channel, WakeChannel::Launch);
            assert!(agent_prompt_store::take(owner)
                .unwrap()
                .contains("p-184-7-succeeded-3"));
            assert_eq!(
                OrchestratorStore::new(workspace)
                    .load_events("p")
                    .unwrap()
                    .len(),
                1
            );
        });
    }

    #[test]
    fn live_owner_gets_live_wakeup() {
        with_fixture(|_, worker, owner| {
            agent_live_pane_store::set(owner, 42).unwrap();
            let result = emit_with_liveness(worker, EventKind::PrOpened, 1, now(), |_| true)
                .unwrap()
                .unwrap();
            assert_eq!(result.wake_channel, WakeChannel::Live);
            assert_eq!(agent_live_prompt_store::take_all(owner).len(), 1);
        });
    }

    #[test]
    fn unregistered_worker_does_not_emit() {
        let worker = tempfile::tempdir().unwrap();
        assert_eq!(
            emit(worker.path(), EventKind::Failed, 0, now()).unwrap(),
            None
        );
    }
}
