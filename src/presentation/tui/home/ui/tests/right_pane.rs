use super::*;

#[test]
fn right_pane_previews_the_cursor_row_in_switch() {
    // 切替 (Switch) is the default mode: the right pane previews the would-be
    // screen for the cursor row.
    let state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    assert_eq!(state.mode(), Mode::Switch);
    let preview = stripped(&right_pane_contents(&state, 40, 12));
    // The idle root row rests the mascot (the workspace-root header still shows).
    assert!(preview.contains("root"));
    assert!(preview.contains("workspace root"));
    assert!(preview.contains("(='-')"));
}

#[test]
fn switch_preview_shows_a_live_session_as_a_reattach() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    // Move the cursor off the root onto the session row.
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("feat"));
    // Header carries the git status and the agent state. The session is live but
    // has reported no turn, so it shows as `ready` (idle, awaiting input).
    assert!(preview.contains("local"));
    assert!(preview.contains("ready"));
    // A live session with no snapshot yet falls back to the re-attach label,
    // not the action menu.
    assert!(preview.contains("live terminal"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_a_live_session_as_its_actual_screen() {
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    // The event loop snapshots the highlighted live session before painting.
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ echo hi".to_string(), "hi".to_string()],
        None,
    ));
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    // The real terminal screen is shown, not the placeholder label.
    assert!(preview.contains("$ echo hi"));
    assert!(preview.contains("hi"));
    assert!(!preview.contains("live terminal"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_shows_the_tab_strip_beside_the_header_for_a_live_session() {
    // In 切替 the highlighted live session's tabs render on the header's own row,
    // so the identity and the `←`/`→` targets read together; the live screen
    // follows below. The event loop publishes the strip from the pool before
    // painting.
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let lines = switch_preview(&state, 80, 12);
    // The header (name + status + agent) and both numbered chips share the top
    // row; the live screen follows below.
    let top = console::strip_ansi_codes(&lines[0]).into_owned();
    assert!(top.contains("feat") && top.contains("ready"));
    // A dim divider separates the identity from the tab chips.
    assert!(top.contains('│'));
    assert!(top.contains("1 agent") && top.contains("2 terminal"));
    assert!(stripped(&lines).contains("$ echo hi"));
}

#[test]
fn switch_preview_keeps_a_fixed_identity_width_so_tabs_do_not_jitter() {
    // The header identity is a fixed width, so the divider and tabs land in the
    // same column whichever session the cursor is on — the row does not shift as
    // the cursor moves between sessions — and a long name is clipped, not spilled.
    let mut short = worktree(Some("x"), false, BranchStatus::Local);
    short.path = PathBuf::from("/repo/short");
    let mut long = worktree(
        Some("feature/really-long-branch-name-here"),
        false,
        BranchStatus::Synced,
    );
    long.path = PathBuf::from("/repo/long");
    let mut state = HomeState::new("usagi", vec![short, long], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/short"), PathBuf::from("/repo/long")].into(),
        ..Default::default()
    });
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ ".to_string()], None));
    state.enter_switch(super::super::super::state::ReturnMode::Base);

    // The display column of the divider, used as the anchor for the tabs. Measured
    // by the terminal's East Asian width (the identity is padded with the `_cjk`
    // helpers, and a clipping ellipsis is two columns wide there), which is where
    // the divider actually lands on screen.
    let divider_col = |lines: &[String]| {
        let top = console::strip_ansi_codes(&lines[0]).into_owned();
        let at = top.find('│').expect("the divider is drawn");
        crate::presentation::tui::widgets::measure_width_cjk(&top[..at])
    };

    state.switch_move_down(); // cursor on the short-named session
    let short_top = switch_preview(&state, 80, 12);
    let short_col = divider_col(&short_top);

    state.switch_move_down(); // cursor on the long-named session
    let long_lines = switch_preview(&state, 80, 12);
    let long_col = divider_col(&long_lines);

    // The divider (and so the tabs beside it) sits in the same column for both.
    assert_eq!(short_col, long_col);
    // The long name is clipped with an ellipsis rather than pushing the divider.
    assert!(console::strip_ansi_codes(&long_lines[0])
        .into_owned()
        .contains('…'));
}

#[test]
fn switch_preview_shows_the_root_live_session_as_its_screen() {
    // Regression: the root row (`⌂ root`) hard-coded `live = false`, so a running
    // root agent previewed its action menu in 切替 instead of its live screen —
    // it only re-appeared once selected. With the workspace root recorded, the
    // root row is matched against the live set like any worktree, so its running
    // agent previews live.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo")].into(),
        ..Default::default()
    });
    // The event loop snapshots the highlighted live session (the root here)
    // before painting.
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ claude".to_string(), "How can I help?".to_string()],
        None,
    ));
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    // The cursor starts on the root row, so no navigation is needed.
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("root"));
    // The live root agent's actual screen is shown, not the action menu.
    assert!(preview.contains("$ claude"));
    assert!(preview.contains("How can I help?"));
    assert!(!preview.contains("Run a command"));
}

