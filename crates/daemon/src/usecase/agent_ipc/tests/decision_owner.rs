//! Decision authority follows exact conversations without retaining old runs.

use super::*;
use crate::usecase::runtime::{DurableRuntimeRecord, RuntimeSnapshotError};

struct Conversation {
    agent: AgentRuntime,
    current: AgentAdmission,
    caller: CallerRef,
    scope: FakeScope,
    _fixture: tempfile::TempDir,
    _worktree: tempfile::TempDir,
}

impl Conversation {
    fn delegated() -> Self {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let scope = FakeScope(Ok(configured_scope(worktree.path())));
        let mut agent = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
        let caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        let current = agent
            .dispatch(
                &OperationId::new().to_string(),
                &DispatchIntent {
                    workspace: WorkspaceId::new(),
                    session_name: "worker".into(),
                    caller: caller.clone(),
                    agent: DispatchAgentIntent::New {
                        runtime: AgentProfileId::new("claude").unwrap(),
                        model: ModelSelector::new("test").unwrap(),
                    },
                    prompt: "finish the task".into(),
                },
                SessionId::new(),
                &scope,
            )
            .unwrap();
        Self {
            agent,
            current,
            caller,
            scope,
            _fixture: fixture,
            _worktree: worktree,
        }
    }

    fn operation(&self) -> OperationId {
        OperationId::parse(&self.current.operation_id).unwrap()
    }

    fn credential(&self) -> String {
        self.agent
            .mcp_callers
            .iter()
            .find(|(_, caller)| caller.operation == self.operation())
            .unwrap()
            .0
            .clone()
    }

    fn record(&self) -> DurableRuntimeRecord {
        self.agent
            .coordinator
            .record_for(&self.current.runtime)
            .unwrap()
            .clone()
    }

    fn finish(&mut self) {
        self.agent.exit(&self.current.terminal, 0).unwrap();
    }

    fn resume(&mut self) {
        let target = self
            .agent
            .inventory(self.current.terminal.workspace_id)
            .resumable
            .into_iter()
            .find(|item| item.runtime_id == self.current.runtime.agent_runtime_id)
            .unwrap()
            .target
            .unwrap();
        self.current = self
            .agent
            .resume_exact(&OperationId::new().to_string(), &target, &self.scope)
            .unwrap();
    }

    fn owner(&self) -> UserDecisionOwner {
        self.agent
            .mcp_user_decision_owner(&self.credential())
            .unwrap()
    }

    fn owners(&self) -> Vec<UserDecisionOwner> {
        self.agent
            .mcp_user_decision_owners(&self.credential())
            .unwrap()
    }

    fn restore(&mut self, snapshot: RuntimeStoreSnapshot) {
        self.agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    }

    fn forget_dispatch_history(&self, operation: OperationId) {
        let path = self.agent.dispatch.registry_path();
        let mut registry: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for field in ["runs", "bindings", "admissions"] {
            registry[field].as_array_mut().unwrap().retain(|entry| {
                entry[if field == "admissions" {
                    "operation_id"
                } else {
                    "run_id"
                }] != json!(operation)
            });
        }
        std::fs::write(path, serde_json::to_vec(&registry).unwrap()).unwrap();
    }

    fn collect_ancestors(&mut self) {
        let current = self.current.runtime.clone();
        let mut snapshot = self.agent.coordinator.snapshot();
        snapshot
            .records
            .retain(|record| record.runtime.fences(&current));
        snapshot
            .generation
            .terminals
            .retain(|ownership| ownership.terminal.fences(&current.terminal));
        self.restore(serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap());
    }
}

#[test]
fn exact_resume_keeps_decision_owner_and_caller_after_source_and_dispatch_gc() {
    let mut conversation = Conversation::delegated();
    let owner = conversation.owner();
    assert_eq!(owner.run_id, conversation.operation());
    assert_eq!(owner.caller, conversation.caller);
    let old_credential = conversation.credential();
    conversation.finish();
    conversation.resume();
    assert_eq!(conversation.owner(), owner);
    assert_ne!(conversation.operation(), owner.run_id);
    assert!(
        conversation
            .agent
            .mcp_user_decision_owner(&old_credential)
            .is_err()
    );
    conversation.finish();

    // Independent runtime and dispatch retention can forget the original run.
    conversation.collect_ancestors();
    conversation.forget_dispatch_history(owner.run_id);
    assert!(
        conversation
            .agent
            .dispatch
            .binding(owner.run_id)
            .unwrap()
            .is_none()
    );
    conversation.resume();
    assert_eq!(conversation.owner(), owner);
    let delivered = conversation
        .agent
        .report_from_mcp(
            &conversation.credential(),
            Some(conversation.operation()),
            InboxKind::Completed,
            "resumed work completed".into(),
            None,
        )
        .unwrap();
    assert!(delivered.accepted);
    assert_eq!(delivered.delivered_to, conversation.caller);
    assert_eq!(
        delivered.committed.unwrap().run_id,
        conversation.operation()
    );
}

