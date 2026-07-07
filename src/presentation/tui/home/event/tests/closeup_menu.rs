use super::*;

#[test]
fn closeup_menu_moves_and_runs_terminal_via_enter() {
    // Overview -> focus "main" (idle, so just Closeup). The menu lists its commands
    // in alphabetical order (agent, close, diff, terminal) and highlights "agent"
    // by default; move down to "terminal" (the last row), then Enter runs it
    // (attaches).
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // Overview
    keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('k'))); // agent wraps up to "terminal" (the last row)
    keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
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
fn closeup_menu_filter_narrows_the_list_then_enter_runs_the_sole_match() {
    // `/` enters filter mode, so the letters that follow narrow the list instead of
    // firing the bare-letter shortcuts; once one command remains, Enter runs it.
    // Nothing launches until Enter, proving `t` was filter input, not the shortcut.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // Overview
    keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('/'))); // enter filter mode
    keys.push(Ok(Key::Char('t'))); // filter -> only "terminal" survives
    keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
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
fn closeup_menu_filter_esc_peels_before_leaving_and_restores_the_shortcuts() {
    // A live `/` filter absorbs Esc (restoring the full list) rather than leaving
    // 集中; Backspace edits the query, Enter on a no-match is inert, and once the
    // filter is cleared the bare-letter shortcuts (`t`) work again.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // Overview
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('/'))); // enter filter mode
    keys.push(Ok(Key::ArrowLeft)); // inert while filtering (no picker to collapse)
    keys.push(Ok(Key::Char('z'))); // no command starts with "z"
    keys.push(Ok(Key::Char('z')));
    keys.push(Ok(Key::Enter)); // no match -> inert (nothing opened)
    keys.push(Ok(Key::Backspace)); // "zz" -> "z" (still no match)
    keys.push(Ok(Key::Escape)); // clears the filter, staying in Closeup
    keys.push(Ok(Key::Char('t'))); // not filtering now -> `t` runs terminal
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
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
    // Only the post-clear `t` shortcut launched a pane.
    assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/main"), false)]);
}

#[test]
fn closeup_menu_shortcut_keys_launch_terminal_and_agent() {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char('t'))); // terminal
    keys.push(Ok(Key::Char('k'))); // a menu move (no-op effect here)
    keys.push(Ok(Key::Char('a'))); // agent
    keys.push(Ok(Key::Escape)); // -> Overview
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
    assert_eq!(*opened.borrow(), vec![false, false]);
}

#[test]
fn closeup_menu_agent_picker_launches_the_chosen_cli() {
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
    keys.push(Ok(Key::Enter)); // Closeup feat ("agent" highlighted by default)
    keys.push(Ok(Key::ArrowRight)); // expand picker (default Claude highlighted)
    keys.push(Ok(Key::ArrowDown)); // Claude -> Codex
    keys.push(Ok(Key::Enter)); // launch Codex
    keys.push(Ok(Key::Escape)); // -> Overview
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
    assert_eq!(*opened.borrow(), vec![(false, Some(AgentCli::Codex))]);
}

#[test]
fn closeup_menu_agent_picker_collapses_on_left_and_esc_without_launching() {
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
    keys.push(Ok(Key::Enter)); // Closeup feat ("agent" highlighted by default)
    keys.push(Ok(Key::ArrowRight)); // expand
    keys.push(Ok(Key::ArrowUp)); // move within the picker (wraps)
    keys.push(Ok(Key::Char('k'))); // move within the picker (vim up)
    keys.push(Ok(Key::Home)); // an unhandled picker key: inert
    keys.push(Ok(Key::ArrowLeft)); // collapse (no launch)
    keys.push(Ok(Key::ArrowRight)); // expand again
    keys.push(Ok(Key::Escape)); // Esc collapses (no launch, stays in Closeup)
    keys.push(Ok(Key::Escape)); // -> Overview
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
    let mut state = prompt_state(); // 集中 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Codex]);
    // Closeup an idle session to reach its 集中 prompt, then type `agent` there.
    // `agent gemini` (not installed, not the default) is refused — no launch — and
    // the prompt stays open, so `agent codex` (installed) can be typed next and
    // launches Codex.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (prompt)
    keys.extend(typed("agent gemini"));
    keys.push(Ok(Key::Enter)); // refused -> stays in the 集中 prompt
    keys.extend(typed("agent codex"));
    keys.push(Ok(Key::Enter)); // launches Codex -> Closed -> 集中 prompt
    keys.push(Ok(Key::Escape)); // -> 選択
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
    assert_eq!(*opened.borrow(), vec![(false, Some(AgentCli::Codex))]);
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
    let mut state = prompt_state(); // 集中 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(Vec::new()); // nothing probed as installed
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (prompt)
    keys.extend(typed("agent claude")); // the configured default by name
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape)); // -> Overview
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
    assert_eq!(*opened.borrow(), vec![(false, Some(AgentCli::Claude))]);
}

