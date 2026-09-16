//! restore の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn blocked_restore_inventory_never_blocks_render_or_quit() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut term = QuitWhileRestoreBlockedTerminal {
        entered: Some(entered_rx),
        keys: VecDeque::from([Key::CtrlQ, Key::Char('y')]),
        frames: Vec::new(),
    };
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: Some(Box::new(BlockingRestorePort {
            entered: entered_tx,
            release: release_rx,
        })),
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    let started = std::time::Instant::now();
    let result =
        run_workspace_controller_with_backend(&mut term, snapshot("blocked-restore"), &mut factory);
    let elapsed = started.elapsed();
    let _ = release_tx.send(());

    assert_eq!(result.unwrap(), Exit::Quit);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "quit waited for a blocked restore worker: {elapsed:?}"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("blocked-restore"))
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| { frame.join("\n").contains("Leave this workspace?") })
    );
}

/// #554 acceptance. The skip covers the drawing and nothing else: the
/// restore retry's admission sits after the gate and must still fire on a
/// tick that drew nothing. The loop is driven until a retry is admitted on
/// such a tick, so the assertion never rests on a run where every tick
/// happened to be material (#567).
#[test]
fn a_skipped_tick_still_admits_the_restore_retry() {
    wait_for_a_stable_relative_time_minute();
    let log = Arc::new(RestoreAdmissionLog::default());
    let mut term = RetryDrivingTerminal {
        log: Arc::clone(&log),
        pace: std::time::Duration::from_millis(60),
        ticks: 0,
        calls: 0,
        quiet_ticks: 0,
        quit: VecDeque::new(),
    };
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: Some(Box::new(AdmissionCountingRestorePort {
            log: Arc::clone(&log),
        })),
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("retry"), &mut factory).unwrap(),
        Exit::Quit
    );

    let admissions = log.admitted_jobs();
    let driven = term.ticks;
    let drawn_at: Vec<usize> = admissions.iter().map(|admission| admission.drawn).collect();
    let admitted = drawn_at.len();
    assert!(
        admitted >= 2,
        "the restore retry was admitted {admitted} time(s) in {driven} tick(s)"
    );
    // A retry admitted on a skipped tick is the whole contract, so a run that
    // never skipped one fails here instead of reading as a pass.
    let retry_on_a_skipped_tick = admissions.iter().skip(1).any(|admission| admission.skipped);
    assert!(
        retry_on_a_skipped_tick,
        "every restore retry followed a redraw in {driven} tick(s), \
             admitted at frame {drawn_at:?}"
    );
}

#[test]
fn restore_without_agent_intent_caches_inventory_and_refresh_clears_it() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let fence = runtime.restore_fence();
    let applied = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::new(),
            terminals: Ok(Vec::new()),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::new(),
    );
    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert_eq!(
        ui.agent_inventory().map(|inventory| inventory.workspace_id),
        Some(workspace)
    );
    ui.refresh_agent_inventory();
    assert!(ui.agent_inventory().is_none());
    assert!(ui.take_agent_observation_request());
    assert!(runtime.active_pane().tabs().is_empty());
}

#[test]
fn restore_worker_retries_both_inventories_without_launching() {
    let workspace = WorkspaceId::new();
    let terminal_attempts = Arc::new(AtomicUsize::new(0));
    let agent_attempts = Arc::new(AtomicUsize::new(0));
    let terminal = scoped_terminal_ref(workspace, None);
    let (sender, receiver) = std::sync::mpsc::channel();

    crate::presentation::spawn_restore_job(
        Box::new(RetryRestorePort {
            workspace,
            entries: vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }],
            runtimes: Vec::new(),
            fail_attempts: 2,
            terminal_attempts: Arc::clone(&terminal_attempts),
            agent_attempts: Arc::clone(&agent_attempts),
        }),
        workspace,
        BTreeSet::new(),
        7,
        11,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("bounded restore retry completes");
    assert_eq!(completion.dispatched_interaction, 7);
    assert_eq!(completion.dispatched_registry_revision, 11);
    assert_eq!(completion.terminals.unwrap()[0].terminal, terminal);
    assert_eq!(completion.agents.unwrap().workspace_id, workspace);
    assert!(completion.observation_coherent);
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 6);
    assert_eq!(agent_attempts.load(Ordering::SeqCst), 3);
}