#[test]
fn switch_preview_rests_the_mascot_for_an_idle_root_on_the_menu_ui() {
    // The menu UI focuses as a floating modal, so an idle root previews the
    // resting mascot and a quip — not the inline choices it once promised.
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("workspace root"));
    assert!(preview.contains("(='-')"), "the mascot rests in the pane");
    assert!(preview.contains("Enter"), "the quip nods to Enter");
    assert!(!preview.contains("Run a command"));
    assert!(!preview.contains("live terminal"));
}

#[test]
fn switch_preview_rests_the_mascot_for_an_idle_session_on_the_menu_ui() {
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    // An idle session rests the mascot rather than previewing the menu, whose
    // choices selecting only reveals as a floating modal.
    assert!(preview.contains("pushed"));
    assert!(preview.contains("(='-')"), "the mascot rests in the pane");
    assert!(!preview.contains("Run a command"));
    assert!(!preview.contains("live terminal"));
}

#[test]
fn switch_preview_centres_the_idle_mascot_with_a_quip_below_it() {
    // The mascot sits in the middle of the pane (blank rows above and below) with
    // its witty English quip on the row beneath it.
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let lines: Vec<String> = switch_preview(&state, 40, 12)
        .iter()
        .map(|l| console::strip_ansi_codes(l).trim_end().to_string())
        .collect();
    let face = lines
        .iter()
        .position(|l| l.contains("(='-')"))
        .expect("the mascot's face shows");
    // The face is the middle of the three-row mascot, so the quip sits three rows
    // below it (mascot feet, a blank separator, then the caption).
    let quip = &lines[face + 3];
    assert!(!quip.trim().is_empty(), "a quip sits below the mascot");
    assert!(quip.contains("Enter"), "the quip nods to Enter");
    // The mascot is pushed down off the top row, so it reads as centred.
    assert!(face > 2, "the mascot is not pinned to the top");
}

#[test]
fn switch_right_pane_fades_the_preview_but_keeps_its_text() {
    // In 切替 the keyboard is on the session list, so the composited right pane
    // fades the whole preview (each row dimmed) to signal it is not where
    // selection happens. The fade only re-styles the rows, so the right pane
    // shows exactly the preview's text. (Styling is off in non-TTY tests, so we
    // assert the text survives rather than that a dim code was added — see
    // `dim_row_strips_existing_colour_but_keeps_the_text`.)
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    let pane = stripped(&right_pane_contents(&state, 40, 12));
    assert_eq!(pane, preview, "the faded right pane is the preview's text");
}

#[test]
fn switch_preview_fills_the_pane_without_a_pinned_key_hint() {
    // The 切替 right pane no longer pins a key hint to its bottom row — the keys
    // live in the footer, so the preview uses the pane's full height and does not
    // duplicate the footer's key list.
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = switch_preview(&state, 60, 12);
    // The pane fills its rows, and the bottom row is no longer a key hint.
    assert_eq!(preview.len(), 12);
    let last = console::strip_ansi_codes(preview.last().unwrap()).into_owned();
    assert!(!last.contains("Enter focus"));
    assert!(!last.contains("x close tab"));
    // The idle mascot fills the pane.
    assert!(stripped(&preview).contains("(='-')"));
}

