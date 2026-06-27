use super::*;

#[test]
fn a_restored_attached_engagement_auto_attaches_on_the_first_pass() {
    // `restore_focus` armed an Attached resume on a live session; the loop attaches
    // it once on entry — before reading any key — so the user lands back in 没入.
    let attached = RefCell::new(false);
    let mut open = |_: &mut HomeState, _: &Path, _: bool, _: bool| -> Result<PaneExit> {
        *attached.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut state = sample_state();
    state.restore_focus("feat", ResumeLevel::Attached);
    // No scripted keys: the loop auto-attaches on entry, then the default Ctrl-C
    // terminator quits (no live session under the detached monitor).
    let outcome = run_full(
        vec![],
        state,
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        *attached.borrow(),
        "the restored session should be attached on the first pass"
    );
}

#[test]
fn no_restored_engagement_leaves_the_first_pass_untouched() {
    // With nothing armed (the usual launch) the entry attach is a no-op: the loop
    // opens in 切替 and never drives a pane before the terminating Ctrl-C.
    let attached = RefCell::new(false);
    let mut open = |_: &mut HomeState, _: &Path, _: bool, _: bool| -> Result<PaneExit> {
        *attached.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let outcome = run_full(
        vec![],
        sample_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        !*attached.borrow(),
        "nothing armed: no pane should be driven"
    );
}

#[test]
fn a_key_press_is_traced_when_tracing_is_enabled() {
    // With tracing on, the loop builds and records the per-key trace event via
    // the `record_with` closure — the construction that is otherwise skipped (and
    // so left uncovered) while tracing is off in every other test. The data dir is
    // pinned to a temp home and the env mutation serialised, as the trace_log
    // tests do.
    let _guard = crate::test_support::process_env_guard();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
    std::env::set_var(crate::infrastructure::trace_log::TRACE_ENV, "1");

    // One inert key (Esc at the base Switch) routes through the trace, then
    // Ctrl-C quits (no live session, so it exits at once).
    let outcome = run(vec![Ok(Key::Escape), Ok(Key::CtrlC)], sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // The press landed in today's trace file as a `tui` event.
    let traced = std::fs::read_dir(home.path().join("logs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let path = e.path();
            path.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && std::fs::read_to_string(&path)
                    .map(|c| c.contains("\"tui\""))
                    .unwrap_or(false)
        });
    assert!(traced, "the key press should be recorded to the trace log");

    std::env::remove_var(crate::infrastructure::trace_log::TRACE_ENV);
    std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
}

#[test]
fn a_populated_update_handle_is_read_before_painting() {
    // With the background check reporting a newer release, the loop reads the
    // handle each frame and renders the top-right notice. It still quits on the
    // trailing Ctrl-C, proving the update path does not disturb the loop.
    use crate::domain::version::Version;
    use crate::usecase::update_check::UpdateStatus;

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
    let monitor = MonitorHandle::detached();
    let update = UpdateHandle::new();
    update.set(UpdateStatus {
        current: Version::parse("0.0.1").unwrap(),
        latest: Version::parse("0.2.0").unwrap(),
    });
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &update,
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

/// 切替 (Switch) reached from the 在席 prompt surface via `Ctrl-O`, then a
/// different session focused: the session changes as expected. Guards the
/// prompt-mode path of `focus_key`'s `Ctrl-O` handling (the menu path is covered
/// by [`focus_ctrl_o_opens_switch_then_esc_re_focuses`]).
#[test]
fn prompt_focus_ctrl_o_opens_switch_and_can_change_session() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only `feat` has a live terminal, so focusing the idle root stays in 在席
    // (no auto-attach) until Ctrl-O reaches Switch and `feat` is selected.
    let mut preview = |p: &Path, _: Sidebar| {
        if p.to_string_lossy().contains("feat") {
            Some(TerminalView::from_rows(vec!["live".to_string()], None))
        } else {
            None
        }
    };
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root (idle -> 在席 prompt, no attach)
    keys.push(Ok(Key::Char(CTRL_O))); // 在席 -> 切替
    keys.push(Ok(Key::ArrowDown)); // root -> main
    keys.push(Ok(Key::ArrowDown)); // main -> feat
    keys.push(Ok(Key::Enter)); // focus feat (live) -> attach
    run_full(
        keys,
        prompt_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert_eq!(
        *opened.borrow(),
        1,
        "Ctrl-O from the prompt surface must reach Switch so focusing the live feat attaches"
    );
}

#[test]
fn a_background_refresh_updates_the_session_list_exactly_once() {
    // The pane-exit sync thread publishes a freshly-synced list to the handle;
    // the loop's `apply_pending_refresh` adopts it on a later frame. With nothing
    // pending the state is untouched; once a list lands it is applied and then
    // taken, so a second poll does not re-apply a stale snapshot. The return tells
    // the loop whether to force a repaint (a landed list changes the git statuses).
    let mut state = state_with_sessions(&["main", "feat"]);
    let refresh = SessionsRefreshHandle::new();

    // No sync has landed yet: the list is left exactly as it was, and the loop is
    // told nothing changed.
    assert!(!apply_pending_refresh(&mut state, &refresh));
    assert_eq!(
        state
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["main".to_string(), "feat".to_string()]
    );

    // A background sync reports that `feat` is gone and `next` was added.
    refresh.set(
        ["main", "next"]
            .iter()
            .map(|n| SessionRecord {
                name: n.to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
                worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
                created_at: Utc::now(),
                last_active: None,
            })
            .collect(),
    );
    assert!(apply_pending_refresh(&mut state, &refresh));
    assert_eq!(
        state
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["main".to_string(), "next".to_string()]
    );

    // The slot is now empty, so a further poll re-applies nothing.
    refresh.set(Vec::new());
    assert!(apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
    assert!(!apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
}

#[test]
fn the_background_llm_probe_result_is_drained_then_the_loop_quits() {
    // The local-LLM probe confirms availability through the one-shot; the first
    // frame drains it (flipping the `ai` command on via `set_ai_available`), then
    // Ctrl-C with nothing live quits.
    let ai = OneShot::<bool>::new();
    ai.set(true);
    assert!(matches!(
        run_with_startup_probes(
            vec![Ok(Key::CtrlC)],
            sample_state(),
            &ai,
            &OneShot::<Vec<AgentCli>>::new()
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert!(ai.take().is_none());
}

#[test]
fn the_background_installed_agents_probe_result_is_drained_then_the_loop_quits() {
    // The installed-agents probe reports the agents found on this machine through
    // the one-shot; the first frame drains it (filling the picker via
    // `set_installed_agents`), then Ctrl-C with nothing live quits.
    let agents = OneShot::<Vec<AgentCli>>::new();
    agents.set(vec![AgentCli::Claude]);
    assert!(matches!(
        run_with_startup_probes(
            vec![Ok(Key::CtrlC)],
            sample_state(),
            &OneShot::<bool>::new(),
            &agents
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert!(agents.take().is_none());
}
