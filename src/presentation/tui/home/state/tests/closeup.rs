use super::*;

// --- 集中 (Closeup) ------------------------------------------------------

#[test]
fn enter_closeup_activates_a_row_and_resets_the_surface() {
    let mut state = state(); // root, main, feature
    state.enter_closeup(2); // feature
    assert_eq!(state.mode(), Mode::Closeup);
    assert_eq!(state.list().active_index(), 2);
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.focused_session_name(), "feature");
    assert_eq!(state.closeup_menu_cursor(), 0);
    assert_eq!(state.closeup_prompt(), "");
}

#[test]
fn focus_modal_opens_from_switch_and_from_a_closeup_pane_preview() {
    let mut state = state(); // root, main, feature
    state.overview_move_down(); // main
    state.open_focus_modal();
    assert_eq!(state.mode(), Mode::Closeup);
    assert_eq!(state.focused_session_name(), "main");
    assert!(state.closeup_action_overlay());

    // When Closeup is browsing an existing pane tab, `Ctrl-O a` brings the Focus
    // modal back over that preview instead of creating a third top-level mode.
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    assert_eq!(state.closeup_select_pane_tab(0), Some(0));
    assert!(!state.closeup_action_overlay());
    state.open_focus_modal();
    assert!(state.closeup_action_overlay());
    assert!(state.closeup_action_over_pane());
}

#[test]
fn focus_modal_is_a_noop_while_already_attached() {
    let mut state = state();
    state.enter_closeup(1);
    state.show_attached();

    state.open_focus_modal();

    assert_eq!(state.mode(), Mode::Closeup);
    assert!(state.closeup_attached());
    assert!(!state.closeup_action_overlay());
}

#[test]
fn closeup_action_overlay_holds_for_both_surfaces_on_the_action_tab() {
    let mut state = state(); // root, main, feature
                             // Not in 集中: nothing floats.
    assert!(!state.closeup_action_overlay());

    // Idle 集中 on the menu UI (the default): the menu floats as an overlay.
    state.enter_closeup(1);
    assert_eq!(state.session_action_ui(), SessionActionUi::Menu);
    assert!(state.closeup_action_overlay());

    // The prompt surface floats too — the setting only picks which surface the
    // box holds, not whether it floats.
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_closeup(1);
    assert!(state.closeup_action_overlay());

    // With live panes it floats on the "+ new" tab (the action surface) but not
    // once the selector steps onto a pane tab — for both surfaces.
    for ui in [SessionActionUi::Menu, SessionActionUi::Prompt] {
        state.set_session_action_ui(ui);
        state.enter_closeup(1);
        state.set_terminal_tabs(vec!["agent".to_string()], 0);
        assert!(state.closeup_on_new_tab());
        assert!(state.closeup_action_overlay());
        state.closeup_tab_next(); // "+ new" -> the sole pane tab
        assert!(!state.closeup_on_new_tab());
        assert!(!state.closeup_action_overlay());
    }
}

#[test]
fn closeup_action_overlay_yields_to_the_loading_indicator_open_overlays_and_palette() {
    // The idle menu floats by default; each screen-owning surface suppresses it so
    // two boxes never fight for the pane.
    let mut loading = state();
    loading.enter_closeup(1);
    assert!(loading.closeup_action_overlay());
    loading.step_loading("起動中…"); // a momentary launch owns the pane
    assert!(!loading.closeup_action_overlay());

    // An open overlay (here a text modal a menu command dumped) captures the screen.
    let mut modal = state();
    modal.enter_closeup(1);
    modal.open_text_modal("Help", vec![LogLine::output("x")], ModalSize::Normal);
    assert!(!modal.closeup_action_overlay());

    // The `:` command palette likewise.
    let mut palette = state();
    palette.enter_closeup(1);
    palette.open_command_palette();
    assert!(!palette.closeup_action_overlay());
}

