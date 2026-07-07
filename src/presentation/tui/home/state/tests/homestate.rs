use super::*;
use crate::presentation::tui::home::tasks::TaskKind;

/// A minimal recorded session rooted at `/repo/.usagi/sessions/<name>`, used to
/// give the primary workspace real rows that survive a rebuild.
fn session(name: &str) -> crate::domain::workspace_state::SessionRecord {
    crate::domain::workspace_state::SessionRecord {
        name: name.to_string(),
        display_name: None,
        note: None,
        label_id: None,
        agent: Default::default(),
        origin: Default::default(),
        started_from: None,
        root: PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
        worktrees: Vec::new(),
        created_at: Utc::now(),
        last_active: None,
    }
}

#[test]
fn set_extra_groups_stacks_other_workspaces_below_the_primary() {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.restore_sessions(vec![session("main"), session("feature")]);
    assert!(!state.is_united());
    assert_eq!(state.list().group_count(), 1);
    // primary: root + 2 sessions
    assert_eq!(state.list().session_count(), 3);

    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![session("b1")],
        issues: Vec::new(),
    }]);
    assert!(state.is_united());
    assert_eq!(state.list().group_count(), 2);
    assert_eq!(state.list().groups()[1].name(), "wsB");
    // primary(root+2) + extra(root+1) = 5
    assert_eq!(state.list().session_count(), 5);

    // A live re-sync of the primary keeps the extra group appended below it.
    state.refresh_sessions(vec![session("main")]);
    assert_eq!(state.list().group_count(), 2);
    assert_eq!(state.list().groups()[0].worktrees().len(), 1);
    assert_eq!(state.list().groups()[1].worktrees().len(), 1);

    // Clearing the extra groups restores the single-workspace view.
    state.set_extra_groups(Vec::new());
    assert!(!state.is_united());
    assert_eq!(state.list().group_count(), 1);
}

fn united_state() -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path("/usagi");
    state.restore_sessions(vec![session("main")]);
    state.restore_root_note(Some("primary note".to_string()));
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: Some("b note".to_string()),
        sessions: vec![session("b1")],
        issues: Vec::new(),
    }]);
    state
}

#[test]
fn add_and_remove_extra_groups_drive_unite_mode() {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path("/usagi");
    state.restore_sessions(vec![session("main")]);
    assert!(!state.is_united());

    let wsb = || GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![session("b1")],
        issues: Vec::new(),
    };
    // Adding a workspace enters unite mode.
    assert!(state.add_extra_group(wsb()));
    assert!(state.is_united());
    assert_eq!(state.list().group_count(), 2);
    assert_eq!(state.united_workspace_names(), vec!["usagi", "wsB"]);
    // Adding the same workspace again (same root) is refused.
    assert!(!state.add_extra_group(wsb()));
    // Adding the primary's own root is refused too.
    assert!(!state.add_extra_group(GroupSource {
        name: "dup".to_string(),
        root_path: PathBuf::from("/usagi"),
        root_note: None,
        sessions: Vec::new(),
        issues: Vec::new(),
    }));
    assert_eq!(state.list().group_count(), 2);

    // Removing the last extra group restores the single-workspace view.
    assert!(state.remove_extra_group("wsB"));
    assert!(!state.is_united());
    assert_eq!(state.list().group_count(), 1);
    // Removing an unknown workspace is a no-op.
    assert!(!state.remove_extra_group("ghost"));
}

#[test]
fn selected_workspace_root_and_note_follow_the_cursor_group() {
    // Flat rows (each expanded workspace owns a create row):
    //   0 usagi root, 1 main, 2 usagi create, 3 wsB root, 4 b1, 5 wsB create.
    let mut state = united_state();
    state.switch_select(0); // primary root
    assert_eq!(state.selected_workspace_root(), PathBuf::from("/usagi"));
    assert_eq!(state.selected_root_note(), Some("primary note"));
    state.switch_select(3); // the extra group's root
    assert_eq!(state.selected_workspace_root(), PathBuf::from("/wsB"));
    assert_eq!(state.selected_root_note(), Some("b note"));

    // An extra group with no root note reports none when the cursor is on it.
    state.set_extra_groups(vec![GroupSource {
        name: "wsC".to_string(),
        root_path: PathBuf::from("/wsC"),
        root_note: None,
        sessions: Vec::new(),
        issues: Vec::new(),
    }]);
    state.switch_select(3); // wsC root (usagi root 0, main 1, usagi create 2, wsC root 3)
    assert_eq!(state.selected_root_note(), None);
}

#[test]
fn selected_workspace_name_follows_the_cursor_group() {
    // Rows (each expanded workspace owns a create row):
    //   0 usagi root, 1 main, 2 usagi create, 3 wsB root, 4 b1, 5 wsB create.
    let mut state = united_state();
    state.switch_select(0); // primary root
    assert_eq!(state.selected_workspace_name(), "usagi");
    state.switch_select(1); // primary session
    assert_eq!(state.selected_workspace_name(), "usagi");
    state.switch_select(3); // the extra group's root
    assert_eq!(state.selected_workspace_name(), "wsB");
    state.switch_select(4); // the extra group's session
    assert_eq!(state.selected_workspace_name(), "wsB");
    // A single-workspace screen always reports the primary.
    let solo = HomeState::new("solo", Vec::new(), None);
    assert_eq!(solo.selected_workspace_name(), "solo");
}

