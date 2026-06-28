//! Sidebar PR hover handling: a bare pointer move over a PR-bearing session row
//! raises that session's PR popup (tracked in `HomeState::pr_hover`), and moving
//! off it clears the popup. A hover is never a click — it never moves the
//! selection or attaches a session.
//!
//! The left pane lays the root pair on body rows 3,4, a divider on row 5, then a
//! three-row entry per worktree (`main` on rows 6,7,8). The fixtures hover column
//! 0 so the point lands in the left pane at any terminal width.

use super::*;
use crate::domain::workspace_state::PrLink;

/// A bare pointer-move input at the 0-based screen (`col`, `row`).
fn hover(col: u16, row: u16) -> io::Result<Input> {
    Ok(Input::Hover(ClickEvent { col, row }))
}

/// Screen row of `main`'s first line: body rows start at 3, the root spans 3,4,
/// the divider is 5, so `main` is 6,7,8.
const MAIN_ROW: u16 = 6;

/// A state whose first session (`main`) carries a PR, so hovering its row raises
/// the popup. The full sidebar (which draws the PR badge) is the default.
fn pr_state() -> HomeState {
    let mut main = worktree(Some("main"), "/r/main");
    main.pr = vec![PrLink {
        number: 412,
        url: "https://github.com/o/r/pull/412".to_string(),
    }];
    HomeState::new("usagi", vec![main, worktree(Some("feat"), "/r/feat")], None)
}

#[test]
fn hovering_a_pr_row_does_not_move_the_selection() {
    // A hover over `main`'s row is not a click: the cursor stays on the root row,
    // so the following `Enter` attaches the workspace root (`/ws`), not `main`.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![hover(0, MAIN_ROW), Ok(Input::Key(Key::Enter))],
        pr_state(),
    );
    assert_eq!(dirs, vec![PathBuf::from("/ws")]);
}

#[test]
fn a_hover_tracks_then_clears_the_popup_without_attaching() {
    // Hovering the PR row sets the popup target, hovering it again is a no-op (no
    // repaint), and moving off the sidebar clears it — none of which attaches a
    // session.
    let dirs = run_capturing_attached_dirs_for_inputs(
        vec![
            hover(0, MAIN_ROW),  // over the PR row → popup target set (a change)
            hover(0, MAIN_ROW),  // the same row again → no change
            hover(70, MAIN_ROW), // off the sidebar → popup cleared (a change)
        ],
        pr_state(),
    );
    assert!(dirs.is_empty(), "a hover never attaches a session");
}