#[test]
fn restore_worker_retries_a_cross_rpc_snapshot_race_until_refs_are_coherent() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, None);
    let replacement = scoped_terminal_ref(workspace, None);
    let entry = |terminal: &TerminalRef| TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_restore_job(
        Box::new(SequencedRestorePort {
            // First terminal/Agent/terminal bracket races O -> R. The
            // second bracket is stable at R and is the only accepted one.
            terminals: VecDeque::from([
                Ok(vec![entry(&old)]),
                Ok(vec![entry(&replacement)]),
                Ok(vec![entry(&replacement)]),
                Ok(vec![entry(&replacement)]),
            ]),
            agents: VecDeque::from([Ok(inventory(&old)), Ok(inventory(&replacement))]),
        }),
        workspace,
        BTreeSet::new(),
        0,
        0,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(completion.observation_coherent);
    assert_eq!(
        completion.terminals.as_ref().unwrap()[0].terminal,
        replacement
    );
    assert!(
        completion.agents.as_ref().unwrap().runtimes[0]
            .runtime
            .terminal
            .fences(&replacement)
    );
}

#[test]
fn restore_worker_rejects_an_agent_inventory_from_another_workspace() {
    let workspace = WorkspaceId::new();
    let wrong_inventory = AgentInventory {
        workspace_id: WorkspaceId::new(),
        runtimes: Vec::new(),
        resumable: Vec::new(),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_restore_job(
        Box::new(SequencedRestorePort {
            terminals: VecDeque::from([
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
            ]),
            agents: VecDeque::from([
                Ok(wrong_inventory.clone()),
                Ok(wrong_inventory.clone()),
                Ok(wrong_inventory),
            ]),
        }),
        workspace,
        BTreeSet::new(),
        0,
        0,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(!completion.observation_coherent);
    assert_eq!(
        completion.agents.unwrap_err(),
        "Agent inventory scope changed while restoring"
    );
}

#[test]
fn partial_transport_failure_restores_nothing_and_outranks_a_stale_fence() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generic = scoped_terminal_ref(workspace, Some(session));
    let mut initial_intent = AgentTabIntent::empty(workspace);
    initial_intent.revision = 3;
    let durable = Arc::new(Mutex::new(initial_intent));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let dispatched = runtime.restore_fence();
    let runtime_before = runtime.active_pane().clone();
    let partial = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Err("Agent inventory unavailable".to_owned()),
            observation_coherent: false,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        partial.outcome,
        crate::presentation::RestoreJobOutcome::TransportFailed
    );
    assert_eq!(runtime.active_pane(), &runtime_before);
    assert_ne!(runtime.focused_terminal(), Some(generic.clone()));
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );

    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(retry.complete(std::time::Duration::ZERO, partial.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_millis(249)));
    assert!(retry.begin_if_due(std::time::Duration::from_millis(250)));

    // User activity advances the runtime fence while the next partial
    // request is in flight. Transport failure still wins and advances the
    // outage backoff instead of immediately redispatching.
    let _ = runtime.handle_key(Key::Down);
    let both_failed = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: partial.port,
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic,
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Err("Agent inventory unavailable".to_owned()),
            observation_coherent: false,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        both_failed.outcome,
        crate::presentation::RestoreJobOutcome::TransportFailed
    );
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    assert!(!retry.complete(std::time::Duration::from_millis(250), both_failed.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_millis(749)));
    assert!(retry.begin_if_due(std::time::Duration::from_millis(750)));
}

#[test]
fn reconnect_racing_an_in_flight_restore_schedules_one_fresh_observation() {
    let now = std::time::Duration::from_secs(7);
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    retry.reconnected(1, now);
    assert_eq!(
        retry.followup,
        crate::presentation::RestoreFollowup::Reconnected
    );
    assert!(!retry.complete(now, crate::presentation::RestoreJobOutcome::Applied));
    assert_eq!(retry.followup, crate::presentation::RestoreFollowup::None);
    assert_eq!(retry.failures, 0);
    assert_eq!(retry.next_retry_at, Some(now));
    assert!(retry.begin_if_due(now));
    assert!(!retry.begin_if_due(now));
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers scope fencing and duplicate normalization.
fn restore_scope_change_rejects_snapshot_and_exact_duplicates_normalize_once() {
    let workspace = WorkspaceId::new();
    let original_session = SessionId::new();
    let added_session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(original_session));
    let entry = TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Terminal,
        live: true,
    };
    let same_terminal_agent = TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let mut duplicated = vec![entry.clone(), same_terminal_agent.clone(), entry.clone()];
    crate::presentation::normalize_terminal_inventory(&mut duplicated);
    assert_eq!(duplicated, vec![same_terminal_agent, entry.clone()]);
    let generic_only = vec![entry.clone()];
    assert!(crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));
    assert!(!crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: WorkspaceId::new(),
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let out_of_scope_terminal = scoped_terminal_ref(workspace, Some(added_session));
    assert!(!crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &[TerminalInventoryEntry {
            terminal: out_of_scope_terminal,
            kind: TerminalKind::Terminal,
            live: true,
        }],
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let mut conflicting = generic_only.clone();
    conflicting.push(TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    });
    assert!(!crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &conflicting,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let foreign = scoped_terminal_ref(workspace, Some(added_session));
    let continuation = AgentContinuationRef::new();
    let foreign_runtime = AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), foreign, Some(added_session)).unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    assert!(!crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: vec![foreign_runtime],
            resumable: Vec::new(),
        },
    ));

    let agent_terminal = scoped_terminal_ref(workspace, Some(original_session));
    let agent_entry = TerminalInventoryEntry {
        terminal: agent_terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let duplicate_runtime = || AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            agent_terminal.clone(),
            Some(original_session),
        )
        .unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    assert!(!crate::presentation::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &[agent_entry],
        &AgentInventory {
            workspace_id: workspace,
            runtimes: vec![duplicate_runtime(), duplicate_runtime()],
            resumable: Vec::new(),
        },
    ));

    let mut view_state = state("demo");
    let mut added_record = view_state.sessions[0].clone();
    added_record.name = "added-session".to_owned();
    view_state.sessions.push(added_record);
    let view = WorkspaceView::with_runtime_ids(
        ws("demo"),
        view_state,
        vec![original_session, added_session],
    );
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![original_session, added_session]);
    let fence = runtime.restore_fence();
    let applied = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([original_session]),
            terminals: Ok(vec![entry]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([original_session, added_session]),
    );
    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::FenceRejected
    );
    assert!(
        runtime
            .panes()
            .pane(Target::Session(original_session))
            .is_some_and(|pane| pane.tabs().is_empty())
    );
    assert_ne!(runtime.focused_terminal(), Some(terminal));
}

