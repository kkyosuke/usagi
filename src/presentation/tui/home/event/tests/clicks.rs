//! 切替 (Switch) left-pane mouse-click handling: a single click selects the
//! session row, a second click on it confirms (focus / attach), and clicks off
//! the list — or while an overlay is open — are ignored.
//!
//! The left pane lays the root pair on body rows 3,4, a divider on row 5, then a
//! three-row entry per worktree (main on rows 6,7,8, feat on rows 9,10,11). The
//! fixtures click column 0 so the hit lands in the left pane at any terminal width.

use super::*;

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
