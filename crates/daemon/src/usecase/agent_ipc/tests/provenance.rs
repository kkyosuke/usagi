use super::*;
use usagi_core::domain::agent::{
    AgentClientSurface, AgentLaunchClient, AgentLaunchEntry, AgentLaunchSource,
};

#[test]
fn manual_launches_record_distinct_origins_and_replay_the_same_audit() {
    let mut agent = runtime();
    let intent = intent(None);
    let scope = FakeScope(Ok(scope()));
    let first = OperationId::new();
    let before = Utc::now();
    agent.launch(&first.to_string(), &intent, &scope).unwrap();
    let second = OperationId::new();
    agent.launch(&second.to_string(), &intent, &scope).unwrap();
    let inventory = agent.inventory(intent.workspace);
    assert_eq!(inventory.runtimes.len(), 2);
    for (item, operation) in inventory.runtimes.iter().zip([first, second]) {
        let provenance = item.launch_provenance.as_ref().unwrap();
        assert_eq!(provenance.created.as_ref(), Some(&provenance.launched));
        assert_eq!(provenance.launched.source, AgentLaunchSource::Manual);
        assert_eq!(provenance.launched.entrypoint, AgentLaunchEntry::Agent);
        assert_eq!(provenance.launched.operation_id, operation);
        assert_eq!(provenance.launched.caller, None);
        assert_eq!(
            provenance.agent_id,
            Some(
                agent
                    .dispatch
                    .binding(operation)
                    .unwrap()
                    .unwrap()
                    .worker
                    .agent_id
            )
        );
        assert!(provenance.launched.at >= before);
    }
    agent.launch(&first.to_string(), &intent, &scope).unwrap();
    assert_eq!(agent.inventory(intent.workspace), inventory);
    let origins = agent.agent_launch_provenance(intent.workspace).unwrap();
    assert_eq!(origins.len(), 2);
    assert!(origins.values().all(Option::is_some));
    assert!(
        agent
            .agent_launch_provenance(WorkspaceId::new())
            .unwrap()
            .values()
            .all(Option::is_none)
    );
    let snapshot = agent.coordinator.snapshot();
    let encoded = serde_json::to_string(&snapshot).unwrap();
    let restored: RuntimeStoreSnapshot = serde_json::from_str(&encoded).unwrap();
    let (restored, _) = restored.reconcile_after_daemon_restart();
    assert_eq!(
        restored.records[0].launch_provenance,
        snapshot.records[0].launch_provenance
    );
}

#[test]
fn mcp_launches_retain_the_authenticated_caller_and_actual_entrypoint() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let resolved = configured_scope(worktree.path());
    let scope = FakeScope(Ok(resolved));
    let mut agent = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let parent = OperationId::new();
    agent.launch(&parent.to_string(), &intent, &scope).unwrap();
    let caller = agent.dispatch.binding(parent).unwrap().unwrap().worker;
    let caller = CallerRef {
        session_id: caller.session_id,
        agent_id: caller.agent_id,
    };
    for entrypoint in [
        AgentLaunchEntry::SessionDispatch,
        AgentLaunchEntry::AgentHandoff,
        AgentLaunchEntry::SessionDelegateBrief,
    ] {
        let operation = OperationId::new();
        let selected = DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("test").unwrap(),
        };
        let dispatch = DispatchIntent {
            workspace: intent.workspace,
            session_name: "worker".into(),
            caller: caller.clone(),
            agent: selected,
            prompt: "Review the task".into(),
        };
        let planned = (entrypoint == AgentLaunchEntry::AgentHandoff)
            .then(|| {
                agent.plan_peer_worker(
                    &operation.to_string(),
                    intent.workspace,
                    &caller,
                    &dispatch.agent,
                )
            })
            .transpose()
            .unwrap();
        let preflight = agent
            .prepare_dispatch_readiness(&operation.to_string(), &dispatch)
            .unwrap();
        let admitted = agent
            .dispatch_from_after_readiness(
                &operation.to_string(),
                &dispatch,
                intent.session.unwrap(),
                &scope,
                preflight.as_ref(),
                planned.as_ref(),
                AgentLaunchContext {
                    caller_operation_id: Some(parent),
                    ..AgentLaunchContext::new(AgentLaunchSource::Mcp, entrypoint)
                },
            )
            .unwrap();
        let inventory = agent.inventory(intent.workspace);
        let provenance = inventory
            .runtimes
            .iter()
            .find(|item| item.runtime.terminal == admitted.terminal)
            .unwrap()
            .launch_provenance
            .as_ref()
            .unwrap();
        assert_eq!(provenance.launched.source, AgentLaunchSource::Mcp);
        assert_eq!(
            provenance.created.as_ref().unwrap().source,
            AgentLaunchSource::Mcp
        );
        assert_eq!(provenance.launched.entrypoint, entrypoint);
        assert_eq!(provenance.launched.caller, Some(caller.clone()));
        assert_eq!(provenance.launched.caller_operation_id, Some(parent));
        let worker = agent
            .dispatch
            .binding(operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id;
        assert_eq!(provenance.agent_id, Some(worker));
        assert_eq!(
            agent.agent_launch_provenance(intent.workspace).unwrap()[&worker].as_ref(),
            Some(provenance)
        );
        agent.exit(&admitted.terminal, 0).unwrap();
    }
}

