//! pr の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn sidebar_reserved_pr_cells_without_a_visible_pr_do_not_open_a_modal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let mut dismissed = pr_link(41);
    dismissed.state = PrState::Dismissed;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![dismissed],
        }),
    );

    assert!(pointer_effects(&mut state, 35, 3, std::time::Duration::ZERO).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn pr_overlay_opens_reflows_material_navigates_opens_and_closes() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // `p` requests the active target's list without showing an empty modal.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::OpenPrs)),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.pr_request(), Some(target));

    // A list for another target is ignored; the matching one fills the overlay.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target: Target::Root(workspace),
            revision: 0,
            prs: vec![pr_link(9)],
        }),
    );
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.pr_request(), Some(target));
    let prs = vec![pr_link(1), pr_link(2)];
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: prs.clone(),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
    assert_eq!(state.pr_overlay().unwrap().selected(), 0);
    assert_eq!(state.session_prs(session), Some(prs.as_slice()));

    // A delayed older snapshot cannot roll either the modal or sidebar
    // projection back after a newer daemon revision has landed.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: vec![pr_link(99)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    assert_eq!(state.session_prs(session), Some(prs.as_slice()));

    // Down/Up wrap around the list.
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.pr_overlay().unwrap().selected(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.pr_overlay().unwrap().selected(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.pr_overlay().unwrap().selected(), 1);

    // Enter opens the selected PR through the browser effect.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::OpenPullRequest {
            url: prs[1].url().to_owned(),
        }]
    );

    // Esc closes the overlay and discards its state.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());

    // Reopening from the cached revision is immediate. A duplicate response
    // cannot make the modal diverge from that sidebar projection.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::OpenPrs)),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    let revision = state.session_pr_revision();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(55)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    assert_eq!(state.session_pr_revision(), revision);

    // Removing the stable session identity also removes its cached PR rows;
    // a later session reusing display text cannot inherit the badge.
    let revision = state.session_pr_revision();
    assert_eq!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new()))
        ),
        vec![Effect::SyncPullRequestTargets {
            sessions: Vec::new()
        }]
    );
    assert!(state.session_prs(session).is_none());
    assert!(state.session_pr_revision() > revision);
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), Selection::Idle);
    assert_eq!(state.active(), None);
}

#[test]
fn an_error_on_an_open_pr_modal_stays_in_it_and_a_stale_snapshot_keeps_it() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1)],
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), Some(Overlay::Prs));

    // A failed refresh keeps the rows the last real answer left and says why
    // they may be stale, instead of replacing the modal.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: safe_error("gh unavailable"),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 1);
    assert_eq!(
        state
            .pr_overlay()
            .unwrap()
            .error()
            .map(|error| error.message.as_str()),
        Some("gh unavailable")
    );

    // A snapshot the reducer rejects proves nothing, so it must not present the
    // unchanged rows as freshly confirmed.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1), pr_link(2)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 1);
    assert!(state.pr_overlay().unwrap().error().is_some());

    // The next authoritative snapshot clears it.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![pr_link(1), pr_link(2)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
    assert!(state.pr_overlay().unwrap().error().is_none());
}

#[test]
fn pr_overlay_stays_hidden_without_prs_and_reports_loading_errors() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    // A fetch error is not an authoritative empty snapshot, so its safe
    // diagnostic remains visible in the PR modal.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: safe_error("gh unavailable"),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(
        state
            .pr_overlay()
            .unwrap()
            .error()
            .map(|error| error.message.as_str()),
        Some("gh unavailable")
    );

    // An authoritative empty result discards the pending modal state.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());

    // Reopening from the known-empty cache also stays hidden, and the
    // duplicate revision returned by an explicit refresh clears its pending
    // request instead of leaving it to misclassify a future discovery.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none(), "a request draws nothing");
    assert_eq!(state.pr_request(), Some(target));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn root_pr_modal_keeps_its_inventory_across_status_tabs() {
    let (workspace, _, _) = ids();
    let target = Target::Root(workspace);
    let mut state = AppState::home(workspace, Vec::new());
    state.set_pr_auto_open(PrAutoOpen::Always);
    let mut merged = pr_link(2);
    merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1)],
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![pr_link(1), merged],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);

    // The workspace root reads the same inventory as a session, so its status
    // tabs filter the cached rows instead of emptying the modal for good.
    for (filter, expected) in [
        (PrFilter::Open, 1),
        (PrFilter::Closed, 0),
        (PrFilter::Merged, 1),
        (PrFilter::All, 2),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        assert_eq!(state.pr_overlay().unwrap().filter(), filter);
        assert_eq!(state.pr_overlay().unwrap().prs().len(), expected);
    }
}