#[test]
fn restore_intent_publish_failure_keeps_bytes_but_does_not_block_generic_restore() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generic = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    let applied = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
    );
    assert!(matches!(
        runtime.active_pane().tabs(),
        [PaneTab::Live(LivePane {
            terminal,
            kind: PaneKind::Terminal
        })] if terminal.fences(&generic)
    ));
    assert_eq!(runtime.focused_terminal(), Some(generic));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(!retry.complete(std::time::Duration::ZERO, applied.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_secs(60)));
    if let crate::presentation::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
        crate::presentation::surface_agent_tab_intent_error(&mut runtime, error);
    }
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Mixed inventory and prior runtime state share one failure fixture.
fn mixed_restore_intent_failure_preserves_visible_agents_and_restores_generics() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let inventory_only_continuation = AgentContinuationRef::new();
    let agent = scoped_terminal_ref(workspace, Some(session));
    let inventory_only_agent = scoped_terminal_ref(workspace, Some(session));
    let existing_generic = scoped_terminal_ref(workspace, Some(session));
    let new_generic = scoped_terminal_ref(workspace, Some(session));
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: agent.clone(),
        select: true,
    });
    intent.revision = 3;
    let durable = Arc::new(Mutex::new(intent));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: agent.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: existing_generic.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(agent.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let fence = runtime.restore_fence();
    let applied = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![
                TerminalInventoryEntry {
                    terminal: agent.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: inventory_only_agent.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: existing_generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: new_generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
            ]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![
                    AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            agent.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    },
                    AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            inventory_only_agent.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation: inventory_only_continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    },
                ],
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
    );
    assert!(matches!(
        runtime.active_pane().tabs(),
        [
            PaneTab::Live(LivePane { terminal: visible_agent, kind: PaneKind::Agent }),
            PaneTab::Live(LivePane { terminal: retained_generic, kind: PaneKind::Terminal }),
            PaneTab::Live(LivePane { terminal: added_generic, kind: PaneKind::Terminal })
        ] if visible_agent.fences(&agent)
            && retained_generic.fences(&existing_generic)
            && added_generic.fences(&new_generic)
    ));
    assert!(runtime.active_pane().tabs().iter().all(|tab| {
        !matches!(tab, PaneTab::Live(pane) if pane.terminal.fences(&inventory_only_agent))
    }));
    assert_eq!(runtime.focused_terminal(), Some(agent));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    if let crate::presentation::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
        crate::presentation::surface_agent_tab_intent_error(&mut runtime, error);
    }
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );
}