#[test]
fn closeup_menu_agent_default_refuses_when_not_installed() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Codex]); // default Claude is not installed
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat ("agent" highlighted by default)
    keys.push(Ok(Key::Enter)); // attempt to launch default agent (Claude) -> refused
    keys.push(Ok(Key::Escape)); // -> Overview
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
    // Refused because Claude is not installed: no launch occurred.
    assert!(opened.borrow().is_empty());
}

/// A zero-argument Session-scope command with no `run_closeup_command` arm, for
/// proving the menu's dispatch fails loudly instead of falling back to an agent
/// launch.
struct MenuOnlyTestCommand;

impl crate::presentation::tui::home::command::Command for MenuOnlyTestCommand {
    fn name(&self) -> &'static str {
        "zzz"
    }

    fn description(&self) -> &'static str {
        "test-only zero-argument command"
    }

    fn scope(&self) -> crate::presentation::tui::home::command::CommandScope {
        crate::presentation::tui::home::command::CommandScope::Session
    }

    fn run(
        &self,
        _args: &str,
        _ctx: &crate::presentation::tui::home::command::CommandContext,
    ) -> crate::presentation::tui::home::command::CommandResult {
        crate::presentation::tui::home::command::CommandResult {
            lines: Vec::new(),
            effect: crate::presentation::tui::home::command::Effect::None,
        }
    }
}

#[test]
fn closeup_menu_refuses_a_session_command_without_a_menu_arm() {
    // A Session-scope command registered without a `run_closeup_command` arm appears
    // in the 集中 menu (sorted alphabetically, so `zzz` lands last), but Enter on
    // it must not silently launch the default agent — the catch-all just logs and
    // stays put. With the local LLM unavailable the menu lists agent (0), close
    // (1), diff (2), terminal (3), zzz (4); ArrowUp from the default "agent" wraps
    // straight onto "zzz".
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.register_command(Box::new(MenuOnlyTestCommand));
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat ("agent" highlighted by default)
    keys.push(Ok(Key::Home)); // ignored in the menu (the key fallthrough arm)
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to "zzz"
    keys.push(Ok(Key::Enter)); // run zzz -> catch-all logs, no launch
    keys.push(Ok(Key::Escape)); // -> Overview
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
    assert!(opened.borrow().is_empty());
}