#[test]
fn issue_command_scopes_to_the_cursor_group() {
    use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
    let ts = Utc::now();
    let issue = |number: u32, title: &str| Issue {
        number,
        title: title.to_string(),
        status: IssueStatus::Todo,
        priority: IssuePriority::Medium,
        labels: vec![],
        dependson: vec![],
        related: vec![],
        parent: None,
        milestone: None,
        created_at: ts,
        updated_at: ts,
        body: String::new(),
    };
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path("/usagi");
    state.restore_sessions(vec![session("main")]);
    state.set_issues(vec![issue(1, "primary-task")]);
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![session("b1")],
        issues: vec![issue(9, "extra-task")],
    }]);
    // Rows: usagi root0, main1, usagi create2, wsB root3, b1 4, wsB create5.

    // Cursor on the primary group: `issue list` surfaces the primary's issues.
    state.switch_select(1); // primary session "main"
    for c in "issue".chars() {
        state.push_char(c);
    }
    state.submit();
    let modal = state.text_modal().expect("issue opens a text modal");
    assert!(modal.lines.iter().any(|l| l.text.contains("primary-task")));
    assert!(!modal.lines.iter().any(|l| l.text.contains("extra-task")));

    // Cursor in the extra group: the same command now scopes to its issues.
    state.switch_select(4); // extra group session "b1"
    for c in "issue".chars() {
        state.push_char(c);
    }
    state.submit();
    let modal = state.text_modal().expect("issue opens a text modal");
    assert!(modal.lines.iter().any(|l| l.text.contains("extra-task")));
    assert!(!modal.lines.iter().any(|l| l.text.contains("primary-task")));
}

#[test]
fn toggle_selected_collapsed_folds_the_cursor_group_and_survives_a_resync() {
    // Rows: usagi root0, main1, usagi create2, wsB root3, b1 4, wsB create5.
    let mut state = united_state();
    state.switch_select(3); // wsB root
    state.toggle_selected_collapsed();
    assert!(state.list().is_collapsed(1));
    // The fold is recorded by name, so a background re-sync (which rebuilds the
    // list wholesale) keeps wsB folded.
    state.refresh_sessions(vec![session("main")]);
    assert!(state.list().is_collapsed(1));
    // Toggling again unfolds it, and that also survives a re-sync.
    state.switch_select(3); // wsB's folded header is its root slot
    state.toggle_selected_collapsed();
    assert!(!state.list().is_collapsed(1));
    state.refresh_sessions(vec![session("main")]);
    assert!(!state.list().is_collapsed(1));
}

#[test]
fn toggle_selected_collapsed_is_a_noop_off_a_root_or_in_a_single_workspace() {
    // Single workspace: never folds — that would hide the whole list.
    let mut solo = HomeState::new("solo", Vec::new(), None);
    solo.restore_sessions(vec![session("s1")]);
    solo.switch_select(0); // root
    solo.toggle_selected_collapsed();
    assert!(!solo.list().is_collapsed(0));
    // Unite, but the cursor is on a session row (not a root): no-op.
    let mut state = united_state();
    state.switch_select(1); // main
    state.toggle_selected_collapsed();
    assert!(!state.list().is_collapsed(0));
}

#[test]
fn expand_selected_group_for_create_unfolds_before_creating() {
    let mut state = united_state();
    state.switch_select(3); // wsB root
    state.toggle_selected_collapsed(); // fold wsB
    assert!(state.list().is_collapsed(1));
    // Creating a session into a folded workspace unfolds it first (its "+ new
    // session" row is otherwise hidden). A no-op when already expanded.
    state.expand_selected_group_for_create();
    assert!(!state.list().is_collapsed(1));
    state.expand_selected_group_for_create();
    assert!(!state.list().is_collapsed(1));
}

#[test]
fn workspace_root_for_session_finds_the_owning_group() {
    let state = united_state();
    // Unqualified: the first workspace (primary, then extras) owning the name.
    assert_eq!(
        state.workspace_root_for_session(None, "main"),
        PathBuf::from("/usagi")
    );
    assert_eq!(
        state.workspace_root_for_session(None, "b1"),
        PathBuf::from("/wsB")
    );
    // An unknown session falls back to the primary workspace.
    assert_eq!(
        state.workspace_root_for_session(None, "ghost"),
        PathBuf::from("/usagi")
    );
}

#[test]
fn workspace_root_for_session_honours_the_workspace_qualifier() {
    let state = united_state();
    // A `workspace:` qualifier targets that workspace's root directly: the
    // primary by its name...
    assert_eq!(
        state.workspace_root_for_session(Some("usagi"), "main"),
        PathBuf::from("/usagi")
    );
    // ...and an extra group by its name.
    assert_eq!(
        state.workspace_root_for_session(Some("wsB"), "b1"),
        PathBuf::from("/wsB")
    );
    // The qualifier wins even when the name only exists elsewhere, so the
    // removal acts on (and reports against) the named workspace.
    assert_eq!(
        state.workspace_root_for_session(Some("wsB"), "main"),
        PathBuf::from("/wsB")
    );
    // An unknown qualifier falls through to name-only resolution.
    assert_eq!(
        state.workspace_root_for_session(Some("ghost"), "b1"),
        PathBuf::from("/wsB")
    );
}

