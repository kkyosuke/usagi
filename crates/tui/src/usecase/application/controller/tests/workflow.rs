//! workflow の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn workflow_recovers_pending_start_and_accepts_instruction_completion() {
    use crate::usecase::application::workflow::{WorkflowJob, fixture_run};
    use usagi_core::domain::workflow::{
        Recipient, WorkflowCommand, WorkflowPendingStart, WorkflowSnapshot,
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let operation = OperationId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let _ = submit_closeup_workflow(&mut state, session, "");
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let snapshot = WorkflowSnapshot {
        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
        session,
        run: None,
        pending_start: Some(WorkflowPendingStart {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            operation_id: operation,
            goal: "Build login".into(),
            error: Some("Sign in to retry".into()),
            issue: None,
        }),
        finished: Vec::new(),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(snapshot.clone())),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert_eq!(panel.draft.value(), "Build login");
    assert_eq!(panel.error.as_deref(), Some("Sign in to retry"));
    assert_eq!(
        panel.pending,
        Some((
            operation,
            WorkflowCommand::Start {
                goal: "Build login".into(),
                agents: usagi_core::domain::workflow::WorkflowAgents::default()
            }
        ))
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(snapshot)),
        }),
    );
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    assert!(
        matches!(&effects[0], Effect::Workflow(job) if job.control.as_ref().unwrap().0 == operation)
    );
    let instruction = (
        OperationId::new(),
        WorkflowCommand::Instruct {
            recipient: Recipient::Implementer,
            body: "Add tests".into(),
        },
    );
    let panel = state.workflows.get_mut(&session).unwrap();
    panel.pending = Some(instruction.clone());
    panel.draft.replace("Add tests");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                control: Some(instruction),
                ..job
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(fixture_run(session)),
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert!(panel.pending.is_none());
    assert!(panel.draft.value().is_empty());
}

#[test]
fn finishing_a_workflow_opens_the_tab_and_resends_one_operation() {
    use crate::usecase::application::workflow::WorkflowJob;
    use usagi_core::domain::workflow::{
        FinishedRun, Outcome, Phase, WorkflowCommand, WorkflowSnapshot,
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);

    // Anything but `finish` is refused without touching the panel.
    assert!(submit_closeup_workflow(&mut state, session, "stop").is_empty());
    assert!(state.notice.is_some());
    assert!(
        state
            .workflows
            .get(&session)
            .is_none_or(|panel| { panel.pending.is_none() && !panel.submitting && !panel.loading })
    );

    // Finishing opens the tab so the daemon's answer has somewhere to land, and
    // carries a control payload rather than a read.
    let effects = submit_closeup_workflow(&mut state, session, "finish");
    let [Effect::OpenWorkflow { .. }, Effect::Workflow(job)] = effects.as_slice() else {
        panic!("finishing opens the tab and dispatches one control, got {effects:?}");
    };
    let control = job.control.clone().expect("finishing is a control request");
    assert_eq!(control.1, WorkflowCommand::Finish);
    let panel = state.workflow_panel(session).unwrap();
    assert!(panel.submitting);
    assert_eq!(panel.pending.as_ref(), Some(&control));

    // A request in flight owns the panel; asking again only re-opens the tab.
    let again = submit_closeup_workflow(&mut state, session, "finish");
    assert!(matches!(again.as_slice(), [Effect::OpenWorkflow { .. }]));

    // A lost answer keeps the same operation, so retrying ends the run once.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Err(crate::usecase::application::workflow::WorkflowError {
                message: "connection lost".into(),
                unconfirmed: true,
            }),
        }),
    );
    let effects = submit_closeup_workflow(&mut state, session, "finish");
    let [_, Effect::Workflow(resent)] = effects.as_slice() else {
        panic!("an unconfirmed finish is resent, got {effects:?}");
    };
    assert_eq!(resent.control, Some(control.clone()));

    // The answer clears the request, keeps the draft, and shows the archive.
    let panel = state.workflows.get_mut(&session).unwrap();
    panel.draft.replace("a goal I am still typing");
    let ended = FinishedRun {
        id: OperationId::new(),
        outcome: Outcome::Completed,
        goal: "Implement login".into(),
        phase: Phase::Ready,
        issue: Some(745),
        pr_url: Some("https://example.test/pr/1".into()),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                control: Some(control),
                ..job.clone()
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
                finished: vec![ended.clone()],
            })),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert!(panel.pending.is_none());
    assert!(!panel.submitting);
    assert!(panel.run.is_none());
    assert_eq!(panel.finished, vec![ended]);
    assert_eq!(
        panel.draft.value(),
        "a goal I am still typing",
        "finishing carries no draft, so it consumes none"
    );

    // With the run gone the tab is back to starting one, and `workflow` alone
    // is a read again.
    let effects = submit_closeup_workflow(&mut state, session, "");
    let [_, Effect::Workflow(read)] = effects.as_slice() else {
        panic!("reopening reads the workflow, got {effects:?}");
    };
    assert!(read.control.is_none());
}