#[test]
fn switch_preview_shows_an_idle_session_as_its_prompt_when_prompt_ui() {
    // With the Prompt action UI, the idle-session preview must mirror the prompt
    // surface (`❯`) — not the command menu — so the 切替 preview matches what
    // focusing the session actually reveals (regression: it previewed the menu
    // regardless of the setting).
    let idle = worktree(Some("feat"), false, BranchStatus::Pushed);
    let mut state = HomeState::new("usagi", vec![idle], None);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_switch(super::super::super::state::ReturnMode::Base);
    state.switch_move_down();
    let preview = stripped(&switch_preview(&state, 40, 12));
    assert!(preview.contains("pushed"));
    assert!(preview.contains('❯'), "the prompt surface is previewed");
    assert!(
        !preview.contains("Run a command"),
        "the command menu must not be shown in prompt mode"
    );
    assert!(!preview.contains("live terminal"));
}

#[test]
fn right_pane_shows_the_focus_menu_or_prompt() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    // Menu (the default) floats as an overlay modal over the frame: it is not
    // drawn inline in the right pane, and its title carries the identity while its
    // body lists the session commands.
    assert!(state.focus_menu_overlay());
    assert!(
        !stripped(&right_pane_contents(&state, 77, 12)).contains("Run a command:"),
        "the menu is not drawn inline in the right pane"
    );
    let menu = stripped(&render_frame(24, 120, &state));
    assert!(menu.contains("session: main"));
    assert!(menu.contains("terminal"));
    assert!(menu.contains("agent"));
    assert!(menu.contains('›'));

    // Prompt shows a typed command line with the session-scope hint, inline in the
    // right pane — only the menu surface floats as an overlay.
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "ter".chars() {
        state.focus_prompt_mut().insert(c);
    }
    assert!(!state.focus_menu_overlay());
    let prompt = stripped(&right_pane_contents(&state, 40, 12));
    assert!(prompt.contains("session: main"));
    assert!(prompt.contains("❯ ter"));
    // The session-scope hint lists terminal as a match.
    assert!(prompt.contains("terminal"));
}

#[test]
fn focus_menu_agent_row_shows_the_default_and_expands_into_a_picker() {
    use crate::domain::settings::AgentCli;
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    // The agent row always names the default CLI a plain launch uses.
    let base = stripped(&render_frame(24, 120, &state));
    assert!(base.contains("Launch Claude"));
    // The expand affordance (▸ / "→ pick agent") shows while the agent row is the
    // highlighted one — and the fixed order highlights `agent` on entry.
    assert!(base.contains('▸'));
    assert!(base.contains("→ pick agent"));
    // Moving off the agent row hides the affordance.
    state.focus_menu_move_down(); // agent -> terminal
    let off_agent = stripped(&render_frame(24, 120, &state));
    assert!(off_agent.contains("Launch Claude"));
    assert!(off_agent.contains("  Launch Claude"));
    assert!(!off_agent.contains("→ pick agent"));
    // Back onto the agent row, expanding lists the installed agents.
    state.focus_menu_move_up(); // terminal -> agent
                                // Expanding lists every installed agent (default tagged) and swaps the hint.
    state.focus_menu_expand_agent();
    let expanded = stripped(&render_frame(24, 120, &state));
    assert!(expanded.contains('▾'));
    assert!(expanded.contains("Codex"));
    assert!(expanded.contains("(default)"));
    assert!(expanded.contains("Enter launch"));
}