#[test]
fn group_value_type_carries_each_workspace_root() {
    use std::path::Path;
    // The root now lives on the `WorkspaceGroup` value type: the primary's is the
    // injected root, each extra group's is its own, and they survive a re-sync.
    let state = united_state();
    assert_eq!(state.list().groups()[0].root_path(), Path::new("/usagi"));
    assert_eq!(state.list().groups()[1].root_path(), Path::new("/wsB"));
    // The legacy single-workspace accessor delegates to the primary group.
    assert_eq!(state.root_path(), Path::new("/usagi"));
}

#[test]
fn ctrl_caret_jump_back_disambiguates_same_named_sessions_across_groups() {
    // Two workspaces each hold a session named "shared"; the Ctrl-^ jump-back
    // must return to the exact one left, not the first same-named match.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path("/usagi");
    state.restore_sessions(vec![session("main"), session("shared")]);
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: vec![session("shared")],
        issues: Vec::new(),
    }]);
    // Flat rows: 0 usagi root, 1 main, 2 shared(A), 3 wsB root, 4 shared(B).
    assert_eq!(state.list().session_count(), 5);
    assert_eq!(state.list().refs().len(), 5);

    // Focus wsB's "shared" (row 4), then focus the primary's "main" (row 1), so the
    // row left behind is wsB's "shared".
    state.enter_focus(4);
    state.enter_focus(1);
    // Ctrl-^ returns to wsB's "shared" at row 4 — not the primary's at row 2 that a
    // bare-name lookup would return first.
    assert_eq!(state.previous_session_row(), Some(4));

    // A background re-sync of the primary keeps the qualified jump target intact.
    state.refresh_sessions(vec![session("main"), session("shared")]);
    assert_eq!(state.previous_session_row(), Some(4));
}

#[test]
fn removable_session_names_qualify_in_unite_mode() {
    // Single-workspace mode offers plain session names.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path("/usagi");
    state.restore_sessions(vec![session("main")]);
    assert_eq!(state.removable_session_names(), vec!["main".to_string()]);

    // 統合(unite) mode qualifies every session as `workspace:session`.
    let state = united_state();
    assert_eq!(
        state.removable_session_names(),
        vec!["usagi:main".to_string(), "wsB:b1".to_string()]
    );
}

#[test]
fn a_session_op_targets_the_recorded_group_then_clears() {
    let mut state = united_state();
    // A create targeting the extra group lands its reloaded sessions there, leaving
    // the primary untouched.
    state.set_op_target(PathBuf::from("/wsB"));
    state.apply_task_completion(
        LogLine::output("created"),
        Some(vec![session("b1"), session("b2")]),
        None,
    );
    assert_eq!(state.list().groups()[1].worktrees().len(), 2); // wsB: b1, b2
    assert_eq!(state.list().groups()[0].worktrees().len(), 1); // primary: main

    // With no target recorded (or the primary's root), a completion routes to the
    // primary workspace.
    state.set_op_target(PathBuf::from("/usagi"));
    state.apply_task_completion(
        LogLine::output("created"),
        Some(vec![session("main2")]),
        None,
    );
    assert_eq!(state.list().groups()[0].worktrees().len(), 1);
    assert_eq!(state.sessions().len(), 1);
    assert_eq!(state.sessions()[0].name, "main2");
}

#[test]
fn a_background_task_completion_routes_by_its_target_root() {
    let mut state = united_state();
    // Even if the transient target currently points at the primary, an explicit
    // background completion root routes the refreshed sessions to the matching
    // unite group. This matters when bulk removals across workspaces finish out
    // of dispatch order.
    state.set_op_target(PathBuf::from("/usagi"));
    state.apply_task_completion(
        LogLine::output("removed"),
        Some(vec![session("b2")]),
        Some(Path::new("/wsB")),
    );
    assert_eq!(state.list().groups()[1].worktrees().len(), 1);
    assert_eq!(
        state.list().groups()[1].worktrees()[0].branch.as_deref(),
        Some("b2")
    );
    assert_eq!(state.sessions()[0].name, "main");
}

#[test]
fn a_sync_outcome_routes_sessions_and_root_note_to_the_target_group() {
    let mut state = united_state();
    // A rename-style outcome (carries sessions) for the extra group.
    state.set_op_target(PathBuf::from("/wsB"));
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("renamed"),
        sessions: Some(vec![session("b1"), session("b3")]),
        select: None,
        root_note: None,
    });
    assert_eq!(state.list().groups()[1].worktrees().len(), 2);

    // A root-note save for the extra group updates that group's marker, leaving the
    // primary's note alone.
    state.set_op_target(PathBuf::from("/wsB"));
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("noted"),
        sessions: None,
        select: None,
        root_note: Some(Some("new b note".to_string())),
    });
    assert!(state.list().groups()[1].root_has_note());
    assert_eq!(state.root_note(), Some("primary note")); // primary untouched
}