#[test]
fn root_pr_overlay_uses_the_unfiltered_inventory_to_decide_visibility() {
    let (workspace, _, _) = ids();
    let target = Target::Root(workspace);
    let mut state = AppState::home(workspace, Vec::new());
    state.pr_request = Some(target);
    assert_eq!(state.pr_request(), Some(target));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1)],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn delayed_pr_request_does_not_steal_focus_and_empty_refresh_closes_modal() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // A foreground interaction opened after `p` wins over a delayed error.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: safe_error("gh unavailable"),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pr_overlay().is_none());

    // The same foreground interaction also wins over a delayed successful
    // response, while the authoritative PR cache still advances.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let pr = pr_link(41);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.session_prs(session), Some(std::slice::from_ref(&pr)));

    // A cached PR opens immediately. If a newer authoritative snapshot no
    // longer contains any visible PR, the stale modal closes.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn newly_detected_pr_auto_opens_without_reopening_or_stealing_focus() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // The empty baseline is not actionable. Its first newly discovered URL
    // opens the modal directly from the resident snapshot lane.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    let first = pr_link(41);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![first.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().target(), target);
    assert_eq!(
        state.pr_overlay().unwrap().prs(),
        std::slice::from_ref(&first)
    );

    // Closing acknowledges the discovery. A title/state refresh and a
    // duplicate cannot reopen it because neither introduces a new URL.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let mut enriched = first;
    enriched.title = Some("ready for review".into());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![enriched.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![enriched.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);

    // A frontmost interaction is never replaced. The new PR still enters
    // the shared cache and is visible the next time the user opens `p`.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let second = pr_link(42);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![enriched, second.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert!(
        state
            .pr_overlay()
            .unwrap()
            .prs()
            .iter()
            .any(|pr| pr.identity == second.identity)
    );
}

#[test]
fn initial_pr_snapshot_is_a_baseline_and_detected_pr_is_selected() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let existing = (1..=7).map(pr_link).collect::<Vec<_>>();

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: existing.clone(),
        }),
    );
    assert_eq!(state.overlay(), None);

    let detected = pr_link(8);
    let mut updated = existing;
    updated.push(detected.clone());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: updated,
        }),
    );

    let overlay = state.pr_overlay().unwrap();
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(overlay.selected(), 7);
    assert_eq!(overlay.selected_pr(), Some(&detected));
}

#[test]
fn pr_reference_filter_copy_dismiss_and_safe_auto_open_modes() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    let mut reference = pr_link(1);
    reference.auto_open = false;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![reference.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);

    state.route = Route::Home(HomeMode::Closeup);
    let open = pr_link(2);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![reference.clone(), open.clone()],
        }),
    );
    assert_eq!(
        state.overlay(),
        None,
        "switch-only must not steal live input"
    );
    let mut merged = open;
    merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![reference, merged],
        }),
    );
    assert!(state.celebrates_pr_merge(session));
    for _ in 0..24 {
        let _ = update(&mut state, AppEvent::Tick);
    }
    assert!(state.celebrates_pr_merge(session));
    for _ in 24..=pull_requests::MERGE_CELEBRATION_TICKS {
        let _ = update(&mut state, AppEvent::Tick);
    }
    assert!(!state.celebrates_pr_merge(session));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Char('c'))),
        vec![Effect::CopyPullRequest {
            url: "https://github.com/o/r/pull/1".into()
        }]
    );
    assert!(update(&mut state, AppEvent::Key(AppKey::Char('d'))).is_empty());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::DismissPullRequest {
            session,
            url: "https://github.com/o/r/pull/1".into()
        }]
    );
}

