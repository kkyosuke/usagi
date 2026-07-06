use super::*;

#[test]
fn focus_prompt_edits_completes_and_runs_terminal() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        assert!(!a);
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt UI)
    keys.extend(typed("ter"));
    keys.push(Ok(Key::Insert)); // unhandled in the prompt: the `_` arm
    keys.push(Ok(Key::Home)); // caret to the start
    keys.push(Ok(Key::End)); // caret to the end
    keys.push(Ok(Key::ArrowLeft)); // caret before 'r'
    keys.push(Ok(Key::Del)); // forward-delete 'r' -> "te"
    keys.push(Ok(Key::Char('r'))); // "ter" again, caret at end
    keys.push(Ok(Key::ArrowLeft)); // before 'r'
    keys.push(Ok(Key::ArrowRight)); // after 'r' (end)
    keys.push(Ok(Key::Backspace)); // "te"
    keys.push(Ok(Key::Tab)); // -> "terminal"
    keys.push(Ok(Key::Enter)); // run terminal (attach)
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            prompt_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 1);
}

#[test]
fn focus_prompt_drops_control_chars() {
    // A control char that surfaces as `Key::Char` (e.g. Ctrl-S -> '\x13') must not
    // be inserted into the prompt. Were it, the typed "ter" would become "ter\x13"
    // and Tab-completion to "terminal" would no longer match, so the terminal would
    // never attach (`opened` would stay 0).
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        assert!(!a);
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt UI)
    keys.extend(typed("ter"));
    keys.push(Ok(Key::Char('\u{13}'))); // Ctrl-S: dropped, not inserted
    keys.push(Ok(Key::Tab)); // completes "ter" -> "terminal"
    keys.push(Ok(Key::Enter)); // run terminal (attach)
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            prompt_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 1);
}

#[test]
fn focus_prompt_runs_agent_and_ignores_empty() {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.push(Ok(Key::Home)); // ignored in the prompt
    keys.push(Ok(Key::Enter)); // empty prompt -> no-op
    keys.extend(typed("agent"));
    keys.push(Ok(Key::Enter)); // attach agent
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            prompt_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![false]);
}

#[test]
fn focus_prompt_ai_launches_the_configured_agent_with_a_prompt() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened
            .borrow_mut()
            .push((a, h.take_agent_choice(), h.take_agent_initial_prompt()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state();
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Claude]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.extend(typed("ai fix the failing test"));
    keys.push(Ok(Key::Enter)); // attach configured agent carrying the prompt
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(
        *opened.borrow(),
        vec![(false, None, Some("fix the failing test".to_string()))]
    );
}

#[test]
fn focus_prompt_ai_refuses_when_the_configured_agent_is_not_installed() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state();
    state.set_default_agent(AgentCli::Gemini);
    state.set_installed_agents(vec![AgentCli::Claude]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.extend(typed("ai fix it"));
    keys.push(Ok(Key::Enter)); // refused because Gemini was not probed installed
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 0);
}

#[test]
fn focus_prompt_ai_skips_the_installed_gate_when_an_agent_pane_is_live() {
    // The installed-CLI gate guards only a *fresh spawn* of the configured
    // default. When the session already shows a live `agent` tab, `ai <prompt>`
    // delivers the prompt to that pane (whatever CLI it runs) and launches
    // nothing new — so an uninstalled default must not refuse the send. On a live
    // session the launch is dispatched through `spawn_pane_bg`; the pool-less
    // compat harness reports "reused/none" and falls back to a re-attach (the
    // prompt delivery to the reused pane is exercised by the background-tab
    // tests). The observable here is that the gate did *not* refuse: a second
    // `open` call happens at all.
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, n: bool| {
        let count = {
            let mut o = opened.borrow_mut();
            o.push((a, n, h.take_agent_initial_prompt()));
            o.len()
        };
        if count == 1 {
            // The first attach (Enter on the live session in 切替) zooms out to
            // 在席, landing on the "+ new" action surface where `ai` is typed.
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    // The focused session publishes a live `agent` tab each frame, as 在席 does
    // for a session whose agent pane is running.
    let mut tabs = |_: &Path, _: Option<TabNav>| (vec!["agent".to_string()], 0);
    let mut state = prompt_state();
    state.set_default_agent(AgentCli::Gemini);
    state.set_installed_agents(vec![AgentCli::Claude]); // default not installed
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // re-attach live feat; open #1 -> ToFocus (prompt)
    keys.extend(typed("ai fix it"));
    keys.push(Ok(Key::Enter)); // agent tab is live -> gate skipped -> launch #2
    keys.push(Ok(Key::CtrlC)); // quit (nothing live in the monitor)
    assert!(matches!(
        run_full_tabs(keys, state, &mut open, &mut preview, &mut tabs).unwrap(),
        Outcome::Quit
    ));
    // Two `open` calls: the initial re-attach, then the gate-skipped `ai` launch
    // resolved to a re-attach by the pool-less harness — proof the gate did not
    // refuse the send. (In production `spawn_pane_bg` consumes the opening prompt
    // and delivers it to the reused agent pane; the pool-less noop leaves it on
    // state, so the re-attach's `open` still observes it here — the delivery path
    // itself is covered by the background-tab tests.)
    assert_eq!(
        *opened.borrow(),
        vec![
            (false, false, None),
            (false, false, Some("fix it".to_string()))
        ]
    );
}

#[test]
fn focus_prompt_agent_with_a_name_launches_that_cli() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state();
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::CodexFugu]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.extend(typed("agent sakana.ai")); // pick the codex-fugu CLI by display name
    keys.push(Ok(Key::Enter)); // attach that agent
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(false, Some(AgentCli::CodexFugu))]);
}
