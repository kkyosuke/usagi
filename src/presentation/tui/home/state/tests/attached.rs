use super::*;

// --- 没入 (Attached) ---------------------------------------------------

#[test]
fn attached_holds_a_terminal_view_and_leaving_drops_it() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    assert_eq!(state.mode(), Mode::Attached);
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ ".to_string()],
        Some((0, 2)),
    ));
    assert_eq!(state.terminal_view().unwrap().rows(), ["$ "]);
    // Leaving 没入 returns to 在席 and drops the snapshot.
    state.leave_attached();
    assert_eq!(state.mode(), Mode::Focus);
    assert!(state.terminal_view().is_none());
}

#[test]
fn clear_terminal_surface_drops_the_snapshot_without_changing_the_mode() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["x".to_string()], None));
    state.clear_terminal_surface();
    assert!(state.terminal_view().is_none());
    // The mode is untouched (the per-frame cleanup must not leave 没入).
    assert_eq!(state.mode(), Mode::Attached);
}

#[test]
fn tab_strip_is_published_and_cleared_with_the_view() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    let strip = state.terminal_tabs().expect("the strip is published");
    assert_eq!(strip.labels, ["agent", "terminal"]);
    assert_eq!(strip.active, 1);
    // The surface clears as a unit: a published view and tab strip drop together,
    // so there is no path that leaves a stale snapshot beside a dropped strip.
    state.set_terminal_view(TerminalView::from_rows(vec!["x".to_string()], None));
    state.clear_terminal_surface();
    assert!(state.terminal_view().is_none());
    assert!(state.terminal_tabs().is_none());
}

#[test]
fn surface_owner_claim_drops_the_previous_owners_snapshot() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    {
        let mut surface = state.surface_writer(SurfaceOwner::Attached);
        surface.set_tabs(vec!["agent".to_string()], 0);
        surface.set_view(TerminalView::from_rows(vec!["attached".to_string()], None));
    }
    assert_eq!(state.terminal_view().unwrap().rows(), ["attached"]);
    assert_eq!(
        state.terminal_tabs().expect("attached tabs are set").labels,
        ["agent"]
    );

    // A preview taking over clears the attached pane's screen before it publishes
    // anything of its own, so an event-loop preview can never be rendered with a
    // stale 没入 snapshot underneath it.
    state
        .surface_writer(SurfaceOwner::Preview)
        .set_tabs(vec!["terminal".to_string()], 0);
    assert!(state.terminal_view().is_none());
    assert_eq!(
        state.terminal_tabs().expect("preview tabs are set").labels,
        ["terminal"]
    );

    state
        .surface_writer(SurfaceOwner::Preview)
        .set_view(TerminalView::from_rows(vec!["preview".to_string()], None));
    assert_eq!(state.terminal_view().unwrap().rows(), ["preview"]);

    // And the same hand-off works in the other direction: the pane driver claims
    // the surface before setting its strip, dropping the event-loop preview's
    // snapshot until the live pane publishes a fresh frame.
    state
        .surface_writer(SurfaceOwner::Attached)
        .set_tabs(vec!["agent".to_string()], 0);
    assert!(state.terminal_view().is_none());
    assert_eq!(
        state.terminal_tabs().expect("attached tabs are set").labels,
        ["agent"]
    );
}

#[test]
fn badge_snapshot_is_replaced_through_an_owner_writer() {
    let mut state = state();
    let running = PathBuf::from("/repo/feature");
    let mut badges = MonitorSnapshot::default();
    badges.running.insert(running.clone());
    state.badge_writer(SurfaceOwner::Preview).apply(badges);
    assert!(state.badges().running.contains(&running));

    let waiting = PathBuf::from("/repo/fix");
    let mut next = MonitorSnapshot::default();
    next.waiting.insert(waiting.clone());
    state.badge_writer(SurfaceOwner::Attached).apply(next);
    assert!(!state.badges().running.contains(&running));
    assert!(state.badges().waiting.contains(&waiting));
}

#[test]
fn leaving_attached_drops_the_tab_strip() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.leave_attached();
    assert!(state.terminal_tabs().is_none());
}

#[test]
fn leave_focus_returns_to_base_switch_clearing_the_create_input() {
    let mut state = state();
    state.enter_focus(1);
    state.switch_begin_create(Vec::new());
    // Leaving 在席 returns to the base 切替 (via `enter_switch(Base)`), which clears
    // the inline create input. Re-entering 在席 later resets the prompt / menu.
    state.leave_focus();
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.switch_return(), ReturnMode::Base);
    assert!(!state.is_creating());
    // Re-entering 在席 resets the focus surface (prompt cleared, cursor at top).
    state.enter_focus(1);
    state.focus_prompt_mut().insert('x');
    state.enter_focus(1);
    assert_eq!(state.focus_prompt(), "");
    assert_eq!(state.focus_menu_cursor(), 0);
}