#[test]
fn fresh_conversation_on_the_same_agent_resets_decision_authority() {
    let mut conversation = Conversation::delegated();
    let previous = conversation.owner();
    let worker = conversation
        .agent
        .dispatch
        .binding(conversation.operation())
        .unwrap()
        .unwrap()
        .worker;
    conversation.finish();
    let operation = OperationId::new();
    conversation.current = conversation
        .agent
        .dispatch(
            &operation.to_string(),
            &DispatchIntent {
                workspace: previous.workspace_id,
                session_name: "worker".into(),
                caller: conversation.caller.clone(),
                agent: DispatchAgentIntent::Existing {
                    agent_id: worker.agent_id,
                },
                prompt: "a separate conversation".into(),
            },
            worker.session_id.unwrap(),
            &conversation.scope,
        )
        .unwrap();
    let next = conversation.owner();
    assert_eq!(next.run_id, operation);
    assert_ne!(next, previous);
    assert_eq!(
        conversation
            .agent
            .dispatch
            .binding(operation)
            .unwrap()
            .unwrap()
            .worker,
        worker
    );
    assert!(conversation.record().resumed_from.is_none());
}

#[test]
fn legacy_exact_lineage_recovers_original_decision_owner_and_dispatch_caller() {
    let mut conversation = Conversation::delegated();
    let original = conversation.owner();
    conversation.finish();
    conversation.resume();
    conversation.finish();
    let mut binding = conversation
        .agent
        .dispatch
        .binding(conversation.operation())
        .unwrap()
        .unwrap();
    // The previous implementation replaced a delegated caller with self.
    binding.caller = CallerRef {
        session_id: binding.worker.session_id,
        agent_id: binding.worker.agent_id,
    };
    let legacy_owner = UserDecisionOwner {
        workspace_id: original.workspace_id,
        session_id: binding.worker.session_id,
        caller: binding.caller.clone(),
        run_id: binding.run_id,
    };
    conversation.agent.dispatch.upsert_binding(binding).unwrap();
    let mut snapshot = conversation.agent.coordinator.snapshot();
    for record in &mut snapshot.records {
        record.user_decision_owner = None;
    }
    let wire = serde_json::to_string(&snapshot).unwrap();
    assert!(!wire.contains("user_decision_owner"));
    conversation.restore(serde_json::from_str(&wire).unwrap());
    conversation.resume();
    assert_eq!(conversation.owner(), legacy_owner);
    assert_eq!(
        conversation.owners(),
        vec![legacy_owner.clone(), original.clone()]
    );
    assert_eq!(
        conversation.record().user_decision_owner,
        Some(original.clone())
    );
    assert_eq!(
        conversation
            .agent
            .dispatch
            .binding(conversation.operation())
            .unwrap()
            .unwrap()
            .caller,
        conversation.caller
    );
    conversation.finish();
    conversation.collect_ancestors();
    conversation.forget_dispatch_history(original.run_id);
    conversation.forget_dispatch_history(legacy_owner.run_id);
    conversation.resume();
    assert_eq!(conversation.owners(), vec![legacy_owner, original]);
    assert_eq!(conversation.record().user_decision_owner_aliases.len(), 1);
    assert!(
        conversation
            .owners()
            .iter()
            .all(|owner| owner.run_id != conversation.operation())
    );
}

#[test]
fn legacy_missing_ancestor_binding_is_a_safe_authority_boundary() {
    let mut conversation = Conversation::delegated();
    let original = conversation.operation();
    conversation.finish();
    conversation.resume();
    conversation.finish();
    let operation = conversation.operation();
    let mut snapshot = conversation.agent.coordinator.snapshot();
    for record in &mut snapshot.records {
        record.user_decision_owner = None;
    }
    conversation.restore(snapshot);
    conversation.forget_dispatch_history(original);
    conversation.resume();
    assert_eq!(conversation.owner().run_id, operation);
    assert_ne!(conversation.owner().run_id, original);
    assert_eq!(conversation.owner().caller, conversation.caller);
}

