use super::super::terminal_pool::MonitorSnapshot;
use super::*;
use crate::domain::workspace_state::BranchStatus;
use chrono::Utc;
use std::path::PathBuf;

fn worktree(branch: &str) -> WorktreeState {
    WorktreeState {
        branch: Some(branch.to_string()),
        path: PathBuf::from(format!("/repo/{branch}")),
        head: "abc1234".to_string(),
        primary: false,
        upstream: None,
        status: BranchStatus::Local,
        updated_at: Utc::now(),
    }
}

fn sample() -> WorktreeList {
    WorktreeList::new(
        "usagi",
        vec![worktree("main"), worktree("feature"), worktree("fix")],
    )
}

#[test]
fn new_list_starts_on_the_root_row() {
    let list = sample();
    assert_eq!(list.workspace_name(), "usagi");
    // The cursor starts on the root row, which belongs to no session.
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
    assert!(list.selected().is_none());
    assert_eq!(list.worktrees().len(), 3);
    assert!(!list.is_empty());
}

#[test]
fn empty_list_still_has_the_root_row() {
    let list = WorktreeList::new("usagi", Vec::new());
    assert!(list.is_empty());
    assert!(list.root_selected());
    // The root row has no worktree behind it.
    assert!(list.selected().is_none());
}

#[test]
fn display_label_uses_the_override_then_falls_back_to_the_branch() {
    // A labels vec shorter than the worktrees is padded with `None`; a longer
    // one is truncated to match.
    let list = WorktreeList::with_labels(
        "usagi",
        vec![worktree("main"), worktree("feature"), worktree("fix")],
        vec![Some("Main".to_string()), None],
    );
    assert_eq!(list.display_label(0), "Main"); // override
    assert_eq!(list.display_label(1), "feature"); // explicit None → branch
    assert_eq!(list.display_label(2), "fix"); // padded None → branch
                                              // An out-of-range index has neither a label nor a worktree.
    assert_eq!(list.display_label(9), "");
}

#[test]
fn move_down_advances_past_the_root_row_and_wraps() {
    let mut list = sample(); // root, main, feature, fix
    list.move_down();
    assert_eq!(list.selected_index(), 1);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    list.move_down();
    list.move_down();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
    // Wraps from the last worktree back to the root row.
    list.move_down();
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
}

#[test]
fn move_up_wraps_from_the_root_row_to_the_bottom() {
    let mut list = sample(); // root, main, feature, fix
    list.move_up();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
    list.move_up();
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
}

#[test]
fn movement_wraps_around_the_lone_root_row_when_empty() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    // Only the root row exists, so movement keeps the cursor on it.
    list.move_up();
    assert_eq!(list.selected_index(), 0);
    list.move_down();
    assert_eq!(list.selected_index(), 0);
}

#[test]
fn the_root_row_is_active_by_default() {
    let list = sample();
    assert_eq!(list.active_index(), 0);
    assert!(list.root_active());
    assert!(list.active().is_none());
}

#[test]
fn activate_selected_follows_the_cursor() {
    let mut list = sample(); // root, main, feature, fix
    list.move_down();
    list.move_down(); // cursor on "feature"
    assert_eq!(list.activate_selected(), "feature");
    assert_eq!(list.active_index(), 2);
    assert!(!list.root_active());
    // The cursor and the active row are independent afterwards.
    list.move_down(); // cursor on "fix"
    assert_eq!(list.active_index(), 2);
    assert_eq!(list.selected_index(), 3);
}

#[test]
fn activate_selected_can_return_to_the_root_row() {
    let mut list = sample();
    list.move_down(); // cursor on "main"
    list.activate_selected();
    assert!(!list.root_active());
    // Moving back to the root row and activating it returns to "root".
    list.move_up(); // cursor on the root row
    assert_eq!(list.activate_selected(), ROOT_NAME);
    assert!(list.root_active());
}

#[test]
fn activate_selected_on_an_empty_list_picks_the_root_row() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    assert_eq!(list.activate_selected(), ROOT_NAME);
    assert!(list.root_active());
    assert!(list.active().is_none());
}

#[test]
fn activate_by_name_matches_worktrees_the_root_or_reports_missing() {
    let mut list = sample(); // root, main, feature, fix
    assert!(list.activate_by_name("fix"));
    assert_eq!(list.active_index(), 3);
    // The root row is reachable by name too.
    assert!(list.activate_by_name(ROOT_NAME));
    assert_eq!(list.active_index(), 0);
    assert!(list.root_active());
    assert!(!list.activate_by_name("nope"));
    // A failed lookup leaves the active row unchanged.
    assert_eq!(list.active_index(), 0);
}