#[test]
fn focus_session_jumps_to_a_row_and_clamps_to_the_list() {
    let mut state = state(); // root (0), main (1), feature (2)
    state.focus_session(2);
    assert_eq!(state.list().selected_index(), 2);
    state.focus_session(0);
    assert!(state.list().root_selected());
    state.focus_session(99);
    assert_eq!(state.list().selected_index(), 2);
}

#[test]
fn apply_session_outcome_logs_and_rebuilds_the_pane_from_sessions() {
    let mut state = state();
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Created session \"x\""),
        sessions: Some(vec![session_record("main", 1), session_record("x", 1)]),
        select: Some("x".to_string()),
        root_note: None,
    });
    assert!(state.log().last().unwrap().text.contains("Created session"));
    assert_eq!(state.sessions().len(), 2);
    assert_eq!(state.list().worktrees().len(), 2);
    assert_eq!(state.list().workspace_name(), "usagi");
    assert!(state
        .list()
        .worktrees()
        .iter()
        .any(|w| w.branch.as_deref() == Some("x")));
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.list().active_index(), 2);

    // A refreshed list with no `select` rebuilds the pane but leaves the cursor
    // to fall back to the root row (the branch with `sessions: Some`, `select:
    // None`).
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Removed session \"x\""),
        sessions: Some(vec![session_record("main", 1)]),
        select: None,
        root_note: None,
    });
    assert_eq!(state.sessions().len(), 1);
    assert_eq!(state.list().worktrees().len(), 1);
    assert_eq!(state.list().selected_index(), 0);

    // A failure outcome only logs; the pane is unchanged.
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::error("session failed"),
        sessions: None,
        select: None,
        root_note: None,
    });
    assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
    assert_eq!(state.list().worktrees().len(), 1);
    assert_eq!(state.sessions().len(), 1);
}

#[test]
fn set_tasks_round_trips_the_panel_rows() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut state = state();
    assert!(state.tasks().is_empty());
    let rows = vec![
        TaskRow {
            label: "作成中… x".to_string(),
            mark: TaskMark::Running(2),
        },
        TaskRow {
            label: "削除完了 y".to_string(),
            mark: TaskMark::Done(true),
        },
    ];
    state.set_tasks(rows.clone());
    assert_eq!(state.tasks(), rows.as_slice());
}

#[test]
fn apply_task_completion_logs_and_refreshes_keeping_the_cursor() {
    let mut state = state();
    // Restore two sessions and move the cursor onto the second one.
    state.restore_sessions(vec![
        session_record("main", 1),
        session_record("feature", 1),
    ]);
    state.switch_move_down();
    state.switch_move_down();
    let selected = state.list().selected_name().to_string();
    assert_eq!(selected, "feature");

    // A finished background create refreshes the list with a new session; the
    // cursor stays on "feature" rather than snapping back to the root row.
    state.apply_task_completion(
        LogLine::output("Created session \"x\" 󰤇"),
        Some(vec![
            session_record("main", 1),
            session_record("feature", 1),
            session_record("x", 1),
        ]),
        None,
    );
    assert!(state.log().last().unwrap().text.contains("Created session"));
    assert_eq!(state.sessions().len(), 3);
    assert_eq!(state.list().selected_name(), "feature");

    // A failure (no refreshed list) only logs; the pane is untouched.
    state.apply_task_completion(LogLine::error("session remove failed"), None, None);
    assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
    assert_eq!(state.sessions().len(), 3);
}

#[test]
fn multi_repo_session_collapses_to_one_row_with_an_aggregated_status() {
    // A session spanning three repositories: two synced, one still local.
    let mut merged_a = worktree("feature");
    merged_a.path = PathBuf::from("/repo/.usagi/sessions/feature/app-a");
    merged_a.primary = true;
    merged_a.status = BranchStatus::Synced;
    merged_a.upstream = Some("origin/feature".to_string());
    let mut merged_b = worktree("feature");
    merged_b.path = PathBuf::from("/repo/.usagi/sessions/feature/app-b");
    merged_b.status = BranchStatus::Synced;
    let mut local_c = worktree("feature");
    local_c.path = PathBuf::from("/repo/.usagi/sessions/feature/app-c");
    local_c.status = BranchStatus::Local;

    let mut state = state();
    state.restore_sessions(vec![SessionRecord {
        name: "feature".to_string(),
        display_name: None,
        note: None,
        label_id: None,
        root: PathBuf::from("/repo/.usagi/sessions/feature"),
        worktrees: vec![merged_a, merged_b, local_c],
        created_at: Utc::now(),
        last_active: None,
    }]);

    // The three repositories collapse into a single row.
    assert_eq!(state.list().worktrees().len(), 1);
    let row = &state.list().worktrees()[0];
    assert_eq!(row.branch.as_deref(), Some("feature"));
    // Keyed on the session tree root (not any single repository's worktree).
    assert_eq!(row.path, PathBuf::from("/repo/.usagi/sessions/feature"));
    // Least-progressed wins: one local repo keeps the whole session `local`.
    assert_eq!(row.status, BranchStatus::Local);
    // Primary is set because one repository's worktree is primary.
    assert!(row.primary);
    // Representative detail comes from the first repository.
    assert_eq!(row.upstream.as_deref(), Some("origin/feature"));
}

