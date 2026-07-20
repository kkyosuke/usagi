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
    // opens in 選択 and never drives a pane before the terminating Ctrl-C.
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

    // One inert key (Esc at the base Overview) routes through the trace, then
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

/// 選択 (Overview) reached from the 集中 prompt surface via `Ctrl-O`, then a
/// different session focused: the session changes as expected. Guards the
/// prompt-mode path of `closeup_key`'s `Ctrl-O` handling (the menu path is covered
/// by [`closeup_ctrl_o_opens_overview_then_esc_re_focuses`]).
#[test]
fn prompt_closeup_ctrl_o_opens_overview_and_can_change_session() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only `feat` has a live terminal, so focusing the idle root stays in 集中
    // (no auto-attach) until Ctrl-O reaches Overview and `feat` is selected.
    let mut preview = |p: &Path, _: Sidebar| {
        if p.to_string_lossy().contains("feat") {
            Some(TerminalView::from_rows(vec!["live".to_string()], None))
        } else {
            None
        }
    };
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // Closeup root (idle -> 集中 prompt, no attach)
    keys.push(Ok(Key::Char(CTRL_O))); // 集中 leader
    keys.push(Ok(Key::Char('o'))); // -> 選択
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
        "Ctrl-O from the prompt surface must reach Overview so focusing the live feat attaches"
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
    // The watcher / pane-exit sync publishes keyed by the workspace root, so a
    // refresh for the primary must target its recorded root.
    let root = state.root_path().to_path_buf();
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
        root.clone(),
        ["main", "next"]
            .iter()
            .map(|n| SessionRecord {
                todos: Vec::new(),
                decisions: Vec::new(),
                name: n.to_string(),
                display_name: None,
                note: None,
                label_id: None,
                agent: Default::default(),
                origin: Default::default(),
                started_from: None,
                root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
                worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
                worktree_provenance: Vec::new(),
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
    refresh.set(root.clone(), Vec::new());
    assert!(apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
    assert!(!apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
}

#[test]
fn a_background_git_sync_refresh_updates_freshness_state() {
    let mut state = state_with_sessions(&["main"]);
    let root = state.root_path().to_path_buf();
    let refresh = SessionsRefreshHandle::new();
    state.begin_git_sync(root.clone(), GitSyncState::syncing(1, Instant::now()));
    let started_at = Instant::now();

    refresh.complete_git_sync(GitSyncOutcome {
        root: root.clone(),
        generation: 1,
        started_at,
        finished_at: started_at,
        result: Err("git status failed".to_string()),
    });

    assert!(apply_pending_refresh(&mut state, &refresh));
    assert_eq!(
        state.git_sync_state(&root).map(|sync| sync.status),
        Some(GitSyncStatus::Stale)
    );
}

#[test]
fn an_external_refresh_updates_the_sidebar_without_entering_switch() {
    // Regression for MCP-created sessions not appearing until the user manually
    // opened Switch mode: the watcher feeds the same refresh handle while the
    // screen may be in 集中 (Closeup). Applying that refresh must rebuild the
    // left-pane list in place, keep the current mode, and keep the cursor on the
    // same session rather than relying on `enter_switch` to rebuild later.
    let mut state = state_with_sessions(&["main", "feat"]);
    assert!(state.enter_closeup_named("feat"));
    let root = state.root_path().to_path_buf();
    let refresh = SessionsRefreshHandle::new();

    refresh.set(
        root,
        ["main", "feat", "delegated"]
            .iter()
            .map(|n| SessionRecord {
                todos: Vec::new(),
                decisions: Vec::new(),
                name: n.to_string(),
                display_name: None,
                note: None,
                label_id: None,
                agent: Default::default(),
                origin: Default::default(),
                started_from: None,
                root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
                worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
                worktree_provenance: Vec::new(),
                created_at: Utc::now(),
                last_active: None,
            })
            .collect(),
    );

    assert!(apply_pending_refresh(&mut state, &refresh));
    assert_eq!(state.mode(), Mode::Closeup, "no Switch round-trip needed");
    assert_eq!(state.list().selected_name(), "feat", "cursor is preserved");
    assert!(
        state
            .list()
            .groups()
            .iter()
            .flat_map(|g| g.worktrees())
            .any(|w| w.branch.as_deref() == Some("delegated")),
        "the externally-created session is present in the left-pane list"
    );
}

#[test]
fn a_background_refresh_routes_to_the_workspace_it_names() {
    // 統合(unite) mode: the watcher publishes each workspace's recorded sessions
    // keyed by its root, so a refresh naming the extra group updates *that* group's
    // rows (a session an agent delegated to a secondary workspace appears there),
    // the primary keyed refresh updates the primary, and a refresh for a root no
    // longer displayed is dropped rather than misfiled onto the primary.
    let mut state = HomeState::new("primary", Vec::new(), None);
    state.set_root_path("/primary");
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: Vec::new(),
        issues: Vec::new(),
    }]);
    let refresh = SessionsRefreshHandle::new();

    let session = |root: &str, name: &str| SessionRecord {
        todos: Vec::new(),
        decisions: Vec::new(),
        name: name.to_string(),
        display_name: None,
        note: None,
        label_id: None,
        agent: Default::default(),
        origin: Default::default(),
        started_from: None,
        root: PathBuf::from(format!("{root}/.usagi/sessions/{name}")),
        worktrees: vec![worktree(Some(name), &format!("{root}/{name}"))],
        worktree_provenance: Vec::new(),
        created_at: Utc::now(),
        last_active: None,
    };

    // A session delegated to the secondary workspace, plus one to the primary, land
    // in the same poll — both are applied, each to its own group.
    refresh.set(PathBuf::from("/wsB"), vec![session("/wsB", "delegated")]);
    refresh.set(PathBuf::from("/primary"), vec![session("/primary", "here")]);
    // A stale refresh for a workspace dropped from unite mode is ignored.
    refresh.set(PathBuf::from("/gone"), vec![session("/gone", "orphan")]);
    assert!(apply_pending_refresh(&mut state, &refresh));

    // The primary group shows its own session; the extra group shows the delegated
    // one; the orphaned root's list appears nowhere.
    assert_eq!(
        state
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["here".to_string()]
    );
    let branches = |group: usize| -> Vec<String> {
        state.list().groups()[group]
            .worktrees()
            .iter()
            .filter_map(|w| w.branch.clone())
            .collect()
    };
    assert_eq!(
        branches(0),
        vec!["here".to_string()],
        "primary group updated"
    );
    assert_eq!(
        branches(1),
        vec!["delegated".to_string()],
        "extra group updated"
    );
    // Nowhere does the orphaned root's session appear.
    assert!(
        state
            .list()
            .groups()
            .iter()
            .flat_map(|g| g.worktrees())
            .all(|w| w.branch.as_deref() != Some("orphan")),
        "unknown-root refresh must be dropped, not misfiled"
    );
}

#[test]
fn the_background_installed_agents_probe_result_is_drained_then_the_loop_quits() {
    // The installed-agents probe reports the agents found on this machine through
    // the one-shot; the first frame drains it (filling the picker via
    // `set_installed_agents`), then Ctrl-C with nothing live quits.
    let agents = OneShot::<Vec<AgentCli>>::new();
    agents.set(vec![AgentCli::Claude]);
    assert!(matches!(
        run_with_startup_probes(vec![Ok(Key::CtrlC)], sample_state(), &agents).unwrap(),
        Outcome::Quit
    ));
    assert!(agents.take().is_none());
}
