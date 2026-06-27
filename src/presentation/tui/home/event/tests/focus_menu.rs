use super::*;

#[test]
fn focus_menu_moves_and_runs_terminal_via_enter() {
    // Switch -> focus "main" (idle, so just Focus). The menu highlights
    // "terminal" by default; move down to "agent" and back up to "terminal",
    // then Enter runs it (attaches).
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // Switch
    keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('j'))); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // agent -> terminal
    keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/main"), false)]);
}

#[test]
fn focus_menu_shortcut_keys_launch_terminal_and_agent() {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char('t'))); // terminal
    keys.push(Ok(Key::Char('k'))); // a menu move (no-op effect here)
    keys.push(Ok(Key::Char('a'))); // agent
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![false, true]);
}

#[test]
fn focus_menu_agent_picker_launches_the_chosen_cli() {
    use crate::domain::settings::AgentCli;
    // The fake pane reads the recorded choice the way the real wiring does.
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowRight)); // expand picker (default Claude highlighted)
    keys.push(Ok(Key::ArrowDown)); // Claude -> Codex
    keys.push(Ok(Key::Enter)); // launch Codex
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
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Codex))]);
}

#[test]
fn focus_menu_agent_picker_collapses_on_left_and_esc_without_launching() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowRight)); // expand
    keys.push(Ok(Key::ArrowUp)); // move within the picker (wraps)
    keys.push(Ok(Key::Char('k'))); // move within the picker (vim up)
    keys.push(Ok(Key::Home)); // an unhandled picker key: inert
    keys.push(Ok(Key::ArrowLeft)); // collapse (no launch)
    keys.push(Ok(Key::ArrowRight)); // expand again
    keys.push(Ok(Key::Escape)); // Esc collapses (no launch, stays in Focus)
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
    // The picker only ever expanded/collapsed; no pane was launched.
    assert!(opened.borrow().is_empty());
}

#[test]
fn typed_agent_name_launches_an_installed_cli_but_refuses_an_uninstalled_one() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state(); // 在席 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Codex]);
    // Focus an idle session to reach its 在席 prompt, then type `agent` there.
    // `agent gemini` (not installed, not the default) is refused — no launch — and
    // the prompt stays open, so `agent codex` (installed) can be typed next and
    // launches Codex.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt)
    keys.extend(typed("agent gemini"));
    keys.push(Ok(Key::Enter)); // refused -> stays in the 在席 prompt
    keys.extend(typed("agent codex"));
    keys.push(Ok(Key::Enter)); // launches Codex -> Closed -> 在席 prompt
    keys.push(Ok(Key::Escape)); // -> 切替
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
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Codex))]);
}

#[test]
fn typed_agent_name_allows_the_default_cli_even_when_not_probed_as_installed() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state(); // 在席 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(Vec::new()); // nothing probed as installed
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt)
    keys.extend(typed("agent claude")); // the configured default by name
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // quit
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
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Claude))]);
}