#[test]
fn focus_close_row_shows_chevron_and_expands_into_a_picker() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    // On entry the cursor is on `agent`, so `close` (and `terminal`) reserve the
    // 2-column chevron slot with blanks — their descriptions never shift as the
    // cursor moves on/off them (no CLS), like the `agent` row.
    let off_close = stripped(&render_frame(24, 120, &state));
    assert!(off_close.contains("  Close the focused session"));
    assert!(off_close.contains("  Open a shell"));
    // Alphabetical order: agent is first and close is second; move down once to close.
    state.focus_menu_move_down();
    // The close row shows ▸ and "→ expand" in the hint while cursor is on it.
    let on_close = stripped(&render_frame(24, 120, &state));
    assert!(on_close.contains('▸'));
    assert!(on_close.contains("→ expand"));
    // Expanding shows the two sub-rows (plain close and --force) and swaps the hint.
    state.focus_menu_expand_close();
    let expanded = stripped(&render_frame(24, 120, &state));
    assert!(expanded.contains('▾'));
    assert!(expanded.contains("close --force"));
    assert!(expanded.contains("(safe)"));
    assert!(expanded.contains("discard uncommitted changes"));
    assert!(expanded.contains("Enter run"));
    // Collapsing hides the sub-rows.
    state.focus_menu_collapse_close();
    let collapsed = stripped(&render_frame(24, 120, &state));
    assert!(!collapsed.contains("close --force"));
}

#[test]
fn focus_menu_terminal_row_expands_into_open_and_new_actions() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.focus_menu_move_up(); // alphabetical order: agent wraps up to terminal (last)
    let base = stripped(&render_frame(24, 120, &state));
    assert!(base.contains("terminal"));
    assert!(base.contains('▸'));
    assert!(base.contains("→ pick terminal"));
    state.focus_menu_expand_terminal();
    let expanded = stripped(&render_frame(24, 120, &state));
    assert!(expanded.contains('▾'));
    assert!(expanded.contains("open"));
    assert!(expanded.contains("new"));
    assert!(expanded.contains("(default)"));
    assert!(expanded.contains("new terminal"));
    assert!(expanded.contains("Enter launch"));
}

#[test]
fn focus_menu_reserves_the_widest_expansion_then_scrolls_a_short_pane() {
    use crate::domain::settings::AgentCli;
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    // The most sub-menu-heavy picker: more installed agents than any other picker.
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![
        AgentCli::Claude,
        AgentCli::Codex,
        AgentCli::CodexFugu,
        AgentCli::Gemini,
    ]);

    // The collapsed menu already reserves the widest expansion's height, so opening
    // the agent picker neither resizes the box nor clips into a `N more` marker —
    // every agent shows in place.
    let collapsed = focus_menu_body(&state, 60, 30);
    state.focus_menu_expand_agent();
    let expanded = focus_menu_body(&state, 60, 30);
    assert_eq!(
        collapsed.len(),
        expanded.len(),
        "the box height does not change when a picker opens"
    );
    let roomy = console::strip_ansi_codes(&expanded.join("\n")).into_owned();
    assert!(
        roomy.contains("Gemini"),
        "the widest picker shows every agent: {roomy:?}"
    );
    assert!(
        !roomy.contains("more"),
        "a reserved-height box needs no scroll marker: {roomy:?}"
    );

    // A pane too short to hold the reserved height caps the window and the overflow
    // is summarised with a scroll marker instead.
    let cramped = focus_menu_body(&state, 60, 11);
    let joined = console::strip_ansi_codes(&cramped.join("\n")).into_owned();
    assert!(joined.contains("more"), "a short pane scrolls: {joined:?}");

    // Moving the picker cursor down scrolls the hidden agents into view without
    // growing the capped window.
    for _ in 0..3 {
        state.focus_menu_move_down();
    }
    let scrolled = focus_menu_body(&state, 60, 11);
    assert_eq!(scrolled.len(), cramped.len());
    let joined = console::strip_ansi_codes(&scrolled.join("\n")).into_owned();
    assert!(
        joined.contains("Gemini"),
        "the last agent scrolls in: {joined:?}"
    );
}