#[test]
fn new_state_starts_in_switch_with_a_hint() {
    let state = state();
    // The default mode is the base 切替 (Switch); the command palette is closed.
    assert_eq!(state.mode(), Mode::Switch);
    assert!(!state.command_palette_open());
    assert_eq!(state.switch_return(), ReturnMode::Base);
    assert_eq!(state.input(), "");
    assert_eq!(state.list().worktrees().len(), 2);
    // The seed log carries the usage hint.
    assert_eq!(state.log().len(), 1);
    assert!(state.log()[0].text.contains("man"));
    // The default action surface is the menu.
    assert_eq!(state.session_action_ui(), SessionActionUi::Menu);
    // The command palette line is always workspace-scoped.
    assert_eq!(state.command_scope(), CommandScope::Workspace);
}

#[test]
fn pr_popup_tracks_the_target_and_reports_changes() {
    let mut state = state();
    // No PR popup pinned by default.
    assert_eq!(state.pr_popup(), None);
    // Pinning a session both records it and reports the change.
    assert!(state.set_pr_popup(Some(1)));
    assert_eq!(state.pr_popup(), Some(1));
    // Re-pinning the same session is no change (the loop skips the repaint).
    assert!(!state.set_pr_popup(Some(1)));
    // Pinning another session, then closing, each count as changes.
    assert!(state.set_pr_popup(Some(0)));
    assert!(state.set_pr_popup(None));
    assert_eq!(state.pr_popup(), None);
    // Closing an already-closed popup is no change.
    assert!(!state.set_pr_popup(None));
}

#[test]
fn command_palette_opens_and_closes_clearing_the_input() {
    let mut state = state();
    assert!(!state.command_palette_open());
    // Typing then opening the palette starts it fresh (input cleared).
    state.push_char('x');
    state.open_command_palette();
    assert!(state.command_palette_open());
    assert_eq!(state.input(), "");
    // Closing it clears the input again.
    state.push_char('y');
    state.close_command_palette();
    assert!(!state.command_palette_open());
    assert_eq!(state.input(), "");
}

#[test]
fn loading_indicator_starts_clear_steps_and_finishes() {
    let mut state = state();
    // No action in flight by default.
    assert!(state.loading().is_none());

    // The first step begins the indicator at frame 0 with its label.
    state.step_loading("作成中…");
    let loading = state.loading().expect("loading begins on the first step");
    assert_eq!(loading.label(), "作成中…");
    assert_eq!(loading.frame(), 0);

    // Each further step advances the animation frame and updates the label,
    // mirroring how a bulk removal steps once per session.
    state.step_loading("削除中… 2/3");
    let loading = state.loading().unwrap();
    assert_eq!(loading.label(), "削除中… 2/3");
    assert_eq!(loading.frame(), 1);

    // Finishing clears it, returning the corner to its resting state.
    state.finish_loading();
    assert!(state.loading().is_none());
}

#[test]
fn pending_session_create_skeletons_begin_dedupe_and_clear_by_root_and_name() {
    let mut state = state();
    let root = PathBuf::from("/repo");
    state.begin_pending_session(root.clone(), "newx".to_string());
    state.begin_pending_session(root.clone(), "newx".to_string());
    assert_eq!(state.pending_sessions().len(), 1);
    assert_eq!(state.pending_sessions()[0].root(), root.as_path());
    assert_eq!(state.pending_sessions()[0].name(), "newx");

    assert!(!state.clear_pending_session(&root, "other"));
    assert_eq!(state.pending_sessions().len(), 1);
    assert!(state.clear_pending_session(&root, "newx"));
    assert!(state.pending_sessions().is_empty());
}

#[test]
fn a_notice_is_seeded_as_an_error_line() {
    let state = HomeState::new("usagi", Vec::new(), Some("load failed".to_string()));
    assert_eq!(state.log().len(), 2);
    assert_eq!(state.log()[1].kind, LineKind::Error);
    assert_eq!(state.log()[1].text, "load failed");
}

#[test]
fn set_session_action_ui_overrides_the_default() {
    let mut state = state();
    state.set_session_action_ui(SessionActionUi::Prompt);
    assert_eq!(state.session_action_ui(), SessionActionUi::Prompt);
}

#[test]
fn key_scheme_defaults_to_prefix_and_can_be_overridden() {
    use crate::domain::settings::KeyScheme;
    let mut state = state();
    // 没入 opens with the Ctrl-O prefix scheme unless the injected setting says
    // otherwise; the pane input loop reads it through `key_scheme()`.
    assert_eq!(state.key_scheme(), KeyScheme::Prefix);
    state.set_key_scheme(KeyScheme::Alt);
    assert_eq!(state.key_scheme(), KeyScheme::Alt);
}

#[test]
fn prefix_pending_starts_clear_and_tracks_the_leader() {
    let mut state = state();
    // No leader is pending until the pane drive loop reports one.
    assert!(!state.prefix_pending());
    state.set_prefix_pending(true);
    assert!(state.prefix_pending());
    state.set_prefix_pending(false);
    assert!(!state.prefix_pending());
}

#[test]
fn sidebar_defaults_to_full_and_toggles() {
    let mut state = state();
    // Opens full unless the injected setting says otherwise.
    assert_eq!(state.sidebar(), Sidebar::Full);
    // `Ctrl-B`'s effect: full ⇄ rail, independent of any mode change.
    state.toggle_sidebar();
    assert_eq!(state.sidebar(), Sidebar::Rail);
    state.toggle_sidebar();
    assert_eq!(state.sidebar(), Sidebar::Full);
    // The injected initial state overrides the default.
    state.set_sidebar(Sidebar::Rail);
    assert_eq!(state.sidebar(), Sidebar::Rail);
}