#[test]
#[allow(clippy::too_many_lines)] // One correlated retry scenario keeps operation/draft assertions together.
fn workflow_control_roundtrip_preserves_unknown_requests_and_newer_text() {
    use crate::usecase::application::workflow::{
        WorkflowEdit, WorkflowError, WorkflowJob, fixture_run,
    };
    use usagi_core::domain::workflow::{WorkflowCommand, WorkflowSnapshot};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let effects = submit_closeup_workflow(&mut state, session, "");
    assert_eq!(effects.len(), 2);
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    assert!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        )
        .is_empty()
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Paste("Build login".into()),
        },
    );
    let first = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(submit) = first[0].clone() else {
        panic!("expected workflow effect")
    };
    assert!(
        matches!(&submit.control, Some((_, WorkflowCommand::Start { goal, .. })) if goal == "Build login")
    );
    assert!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        )
        .is_empty()
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit.clone(),
            result: Err(WorkflowError {
                message: "lost response".into(),
                unconfirmed: true,
            }),
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Paste(" plus tests".into()),
        },
    );
    assert_eq!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        ),
        first
    );
    let run = fixture_run(session);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit,
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(run.clone()),
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    assert_eq!(
        state.workflow_panel(session).unwrap().draft.value(),
        "Build login plus tests"
    );
    assert!(state.workflow_panel(session).unwrap().pending.is_none());
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Start,
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::End,
        },
    );
    for key in [
        AppKey::Left,
        AppKey::Right,
        AppKey::Up,
        AppKey::Down,
        AppKey::Tab,
        AppKey::PageUp,
        AppKey::PageDown,
        AppKey::Backspace,
        AppKey::Enter,
        AppKey::Escape,
    ] {
        let _ = update(&mut state, AppEvent::WorkflowInput { session, key });
    }
    let next = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(submit) = next[0].clone() else {
        panic!("expected instruction")
    };
    assert!(matches!(
        &submit.control,
        Some((_, WorkflowCommand::Instruct { .. }))
    ));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit.clone(),
            result: Err(WorkflowError {
                message: "invalid".into(),
                unconfirmed: false,
            }),
        }),
    );
    assert!(state.workflow_panel(session).unwrap().pending.is_none());
    // A late reply for the rejected operation cannot clear a new draft.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit,
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(run),
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    assert!(
        !state
            .workflow_panel(session)
            .unwrap()
            .draft
            .value()
            .is_empty()
    );
    // Periodic observation is non-blocking and does not start work. It is
    // spaced from the previous read rather than run on a frame counter, so the
    // pane cannot put the daemon under a per-frame read flood.
    let due = state.workflow_panel(session).unwrap().snapshot_due_tick;
    assert!(due >= 60, "a read schedules the next one a second out");
    state.mascot_tick = due - 2;
    assert!(update(&mut state, AppEvent::Tick).is_empty());
    assert_eq!(
        update(&mut state, AppEvent::Tick),
        vec![Effect::Workflow(job)]
    );
}