#[test]
fn restore_retry_backoff_bounds_long_outage_and_reconnect_dispatches_once() {
    let mut retry = crate::presentation::RestoreRetryState::new();
    let mut jobs = 0_u32;
    let mut rpc_attempts = 0_u32;
    let mut notices = 0_u32;
    let mut frames = 0_u32;
    let end = std::time::Duration::from_secs(60);
    let mut now = std::time::Duration::ZERO;
    while now <= end {
        frames += 1;
        if retry.begin_if_due(now) {
            jobs += 1;
            // One bounded worker attempts a terminal/Agent/terminal
            // consistency bracket three times; ticks add no RPCs.
            rpc_attempts += 9;
            notices += u32::from(
                retry.complete(now, crate::presentation::RestoreJobOutcome::TransportFailed),
            );
        }
        now += std::time::Duration::from_millis(16);
    }
    assert!(frames > 3_000, "the render/input clock stayed live");
    assert!(jobs <= 20, "capped backoff bounded worker churn: {jobs}");
    assert_eq!(rpc_attempts, jobs * 9);
    assert_eq!(notices, 1, "one outage produces one notice");

    retry.reconnected(1, end);
    retry.reconnected(1, end);
    assert!(retry.begin_if_due(end));
    assert!(
        !retry.begin_if_due(end),
        "only one restore can be in flight"
    );
    assert!(!retry.complete(end, crate::presentation::RestoreJobOutcome::Applied));
    for offset in 1..=1_000 {
        assert!(!retry.begin_if_due(end + std::time::Duration::from_millis(offset)));
    }

    let mut outage = crate::presentation::RestoreRetryState::new();
    assert!(outage.begin_if_due(std::time::Duration::ZERO));
    assert!(outage.complete(
        std::time::Duration::ZERO,
        crate::presentation::RestoreJobOutcome::TransportFailed
    ));
    outage.request_observation(std::time::Duration::from_millis(10));
    assert!(
        !outage.begin_if_due(std::time::Duration::from_millis(10)),
        "a local Reopen cannot bypass the outage epoch backoff"
    );
    assert!(outage.begin_if_due(std::time::Duration::from_millis(250)));

    let mut in_flight = crate::presentation::RestoreRetryState::new();
    assert!(in_flight.begin_if_due(std::time::Duration::ZERO));
    in_flight.request_observation(std::time::Duration::from_millis(1));
    assert!(!in_flight.complete(
        std::time::Duration::from_millis(1),
        crate::presentation::RestoreJobOutcome::Applied
    ));
    assert!(!in_flight.begin_if_due(std::time::Duration::from_secs(1)));

    let mut changed_idle = crate::presentation::RestoreRetryState::new();
    assert!(changed_idle.begin_if_due(std::time::Duration::ZERO));
    assert!(!changed_idle.complete(
        std::time::Duration::ZERO,
        crate::presentation::RestoreJobOutcome::Applied
    ));
    changed_idle.request_changed_observation(std::time::Duration::from_millis(1));
    assert!(changed_idle.begin_if_due(std::time::Duration::from_millis(1)));

    let mut changed_in_flight = crate::presentation::RestoreRetryState::new();
    assert!(changed_in_flight.begin_if_due(std::time::Duration::ZERO));
    changed_in_flight.request_changed_observation(std::time::Duration::from_millis(1));
    assert!(!changed_in_flight.complete(
        std::time::Duration::from_millis(1),
        crate::presentation::RestoreJobOutcome::Applied
    ));
    assert!(
        changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)),
        "an in-flight snapshot may predate an Agent exit and needs one follow-up"
    );
    assert!(!changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)));
}

#[test]
fn failed_restore_keeps_the_port_for_a_reconnect_dispatch() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_restore_job(
        Box::new(RetryRestorePort {
            workspace,
            entries: Vec::new(),
            runtimes: Vec::new(),
            fail_attempts: usize::MAX,
            terminal_attempts: Arc::new(AtomicUsize::new(0)),
            agent_attempts: Arc::new(AtomicUsize::new(0)),
        }),
        workspace,
        BTreeSet::from([session]),
        0,
        0,
        sender,
    );
    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    let applied = crate::presentation::apply_restore_completion(
        completion,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::TransportFailed
    );
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(retry.complete(
        std::time::Duration::ZERO,
        crate::presentation::RestoreJobOutcome::TransportFailed
    ));
    assert!(runtime.state().notice().is_none());
}

