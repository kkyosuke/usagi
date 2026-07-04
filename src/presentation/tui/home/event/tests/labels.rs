//! 切替 (Switch) manual-status label keys: `Tab` / `Shift-Tab` cycle, `1`–`9`
//! select, `0` clears — driven through the real event loop with a capturing
//! `set_label` wiring so both the branch and the persisted `(name, id)` are
//! covered.

use super::*;
use crate::domain::settings::{LabelColor, SessionLabelDef, SessionLabelMaster};
use std::cell::RefCell;

fn def(id: &str, name: &str) -> SessionLabelDef {
    SessionLabelDef {
        id: id.to_string(),
        name: name.to_string(),
        color: LabelColor::Gray,
        icon: None,
    }
}

fn master() -> SessionLabelMaster {
    SessionLabelMaster {
        labels: vec![
            def("todo", "Todo"),
            def("doing", "Doing"),
            def("done", "Done"),
        ],
    }
}

fn session(name: &str, label: Option<&str>) -> SessionRecord {
    SessionRecord {
        name: name.to_string(),
        display_name: None,
        note: None,
        label_id: label.map(str::to_string),
        root: PathBuf::from(format!("/r/{name}")),
        worktrees: vec![worktree(Some(name), &format!("/r/{name}"))],
        created_at: Utc::now(),
        last_active: None,
    }
}

/// Drive the loop over `keys` against a single session `alpha` carrying `current`
/// as its label, with `label_master` installed, and return every `(name, id)` the
/// `set_label` wiring was asked to persist. The capturing wiring mutates its own
/// session copy and hands it back, so successive keys advance as they would in
/// production (the reload refreshes the in-memory label).
fn drive(
    current: Option<&str>,
    label_master: SessionLabelMaster,
    keys: Vec<io::Result<Key>>,
) -> Vec<(String, Option<String>)> {
    let mut state = sample_state();
    state.set_label_master(label_master);
    state.restore_sessions(vec![session("alpha", current)]);

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    let sessions = RefCell::new(vec![session("alpha", current)]);
    let calls = RefCell::new(Vec::new());

    let mut set_label = |_root: &Path, name: &str, id: Option<&str>| {
        calls
            .borrow_mut()
            .push((name.to_string(), id.map(str::to_string)));
        let mut ss = sessions.borrow_mut();
        if let Some(s) = ss.iter_mut().find(|s| s.name == name) {
            s.label_id = id.map(str::to_string);
        }
        SessionOutcome {
            line: LogLine::output("label"),
            sessions: Some(ss.clone()),
            select: Some(name.to_string()),
            root_note: None,
        }
    };
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
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut dispatch_update = || {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut chat_ask = ready_chat_ask;
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
        set_label: &mut set_label,
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
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
    };
    let outcome = event_loop(
        &term,
        &mut reader,
        state,
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    calls.into_inner()
}

#[test]
fn tab_assigns_the_first_label_then_advances() {
    // Two Tabs from an unset session step through the master in order.
    let calls = drive(
        None,
        master(),
        vec![
            Ok(Key::ArrowDown), // root -> alpha
            Ok(Key::Tab),
            Ok(Key::Tab),
            Ok(Key::CtrlC),
        ],
    );
    assert_eq!(
        calls,
        vec![
            ("alpha".to_string(), Some("todo".to_string())),
            ("alpha".to_string(), Some("doing".to_string())),
        ]
    );
}

#[test]
fn shift_tab_cycles_backward_to_the_last_label() {
    let calls = drive(
        None,
        master(),
        vec![Ok(Key::ArrowDown), Ok(Key::BackTab), Ok(Key::CtrlC)],
    );
    assert_eq!(calls, vec![("alpha".to_string(), Some("done".to_string()))]);
}

#[test]
fn digit_selects_the_nth_label_and_zero_clears() {
    // `2` picks the second label; `0` then clears it.
    let calls = drive(
        Some("todo"),
        master(),
        vec![
            Ok(Key::ArrowDown),
            Ok(Key::Char('2')),
            Ok(Key::Char('0')),
            Ok(Key::CtrlC),
        ],
    );
    assert_eq!(
        calls,
        vec![
            ("alpha".to_string(), Some("doing".to_string())),
            ("alpha".to_string(), None),
        ]
    );
}

#[test]
fn out_of_range_digit_root_row_and_unset_zero_persist_nothing() {
    // `9` is past a 3-label master (no-op); a Tab on the root row is a no-op; a `0`
    // on an already-unset session writes nothing.
    let calls = drive(
        None,
        master(),
        vec![
            Ok(Key::ArrowDown), // -> alpha
            Ok(Key::Char('9')), // out of range
            Ok(Key::Char('0')), // already unset
            Ok(Key::ArrowUp),   // -> root
            Ok(Key::Tab),       // no session under the cursor
            Ok(Key::CtrlC),
        ],
    );
    assert!(calls.is_empty());
}

#[test]
fn tab_is_dormant_when_no_labels_are_defined() {
    let calls = drive(
        None,
        SessionLabelMaster { labels: vec![] },
        vec![Ok(Key::ArrowDown), Ok(Key::Tab), Ok(Key::CtrlC)],
    );
    assert!(calls.is_empty());
}