#[test]
fn workflow_rejects_foreign_stale_and_overlay_input() {
    use crate::usecase::application::workflow::{WorkflowEdit, WorkflowJob};
    use usagi_core::domain::workflow::WorkflowSnapshot;
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    assert!(state.workflow_panel(session).is_none());
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    assert!(submit_closeup_workflow(&mut state, session, "invalid").is_empty());
    let _ = submit_closeup_workflow(&mut state, session, "");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session: SessionId::new(),
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    assert!(state.workflow_panel(session).unwrap().error.is_some());
    let foreign = WorkflowJob {
        workspace: WorkspaceId::new(),
        ..job
    };
    assert!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Workflow {
                job: foreign,
                result: Ok(Box::new(WorkflowSnapshot {
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    session,
                    run: None,
                    pending_start: None,
                    finished: Vec::new()
                }))
            })
        )
        .is_empty()
    );
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Char('x'),
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    assert!(
        state
            .workflow_panel(session)
            .unwrap()
            .draft
            .value()
            .is_empty()
    );
}

#[test]
fn workflow_offers_and_submits_only_providers_this_machine_can_launch() {
    use crate::usecase::application::workflow::WorkflowJob;
    use usagi_core::domain::{
        settings::{AvailableModels, DefaultModel},
        workflow::{WorkflowAgents, WorkflowCommand, WorkflowSnapshot},
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    // Claude Code is installed; Fugu shares its executable but has no key, and
    // neither Codex nor AGY is installed.
    state.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    submit_closeup_workflow(&mut state, session, "");
    assert_eq!(
        state.workflow_panel(session).unwrap().agents,
        WorkflowAgents {
            planner: DefaultModel::Claude,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::Claude,
        }
    );
    // The daemon keeps the previous run's choices, which may name a provider
    // that has since stopped being launchable here.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                workspace,
                session,
                control: None,
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: WorkflowAgents {
                    planner: DefaultModel::Agy,
                    implementer: DefaultModel::OpenAi,
                    reviewer: DefaultModel::SakanaAi,
                },
                session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    for key in [
        AppKey::Paste("Task".into()),
        AppKey::Tab,
        AppKey::Right,
        AppKey::Right,
    ] {
        let _ = update(&mut state, AppEvent::WorkflowInput { session, key });
    }
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(job) = &effects[0] else {
        panic!("workflow submission");
    };
    let Some((_, WorkflowCommand::Start { agents, .. })) = &job.control else {
        panic!("workflow start");
    };
    assert_eq!(
        *agents,
        WorkflowAgents {
            planner: DefaultModel::Claude,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::Claude,
        }
    );
}

#[test]
fn workflow_uses_saved_agents_without_overwriting_edits_and_submits_exact_choices() {
    use crate::usecase::application::workflow::WorkflowJob;
    use usagi_core::domain::{
        settings::DefaultModel,
        workflow::{WorkflowAgents, WorkflowCommand, WorkflowSnapshot},
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    submit_closeup_workflow(&mut state, session, "");
    let saved = WorkflowAgents {
        planner: DefaultModel::Agy,
        implementer: DefaultModel::Claude,
        reviewer: DefaultModel::OpenAi,
    };
    let snapshot = AppEvent::Backend(BackendEvent::Workflow {
        job: WorkflowJob {
            workspace,
            session,
            control: None,
        },
        result: Ok(Box::new(WorkflowSnapshot {
            agents: saved,
            session,
            run: None,
            pending_start: None,
            finished: Vec::new(),
        })),
    });
    let _ = update(&mut state, snapshot.clone());
    assert_eq!(state.workflow_panel(session).unwrap().agents, saved);
    for key in [
        AppKey::Paste("Task".into()),
        AppKey::Tab,
        AppKey::Right,
        AppKey::Left,
        AppKey::Right,
        AppKey::Char('x'),
    ] {
        let _ = update(&mut state, AppEvent::WorkflowInput { session, key });
    }
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: crate::usecase::application::workflow::WorkflowEdit::Start,
        },
    );
    assert_eq!(state.workflow_panel(session).unwrap().draft.cursor(), 4);
    let chosen = state.workflow_panel(session).unwrap().agents;
    assert_ne!(chosen.planner, saved.planner);
    assert_eq!(chosen.implementer, saved.implementer);
    let _ = update(&mut state, snapshot);
    assert_eq!(state.workflow_panel(session).unwrap().agents, chosen);
    assert_eq!(state.workflow_panel(session).unwrap().draft.value(), "Task");
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(job) = &effects[0] else {
        panic!("workflow submission");
    };
    assert!(
        matches!(&job.control, Some((_, WorkflowCommand::Start { goal, agents })) if goal == "Task" && *agents == chosen)
    );
}