#[test]
fn select_by_name_moves_the_cursor_and_active_row_to_the_match() {
    let mut list = sample(); // root, main, feature, fix
    assert!(list.select_by_name("feature"));
    // Both the cursor and the active row land on the matched worktree.
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.active_index(), 2);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
    // An unknown name leaves both cursors unchanged.
    assert!(!list.select_by_name("nope"));
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.active_index(), 2);
}

#[test]
fn refs_expose_the_root_row_then_worktrees_with_the_active_flag() {
    let mut list = sample();
    list.activate_by_name("feature");
    let refs = list.refs();
    assert_eq!(refs.len(), 4);
    assert_eq!(refs[0].name, ROOT_NAME);
    assert!(!refs[0].active);
    assert_eq!(refs[1].name, "main");
    assert!(!refs[1].active);
    assert_eq!(refs[2].name, "feature");
    assert!(refs[2].active);
}

#[test]
fn refs_mark_the_root_row_active_by_default() {
    let refs = sample().refs();
    assert_eq!(refs[0].name, ROOT_NAME);
    assert!(refs[0].active);
}

#[test]
fn worktree_name_falls_back_to_detached() {
    let mut detached = worktree("main");
    detached.branch = None;
    assert_eq!(worktree_name(&detached), "(detached)");
}

// --- HomeState ---------------------------------------------------------

fn state() -> HomeState {
    HomeState::new("usagi", vec![worktree("main"), worktree("feature")], None)
}

/// A [`Logger`](crate::infrastructure::error_log::Logger) that captures every
/// recorded message, so a test can assert which on-screen errors are persisted.
/// The shared `Rc<RefCell<…>>` lets the test read what the injected sink received.
#[derive(Clone, Default)]
struct SpyLogger {
    recorded: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl crate::infrastructure::error_log::Logger for SpyLogger {
    fn record(&self, message: &str) {
        self.recorded.borrow_mut().push(message.to_string());
    }
}

/// A [`HomeState`] wired to a [`SpyLogger`], returning both so the test can drive
/// the screen and inspect what was recorded.
fn state_with_spy() -> (HomeState, SpyLogger) {
    let spy = SpyLogger::default();
    let mut state = state();
    state.set_logger(Box::new(spy.clone()));
    (state, spy)
}

#[test]
fn new_state_starts_in_overview_with_a_hint() {
    let state = state();
    assert_eq!(state.mode(), Mode::Overview);
    assert_eq!(state.input(), "");
    assert_eq!(state.list().worktrees().len(), 2);
    // The seed log carries the usage hint.
    assert_eq!(state.log().len(), 1);
    assert!(state.log()[0].text.contains("man"));
    // The default action surface is the menu.
    assert_eq!(state.session_action_ui(), SessionActionUi::Menu);
    // The Overview line is always workspace-scoped.
    assert_eq!(state.command_scope(), CommandScope::Workspace);
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
    assert_eq!(submission.effect, Effect::ShowText("Help"));
    assert_eq!(submission.recorded.as_deref(), Some("man"));
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
    assert_eq!(submission.effect, Effect::ShowText("Issues"));
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
    assert_eq!(state.history, vec!["man", "doctor"]);
}

#[test]
fn restored_history_feeds_recall_and_new_commands_append_to_it() {
    let mut state = state();
    state.restore_history(vec!["session".to_string(), "space".to_string()]);
    state.recall_prev();
    assert_eq!(state.input(), "space");
    state.recall_prev();
    assert_eq!(state.input(), "session");
    state.input.set_value("man");
    state.submit();
    assert_eq!(state.history, vec!["session", "space", "man"]);
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

// --- caret editing -----------------------------------------------------

#[test]
fn arrows_move_the_caret_and_insert_mid_line() {
    let mut state = state();
    for c in "mn".chars() {
        state.push_char(c);
    }
    // Caret sits past the end after typing.
    assert_eq!(state.cursor(), 2);
    state.cursor_left();
    assert_eq!(state.cursor(), 1);
    // Insert between the two characters.
    state.push_char('a');
    assert_eq!(state.input(), "man");
    assert_eq!(state.cursor(), 2);
    // Right then past the end is clamped.
    state.cursor_right();
    state.cursor_right();
    assert_eq!(state.cursor(), 3);
    // Left at the start is clamped to 0.
    state.cursor_home();
    state.cursor_left();
    assert_eq!(state.cursor(), 0);
    state.cursor_end();
    assert_eq!(state.cursor(), 3);
}

#[test]
fn backspace_and_delete_act_around_the_caret() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.cursor_home();
    // Backspace at the start is a no-op.
    state.backspace();
    assert_eq!(state.input(), "man");
    // Delete-forward removes the character at the caret.
    state.delete_forward();
    assert_eq!(state.input(), "an");
    assert_eq!(state.cursor(), 0);
    // Delete-forward at the end is a no-op.
    state.cursor_end();
    state.delete_forward();
    assert_eq!(state.input(), "an");
    // Backspace removes the character before the caret.
    state.backspace();
    assert_eq!(state.input(), "a");
    assert_eq!(state.cursor(), 1);
}

#[test]
fn caret_moves_by_whole_multibyte_characters() {
    let mut state = state();
    for c in "あい".chars() {
        state.push_char(c);
    }
    // Each Japanese character is three bytes; the caret tracks byte offsets but
    // moves a whole character at a time.
    assert_eq!(state.cursor(), 6);
    state.cursor_left();
    assert_eq!(state.cursor(), 3);
    state.push_char('x');
    assert_eq!(state.input(), "あxい");
    state.backspace();
    assert_eq!(state.input(), "あい");
    assert_eq!(state.cursor(), 3);
    state.delete_forward();
    assert_eq!(state.input(), "あ");
}

#[test]
fn recall_and_submit_place_the_caret_at_the_end() {
    let mut state = state();
    state.restore_history(vec!["session".to_string()]);
    state.recall_prev();
    assert_eq!(state.cursor(), state.input().len());
    state.recall_next();
    assert_eq!(state.cursor(), 0);
    state.push_char('m');
    state.submit();
    assert_eq!(state.cursor(), 0);
}

// --- 切替 (Switch) -----------------------------------------------------

#[test]
fn enter_switch_remembers_its_return_mode_and_moves_the_cursor() {
    let mut state = state(); // root, main, feature
    state.enter_switch(ReturnMode::Overview);
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.switch_return(), ReturnMode::Overview);
    state.switch_move_down();
    assert_eq!(state.list().selected_index(), 1);
    state.switch_move_up();
    assert_eq!(state.list().selected_index(), 0);
    // Up from the root wraps to the bottom (the last worktree row, 2).
    state.switch_move_up();
    assert_eq!(state.list().selected_index(), 2);
}

