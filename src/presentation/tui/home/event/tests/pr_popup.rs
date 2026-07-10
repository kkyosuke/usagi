//! Sidebar PR popup handling: clicking a session's `<icon> <count>` PR badge pins
//! its `#<number>` popup open, the popup stays put as the pointer moves into it,
//! clicking a `#<number>` opens that PR in the browser, and a click outside it — or
//! any keypress — dismisses it. Pinning the popup never moves the selection.
//!
//! The left pane lays the root pair on body rows 2,3, a divider on row 4, then a
//! three-row entry per worktree (`main` on rows 5,6,7), so `main`'s folded
//! `<icon> 1` badge seats flush-right on its detail line (row 6) and its pinned
//! popup floats just past the pane and the 3-column divider: the `PR` box's top
//! border on row 5, the `#412` content row on row 6, the bottom border on row 7.
//!
//! The loop reads the real terminal width (not a fixed test width), so the columns
//! are derived from it via [`geom`] rather than hard-coded — the badge's three
//! flush-right columns and the popup `#412` token both shift with the pane width.

use super::*;
use crate::domain::workspace_state::PrLink;

/// A left-button click input at the 0-based screen (`col`, `row`).
fn click(col: u16, row: u16) -> io::Result<Input> {
    Ok(Input::Click(ClickEvent { col, row }))
}

/// `main`'s detail line (its badge row) sits on screen row 6. The pinned popup's
/// top border is row 5, its `owner/repo` group header row 6, and the `#412` PR row
/// 7 (indented under the header).
const BADGE_ROW: u16 = 6;
const POPUP_ROW: u16 = 7;

/// Columns derived from the loop's real terminal width: the left pane is
/// `(width / 3)` clamped to 16..=40, the flush-right `<icon> 1` badge is its last
/// three columns, and the pinned popup floats at `left_w + 3` (past the divider) —
/// its `#412` number starting five columns in (the box's `│ ` border + pad, then
/// the row's indent + state-glyph + space). A click there lands on the PR's middle
/// (open) rather than the glyph-toggle or hide-action zones. Returns
/// `(badge_col, token_col, inside_pad_col)`.
fn geom() -> (u16, u16, u16) {
    let (_h, w) = Term::stdout().size();
    let left_w = ((w as usize) / 3).clamp(16, 40);
    let badge_col = (left_w - 2) as u16; // middle of the 3 flush-right badge columns
    let popup_left = left_w + 3; // left pane + the 3-column divider
    let token_col = (popup_left + 5) as u16; // first column of `#412`, past `│  ○ `
    let inside_pad_col = (popup_left + 6) as u16; // trailing pad inside the box, on no token
    (badge_col, token_col, inside_pad_col)
}

/// A state whose first session (`main`) carries one PR, so clicking its badge pins
/// a popup listing `#412`. The full sidebar (which draws the badge) is the default.
fn pr_state() -> HomeState {
    let mut main = worktree(Some("main"), "/r/main");
    main.pr = vec![PrLink::new(412, "https://github.com/o/r/pull/412")];
    HomeState::new("usagi", vec![main, worktree(Some("feat"), "/r/feat")], None)
}

/// Drive the real loop over `inputs`, capturing the URLs a popup click opened and
/// the session dirs an attach reached, so a test can assert both. The loop quits on
/// the drained `Ctrl-C` terminator (the fixture has no live session).
fn run_pr_clicks(inputs: Vec<io::Result<Input>>, state: HomeState) -> (Vec<String>, Vec<PathBuf>) {
    let term = Term::stdout();
    let mut reader = InputReader::new(inputs);
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    let opened = RefCell::new(Vec::new());
    let urls = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut open_url = |u: &str| urls.borrow_mut().push(u.to_string());
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_todos_fake =
        |_: &Path, _: &str, _: &[crate::domain::workspace_state::SessionTodo]| noop_set_todos();
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    // A live preview makes the root row attachable, so `Enter` on it reaches
    // `open_terminal` (the selection-didn't-move test reads the attached dir).
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
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
    let mut broadcast_wake = noop_broadcast_wake as fn(&HomeState) -> usize;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_todos: &mut set_todos_fake,
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
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
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
    (urls.into_inner(), opened.into_inner())
}

#[test]
fn clicking_the_badge_then_a_number_opens_that_pr() {
    // The badge click pins the popup; a click on `#412` inside it opens that PR. No
    // attach happens — pinning and opening are click-only affordances.
    let (badge, token, _) = geom();
    let (urls, dirs) = run_pr_clicks(
        vec![click(badge, BADGE_ROW), click(token, POPUP_ROW)],
        pr_state(),
    );
    assert_eq!(urls, vec!["https://github.com/o/r/pull/412".to_string()]);
    assert!(dirs.is_empty());
}