#[test]
fn focus_menu_can_run_the_coming_soon_ai_command() {
    // With the local LLM available the menu lists terminal (0, default),
    // agent (1), ai (2), close (3). ArrowUp from the top wraps to "close"; one
    // more lands on "ai"; Enter on it just logs (no attach).
    let mut state = sample_state();
    state.set_ai_available(true);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus
    keys.push(Ok(Key::Home)); // ignored in the menu
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // back to terminal
    keys.push(Ok(Key::ArrowUp)); // wrap to "close"
    keys.push(Ok(Key::ArrowUp)); // up to "ai"
    keys.push(Ok(Key::Enter)); // run ai (coming soon)
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_o_opens_switch_then_esc_re_focuses() {
    // Under the prefix scheme `Ctrl-O` is the leader in 在席 too, so `Ctrl-O o`
    // zooms out to Switch(return=Focus) — matching 没入. Esc re-enters Focus; Esc
    // -> base Switch; Esc inert, fallback Ctrl-C quits.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader (no visible change yet)
    keys.push(Ok(Key::Char('o'))); // -> Switch(return Focus)
    keys.push(Ok(Key::Escape)); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_double_leader_also_opens_switch() {
    // `Ctrl-O Ctrl-O` zooms out to Switch just like `Ctrl-O o`, the same as 没入 —
    // a control-char second key, so it works with a Japanese IME left on.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char(CTRL_O))); // double leader -> Switch
    keys.push(Ok(Key::Escape)); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_q_raises_the_quit_modal() {
    // `Ctrl-O q` raises the quit-confirmation modal from 在席, mirroring 没入's
    // `Ctrl-O q`. Confirming it with `y` quits.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('q'))); // -> quit modal
    keys.push(Ok(Key::Char('y'))); // confirm quit
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_s_and_e_drive_the_sidebar_and_note() {
    // `Ctrl-O s` toggles the sidebar and `Ctrl-O e` opens the note editor from
    // 在席, mirroring 没入. Both are driven through the loop (the editor is then
    // dismissed) before quitting.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('s'))); // toggle sidebar
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('e'))); // open note editor
    keys.push(Ok(Key::Escape)); // close note editor -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_then_unknown_key_is_swallowed() {
    // An unrecognised key right after the leader is swallowed (it clears the
    // leader and does nothing), exactly as in 没入 — so a following `Esc` still
    // leaves Focus for Switch rather than acting on the stale leader.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('z'))); // unknown -> swallowed, leader cleared
    keys.push(Ok(Key::Escape)); // Focus -> base Switch (leader is gone)
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_colon_opens_the_command_palette_then_esc_returns_to_focus() {
    // `:` in 在席 summons the command palette over the focus surface; `Esc` closes
    // it back to 在席, where `Esc` again leaves for the base 切替.
    let opened = RefCell::new(0);
    let mut config = |_: &Term| {
        *opened.borrow_mut() += 1;
        Ok(Some(reload(SessionActionUi::Menu)))
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(':'))); // -> command palette over Focus
    keys.extend(typed("config")); // type into the palette
    keys.push(Ok(Key::Enter)); // run config (palette closes) -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 1, "the palette ran the config command");
}

#[test]
fn focus_ctrl_n_and_ctrl_p_walk_the_tab_strip_via_tab_op() {
    // In 在席 with live panes, Ctrl-N / Ctrl-P walk the focused session's pane
    // tabs by making the chosen pane active through `tab_op` (`To(index)`), so its
    // preview shows and a re-attach lands on it — and they stay in Focus. The
    // session is reached live (a pane open, then `Ctrl-T` zooms out to Focus
    // keeping the panes alive), so the tab strip is published.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    // A stateful tab strip of two panes that applies each `To(index)` so the next
    // frame's read reflects the move (the real pool behaves this way).
    let active = RefCell::new(0usize);
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
            if let TabNav::To(i) = n {
                *active.borrow_mut() = i;
            }
        }
        (
            vec!["agent".to_string(), "terminal".to_string()],
            *active.borrow(),
        )
    };
    // Entering Focus on a live session attaches; `Ctrl-T` (ToFocus) zooms back out
    // to Focus with the panes still alive, which is where the tab strip shows.
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open returns ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Char(CTRL_N))); // "+ new" wraps to pane 0: To(0)
    keys.push(Ok(Key::Char(CTRL_N))); // pane 0 -> pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_P))); // pane 1 -> pane 0: To(0)
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    // A live preview so the surface drive publishes the tab strip while in Focus.
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *navs.borrow(),
        vec![TabNav::To(0), TabNav::To(1), TabNav::To(0)]
    );
}

#[test]
fn focus_ctrl_o_prefix_walks_the_tab_strip_with_letters_and_arrows() {
    // Under the prefix scheme `Ctrl-O n`/`p` (and `Ctrl-O →`/`←`) walk the tab
    // strip exactly as the direct Ctrl-N/P do — the same prefix grammar as 没入.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    let active = RefCell::new(0usize);
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
            if let TabNav::To(i) = n {
                *active.borrow_mut() = i;
            }
        }
        (
            vec!["agent".to_string(), "terminal".to_string()],
            *active.borrow(),
        )
    };
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('n'))); // "+ new" wraps to pane 0: To(0)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::ArrowRight)); // pane 0 -> pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('p'))); // pane 1 -> pane 0: To(0)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::ArrowLeft)); // pane 0 wraps to "+ new": no tab_op
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *navs.borrow(),
        vec![TabNav::To(0), TabNav::To(1), TabNav::To(0)]
    );
}

#[test]
fn focus_ctrl_o_g_launches_an_agent() {
    // `Ctrl-O g` launches an agent from 在席 — the analogue of 没入's "add an
    // agent tab" — driving the open callback (attach) for the focused session.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('g'))); // launch agent (attach) -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/feat"), true)]);
}