#[test]
fn switch_return_carries_each_origin() {
    let mut state = state();
    state.enter_switch(ReturnMode::Focus);
    assert_eq!(state.switch_return(), ReturnMode::Focus);
    state.enter_switch(ReturnMode::Attached);
    assert_eq!(state.switch_return(), ReturnMode::Attached);
}

#[test]
fn switch_inline_create_edits_then_confirms_a_fresh_name() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    assert!(!state.is_creating());
    state.switch_begin_create(Vec::new());
    assert!(state.is_creating());
    assert_eq!(state.create().unwrap().value(), "");
    {
        let input = state.create_mut().unwrap();
        for c in "  wip  ".chars() {
            input.push_char(c);
        }
        input.backspace(); // drop a trailing space
    }
    // A fresh, trimmed name is accepted and the input closes.
    assert_eq!(state.switch_confirm_create().as_deref(), Some("wip"));
    assert!(!state.is_creating());
}

#[test]
fn switch_inline_create_rejects_empty_and_duplicate_names() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    // "feature" is an existing branch, so it is in the taken set.
    state.switch_begin_create(vec!["feature".to_string()]);
    // Whitespace only is empty after trimming: no live error (it does not nag),
    // but Enter rejects it.
    state.create_mut().unwrap().push_char(' ');
    assert!(state.create().unwrap().error().is_none());
    assert!(state.switch_confirm_create().is_none());
    assert!(state
        .create()
        .unwrap()
        .error()
        .unwrap()
        .contains("must not be empty"));
    // Typing a duplicate name flags it live, before Enter, and Enter rejects it.
    for c in "feature".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    assert!(state.create().unwrap().error().unwrap().contains("feature"));
    assert!(state.switch_confirm_create().is_none());
    assert!(state.create().unwrap().error().unwrap().contains("feature"));
    assert!(state.is_creating());
}

#[test]
fn switch_inline_create_flags_a_branch_namespace_clash_live() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    // Branches nested under `test/` make a plain `test` session impossible.
    state.switch_begin_create(vec!["test/home-ui-e2e".to_string()]);
    for c in "test".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    // The clash is shown live and blocks confirmation.
    let err = state.create().unwrap().error().unwrap().to_string();
    assert!(err.contains("conflicts with branch"), "{err}");
    assert!(err.contains("test/home-ui-e2e"), "{err}");
    assert!(state.switch_confirm_create().is_none());
    // Backspacing to "tes" (no longer a clash) clears the error.
    state.create_mut().unwrap().backspace();
    assert!(state.create().unwrap().error().is_none());
    // Typing a path separator is itself rejected (not a legal session name).
    state.create_mut().unwrap().push_char('/');
    assert!(state
        .create()
        .unwrap()
        .error()
        .unwrap()
        .contains("path separators"));
}