#[test]
#[allow(clippy::too_many_lines)] // Runtime, durable bytes, and retry fencing share one race fixture.
fn late_restore_leaves_runtime_and_durable_intent_bytes_unchanged() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: first,
        terminal: first_terminal.clone(),
        select: true,
    });
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: second,
        terminal: second_terminal.clone(),
        select: false,
    });
    initial.revision = 4;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Down);
    let _ = runtime.handle_key(Key::Enter);
    let (dispatched_interaction, dispatched_revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        dispatched_interaction,
        dispatched_revision,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second_terminal.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));

    // These are the user changes which make the dispatched observation
    // stale: reorder, select the survivor, then close the former selection.
    let _ = runtime.reorder_tab(TabDirection::Next);
    let _ = runtime.focus_terminal(Target::Session(session), second_terminal.clone());
    let _ = runtime.focus_terminal(Target::Session(session), first_terminal.clone());
    let _ = runtime.close_focused_pane();
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Reorder {
        session_id: Some(session),
        continuations: vec![second, first],
    });
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Select {
        session_id: Some(session),
        continuation: Some(second),
    });
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss {
        continuation: first,
    });
    let durable_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let revision_before = durable.lock().unwrap().revision;
    let runtime_before = runtime.active_pane().clone();
    let mutation_count = mutations.lock().unwrap().len();

    let runtime_item = |continuation, terminal: &TerminalRef| AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
            .unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    let terminal_inventory = || {
        vec![
            TerminalInventoryEntry {
                terminal: first_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: second_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
        ]
    };
    let agent_inventory = || AgentInventory {
        workspace_id: workspace,
        runtimes: vec![
            runtime_item(first, &first_terminal),
            runtime_item(second, &second_terminal),
        ],
        resumable: Vec::new(),
    };
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let completion = crate::presentation::RestoreCompletion {
        port: Box::new(UnavailableAgentCommandPort),
        dispatched_interaction,
        dispatched_registry_revision: dispatched_revision,
        dispatched_allowed_sessions: BTreeSet::from([session]),
        terminals: Ok(terminal_inventory()),
        agents: Ok(agent_inventory()),
        observation_coherent: true,
    };
    let applied = crate::presentation::apply_restore_completion(
        completion,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::FenceRejected
    );
    assert_eq!(runtime.active_pane(), &runtime_before);
    assert_eq!(mutations.lock().unwrap().len(), mutation_count);
    assert_eq!(durable.lock().unwrap().revision, revision_before);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        durable_before
    );

    // A fence rejection is a local UI race, not a daemon outage. Return
    // the dedicated port and admit one observation immediately under the
    // fresh fence, without a notice/backoff or duplicate in-flight job.
    let redispatch_at = std::time::Duration::from_secs(1);
    assert!(!retry.complete(redispatch_at, applied.outcome));
    assert!(retry.begin_if_due(redispatch_at));
    assert!(!retry.begin_if_due(redispatch_at));

    let fresh_fence = runtime.restore_fence();
    let fresh = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: applied.port,
            dispatched_interaction: fresh_fence.0,
            dispatched_registry_revision: fresh_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminal_inventory()),
            agents: Ok(agent_inventory()),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        fresh.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(redispatch_at, fresh.outcome));
    assert_eq!(mutations.lock().unwrap().len(), mutation_count + 1);
    assert_eq!(runtime.focused_terminal(), Some(second_terminal.clone()));
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(!runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
            if terminal.fences(&first_terminal)
    )));
    assert!(runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
            if terminal.fences(&second_terminal)
    )));
    assert!(!retry.begin_if_due(redispatch_at + std::time::Duration::from_secs(60)));
}

#[test]
#[allow(clippy::too_many_lines)] // The stale and fresh observations must share one durable fixture.
fn cross_tui_stale_observe_omits_old_ref_then_fresh_observation_restores_replacement() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, Some(session));
    let replacement = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: old.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let dispatched = runtime.restore_fence();

    // Another TUI replaces O with R after this controller loaded revision 1.
    {
        let mut latest = durable.lock().unwrap();
        latest.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: replacement.clone(),
            select: true,
        });
        latest.revision += 1;
    }
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let terminals = |terminal: &TerminalRef| {
        vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }]
    };
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let stale = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminals(&old)),
            agents: Ok(inventory(&old)),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        stale.outcome,
        crate::presentation::RestoreJobOutcome::FenceRejected
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert_ne!(runtime.focused_terminal(), Some(old));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&replacement)
    );
    let redispatch_at = std::time::Duration::from_secs(1);
    assert!(!retry.complete(redispatch_at, stale.outcome));
    assert!(retry.begin_if_due(redispatch_at));
    assert!(!retry.begin_if_due(redispatch_at));

    let fresh_fence = runtime.restore_fence();
    let fresh = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: stale.port,
            dispatched_interaction: fresh_fence.0,
            dispatched_registry_revision: fresh_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminals(&replacement)),
            agents: Ok(inventory(&replacement)),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        fresh.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(redispatch_at, fresh.outcome));
    assert_eq!(runtime.focused_terminal(), Some(replacement));
    assert_eq!(mutations.lock().unwrap().len(), 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn successful_restore_retains_port_and_reconnect_reobserves_exactly_once() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let terminal_attempts = Arc::new(AtomicUsize::new(0));
    let agent_attempts = Arc::new(AtomicUsize::new(0));
    let port: Box<dyn AgentCommandPort> = Box::new(RetryRestorePort {
        workspace,
        entries: vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }],
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        fail_attempts: 0,
        terminal_attempts: Arc::clone(&terminal_attempts),
        agent_attempts: Arc::clone(&agent_attempts),
    });
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut retry = crate::presentation::RestoreRetryState::new();

    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let fence = runtime.restore_fence();
    crate::presentation::spawn_restore_job(
        port,
        workspace,
        BTreeSet::from([session]),
        fence.0,
        fence.1,
        sender.clone(),
    );
    let first = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let first = crate::presentation::apply_restore_completion(
        first,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        first.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(
        std::time::Duration::ZERO,
        crate::presentation::RestoreJobOutcome::Applied
    ));
    assert_eq!(mutations.lock().unwrap().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    let focus_before = runtime.focused_terminal();
    for tick in 1..=1_000 {
        assert!(!retry.begin_if_due(std::time::Duration::from_millis(tick)));
    }

    // A typed reconnect epoch, not a frame tick, admits one new observation
    // with the same dedicated port. RetryRestorePort::launch panics, so this
    // also proves reconnect inventory never becomes a spawn replay.
    let reconnect_at = std::time::Duration::from_secs(2);
    retry.reconnected(1, reconnect_at);
    retry.reconnected(1, reconnect_at);
    assert!(retry.begin_if_due(reconnect_at));
    assert!(!retry.begin_if_due(reconnect_at));
    let fence = runtime.restore_fence();
    crate::presentation::spawn_restore_job(
        first.port,
        workspace,
        BTreeSet::from([session]),
        fence.0,
        fence.1,
        sender,
    );
    let second = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let second = crate::presentation::apply_restore_completion(
        second,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        second.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(
        reconnect_at,
        crate::presentation::RestoreJobOutcome::Applied
    ));
    assert_eq!(mutations.lock().unwrap().len(), 2);
    assert_eq!(runtime.focused_terminal(), focus_before);
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 4);
    assert_eq!(agent_attempts.load(Ordering::SeqCst), 2);
    assert!(!retry.begin_if_due(reconnect_at + std::time::Duration::from_secs(60)));
}

