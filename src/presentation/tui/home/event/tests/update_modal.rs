use super::*;

use console::Term;

use crate::domain::version::Version;
use crate::usecase::update_check::UpdateStatus;

/// A reader that replays a scripted sequence of [`Input`]s; once it empties it
/// falls back to `Ctrl-C` so the loop quits (no live session) rather than
/// spinning forever.
struct InputReader {
    inputs: VecDeque<Input>,
}

impl KeyReader for InputReader {
    fn read_key(&mut self) -> io::Result<Key> {
        match self.inputs.pop_front() {
            Some(Input::Key(key)) => Ok(key),
            _ => Ok(Key::CtrlC),
        }
    }
    fn read_input(&mut self) -> io::Result<Input> {
        Ok(self.inputs.pop_front().unwrap_or(Input::Key(Key::CtrlC)))
    }
    fn read_input_timeout(&mut self, _t: Duration) -> io::Result<Option<Input>> {
        Ok(Some(
            self.inputs.pop_front().unwrap_or(Input::Key(Key::CtrlC)),
        ))
    }
}

/// Drive the loop with an available update and the update-confirmation modal
/// already open, feeding `keys`. Returns the outcome and how many times the
/// self-update was dispatched (`dispatch_update` is captured rather than shelling
/// out). After the scripted keys drain the reader falls back to `Ctrl-C`, which
/// quits outright since no session is live.
fn run_update(keys: Vec<Key>) -> (Outcome, u32) {
    let term = Term::stdout();
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    // A handle carrying a status reports a newer release, so the screen knows an
    // update is available (what the mascot announces and the modal names).
    let update = UpdateHandle::new();
    update.set(UpdateStatus {
        current: Version::parse("0.0.1").unwrap(),
        latest: Version::parse("9.9.9").unwrap(),
    });
    let count = std::cell::Cell::new(0u32);

    let mut persist: fn(&str) = noop_persist;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool| {};
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
    let mut dispatch_update = || count.set(count.get() + 1);
    let mut unite_resolve = no_unite_resolve;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_url: &mut open_url,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };

    let mut state = sample_state();
    state.open_update_confirm();
    let mut reader = InputReader {
        inputs: keys.into_iter().map(Input::Key).collect(),
    };
    let outcome = event_loop(
        &term,
        &mut reader,
        state,
        &monitor,
        &update,
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
    .unwrap();
    (outcome, count.get())
}

#[test]
fn y_confirms_the_update_modal_dispatching_the_self_update() {
    // `y` in the modal launches the self-update once, then the trailing Ctrl-C
    // quits (the modal closed, so the key is no longer captured).
    let (outcome, dispatched) = run_update(vec![Key::Char('y')]);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(dispatched, 1);
}

#[test]
fn enter_also_confirms_the_update_modal() {
    let (outcome, dispatched) = run_update(vec![Key::Enter]);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(dispatched, 1);
}

#[test]
fn n_cancels_the_update_modal_without_dispatching() {
    // An ignored key inside the modal is a no-op (it stays open); `n` then cancels
    // without launching the update, and the trailing Ctrl-C quits.
    let (outcome, dispatched) = run_update(vec![Key::Home, Key::Char('n')]);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(dispatched, 0);
}

#[test]
fn escape_cancels_the_update_modal_without_dispatching() {
    let (outcome, dispatched) = run_update(vec![Key::Escape]);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(dispatched, 0);
}

#[test]
fn ctrl_c_and_ctrl_q_cancel_the_update_modal_without_dispatching() {
    // The update modal sits above the global Ctrl-C / Ctrl-Q quit chords, so it
    // must handle them itself or they would be inert while it is open. Each
    // cancels the modal (it must not dispatch the update); the trailing Ctrl-C
    // the reader falls back to then quits, the modal having closed.
    for chord in [Key::CtrlC, Key::Char(CTRL_Q)] {
        let (outcome, dispatched) = run_update(vec![chord]);
        assert!(matches!(outcome, Outcome::Quit));
        assert_eq!(dispatched, 0);
    }
}

#[test]
fn the_compat_loop_dispatches_through_its_update_hook() {
    // The compat shim builds its own `Wiring` whose `dispatch_update` is a no-op
    // (its tests never shell out). Open the modal and confirm with `y` so that
    // hook is exercised too, then Ctrl-C quits (the modal closed).
    let mut state = sample_state();
    state.open_update_confirm();
    let outcome = run(vec![Ok(Key::Char('y')), Ok(Key::CtrlC)], state).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}