#[test]
fn switch_inline_create_can_be_cancelled() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    state.switch_begin_create(Vec::new());
    state.create_mut().unwrap().push_char('x');
    state.create_cancel();
    assert!(!state.is_creating());
}

#[test]
fn create_accessors_are_none_when_not_creating() {
    let mut state = state();
    // Nothing open: the accessors are empty and the lifecycle calls are safe.
    assert!(!state.is_creating());
    assert!(state.create().is_none());
    assert!(state.create_mut().is_none());
    assert!(state.switch_confirm_create().is_none());
    state.create_cancel();
    assert!(!state.is_creating());
}

#[test]
fn create_caret_moves_and_edits_mid_name() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    state.switch_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    assert_eq!(state.create().unwrap().cursor(), 3);
    // Home, then insert at the front.
    state.create_mut().unwrap().move_home();
    assert_eq!(state.create().unwrap().cursor(), 0);
    state.create_mut().unwrap().push_char('x');
    assert_eq!(state.create().unwrap().value(), "xwip");
    assert_eq!(state.create().unwrap().cursor(), 1);
    // Del removes the character at the caret; Backspace the one before.
    state.create_mut().unwrap().delete_forward(); // removes 'w' → "xip"
    assert_eq!(state.create().unwrap().value(), "xip");
    state.create_mut().unwrap().move_right(); // between 'i' and 'p'
    state.create_mut().unwrap().backspace(); // removes 'i' → "xp"
    assert_eq!(state.create().unwrap().value(), "xp");
    // End parks the caret past the last character.
    state.create_mut().unwrap().move_end();
    assert_eq!(state.create().unwrap().cursor(), 2);
}

// --- 切替 (Switch) inline rename ---------------------------------------

#[test]
fn switch_inline_rename_prefills_edits_then_confirms_a_label() {
    let mut state = state(); // sessions: main, feature
    state.enter_switch(ReturnMode::Overview);
    state.switch_move_down(); // cursor onto "main"
    assert!(state.switch_begin_rename());
    assert!(state.is_renaming());
    assert_eq!(state.rename().unwrap().target(), "main");
    // The input is pre-filled with the current label (the session name).
    assert_eq!(state.rename().unwrap().value(), "main");
    // Edit it to a custom label.
    {
        let input = state.rename_mut().unwrap();
        for _ in 0..4 {
            input.backspace();
        }
        for c in "  My main  ".chars() {
            input.push_char(c);
        }
    }
    // Confirm returns the target and the trimmed label, and closes the input.
    assert_eq!(
        state.switch_confirm_rename(),
        Some(("main".to_string(), "My main".to_string()))
    );
    assert!(!state.is_renaming());
}

#[test]
fn switch_begin_rename_is_a_noop_on_the_root_row_and_when_already_open() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    // Cursor on the root row: there is no session to rename.
    assert!(state.list().root_selected());
    assert!(!state.switch_begin_rename());
    assert!(!state.is_renaming());

    // On a session it opens, and a second begin while open is a no-op.
    state.switch_move_down();
    assert!(state.switch_begin_rename());
    assert!(!state.switch_begin_rename());

    // It also refuses to open while a create input is up.
    state.rename_cancel();
    state.switch_begin_create(Vec::new());
    assert!(!state.switch_begin_rename());
}

#[test]
fn rename_accessors_are_none_when_not_renaming() {
    let mut state = state();
    assert!(!state.is_renaming());
    assert!(state.rename().is_none());
    assert!(state.rename_mut().is_none());
    assert!(state.switch_confirm_rename().is_none());
}

#[test]
fn rename_can_be_cancelled() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    state.switch_move_down();
    state.switch_begin_rename();
    state.rename_mut().unwrap().push_char('x');
    state.rename_cancel();
    assert!(!state.is_renaming());
}

#[test]
fn restore_sessions_carries_the_display_name_onto_the_pane_label() {
    let mut state = state();
    let mut record = session_record("feature", 1);
    record.display_name = Some("Login flow".to_string());
    state.restore_sessions(vec![session_record("main", 1), record]);
    // Row 0 is the root; worktree index 0 = "main" (no override), 1 = "feature".
    assert_eq!(state.list().display_label(0), "main");
    assert_eq!(state.list().display_label(1), "Login flow");
    // The branch / identity is unchanged, so commands still key on it.
    assert_eq!(
        state.list().worktrees()[1].branch.as_deref(),
        Some("feature")
    );
}

// --- 在席 (Focus) ------------------------------------------------------