#[test]
#[allow(clippy::too_many_lines)] // One round trip retains both surfaces' independent viewport and selection state.
fn drawer_round_trip_restores_both_views_and_restates_each_viewport_without_resync() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root = scoped_terminal_ref(workspace, None);
    let calls = Arc::new(Mutex::new(StreamCalls::default()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RecordingStreamPort(Arc::clone(&calls))),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![
            crate::presentation::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: root.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(root.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            crate::presentation::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: managed.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(managed.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let managed_geometry = terminal_geometry(24, 100);
    let drawer_geometry = foreground_terminal_geometry(
        24,
        100,
        true,
        false,
        false,
        Some(WorkspaceDrawerFocus::Director),
    );
    let mut controls = LiveTerminalControls::default();

    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    controls.scroll_up();
    controls.begin_selection(TerminalSelection::begin(
        vec!["managed".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    controls.extend_selection(TerminalPoint { row: 0, column: 6 });
    let _ = controls.finish_drag();

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root.clone()));
    sync_test_director_terminals(&mut ui, &root, &managed, 24, 100);
    let retained = ui.retained_terminal_view(&managed, 1).unwrap();
    assert_eq!(plain_terminal_rows(&retained), "three");
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    controls.scroll_up();
    controls.scroll_up();
    controls.begin_selection(TerminalSelection::begin(
        vec!["drawer".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    controls.extend_selection(TerminalPoint { row: 0, column: 5 });
    let _ = controls.finish_drag();

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(managed.clone()));
    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let managed_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    assert_eq!(managed_view.scroll, 1);
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("managed")
    );

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root.clone()));
    sync_test_director_terminals(&mut ui, &root, &managed, 24, 100);
    let drawer_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    assert_eq!(drawer_view.scroll, 2);
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("drawer")
    );

    let calls = calls.lock().unwrap();
    // The managed pane stays attached at its normal workspace geometry
    // through both Director visits. Only the root conversation is detached
    // when the overlay closes and attached again when it reopens.
    assert_eq!(
        calls.attach_geometries,
        [
            (managed.clone(), managed_geometry),
            (root.clone(), drawer_geometry),
            (root, drawer_geometry),
        ]
    );
    assert!(
        calls.resize_geometries.is_empty(),
        "drawer round trips must leave the managed terminal geometry untouched"
    );
    assert_eq!(calls.attaches, 3);
    assert_eq!(calls.detaches, 1);
}

#[test]
fn workflow_focus_survives_agent_restore_and_explicit_agent_selection_still_works() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let allowed = BTreeSet::from([session]);
    let terminals = [
        scoped_terminal_ref(workspace, Some(session)),
        scoped_terminal_ref(workspace, Some(session)),
    ];
    let agents = terminals
        .iter()
        .map(|terminal| AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation: AgentContinuationRef::new(),
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        })
        .collect::<Vec<_>>();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            allowed.clone(),
            Box::new(MemoryIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    runtime.on_effect(&Effect::OpenWorkflow { session });
    let workflow_selection = runtime.active_pane().selected().clone();
    // First Codex and then Claude appear through the real coherent restore path.
    for count in [1, 2] {
        let fence = runtime.restore_fence();
        let restored = crate::presentation::apply_restore_completion(
            crate::presentation::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: allowed.clone(),
                terminals: Ok(terminals[..count]
                    .iter()
                    .map(|terminal| TerminalInventoryEntry {
                        terminal: terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    })
                    .collect()),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: agents[..count].to_vec(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &allowed,
        );
        assert_eq!(
            restored.outcome,
            crate::presentation::RestoreJobOutcome::Applied
        );
        assert_eq!(runtime.active_pane().selected(), &workflow_selection);
        assert_eq!(runtime.active_pane().tabs().len(), count + 1);
        assert_eq!(runtime.focused_terminal(), None);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    // Explicit tab navigation remains the existing exact-terminal selection path.
    for terminal in &terminals {
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal().as_ref(), Some(terminal));
    }
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.active_pane().selected(), &workflow_selection);
}

#[test]
fn restore_open_panes_projects_live_runtimes_and_skips_dead_and_duplicates() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_terminal = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, Some(session));
    let session_terminal = scoped_terminal_ref(workspace, Some(session));
    let dead = scoped_terminal_ref(workspace, Some(session));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: root_agent.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        // A dead process is reported non-live and must not become a tab.
        TerminalInventoryEntry {
            terminal: dead.clone(),
            kind: TerminalKind::Terminal,
            live: false,
        },
        // A duplicate of a live runtime must not double the tab.
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));

    // All three managed-session runtimes are projected, with the duplicate
    // terminal removed.
    assert_eq!(runtime.active_pane().tabs().len(), 3);
    assert!(runtime.state().has_live_pane());
    // Every live runtime is attached and streaming; the dead one is not.
    assert!(ui.terminal_rows(&root_terminal, None).is_some());
    assert!(ui.terminal_rows(&root_agent, None).is_some());
    assert!(ui.terminal_rows(&session_terminal, None).is_some());
    assert!(ui.terminal_rows(&dead, None).is_none());
}

