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
fn focus_prompt_runs_agent_and_coming_soon_and_ignores_empty() {
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
    keys.extend(typed("ai go"));
    keys.push(Ok(Key::Enter)); // coming soon -> log, no attach
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
    assert_eq!(*opened.borrow(), vec![true]);
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
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::CodexFugu))]);
}