#[test]
fn focus_ctrl_o_prefix_jumps_to_the_previous_session() {
    // `Ctrl-O Ctrl-^` jumps to the previously focused session, like the direct
    // `Ctrl-^` (and like 没入). Focus feat, then main, then `Ctrl-O Ctrl-^` toggles
    // back to feat.
    let dirs = run_capturing_attached_dirs({
        let mut keys = cmd("session switch feat");
        keys.push(Ok(Key::Enter)); // attach feat
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char('o'))); // -> Switch
        keys.push(Ok(Key::ArrowUp)); // cursor main
        keys.push(Ok(Key::Enter)); // attach main
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char(CTRL_CARET))); // jump back to feat
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char('o'))); // -> Switch
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        keys
    });
    // The last attach is feat again — the previous-session jump landed on it.
    assert_eq!(dirs.last(), Some(&PathBuf::from("/r/feat")));
}

#[test]
fn focus_alt_scheme_keeps_ctrl_o_a_direct_zoom_out() {
    // Under the alt scheme 没入 navigates with `Alt`-chords and leaves bare
    // `Ctrl-O` to the shell, so in 在席 `Ctrl-O` keeps its single-key zoom-out to
    // Switch (no leader). Mirrors the prefix `Ctrl-O o` test, one key shorter.
    let mut state = sample_state();
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // direct -> Switch(return Focus)
    keys.push(Ok(Key::Escape)); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn focus_tab_nav_is_inert_without_live_panes() {
    // An idle focused session (no live panes, so only the "+ new" tab) has nothing
    // to walk: Ctrl-N / Ctrl-P make no `tab_op` call and stay on the action surface.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
        }
        (Vec::new(), 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus feat (idle: noop_preview is not live)
    keys.push(Ok(Key::Char(CTRL_N)));
    keys.push(Ok(Key::Char(CTRL_P)));
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(navs.borrow().is_empty());
}

#[test]
fn focus_enter_on_a_pane_tab_reattaches_while_other_keys_are_inert() {
    // In 在席 on a pane tab (reached by `Ctrl-T` from 没入, which lands on "+ new",
    // then `Ctrl-N` onto a pane tab), `Enter` re-attaches the selected pane
    // (`open_terminal` with `new_pane = false`); a non-`Enter` key there is inert
    // (the action surface only drives the "+ new" tab).
    let term = Term::stdout();
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, agent: bool, new_pane: bool| {
        let count = {
            let mut o = opens.borrow_mut();
            o.push((agent, new_pane));
            o.len()
        };
        // The first attach (from focusing the live session) zooms out to Focus with
        // the panes kept alive; the re-attach then drops straight back out.
        if count == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut tab_op = |_d: &Path, _nav: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Char(CTRL_N))); // "+ new" -> pane 0: now a pane tab is selected
    keys.push(Ok(Key::Char('j'))); // on a pane tab: inert, no open
    keys.push(Ok(Key::Enter)); // re-attach the selected pane; open #2 (false, false)
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // Two attaches: the initial focus-and-attach, then the `Enter` re-attach — the
    // `j` between them opened nothing. Both go in with `new_pane = false`.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn focus_esc_on_the_new_tab_over_panes_steps_back_onto_the_pane() {
    // In 在席 on the "+ new" tab opened over live panes (`Ctrl-T` from 没入), `Esc`
    // discards the launch surface and steps back onto the active pane's tab —
    // staying in Focus, not zooming out to 統括. A following `Enter` re-attaches
    // that pane, proving the selector landed on a pane tab (not "+ new", whose
    // `Enter` would open a fresh pane with `new_pane = true`).
    let term = Term::stdout();
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, agent: bool, new_pane: bool| {
        let count = {
            let mut o = opens.borrow_mut();
            o.push((agent, new_pane));
            o.len()
        };
        if count == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut tab_op = |_d: &Path, _nav: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Escape)); // discard "+ new" -> step onto the active pane tab
    keys.push(Ok(Key::Enter)); // re-attach the pane; open #2 (false, false)
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // The `Esc` opened nothing (it stayed in Focus); the trailing `Enter`
    // re-attached the pane it stepped onto, both with `new_pane = false`.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn focus_ctrl_c_quits() {
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn cheatsheet_opens_from_the_focus_menu_and_dismisses() {
    // `t` enters 在席 (the new-pane action menu) on the selected session; `?`
    // there opens the cheat sheet, Esc dismisses it back to the menu, and a
    // further Esc backs out to 切替 where Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char('t')), // base 切替 -> 在席 action menu
        Ok(Key::Char('?')), // open the cheat sheet over the menu
        Ok(Key::Char('j')), // scroll it
        Ok(Key::Escape),    // dismiss -> back on the 在席 menu
        Ok(Key::Escape),    // 在席 -> 切替
        Ok(Key::CtrlC),     // quit
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}