#[test]
fn previous_session_row_tracks_the_last_focused_session() {
    let mut state = state(); // root, main, feature
                             // Nothing to jump back to before a second session is focused.
    assert_eq!(state.previous_session_row(), None);
    state.enter_closeup(1); // main; left the root behind
    assert_eq!(state.previous_session_row(), Some(0));
    state.enter_closeup(2); // feature; left "main" behind
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
    state.enter_closeup(1); // alpha
    state.enter_closeup(2); // beta; previous = alpha
    assert_eq!(state.previous_session_row(), Some(1)); // alpha is row 1

    // A re-sync that keeps alpha must keep the jump target pointing at it.
    state.refresh_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
    assert_eq!(state.previous_session_row(), Some(1));

    // A re-sync that drops alpha leaves no target to jump back to.
    state.refresh_sessions(vec![session_record("beta", 1)]);
    assert_eq!(state.previous_session_row(), None);
}

#[test]
fn enter_closeup_named_focuses_the_matching_session() {
    let mut state = state(); // root, main, feature
    assert!(state.enter_closeup_named("feature"));
    assert_eq!(state.mode(), Mode::Closeup);
    assert_eq!(state.list().active_index(), 2);
    assert_eq!(state.list().selected_index(), 2);
    assert_eq!(state.focused_session_name(), "feature");
    assert_eq!(state.closeup_menu_cursor(), 0);
    assert_eq!(state.closeup_prompt(), "");
}

#[test]
fn enter_closeup_named_is_a_no_op_for_an_unknown_session() {
    let mut state = state();
    // An unmatched name leaves the mode and cursor untouched (still 選択, root row).
    assert!(!state.enter_closeup_named("nope"));
    assert_eq!(state.mode(), Mode::Switch);
    assert!(state.list().root_active());
}

#[test]
fn focus_switch_named_selects_the_matching_session_without_entering_closeup() {
    let mut state = state();
    state.enter_closeup(1);
    assert_eq!(state.mode(), Mode::Closeup);

    assert!(state.focus_switch_named("feature"));
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.focused_session_name(), "feature");

    // An unmatched name leaves the mode and cursor untouched.
    assert!(!state.focus_switch_named("nope"));
    assert_eq!(state.mode(), Mode::Switch);
    assert_eq!(state.focused_session_name(), "feature");
}

#[test]
fn enter_closeup_on_the_root_row_names_root() {
    let mut state = state();
    state.enter_closeup(0);
    assert!(state.list().root_active());
    assert_eq!(state.focused_session_name(), ROOT_NAME);
}

#[test]
fn leave_closeup_returns_to_base_overview() {
    let mut state = state();
    state.enter_closeup(1);
    state.leave_closeup();
    assert_eq!(state.mode(), Mode::Switch);
}

#[test]
fn entering_closeup_selects_the_new_tab() {
    // Entering 集中 fresh lands on the "+ new" action surface, not a pane preview.
    let mut by_row = state();
    by_row.enter_closeup(1);
    assert!(by_row.closeup_on_new_tab());
    let mut by_name = state();
    assert!(by_name.enter_closeup_named("feature"));
    assert!(by_name.closeup_on_new_tab());
}

#[test]
fn entering_closeup_existing_selects_a_live_pane_instead_of_new_tab() {
    // Existing-pane Closeup entry lands on the session's current live pane when
    // one exists, rather than opening the "+ new" action surface.
    let mut live = state();
    assert!(live.enter_closeup_named_existing("feature"));
    live.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(!live.closeup_on_new_tab());

    // An idle session still falls back to "+ new" because there is no existing
    // pane to show.
    let mut idle = state();
    assert!(idle.enter_closeup_named_existing("feature"));
    assert!(idle.closeup_on_new_tab());
}

#[test]
fn enter_closeup_named_existing_is_a_no_op_for_an_unknown_session() {
    let mut state = state();
    // An unmatched name leaves the mode and cursor untouched (still 選択, root row).
    assert!(!state.enter_closeup_named_existing("nope"));
    assert_eq!(state.mode(), Mode::Switch);
    assert!(state.list().root_active());
}

#[test]
fn an_idle_session_is_always_on_the_new_tab() {
    // With no live panes published the "+ new" tab is the only one — navigation is
    // inert (no pane index to make active) and the selector never leaves it.
    let mut state = state();
    state.enter_closeup(1);
    assert!(state.closeup_on_new_tab());
    assert_eq!(state.closeup_tab_next(), None);
    assert!(state.closeup_on_new_tab());
    assert_eq!(state.closeup_tab_prev(), None);
    assert!(state.closeup_on_new_tab());
}