#[test]
fn resume_records_the_new_initiator_and_preserves_unknown_legacy_creation() {
    for legacy in [false, true] {
        let mut agent = codex_runtime();
        let intent = intent(Some("codex"));
        let scope = FakeScope(Ok(scope()));
        let created = OperationId::new();
        let admission = agent.launch(&created.to_string(), &intent, &scope).unwrap();
        let credential = agent.mcp_callers.keys().next().unwrap().clone();
        agent
            .capture_codex_session(
                &credential,
                ProviderSessionId::new("private-conversation").unwrap(),
            )
            .unwrap();
        agent.exit(&admission.terminal, 0).unwrap();
        let original = agent.inventory(intent.workspace).runtimes[0]
            .launch_provenance
            .clone()
            .unwrap()
            .created;
        if legacy {
            let mut snapshot = agent.coordinator.snapshot();
            snapshot.records[0].launch_provenance = None;
            agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
        }
        let target = agent.inventory(intent.workspace).resumable[0]
            .target
            .clone()
            .unwrap();
        let operation = OperationId::new();
        let caller = CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        };
        let resumed = agent
            .resume_exact_from(
                &operation.to_string(),
                &target,
                &scope,
                AgentLaunchContext {
                    caller: Some(caller.clone()),
                    caller_operation_id: Some(created),
                    ..AgentLaunchContext::new(
                        AgentLaunchSource::Mcp,
                        AgentLaunchEntry::SessionResume,
                    )
                },
            )
            .unwrap();
        let inventory = agent.inventory(intent.workspace);
        let provenance = inventory
            .runtimes
            .iter()
            .find(|item| item.runtime.terminal == resumed.terminal)
            .unwrap()
            .launch_provenance
            .as_ref()
            .unwrap();
        assert_eq!(provenance.created, if legacy { None } else { original });
        assert_eq!(provenance.launched.source, AgentLaunchSource::Mcp);
        assert_eq!(provenance.launched.caller, Some(caller));
        assert_eq!(provenance.launched.operation_id, operation);
        assert!(
            !serde_json::to_string(&inventory)
                .unwrap()
                .contains("private-conversation")
        );
    }
}

