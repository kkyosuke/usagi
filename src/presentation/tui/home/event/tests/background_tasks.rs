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
    // modal, and a second confirms. The blocking queue holds an error: were the
    // loop to block on `read_key` (the bug), it would surface here instead of
    // quitting cleanly through the timeout path.
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC)), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::from(vec![Err(io::Error::other("loop blocked on a key"))]),
    };
    assert!(matches!(
        run_with_live_session(&mut reader).unwrap(),
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