#[test]
fn legacy_missing_ancestor_runtime_does_not_guess_an_older_owner() {
    let mut conversation = Conversation::delegated();
    let original = conversation.operation();
    conversation.finish();
    conversation.resume();
    let mut record = conversation.record();
    record.user_decision_owner = None;
    let recovered = conversation
        .agent
        .user_decision_authority_for_record(&record, &[])
        .unwrap();
    assert_eq!(recovered.owner.run_id, conversation.operation());
    assert_ne!(recovered.owner.run_id, original);
}

#[test]
fn decision_owner_rejects_unknown_stale_or_non_running_credentials() {
    let mut conversation = Conversation::delegated();
    assert!(
        conversation
            .agent
            .mcp_user_decision_owner("unknown")
            .is_err()
    );
    let credential = conversation.credential();
    let operation = conversation.operation();
    conversation
        .agent
        .dispatch
        .transition_run(operation, RunStatus::Completed, Some(Utc::now()))
        .unwrap();
    assert!(
        conversation
            .agent
            .mcp_user_decision_owner(&credential)
            .is_err()
    );
    conversation
        .agent
        .dispatch
        .transition_run(operation, RunStatus::Running, None)
        .unwrap();
    conversation
        .agent
        .coordinator
        .exit(
            &conversation.current.runtime,
            0,
            &mut *conversation.agent.store,
        )
        .unwrap();
    assert!(conversation.agent.mcp_callers.contains_key(&credential));
    assert!(
        conversation
            .agent
            .mcp_user_decision_owner(&credential)
            .is_err()
    );
}

#[test]
fn decision_owner_rejects_a_live_credential_registered_to_another_run() {
    let mut conversation = Conversation::delegated();
    let credential = conversation.credential();
    let operation = OperationId::new();
    let mut binding = conversation
        .agent
        .dispatch
        .binding(conversation.operation())
        .unwrap()
        .unwrap();
    binding.run_id = operation;
    conversation
        .agent
        .dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: binding.worker.agent_id,
            prompt: String::new(),
            started_at: Utc::now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    conversation.agent.dispatch.upsert_binding(binding).unwrap();
    conversation
        .agent
        .mcp_callers
        .get_mut(&credential)
        .unwrap()
        .operation = operation;
    assert!(
        conversation
            .agent
            .mcp_dispatch_context(&credential)
            .is_some()
    );
    assert!(
        conversation
            .agent
            .mcp_user_decision_owners(&credential)
            .is_err()
    );
}

#[test]
fn legacy_decision_authority_refuses_another_worker_in_the_same_scope() {
    let mut conversation = Conversation::delegated();
    let initial = conversation.operation();
    conversation.finish();
    conversation.resume();
    let mut current = conversation.record();
    current.user_decision_owner = None;
    let mut binding = conversation
        .agent
        .dispatch
        .binding(initial)
        .unwrap()
        .unwrap();
    let mut peer = conversation
        .agent
        .dispatch
        .agent(binding.worker.agent_id)
        .unwrap()
        .unwrap();
    peer.agent_id = AgentId::new();
    peer.current_run = None;
    conversation
        .agent
        .dispatch
        .upsert_agent(current.operation.workspace_id, peer.clone())
        .unwrap();
    binding.worker.agent_id = peer.agent_id;
    conversation.agent.dispatch.upsert_binding(binding).unwrap();
    assert!(
        conversation
            .agent
            .user_decision_authority_for_record(
                &current,
                &conversation.agent.coordinator.snapshot().records,
            )
            .is_err()
    );
}