#[test]
fn leaving_attached_lands_on_the_new_tab() {
    // A bare `leave_attached` (the shell exited, or a quit was raised) drops back
    // to 集中 on the trailing "+ new" launch surface — not a pane preview. The
    // deliberate zoom-out layers `closeup_action_over_active_pane` on top (see the
    // dedicated tests below).
    let mut state = state();
    state.enter_closeup(1);
    // `leave_attached` clears the surface; the event loop republishes the strip on
    // the next frame, so set it afterwards to mirror that.
    state.leave_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert_eq!(state.mode(), Mode::Closeup);
    assert!(state.closeup_on_new_tab());
    assert!(!state.closeup_action_over_pane());
}

#[test]
fn zooming_out_floats_the_menu_over_the_pane_tab() {
    // `Ctrl-T` / `Ctrl-O a` (leave_attached + closeup_action_over_active_pane) keeps
    // the selector on the pane the zoom left: the strip grows no "+ new" chip for
    // a tab that was never created, the pane's live preview keeps showing, and
    // the action menu floats over it.
    let mut state = state();
    state.enter_closeup(1);
    state.leave_attached();
    state.closeup_action_over_active_pane();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert_eq!(state.mode(), Mode::Closeup);
    assert!(!state.closeup_on_new_tab());
    assert!(state.closeup_action_over_pane());
    assert!(state.closeup_action_overlay());
    // Dismissing the menu (`Esc` once the re-attach arming is spent) leaves the
    // pane previewing — one step short of leaving 集中 — and reports it was up
    // exactly once.
    assert!(state.close_closeup_action_over_pane());
    assert!(!state.closeup_action_overlay());
    assert!(!state.close_closeup_action_over_pane());
}

#[test]
fn zooming_out_floats_the_prompt_over_the_pane_tab_too() {
    // The prompt surface floats like the menu, so a zoom-out keeps the selector
    // on the pane the zoom left (its preview showing behind the floating prompt)
    // rather than jumping to a "+ new" landing.
    let mut state = state();
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_closeup(1);
    state.leave_attached();
    state.closeup_action_over_active_pane();
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    assert!(!state.closeup_on_new_tab());
    assert!(state.closeup_action_over_pane());
    assert!(state.closeup_action_overlay());
    // And `?` / `End` stay literal edits while the prompt floats over the pane.
    assert!(state.closeup_prompt_capturing());
}

#[test]
fn closeup_prompt_capturing_tracks_the_prompt_command_line() {
    // The `?` / `End` guards read `closeup_prompt_capturing`: true only while the
    // Prompt command line is the surface capturing keys (on the "+ new" tab or
    // floating over a pane), false for the menu or a bare pane preview.
    let mut state = state();

    // Menu surface: never captures — `?` / `End` keep their note / cheat-sheet
    // bindings.
    state.enter_closeup(1);
    assert_eq!(state.session_action_ui(), SessionActionUi::Menu);
    assert!(!state.closeup_prompt_capturing());

    // Prompt on the "+ new" tab (an idle session, no panes): captures.
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_closeup(1);
    assert!(state.closeup_on_new_tab());
    assert!(state.closeup_prompt_capturing());

    // Prompt with the selector on a bare pane tab (not "+ new", not floating over
    // it): the pane previews, so the prompt is not capturing.
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.closeup_tab_next(); // "+ new" -> the sole pane tab
    assert!(!state.closeup_on_new_tab());
    assert!(!state.closeup_action_over_pane());
    assert!(!state.closeup_prompt_capturing());
}

#[test]
fn walking_or_clicking_tabs_dismisses_the_menu_over_a_pane() {
    // Moving the tab selector is browsing previews: the floating menu steps aside
    // whichever way the move happens (Ctrl-N / Ctrl-P / a tab click).
    let mut state = state();
    state.enter_closeup(1);
    state.leave_attached();
    state.closeup_action_over_active_pane();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    assert!(state.closeup_action_over_pane());
    state.closeup_tab_next();
    assert!(!state.closeup_action_over_pane());

    state.closeup_action_over_active_pane();
    state.closeup_tab_prev();
    assert!(!state.closeup_action_over_pane());

    state.closeup_action_over_active_pane();
    state.closeup_select_pane_tab(1);
    assert!(!state.closeup_action_over_pane());
}

