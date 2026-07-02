//! 切替 (Switch) left-pane mouse-click handling: a single click selects the
//! session row, a second click on it confirms (focus / attach), and clicks off
//! the list — or while an overlay is open — are ignored.
//!
//! The left pane lays the root pair on body rows 3,4, a divider on row 5, then a
//! three-row entry per worktree (main on rows 6,7,8, feat on rows 9,10,11). The
//! fixtures click column 0 so the hit lands in the left pane at any terminal width.

use super::*;
use crate::presentation::tui::home::tasks::AutoFocus;
use crate::presentation::tui::home::terminal::pool::MonitorSnapshot;
use crate::presentation::tui::home::terminal::tabs::TabSwap;

/// A left-button click input at the 0-based screen (`col`, `row`).
fn click(col: u16, row: u16) -> io::Result<Input> {
    Ok(Input::Click(ClickEvent { col, row }))
}

fn right_click(col: u16, row: u16) -> io::Result<Input> {
    Ok(Input::RightClick(ClickEvent { col, row }))
}

/// Screen row of the first line of the second worktree (`feat`, selectable index
/// 2): body rows start at 3, root spans 3,4, the divider is 5, `main` 6,7,8,
/// `feat` 9,10,11.
const FEAT_ROW: u16 = 9;

/// Screen row of the persistent "+ new session" row after `sample_state`'s two
/// sessions: root rows 3,4; divider 5; `main` 6-8; `feat` 9-11; create row 12.
const CREATE_ROW: u16 = 12;

#[test]
fn a_double_click_on_a_session_row_focuses_and_attaches_it() {
    // Two clicks on `feat`'s row attach it without any keypress — the second
    // click confirms the row the first selected.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(0, FEAT_ROW), click(0, FEAT_ROW)],
        sample_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/r/feat")]);
}

#[test]
fn a_single_click_selects_the_row_so_enter_attaches_it() {
    // One click moves the cursor onto `feat`; the following `Enter` then focuses
    // the now-selected row (the default cursor is the root row, `/ws`).
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(0, FEAT_ROW), Ok(Input::Key(Key::Enter))],
        sample_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/r/feat")]);
}

#[test]
fn a_single_click_alone_does_not_attach() {
    // A lone click selects but never confirms, so nothing is attached before the
    // loop quits on the drained terminator.
    let dirs = run_capturing_attached_dirs_for_inputs(vec![click(0, FEAT_ROW)], sample_state());
    assert!(dirs.is_empty());
}

#[test]
fn a_click_off_the_session_list_is_ignored() {
    // A click in the right pane (column 70 is past the left pane at any width)
    // selects nothing, so the cursor stays on the root row and `Enter` attaches
    // it (`/ws`), not `feat`.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(70, FEAT_ROW), Ok(Input::Key(Key::Enter))],
        sample_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

#[test]
fn a_click_on_the_create_row_opens_the_inline_create_input() {
    // A single click on the visible create affordance starts input mode. The
    // subsequent typed name and Enter create the session; no double-click is
    // needed because the row is an action target, not a session selection.
    let mut inputs = vec![click(0, CREATE_ROW)];
    inputs.extend(typed("wip").into_iter().map(|key| key.map(Input::Key)));
    inputs.push(Ok(Input::Key(Key::Enter)));
    inputs.push(Ok(Input::Key(Key::CtrlC)));
    let created = run_capturing_creates_for_inputs(inputs, sample_state());
    assert_eq!(created, vec!["wip"]);
}

#[test]
fn a_click_on_the_create_row_from_focus_opens_the_inline_create_input() {
    // The same visible row also works while the right pane is in 在席: clicking it
    // zooms back to 切替 and opens the create input.
    let mut inputs = vec![click(0, CREATE_ROW)];
    inputs.extend(typed("focus").into_iter().map(|key| key.map(Input::Key)));
    inputs.push(Ok(Input::Key(Key::Enter)));
    inputs.push(Ok(Input::Key(Key::CtrlC)));
    let created = run_capturing_creates_for_inputs(inputs, focused_state());
    assert_eq!(created, vec!["focus"]);
}

#[test]
fn a_click_while_the_command_palette_is_open_is_ignored() {
    // With the `:` palette open the click is swallowed, so closing it (`Esc`) and
    // pressing `Enter` focuses the still-selected root row, not the clicked `feat`.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![
            Ok(Input::Key(Key::Char(':'))),
            click(0, FEAT_ROW),
            Ok(Input::Key(Key::Escape)),
            Ok(Input::Key(Key::Enter)),
        ],
        sample_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

/// A `sample_state` already in 在席 (Focus) on the root row, so a click on another
/// session row exercises [`focus_click`] rather than 切替's `switch_click`.
fn focused_state() -> HomeState {
    let mut state = sample_state();
    state.enter_focus(0);
    state
}

#[test]
fn a_single_click_in_focus_switches_the_focused_session() {
    // Focused on the root row, one click on `feat` re-focuses onto it without
    // attaching; the following `t` (the menu's terminal shortcut) then attaches the
    // now-focused session — `/r/feat`, proving the click moved the focus there
    // (without it, `t` would attach the root `/ws`).
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(0, FEAT_ROW), Ok(Input::Key(Key::Char('t')))],
        focused_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/r/feat")]);
}