#[test]
fn open_pr_overlay_tracks_new_detection_and_navigates_status_tabs() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let mut merged = pr_link(2);
    merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1), merged.clone()],
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let newly_detected = pr_link(3);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![pr_link(1), merged, newly_detected.clone()],
        }),
    );
    assert_eq!(
        state.pr_overlay().unwrap().selected_pr(),
        Some(&newly_detected)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Right));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
    for (filter, expected) in [
        (PrFilter::Closed, 0),
        (PrFilter::Merged, 1),
        (PrFilter::All, 3),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        assert_eq!(state.pr_overlay().unwrap().filter(), filter);
        assert_eq!(state.pr_overlay().unwrap().prs().len(), expected);
    }

    // A resident refresh while the active status tab has no matches keeps
    // the modal open. The unfiltered inventory still has PRs, so the user
    // must be able to navigate to another tab without reopening it.
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Closed);
    assert!(state.pr_overlay().unwrap().prs().is_empty());
    let mut refreshed_merged = newly_detected;
    refreshed_merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![pr_link(1), refreshed_merged],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Closed);
    assert!(state.pr_overlay().unwrap().prs().is_empty());

    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 1);

    // The old hidden `f` shortcut is inert now that the visible tabs own
    // status navigation.
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('f')));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(PrFilter::All.label(), "all");
    assert_eq!(PrFilter::Open.label(), "open");
    assert_eq!(PrFilter::Closed.label(), "closed");
    assert_eq!(PrFilter::Merged.label(), "merged");
    assert_eq!(PrFilter::TABS.map(PrFilter::tab_index), [0, 1, 2, 3]);
    assert_eq!(
        PrFilter::TABS.map(PrFilter::previous),
        [
            PrFilter::Merged,
            PrFilter::All,
            PrFilter::Open,
            PrFilter::Closed,
        ]
    );

    state.pr_overlay = None;
    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
}

#[test]
fn explicit_pr_auto_open_modes_cover_always_notify_and_never() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    for (mode, opens, notifies) in [
        (PrAutoOpen::Always, true, false),
        (PrAutoOpen::NotifyOnly, false, true),
        (PrAutoOpen::Never, false, false),
    ] {
        let mut state = AppState::home(workspace, vec![session]);
        state.route = Route::Home(HomeMode::Closeup);
        state.set_pr_auto_open(mode);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 0,
                prs: Vec::new(),
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 1,
                prs: vec![pr_link(7)],
            }),
        );
        assert_eq!(state.overlay() == Some(Overlay::Prs), opens);
        assert_eq!(state.notice().is_some(), notifies);
    }
}

#[test]
fn dismissed_pr_does_not_auto_open() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    let mut dismissed = pr_link(41);
    dismissed.state = PrState::Dismissed;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![dismissed],
        }),
    );

    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn inbox_pr_focus_survives_loading_and_targets_the_selected_number() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let first = pr_link(1);
    let second = pr_link(2);
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone()))
        ),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.pr_request(), Some(target));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![first.clone(), second.clone()],
        }),
    );
    assert_eq!(
        state.pr_overlay().unwrap().selected_pr().unwrap().identity,
        second.identity
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::OpenPullRequest {
            url: second.url().into()
        }]
    );
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![first.clone(), second.clone()],
        }),
    );
    assert_eq!(
        state.pr_overlay().unwrap().selected_pr().unwrap().identity,
        second.identity
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![second.clone(), first.clone()],
        }),
    );
    assert_eq!(
        state.pr_overlay().unwrap().selected_pr().unwrap().identity,
        second.identity
    );
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![first.clone()],
        }),
    );
    assert!(state.pr_overlay().unwrap().error().is_some());
    assert!(state.pr_overlay().unwrap().selected_pr().is_none());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 4,
            prs: vec![first],
        }),
    );
    assert!(state.pr_overlay().unwrap().error().is_some());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
}

#[test]
fn inbox_pr_loading_is_cancelled_by_escape_or_a_new_interaction() {
    for cached in [false, true] {
        for event in [
            AppEvent::Key(AppKey::Escape),
            AppEvent::Key(AppKey::Down),
            AppEvent::Pointer {
                column: 2,
                row: 4,
                at: std::time::Duration::ZERO,
            },
        ] {
            let session = SessionId::new();
            let target = Target::Session(session);
            let mut state = AppState::home(WorkspaceId::new(), vec![session, SessionId::new()]);
            let first = pr_link(1);
            let pr = pr_link(2);
            if cached {
                let _ = update(
                    &mut state,
                    AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                        target,
                        revision: 1,
                        prs: vec![first.clone()],
                    }),
                );
            }
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
            );
            let _ = update(&mut state, AppEvent::Tick);
            assert_eq!(state.pr_request(), Some(target));
            let _ = update(&mut state, event);
            assert!(state.pr_request().is_none());
            assert!(state.pr_focus.is_none());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::PullRequestsError {
                    target,
                    error: SafeError {
                        message: SafeMessage::new("Unavailable"),
                        error_id: "failed".into(),
                    },
                }),
            );
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                    target,
                    revision: 2,
                    prs: vec![first, pr],
                }),
            );
            assert_eq!(state.overlay(), None);
            assert!(state.pr_overlay().is_none());
        }
    }
}