#[test]
fn focus_shows_pane_tabs_with_a_trailing_new_tab_and_the_action_surface() {
    // With live panes published, 在席 gains a tab strip — one chip per pane plus a
    // trailing "+ new" tab. On the "+ new" tab (the default on entry) the pane
    // preview does not show; the menu action surface floats as an overlay modal
    // over the frame while the tab strip stays inline behind it.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    // The tab strip stays inline in the right pane (identity + chips + "+ new"),
    // but the menu itself no longer draws there — it floats as an overlay.
    let pane = stripped(&right_pane_contents(&state, 77, 12));
    assert!(pane.contains("main"));
    assert!(pane.contains("agent"));
    assert!(pane.contains("+ new"));
    assert!(!pane.contains("Run a command:"), "the menu is not inline");
    assert!(
        !pane.contains("$ echo hi"),
        "no pane preview on the + new tab"
    );
    // The floating menu modal carries the command surface, composited over the
    // frame; the pane preview still does not show.
    assert!(state.focus_menu_overlay());
    let out = stripped(&render_frame(24, 120, &state));
    assert!(out.contains("+ new"));
    assert!(out.contains("Run a command:"));
    assert!(!out.contains("$ echo hi"));
}

#[test]
fn zoomed_out_menu_floats_over_the_pane_preview() {
    // Zooming out of a live pane (`Ctrl-T` / `Ctrl-O a`) keeps the pane's own tab
    // selected: no "+ new" chip appears for a tab that was never created, the
    // pane's live preview keeps showing behind the floating menu, and both share
    // the frame.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.leave_attached();
    state.focus_menu_over_active_pane();
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    assert!(state.focus_menu_overlay());
    let pane = stripped(&right_pane_contents(&state, 77, 12));
    assert!(pane.contains("$ echo hi"), "the pane preview stays drawn");
    assert!(!pane.contains("+ new"), "no chip for an uncreated tab");
    let out = stripped(&render_frame(24, 120, &state));
    assert!(out.contains("Run a command:"), "the menu floats over it");
    assert!(out.contains("session: main"));
}

#[test]
fn focus_new_tab_with_panes_shows_the_prompt_surface() {
    // The "+ new" tab honours the Prompt action UI just as the menu does — with
    // live panes its command line shows below the strip (header-less).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    for c in "ter".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let out = stripped(&right_pane_contents(&state, 100, 12));
    assert!(out.contains("+ new"));
    assert!(out.contains("❯ ter"));
}

#[test]
fn focus_previews_the_selected_pane_when_a_pane_tab_is_chosen() {
    // Off the "+ new" tab (as after navigating onto a pane tab with `Ctrl-N`/`Ctrl-P`),
    // the selected pane's live snapshot previews below the strip instead of the
    // action surface, and the "+ new" chip is gone — it shows only while selected.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.focus_tab_prev(); // "+ new" -> the last (active) pane tab
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let out = stripped(&right_pane_contents(&state, 100, 12));
    assert!(!out.contains("+ new")); // stepping off "+ new" drops the chip
    assert!(out.contains("$ echo hi")); // the selected pane previews
    assert!(!out.contains("Run a command:")); // not the action surface
}

#[test]
fn focus_pane_tab_falls_back_to_a_hint_until_the_first_snapshot() {
    // A pane tab is selected but no snapshot has arrived yet: a live-terminal hint
    // stands in until one does.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["agent".to_string()], 0);
    state.focus_tab_next(); // "+ new" -> the sole pane tab
    let out = stripped(&right_pane_contents(&state, 60, 12));
    assert!(out.contains("live terminal"));
    assert!(out.contains("再アタッチ"));
}

#[test]
fn focus_prompt_shows_usage_for_arguments() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "terminal ".chars() {
        state.focus_prompt_mut().insert(c);
    }
    let prompt = stripped(&right_pane_contents(&state, 60, 12));
    assert!(prompt.contains("usage"));
    assert!(prompt.contains("terminal"));
}