#[test]
fn decision_owner_refuses_inconsistent_scope_caller_and_lineage() {
    let mut conversation = Conversation::delegated();
    let original = conversation.owner();
    let record = conversation.record();
    for owner in [
        UserDecisionOwner {
            workspace_id: WorkspaceId::new(),
            ..original.clone()
        },
        UserDecisionOwner {
            session_id: Some(SessionId::new()),
            ..original.clone()
        },
        UserDecisionOwner {
            caller: CallerRef {
                agent_id: AgentId::new(),
                ..original.caller.clone()
            },
            ..original.clone()
        },
        UserDecisionOwner {
            run_id: OperationId::new(),
            ..original.clone()
        },
    ] {
        let mut invalid = record.clone();
        invalid.user_decision_owner = Some(owner);
        assert!(
            conversation
                .agent
                .user_decision_authority_for_record(&invalid, &[])
                .is_err()
        );
    }
    conversation.finish();
    conversation.resume();
    let mut records = conversation.agent.coordinator.snapshot().records;
    for record in &mut records {
        record.user_decision_owner = None;
    }
    let current = records
        .iter()
        .find(|record| record.runtime.fences(&conversation.current.runtime))
        .unwrap()
        .clone();
    let source = records
        .iter()
        .find(|record| record.resume_source == current.resumed_from)
        .unwrap()
        .clone();
    let mut wrong_continuation = source.clone();
    wrong_continuation.continuation = Some(AgentContinuationRef::new());
    let mut missing_backref = source.clone();
    missing_backref.superseded_by = None;
    let mut wrong_scope = source;
    wrong_scope.launch.request.scope.worktree_id = WorktreeId::new();
    for invalid in [wrong_continuation, missing_backref, wrong_scope] {
        assert!(
            conversation
                .agent
                .user_decision_authority_for_record(&current, &[invalid])
                .is_err()
        );
    }
    let mut cycle = current.clone();
    cycle.resumed_from = cycle.resume_source;
    cycle.superseded_by = Some(cycle.runtime.agent_runtime_id);
    assert!(
        conversation
            .agent
            .user_decision_authority_for_record(&cycle, std::slice::from_ref(&cycle))
            .is_err()
    );
}

#[test]
fn failed_resume_keeps_owner_without_authorizing_a_failed_runtime() {
    let mut conversation = Conversation::delegated();
    let original = conversation.owner();
    conversation.finish();
    let target = conversation
        .agent
        .inventory(original.workspace_id)
        .resumable[0]
        .target
        .clone()
        .unwrap();
    pty_mut(&mut conversation.agent).spawn = Some(SpawnFailure::Definite);
    let failed = OperationId::new();
    assert!(
        conversation
            .agent
            .resume_exact(&failed.to_string(), &target, &conversation.scope)
            .is_err()
    );
    assert!(conversation.agent.mcp_callers.is_empty());
    assert_eq!(conversation.record().user_decision_owner, Some(original));
    assert!(
        conversation
            .agent
            .dispatch
            .run(failed)
            .unwrap()
            .is_some_and(|run| run.status == RunStatus::Failed)
    );
}

#[test]
fn hydrate_rejects_a_decision_anchor_in_another_scope_or_fresh_conversation() {
    let conversation = Conversation::delegated();
    let snapshot = conversation.agent.coordinator.snapshot();
    for owner in [
        UserDecisionOwner {
            workspace_id: WorkspaceId::new(),
            ..conversation.owner()
        },
        UserDecisionOwner {
            run_id: OperationId::new(),
            ..conversation.owner()
        },
    ] {
        let mut snapshot = snapshot.clone();
        snapshot.records[0].user_decision_owner = Some(owner);
        assert_eq!(
            RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap_err(),
            RuntimeSnapshotError::ScopeMismatch
        );
    }
}

#[test]
fn hydrate_refuses_a_replacement_that_changed_its_decision_authority() {
    let mut conversation = Conversation::delegated();
    let owner = conversation.owner();
    conversation.finish();
    conversation.resume();
    let snapshot = conversation.agent.coordinator.snapshot();
    let current = snapshot
        .records
        .iter()
        .position(|record| record.runtime == conversation.current.runtime)
        .unwrap();
    for changed_aliases in [false, true] {
        let mut invalid = snapshot.clone();
        let changed_owner = UserDecisionOwner {
            run_id: OperationId::new(),
            ..owner.clone()
        };
        if changed_aliases {
            invalid.records[current].user_decision_owner_aliases = vec![changed_owner];
        } else {
            invalid.records[current].user_decision_owner = Some(changed_owner);
        }
        assert_eq!(
            RuntimeCoordinator::hydrate(invalid, 16, 64 * 1024, 64).unwrap_err(),
            RuntimeSnapshotError::ResumeRelation
        );
    }
}