#[test]
fn redispatch_and_manual_reuse_keep_the_agent_creator_even_with_an_older_operation_id() {
    let mut agent = runtime();
    let intent = intent(None);
    let scope = FakeScope(Ok(scope()));
    let older_dispatch = OperationId::new();
    let created = OperationId::new();
    let admitted = agent.launch(&created.to_string(), &intent, &scope).unwrap();
    let worker = agent
        .dispatch
        .binding(created)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    let original = agent.inventory(intent.workspace).runtimes[0]
        .launch_provenance
        .clone()
        .unwrap()
        .created;
    agent.exit(&admitted.terminal, 0).unwrap();
    let dispatched = agent
        .dispatch(
            &older_dispatch.to_string(),
            &DispatchIntent {
                workspace: intent.workspace,
                session_name: "worker".into(),
                caller: CallerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
                agent: DispatchAgentIntent::Existing { agent_id: worker },
                prompt: "Another task".into(),
            },
            intent.session.unwrap(),
            &scope,
        )
        .unwrap();
    let provenance = agent.agent_launch_provenance(intent.workspace).unwrap()[&worker]
        .clone()
        .unwrap();
    assert_eq!(provenance.created, original);
    assert_eq!(provenance.launched.source, AgentLaunchSource::Mcp);
    assert_eq!(provenance.launched.operation_id, older_dispatch);
    agent.exit(&dispatched.terminal, 0).unwrap();
    let reused = OperationId::new();
    agent.launch(&reused.to_string(), &intent, &scope).unwrap();
    assert_eq!(
        agent
            .dispatch
            .binding(reused)
            .unwrap()
            .unwrap()
            .worker
            .agent_id,
        worker
    );
    let provenance = agent.agent_launch_provenance(intent.workspace).unwrap()[&worker]
        .clone()
        .unwrap();
    assert_eq!(provenance.created, original);
    assert_eq!(provenance.launched.source, AgentLaunchSource::Manual);
}

#[test]
fn legacy_wire_record_remains_unknown_after_a_fresh_manual_conversation() {
    let mut agent = runtime();
    let intent = intent(None);
    let scope = FakeScope(Ok(scope()));
    let first = OperationId::new();
    let admitted = agent.launch(&first.to_string(), &intent, &scope).unwrap();
    agent.exit(&admitted.terminal, 0).unwrap();
    let mut encoded = serde_json::to_value(agent.coordinator.snapshot()).unwrap();
    encoded["records"][0]
        .as_object_mut()
        .unwrap()
        .remove("launch_provenance");
    let restored: RuntimeStoreSnapshot = serde_json::from_value(encoded).unwrap();
    assert_eq!(restored.records[0].launch_provenance, None);
    agent.coordinator = RuntimeCoordinator::hydrate(restored, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        agent.inventory(intent.workspace).runtimes[0].launch_provenance,
        None
    );
    let second = OperationId::new();
    let worker = agent
        .dispatch
        .binding(first)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    agent.launch(&second.to_string(), &intent, &scope).unwrap();
    let provenance = agent.agent_launch_provenance(intent.workspace).unwrap()[&worker]
        .clone()
        .unwrap();
    assert_eq!(provenance.created, None);
    assert_eq!(provenance.launched.source, AgentLaunchSource::Manual);
}

