//! 切替 (Switch) left-pane mouse-click handling: a single click selects the
//! session row, a second click on it confirms (focus / attach), and clicks off
//! the list — or while an overlay is open — are ignored.
//!
//! The left pane lays the root pair on body rows 3,4, a divider on row 5, then a
//! three-row entry per worktree (main on rows 6,7,8, feat on rows 9,10,11). The
//! fixtures click column 0 so the hit lands in the left pane at any terminal width.

use super::*;
use crate::presentation::tui::home::terminal::pool::MonitorSnapshot;

/// A left-button click input at the 0-based screen (`col`, `row`).
fn click(col: u16, row: u16) -> io::Result<Input> {
    Ok(Input::Click(ClickEvent { col, row }))
}

/// Screen row of the first line of the second worktree (`feat`, selectable index
/// 2): body rows start at 3, root spans 3,4, the divider is 5, `main` 6,7,8,
/// `feat` 9,10,11.
const FEAT_ROW: u16 = 9;

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