#[test]
fn a_session_with_no_worktrees_still_yields_a_row() {
    let mut state = state();
    state.restore_sessions(vec![SessionRecord {
        name: "empty".to_string(),
        display_name: None,
        note: None,
        label_id: None,
        root: PathBuf::from("/repo/.usagi/sessions/empty"),
        worktrees: Vec::new(),
        created_at: Utc::now(),
        last_active: None,
    }]);
    assert_eq!(state.list().worktrees().len(), 1);
    let row = &state.list().worktrees()[0];
    assert_eq!(row.branch.as_deref(), Some("empty"));
    // No repositories: the empty aggregate is `new` (least-progressed), no
    // primary, no upstream, and an empty representative head.
    assert_eq!(row.status, BranchStatus::New);
    assert!(!row.primary);
    assert!(row.upstream.is_none());
    assert!(row.head.is_empty());
}

#[test]
fn refresh_sessions_updates_statuses_and_keeps_the_cursor_in_place() {
    let mut state = state();
    // Create alpha + beta and land the cursor / active row on beta.
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("created"),
        sessions: Some(vec![session_record("alpha", 1), session_record("beta", 1)]),
        select: Some("beta".to_string()),
        root_note: None,
    });
    assert_eq!(state.list().selected_index(), 2); // root, alpha, beta
    assert_eq!(state.list().active_name(), "beta");

    // Re-sync: beta's branch is now synced (it was local). The cursor and the
    // active row must stay on beta, and its row must show the new status.
    let mut beta = session_record("beta", 1);
    beta.worktrees[0].status = BranchStatus::Synced;
    state.refresh_sessions(vec![session_record("alpha", 1), beta]);
    assert_eq!(state.list().selected_name(), "beta");
    assert_eq!(state.list().active_name(), "beta");
    assert_eq!(state.list().worktrees()[1].status, BranchStatus::Synced);

    // A refresh that drops the selected session falls back to the root row
    // (no panic, no stale cursor).
    state.refresh_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(state.list().selected_name(), ROOT_NAME);
    assert_eq!(state.list().active_name(), ROOT_NAME);
}

#[test]
fn refresh_sessions_keeps_the_switch_cursor_on_the_create_row() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    state.enter_switch(ReturnMode::Base);
    state.switch_select(state.list().create_row());
    assert!(state.list().create_row_selected());

    // The persistent "+ new session" row is not a session name, so preserving
    // only by selected_name() would collapse it to ROOT_NAME. Keep the cursor on
    // the affordance while background refreshes replace the real rows above it.
    state.refresh_sessions(vec![
        session_record("alpha", 1),
        session_record("beta", 1),
        session_record("gamma", 1),
    ]);
    assert!(state.list().create_row_selected());
}

#[test]
fn refresh_sessions_normalizes_a_corrupt_active_create_row_to_root() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.enter_switch(ReturnMode::Base);
    // The active row is command-facing and should never point at the create
    // affordance, but older/corrupt state must be normalized safely if it does.
    state.list.activate_index(state.list.create_row());

    state.refresh_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(state.list().active_index(), 0);
    assert_eq!(state.list().active_name(), ROOT_NAME);
}

#[test]
fn refresh_sessions_keeps_the_switch_cursor_on_an_extra_unite_root() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.set_extra_groups(vec![GroupSource {
        name: "tools".to_string(),
        root_path: PathBuf::from("/tools"),
        root_note: None,
        sessions: vec![session_record("beta", 1)],
    }]);
    state.enter_switch(ReturnMode::Base);
    state.switch_select(2); // primary root, alpha, then tools' root row.
    assert_eq!(state.list().selected_group(), 1);
    assert!(state.list().root_selected());

    // A primary-workspace re-sync landing while the user is in 切替 must not
    // resolve the ambiguous `ROOT_NAME` to the first workspace's root row.
    state.refresh_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.list().selected_group(), 1);
    assert!(state.list().root_selected());
}

#[test]
fn refresh_sessions_keeps_the_switch_cursor_in_the_same_unite_group_on_duplicate_names() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.set_extra_groups(vec![GroupSource {
        name: "tools".to_string(),
        root_path: PathBuf::from("/tools"),
        root_note: None,
        sessions: vec![session_record("alpha", 1)],
    }]);
    state.enter_switch(ReturnMode::Base);
    state.switch_select(3); // primary root, primary alpha, tools root, tools alpha.
    assert_eq!(state.list().selected_group(), 1);
    assert_eq!(state.list().selected_name(), "alpha");

    // The branch name is present in both workspaces; preserving only by name
    // would jump to the primary group's alpha (row 1). The cursor should stay on
    // the extra group's alpha row.
    state.refresh_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(state.list().selected_index(), 3);
    assert_eq!(state.list().selected_group(), 1);
    assert_eq!(state.list().selected_name(), "alpha");
}