#[test]
fn closeup_ctrl_o_o_opens_overview_then_esc_re_focuses() {
    // Under the prefix scheme `Ctrl-O` is the leader in 集中 too, so `Ctrl-O o`
    // zooms out to Overview(return=Closeup) — matching 没入. Esc re-enters Closeup; Esc
    // -> base Overview; Esc inert, fallback Ctrl-C quits.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader (no visible change yet)
    keys.push(Ok(Key::Char('o'))); // -> Overview(return Closeup)
    keys.push(Ok(Key::Escape)); // back -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_ctrl_o_double_leader_also_opens_overview() {
    // `Ctrl-O Ctrl-O` zooms out to Overview just like `Ctrl-O o`, the same as 没入 —
    // a control-char second key, so it works with a Japanese IME left on.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char(CTRL_O))); // double leader -> Overview
    keys.push(Ok(Key::Escape)); // back -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_ctrl_o_q_raises_the_quit_modal() {
    // `Ctrl-O q` raises the quit-confirmation modal from 集中, mirroring 没入's
    // `Ctrl-O q`. Confirming it with `y` quits.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('q'))); // -> quit modal
    keys.push(Ok(Key::Char('y'))); // confirm quit
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_ctrl_o_s_and_e_drive_the_sidebar_and_note() {
    // `Ctrl-O s` toggles the sidebar and `Ctrl-O e` opens the note editor from
    // 集中, mirroring 没入. Both are driven through the loop (the editor is then
    // dismissed) before quitting.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('s'))); // toggle sidebar
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('e'))); // open note editor
    keys.push(Ok(Key::Escape)); // close note editor -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_ctrl_o_then_unknown_key_is_swallowed() {
    // An unrecognised key right after the leader is swallowed (it clears the
    // leader and does nothing), exactly as in 没入 — so a following `Esc` still
    // leaves Closeup for Overview rather than acting on the stale leader.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('z'))); // unknown -> swallowed, leader cleared
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview (leader is gone)
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_colon_opens_the_command_palette_then_esc_returns_to_closeup() {
    // `:` in 集中 summons the command palette over the focus surface; `Esc` closes
    // it back to 集中, where `Esc` again leaves for the base 選択.
    let opened = RefCell::new(0);
    let mut config = |_: &Term| {
        *opened.borrow_mut() += 1;
        Ok(Some(reload(SessionActionUi::Menu)))
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(':'))); // -> command palette over Closeup
    keys.extend(typed("config")); // type into the palette
    keys.push(Ok(Key::Enter)); // run config (palette closes) -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
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
fn closeup_ctrl_n_and_ctrl_p_walk_the_tab_strip_via_tab_op() {
    // In 集中 with live panes, Ctrl-N / Ctrl-P walk the focused session's pane
    // tabs by making the chosen pane active through `tab_op` (`To(index)`), so its
    // preview shows and a re-attach lands on it — and they stay in Closeup. The
    // session is reached live (a pane open, then `Ctrl-T` zooms out to Closeup
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
    // Entering Closeup on a live session attaches; `Ctrl-T` (ToCloseup) zooms back out
    // to Closeup with the panes still alive, which is where the tab strip shows.
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; ToCloseup -> Closeup on the pane's own tab (pane 0)
    keys.push(Ok(Key::Char(CTRL_N))); // pane 0 -> pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_N))); // pane 1 (last) -> "+ new": no tab_op
    keys.push(Ok(Key::Char(CTRL_P))); // "+ new" wraps back to pane 1: To(1)
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    // A live preview so the surface drive publishes the tab strip while in Closeup.
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
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
    assert_eq!(*navs.borrow(), vec![TabNav::To(1), TabNav::To(1)]);
}

#[test]
fn closeup_ctrl_o_prefix_walks_the_tab_strip_with_letters_and_arrows() {
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
    keys.push(Ok(Key::Enter)); // attach feat; ToCloseup -> Closeup on the pane's own tab (pane 0)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('n'))); // pane 0 -> pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::ArrowRight)); // pane 1 (last) -> "+ new": no tab_op
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('p'))); // "+ new" wraps back to pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::ArrowLeft)); // pane 1 -> pane 0: To(0)
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
        vec![TabNav::To(1), TabNav::To(1), TabNav::To(0)]
    );
}

#[test]
fn zoomed_out_menu_keeps_the_keyboard_over_the_pane_tab() {
    // Zooming out of a live pane (ToCloseup) keeps the pane's own tab selected with
    // the action menu floating over its preview — and the menu, not the preview,
    // owns the keyboard there: ↓ moves its cursor (cancelling the one-shot
    // re-attach) and Enter runs the highlighted command — proof the Enter drove
    // the menu rather than being inert. On a live session `terminal` now spawns a
    // *background* tab (through `spawn_pane_bg`); the pool-less compat harness
    // reports "no new tab" and falls back to a re-attach, so the second `open`
    // call arrives with `new_pane = false` like the initial one.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, n: bool| {
        opened.borrow_mut().push(n);
        if opened.borrow().len() == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut tab_op = |_: &Path, _: Option<TabNav>| (vec!["agent".to_string()], 0usize);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> ToCloseup: menu floats over the pane tab
    keys.push(Ok(Key::ArrowUp)); // menu cursor agent wraps up to terminal (cancels the re-attach arming)
    keys.push(Ok(Key::Enter)); // run terminal: a fresh pane -> Closed -> 集中 on "+ new"
    keys.push(Ok(Key::Escape)); // discard "+ new" -> the pane's preview
    keys.push(Ok(Key::Escape)); // 集中 -> 選択
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full_tabs(keys, sample_state(), &mut open, &mut preview, &mut tab_op).unwrap(),
        Outcome::Quit
    ));
    // Two attaches: the initial re-attach, then the menu's `terminal` — dispatched
    // as a background tab and, in this pool-less harness, resolved to a re-attach.
    // Both `new_pane = false`; the second proves the Enter drove the menu.
    assert_eq!(*opened.borrow(), vec![false, false]);
}