#[test]
fn restored_terminal_and_agent_tabs_deliver_ordinary_closeup_input() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let agent = scoped_terminal_ref(workspace, Some(session));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: agent.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            Vec::new(),
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: inputs.clone(),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();

    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    // Inventory restoration stays in Switch, but preselects the first tab so
    // entering Closeup has a concrete input owner instead of a target-only
    // selection hidden behind a non-empty tab strip.
    assert!(!runtime.wants_live_input());
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    assert!(runtime.handle_key(Key::Enter).is_empty());
    assert!(runtime.wants_live_input());
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Char('x'),
    ));

    let _ = runtime.select_tab(TabDirection::Next);
    assert_eq!(runtime.focused_terminal(), Some(agent.clone()));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Enter,
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: b"copy".to_vec(),
        },
    ));
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![
            (terminal, b"x".to_vec()),
            (agent.clone(), b"\r".to_vec()),
            (agent, b"copy".to_vec()),
        ]
    );
}

#[test]
fn double_clicked_append_restored_session_attaches_and_receives_input() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_terminal = scoped_terminal_ref(workspace, None);
    let session_terminal = scoped_terminal_ref(workspace, Some(session));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: inputs.clone(),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.append_restore_snapshot(
        interaction,
        revision,
        vec![
            crate::presentation::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: root_terminal,
                    kind: PaneKind::Terminal,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            crate::presentation::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: session_terminal.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let _ = runtime.apply_event(AppEvent::Resize {
        width: 100,
        height: 30,
    });
    for at in [1_000, 1_100] {
        let _ = runtime.apply_event(AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(at),
        });
    }
    ui.sync_foreground_terminal(
        runtime.focused_terminal().as_ref(),
        terminal_geometry(20, 80),
    );

    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Char('x'),
    ));
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![(session_terminal, b"x".to_vec())]
    );
}

#[test]
fn restore_open_panes_restores_nothing_on_daemon_failure_or_without_a_port() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let live = TerminalInventoryEntry {
        terminal: scoped_terminal_ref(workspace, None),
        kind: TerminalKind::Terminal,
        live: true,
    };

    // A daemon failure restores nothing (and never spawns locally).
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries: vec![live],
                fail: true,
                inputs: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(!runtime.state().has_live_pane());

    // An embedder with no Agent port simply finds nothing to restore.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    assert!(runtime.active_pane().tabs().is_empty());
}