#[test]
fn open_remove_modal_lists_the_session_names() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    assert!(state.remove_modal().is_none());
    state.open_remove_modal(false);
    let modal = state.remove_modal().unwrap();
    let names: Vec<&str> = modal.entries().iter().map(|entry| entry.name()).collect();
    let labels: Vec<&str> = modal
        .entries()
        .iter()
        .map(|entry| entry.display())
        .collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(labels, ["alpha", "beta"]);
    assert_eq!(modal.cursor(), 0);
    assert_eq!(modal.selected_count(), 0);
    assert!(!modal.is_empty());
    assert!(!modal.is_selected(0));
}

#[test]
fn open_remove_modal_lists_all_united_sessions_with_workspace_prefixes() {
    let mut state = state();
    state.set_root_path("/repo/usagi");
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.set_extra_groups(vec![GroupSource {
        name: "tools".to_string(),
        root_path: "/repo/tools".into(),
        root_note: None,
        sessions: vec![session_record("alpha", 1), session_record("beta", 1)],
    }]);

    state.open_remove_modal(false);
    let modal = state.remove_modal().unwrap();
    let labels: Vec<&str> = modal
        .entries()
        .iter()
        .map(|entry| entry.display())
        .collect();
    assert_eq!(labels, ["usagi: alpha", "tools: alpha", "tools: beta"]);
    assert_eq!(
        modal.entries()[0].root_path(),
        &PathBuf::from("/repo/usagi")
    );
    assert_eq!(
        modal.entries()[1].root_path(),
        &PathBuf::from("/repo/tools")
    );
}

#[test]
fn remove_modal_cursor_wraps_in_both_directions() {
    let mut state = state();
    state.restore_sessions(vec![
        session_record("a", 1),
        session_record("b", 1),
        session_record("c", 1),
    ]);
    state.open_remove_modal(false);
    state.remove_modal_mut().unwrap().move_down();
    assert_eq!(state.remove_modal().unwrap().cursor(), 1);
    state.remove_modal_mut().unwrap().move_up();
    state.remove_modal_mut().unwrap().move_up();
    assert_eq!(state.remove_modal().unwrap().cursor(), 2);
    state.remove_modal_mut().unwrap().move_down();
    assert_eq!(state.remove_modal().unwrap().cursor(), 0);
}

#[test]
fn remove_modal_toggle_checks_and_unchecks_the_cursor_row() {
    let mut state = state();
    state.restore_sessions(vec![session_record("a", 1), session_record("b", 1)]);
    state.open_remove_modal(false);
    state.remove_modal_mut().unwrap().toggle();
    state.remove_modal_mut().unwrap().move_down();
    state.remove_modal_mut().unwrap().toggle();
    let modal = state.remove_modal().unwrap();
    assert!(modal.is_selected(0));
    assert!(modal.is_selected(1));
    assert_eq!(modal.selected_count(), 2);
    state.remove_modal_mut().unwrap().toggle();
    assert!(!state.remove_modal().unwrap().is_selected(1));
}

#[test]
fn remove_modal_navigation_is_a_noop_when_empty_or_closed() {
    let mut state = state();
    state.open_remove_modal(false);
    assert!(state.remove_modal().unwrap().is_empty());
    // Open but empty: the modal's own navigation is a no-op.
    state.remove_modal_mut().unwrap().move_up();
    state.remove_modal_mut().unwrap().move_down();
    state.remove_modal_mut().unwrap().toggle();
    assert_eq!(state.remove_modal().unwrap().cursor(), 0);
    assert_eq!(state.remove_modal().unwrap().selected_count(), 0);

    // Closed: there is no modal to navigate, and confirm returns None.
    state.cancel_remove_modal();
    assert!(state.remove_modal().is_none());
    assert!(state.remove_modal_mut().is_none());
    assert!(state.submit_remove_modal().is_none());
}

#[test]
fn submit_remove_modal_returns_checked_names_in_order_and_closes() {
    let mut state = state();
    state.restore_sessions(vec![
        session_record("a", 1),
        session_record("b", 1),
        session_record("c", 1),
    ]);
    state.open_remove_modal(true);
    state.remove_modal_mut().unwrap().move_down();
    state.remove_modal_mut().unwrap().move_down();
    state.remove_modal_mut().unwrap().toggle(); // "c"
    state.remove_modal_mut().unwrap().move_up();
    state.remove_modal_mut().unwrap().move_up();
    state.remove_modal_mut().unwrap().toggle(); // "a"
    let (entries, force) = state.submit_remove_modal().unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name()).collect();
    assert_eq!(names, ["a", "c"]);
    assert!(force);
    assert!(state.remove_modal().is_none());
}