#[test]
fn attaching_or_reentering_closeup_drops_the_menu_over_a_pane() {
    // Attaching consumes the floating menu, and every fresh 集中 entry (or a bare
    // leave_attached) resets it, so no stale menu survives a surface change.
    let mut state = state();
    state.enter_closeup(1);
    state.leave_attached();
    state.closeup_action_over_active_pane();
    assert!(state.closeup_action_over_pane());
    state.show_attached();
    assert!(!state.closeup_action_over_pane());

    state.closeup_action_over_active_pane();
    state.enter_closeup(1);
    assert!(!state.closeup_action_over_pane());

    state.closeup_action_over_active_pane();
    state.leave_attached();
    assert!(!state.closeup_action_over_pane());
}

#[test]
fn closeup_discard_new_tab_steps_back_onto_the_active_pane() {
    // On "+ new" over live panes (as after `Ctrl-T`), discarding steps onto the
    // active pane's tab so it previews again, staying in 集中.
    let mut state = state();
    state.enter_closeup(1);
    state.leave_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(state.closeup_on_new_tab());
    assert!(state.closeup_discard_new_tab());
    assert!(!state.closeup_on_new_tab());
    assert_eq!(state.mode(), Mode::Closeup);
}

#[test]
fn closeup_discard_new_tab_is_inert_without_live_panes() {
    // With no pane behind "+ new" (an idle session) there is nothing to step back
    // to, so discarding is a no-op and the caller backs out of 集中 instead.
    let mut state = state();
    state.enter_closeup(1);
    assert!(state.closeup_on_new_tab());
    assert!(!state.closeup_discard_new_tab());
    assert!(state.closeup_on_new_tab());
}

#[test]
fn closeup_tab_next_walks_panes_then_the_new_tab() {
    let mut state = state();
    state.enter_closeup(1);
    // Two live panes (active = 0), then the "+ new" tab after them; entry lands on
    // "+ new".
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    assert!(state.closeup_on_new_tab());
    // "+ new" wraps to pane 0.
    assert_eq!(state.closeup_tab_next(), Some(0));
    assert!(!state.closeup_on_new_tab());
    // pane 0 -> pane 1.
    assert_eq!(state.closeup_tab_next(), Some(1));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(!state.closeup_on_new_tab());
    // pane 1 (last) -> "+ new".
    assert_eq!(state.closeup_tab_next(), None);
    assert!(state.closeup_on_new_tab());
}

#[test]
fn closeup_tab_prev_walks_the_new_tab_then_panes() {
    let mut state = state();
    state.enter_closeup(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    // On the "+ new" tab, prev wraps to the last pane.
    assert!(state.closeup_on_new_tab());
    assert_eq!(state.closeup_tab_prev(), Some(1));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    assert!(!state.closeup_on_new_tab());
    // pane 1 -> pane 0.
    assert_eq!(state.closeup_tab_prev(), Some(0));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    assert!(!state.closeup_on_new_tab());
    // pane 0 (first) -> "+ new".
    assert_eq!(state.closeup_tab_prev(), None);
    assert!(state.closeup_on_new_tab());
}

#[test]
fn closeup_menu_hides_prompt_taking_ai_command() {
    // Focus a session row (not the root) so `close` is offered.
    let mut state = state();
    state.enter_closeup(1);
    // `ai <prompt>` needs typed arguments, so it is always kept out of the menu.
    // `chat` (local LLM) is gated on availability, so by default (LLM unavailable)
    // the menu lists the pane actions and `close` in alphabetical order.
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert!(!names.contains(&"ai"));
    assert_eq!(names, vec!["agent", "close", "diff", "terminal"]);
    // Once the local LLM is usable (enabled + model pulled), `chat` appears — but
    // `ai` (prompt-taking) never does.
    state.set_ai_available(true);
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert!(!names.contains(&"ai"));
    assert_eq!(names, vec!["agent", "chat", "close", "diff", "terminal"]);
}

#[test]
fn session_menu_commands_are_alphabetical() {
    // The 集中 menu lists the Session-scope commands sorted by name.
    let mut state = state();
    state.enter_closeup(1);
    state.set_ai_available(true);
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[test]
fn closeup_menu_hides_close_and_diff_on_the_root_row() {
    // The root row is the workspace itself, not a session, so it cannot be closed
    // or diffed — neither `close` nor `diff` is offered there.
    let mut state = state();
    state.enter_closeup(0);
    assert!(state.list().root_active());
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "terminal"]);
    // A session row offers both `diff` and `close`.
    state.enter_closeup(1);
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "close", "diff", "terminal"]);
}