#[test]
fn history_scrolling_is_bounded_and_returns_to_the_latest_in_one_operation() {
    use crate::usecase::application::workflow::{WorkflowEdit, WorkflowJob, fixture_run};
    use usagi_core::domain::workflow::{WorkflowHistoryEntry, WorkflowSnapshot};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let _ = submit_closeup_workflow(&mut state, session, "");

    let mut run = fixture_run(session);
    let entry = |index: usize| WorkflowHistoryEntry {
        id: usagi_core::domain::id::OperationId::new(),
        actor: "claude".into(),
        body: format!("entry {index}"),
    };
    run.history.extend((0..12).map(entry));
    let land = |state: &mut AppState, run: &usagi_core::domain::workflow::WorkflowRun| {
        let _ = update(
            state,
            AppEvent::Backend(BackendEvent::Workflow {
                job: WorkflowJob {
                    workspace,
                    session,
                    control: None,
                },
                result: Ok(Box::new(WorkflowSnapshot {
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    session,
                    run: Some(run.clone()),
                    pending_start: None,
                    finished: Vec::new(),
                })),
            }),
        );
    };
    land(&mut state, &run);

    // Paging back past the oldest row used to leave the viewport empty.
    for _ in 0..20 {
        let _ = update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::PageUp,
            },
        );
    }
    let offset = state.workflow_panel(session).unwrap().history_offset;
    assert_eq!(offset, 11);

    // Rows arriving under a scrolled-back reader hold their place.
    run.history.extend((12..15).map(entry));
    land(&mut state, &run);
    assert_eq!(
        state.workflow_panel(session).unwrap().history_offset,
        offset + 3
    );

    // One operation follows the latest again.
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::HistoryLatest,
        },
    );
    assert_eq!(state.workflow_panel(session).unwrap().history_offset, 0);

    // Following the latest, arriving rows are what the reader wants to see.
    run.history.extend((15..17).map(entry));
    land(&mut state, &run);
    assert_eq!(state.workflow_panel(session).unwrap().history_offset, 0);
}

#[test]
fn history_keys_stay_live_while_the_start_form_owns_the_caret() {
    use crate::usecase::application::workflow::{WorkflowEdit, WorkflowJob};
    use usagi_core::domain::workflow::WorkflowSnapshot;
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let _ = submit_closeup_workflow(&mut state, session, "");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                workspace,
                session,
                control: None,
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );

    // Reading the history is not editing the draft, and the pane's hint and
    // `document/11-keybindings.md` promise these keys without a caveat.
    let panel = state.workflows.get_mut(&session).unwrap();
    panel.agent_field = Some(0);
    panel.finished = (0..8)
        .map(|index| usagi_core::domain::workflow::FinishedRun {
            id: usagi_core::domain::id::OperationId::new(),
            outcome: usagi_core::domain::workflow::Outcome::Stopped,
            goal: format!("attempt {index}"),
            phase: usagi_core::domain::workflow::Phase::Revising,
            issue: None,
            pr_url: None,
        })
        .collect();
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::PageUp,
        },
    );
    assert_eq!(state.workflow_panel(session).unwrap().history_offset, 5);
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::HistoryLatest,
        },
    );
    assert_eq!(state.workflow_panel(session).unwrap().history_offset, 0);
    // Neither of them touched the draft, and the form still owns the caret.
    let panel = state.workflow_panel(session).unwrap();
    assert!(panel.draft.value().is_empty());
    assert_eq!(panel.agent_field, Some(0));
}