#[test]
fn submit_remove_modal_with_nothing_checked_keeps_it_open() {
    let mut state = state();
    state.restore_sessions(vec![session_record("a", 1)]);
    state.open_remove_modal(false);
    assert!(state.submit_remove_modal().is_none());
    assert!(state.remove_modal().is_some());
}

#[test]
fn log_output_and_error_append_lines() {
    let mut state = state();
    state.log_output("did a thing");
    state.log_error("it broke");
    let last_two: Vec<_> = state.log().iter().rev().take(2).collect();
    assert_eq!(last_two[0].kind, LineKind::Error);
    assert_eq!(last_two[0].text, "it broke");
    assert_eq!(last_two[1].kind, LineKind::Output);
    assert_eq!(last_two[1].text, "did a thing");
}

#[test]
fn log_error_persists_through_the_injected_logger() {
    let (mut state, spy) = state_with_spy();
    // An operation failure is both shown on screen and recorded to the sink.
    state.log_error("preview failed: no such file");
    assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
    assert_eq!(
        spy.recorded.borrow().as_slice(),
        ["preview failed: no such file"]
    );
    // An ordinary output line is shown only, never recorded.
    state.log_output("did a thing");
    assert_eq!(spy.recorded.borrow().len(), 1);
}

#[test]
fn input_mistakes_are_shown_but_not_recorded() {
    // Unknown-command / usage errors come back as command-result error lines via
    // `submit` (not `log_error`), so they reach the screen as red notices but are
    // never written to the daily log — the file keeps only real failures.
    let (mut state, spy) = state_with_spy();
    state.push_char('n');
    state.push_char('o');
    state.push_char('p');
    state.push_char('e');
    state.submit();
    assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
    assert!(spy.recorded.borrow().is_empty());
}

#[test]
fn applied_failure_lines_are_recorded_success_lines_are_not() {
    let (mut state, spy) = state_with_spy();
    // A background task / session outcome that succeeded only logs its line.
    state.apply_task_completion(LogLine::output("Created session \"x\" 󰤇"), None, None);
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Renamed \"x\""),
        sessions: None,
        select: None,
        root_note: None,
    });
    assert!(spy.recorded.borrow().is_empty());

    // A failure line from either path is persisted through the sink.
    state.apply_task_completion(LogLine::error("session remove failed: boom"), None, None);
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::error("rename failed: locked"),
        sessions: None,
        select: None,
        root_note: None,
    });
    assert_eq!(
        spy.recorded.borrow().as_slice(),
        ["session remove failed: boom", "rename failed: locked"]
    );
}

#[test]
fn apply_badges_replaces_every_set_at_once() {
    let mut state = state();
    // Every set starts empty.
    assert!(!state.is_running(Path::new("/repo/run")));
    assert!(state.running_paths().is_empty());
    assert!(state.waiting_paths().is_empty());
    assert!(state.live_paths().is_empty());
    assert!(state.done_paths().is_empty());

    // The accessor a render loop compares against starts at the empty snapshot.
    assert_eq!(state.badges(), &MonitorSnapshot::default());

    // One reading populates all four sets together (running / waiting / live /
    // done), so the getters read a single consistent snapshot.
    let snapshot = MonitorSnapshot {
        running: [PathBuf::from("/repo/run")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        live: [PathBuf::from("/repo/run"), PathBuf::from("/repo/wait")].into(),
        done: [PathBuf::from("/repo/done")].into(),
        ..Default::default()
    };
    state.apply_badges(snapshot.clone());
    // `badges` echoes the whole applied snapshot, so a loop can detect a change
    // since its last paint by comparing against it.
    assert_eq!(state.badges(), &snapshot);
    assert!(state.is_running(Path::new("/repo/run")));
    assert!(!state.is_running(Path::new("/repo/wait")));
    assert_eq!(state.running_paths().len(), 1);
    assert!(state.is_waiting(Path::new("/repo/wait")));
    assert_eq!(state.waiting_paths().len(), 1);
    assert!(state.is_live(Path::new("/repo/run")));
    assert!(state.is_live(Path::new("/repo/wait")));
    assert_eq!(state.live_paths().len(), 2);
    assert!(state.is_done(Path::new("/repo/done")));
    assert_eq!(state.done_paths().len(), 1);

    // A fresh reading replaces the lot — a now-empty set clears, it does not
    // merge with the previous frame.
    state.apply_badges(MonitorSnapshot::default());
    assert!(!state.is_running(Path::new("/repo/run")));
    assert!(state.running_paths().is_empty());
    assert!(state.waiting_paths().is_empty());
    assert!(state.live_paths().is_empty());
    assert!(state.done_paths().is_empty());
}

#[test]
fn update_holds_the_latest_release_once_set() {
    use crate::domain::version::Version;
    let mut state = state();
    assert!(state.update().is_none());
    let latest = Version::parse("0.2.0");
    state.set_update(latest);
    assert_eq!(state.update(), latest);
    state.set_update(None);
    assert!(state.update().is_none());
}

#[test]
fn has_live_sessions_and_live_count_follow_the_live_set() {
    let mut state = state();
    assert!(!state.has_live_sessions());
    assert_eq!(state.live_count(), 0);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/feature"), PathBuf::from("/repo/main")].into(),
        ..Default::default()
    });
    assert!(state.has_live_sessions());
    assert_eq!(state.live_count(), 2);
}