#[test]
fn closeup_menu_keeps_agent_when_an_agent_pane_is_already_open() {
    // Focus a session row so `close` is offered too.
    let mut state = state();
    state.enter_closeup(1);
    // No live panes yet: `agent` is offered.
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "close", "diff", "terminal"]);
    // A session holds one agent per CLI, so `agent` stays offered even once a live
    // agent pane is published — launching it adds a different CLI's agent (and
    // re-selecting the running CLI just re-focuses its tab).
    state.set_terminal_tabs(vec!["Claude".to_string(), "terminal".to_string()], 0);
    let names: Vec<&str> = state
        .closeup_menu_commands()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["agent", "close", "diff", "terminal"]);
}

#[test]
fn closeup_menu_cursor_moves_and_wraps_and_selects() {
    let mut state = state();
    state.enter_closeup(1);
    // With `ai` hidden, alphabetical order: agent (0, highlighted by default),
    // close (1), diff (2), terminal (3).
    assert_eq!(state.closeup_selected_command().unwrap().name, "agent");
    state.closeup_menu_move_down();
    assert_eq!(state.closeup_selected_command().unwrap().name, "close");
    state.closeup_menu_move_down();
    assert_eq!(state.closeup_selected_command().unwrap().name, "diff");
    state.closeup_menu_move_down();
    state.closeup_menu_move_down(); // wraps to the top
    assert_eq!(state.closeup_menu_cursor(), 0);
    // Up from the top wraps to the bottom (`terminal`).
    state.closeup_menu_move_up();
    assert_eq!(state.closeup_selected_command().unwrap().name, "terminal");
}

#[test]
fn closeup_menu_terminal_picker_expands_only_on_terminal_row() {
    let mut state = state();
    state.enter_closeup(1);
    // Starts on agent, so the terminal picker cannot open yet.
    assert!(!state.closeup_menu_terminal_can_expand());
    state.closeup_menu_expand_terminal();
    assert!(!state.closeup_menu_expanded());

    // Move down from agent to terminal (the last row) and open the terminal picker.
    state.closeup_menu_move_down(); // agent -> close
    state.closeup_menu_move_down(); // close -> diff
    state.closeup_menu_move_down(); // diff -> terminal
    assert_eq!(state.closeup_selected_command().unwrap().name, "terminal");
    assert!(state.closeup_menu_terminal_can_expand());
    state.closeup_menu_expand_terminal();
    assert!(state.closeup_menu_expanded());
    assert_eq!(state.closeup_menu_terminal_cursor(), Some(0));
    assert_eq!(state.closeup_menu_selected_terminal_action(), Some("open"));
    state.closeup_menu_move_down();
    assert_eq!(state.closeup_menu_selected_terminal_action(), Some("new"));
}

#[test]
fn closeup_menu_close_picker_expands_only_on_close_row() {
    let mut state = state();
    state.enter_closeup(1);
    // Starts on agent, so the close picker cannot open yet.
    assert!(!state.closeup_close_can_expand());
    state.closeup_menu_expand_close();
    assert!(!state.closeup_menu_expanded());

    state.closeup_menu_move_down(); // agent -> close
    assert_eq!(state.closeup_selected_command().unwrap().name, "close");
    assert!(state.closeup_close_can_expand());
    state.closeup_menu_expand_close();
    assert!(state.closeup_menu_expanded());
    assert_eq!(state.closeup_close_cursor(), Some(0));
    assert!(!state.closeup_menu_selected_close_force());
    state.closeup_menu_move_down();
    assert_eq!(state.closeup_close_cursor(), Some(1));
    assert!(state.closeup_menu_selected_close_force());
}