#[test]
fn a_double_click_in_focus_attaches_the_clicked_session() {
    // Two clicks on `feat`'s row attach it without any keypress — the second click
    // on the same row confirms it, exactly like a 切替 double click.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(0, FEAT_ROW), click(0, FEAT_ROW)],
        focused_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/r/feat")]);
}

#[test]
fn a_click_off_the_list_in_focus_is_ignored() {
    // A click in the right pane (column 70 is past the left pane at any width)
    // re-focuses nothing, so `t` attaches the still-focused root (`/ws`), not `feat`.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![click(70, FEAT_ROW), Ok(Input::Key(Key::Char('t')))],
        focused_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

#[test]
fn a_click_on_a_focus_right_pane_tab_switches_that_live_pane() {
    // With live panes published, 在席's right-pane tab strip is clickable: a click
    // on the inactive pane chip selects that pane through `tab_op(To(index))`.
    let term = Term::stdout();
    let (height, width) = term.size();
    let mut state = sample_state();
    state.enter_focus(2); // feat
    state.set_terminal_tabs(vec!["a".to_string(), "b".to_string()], 0);
    let geo = crate::presentation::tui::home::ui::terminal_geometry(
        height as usize,
        width as usize,
        state.sidebar(),
    );
    let col = (geo.origin_col..geo.origin_col + geo.cols)
        .find(|&col| {
            crate::presentation::tui::home::ui::focus_tab_at(
                &state,
                col,
                geo.origin_row,
                height as usize,
                width as usize,
            ) == Some(1)
        })
        .expect("terminal tab chip is visible at the test terminal width");

    let navs = RefCell::new(Vec::new());
    let active = RefCell::new(0usize);
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
            if let TabNav::To(i) = n {
                *active.borrow_mut() = i;
            }
        }
        (vec!["a".to_string(), "b".to_string()], *active.borrow())
    };
    let mut reader = InputReader::new(vec![click(col, geo.origin_row), Ok(Input::Key(Key::CtrlC))]);
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
        state,
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
        &mut (noop_open as fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>),
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*navs.borrow(), vec![TabNav::To(1)]);
}