#[test]
fn focus_prompt_has_no_hint_for_an_unknown_command_word() {
    // An unknown word yields `Hint::None`, so no hint rows are drawn.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.set_session_action_ui(SessionActionUi::Prompt);
    state.enter_focus(1);
    for c in "zzz".chars() {
        state.focus_prompt_mut().insert(c);
    }
    // The header, blank, and prompt lines are present, but no hint rows follow.
    let rows = right_pane_contents(&state, 60, 12);
    let text = stripped(&rows);
    assert!(text.contains("❯ zzz"));
    // An unknown word yields `Hint::None`, so neither a usage line nor example
    // rows are drawn below the prompt.
    assert!(!text.contains("usage"));
    assert!(!text.contains("e.g."));
}

#[test]
fn right_pane_shows_the_terminal_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    // The active session's header tops the pane, filling the reserved tab-strip
    // rows; the terminal (or its starting hint) follows below.
    let starting = right_pane_contents(&state, 40, 5);
    assert!(console::strip_ansi_codes(&starting[0])
        .into_owned()
        .contains("main"));
    assert!(starting[super::TAB_BAR_ROWS].contains("Starting terminal"));
    // Once a snapshot arrives, its rows are shown below the header.
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let running = right_pane_contents(&state, 40, 5);
    assert!(running[super::TAB_BAR_ROWS].contains("$ echo hi"));
}

#[test]
fn right_pane_shows_the_tab_strip_above_the_terminal_when_attached() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 1);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let rows = right_pane_contents(&state, 80, 5);
    // The strip takes the top two rows — the identity + chips, then the active-tab
    // underline — listing both panes; the terminal follows below them.
    let chips = console::strip_ansi_codes(&rows[0]).into_owned();
    assert!(chips.contains("agent") && chips.contains("terminal"));
    assert!(rows[2].contains("$ echo hi"));
}

#[test]
fn attached_header_shows_the_active_session_identity_beside_the_tabs() {
    // 没入 carries the same identity line as 切替: the active session's name, git
    // status, and agent state on the top row, sharing it with the tab chips.
    let mut running = worktree(Some("feat"), false, BranchStatus::Local);
    running.path = PathBuf::from("/repo/run");
    let mut state = HomeState::new("usagi", vec![running], None);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        running: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], 0);
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let rows = right_pane_contents(&state, 80, 8);
    let header = console::strip_ansi_codes(&rows[0]).into_owned();
    // Name + status + agent state, then the numbered chips, all on the top row.
    assert!(header.contains("feat") && header.contains("local") && header.contains("running"));
    assert!(header.contains("1 agent") && header.contains("2 terminal"));
    // The terminal follows below the reserved tab-strip rows.
    assert!(rows[super::TAB_BAR_ROWS].contains("$ echo hi"));

    // A completed agent keeps the same header placement but reports `done` (done
    // wins over a stale running flag from the monitor snapshot).
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/run")].into(),
        running: [PathBuf::from("/repo/run")].into(),
        done: [PathBuf::from("/repo/run")].into(),
        ..Default::default()
    });
    let done_header =
        console::strip_ansi_codes(&right_pane_contents(&state, 80, 8)[0]).into_owned();
    assert!(done_header.contains("feat") && done_header.contains("local"));
    assert!(done_header.contains("done"));
    assert!(!done_header.contains("running"));
}

#[test]
fn attached_header_shows_the_root_note_when_the_root_is_active() {
    // With the workspace root active, the 没入 header mirrors the 切替 root row:
    // the root name and its `workspace root` note (no git status / agent).
    let mut state = HomeState::new("usagi", Vec::new(), None);
    state.set_root_path(PathBuf::from("/repo"));
    state.enter_focus(0);
    state.show_attached();
    let header = console::strip_ansi_codes(&right_pane_contents(&state, 60, 6)[0]).into_owned();
    assert!(header.contains(ROOT_NAME));
    assert!(header.contains("workspace root"));
}