#[test]
fn closeup_ctrl_o_g_launches_an_agent() {
    // `Ctrl-O g` launches an agent from 集中 — the analogue of 没入's "add an
    // agent tab" — driving the open callback (attach) for the focused session.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // leader
    keys.push(Ok(Key::Char('g'))); // launch agent (attach) -> Closed -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
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
    assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/feat"), false)]);
}

#[test]
fn closeup_ctrl_o_prefix_jumps_to_the_previous_session() {
    // `Ctrl-O Ctrl-^` jumps to the previously focused session, like the direct
    // `Ctrl-^` (and like 没入). Closeup feat, then main, then `Ctrl-O Ctrl-^` toggles
    // back to feat.
    let dirs = run_capturing_attached_dirs({
        let mut keys = cmd("session switch feat");
        keys.push(Ok(Key::Enter)); // attach feat
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char('o'))); // -> Overview
        keys.push(Ok(Key::ArrowUp)); // cursor main
        keys.push(Ok(Key::Enter)); // attach main
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char(CTRL_CARET))); // jump back to feat
        keys.push(Ok(Key::Char(CTRL_O))); // leader
        keys.push(Ok(Key::Char('o'))); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        keys
    });
    // The last attach is feat again — the previous-session jump landed on it.
    assert_eq!(dirs.last(), Some(&PathBuf::from("/r/feat")));
}

