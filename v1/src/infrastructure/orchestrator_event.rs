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
const CREDENTIAL_DIR: &str = ".usagi/orchestrator-workers";
pub const CREDENTIAL_ENV: &str = "USAGI_ORCHESTRATOR_WORKER_CREDENTIAL";

/// Durable address attached to a worker when a generation is delegated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerBinding {
    pub workspace: PathBuf,
    pub plan: String,
    pub issue: u64,
    pub generation: u64,
    #[serde(default)]
    pub credential: Option<String>,
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
    let credential = binding
        .credential
        .as_deref()
        .context("worker binding requires an immutable credential")?;
    validate_credential(credential)?;
    let credential_dir = worker_worktree.join(CREDENTIAL_DIR);
    let credential_path = credential_dir.join(format!("{credential}.json"));
    if credential_path.exists() {
        let existing: WorkerBinding = json_file::read(&credential_path)?
            .context("worker credential disappeared while registering")?;
        if existing != *binding {
            anyhow::bail!("worker credential {credential:?} is already bound differently");
        }
    } else {
        json_file::write_atomic(&credential_dir, &credential_path, binding)?;
    }
    json_file::write_atomic(&dir, &worker_worktree.join(BINDING), binding)
}

pub fn binding(worker_worktree: &Path) -> Result<Option<WorkerBinding>> {
    json_file::read(&worker_worktree.join(BINDING))
}

pub fn credential_binding(
    worker_worktree: &Path,
    credential: &str,
) -> Result<Option<WorkerBinding>> {
    validate_credential(credential)?;
    json_file::read(
        &worker_worktree
            .join(CREDENTIAL_DIR)
            .join(format!("{credential}.json")),
    )
}

fn validate_credential(credential: &str) -> Result<()> {
    if credential.is_empty()
        || !credential
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid orchestrator worker credential")
    }
    Ok(())
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
    let credential = std::env::var(CREDENTIAL_ENV).ok();
    emit_with_credential_and_liveness(
        worker_worktree,
        credential.as_deref(),
        kind,
        terminal_revision,
        observed_at,
        pid_alive,
    )
}

fn emit_with_credential_and_liveness(
    worker_worktree: &Path,
    credential: Option<&str>,
    kind: EventKind,
    terminal_revision: u64,
    observed_at: DateTime<Utc>,
    pid_alive: fn(u32) -> bool,
) -> Result<Option<EmitResult>> {
    // The active file is used only to route a legacy/unknown event to its plan.
    // Provenance always comes from the immutable credential captured at spawn.
    let active = binding(worker_worktree)?;
    let resolved = match credential {
        Some(value) => credential_binding(worker_worktree, value)?,
        None => None,
    };
    let Some(binding) = resolved.or(active) else {
        return Ok(None);
    };
    let generation = if credential.is_some() && binding.credential.as_deref() == credential {
        binding.generation
    } else {
        u64::MAX
    };
    let id = Event::deterministic_id(
        &binding.plan,
        binding.issue,
        generation,
        &kind,
        terminal_revision,
    );
    let event = Event {
        id: id.clone(),
        plan: binding.plan.clone(),
        issue: binding.issue,
        generation,
        credential: credential.map(str::to_owned),
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
                credential: Some("p-184-7".into()),
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
            let first = emit_with_credential_and_liveness(
                worker,
                Some("p-184-7"),
                EventKind::Succeeded,
                3,
                now(),
                |_| false,
            )
            .unwrap()
            .unwrap();
            let second = emit_with_credential_and_liveness(
                worker,
                Some("p-184-7"),
                EventKind::Succeeded,
                3,
                now(),
                |_| false,
            )
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
            let result = emit_with_credential_and_liveness(
                worker,
                Some("p-184-7"),
                EventKind::PrOpened,
                1,
                now(),
                |_| true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(result.wake_channel, WakeChannel::Live);
            assert_eq!(agent_live_prompt_store::take_all(owner).len(), 1);
        });
    }

    #[test]
    fn old_process_credential_cannot_borrow_a_new_active_generation() {
        with_fixture(|workspace, worker, _| {
            register(
                worker,
                &WorkerBinding {
                    workspace: workspace.into(),
                    plan: "p".into(),
                    issue: 184,
                    generation: 8,
                    credential: Some("p-184-8".into()),
                    owner_worktree: worker.into(),
                },
            )
            .unwrap();

            emit_with_credential_and_liveness(
                worker,
                Some("p-184-7"),
                EventKind::Succeeded,
                9,
                now(),
                |_| false,
            )
            .unwrap();
            let events = OrchestratorStore::new(workspace).load_events("p").unwrap();
            let old = events
                .iter()
                .find(|event| event.terminal_revision == 9)
                .unwrap();
            assert_eq!(old.generation, 7);
            assert_eq!(old.credential.as_deref(), Some("p-184-7"));
        });
    }

    #[test]
    fn legacy_process_without_credential_is_marked_unknown() {
        with_fixture(|workspace, worker, _| {
            emit_with_credential_and_liveness(
                worker,
                None,
                EventKind::Succeeded,
                10,
                now(),
                |_| false,
            )
            .unwrap();
            let events = OrchestratorStore::new(workspace).load_events("p").unwrap();
            let legacy = events
                .iter()
                .find(|event| event.terminal_revision == 10)
                .unwrap();
            assert_eq!(legacy.generation, u64::MAX);
            assert_eq!(legacy.credential, None);
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

    #[test]
    fn credential_paths_reject_empty_and_traversal_values() {
        let worker = tempfile::tempdir().unwrap();
        for credential in ["", "../escape", "with/slash"] {
            assert!(credential_binding(worker.path(), credential).is_err());
        }
    }

    #[test]
    fn registration_requires_one_immutable_binding_per_credential() {
        let worker = tempfile::tempdir().unwrap();
        let mut binding = WorkerBinding {
            workspace: worker.path().into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            credential: Some("generation-1".into()),
            owner_worktree: worker.path().into(),
        };
        register(worker.path(), &binding).unwrap();
        register(worker.path(), &binding).unwrap();
        binding.issue = 2;
        assert!(register(worker.path(), &binding).is_err());
        binding.credential = None;
        assert!(register(worker.path(), &binding).is_err());
    }
}