#[test]
fn enter_focus_activates_a_row_and_resets_the_surface() {
    let mut state = state(); // root, main, feature
    state.enter_focus(2); // feature
    assert_eq!(state.mode(), Mode::Focus);
    assert_eq!(state.list().active_index(), 2);
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.focused_session_name(), "feature");
    assert_eq!(state.focus_menu_cursor(), 0);
    assert_eq!(state.focus_prompt(), "");
}

#[test]
fn enter_focus_named_focuses_the_matching_session() {
    let mut state = state(); // root, main, feature
    assert!(state.enter_focus_named("feature"));
    assert_eq!(state.mode(), Mode::Focus);
    assert_eq!(state.list().active_index(), 2);
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.focused_session_name(), "feature");
    assert_eq!(state.focus_menu_cursor(), 0);
    assert_eq!(state.focus_prompt(), "");
}

#[test]
fn enter_focus_named_is_a_no_op_for_an_unknown_session() {
    let mut state = state();
    // An unmatched name leaves the mode and cursor untouched (still 統括, root row).
    assert!(!state.enter_focus_named("nope"));
    assert_eq!(state.mode(), Mode::Overview);
    assert!(state.list().root_active());
}

#[test]
fn enter_focus_on_the_root_row_names_root() {
    let mut state = state();
    state.enter_focus(0);
    assert!(state.list().root_active());
    assert_eq!(state.focused_session_name(), ROOT_NAME);
}

#[test]
fn leave_focus_returns_to_overview() {
    let mut state = state();
    state.enter_focus(1);
    state.leave_focus();
    assert_eq!(state.mode(), Mode::Overview);
}

#[test]
fn entering_focus_selects_the_new_tab() {
    // Entering 在席 fresh lands on the "+ new" action surface, not a pane preview.
    let mut by_row = state();
    by_row.enter_focus(1);
    assert!(by_row.focus_on_new_tab());
    let mut by_name = state();
    assert!(by_name.enter_focus_named("feature"));
    assert!(by_name.focus_on_new_tab());
}

#[test]
fn an_idle_session_is_always_on_the_new_tab() {
    // With no live panes published the "+ new" tab is the only one — navigation is
    // inert (no pane index to make active) and the selector never leaves it.
    let mut state = state();
    state.enter_focus(1);
    assert!(state.focus_on_new_tab());
    assert_eq!(state.focus_tab_next(), None);
    assert!(state.focus_on_new_tab());
    assert_eq!(state.focus_tab_prev(), None);
    assert!(state.focus_on_new_tab());
}

#[test]
fn leaving_attached_lands_on_the_new_tab() {
    // `Ctrl-T` (leave_attached) drops back to 在席 on the trailing "+ new" launch
    // surface — the action menu over the (still-live) panes — not a pane preview.
    let mut state = state();
    state.enter_focus(1);
    // `leave_attached` clears the surface; the event loop republishes the strip on
    // the next frame, so set it afterwards to mirror that.
    state.leave_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert_eq!(state.mode(), Mode::Focus);
    assert!(state.focus_on_new_tab());
}

#[test]
fn focus_discard_new_tab_steps_back_onto_the_active_pane() {
    // On "+ new" over live panes (as after `Ctrl-T`), discarding steps onto the
    // active pane's tab so it previews again, staying in 在席.
    let mut state = state();
    state.enter_focus(1);
    state.leave_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(state.focus_on_new_tab());
    assert!(state.focus_discard_new_tab());
    assert!(!state.focus_on_new_tab());
    assert_eq!(state.mode(), Mode::Focus);
}

#[test]
fn focus_discard_new_tab_is_inert_without_live_panes() {
    // With no pane behind "+ new" (an idle session) there is nothing to step back
    // to, so discarding is a no-op and the caller backs out of 在席 instead.
    let mut state = state();
    state.enter_focus(1);
    assert!(state.focus_on_new_tab());
    assert!(!state.focus_discard_new_tab());
    assert!(state.focus_on_new_tab());
}

#[test]
fn focus_tab_next_walks_panes_then_the_new_tab() {
    let mut state = state();
    state.enter_focus(1);
    // Two live panes (active = 0), then the "+ new" tab after them; entry lands on
    // "+ new".
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    assert!(state.focus_on_new_tab());
    // "+ new" wraps to pane 0.
    assert_eq!(state.focus_tab_next(), Some(0));
    assert!(!state.focus_on_new_tab());
    // pane 0 -> pane 1.
    assert_eq!(state.focus_tab_next(), Some(1));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(!state.focus_on_new_tab());
    // pane 1 (last) -> "+ new".
    assert_eq!(state.focus_tab_next(), None);
    assert!(state.focus_on_new_tab());
}