#[test]
fn mascot_blinks_on_a_kick_then_reopens_after_the_window() {
    use std::time::{Duration, Instant};
    let mut state = state();
    let t0 = Instant::now();
    // Resting: eyes open.
    state.tick_mascot(t0);
    assert!(!state.mascot_blinking());
    // A kick shuts the eyes for the blink window.
    state.kick_mascot_blink(t0);
    state.tick_mascot(t0 + Duration::from_millis(50));
    assert!(state.mascot_blinking());
    // Once the window passes the eyes reopen, and a later tick stays open (the
    // spent deadline was dropped, not re-armed).
    state.tick_mascot(t0 + Duration::from_millis(500));
    assert!(!state.mascot_blinking());
    state.tick_mascot(t0 + Duration::from_millis(600));
    assert!(!state.mascot_blinking());
}

#[test]
fn mascot_tick_advances_only_while_animation_is_enabled() {
    use std::time::Instant;
    let mut state = state();
    let t = Instant::now();
    let start = state.mascot_tick();
    state.tick_mascot(t);
    state.tick_mascot(t);
    assert_eq!(state.mascot_tick(), start + 2);
    // Disabling it freezes the pose, forces the eyes open, and makes a kick inert.
    state.set_mascot_animation_enabled(false);
    state.kick_mascot_blink(t);
    let frozen = state.mascot_tick();
    state.tick_mascot(t);
    assert_eq!(state.mascot_tick(), frozen);
    assert!(!state.mascot_blinking());
}

#[test]
fn mascot_reacts_on_a_click_then_settles_after_the_window() {
    use std::time::{Duration, Instant};
    let mut state = state();
    let t0 = Instant::now();
    // Resting: no reaction in flight.
    state.tick_mascot(t0);
    assert!(!state.mascot_reacting());
    assert_eq!(state.mascot_reaction(), None);
    // A click kicks a reaction that plays for the reaction window.
    state.kick_mascot_reaction(t0);
    assert!(state.mascot_reacting());
    assert!(state.mascot_reaction().is_some());
    state.tick_mascot(t0 + Duration::from_millis(100));
    assert!(state.mascot_reacting());
    // Once the window passes the reaction settles back to rest and stays settled.
    state.tick_mascot(t0 + Duration::from_millis(700));
    assert!(!state.mascot_reacting());
    assert_eq!(state.mascot_reaction(), None);
    state.tick_mascot(t0 + Duration::from_millis(900));
    assert!(!state.mascot_reacting());
}

#[test]
fn mascot_reaction_phase_counts_from_the_click() {
    use std::time::{Duration, Instant};
    let mut state = state();
    let t0 = Instant::now();
    // Advance the live tick a few times so the reaction's start tick is non-zero.
    state.tick_mascot(t0);
    state.tick_mascot(t0);
    state.kick_mascot_reaction(t0);
    // Right after the kick the phase is zero (no tick has advanced since).
    assert_eq!(state.mascot_reaction_phase(), 0);
    // Each in-window tick advances the phase by one, counting from the click.
    state.tick_mascot(t0 + Duration::from_millis(100));
    assert_eq!(state.mascot_reaction_phase(), 1);
    state.tick_mascot(t0 + Duration::from_millis(200));
    assert_eq!(state.mascot_reaction_phase(), 2);
}

#[test]
fn mascot_reaction_varies_across_repeated_clicks() {
    use std::time::Instant;
    let mut state = state();
    let t = Instant::now();
    // Repeated clicks pick from all three reactions rather than replaying one.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..12 {
        state.kick_mascot_reaction(t);
        seen.insert(state.mascot_reaction());
    }
    assert!(seen.len() >= 2, "repeated clicks vary the reaction");
}

#[test]
fn update_confirm_opens_and_cancels() {
    let mut state = state();
    assert!(!state.update_confirm());
    state.open_update_confirm();
    assert!(state.update_confirm());
    state.cancel_update_confirm();
    assert!(!state.update_confirm());
}

#[test]
fn clicking_the_mascot_opens_the_update_modal_when_an_update_is_available() {
    use crate::domain::version::Version;
    use std::time::Instant;
    let mut state = state();
    state.set_update(Version::parse("9.9.9"));
    // With an update pending, a click on the mascot asks to update rather than
    // playing a reaction.
    state.click_mascot(Instant::now());
    assert!(state.update_confirm());
    assert!(!state.mascot_reacting());
}

#[test]
fn clicking_the_mascot_reacts_when_work_status_hides_the_update_notice() {
    use crate::domain::version::Version;
    use std::time::Instant;

    let mut loading = state();
    loading.set_update(Version::parse("9.9.9"));
    loading.step_loading("エージェント起動中…");
    loading.click_mascot(Instant::now());
    assert!(!loading.update_confirm());
    assert!(loading.mascot_reacting());

    let mut tasking = state();
    tasking.set_update(Version::parse("9.9.9"));
    tasking.set_tasks(vec![crate::presentation::tui::home::tasks::TaskRow {
        kind: TaskKind::CreateSession,
        label: "作成中… main".to_string(),
        mark: crate::presentation::tui::home::tasks::TaskMark::Running(0),
    }]);
    tasking.click_mascot(Instant::now());
    assert!(!tasking.update_confirm());
    assert!(tasking.mascot_reacting());
}