#[test]
fn pinning_the_popup_does_not_move_the_selection() {
    // Clicking the badge pins the popup but leaves the cursor on the root row, so
    // the following `Enter` (which also dismisses the popup) attaches the root
    // (`/ws`), not the clicked `main`.
    let (badge, _, _) = geom();
    let (urls, dirs) = run_pr_clicks(
        vec![click(badge, BADGE_ROW), Ok(Input::Key(Key::Enter))],
        pr_state(),
    );
    assert!(urls.is_empty());
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

#[test]
fn a_keypress_dismisses_the_pinned_popup() {
    // `Esc` dismisses the popup, so the later click on where `#412` sat lands in the
    // empty right pane (the popup is gone) and opens nothing.
    let (badge, token, _) = geom();
    let (urls, _) = run_pr_clicks(
        vec![
            click(badge, BADGE_ROW),
            Ok(Input::Key(Key::Escape)),
            click(token, POPUP_ROW),
        ],
        pr_state(),
    );
    assert!(urls.is_empty());
}

#[test]
fn a_click_outside_the_box_dismisses_it_then_the_badge_re_pins() {
    // A click left of the box (over the sidebar) dismisses the popup without opening
    // anything; clicking the badge again re-pins it, and the number then opens.
    let (badge, token, _) = geom();
    let (urls, _) = run_pr_clicks(
        vec![
            click(badge, BADGE_ROW), // pin
            click(2, POPUP_ROW),     // outside the box → dismiss
            click(badge, BADGE_ROW), // re-pin
            click(token, POPUP_ROW),
        ],
        pr_state(),
    );
    assert_eq!(urls, vec!["https://github.com/o/r/pull/412".to_string()]);
}

#[test]
fn a_pointer_move_is_ignored() {
    // Motion no longer drives the popup, so a bare hover does nothing: the cursor
    // stays on the root row and the following `Enter` attaches it (`/ws`).
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![
            Ok(Input::Hover(ClickEvent {
                col: 2,
                row: BADGE_ROW,
            })),
            Ok(Input::Key(Key::Enter)),
        ],
        pr_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

#[test]
fn the_compat_loop_also_opens_a_clicked_pr() {
    // The compat shim wires a no-op browser launcher; clicking the badge then the
    // number drives the same open path through it (the click attaches nothing).
    let (badge, token, _) = geom();
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(badge, BADGE_ROW), click(token, POPUP_ROW)],
        pr_state(),
    );
    assert!(dirs.is_empty());
}

#[test]
fn clicking_the_glyph_toggles_state_and_the_footer_reveals_hidden_prs() {
    use crate::domain::workspace_state::PrState;
    use crate::infrastructure::{pr_link_store, storage};

    // The state change persists through the store, so pin it to a temp data dir.
    let _guard = crate::test_support::process_env_guard();
    let data = tempfile::tempdir().unwrap();
    let old = std::env::var_os(storage::DATA_DIR_ENV);
    std::env::set_var(storage::DATA_DIR_ENV, data.path());

    // The pinned session's row path is `/r/main`; seed its store with an open and a
    // dismissed PR so the collapsed popup shows the open PR plus a `1 件非表示`
    // footer.
    let open = PrLink::new(412, "https://github.com/o/r/pull/412");
    let mut hidden = PrLink::new(99, "https://github.com/o/r/pull/99");
    hidden.state = PrState::Dismissed;
    pr_link_store::add(Path::new("/r/main"), &[open.clone(), hidden.clone()]).unwrap();
    let mut main = worktree(Some("main"), "/r/main");
    main.pr = vec![open, hidden];
    let state = HomeState::new("usagi", vec![main, worktree(Some("feat"), "/r/feat")], None);

    let (badge, _, _) = geom();
    let (_h, w) = Term::stdout().size();
    let left_w = ((w as usize) / 3).clamp(16, 40);
    let popup_left = (left_w + 3) as u16;
    let glyph_col = popup_left + 2; // the state glyph on the #412 row (content row 7)
    let footer_col = popup_left + 3; // anywhere on the `1 件非表示` footer row (row 8)

    // Rows: 5 border, 6 `o/r` header, 7 #412, 8 footer, 9 border. Pin, click the
    // glyph (toggle open→merged), then the footer (reveal hidden). Neither opens a
    // PR, and the popup stays pinned across both.
    let (urls, dirs) = run_pr_clicks(
        vec![
            click(badge, BADGE_ROW),
            click(glyph_col, 7),
            click(footer_col, 8),
        ],
        state,
    );
    assert!(urls.is_empty());
    assert!(dirs.is_empty());
    // The glyph click persisted the merge, pinned so `gh` can't override it.
    let stored = pr_link_store::get(Path::new("/r/main"));
    let merged = stored
        .iter()
        .find(|p| p.number == 412)
        .expect("#412 recorded");
    assert_eq!(merged.state, PrState::Merged);
    assert!(merged.pinned);

    match old {
        Some(old) => std::env::set_var(storage::DATA_DIR_ENV, old),
        None => std::env::remove_var(storage::DATA_DIR_ENV),
    }
}

#[test]
fn a_click_on_the_box_border_keeps_it_pinned() {
    // The row directly above the PR (`POPUP_ROW - 1`) is its `owner/repo` group
    // header — inside the box but not a PR, so a click there neither opens nor
    // dismisses (stays pinned); the PR's own row then still opens it.
    let (badge, token, _) = geom();
    let (urls, _) = run_pr_clicks(
        vec![
            click(badge, BADGE_ROW),
            click(token, POPUP_ROW - 1),
            click(token, POPUP_ROW),
        ],
        pr_state(),
    );
    assert_eq!(urls, vec!["https://github.com/o/r/pull/412".to_string()]);
}
