//! garden の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn garden_shortcut_opens_without_replacing_a_front_surface() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    state.notice = Some(Notice::new("stale feedback"));
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(state.notice().is_none());

    state.overlay = None;
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
    assert_eq!(state.overlay(), None);
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));

    for overlay in [Overlay::Overview, Overlay::Closeup] {
        state.overlay = Some(overlay);
        assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
        assert_eq!(state.overlay(), Some(overlay));
    }
}

#[test]
fn overview_garden_opens_a_screen_saver_that_any_key_wakes() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(state.notice().is_none());

    // 最初の入力は wake-up として消費され Home へ戻る。Escape 専用ではなく、
    // 矢印や drawer を開く key も背面へ渡らない。
    for key in [
        AppKey::Escape,
        AppKey::Left,
        AppKey::Right,
        AppKey::Down,
        AppKey::ToggleDirectorDrawer,
        AppKey::OpenDirectorNew,
    ] {
        state.overlay = Some(Overlay::Garden);
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert_eq!(state.overlay(), None);
        assert!(!state.director_drawer_open());
    }

    state.overlay = Some(Overlay::Overview);
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden extra".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(
        state
            .notice()
            .is_some_and(|notice| notice.message.as_str().contains("takes no arguments"))
    );
}

#[test]
fn garden_click_and_arrow_keys_wake_the_overlay() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
    assert_eq!(state.overlay(), None);

    state.overlay = Some(Overlay::Garden);
    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn garden_list_scroll_keeps_home_selection_and_does_not_emit_effects() {
    let (workspace, a, b) = ids();
    let mut state = AppState::home(workspace, vec![a, b]);
    let selected = state.selected();
    let active = state.active();
    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Scroll { offset: 8 })
        )
        .is_empty()
    );
    assert_eq!(state.garden_sidebar_scroll(), 0);
    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Scroll { offset: 8 })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert_eq!(state.garden_sidebar_scroll(), 8);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.garden_sidebar_scroll(), 0);
}

#[test]
fn manual_garden_refuses_an_unavailable_layout_without_leaving_an_overlay() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    assert!(update(&mut state, AppEvent::GardenAvailability(false)).is_empty());
    state.overlay = Some(Overlay::Overview);

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert!(
        state
            .notice()
            .is_some_and(|notice| { notice.message.as_str().contains("at least 64 columns") })
    );
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), None);

    assert!(update(&mut state, AppEvent::GardenAvailability(true)).is_empty());
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(update(&mut state, AppEvent::GardenAvailability(false)).is_empty());
    assert_eq!(state.overlay(), None);
}

/// Just under the threshold nothing happens; reaching it opens the garden.
/// The reducer owns no clock, so the whole timer is one injected duration.
#[test]
fn the_garden_opens_only_once_home_reaches_the_idle_threshold() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);

    let almost = GARDEN_IDLE_THRESHOLD
        .checked_sub(std::time::Duration::from_millis(1))
        .expect("the threshold is longer than a millisecond");
    assert!(update(&mut state, AppEvent::IdleElapsed(almost)).is_empty());
    assert_eq!(state.overlay(), None);

    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));

    // Idle keeps being reported while the garden is up; that is inert rather
    // than a second open, and the route underneath is untouched.
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD * 4)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}

/// A Closeup with no overlay in front of it is eligible — including one
/// attached to a live terminal, which keeps running behind the garden.
#[test]
fn an_idle_closeup_is_still_eligible_for_the_garden() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    state.overlay = None;

    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    // The garden is a layer: the route and active target behind it are the
    // ones the wake-up returns to.
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.active(), Some(session));
}

/// Confirmations, form drafts, read-only surfaces, and the Director drawer
/// all stay in front: an unsent edit or a destructive prompt must never be
/// covered by a screen saver.
#[test]
fn a_front_surface_keeps_the_idle_garden_away() {
    let (workspace, session, _) = ids();
    for overlay in [
        Overlay::Overview,
        Overlay::Daemon,
        Overlay::Closeup,
        Overlay::QuitConfirmation,
        Overlay::Notes,
        Overlay::Environment,
        Overlay::Roles,
        Overlay::CreateSession,
        Overlay::Decisions,
        Overlay::Prs,
        Overlay::Preview,
        Overlay::CreateSessionError,
        Overlay::Garden,
    ] {
        let mut state = sized_home(workspace, vec![session], 100, 30);
        state.overlay = Some(overlay);
        let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        assert_eq!(
            state.overlay(),
            Some(overlay),
            "{overlay:?} was replaced by the garden"
        );
    }

    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(state.director_drawer_open());
    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(state.overlay(), None);
    assert!(state.director_drawer_open());
}

/// Resize arrives as a per-frame level, so only its edge closes the garden.
#[test]
fn a_resize_closes_the_garden_but_a_resampled_size_does_not() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    state.overlay = Some(Overlay::Garden);

    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 100,
            height: 30,
        },
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));

    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 120,
            height: 30,
        },
    );
    assert_eq!(state.overlay(), None);

    // A resize with no garden up is the plain size update it always was.
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 80,
            height: 24,
        },
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.size, Some((80, 24)));
}

#[test]
fn unavailable_garden_closes_with_visible_feedback() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 40, 10);
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::GardenUnavailable).is_empty());
    assert_eq!(state.overlay(), None);
    assert!(
        state
            .notice()
            .is_some_and(|notice| notice.message.contains("terminal size"))
    );
}

/// The press and the snapshot race. A session that left the workspace
/// between the frame and the click is a stale target: close the garden, run
/// nothing.
#[test]
fn a_click_on_a_vanished_usagi_only_closes_the_garden() {
    let (workspace, session, gone) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let (selected, active) = (state.selected(), state.active());
    state.overlay = Some(Overlay::Garden);

    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Visit {
                workspace,
                session: gone,
                agent: None,
            })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}

/// A click resolved against a frame the garden has since left behind must
/// not activate anything.
#[test]
fn a_garden_click_without_an_open_garden_is_inert() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let route = state.route();

    for click in [
        GardenClick::Visit {
            workspace,
            session,
            agent: None,
        },
        GardenClick::Dismiss,
    ] {
        assert!(update(&mut state, AppEvent::GardenClick(click)).is_empty());
        assert_eq!(state.overlay(), None);
        assert_eq!(state.route(), route);
    }
}