#[test]
fn workspace_switch_restores_each_projects_last_session_cursor() {
    let alpha = snapshot("alpha");
    let beta = snapshot_with_sessions("beta", &["first", "remembered"]);
    let remembered = beta.session_ids[1];
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    deck.set_icon_mode(usagi_core::domain::settings::IconMode::Text);
    let mut previous = WorkspaceRuntime::new(beta.workspace_id, beta.session_ids.clone());
    let _ = previous.handle_key(Key::Down);

    remember_workspace_session_focus(&mut deck, previous.state());
    let _ = previous.handle_key(Key::Down);
    remember_workspace_session_focus(&mut deck, previous.state());
    assert_eq!(
        deck.focused_session_for_path(&beta.workspace.path),
        Some(remembered),
        "the transient new-session row does not replace the last session focus"
    );

    let mut restored = WorkspaceRuntime::new(beta.workspace_id, beta.session_ids.clone());
    restore_workspace_session_focus(&deck, &beta.workspace.path, &mut restored);
    assert_eq!(
        restored.state().selected(),
        crate::usecase::application::controller::Selection::Target(Target::Session(remembered))
    );
    assert_eq!(restored.state().route(), Route::Home(HomeMode::Switch));

    deck.schedule_closeup(beta.workspace.path.clone());
    let _ = restored.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
        beta.session_lifecycles.clone(),
    )));
    restore_workspace_closeup(&mut deck, &beta.workspace.path, &mut restored);
    assert_eq!(restored.state().active(), Some(remembered));
    assert_eq!(restored.state().route(), Route::Home(HomeMode::Closeup));

    let transition = strip_ansi(
        &cached_workspace_switch_frame(
            &deck,
            &beta.workspace.path,
            24,
            80,
            0,
            "Opening workspace…",
            false,
        )
        .unwrap()
        .join("\n"),
    );
    assert!(transition.contains("> remembered"), "{transition}");
    assert!(!transition.contains("> first"), "{transition}");
}

#[test]
fn cancelling_recent_and_open_list_restores_the_originating_screen() {
    let cases = [
        (
            vec![Key::Char('1'), Key::Quit],
            Vec::new(),
            vec![recent("recent")],
            "Menu",
        ),
        (
            vec![Key::Char('o'), Key::Enter, Key::Quit],
            vec![ws("open")],
            Vec::new(),
            "Open Workspace",
        ),
    ];

    for (keys, workspaces, recent, originating_screen) in cases {
        let mut term = ResponsiveLoadingTerminal {
            keys: keys.into(),
            wait_keys: VecDeque::from([Key::Escape]),
            ..ResponsiveLoadingTerminal::default()
        };
        let mut loader = FakeLoader {
            operation_mode: FakeOperationMode::Background,
            open_delay: std::time::Duration::from_millis(20),
            ..FakeLoader::default()
        };
        let mut settings = RecordingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                workspaces,
                recent,
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains("Workspace opening was cancelled.")),
            "cancel notice was absent from {originating_screen}: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains(originating_screen)),
            "originating screen was absent: {frames:?}"
        );
    }
}

#[test]
fn restored_or_unreadable_workspace_is_not_unregistered() {
    let alpha = ws("alpha");
    let mut restored_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('y'), Key::Quit]);
    let mut restored = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: Vec::new(),
        ..FakeLoader::default()
    };
    run(
        &mut restored_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut restored,
    )
    .unwrap();
    assert_eq!(restored.cleanup_calls, 1);
    assert!(restored_term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("Workspace changed while confirming")
    }));

    let mut unreadable_term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Quit]);
    let mut unreadable = FakeLoader {
        missing_error: Some(io::ErrorKind::PermissionDenied),
        ..FakeLoader::default()
    };
    run(
        &mut unreadable_term,
        vec![alpha],
        Vec::new(),
        now(),
        &mut unreadable,
    )
    .unwrap();
    assert_eq!(unreadable.cleanup_calls, 0);
    let frames = unreadable_term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("workspace path could not be checked"))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains("Workspace not found"))
    );
}

#[test]
fn interrupted_history_joins_its_own_scope_in_the_restore_projection() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let other = SessionId::new();
    let root_history = interrupted_history(workspace, None, true);
    let session_history = interrupted_history(workspace, Some(session), true);
    let second_session_history = interrupted_history(workspace, Some(session), false);

    let targets = crate::presentation::pane_restore_targets(
        workspace,
        &BTreeSet::from([session, other]),
        AgentTabProjection::default(),
        &[],
        None,
        vec![
            root_history.clone(),
            session_history.clone(),
            second_session_history.clone(),
        ],
        &BTreeMap::from([(Some(session), second_session_history.continuation)]),
    );

    let root = targets
        .iter()
        .find(|target| target.target == Target::Root(workspace))
        .unwrap();
    assert_eq!(
        root.interrupted
            .iter()
            .map(|tab| tab.continuation)
            .collect::<Vec<_>>(),
        vec![root_history.continuation]
    );
    let managed = targets
        .iter()
        .find(|target| target.target == Target::Session(session))
        .unwrap();
    // Several histories in one scope stay separate tabs, in projection order.
    assert_eq!(
        managed.selected_interrupted,
        Some(second_session_history.continuation)
    );
    assert_eq!(
        managed
            .interrupted
            .iter()
            .map(|tab| tab.continuation)
            .collect::<Vec<_>>(),
        vec![
            session_history.continuation,
            second_session_history.continuation
        ]
    );
    // A session without history keeps an empty entry rather than borrowing
    // another scope's tabs.
    let empty = targets
        .iter()
        .find(|target| target.target == Target::Session(other))
        .unwrap();
    assert!(empty.interrupted.is_empty());
}