#[test]
fn chat_overlay_opens_closes_and_carries_the_configured_model() {
    let mut state = state();
    // The injected model is read back and used when the overlay opens.
    state.set_local_llm_model("qwen2.5-coder:3b");
    assert_eq!(state.local_llm_model(), "qwen2.5-coder:3b");
    // No overlay yet: both accessors report none (covers the `chat_mut` miss arm).
    assert!(state.chat().is_none());
    assert!(state.chat_mut().is_none());
    // Opening binds the chat to the configured model.
    state.enter_closeup(1);
    state.open_chat();
    assert_eq!(state.chat().unwrap().model(), "qwen2.5-coder:3b");
    // Mutable access edits the composed line.
    state.chat_mut().unwrap().input_mut().insert('a');
    assert_eq!(state.chat().unwrap().input().value(), "a");
    // Closing clears the overlay; a second close is a no-op.
    state.close_chat();
    assert!(state.chat().is_none());
    state.close_chat();
    assert!(state.chat().is_none());
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
fn closeup_menu_agent_picker_expands_only_with_a_choice_and_navigates_agents() {
    use crate::domain::settings::AgentCli;
    let mut state = state();
    state.enter_closeup(1);
    // Fixed order (agent, terminal, diff, close) highlights the `agent` row on entry.
    assert_eq!(state.closeup_selected_command().unwrap().name, "agent");
    // With fewer than two agents installed there is nothing to pick: no expand.
    state.set_installed_agents(vec![AgentCli::Claude]);
    assert!(!state.closeup_menu_agent_can_expand());
    state.closeup_menu_expand_agent();
    assert!(!state.closeup_menu_expanded());
    // Two installed agents: the row can expand, highlighting the default's index.
    state.set_default_agent(AgentCli::Codex);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    assert!(state.closeup_menu_agent_can_expand());
    state.closeup_menu_expand_agent();
    assert!(state.closeup_menu_expanded());
    assert_eq!(state.closeup_menu_agent_cursor(), Some(1)); // Codex is index 1
    assert_eq!(state.closeup_menu_selected_agent(), Some(AgentCli::Codex));
    // Navigation now walks the agents, not the commands.
    state.closeup_menu_move_up();
    assert_eq!(state.closeup_menu_selected_agent(), Some(AgentCli::Claude));
    // Collapsing restores the menu and reports it was open.
    assert!(state.closeup_menu_collapse_agent());
    assert!(!state.closeup_menu_expanded());
    assert_eq!(state.closeup_menu_selected_agent(), None);
    assert!(!state.closeup_menu_collapse_agent());
}

#[test]
fn closeup_menu_agent_picker_does_not_expand_off_the_agent_row() {
    use crate::domain::settings::AgentCli;
    let mut state = state();
    state.enter_closeup(1);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    // Move off the `agent` row (down to `close`): the picker cannot open there.
    state.closeup_menu_move_down();
    assert_eq!(state.closeup_selected_command().unwrap().name, "close");
    assert!(!state.closeup_menu_agent_can_expand());
    state.closeup_menu_expand_agent();
    assert!(!state.closeup_menu_expanded());
}

#[test]
fn closeup_menu_agent_cursor_is_none_without_installed_agents() {
    let mut state = state();
    state.enter_closeup(1);
    // Even if the menu were somehow expanded, no installed agents means the
    // picker reports no highlight (the renderer then draws no sub-rows).
    assert_eq!(state.closeup_menu_agent_cursor(), None);
}

#[test]
fn closeup_prompt_edits_completes_and_hints_in_session_scope() {
    let mut state = state();
    state.enter_closeup(1);
    for c in "ter".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    state.closeup_prompt_mut().backspace(); // "te"
                                            // "te" uniquely completes to "terminal" (a session command).
    let completion = state.closeup_prompt_complete();
    assert_eq!(state.closeup_prompt(), "terminal");
    assert!(completion.candidates.is_empty());
    // The hint is computed in the session scope: arguments show usage.
    state.closeup_prompt_mut().insert(' ');
    assert!(matches!(state.closeup_prompt_hint(), Hint::Usage { .. }));
}

#[test]
fn closeup_prompt_completes_command_arguments_and_lists_candidates() {
    let mut state = state();
    state.enter_closeup(1);
    // `man ` completes its argument against the command names — ambiguous here,
    // so the candidates are listed in the log (mirroring the palette line).
    for c in "man ".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    let before = state.log().len();
    let completion = state.closeup_prompt_complete();
    assert!(!completion.candidates.is_empty());
    let listed = state.log().last().unwrap();
    assert!(listed.text.contains("man"));
    assert_eq!(state.log().len(), before + 1);
}

#[test]
fn closeup_prompt_caret_moves_and_edits_mid_line() {
    let mut state = state();
    state.enter_closeup(1);
    for c in "abc".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    assert_eq!(state.closeup_prompt_cursor(), 3);
    state.closeup_prompt_mut().move_home();
    assert_eq!(state.closeup_prompt_cursor(), 0);
    state.closeup_prompt_mut().delete_forward(); // removes 'a' → "bc"
    assert_eq!(state.closeup_prompt(), "bc");
    state.closeup_prompt_mut().move_right(); // between 'b' and 'c'
    state.closeup_prompt_mut().insert('x'); // "bxc"
    assert_eq!(state.closeup_prompt(), "bxc");
    state.closeup_prompt_mut().move_left();
    state.closeup_prompt_mut().backspace(); // removes 'b' → "xc"
    assert_eq!(state.closeup_prompt(), "xc");
    state.closeup_prompt_mut().move_end();
    assert_eq!(state.closeup_prompt_cursor(), 2);
}

#[test]
fn closeup_prompt_submit_runs_a_session_command() {
    let mut state = state();
    state.enter_closeup(1);
    for c in "terminal".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    let submission = state.closeup_prompt_submit();
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
    assert_eq!(state.closeup_prompt(), "");
    assert_eq!(state.cmdline.history, vec!["terminal"]);
}

#[test]
fn closeup_prompt_runs_a_text_command_into_a_modal() {
    // A text-dumping utility (`man`) typed in the 集中 prompt opens the text
    // modal too, rather than appending to the log.
    let mut state = state();
    state.enter_closeup(1);
    for c in "man".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    let submission = state.closeup_prompt_submit();
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
fn closeup_prompt_submit_on_empty_input_is_a_noop() {
    let mut state = state();
    state.enter_closeup(1);
    let submission = state.closeup_prompt_submit();
    assert_eq!(submission.effect, Effect::None);
    assert!(submission.recorded.is_none());
    assert!(state.cmdline.history.is_empty());
}

#[test]
fn closeup_prompt_runs_the_ai_prompt_command() {
    let mut state = state();
    state.enter_closeup(1);
    for c in "ai hi".chars() {
        state.closeup_prompt_mut().insert(c);
    }
    let submission = state.closeup_prompt_submit();
    assert_eq!(submission.effect, Effect::OpenAgentPrompt("hi".to_string()));
    assert!(submission.recorded.is_some());
}

#[test]
fn closeup_select_pane_tab_clamps_to_a_live_pane_and_clears_the_new_tab() {
    let mut state = state();
    state.enter_closeup(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    // Entry lands on "+ new"; clicking a concrete pane tab clears it and returns
    // that pane's index.
    assert!(state.closeup_on_new_tab());
    assert_eq!(state.closeup_select_pane_tab(1), Some(1));
    assert!(!state.closeup_on_new_tab());
    // Out-of-range clicks clamp onto the last pane.
    assert_eq!(state.closeup_select_pane_tab(9), Some(1));
}

#[test]
fn closeup_select_pane_tab_without_live_panes_falls_back_to_the_new_tab() {
    let mut state = state();
    state.enter_closeup(1);
    // An idle session has no live panes, so there is nothing to select: the
    // selector snaps back to "+ new".
    assert_eq!(state.closeup_select_pane_tab(0), None);
    assert!(state.closeup_on_new_tab());
}
