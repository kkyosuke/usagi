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