/// The branch names of the left-pane rows in display order (the synthetic root
/// row carries no branch and is skipped), for asserting the sessions' ordering.
fn row_names(state: &HomeState) -> Vec<String> {
    state
        .list()
        .worktrees()
        .iter()
        .filter_map(|w| w.branch.clone())
        .collect()
}

/// A badge snapshot whose waiting set holds the given sessions (keyed by the
/// `session_record` root path), leaving the other sets empty.
fn waiting_snapshot(names: &[&str]) -> MonitorSnapshot {
    MonitorSnapshot {
        waiting: names
            .iter()
            .map(|n| PathBuf::from(format!("/repo/.usagi/sessions/{n}")))
            .collect(),
        ..Default::default()
    }
}

#[test]
fn sort_waiting_is_off_by_default_and_keeps_the_canonical_order() {
    let mut state = state();
    state.restore_sessions(vec![
        session_record("alpha", 1),
        session_record("beta", 1),
        session_record("gamma", 1),
    ]);
    assert!(!state.sort_waiting());
    // Even with beta waiting, the order is untouched while the sort is off.
    state.apply_badges(waiting_snapshot(&["beta"]));
    assert_eq!(row_names(&state), ["alpha", "beta", "gamma"]);
}

#[test]
fn toggle_sort_waiting_lifts_waiting_sessions_to_the_top_then_restores() {
    let mut state = state();
    state.restore_sessions(vec![
        session_record("alpha", 1),
        session_record("beta", 1),
        session_record("gamma", 1),
    ]);
    // beta and gamma are waiting; alpha is not.
    state.apply_badges(waiting_snapshot(&["beta", "gamma"]));
    // Land the cursor on beta so we can confirm it follows its row.
    state.switch_move_down();
    state.switch_move_down();
    assert_eq!(state.list().selected_name(), "beta");

    // Toggling on lifts beta + gamma above alpha, each group keeping its canonical
    // order (a stable partition), and the cursor follows beta to the top.
    state.toggle_sort_waiting();
    assert!(state.sort_waiting());
    assert_eq!(row_names(&state), ["beta", "gamma", "alpha"]);
    assert_eq!(state.list().selected_name(), "beta");

    // Toggling off restores the canonical order, cursor still on beta.
    state.toggle_sort_waiting();
    assert!(!state.sort_waiting());
    assert_eq!(row_names(&state), ["alpha", "beta", "gamma"]);
    assert_eq!(state.list().selected_name(), "beta");
}

#[test]
fn apply_badges_resorts_only_when_the_waiting_set_moves_under_the_sort() {
    let mut state = state();
    state.restore_sessions(vec![
        session_record("alpha", 1),
        session_record("beta", 1),
        session_record("gamma", 1),
    ]);
    state.toggle_sort_waiting();
    // No one waiting yet, so the order is still canonical.
    assert_eq!(row_names(&state), ["alpha", "beta", "gamma"]);

    // gamma starts waiting: it rises to the top on the next badge reading.
    state.apply_badges(waiting_snapshot(&["gamma"]));
    assert_eq!(row_names(&state), ["gamma", "alpha", "beta"]);

    // Re-applying the same waiting set is a no-op for the order (no needless
    // rebuild), and the rows stay put.
    state.apply_badges(waiting_snapshot(&["gamma"]));
    assert_eq!(row_names(&state), ["gamma", "alpha", "beta"]);

    // gamma stops waiting: it falls back to its canonical place.
    state.apply_badges(MonitorSnapshot::default());
    assert_eq!(row_names(&state), ["alpha", "beta", "gamma"]);
}

#[test]
fn quit_confirm_opens_and_cancels() {
    let mut state = state();
    assert!(!state.quit_confirm());
    state.open_quit_confirm();
    assert!(state.quit_confirm());
    state.cancel_quit_confirm();
    assert!(!state.quit_confirm());
}

#[test]
fn resume_level_reads_off_the_current_mode_when_nothing_is_armed() {
    // 切替 (the default) records Switch.
    let mut switch = state();
    assert_eq!(switch.resume_level(), ResumeLevel::Switch);
    // 在席 records Focus.
    let mut focus = state();
    focus.enter_focus(1);
    assert_eq!(focus.resume_level(), ResumeLevel::Focus);
}