#[test]
fn a_click_on_a_switch_right_pane_tab_switches_the_previewed_live_pane() {
    // 切替 also draws a live session's pane tabs in the right pane. Clicking an
    // inactive chip should drive `tab_op(To(index))`, just like ←/→ keyboard
    // navigation, without treating the click as a left-pane row selection.
    let term = Term::stdout();
    let (height, width) = term.size();
    let mut state = sample_state();
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/r/feat")].into(),
        ..Default::default()
    });
    state.switch_move_down(); // root -> main
    state.switch_move_down(); // main -> feat
    state.set_terminal_tabs(vec!["a".to_string(), "b".to_string()], 0);
    let geo = crate::presentation::tui::home::ui::terminal_geometry(
        height as usize,
        width as usize,
        state.sidebar(),
    );
    let col = (geo.origin_col..geo.origin_col + geo.cols)
        .find(|&col| {
            crate::presentation::tui::home::ui::switch_tab_at(
                &state,
                col,
                geo.origin_row,
                height as usize,
                width as usize,
            ) == Some(1)
        })
        .expect("switch preview tab chip is visible at the test terminal width");

    let navs = RefCell::new(Vec::new());
    let active = RefCell::new(0usize);
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
            if let TabNav::To(i) = n {
                *active.borrow_mut() = i;
            }
        }
        (vec!["a".to_string(), "b".to_string()], *active.borrow())
    };
    let mut reader = InputReader::new(vec![
        click(col, geo.origin_row),
        Ok(Input::Key(Key::Char(CTRL_Q))),
        Ok(Input::Key(Key::Char('y'))),
    ]);
    let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/feat")]);
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        state,
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
        &mut (noop_open as fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>),
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*navs.borrow(), vec![TabNav::To(1)]);
}

#[test]
fn a_right_click_on_a_switch_tab_opens_a_menu_and_runs_the_selected_action() {
    let term = Term::stdout();
    let (height, width) = term.size();
    let mut state = sample_state();
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/r/feat")].into(),
        ..Default::default()
    });
    state.switch_move_down();
    state.switch_move_down(); // feat
    state.set_terminal_tabs(vec!["a".to_string(), "b".to_string()], 1);
    let geo = crate::presentation::tui::home::ui::terminal_geometry(
        height as usize,
        width as usize,
        state.sidebar(),
    );
    let col = (geo.origin_col..geo.origin_col + geo.cols)
        .find(|&col| {
            crate::presentation::tui::home::ui::switch_tab_hit(
                &state,
                col,
                geo.origin_row,
                height as usize,
                width as usize,
            ) == Some(1)
        })
        .expect("switch preview tab chip is visible at the test terminal width");

    let actions = RefCell::new(Vec::new());
    let mut tab_action = |_: &mut HomeState, dir: &Path, tab: usize, action: TabMenuAction| {
        actions
            .borrow_mut()
            .push((dir.to_path_buf(), tab, action.clone()));
    };
    let mut reader = InputReader::new(vec![
        right_click(col, geo.origin_row),
        Ok(Input::Key(Key::Enter)), // default menu row: Move left
        Ok(Input::Key(Key::CtrlC)),
    ]);
    let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/feat")]);
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&Path, &str, u64) = |_, _, _| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut note = |_: &Path, n: &str, v: &str| noop_set_note(n, v);
    let mut reorder: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut remove: fn(&Path, &str, bool, Option<AutoFocus>) = |_, _, _, _| {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut dispatch_update = || {};
    let mut evict: fn(&Path) = |_| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut open_url: fn(&str) = noop_open_url;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut tab_op = |_: &Path, _: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["a".to_string(), "b".to_string()], 1)
    };
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut wiring = Wiring {
        interaction_epoch: 0,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut create,
        rename_display: &mut rename,
        set_note: &mut note,
        reorder_session: &mut reorder,
        dispatch_remove: &mut remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
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
        &TaskHandle::new(),
        &mut wiring,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *actions.borrow(),
        vec![(
            PathBuf::from("/r/feat"),
            1,
            TabMenuAction::Move(TabSwap::Left)
        )]
    );
}