#[test]
fn clicking_the_mascot_without_an_update_plays_a_reaction() {
    use std::time::Instant;
    let mut state = state();
    assert!(state.update().is_none());
    state.click_mascot(Instant::now());
    // No update to offer, so the click is just the playful reaction.
    assert!(!state.update_confirm());
    assert!(state.mascot_reacting());
}

#[test]
fn disabling_mascot_animation_makes_a_click_inert_and_clears_a_reaction() {
    use std::time::Instant;
    let mut state = state();
    let t = Instant::now();
    // A reaction in flight is cleared the moment the mascot is turned off.
    state.kick_mascot_reaction(t);
    assert!(state.mascot_reacting());
    state.set_mascot_animation_enabled(false);
    assert!(!state.mascot_reacting());
    // And a click on a disabled mascot kicks nothing.
    state.kick_mascot_reaction(t);
    assert!(!state.mascot_reacting());
}

#[test]
fn disabling_mascot_animation_clears_a_blink_in_flight() {
    use std::time::Instant;
    let mut state = state();
    let t = Instant::now();
    state.kick_mascot_blink(t);
    state.tick_mascot(t);
    assert!(state.mascot_blinking());
    // Turning the mascot off mid-blink settles it to a still, open-eyed image.
    state.set_mascot_animation_enabled(false);
    assert!(!state.mascot_blinking());
}

#[test]
fn backspace_removes_the_last_character() {
    let mut state = state();
    state.push_char('m');
    state.push_char('a');
    state.backspace();
    assert_eq!(state.input(), "m");
    state.backspace();
    state.backspace(); // popping past empty is harmless
    assert_eq!(state.input(), "");
}

#[test]
fn tab_completes_a_unique_command() {
    let mut state = state();
    state.push_char('d');
    state.push_char('o');
    state.push_char('c');
    state.complete();
    assert_eq!(state.input(), "doctor");
    // A unique completion adds nothing to the log.
    assert_eq!(state.log().len(), 1);
}

#[test]
fn tab_completes_a_session_name_after_remove() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    for c in "session remove al".chars() {
        state.push_char(c);
    }
    state.complete();
    // The unique session-name prefix fills in.
    assert_eq!(state.input(), "session remove alpha");
}

#[test]
fn tab_completes_a_qualified_session_name_after_remove_in_unite_mode() {
    let mut state = united_state(); // primary "usagi" (main), extra "wsB" (b1)
    for c in "session remove wsB:".chars() {
        state.push_char(c);
    }
    state.complete();
    // The lone session under the qualified workspace fills straight in.
    assert_eq!(state.input(), "session remove wsB:b1");
}

#[test]
fn tab_lists_candidates_when_ambiguous() {
    let mut state = state();
    // Empty input matches every workspace command, so Tab lists them.
    state.complete();
    assert_eq!(state.input(), "");
    let last = state.log().last().unwrap();
    assert!(last.text.contains("session"));
    assert!(last.text.contains("man"));
}

#[test]
fn submitting_an_empty_line_is_a_noop() {
    let mut state = state();
    let before = state.log().len();
    let submission = state.submit();
    assert_eq!(submission.effect, Effect::None);
    assert!(submission.recorded.is_none());
    assert_eq!(state.log().len(), before);
}

#[test]
fn submitting_a_command_echoes_and_runs_it() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    let submission = state.submit();
    // `man` is a text-dumping command: it echoes, then opens a text modal
    // (its output does not land in the band's log).
    assert_eq!(
        submission.effect,
        Effect::ShowText {
            title: "Help",
            size: ModalSize::Large,
        }
    );
    assert_eq!(
        submission.recorded.as_ref().map(|e| e.command.as_str()),
        Some("man")
    );
    assert_eq!(
        submission
            .recorded
            .as_ref()
            .and_then(|e| e.session.as_ref()),
        None
    );
    assert!(submission.recorded.as_ref().is_some_and(|e| e.success));
    let echoed = state.log().iter().find(|l| l.kind == LineKind::Command);
    assert_eq!(echoed.unwrap().text, "man");
    let modal = state.text_modal().expect("man opens a text modal");
    assert_eq!(modal.title, "Help");
    assert!(modal.lines.iter().any(|l| l.text.contains("Available")));
    // The band shows none of the modal's output (its response is empty).
    assert!(state.response_lines().is_empty());
    assert_eq!(state.input(), "");
}

#[test]
fn submitting_an_error_command_records_failure() {
    let mut state = state();
    for c in "nope".chars() {
        state.push_char(c);
    }
    let submission = state.submit();
    assert_eq!(
        submission.recorded.as_ref().map(|e| e.command.as_str()),
        Some("nope")
    );
    assert!(submission.recorded.as_ref().is_some_and(|e| !e.success));
}