#[test]
fn focus_tab_prev_walks_the_new_tab_then_panes() {
    let mut state = state();
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    // On the "+ new" tab, prev wraps to the last pane.
    assert!(state.focus_on_new_tab());
    assert_eq!(state.focus_tab_prev(), Some(1));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(!state.focus_on_new_tab());
    // pane 1 -> pane 0.
    assert_eq!(state.focus_tab_prev(), Some(0));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    assert!(!state.focus_on_new_tab());
    // pane 0 (first) -> "+ new".
    assert_eq!(state.focus_tab_prev(), None);
    assert!(state.focus_on_new_tab());
}

#[test]
fn focus_menu_hides_ai_until_the_local_llm_is_available() {
    // Focus a session row (not the root) so `close` is offered.
    let mut state = state();
    state.enter_focus(1);
    // By default the local LLM is unavailable, so the `ai` command is hidden.
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["terminal", "agent", "close"]);
    // Once the local LLM is usable (enabled + model pulled), `ai` appears.
    state.set_ai_available(true);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["terminal", "agent", "ai", "close"]);
}

#[test]
fn focus_menu_hides_close_on_the_root_row() {
    // The root row is the workspace itself, not a session, so it cannot be
    // closed and `close` is not offered there.
    let mut state = state();
    state.enter_focus(0);
    assert!(state.list().root_active());
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["terminal", "agent"]);
    // A session row still offers `close`.
    state.enter_focus(1);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["terminal", "agent", "close"]);
}

#[test]
fn focus_menu_cursor_moves_and_wraps_and_selects() {
    let mut state = state();
    state.enter_focus(1);
    // With `ai` hidden: terminal (0, highlighted by default), agent (1),
    // close (2).
    assert_eq!(state.focus_selected_command().unwrap().name, "terminal");
    state.focus_menu_move_down();
    assert_eq!(state.focus_selected_command().unwrap().name, "agent");
    state.focus_menu_move_down();
    state.focus_menu_move_down(); // wraps to the top
    assert_eq!(state.focus_menu_cursor(), 0);
    // Up from the top wraps to the bottom (`close`).
    state.focus_menu_move_up();
    assert_eq!(state.focus_selected_command().unwrap().name, "close");
}

#[test]
fn focus_prompt_edits_completes_and_hints_in_session_scope() {
    let mut state = state();
    state.enter_focus(1);
    for c in "ter".chars() {
        state.focus_prompt_mut().insert(c);
    }
    state.focus_prompt_mut().backspace(); // "te"
                                          // "te" uniquely completes to "terminal" (a session command).
    let completion = state.focus_prompt_complete();
    assert_eq!(state.focus_prompt(), "terminal");
    assert!(completion.candidates.is_empty());
    // The hint is computed in the session scope: arguments show usage.
    state.focus_prompt_mut().insert(' ');
    assert!(matches!(state.focus_prompt_hint(), Hint::Usage { .. }));
}

#[test]
fn focus_prompt_caret_moves_and_edits_mid_line() {
    let mut state = state();
    state.enter_focus(1);
    for c in "abc".chars() {
        state.focus_prompt_mut().insert(c);
    }
    assert_eq!(state.focus_prompt_cursor(), 3);
    state.focus_prompt_mut().move_home();
    assert_eq!(state.focus_prompt_cursor(), 0);
    state.focus_prompt_mut().delete_forward(); // removes 'a' → "bc"
    assert_eq!(state.focus_prompt(), "bc");
    state.focus_prompt_mut().move_right(); // between 'b' and 'c'
    state.focus_prompt_mut().insert('x'); // "bxc"
    assert_eq!(state.focus_prompt(), "bxc");
    state.focus_prompt_mut().move_left();
    state.focus_prompt_mut().backspace(); // removes 'b' → "xc"
    assert_eq!(state.focus_prompt(), "xc");
    state.focus_prompt_mut().move_end();
    assert_eq!(state.focus_prompt_cursor(), 2);
}

#[test]
fn focus_prompt_submit_runs_a_session_command() {
    let mut state = state();
    state.enter_focus(1);
    for c in "terminal".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let submission = state.focus_prompt_submit();
    assert_eq!(submission.effect, Effect::OpenTerminal);
    assert_eq!(submission.recorded.as_deref(), Some("terminal"));
    // The prompt is cleared and the command recorded in history.
    assert_eq!(state.focus_prompt(), "");
    assert_eq!(state.history, vec!["terminal"]);
}

#[test]
fn focus_prompt_runs_a_text_command_into_a_modal() {
    // A text-dumping utility (`man`) typed in the 在席 prompt opens the text
    // modal too, rather than appending to the log.
    let mut state = state();
    state.enter_focus(1);
    for c in "man".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let submission = state.focus_prompt_submit();
    assert_eq!(submission.effect, Effect::ShowText("Help"));
    let modal = state.text_modal().expect("man opens a modal");
    assert!(modal.lines.iter().any(|l| l.text.contains("Available")));
}

