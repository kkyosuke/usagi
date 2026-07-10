use super::*;

#[test]
fn interrupted_read_returns_quit() {
    let keys = vec![Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "interrupted",
    ))];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn unexpected_read_error_is_propagated() {
    let err = run(vec![Err(io::Error::other("boom"))], sample_state()).unwrap_err();
    assert!(err.to_string().contains("Failed to read input"));
}

// --- background-task read & drain --------------------------------------

#[test]
fn a_tick_with_no_key_re_iterates_while_a_task_runs() {
    // A running task keeps the loop animating: the read wakes on the timeout
    // with no key (Ok(None)), the loop re-iterates and repaints, then the next
    // timeout yields Ctrl-C and the idle screen quits.
    let tasks = TaskHandle::new();
    tasks.begin(super::super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn an_interrupted_timeout_read_returns_quit() {
    // While a task animates, an interrupted timeout read means quit.
    let tasks = TaskHandle::new();
    tasks.begin(super::super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn an_unexpected_timeout_read_error_is_propagated() {
    let tasks = TaskHandle::new();
    tasks.begin(super::super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Err(io::Error::other("boom"))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    let err = run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap_err();
    assert!(err.to_string().contains("Failed to read input"));
}

#[test]
fn a_live_session_wakes_the_loop_without_a_key() {
    // Regression for #66: with no install or task running but a session live, the
    // loop must still wake on the timeout tick so a background agent's badge
    // (waiting ◆ / finished ✓) and the update notice are reflected without the
    // user pressing a key. The first tick yields no key (Ok(None)) and the loop
    // re-iterates; the live session makes the next Ctrl-C raise the quit-confirm
    // modal, and Enter confirms (Ctrl-C is inert inside the modal). The blocking
    // queue holds an error: were the loop to block on `read_key` (the bug), it
    // would surface here instead of quitting cleanly through the timeout path.
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC)), Ok(Some(Key::Enter))]),
        blocking: VecDeque::from(vec![Err(io::Error::other("loop blocked on a key"))]),
    };
    assert!(matches!(
        run_with_live_session(&mut reader).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn a_pending_session_skeleton_wakes_the_loop_without_a_key() {
    // A session create / remove skeleton is rendered inline in the sidebar and
    // advances from wall-clock time. Even without a task row, live session, or
    // state watcher, the loop must wake through the animation timeout; otherwise
    // the skeleton freezes until the next keypress. The blocking queue holds an
    // error to prove the loop did not fall back to a blocking read.
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::from(vec![Err(io::Error::other("loop blocked on a key"))]),
    };
    assert!(matches!(
        run_with_pending_session(&mut reader).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn an_idle_watched_screen_wakes_on_the_watch_tick_without_a_key() {
    // A create / remove made outside this screen (an agent's MCP call, another
    // usagi window, or the CLI) only writes state.json; the background watcher
    // republishes it, so an otherwise-idle loop must wake on the watch tick to
    // apply it rather than blocking until the next keypress. The first tick yields
    // no key (Ok(None)) and the loop re-iterates (draining any pending refresh);
    // the next Ctrl-C quits (no session is live). The blocking queue holds an
    // error: were the loop to block on `read_key` (no watch tick), it would
    // surface here instead of quitting cleanly through the timeout path.
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::from(vec![Err(io::Error::other("loop blocked on a key"))]),
    };
    assert!(matches!(
        run_idle_watching(&mut reader).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn a_finished_removal_evicts_the_pooled_shell() {
    // A completed removal carrying an evict path makes the loop evict that pool
    // path on the next drain (on this thread, since the pool is not `Send`).
    let tasks = TaskHandle::new();
    let id = tasks.begin(super::super::super::tasks::TaskKind::RemoveSession, "feat");
    let path = PathBuf::from("/ws/.usagi/sessions/feat");
    tasks.complete(
        id,
        true,
        super::super::super::tasks::Completion {
            line: LogLine::output("Removed session \"feat\" 🧹"),
            sessions: None,
            target_root: None,
            evict: Some(path.clone()),
            focus: None,
            created: None,
            removed: Some("feat".to_string()),
        },
    );
    let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
    let mut remove = |_: &str, _: bool| {};
    let evicted = RefCell::new(Vec::new());
    let mut evict = |p: &Path| evicted.borrow_mut().push(p.to_path_buf());
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*evicted.borrow(), vec![path]);
}

#[test]
fn page_keys_are_inert_in_overview() {
    let keys = vec![Ok(Key::PageUp), Ok(Key::PageDown), Ok(Key::Escape)];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn default_callbacks_run_through_the_harness() {
    // Drive the shared no-op `open_terminal` (via the live harness, which
    // attaches) and `open_config` (via `config`) so both default callbacks
    // execute end to end.
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // live -> attach via noop_open -> Closed -> Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert at the base Overview; fallback Ctrl-C quits
    assert!(matches!(
        run_live(keys, sample_state()).unwrap(),
        Outcome::Quit
    ));

    // `config` through the default `noop_config` (returns false -> resume).
    let mut keys = cmd("config");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

// --- apply_task_completions (drain shared with the attached pane loop) ---

/// Queue a finished create for `name` under `root` on `tasks`, carrying `focus`
/// — the shape `run_create` hands back on success (minus the session reload,
/// which these direct tests do not need).
fn queue_finished_create(tasks: &TaskHandle, root: &Path, name: &str, focus: Option<AutoFocus>) {
    let id = tasks.begin(super::super::super::tasks::TaskKind::CreateSession, name);
    tasks.complete(
        id,
        true,
        super::super::super::tasks::Completion {
            line: LogLine::output(format!("Created session \"{name}\" 󰤇")),
            sessions: None,
            target_root: Some(root.to_path_buf()),
            evict: None,
            focus,
            created: Some(name.to_string()),
            removed: None,
        },
    );
}

#[test]
fn apply_task_completions_with_an_empty_mailbox_applies_nothing() {
    let tasks = TaskHandle::new();
    let mut state = sample_state();
    let mut evict = |_: &Path| unreachable!("nothing finished, nothing to evict");
    assert!(!apply_task_completions(
        &mut state,
        &tasks,
        &mut evict,
        Some(0)
    ));
}

#[test]
fn a_create_finishing_while_attached_still_clears_its_skeleton() {
    // Regression: while 没入 (Attached) owns the event loop, the outer loop
    // cannot drain finished tasks, so the pane loop drains them itself with
    // `focus_epoch: None`. The create's inline sidebar skeleton must clear on
    // that drain — before the fix it stayed animating until the user detached
    // back to 選択 — while the auto-focus is ignored (the user is operating
    // another session, so yanking them into 集中 would be wrong).
    let tasks = TaskHandle::new();
    let mut state = sample_state();
    let root = PathBuf::from("/r/main");
    state.begin_pending_session(root.clone(), "new-session".to_string());
    queue_finished_create(
        &tasks,
        &root,
        "new-session",
        Some(AutoFocus {
            name: "feat".to_string(),
            landing: super::super::super::tasks::FocusLanding::Closeup,
            interaction_epoch: 0,
        }),
    );
    let mut evict = |_: &Path| unreachable!("a create evicts nothing");
    assert!(apply_task_completions(&mut state, &tasks, &mut evict, None));
    assert!(state.pending_sessions().is_empty());
    assert_eq!(state.mode(), Mode::Switch, "auto-focus must be ignored");
}

#[test]
fn a_removal_finishing_while_attached_clears_its_skeleton_and_evicts() {
    // The attached drain applies a finished removal in full: the removal
    // skeleton clears and the removed session's pooled shell is evicted, with
    // the auto-focus ignored exactly as for a create.
    let tasks = TaskHandle::new();
    let mut state = sample_state();
    let root = PathBuf::from("/r/main");
    state.begin_removing_session(root.clone(), "feat".to_string());
    let id = tasks.begin(super::super::super::tasks::TaskKind::RemoveSession, "feat");
    let pool_path = PathBuf::from("/r/main/.usagi/sessions/feat");
    tasks.complete(
        id,
        true,
        super::super::super::tasks::Completion {
            line: LogLine::output("Removed session \"feat\" 🧹"),
            sessions: None,
            target_root: Some(root.clone()),
            evict: Some(pool_path.clone()),
            focus: Some(AutoFocus {
                name: "main".to_string(),
                landing: super::super::super::tasks::FocusLanding::Switch,
                interaction_epoch: 0,
            }),
            created: None,
            removed: Some("feat".to_string()),
        },
    );
    let evicted = RefCell::new(Vec::new());
    let mut evict = |p: &Path| evicted.borrow_mut().push(p.to_path_buf());
    assert!(apply_task_completions(&mut state, &tasks, &mut evict, None));
    assert!(state.pending_sessions().is_empty());
    assert_eq!(*evicted.borrow(), vec![pool_path]);
}

#[test]
fn apply_task_completions_honors_focus_on_a_matching_epoch() {
    // The outer loop's path: with the dispatch-time epoch still current, a
    // finished create drops into 集中 (Closeup) on the landing session.
    let tasks = TaskHandle::new();
    let mut state = sample_state();
    let root = PathBuf::from("/r/main");
    state.begin_pending_session(root.clone(), "feat".to_string());
    queue_finished_create(
        &tasks,
        &root,
        "feat",
        Some(AutoFocus {
            name: "feat".to_string(),
            landing: super::super::super::tasks::FocusLanding::Closeup,
            interaction_epoch: 5,
        }),
    );
    let mut evict = |_: &Path| unreachable!("a create evicts nothing");
    assert!(apply_task_completions(
        &mut state,
        &tasks,
        &mut evict,
        Some(5)
    ));
    assert!(state.pending_sessions().is_empty());
    assert_eq!(state.mode(), Mode::Closeup);
    assert_eq!(state.focused_session_name(), "feat");
}

#[test]
fn apply_task_completions_skips_focus_on_a_stale_epoch() {
    // The user typed since the dispatch (the epoch moved on): the skeleton
    // still clears but the cursor stays put.
    let tasks = TaskHandle::new();
    let mut state = sample_state();
    let root = PathBuf::from("/r/main");
    state.begin_pending_session(root.clone(), "feat".to_string());
    queue_finished_create(
        &tasks,
        &root,
        "feat",
        Some(AutoFocus {
            name: "feat".to_string(),
            landing: super::super::super::tasks::FocusLanding::Closeup,
            interaction_epoch: 5,
        }),
    );
    let mut evict = |_: &Path| unreachable!("a create evicts nothing");
    assert!(apply_task_completions(
        &mut state,
        &tasks,
        &mut evict,
        Some(6)
    ));
    assert!(state.pending_sessions().is_empty());
    assert_eq!(state.mode(), Mode::Switch);
}
