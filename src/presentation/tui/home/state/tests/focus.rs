use super::*;

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
fn previous_session_row_tracks_the_last_focused_session() {
    let mut state = state(); // root, main, feature
                             // Nothing to jump back to before a second session is focused.
    assert_eq!(state.previous_session_row(), None);
    state.enter_focus(1); // main; left the root behind
    assert_eq!(state.previous_session_row(), Some(0));
    state.enter_focus(2); // feature; left "main" behind
    assert_eq!(state.previous_session_row(), Some(1)); // back to "main"
}

#[test]
fn refresh_sessions_keeps_the_previous_session_jump_target() {
    let mut state = state();
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("created"),
        sessions: Some(vec![session_record("alpha", 1), session_record("beta", 1)]),
        select: Some("beta".to_string()),
        root_note: None,
    });
    state.enter_focus(1); // alpha
    state.enter_focus(2); // beta; previous = alpha
    assert_eq!(state.previous_session_row(), Some(1)); // alpha is row 1

    // A re-sync that keeps alpha must keep the jump target pointing at it.
    state.refresh_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    assert_eq!(state.previous_session_row(), Some(1));

    // A re-sync that drops alpha leaves no target to jump back to.
    state.refresh_sessions(vec![session_record("beta", 1)]);
    assert_eq!(state.previous_session_row(), None);
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
    // An unmatched name leaves the mode and cursor untouched (still 切替, root row).
    assert!(!state.enter_focus_named("nope"));
    assert_eq!(state.mode(), Mode::Switch);
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
fn leave_focus_returns_to_base_switch() {
    let mut state = state();
    state.enter_focus(1);
    state.leave_focus();
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.switch_return(), ReturnMode::Base);
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
    // The menu lists the remaining commands in the fixed display order.
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal", "close"]);
    // Once the local LLM is usable (enabled + model pulled), `ai` appears.
    state.set_ai_available(true);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal", "ai", "close"]);
}

#[test]
fn session_menu_rank_orders_the_known_commands_and_ranks_others_last() {
    use super::super::session_menu_rank;
    // The fixed 在席 display order: agent, terminal, ai, close.
    assert!(session_menu_rank("agent") < session_menu_rank("terminal"));
    assert!(session_menu_rank("terminal") < session_menu_rank("ai"));
    assert!(session_menu_rank("ai") < session_menu_rank("close"));
    // Any command outside the four sorts after all of them.
    assert!(session_menu_rank("close") < session_menu_rank("session"));
}

#[test]
fn focus_menu_hides_close_on_the_root_row() {
    // The root row is the workspace itself, not a session, so it cannot be
    // closed and `close` is not offered there.
    let mut state = state();
    state.enter_focus(0);
    assert!(state.list().root_active());
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal"]);
    // A session row still offers `close`.
    state.enter_focus(1);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal", "close"]);
}

#[test]
fn focus_menu_hides_agent_when_an_agent_pane_is_already_open() {
    // Focus a session row so `close` is offered too.
    let mut state = state();
    state.enter_focus(1);
    // No live panes yet: `agent` is offered.
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal", "close"]);
    // Once the session publishes a live `agent` pane, the launch command is hidden
    // (its agent is already running); the rest keep the fixed display order.
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["terminal", "close"]);
    // A session whose only live panes are plain terminals keeps offering `agent`.
    state.set_terminal_tabs(vec!["terminal".to_string()], 0);
    let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
    assert_eq!(names, vec!["agent", "terminal", "close"]);
}

#[test]
fn preview_menu_commands_follow_the_cursor_not_the_active_row() {
    // The 切替 preview shows what *selecting* the highlighted row reveals, so its
    // command list (and `close` visibility) must track the cursor, independent of
    // whichever row happens to be active.
    let mut state = state();

    // Active row is the root, cursor moved onto a session row: the preview is the
    // session's, so `close` is offered even though the active row cannot close.
    state.enter_focus(0);
    state.switch_move_down();
    assert!(state.list().root_active());
    assert!(!state.list().root_selected());
    let names: Vec<&str> = state
        .preview_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "terminal", "close"]);

    // Active row is a session, cursor moved back onto the root row: the preview is
    // the root's, so `close` is hidden even though the active session could close.
    state.enter_focus(1);
    state.switch_move_up();
    assert!(!state.list().root_active());
    assert!(state.list().root_selected());
    let names: Vec<&str> = state
        .preview_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "terminal"]);
}

#[test]
fn focus_menu_cursor_moves_and_wraps_and_selects() {
    let mut state = state();
    state.enter_focus(1);
    // With `ai` hidden, fixed order: agent (0, highlighted by default),
    // terminal (1), close (2).
    assert_eq!(state.focus_selected_command().unwrap().name, "agent");
    state.focus_menu_move_down();
    assert_eq!(state.focus_selected_command().unwrap().name, "terminal");
    state.focus_menu_move_down();
    state.focus_menu_move_down(); // wraps to the top
    assert_eq!(state.focus_menu_cursor(), 0);
    // Up from the top wraps to the bottom (`close`).
    state.focus_menu_move_up();
    assert_eq!(state.focus_selected_command().unwrap().name, "close");
}