#[test]
fn issue_command_reads_injected_issues() {
    use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
    let mut state = state();
    let ts = Utc::now();
    state.set_issues(vec![Issue {
        number: 1,
        title: "task".to_string(),
        status: IssueStatus::Todo,
        priority: IssuePriority::Medium,
        labels: vec![],
        dependson: vec![],
        related: vec![],
        parent: None,
        milestone: None,
        created_at: ts,
        updated_at: ts,
        body: String::new(),
    }]);
    for c in "issue".chars() {
        state.push_char(c);
    }
    let submission = state.submit();
    // The injected issue is surfaced through the `issue` command's modal.
    assert_eq!(
        submission.effect,
        Effect::ShowText {
            title: "Issues",
            size: ModalSize::Normal,
        }
    );
    let modal = state.text_modal().expect("issue opens a text modal");
    assert!(modal.lines.iter().any(|l| l.text.contains("task")));
}

#[test]
fn session_switch_with_no_name_yields_the_enter_switch_effect() {
    // The screen leaves the mode transition to the event loop; submit only
    // surfaces the effect and logs no resolution line.
    let mut state = state();
    for c in "session switch".chars() {
        state.push_char(c);
    }
    let before = state.log().len();
    let submission = state.submit();
    assert_eq!(submission.effect, Effect::EnterSwitch);
    // Only the echoed command line was appended.
    assert_eq!(state.log().len(), before + 1);
}

#[test]
fn session_switch_with_a_name_yields_the_activate_effect() {
    let mut state = state();
    for c in "session switch feature".chars() {
        state.push_char(c);
    }
    let submission = state.submit();
    assert_eq!(submission.effect, Effect::Activate("feature".to_string()));
    // The list is not resolved here (the event loop does it).
    assert_eq!(state.list().active_index(), 0);
}

#[test]
fn clear_command_empties_the_log() {
    let mut state = state();
    for c in "clear".chars() {
        state.push_char(c);
    }
    assert_eq!(state.submit().effect, Effect::Clear);
    assert!(state.log().is_empty());
}

#[test]
fn quit_command_returns_the_quit_effect() {
    let mut state = state();
    for c in "quit".chars() {
        state.push_char(c);
    }
    assert_eq!(state.submit().effect, Effect::Quit);
}

#[test]
fn submitted_commands_are_recorded_in_history() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.submit();
    for c in "doctor".chars() {
        state.push_char(c);
    }
    state.submit();
    assert_eq!(state.cmdline.history, vec!["man", "doctor"]);
}

#[test]
fn a_repeated_command_is_not_recorded_twice_in_a_row() {
    let mut state = state();
    for _ in 0..2 {
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
    }
    // The consecutive duplicate is dropped (shell-style), so recall has one entry.
    assert_eq!(state.cmdline.history, vec!["man"]);
}

#[test]
fn restored_history_is_capped_to_the_most_recent_entries() {
    let mut state = state();
    let total = MAX_COMMAND_HISTORY + 5;
    let entries: Vec<String> = (0..total).map(|i| format!("cmd-{i}")).collect();
    state.restore_history(entries);
    assert_eq!(state.cmdline.history.len(), MAX_COMMAND_HISTORY);
    // The oldest five were dropped; the newest is kept.
    assert_eq!(state.cmdline.history.first().unwrap(), "cmd-5");
    assert_eq!(
        state.cmdline.history.last().unwrap(),
        &format!("cmd-{}", total - 1)
    );
}

#[test]
fn appending_past_the_cap_drops_the_oldest_command() {
    let mut state = state();
    let entries: Vec<String> = (0..MAX_COMMAND_HISTORY)
        .map(|i| format!("cmd-{i}"))
        .collect();
    state.restore_history(entries);
    state.cmdline.input.set_value("man");
    state.submit();
    // Still capped, oldest evicted, newest appended.
    assert_eq!(state.cmdline.history.len(), MAX_COMMAND_HISTORY);
    assert_eq!(state.cmdline.history.first().unwrap(), "cmd-1");
    assert_eq!(state.cmdline.history.last().unwrap(), "man");
}

#[test]
fn the_output_log_is_capped_so_it_cannot_grow_without_bound() {
    let mut state = state();
    for i in 0..(MAX_LOG_LINES + 10) {
        state.log_output(format!("line {i}"));
    }
    assert_eq!(state.log().len(), MAX_LOG_LINES);
    // The newest line survives; the oldest were dropped.
    assert_eq!(
        state.log().last().unwrap().text,
        format!("line {}", MAX_LOG_LINES + 9)
    );
}

#[test]
fn restored_history_feeds_recall_and_new_commands_append_to_it() {
    let mut state = state();
    state.restore_history(vec!["session".to_string(), "space".to_string()]);
    state.recall_prev();
    assert_eq!(state.input(), "space");
    state.recall_prev();
    assert_eq!(state.input(), "session");
    state.cmdline.input.set_value("man");
    state.submit();
    assert_eq!(state.cmdline.history, vec!["session", "space", "man"]);
}

#[test]
fn history_recall_walks_backwards_and_forwards() {
    let mut state = state();
    for entry in ["man", "doctor"] {
        for c in entry.chars() {
            state.push_char(c);
        }
        state.submit();
    }
    state.recall_prev();
    assert_eq!(state.input(), "doctor");
    state.recall_prev();
    assert_eq!(state.input(), "man");
    state.recall_prev();
    assert_eq!(state.input(), "man");
    state.recall_next();
    assert_eq!(state.input(), "doctor");
    state.recall_next();
    assert_eq!(state.input(), "");
}

