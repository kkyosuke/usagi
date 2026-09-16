//! pointer の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn pointer_click_resolves_and_selects_each_sidebar_row() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = sized_home(workspace, vec![session], 100, 30);

    // Content begins after the two chrome rows: the session's three lines
    // (summary, change history, Agents — rows 2-4), then the action row
    // (row 5). There is no `main` row or root divider. Each click moves the
    // navigation cursor to that row.
    assert_eq!(
        click_at(&mut state, 5, 2, 0),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(
        click_at(&mut state, 5, 3, 1_000),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(
        click_at(&mut state, 5, 4, 2_000),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(click_at(&mut state, 5, 5, 3_000), Selection::NewSession);

    // A click below every rendered row selects nothing new: the cursor stays
    // where it last landed.
    let before = state.selected();
    let effects = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 8,
            at: std::time::Duration::from_millis(4_000),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.selected(), before);
    let _ = click_at(&mut state, 5, 9, 5_000);
    assert_eq!(state.selected(), before);
}

#[test]
fn pointer_click_outside_the_sidebar_body_is_inert() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // A pointer before the first resize has no geometry and cannot resolve.
    let mut ungeometried = AppState::home(workspace, vec![session]);
    assert!(
        update(
            &mut ungeometried,
            AppEvent::Pointer {
                column: 5,
                row: 2,
                at: std::time::Duration::ZERO,
            },
        )
        .is_empty()
    );
    assert_eq!(
        ungeometried.selected(),
        Selection::Target(Target::Session(session))
    );

    let mut state = sized_home(workspace, vec![session], 100, 30);
    let resting = Selection::Target(Target::Session(session));
    for (column, row) in [
        (90, 4), // right-pane column
        (5, 0),  // header row
        (5, 1),  // spacer row
    ] {
        let _ = click_at(&mut state, column, row, u64::from(row));
        assert_eq!(state.selected(), resting);
    }
    // Zero dimensions fall back to 80x24, so a mid-sidebar click still lands.
    let mut zeroed = sized_home(workspace, vec![session], 0, 0);
    assert_eq!(click_at(&mut zeroed, 5, 2, 0), resting);
    // A viewport at or under the chrome, and a click past the content
    // capacity, both resolve to nothing.
    let tiny = sized_home(workspace, vec![session], 100, 2);
    assert!(tiny.sidebar_selection_at(5, 2).is_none());
    let short = sized_home(workspace, vec![SessionId::new()], 100, 8);
    assert!(short.sidebar_selection_at(5, 7).is_none());
}

#[test]
fn pointer_click_handles_single_body_line_and_overflow() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // body_height == 1 (height 3): only the first row is addressable.
    let single = sized_home(workspace, vec![session], 100, 3);
    assert_eq!(
        single.sidebar_selection_at(5, 2),
        Some(Selection::Target(Target::Session(session)))
    );
    assert_eq!(single.sidebar_selection_at(5, 5), None);
    // A click past the single addressable body line resolves to nothing.
    let overflow = sized_home(workspace, vec![session], 100, 3);
    assert_eq!(overflow.sidebar_selection_at(5, 4), None);
}

#[test]
fn pointer_click_reaches_the_scrolled_viewport_tail() {
    let workspace = WorkspaceId::new();
    let sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
    // A short viewport cannot show every row at once. Moving the cursor to the
    // tail (`+ new session`) scrolls the list, and a click on the last body row
    // still resolves to the row the frame now shows there.
    let mut state = sized_home(workspace, sessions.clone(), 100, 10);
    for _ in 0..sessions.len() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    }
    assert_eq!(state.selected(), Selection::NewSession);
    // The short viewport scrolled to the tail. The mascot reserves the sidebar
    // foot, leaving three clickable body rows — not enough for a session's
    // three lines plus the action, so the frame shows the action alone on the
    // first of them (row 2).
    let hit = state
        .sidebar_selection_at(5, 2)
        .expect("the tail row is addressable once scrolled");
    assert_eq!(hit, Selection::NewSession);
    let _ = click_at(&mut state, 5, 2, 0);
    assert_eq!(state.selected(), Selection::NewSession);
}

#[test]
fn consumed_double_click_does_not_turn_a_third_press_into_activation() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let _ = click_at(&mut state, 5, 2, 1_000);
    let _ = click_at(&mut state, 5, 2, 1_100);
    assert_eq!(state.active(), Some(session));
    state.route = Route::Home(HomeMode::Switch);
    let _ = click_at(&mut state, 5, 2, 1_200);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
}

#[test]
fn pointer_click_is_inert_while_an_overlay_owns_the_surface() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    // Open the workspace Overview overlay, then click a background session row.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    let before = state.selected();
    state.pending_session_click = Some((session, std::time::Duration::from_millis(1_000)));
    let effects = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(1_100),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.selected(), before);
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pending_session_click.is_none());
    state.overlay = None;
    let _ = click_at(&mut state, 5, 2, 1_200);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));

    // The inline create form owns the same background pointer boundary.
    state.overlay = Some(Overlay::CreateSession);
    let _ = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(1_300),
        },
    );
    assert!(state.pending_session_click.is_none());
    state.overlay = None;
    let _ = click_at(&mut state, 5, 2, 1_400);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
}

#[test]
fn pointer_focus_moves_only_to_an_open_workspace_drawer() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    assert!(
        update(
            &mut state,
            AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Director),
        )
        .is_empty()
    );
    assert_eq!(state.workspace_drawer_focus(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    let _ = update(
        &mut state,
        AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Terminal),
    );
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );

    let _ = update(
        &mut state,
        AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Director),
    );
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert!(!state.root_terminal_full_height());
}

/// Everything else in the garden is a wake-up: consume the press, restore
/// the Home from before the screen saver, and change no target.
#[test]
fn clicking_beside_the_usagi_only_returns_home() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);
    let (selected, active) = (state.selected(), state.active());
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}