#[test]
fn text_modal_opens_scrolls_and_closes() {
    let mut state = state();
    let lines: Vec<LogLine> = (0..30)
        .map(|i| LogLine::output(format!("line {i}")))
        .collect();
    state.open_text_modal("Help", lines);
    assert_eq!(state.text_modal().unwrap().scroll, 0);
    // Scrolling up at the top is a no-op.
    state.text_modal_scroll_up();
    assert_eq!(state.text_modal().unwrap().scroll, 0);
    // Scrolling down advances, clamped so the last `visible` lines stay shown.
    state.text_modal_scroll_down(10);
    assert_eq!(state.text_modal().unwrap().scroll, 1);
    for _ in 0..100 {
        state.text_modal_scroll_down(10);
    }
    assert_eq!(state.text_modal().unwrap().scroll, 30 - 10);
    state.text_modal_scroll_up();
    assert_eq!(state.text_modal().unwrap().scroll, 30 - 10 - 1);
    state.close_text_modal();
    assert!(state.text_modal().is_none());
    // Scroll calls are no-ops once closed.
    state.text_modal_scroll_down(10);
    state.text_modal_scroll_up();
    assert!(state.text_modal().is_none());
}

#[test]
fn focus_prompt_submit_on_empty_input_is_a_noop() {
    let mut state = state();
    state.enter_focus(1);
    let submission = state.focus_prompt_submit();
    assert_eq!(submission.effect, Effect::None);
    assert!(submission.recorded.is_none());
    assert!(state.history.is_empty());
}

#[test]
fn focus_prompt_runs_the_coming_soon_ai_command() {
    let mut state = state();
    state.enter_focus(1);
    for c in "ai hi".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let submission = state.focus_prompt_submit();
    assert_eq!(submission.effect, Effect::None);
    assert!(state.log().last().unwrap().text.contains("coming soon"));
}

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
fn leaving_attached_drops_the_tab_strip() {
    let mut state = state();
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.leave_attached();
    assert!(state.terminal_tabs().is_none());
}

#[test]
fn enter_overview_clears_transient_state() {
    let mut state = state();
    state.enter_switch(ReturnMode::Overview);
    state.switch_begin_create(Vec::new());
    state.enter_focus(1);
    state.focus_prompt_mut().insert('x');
    state.focus_menu_move_down();
    state.enter_overview();
    assert_eq!(state.mode(), Mode::Overview);
    assert!(!state.is_creating());
    assert_eq!(state.focus_prompt(), "");
    assert_eq!(state.focus_menu_cursor(), 0);
    assert_eq!(state.input(), "");
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

fn session_record(name: &str, worktrees: usize) -> SessionRecord {
    SessionRecord {
        name: name.to_string(),
        display_name: None,
        note: None,
        root: std::path::PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
        worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
        created_at: Utc::now(),
    }
}

#[test]
fn apply_session_outcome_logs_and_rebuilds_the_pane_from_sessions() {
    let mut state = state();
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Created session \"x\""),
        sessions: Some(vec![session_record("main", 1), session_record("x", 1)]),
        select: Some("x".to_string()),
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
    });
    assert_eq!(state.sessions().len(), 1);
    assert_eq!(state.list().worktrees().len(), 1);
    assert_eq!(state.list().selected_index(), 0);

    // A failure outcome only logs; the pane is unchanged.
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::error("session failed"),
        sessions: None,
        select: None,
    });
    assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
    assert_eq!(state.list().worktrees().len(), 1);
    assert_eq!(state.sessions().len(), 1);
}

#[test]
fn set_tasks_round_trips_the_panel_rows() {
    use super::super::tasks::{TaskMark, TaskRow};
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
        LogLine::output("Created session \"x\" 🐰"),
        Some(vec![
            session_record("main", 1),
            session_record("feature", 1),
            session_record("x", 1),
        ]),
    );
    assert!(state.log().last().unwrap().text.contains("Created session"));
    assert_eq!(state.sessions().len(), 3);
    assert_eq!(state.list().selected_name(), "feature");

    // A failure (no refreshed list) only logs; the pane is untouched.
    state.apply_task_completion(LogLine::error("session remove failed"), None);
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
        root: PathBuf::from("/repo/.usagi/sessions/feature"),
        worktrees: vec![merged_a, merged_b, local_c],
        created_at: Utc::now(),
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
        root: PathBuf::from("/repo/.usagi/sessions/empty"),
        worktrees: Vec::new(),
        created_at: Utc::now(),
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
fn open_remove_modal_lists_the_session_names() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    assert!(state.remove_modal().is_none());
    state.open_remove_modal(false);
    let modal = state.remove_modal().unwrap();
    assert_eq!(modal.names(), ["alpha", "beta"]);
    assert_eq!(modal.cursor(), 0);
    assert_eq!(modal.selected_count(), 0);
    assert!(!modal.is_empty());
    assert!(!modal.is_selected(0));
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
    let (names, force) = state.submit_remove_modal().unwrap();
    assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
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
    state.apply_task_completion(LogLine::output("Created session \"x\" 🐰"), None);
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Renamed \"x\""),
        sessions: None,
        select: None,
    });
    assert!(spy.recorded.borrow().is_empty());

    // A failure line from either path is persisted through the sink.
    state.apply_task_completion(LogLine::error("session remove failed: boom"), None);
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::error("rename failed: locked"),
        sessions: None,
        select: None,
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