#[test]
fn collected_dispatch_history_keeps_a_reused_agent_creator_unknown() {
    for via_mcp in [false, true] {
        let mut agent = runtime();
        let intent = intent(None);
        let scope = FakeScope(Ok(scope()));
        let first = OperationId::new();
        let admitted = agent.launch(&first.to_string(), &intent, &scope).unwrap();
        let worker = agent
            .dispatch
            .binding(first)
            .unwrap()
            .unwrap()
            .worker
            .agent_id;
        agent.exit(&admitted.terminal, 0).unwrap();

        // Age-based retention removes the old run and its admission/binding,
        // while the dispatchable Agent identity deliberately survives.
        let mut old = agent.dispatch.run(first).unwrap().unwrap();
        old.ended_at = Some(Utc::now() - chrono::Duration::days(366));
        agent.dispatch.upsert_run(old).unwrap();
        for _ in 0..32 {
            agent
                .dispatch
                .upsert_run(DispatchRun {
                    run_id: OperationId::new(),
                    agent_id: AgentId::new(),
                    prompt: String::new(),
                    started_at: Utc::now(),
                    ended_at: Some(Utc::now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }
        assert_eq!(agent.dispatch.run(first).unwrap(), None);
        assert!(agent.dispatch.agent(worker).unwrap().is_some());
        let next = OperationId::new();
        let admitted = if via_mcp {
            agent
                .dispatch(
                    &next.to_string(),
                    &DispatchIntent {
                        workspace: intent.workspace,
                        session_name: "worker".into(),
                        caller: CallerRef {
                            session_id: None,
                            agent_id: AgentId::new(),
                        },
                        agent: DispatchAgentIntent::Existing { agent_id: worker },
                        prompt: "Another task".into(),
                    },
                    intent.session.unwrap(),
                    &scope,
                )
                .unwrap()
        } else {
            agent.launch(&next.to_string(), &intent, &scope).unwrap()
        };
        assert_eq!(
            agent
                .dispatch
                .binding(next)
                .unwrap()
                .unwrap()
                .worker
                .agent_id,
            worker
        );
        let inventory = agent.inventory(intent.workspace);
        let provenance = inventory
            .runtimes
            .iter()
            .find(|item| item.runtime.terminal == admitted.terminal)
            .unwrap()
            .launch_provenance
            .as_ref()
            .unwrap();
        assert_eq!(provenance.created, None);
        assert_eq!(
            provenance.launched.source,
            if via_mcp {
                AgentLaunchSource::Mcp
            } else {
                AgentLaunchSource::Manual
            }
        );
    }
}

#[test]
fn existing_legacy_idle_agent_without_runs_keeps_creation_unknown() {
    let mut agent = runtime();
    let intent = intent(None);
    let worker = agent
        .dispatch
        .upsert_agent_by_runtime_model(
            intent.workspace,
            intent.session,
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("default").unwrap(),
        )
        .unwrap();
    assert!(agent.dispatch.runs().unwrap().is_empty());
    let operation = OperationId::new();
    agent
        .launch(&operation.to_string(), &intent, &FakeScope(Ok(scope())))
        .unwrap();
    let provenance = agent.agent_launch_provenance(intent.workspace).unwrap()[&worker.agent_id]
        .clone()
        .unwrap();
    assert_eq!(provenance.created, None);
    assert_eq!(provenance.launched.source, AgentLaunchSource::Manual);
}

#[test]
fn daemon_repair_resume_records_restart_without_changing_the_manual_creator() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut agent = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let scope = FakeScope(Ok(scope()));
    let operation = OperationId::new();
    let admitted = agent
        .launch(&operation.to_string(), &intent, &scope)
        .unwrap();
    let original = agent.inventory(intent.workspace).runtimes[0]
        .launch_provenance
        .clone()
        .unwrap()
        .created;
    agent.exit(&admitted.terminal, 0).unwrap();
    let target = agent.inventory(intent.workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let operation = OperationId::new().to_string();
    let expected_revision = agent
        .registry
        .profile(&AgentProfileId::new("claude").unwrap())
        .unwrap()
        .revision;
    let preflight = agent
        .prepare_current_integration_resume_readiness(&operation, &target, expected_revision)
        .unwrap();
    let admitted = agent
        .resume_with_current_integration_from_after_readiness(
            &operation,
            &target,
            expected_revision,
            &scope,
            preflight.as_ref(),
            AgentLaunchContext::new(AgentLaunchSource::Daemon, AgentLaunchEntry::DaemonRestart),
        )
        .unwrap();
    let inventory = agent.inventory(intent.workspace);
    let provenance = inventory
        .runtimes
        .iter()
        .find(|item| item.runtime.terminal == admitted.terminal)
        .unwrap()
        .launch_provenance
        .as_ref()
        .unwrap();
    assert_eq!(provenance.created, original);
    assert_eq!(provenance.launched.source, AgentLaunchSource::Daemon);
    assert_eq!(
        provenance.launched.entrypoint,
        AgentLaunchEntry::DaemonRestart
    );
    agent.exit(&admitted.terminal, 0).unwrap();
    let target = agent.inventory(intent.workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let operation = OperationId::new().to_string();
    let preflight = agent.prepare_resume_readiness(&operation, &target).unwrap();
    let admitted = agent
        .resume_from_after_readiness(
            &operation,
            &target,
            &scope,
            preflight.as_ref(),
            AgentLaunchContext::new(AgentLaunchSource::Daemon, AgentLaunchEntry::DaemonRestart),
        )
        .unwrap();
    let inventory = agent.inventory(intent.workspace);
    let provenance = inventory
        .runtimes
        .iter()
        .find(|item| item.runtime.terminal == admitted.terminal)
        .unwrap()
        .launch_provenance
        .as_ref()
        .unwrap();
    assert_eq!(
        provenance.created.as_ref().unwrap().source,
        AgentLaunchSource::Manual
    );
    assert_eq!(provenance.launched.source, AgentLaunchSource::Daemon);
}

#[test]
fn human_goal_launch_records_the_manual_goal_entrypoint() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut agent = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let operation = OperationId::new().to_string();
    agent
        .launch_goal(
            &operation,
            &AgentGoalIntent {
                workspace,
                profile: None,
                goal: "Update the docs".into(),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let inventory = agent.inventory(workspace);
    let provenance = inventory.runtimes[0].launch_provenance.as_ref().unwrap();
    assert_eq!(provenance.launched.source, AgentLaunchSource::Manual);
    assert_eq!(provenance.launched.entrypoint, AgentLaunchEntry::AgentGoal);
}

fn audit_client(surface: AgentClientSurface) -> AgentLaunchClient {
    AgentLaunchClient {
        surface: Some(surface),
        client_id: "public-client-id".into(),
        connection_id: "public-connection-id".into(),
        request_id: "public-request-id".into(),
        peer_pid: 1234,
    }
}

#[test]
fn manual_connection_evidence_is_durable_and_a_retry_cannot_rewrite_its_creator() {
    let mut agent = runtime();
    let intent = intent(None);
    let scope = FakeScope(Ok(scope()));
    for surface in [
        AgentClientSurface::Tui,
        AgentClientSurface::Cli,
        AgentClientSurface::Mcp,
    ] {
        let operation = OperationId::new();
        let client = audit_client(surface);
        let context = AgentLaunchContext {
            client: Some(client.clone()),
            ..AgentLaunchContext::new(AgentLaunchSource::Manual, AgentLaunchEntry::Agent)
        };
        let ticket = agent
            .prepare_launch_readiness(&operation.to_string(), &intent)
            .unwrap();
        let admission = agent
            .launch_from_after_readiness(
                &operation.to_string(),
                &intent,
                &scope,
                ticket.as_ref(),
                context,
            )
            .unwrap();
        let audit = agent
            .inventory(intent.workspace)
            .runtimes
            .into_iter()
            .find(|item| item.runtime.terminal == admission.terminal)
            .unwrap()
            .launch_provenance
            .unwrap();
        assert_eq!(audit.launched.source, AgentLaunchSource::Manual);
        assert_eq!(audit.launched.client, Some(client));
        assert_eq!(audit.created.as_ref(), Some(&audit.launched));
        assert_eq!(
            audit.agent_id,
            Some(
                agent
                    .dispatch
                    .binding(operation)
                    .unwrap()
                    .unwrap()
                    .worker
                    .agent_id
            )
        );
        let snapshot = agent.coordinator.snapshot();
        let restored: RuntimeStoreSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(snapshot, restored);
        agent
            .launch_from_after_readiness(
                &operation.to_string(),
                &intent,
                &scope,
                None,
                AgentLaunchContext {
                    client: Some(audit_client(AgentClientSurface::Cli)),
                    ..AgentLaunchContext::new(AgentLaunchSource::Manual, AgentLaunchEntry::Agent)
                },
            )
            .unwrap();
        assert_eq!(agent.coordinator.snapshot(), snapshot);
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One lifetime verifies Workflow -> MCP -> exact-resume audit continuity.
fn workflow_and_its_mcp_workers_retain_one_workflow_identity_across_resume() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let scope = FakeScope(Ok(configured_scope(worktree.path())));
    let mut agent = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let workflow = OperationId::new();
    let prompt = "Implement and request a peer review";
    let ticket = agent
        .prepare_workflow_readiness(&workflow.to_string(), &intent, prompt)
        .unwrap();
    agent
        .launch_workflow_from_after_readiness(
            &workflow.to_string(),
            &intent,
            prompt,
            &scope,
            ticket.as_ref(),
            AgentLaunchContext {
                client: Some(audit_client(AgentClientSurface::Tui)),
                ..AgentLaunchContext::new(
                    AgentLaunchSource::Workflow,
                    AgentLaunchEntry::WorkflowStart,
                )
            },
        )
        .unwrap();
    let parent = agent.dispatch.binding(workflow).unwrap().unwrap().worker;
    let parent_caller = CallerRef {
        agent_id: parent.agent_id,
        session_id: parent.session_id,
    };
    let dispatch = DispatchIntent {
        workspace: intent.workspace,
        session_name: "worker".into(),
        caller: parent_caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("test").unwrap(),
        },
        prompt: "Review the task".into(),
    };
    let operation = OperationId::new();
    let ticket = agent
        .prepare_dispatch_readiness(&operation.to_string(), &dispatch)
        .unwrap();
    let worker = agent
        .dispatch_from_after_readiness(
            &operation.to_string(),
            &dispatch,
            intent.session.unwrap(),
            &scope,
            ticket.as_ref(),
            None,
            AgentLaunchContext {
                client: Some(audit_client(AgentClientSurface::Mcp)),
                caller_operation_id: Some(workflow),
                ..AgentLaunchContext::new(AgentLaunchSource::Mcp, AgentLaunchEntry::AgentHandoff)
            },
        )
        .unwrap();
    let inventory = agent.inventory(intent.workspace);
    let parent_audit = inventory
        .runtimes
        .iter()
        .find(|item| item.launch_provenance.as_ref().unwrap().agent_id == Some(parent.agent_id))
        .unwrap()
        .launch_provenance
        .as_ref()
        .unwrap();
    assert_eq!(parent_audit.launched.workflow_id, Some(workflow));
    let worker_audit = inventory
        .runtimes
        .iter()
        .find(|item| item.runtime.terminal == worker.terminal)
        .unwrap()
        .launch_provenance
        .as_ref()
        .unwrap();
    assert_eq!(worker_audit.launched.workflow_id, Some(workflow));
    assert_eq!(worker_audit.launched.caller, Some(parent_caller));
    assert_eq!(worker_audit.launched.caller_operation_id, Some(workflow));
    assert_eq!(worker_audit.launched.source, AgentLaunchSource::Mcp);
    let credential = agent
        .mcp_callers
        .iter()
        .find(|(_, caller)| caller.operation == operation)
        .unwrap()
        .0
        .clone();
    agent
        .report_agent_phase_with_session(
            &credential,
            AgentPhase::Ready,
            Some(ProviderSessionId::new("private-workflow-conversation").unwrap()),
        )
        .unwrap();
    agent.exit(&worker.terminal, 0).unwrap();
    let target = agent
        .inventory(intent.workspace)
        .resumable
        .iter()
        .find(|item| item.runtime_id == worker.runtime.agent_runtime_id)
        .unwrap()
        .target
        .clone()
        .unwrap();
    let resumed = agent
        .resume_exact(&OperationId::new().to_string(), &target, &scope)
        .unwrap();
    let resumed_audit = agent
        .inventory(intent.workspace)
        .runtimes
        .into_iter()
        .find(|item| item.runtime.terminal == resumed.terminal)
        .unwrap()
        .launch_provenance
        .unwrap();
    assert_eq!(resumed_audit.agent_id, worker_audit.agent_id);
    assert_eq!(resumed_audit.created, worker_audit.created);
    assert_eq!(resumed_audit.launched.source, AgentLaunchSource::Manual);
    assert_eq!(resumed_audit.launched.workflow_id, Some(workflow));
}