#[test]
fn arming_attached_overrides_the_mode_for_one_quit() {
    // A 没入 quit drops to 在席 before the modal, so the level is armed beforehand;
    // it then wins over the (now Focus) mode, and is consumed once.
    let mut state = state();
    state.enter_focus(1);
    state.arm_resume_attached();
    assert_eq!(state.resume_level(), ResumeLevel::Attached);
    // Consumed: a second read falls back to the current mode.
    assert_eq!(state.resume_level(), ResumeLevel::Focus);
}

#[test]
fn cancelling_the_quit_modal_drops_an_armed_level() {
    // Arming then cancelling the modal (the user backed out of a 没入 Ctrl-Q) must
    // not leave the Attached arm to mislabel a later, shallower quit.
    let mut state = state();
    state.arm_resume_attached();
    state.cancel_quit_confirm();
    assert_eq!(state.resume_level(), ResumeLevel::Switch);
}

#[test]
fn restore_focus_switch_moves_the_cursor_without_focusing() {
    let mut state = state(); // root, main, feature
    state.restore_focus("feature", ResumeLevel::Switch);
    // The cursor lands on the session, but the screen stays in 切替.
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.list().selected_name(), "feature");
    assert!(!state.take_resume_attach());
}

#[test]
fn restore_focus_focus_enters_the_session_without_arming_attach() {
    let mut state = state();
    state.restore_focus("feature", ResumeLevel::Focus);
    assert_eq!(state.mode(), Mode::Focus);
    assert_eq!(state.focused_session_name(), "feature");
    assert!(!state.take_resume_attach());
}

#[test]
fn restore_focus_attached_focuses_and_arms_a_one_shot_attach() {
    let mut state = state();
    state.restore_focus("feature", ResumeLevel::Attached);
    // Focused synchronously; the attach is armed for the event loop's first pass.
    assert_eq!(state.mode(), Mode::Focus);
    assert_eq!(state.focused_session_name(), "feature");
    assert!(state.take_resume_attach());
    // Consumed: only one attach is performed.
    assert!(!state.take_resume_attach());
}

#[test]
fn restore_focus_is_a_no_op_for_a_since_removed_session() {
    let mut state = state();
    state.restore_focus("gone", ResumeLevel::Attached);
    // No matching row: the screen opens in the default 切替 with nothing armed.
    assert_eq!(state.mode(), Mode::Switch);
    assert!(state.list().root_selected());
    assert!(!state.take_resume_attach());
}

#[test]
fn open_preview_result_renders_a_loaded_file_and_titles_it() {
    let mut state = state();
    assert!(state.preview().is_none());
    state.open_preview_result(Ok(("README.md".to_string(), "# Hi\nbody".to_string())));
    let preview = state.preview().expect("preview is open");
    assert_eq!(preview.title, "README.md");
    // The contents were rendered to Markdown lines (heading + body).
    assert_eq!(preview.lines.len(), 2);
    assert_eq!(preview.lines[0].plain_text(), "Hi");
    assert_eq!(preview.scroll, 0);
}

#[test]
fn open_preview_result_logs_a_failed_load_and_opens_nothing() {
    let mut state = state();
    state.open_preview_result(Err(anyhow::anyhow!("no such file")));
    assert!(state.preview().is_none());
    let last = state.log().last().unwrap();
    assert_eq!(last.kind, LineKind::Error);
    assert!(last.text.contains("preview failed"));
    assert!(last.text.contains("no such file"));
}

#[test]
fn preview_scrolls_within_bounds_and_closes() {
    let mut state = state();
    let body = (0..10)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    state.open_preview_result(Ok(("doc.md".to_string(), body)));

    // Up at the top is a no-op (saturating).
    state.preview_scroll_up();
    assert_eq!(state.preview().unwrap().scroll, 0);

    // Down advances, but clamps so the last line stays in view (10 lines, a
    // 4-row window -> max scroll 6).
    for _ in 0..20 {
        state.preview_scroll_down(4);
    }
    assert_eq!(state.preview().unwrap().scroll, 6);

    state.preview_scroll_up();
    assert_eq!(state.preview().unwrap().scroll, 5);

    state.close_preview();
    assert!(state.preview().is_none());
}

#[test]
fn preview_scrolling_is_a_no_op_when_no_preview_is_open() {
    let mut state = state();
    // With nothing open, the scroll helpers do nothing and open nothing.
    state.preview_scroll_up();
    state.preview_scroll_down(5);
    assert!(state.preview().is_none());
}