#[test]
fn agent_choice_round_trips_and_is_consumed_once() {
    use crate::domain::settings::AgentCli;
    let mut state = state();
    // Defaults: Claude is the configured agent, nothing installed, no choice.
    assert_eq!(state.default_agent(), AgentCli::Claude);
    assert!(state.installed_agents().is_empty());
    assert_eq!(state.take_agent_choice(), None);
    // Injected configuration is read back.
    state.set_default_agent(AgentCli::Codex);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    assert_eq!(state.default_agent(), AgentCli::Codex);
    assert_eq!(
        state.installed_agents(),
        [AgentCli::Claude, AgentCli::Codex]
    );
    // A recorded choice is returned once, then cleared.
    state.set_agent_choice(Some(AgentCli::Claude));
    assert_eq!(state.take_agent_choice(), Some(AgentCli::Claude));
    assert_eq!(state.take_agent_choice(), None);
}

#[test]
fn focus_menu_agent_picker_expands_only_with_a_choice_and_navigates_agents() {
    use crate::domain::settings::AgentCli;
    let mut state = state();
    state.enter_focus(1);
    // Fixed order (agent, terminal, close) highlights the `agent` row on entry.
    assert_eq!(state.focus_selected_command().unwrap().name, "agent");
    // With fewer than two agents installed there is nothing to pick: no expand.
    state.set_installed_agents(vec![AgentCli::Claude]);
    assert!(!state.focus_menu_agent_can_expand());
    state.focus_menu_expand_agent();
    assert!(!state.focus_menu_expanded());
    // Two installed agents: the row can expand, highlighting the default's index.
    state.set_default_agent(AgentCli::Codex);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    assert!(state.focus_menu_agent_can_expand());
    state.focus_menu_expand_agent();
    assert!(state.focus_menu_expanded());
    assert_eq!(state.focus_menu_agent_cursor(), Some(1)); // Codex is index 1
    assert_eq!(state.focus_menu_selected_agent(), Some(AgentCli::Codex));
    // Navigation now walks the agents, not the commands.
    state.focus_menu_move_up();
    assert_eq!(state.focus_menu_selected_agent(), Some(AgentCli::Claude));
    // Collapsing restores the menu and reports it was open.
    assert!(state.focus_menu_collapse_agent());
    assert!(!state.focus_menu_expanded());
    assert_eq!(state.focus_menu_selected_agent(), None);
    assert!(!state.focus_menu_collapse_agent());
}

#[test]
fn focus_menu_agent_picker_does_not_expand_off_the_agent_row() {
    use crate::domain::settings::AgentCli;
    let mut state = state();
    state.enter_focus(1);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    // Move off the `agent` row (down to `terminal`): the picker cannot open there.
    state.focus_menu_move_down();
    assert_eq!(state.focus_selected_command().unwrap().name, "terminal");
    assert!(!state.focus_menu_agent_can_expand());
    state.focus_menu_expand_agent();
    assert!(!state.focus_menu_expanded());
}

#[test]
fn focus_menu_agent_cursor_is_none_without_installed_agents() {
    let mut state = state();
    state.enter_focus(1);
    // Even if the menu were somehow expanded, no installed agents means the
    // picker reports no highlight (the renderer then draws no sub-rows).
    assert_eq!(state.focus_menu_agent_cursor(), None);
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
fn focus_prompt_completes_command_arguments_and_lists_candidates() {
    let mut state = state();
    state.enter_focus(1);
    // `man ` completes its argument against the command names — ambiguous here,
    // so the candidates are listed in the log (mirroring the palette line).
    for c in "man ".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let before = state.log().len();
    let completion = state.focus_prompt_complete();
    assert!(!completion.candidates.is_empty());
    let listed = state.log().last().unwrap();
    assert!(listed.text.contains("man"));
    assert_eq!(state.log().len(), before + 1);
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
    assert_eq!(
        submission.recorded.as_ref().map(|e| e.command.as_str()),
        Some("terminal")
    );
    assert_eq!(
        submission
            .recorded
            .as_ref()
            .and_then(|e| e.session.as_deref()),
        Some("main")
    );
    assert!(submission.recorded.as_ref().is_some_and(|e| e.success));
    // The prompt is cleared and the command recorded in history.
    assert_eq!(state.focus_prompt(), "");
    assert_eq!(state.cmdline.history, vec!["terminal"]);
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
    assert_eq!(
        submission.effect,
        Effect::ShowText {
            title: "Help",
            size: ModalSize::Large,
        }
    );
    let modal = state.text_modal().expect("man opens a modal");
    assert!(modal.lines.iter().any(|l| l.text.contains("Available")));
}

#[test]
fn text_modal_opens_scrolls_and_closes() {
    let mut state = state();
    let lines: Vec<LogLine> = (0..30)
        .map(|i| LogLine::output(format!("line {i}")))
        .collect();
    state.open_text_modal("Help", lines, ModalSize::Normal);
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
    assert!(state.cmdline.history.is_empty());
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

#[test]
fn focus_select_pane_tab_clamps_to_a_live_pane_and_clears_the_new_tab() {
    let mut state = state();
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    // Entry lands on "+ new"; clicking a concrete pane tab clears it and returns
    // that pane's index.
    assert!(state.focus_on_new_tab());
    assert_eq!(state.focus_select_pane_tab(1), Some(1));
    assert!(!state.focus_on_new_tab());
    // Out-of-range clicks clamp onto the last pane.
    assert_eq!(state.focus_select_pane_tab(9), Some(1));
}

#[test]
fn focus_select_pane_tab_without_live_panes_falls_back_to_the_new_tab() {
    let mut state = state();
    state.enter_focus(1);
    // An idle session has no live panes, so there is nothing to select: the
    // selector snaps back to "+ new".
    assert_eq!(state.focus_select_pane_tab(0), None);
    assert!(state.focus_on_new_tab());
}
