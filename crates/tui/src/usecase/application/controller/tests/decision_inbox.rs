use super::*;

fn saved(workspace: WorkspaceId) -> UserDecision {
    let mut answer = pending_decision(workspace);
    answer.status = UserDecisionStatus::Resolved;
    answer.answer = Some(UserDecisionAnswer::Option {
        option_id: answer.options[0].id.clone(),
        comment: Some("Only staging".into()),
    });
    answer
}

fn snapshot(state: &mut AppState, decisions: Vec<UserDecision>) {
    let workspace = state.workspace;
    let _ = update(
        state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions,
        }),
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One human interaction covers inspection, cancellation, failure, retry, and confirmed disposal.
fn decision_inbox_saved_answers_are_quiet_read_only_and_require_explicit_disposal() {
    let workspace = WorkspaceId::new();
    let foreign = WorkspaceId::new();
    let answer = saved(foreign);
    let mut state = AppState::home(workspace, vec![]);
    snapshot(&mut state, vec![answer.clone()]);
    assert!(state.decisions().is_empty());
    assert_eq!(state.decision_answers(), std::slice::from_ref(&answer));
    assert!(state.unread_decisions.is_empty());
    assert!(state.overlay().is_none());
    let retained = state.decision_snapshot();
    let mut restored = AppState::home(WorkspaceId::new(), vec![]);
    let _ = update(&mut restored, AppEvent::RestoreDecisions(retained));
    assert_eq!(restored.decision_answers(), std::slice::from_ref(&answer));

    let _ = open_decisions(&mut state, None);
    assert!(state.decision_overlay().unwrap().editor().is_none());
    assert!(state.decision_overlay().unwrap().show_answers());
    let _ = update_decisions_overlay(&mut state, AppKey::Tab);
    assert!(!state.decision_overlay().unwrap().show_answers());
    assert!(update_decisions_overlay(&mut state, AppKey::Enter).is_empty());
    assert_eq!(
        update_decisions_overlay(&mut state, AppKey::Tab),
        vec![Effect::RefreshDecisions { workspace }]
    );
    assert!(state.decision_overlay().unwrap().show_answers());
    for key in [AppKey::Down, AppKey::Up, AppKey::Enter] {
        assert!(update_decisions_overlay(&mut state, key).is_empty());
    }
    for key in [
        AppKey::Enter,
        AppKey::SubmitDecision,
        AppKey::Paste("overwrite".into()),
        AppKey::SetDecisionFreeform("overwrite".into()),
        AppKey::Char('x'),
        AppKey::Tab,
    ] {
        assert!(update_decisions_overlay(&mut state, key).is_empty());
    }
    assert_eq!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .decision(),
        &answer
    );
    assert!(update_decisions_overlay(&mut state, AppKey::Char('d')).is_empty());
    assert!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .discard_confirmation()
    );
    assert!(update_decisions_overlay(&mut state, AppKey::Escape).is_empty());
    assert!(
        !state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .discard_confirmation()
    );
    assert!(update_decisions_overlay(&mut state, AppKey::Enter).is_empty());
    let _ = update_decisions_overlay(&mut state, AppKey::Char('d'));
    let expected = vec![Effect::DiscardDecision {
        workspace: foreign,
        decision_id: answer.decision_id,
    }];
    assert_eq!(
        update_decisions_overlay(&mut state, AppKey::Enter),
        expected
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionError {
            workspace: foreign,
            decision_id: answer.decision_id,
            error: SafeError {
                message: SafeMessage::new("Try again"),
                error_id: "retry".into(),
            },
        }),
    );
    snapshot(&mut state, vec![answer.clone()]);
    assert_eq!(state.decision_answers(), std::slice::from_ref(&answer));
    assert!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .error()
            .is_some()
    );
    assert_eq!(
        update_decisions_overlay(&mut state, AppKey::Enter),
        expected
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionDiscarded {
            workspace: foreign,
            decision_id: answer.decision_id,
        }),
    );
    assert!(state.decision_answers().is_empty());
    assert!(state.overlay().is_none());
}

#[test]
fn decision_inbox_tracks_remote_resolution_ack_and_exact_selection() {
    let workspace = WorkspaceId::new();
    let mut question = pending_decision(workspace);
    let id = question.decision_id;
    let mut state = AppState::home(workspace, vec![]);
    snapshot(&mut state, vec![question.clone()]);
    assert!(state.decision_overlay().unwrap().editor().is_some());
    question.status = UserDecisionStatus::Resolved;
    question.answer = saved(workspace).answer;
    snapshot(&mut state, vec![question.clone()]);
    assert!(state.decision_overlay().unwrap().show_answers());
    assert!(update_decisions_overlay(&mut state, AppKey::Enter).is_empty());
    assert_eq!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .decision(),
        &question
    );
    let _ = update_decisions_overlay(&mut state, AppKey::Escape);
    let _ = update_decisions_overlay(&mut state, AppKey::Tab);
    assert!(!state.decision_overlay().unwrap().show_answers());
    let _ = open_decisions(&mut state, Some(id));
    snapshot(&mut state, vec![question]);
    assert!(state.decision_overlay().unwrap().show_answers());
    assert_eq!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .decision()
            .decision_id,
        id
    );
    snapshot(&mut state, vec![]);
    assert!(state.decision_overlay().unwrap().editor().is_none());
    assert!(state.decision_overlay().unwrap().request_error().is_some());
    assert!(update_decisions_overlay(&mut state, AppKey::Char('d')).is_empty());
    assert!(state.decision_answers().is_empty());
}