#[test]
fn closeup_alt_scheme_keeps_ctrl_o_a_direct_zoom_out() {
    // Under the alt scheme 没入 navigates with `Alt`-chords and leaves bare
    // `Ctrl-O` to the shell, so in 集中 `Ctrl-O` keeps its single-key zoom-out to
    // Overview (no leader). Mirrors the prefix `Ctrl-O o` test, one key shorter.
    let mut state = sample_state();
    state.set_key_scheme(crate::domain::settings::KeyScheme::Alt);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.push(Ok(Key::Char(CTRL_O))); // direct -> Overview(return Closeup)
    keys.push(Ok(Key::Escape)); // back -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_tab_nav_is_inert_without_live_panes() {
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
    keys.push(Ok(Key::Enter)); // -> Closeup feat (idle: noop_preview is not live)
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
fn closeup_enter_on_a_pane_tab_reattaches_while_other_keys_are_inert() {
    // In 集中 on a pane tab (reached by `Ctrl-T` from 没入, which lands on "+ new",
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
        // The first attach (from focusing the live session) zooms out to Closeup with
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
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToCloseup -> Closeup on "+ new"
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
fn closeup_esc_on_the_new_tab_over_panes_steps_back_onto_the_pane() {
    // In 集中 on the "+ new" tab opened over live panes, `Esc` discards the launch
    // surface and steps back onto the active pane's tab — staying in Closeup, not
    // zooming out to 選択. A following `Enter` re-attaches that pane, proving the
    // selector landed on a pane tab (not "+ new", whose `Enter` would open a fresh
    // pane with `new_pane = true`).
    //
    // Reaching here via `Ctrl-T` (ToCloseup) arms a one-shot "next Esc re-attaches",
    // so a `j` (menu move) is pressed first to cancel that arming — this test
    // covers the *unarmed* discard path; the armed re-attach is covered by
    // `attached::ctrl_t_then_esc_re_attaches_to_the_zoomed_out_pane`.
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
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToCloseup -> Closeup on "+ new" (arm return)
    keys.push(Ok(Key::Char('j'))); // a menu move cancels the one-shot return arming
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
    // The `Esc` opened nothing (it stayed in Closeup); the trailing `Enter`
    // re-attached the pane it stepped onto, both with `new_pane = false`.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

/// Drive the loop with a capturing `chat_ask` hook, so the tests can control the
/// local-LLM reply (ready / withheld / disconnected) and exercise the right-pane
/// chat overlay. After the scripted keys drain, [`ScriptedReader`] falls back to
/// `Ctrl-C`, quitting.
fn run_with_chat(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    chat_ask: &mut dyn FnMut(String) -> std::sync::mpsc::Receiver<Result<String, String>>,
) -> Outcome {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    let update = UpdateHandle::new();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut dispatch_update = || {};
    let mut unite_resolve = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut start_pending_spawn: fn(&mut HomeState, &Path, bool) -> anyhow::Result<StartPending> =
        noop_start_pending_spawn;
    let mut poll_pending_spawn: fn(&Path) -> PendingPoll = noop_poll_pending_spawn;
    let mut activate_pending: fn(&Path) -> bool = noop_activate_pending;
    let mut clear_pending_spawn: fn() = noop_clear_pending_spawn;
    let mut autostart_queued = noop_autostart as fn(&HomeState) -> Vec<String>;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
    };
    // A pre-filled probe result so the loop's `ai_available` drain runs (the menu
    // gate flips on); the chat tests reach the overlay via the prompt regardless.
    let ai_available = OneShot::new();
    ai_available.set(true);
    event_loop(
        &term,
        &mut reader,
        state,
        &monitor,
        &update,
        &SessionsRefreshHandle::new(),
        &ai_available,
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
    .unwrap()
}

/// A `sample_state` focused on `feat` with the local LLM marked usable, so the
/// 集中 menu offers the `chat` row.
fn chat_ready_state() -> HomeState {
    let mut state = sample_state();
    state.set_ai_available(true);
    state
}

#[test]
fn closeup_menu_chat_row_opens_the_chat_overlay() {
    // With the local LLM available, the menu lists agent, terminal, diff, chat,
    // close; three ArrowDowns land on `chat` and Enter opens the right-pane chat
    // overlay (sidebar stays). Esc closes it back to Closeup.
    let mut ask = ready_chat_ask;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (menu)
    keys.push(Ok(Key::ArrowDown)); // agent -> terminal
    keys.push(Ok(Key::ArrowDown)); // terminal -> diff
    keys.push(Ok(Key::ArrowDown)); // diff -> chat
    keys.push(Ok(Key::Enter)); // open the chat overlay
    keys.push(Ok(Key::Escape)); // close chat -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    let outcome = run_with_chat(keys, chat_ready_state(), &mut ask);
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn closeup_prompt_chat_opens_and_converses() {
    // Typing `chat` in the 集中 prompt opens the overlay (regardless of the menu
    // gate); typing a line + Enter submits it, the echoed reply drains on the next
    // pass, and Esc closes it. Routed through the default wiring so the compat
    // `chat_ask` echo is exercised end to end.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (prompt UI)
    keys.extend(typed("chat"));
    keys.push(Ok(Key::Enter)); // run `chat` -> overlay
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Enter)); // submit -> echoed reply
    keys.push(Ok(Key::ArrowUp)); // a pass so the reply drains (and a scroll)
    keys.push(Ok(Key::Escape)); // close chat -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, prompt_state()).unwrap(), Outcome::Quit));
}

#[test]
fn chat_overlay_editing_pending_guard_and_close_mid_request() {
    // Exercise the overlay's editing keys, a blank Enter (no request), then a real
    // submit against a *withheld* reply: the spinner ticks (Empty poll), keys are
    // inert while pending (except scroll), and Esc closes it mid-request (dropping
    // the receiver). A Tab covers the catch-all key arm. The overlay is reached via
    // the prompt (`chat`), which is not gated on local-LLM availability.
    let mut senders = Vec::new();
    let mut ask = |_: String| {
        let (tx, rx) = std::sync::mpsc::channel();
        senders.push(tx); // keep the sender alive so the receiver reads Empty
        rx
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (prompt UI)
    keys.extend(typed("chat"));
    keys.push(Ok(Key::Enter)); // open the chat overlay
    keys.push(Ok(Key::Enter)); // blank line: no request
    keys.extend(typed("ab"));
    keys.push(Ok(Key::ArrowLeft));
    keys.push(Ok(Key::Backspace));
    keys.push(Ok(Key::ArrowRight));
    keys.push(Ok(Key::Del));
    keys.push(Ok(Key::Home));
    keys.push(Ok(Key::End));
    keys.push(Ok(Key::Tab)); // catch-all key arm
    keys.push(Ok(Key::Char('q')));
    keys.push(Ok(Key::Enter)); // submit -> withheld reply (stays pending)
    keys.push(Ok(Key::Char('x'))); // ignored while pending
    keys.push(Ok(Key::ArrowUp)); // scroll up works while pending
    keys.push(Ok(Key::ArrowDown)); // scroll down works while pending
    keys.push(Ok(Key::Escape)); // close mid-request -> Closeup (receiver dropped)
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    let outcome = run_with_chat(keys, prompt_state(), &mut ask);
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn chat_overlay_reports_a_failed_request() {
    // A disconnected channel (the worker dropped its sender before sending) is
    // surfaced as a failed reply in the transcript on the next poll.
    let mut ask = |_: String| {
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
        drop(tx);
        rx
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (prompt UI)
    keys.extend(typed("chat"));
    keys.push(Ok(Key::Enter)); // open the chat overlay
    keys.extend(typed("q"));
    keys.push(Ok(Key::Enter)); // submit -> disconnected reply
    keys.push(Ok(Key::ArrowUp)); // a pass so the failure drains
    keys.push(Ok(Key::Escape)); // close chat -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    let outcome = run_with_chat(keys, prompt_state(), &mut ask);
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn closeup_ctrl_c_quits() {
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn cheatsheet_opens_from_the_closeup_menu_and_dismisses() {
    // `t` enters 集中 (the new-pane action menu) on the selected session; `?`
    // there opens the cheat sheet, Esc dismisses it back to the menu, and a
    // further Esc backs out to 選択 where Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char('t')), // base 選択 -> 集中 action menu
        Ok(Key::Char('?')), // open the cheat sheet over the menu
        Ok(Key::Char('j')), // scroll it
        Ok(Key::Escape),    // dismiss -> back on the 集中 menu
        Ok(Key::Escape),    // 集中 -> 選択
        Ok(Key::CtrlC),     // quit
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn closeup_menu_terminal_picker_new_stays_in_closeup() {
    let opened = RefCell::new(Vec::new());
    let external = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, d: &Path, a: bool, n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a, n, h.mode()));
        Ok(PaneExit::Closed)
    };
    let mut open_external = |d: &Path| {
        external.borrow_mut().push(d.to_path_buf());
        Ok(())
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (agent highlighted)
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to terminal (the last row)
    keys.push(Ok(Key::ArrowRight)); // expand terminal picker, default open
    keys.push(Ok(Key::Enter)); // open = embedded pane/tab
    keys.push(Ok(Key::ArrowRight)); // expand terminal picker again
    keys.push(Ok(Key::ArrowDown)); // open -> new
    keys.push(Ok(Key::Enter)); // new = native terminal, staying in Closeup
    keys.push(Ok(Key::Char('t'))); // proves Closeup remained active: launches a pane
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full_external(keys, sample_state(), &mut open, &mut open_external).unwrap(),
        Outcome::Quit
    ));
    assert_eq!(
        *opened.borrow(),
        vec![
            (PathBuf::from("/r/feat"), false, false, Mode::Closeup),
            (PathBuf::from("/r/feat"), false, false, Mode::Closeup),
        ]
    );
    assert_eq!(*external.borrow(), vec![PathBuf::from("/r/feat")]);
}

#[test]
fn closeup_prompt_terminal_new_opens_native_terminal() {
    let opened = RefCell::new(Vec::new());
    let external = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a, n));
        Ok(PaneExit::Closed)
    };
    let mut open_external = |d: &Path| {
        external.borrow_mut().push(d.to_path_buf());
        Ok(())
    };
    let mut state = sample_state();
    state.set_session_action_ui(SessionActionUi::Prompt);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.extend(typed("terminal new"));
    keys.push(Ok(Key::Enter)); // native terminal, staying in Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full_external(keys, state, &mut open, &mut open_external).unwrap(),
        Outcome::Quit
    ));
    assert!(opened.borrow().is_empty());
    assert_eq!(*external.borrow(), vec![PathBuf::from("/r/feat")]);
}

#[test]
fn closeup_prompt_terminal_new_reports_native_terminal_errors() {
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::Closed);
    let mut open_external = |_d: &Path| Err("no terminal app".to_string());
    let mut state = sample_state();
    state.set_session_action_ui(SessionActionUi::Prompt);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat
    keys.extend(typed("terminal new"));
    keys.push(Ok(Key::Enter)); // native terminal (errors), staying in Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full_external(keys, state, &mut open, &mut open_external).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn closeup_menu_close_picker_runs_the_selected_close_action() {
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Closeup feat (agent highlighted)
    keys.push(Ok(Key::ArrowDown)); // agent -> close
    keys.push(Ok(Key::ArrowRight)); // expand close picker (safe close selected)
    keys.push(Ok(Key::ArrowDown)); // close -> close --force
    keys.push(Ok(Key::Enter)); // run close --force -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}