#[test]
fn recall_prev_is_a_noop_without_history() {
    let mut state = state();
    state.recall_prev();
    assert_eq!(state.input(), "");
}

#[test]
fn recall_next_without_active_recall_is_a_noop() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.submit();
    state.recall_next();
    assert_eq!(state.input(), "");
}

#[test]
fn typing_or_completing_cancels_an_active_recall() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.submit();
    state.recall_prev();
    assert_eq!(state.input(), "man");
    state.push_char('!');
    state.recall_next();
    assert_eq!(state.input(), "man!");
}

#[test]
fn set_pr_links_updates_the_sidebar_row_live() {
    use crate::domain::workspace_state::PrLink;
    use std::path::Path;

    // `state()` records two sessions: `main` at /repo/main and `feature` at
    // /repo/feature.
    let mut state = state();
    let pr = |n: u32| PrLink {
        number: n,
        url: format!("https://github.com/o/r/pull/{n}"),
    };

    // A new PR for an existing row updates its in-memory badge and reports the
    // change (so the attached pane knows it has something fresh to show).
    assert!(state.set_pr_links(Path::new("/repo/main"), vec![pr(442)]));
    let row = state
        .list()
        .worktrees()
        .iter()
        .find(|w| w.branch.as_deref() == Some("main"))
        .expect("the main row exists");
    assert_eq!(row.pr, vec![pr(442)]);

    // Re-applying the same set is a no-op, so the pane skips a needless repaint.
    assert!(!state.set_pr_links(Path::new("/repo/main"), vec![pr(442)]));

    // An unknown root (e.g. the workspace root, which has no worktree row) does
    // nothing.
    assert!(!state.set_pr_links(Path::new("/repo/nope"), vec![pr(1)]));
}

#[test]
fn tab_menu_and_rename_overlay_lifecycle() {
    let mut state = state();
    state.open_tab_menu(PathBuf::from("/repo/main"), 1, "terminal", 42, 7);
    let menu = state.tab_menu().expect("menu open");
    assert_eq!(menu.dir(), PathBuf::from("/repo/main").as_path());
    assert_eq!(menu.tab(), 1);
    assert_eq!(menu.label(), "terminal");
    assert_eq!(menu.col(), 42);
    assert_eq!(menu.row(), 7);

    state.tab_menu_mut().unwrap().move_down();
    assert_eq!(state.tab_menu().unwrap().item(), TabMenuItem::MoveRight);
    assert!(state.begin_tab_rename_from_menu().is_some());
    assert!(state.tab_menu().is_none());
    assert_eq!(state.tab_rename().unwrap().value(), "terminal");

    let input = state.tab_rename_mut().unwrap();
    input.move_end();
    input.push_char('2');
    let (dir, tab, label) = state.confirm_tab_rename().unwrap();
    assert_eq!(dir, PathBuf::from("/repo/main"));
    assert_eq!(tab, 1);
    assert_eq!(label, "terminal2");
    assert!(state.tab_rename().is_none());

    assert!(state.tab_menu_mut().is_none());
    assert!(state.tab_rename_mut().is_none());
    assert!(state.begin_tab_rename_from_menu().is_none());
    assert!(state.confirm_tab_rename().is_none());
    state.cancel_tab_rename();
    state.close_tab_menu();
}

#[test]
fn tab_menu_and_rename_cancel_paths() {
    let mut state = state();
    state.open_tab_menu(PathBuf::from("/repo/main"), 0, "agent", 1, 2);
    state.close_tab_menu();
    assert!(state.tab_menu().is_none());

    state.open_tab_menu(PathBuf::from("/repo/main"), 0, "agent", 1, 2);
    assert!(state.begin_tab_rename_from_menu().is_some());
    state.cancel_tab_rename();
    assert!(state.tab_rename().is_none());
}

#[test]
fn session_agent_for_resolves_the_override_by_root_or_worktree_path() {
    use crate::domain::settings::AgentCli;
    use crate::domain::workspace_state::{SessionAgent, WorktreeState};

    let mut record = session("pinned");
    record.agent = SessionAgent {
        cli: Some(AgentCli::Gemini),
        model: Some("gemini-2.5-pro".to_string()),
    };
    // Give it a worktree at a distinct path (a multi-repo session's worktree
    // differs from its session root).
    let wt_path = PathBuf::from("/repo/app-a/.usagi/sessions/pinned");
    record.worktrees = vec![WorktreeState {
        branch: None,
        path: wt_path.clone(),
        head: String::new(),
        primary: true,
        upstream: None,
        status: Default::default(),
        diff: None,
        ahead_behind: None,
        pr: Vec::new(),
        updated_at: Utc::now(),
    }];
    let root = record.root.clone();

    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.restore_sessions(vec![record]);

    // Matched by session root...
    let by_root = state.session_agent_for(&root);
    assert_eq!(by_root.cli, Some(AgentCli::Gemini));
    assert_eq!(by_root.model.as_deref(), Some("gemini-2.5-pro"));
    // ...and by any of its worktree paths.
    assert_eq!(
        state.session_agent_for(&wt_path).cli,
        Some(AgentCli::Gemini)
    );
    // A path belonging to no session (e.g. the ⌂ root row) yields the default.
    assert!(state
        .session_agent_for(&PathBuf::from("/somewhere/else"))
        .is_unset());
}