// --- session note editor -----------------------------------------------

/// A state with two sessions recorded, the cursor moved onto the first one
/// (`alpha`), so the note-editor helpers act on a real session row.
fn state_on_alpha() -> HomeState {
    let mut state = state();
    let mut alpha = session_record("alpha", 1);
    alpha.note = Some("existing".to_string());
    state.restore_sessions(vec![alpha, session_record("beta", 1)]);
    state.switch_move_down(); // root -> alpha
    state
}

#[test]
fn switch_begin_note_opens_the_editor_prefilled_with_the_sessions_note() {
    let mut state = state_on_alpha();
    assert!(state.switch_begin_note());
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    // Pre-filled with the recorded note, caret parked at its end.
    assert_eq!(editor.area().text(), "existing");
    assert!(!editor.reattach());
    assert!(!state.note_editor_reattaches());

    // A second begin is a no-op while one is already open.
    assert!(!state.switch_begin_note());
}

#[test]
fn switch_begin_note_is_a_noop_on_the_root_row() {
    // The cursor starts on the root row, which is the workspace, not a session.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    assert!(!state.switch_begin_note());
    assert!(state.note_editor().is_none());
}

#[test]
fn open_focused_note_targets_the_active_session_and_carries_reattach() {
    let mut state = state_on_alpha();
    state.enter_focus(state.list().selected_index()); // 在席 on alpha
                                                      // 没入's `Ctrl-E` opens with reattach = true.
    assert!(state.open_focused_note(true));
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    assert!(editor.reattach());
    assert!(state.note_editor_reattaches());
    // Already open: a second open is refused.
    assert!(!state.open_focused_note(true));

    // 在席's `Ctrl-E` opens with reattach = false (close returns to the action
    // surface, no pane to re-attach).
    state.note_editor_cancel();
    assert!(state.open_focused_note(false));
    assert!(!state.note_editor_reattaches());
}

#[test]
fn open_focused_note_is_a_noop_on_the_root_row() {
    // The root row is focused by default; it has no note to edit.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.enter_focus(0);
    assert!(!state.open_focused_note(false));
    assert!(state.note_editor().is_none());
}

#[test]
fn note_editor_edits_confirm_and_cancel() {
    let mut state = state_on_alpha();
    // A session with no note opens an empty editor.
    let mut beta = session_record("beta", 1);
    beta.note = None;
    state.restore_sessions(vec![session_record("alpha", 1), beta]);
    state.switch_move_down();
    state.switch_move_down(); // alpha -> beta
    assert!(state.switch_begin_note());
    let area = state.note_editor_mut().unwrap().area_mut();
    assert!(area.is_empty());
    area.insert('h');
    area.insert('i');
    // Confirm returns the target, the typed text, and reattach=false (切替).
    let (target, text, reattach) = state.confirm_note_editor().unwrap();
    assert_eq!(target, "beta");
    assert_eq!(text, "hi");
    assert!(!reattach);
    assert!(state.note_editor().is_none());
    // Confirm / cancel with nothing open are no-ops.
    assert!(state.confirm_note_editor().is_none());

    // Cancel discards an open editor.
    state.switch_begin_note();
    assert!(state.note_editor().is_some());
    state.note_editor_cancel();
    assert!(state.note_editor().is_none());
    assert!(!state.note_editor_reattaches());
}

#[test]
fn selected_session_note_reads_the_cursor_rows_note() {
    // `state_on_alpha` records alpha with the note "existing" and parks the
    // cursor on it.
    let state = state_on_alpha();
    assert_eq!(state.selected_session_note(), Some("existing"));
}

#[test]
fn selected_session_note_is_none_on_root_and_for_a_noteless_session() {
    let mut state = state();
    // `session_record` records no note.
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // The cursor starts on the root row (not a session).
    assert_eq!(state.selected_session_note(), None);
    // Moving onto a session with no note still reports `None`.
    state.switch_move_down();
    assert_eq!(state.selected_session_note(), None);
}