#[test]
fn open_diff_result_parses_the_patch_into_the_diff_view() {
    let mut state = state();
    assert!(state.diff_view().is_none());
    let patch = "diff --git a/f b/f\n@@ -1 +1 @@\n-old\n+new".to_string();
    state.open_diff_result(Ok(("feature → main".to_string(), patch)));
    // The diff view is titled by branch → base, starts unified at the top, and
    // holds the parsed rows (file header + hunk + del + add).
    let view = state.diff_view().expect("diff view is open");
    assert_eq!(view.title, "feature → main");
    assert!(!view.split);
    assert_eq!(view.scroll, 0);
    assert_eq!(view.doc.rows.len(), 4);
    assert!(!view.doc.is_empty());

    // Scrolling and toggling the layout run through the view's own helpers.
    state.diff_scroll_down(2);
    assert_eq!(state.diff_view().unwrap().scroll, 1);
    state.diff_scroll_up();
    assert_eq!(state.diff_view().unwrap().scroll, 0);
    state.diff_toggle_split();
    assert!(state.diff_view().unwrap().split);
    // Scrolling in the split layout clamps against the folded (split) row count.
    state.diff_scroll_down(1);
    state.close_diff();
    assert!(state.diff_view().is_none());
}

#[test]
fn open_diff_result_reports_an_empty_patch_as_no_changes() {
    let mut state = state();
    state.open_diff_result(Ok(("main → main".to_string(), String::new())));
    let view = state.diff_view().expect("diff view is open");
    assert!(view.doc.is_empty());
}

#[test]
fn open_diff_result_logs_a_failure_and_opens_nothing() {
    let mut state = state();
    state.open_diff_result(Err(anyhow::anyhow!("highlight a session")));
    assert!(state.diff_view().is_none());
    let last = state.log().last().unwrap();
    assert_eq!(last.kind, LineKind::Error);
    assert!(last.text.contains("diff failed"));
    assert!(last.text.contains("highlight a session"));
}

#[test]
fn diff_scroll_and_toggle_are_no_ops_when_closed() {
    let mut state = state();
    state.diff_scroll_up();
    state.diff_scroll_down(5);
    state.diff_toggle_split();
    assert!(state.diff_view().is_none());
}

// --- session freshness ("heat") dot --------------------------------------

/// A monitor snapshot marking the named session roots as actively running, for
/// the heat-bump tests.
fn running_snapshot(names: &[&str]) -> MonitorSnapshot {
    MonitorSnapshot {
        running: names
            .iter()
            .map(|n| PathBuf::from(format!("/repo/.usagi/sessions/{n}")))
            .collect(),
        ..Default::default()
    }
}

#[test]
fn changed_roots_reports_paths_that_entered_or_left_any_set() {
    let p = |n: &str| PathBuf::from(format!("/r/{n}"));
    let old = MonitorSnapshot {
        running: [p("a")].into_iter().collect(),
        ..Default::default()
    };
    let new = MonitorSnapshot {
        running: [p("a")].into_iter().collect(), // a unchanged → not reported
        done: [p("b")].into_iter().collect(),    // b entered the done set
        ..Default::default()
    };
    let changed = changed_roots(&old, &new);
    assert!(changed.contains(&p("b")));
    assert!(!changed.contains(&p("a")));
}

#[test]
fn apply_badges_bumps_last_active_for_sessions_whose_activity_changed() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    // Both start untouched: heat falls back to creation time.
    assert!(state.sessions().iter().all(|s| s.last_active.is_none()));

    // alpha starts running — only its freshness is stamped.
    state.apply_badges(running_snapshot(&["alpha"]));
    let alpha = state.sessions().iter().find(|s| s.name == "alpha").unwrap();
    let beta = state.sessions().iter().find(|s| s.name == "beta").unwrap();
    assert!(alpha.last_active.is_some());
    assert!(beta.last_active.is_none());
}

#[test]
fn apply_badges_ignores_activity_on_paths_with_no_session() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // A running path matching no recorded session bumps nothing (the
    // `bump_last_active` miss path) and triggers no rebuild.
    state.apply_badges(running_snapshot(&["ghost"]));
    assert!(state.sessions().iter().all(|s| s.last_active.is_none()));
}

#[test]
fn entering_focus_touches_the_active_session() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    // Row 1 is the first session (row 0 is the workspace root).
    state.enter_focus(1);
    assert!(state.sessions()[0].last_active.is_some());
    assert!(state.sessions()[1].last_active.is_none());
}

#[test]
fn focusing_the_root_row_touches_no_session() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // The root row belongs to no session, so focusing it stamps nothing.
    state.enter_focus(0);
    assert!(state.sessions().iter().all(|s| s.last_active.is_none()));
}

#[test]
fn last_active_flush_collects_only_touched_sessions() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    // Nothing touched yet → nothing to flush.
    assert!(state.last_active_flush().is_empty());

    state.apply_badges(running_snapshot(&["alpha"]));
    let flush = state.last_active_flush();
    assert_eq!(flush.len(), 1);
    assert_eq!(flush[0].0, "alpha");
}
