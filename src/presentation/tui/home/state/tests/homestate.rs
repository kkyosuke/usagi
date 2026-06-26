use super::*;

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