fn run_switch_tab_menu_inputs(after_open: Vec<io::Result<Input>>) -> Vec<TabMenuAction> {
    let term = Term::stdout();
    let (height, width) = term.size();
    let mut state = sample_state();
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/r/feat")].into(),
        ..Default::default()
    });
    state.switch_move_down();
    state.switch_move_down(); // feat
    state.set_terminal_tabs(vec!["a".to_string(), "b".to_string()], 1);
    let geo = crate::presentation::tui::home::ui::terminal_geometry(
        height as usize,
        width as usize,
        state.sidebar(),
    );
    let col = (geo.origin_col..geo.origin_col + geo.cols)
        .find(|&col| {
            crate::presentation::tui::home::ui::switch_tab_hit(
                &state,
                col,
                geo.origin_row,
                height as usize,
                width as usize,
            ) == Some(1)
        })
        .expect("switch preview tab chip is visible at the test terminal width");
    let mut inputs = vec![right_click(col, geo.origin_row)];
    inputs.extend(after_open);
    inputs.extend([
        Ok(Input::Key(Key::Char(CTRL_Q))),
        Ok(Input::Key(Key::Char('y'))),
    ]);

    let actions = RefCell::new(Vec::new());
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, action: TabMenuAction| {
        actions.borrow_mut().push(action);
    };
    let mut reader = InputReader::new(inputs);
    let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/feat")]);
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&Path, &str, u64) = |_, _, _| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut note = |_: &Path, n: &str, v: &str| noop_set_note(n, v);
    let mut reorder: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut remove: fn(&Path, &str, bool, Option<AutoFocus>) = |_, _, _, _| {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut dispatch_update = || {};
    let mut evict: fn(&Path) = |_| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut open_url: fn(&str) = noop_open_url;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut tab_op = |_: &Path, _: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["a".to_string(), "b".to_string()], 1)
    };
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut wiring = Wiring {
        interaction_epoch: 0,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut create,
        rename_display: &mut rename,
        set_note: &mut note,
        reorder_session: &mut reorder,
        dispatch_remove: &mut remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
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
        &TaskHandle::new(),
        &mut wiring,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    actions.into_inner()
}

#[test]
fn switch_tab_menu_runs_move_right_close_and_rename_actions() {
    assert_eq!(
        run_switch_tab_menu_inputs(vec![
            Ok(Input::Key(Key::ArrowDown)),
            Ok(Input::Key(Key::Enter)),
        ]),
        vec![TabMenuAction::Move(TabSwap::Right)]
    );

    assert_eq!(
        run_switch_tab_menu_inputs(vec![
            Ok(Input::Key(Key::ArrowUp)),
            Ok(Input::Key(Key::Enter)),
        ]),
        vec![TabMenuAction::Close]
    );

    assert_eq!(
        run_switch_tab_menu_inputs(vec![
            Ok(Input::Key(Key::ArrowDown)),
            Ok(Input::Key(Key::ArrowDown)),
            Ok(Input::Key(Key::Enter)),
            Ok(Input::Key(Key::Home)),
            Ok(Input::Key(Key::ArrowRight)),
            Ok(Input::Key(Key::Backspace)),
            Ok(Input::Key(Key::Del)),
            Ok(Input::Key(Key::Char('X'))),
            Ok(Input::Key(Key::End)),
            Ok(Input::Key(Key::Char('!'))),
            Ok(Input::Key(Key::PageUp)),
            Ok(Input::Key(Key::Enter)),
        ]),
        vec![TabMenuAction::Rename("X!".to_string())]
    );
}

#[test]
fn tab_menu_escape_and_right_click_miss_dismiss_the_menu() {
    assert!(run_switch_tab_menu_inputs(vec![
        Ok(Input::Key(Key::PageUp)),
        Ok(Input::Key(Key::Escape)),
    ])
    .is_empty());
    assert!(run_switch_tab_menu_inputs(vec![
        Ok(Input::Key(Key::ArrowDown)),
        Ok(Input::Key(Key::ArrowDown)),
        Ok(Input::Key(Key::Enter)),
        Ok(Input::Key(Key::ArrowLeft)),
        Ok(Input::Key(Key::Escape)),
    ])
    .is_empty());
    assert!(run_switch_tab_menu_inputs(vec![right_click(0, 0)]).is_empty());
}