#[test]
fn inbox_pr_cancellation_is_forgotten_when_its_session_disappears() {
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state = AppState::home(WorkspaceId::new(), vec![session]);
    let pr = pr_link(2);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(vec![])),
    );
    assert!(state.pr_cancelled.is_empty());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr],
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn inbox_pr_refreshes_never_replace_a_missing_exact_selection() {
    for failed in [false, true] {
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(WorkspaceId::new(), vec![session]);
        let first = pr_link(1);
        let second = pr_link(2);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 1,
                prs: vec![first.clone(), second.clone()],
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
        );
        let event = if failed {
            BackendEvent::PullRequestsError {
                target,
                error: SafeError {
                    message: SafeMessage::new("Unavailable"),
                    error_id: "failed".into(),
                },
            }
        } else {
            BackendEvent::PullRequestsLoaded {
                target,
                revision: 1,
                prs: vec![first.clone(), second],
            }
        };
        let _ = update(&mut state, AppEvent::Backend(event));
        for revision in [2, 3] {
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                    target,
                    revision,
                    prs: vec![first.clone()],
                }),
            );
            assert!(state.pr_overlay().unwrap().selected_pr().is_none());
            assert!(state.pr_overlay().unwrap().error().is_some());
            assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        }
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Enter)),
            vec![Effect::OpenPullRequest {
                url: first.url().into()
            }]
        );
    }
}

#[test]
fn inbox_pr_focus_is_released_when_visiting_another_session() {
    let session = SessionId::new();
    let other = SessionId::new();
    let target = Target::Session(session);
    let mut state = AppState::home(WorkspaceId::new(), vec![session, other]);
    let pr = pr_link(2);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr.clone()],
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
    );
    assert!(state.pr_focus.is_some());
    let _ = update(&mut state, AppEvent::VisitSession(other));
    assert_eq!(state.active(), Some(other));
    assert!(state.pr_focus.is_none());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![pr],
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn inbox_pr_requests_can_replace_the_drawer_they_were_opened_from() {
    for focus in [
        WorkspaceDrawerFocus::Director,
        WorkspaceDrawerFocus::Terminal,
    ] {
        for failed in [false, true] {
            let session = SessionId::new();
            let target = Target::Session(session);
            let mut state = AppState::home(WorkspaceId::new(), vec![session]);
            state.workspace_drawer_focus = Some(focus);
            state.director_drawer_open = focus == WorkspaceDrawerFocus::Director;
            state.root_terminal_drawer_open = focus == WorkspaceDrawerFocus::Terminal;
            let _ = update(&mut state, AppEvent::VisitSession(session));
            assert!(state.workspace_drawer_open());
            let first = pr_link(1);
            let second = pr_link(2);
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::OpenPullRequest(second.identity.clone())),
            );
            let event = if failed {
                BackendEvent::PullRequestsError {
                    target,
                    error: SafeError {
                        message: SafeMessage::new("Unavailable"),
                        error_id: "failed".into(),
                    },
                }
            } else {
                BackendEvent::PullRequestsLoaded {
                    target,
                    revision: 1,
                    prs: vec![first, second.clone()],
                }
            };
            let _ = update(&mut state, AppEvent::Backend(event));
            assert_eq!(state.overlay(), Some(Overlay::Prs));
            if failed {
                assert!(state.pr_overlay().unwrap().error().is_some());
                assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
            } else {
                assert_eq!(
                    state.pr_overlay().unwrap().selected_pr().unwrap().identity,
                    second.identity
                );
            }
        }
    }
}

#[test]
fn inbox_pr_load_errors_keep_focus_until_a_newer_interaction() {
    let mut empty = AppState::home(WorkspaceId::new(), Vec::new());
    assert!(
        update(
            &mut empty,
            AppEvent::Key(AppKey::OpenPullRequest(pr_link(2).identity))
        )
        .is_empty()
    );
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let pr = pr_link(2);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: SafeError {
                message: SafeMessage::new("Unavailable"),
                error_id: "failed".into(),
            },
        }),
    );
    assert_eq!(
        state.pr_focus.as_ref().map(|(_, id)| id),
        Some(&pr.identity)
    );
    assert!(state.pr_overlay().unwrap().error().is_some());
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Decisions));
    assert!(state.pr_focus.is_none());
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenPullRequest(pr.identity.clone())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert!(state.pr_focus.is_none());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: SafeError {
                message: SafeMessage::new("Unavailable"),
                error_id: "failed".into(),
            },
        }),
    );
    assert!(state.pr_overlay().unwrap().error().is_some());
}