#[test]
fn right_click_tab_paths_cover_focus_and_attached_modes() {
    let term = Term::stdout();
    let (height, width) = term.size();
    let mut focus = sample_state();
    focus.enter_focus(2);
    focus.set_terminal_tabs(vec!["a".to_string(), "b".to_string()], 0);
    let geo = crate::presentation::tui::home::ui::terminal_geometry(
        height as usize,
        width as usize,
        focus.sidebar(),
    );
    let col = (geo.origin_col..geo.origin_col + geo.cols)
        .find(|&col| {
            crate::presentation::tui::home::ui::focus_tab_hit(
                &focus,
                col,
                geo.origin_row,
                height as usize,
                width as usize,
            ) == Some(1)
        })
        .expect("focus tab chip is visible");
    // In Focus, the right click opens the same tab menu, and Enter executes the
    // default Move-left action for the clicked tab.
    let actions = {
        let actions = RefCell::new(Vec::new());
        let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, action: TabMenuAction| {
            actions.borrow_mut().push(action);
        };
        let mut reader = InputReader::new(vec![
            right_click(col, geo.origin_row),
            Ok(Input::Key(Key::Enter)),
            Ok(Input::Key(Key::CtrlC)),
        ]);
        let monitor = MonitorHandle::detached();
        let mut persist: fn(&str) = noop_persist;
        let mut create: fn(&Path, &str, u64) = |_, _, _| {};
        let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
        let mut note = |_: &Path, n: &str, v: &str| noop_set_note(n, v);
        let mut reorder: fn(&str, bool) -> SessionReorder = noop_reorder;
        let mut remove: fn(&Path, &str, bool, Option<AutoFocus>) = |_, _, _, _| {};
        let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> =
            no_unite_resolve;
        let mut dispatch_update = || {};
        let mut evict: fn(&Path) = |_| {};
        let mut branches: fn() -> Vec<String> = no_branches;
        let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
        let mut open_url: fn(&str) = noop_open_url;
        let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
        let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
        let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
        let mut tab_op = |_: &Path, _: Option<TabNav>| -> (Vec<String>, usize) {
            (vec!["a".to_string(), "b".to_string()], 0)
        };
        let mut close: fn(&mut HomeState, &Path) = noop_close;
        let mut save_resume = |_: &str, _: ResumeLevel| {};
        let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
        let mut wiring = Wiring {
            interaction_epoch: 0,
            workspace_root: Path::new("/ws"),
            persist: &mut persist,
            dispatch_create: &mut create,
            rename_display: &mut rename,
            set_note: &mut note,
            reorder_session: &mut reorder,
            dispatch_remove: &mut remove,
            unite_resolve: &mut unite_resolve,
            dispatch_update: &mut dispatch_update,
            evict_pool: &mut evict,
            existing_branches: &mut branches,
            open_terminal: &mut open,
            open_url: &mut open_url,
            open_external_terminal: &mut open_external_terminal,
            open_config: &mut config,
            preview: &mut preview,
            tab_op: &mut tab_op,
            close_tab: &mut close,
            tab_action: &mut tab_action,
            save_resume: &mut save_resume,
            save_last_active: &mut save_last_active,
        };
        assert!(matches!(
            event_loop(
                &term,
                &mut reader,
                focus,
                &monitor,
                &UpdateHandle::new(),
                &SessionsRefreshHandle::new(),
                &OneShot::<bool>::new(),
                &OneShot::<Vec<AgentCli>>::new(),
                &TaskHandle::new(),
                &mut wiring,
            )
            .unwrap(),
            Outcome::Quit
        ));
        actions.into_inner()
    };
    assert_eq!(actions, vec![TabMenuAction::Move(TabSwap::Left)]);

    // Attached mode is not driven by this event loop, but the defensive branch is
    // still harmless: a right click does not open a home-level menu.
    let mut attached = sample_state();
    attached.enter_focus(2);
    attached.show_attached();
    let outcome = run_capturing_attached_dirs_for_inputs(
        vec![click(col, geo.origin_row), right_click(col, geo.origin_row)],
        attached,
    );
    assert!(outcome.is_empty());
}

#[test]
fn a_scroll_is_ignored() {
    // The TUI itself never scrolls: a wheel turn is dropped without moving the
    // cursor, so the following `Enter` attaches the still-selected root (`/ws`).
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![
            Ok(Input::Scroll(ScrollEvent {
                lines: -3,
                col: 0,
                row: FEAT_ROW,
            })),
            Ok(Input::Key(Key::Enter)),
        ],
        sample_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}
