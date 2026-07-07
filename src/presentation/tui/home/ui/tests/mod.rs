use super::chrome::*;
use super::closeup_menu::*;
use super::diff_render::*;
use super::markdown_render::*;
use super::panes::*;
use super::pr_popup::*;
use super::sidebar::*;
use super::tabs_hit::*;
use super::*;

use super::super::command::{CommandHint, CommandInfo};
use super::super::state::{
    GroupSource, LogLine, ModalSize, Preview, TextModal, WorkspaceGroup, WorktreeList, ROOT_NAME,
};
use super::super::terminal::pool::MonitorSnapshot;
use super::super::terminal::view::TerminalView;
use crate::domain::resource::ResourceUsage;
use crate::domain::settings::{SessionActionUi, Sidebar};
use crate::domain::workspace_state::{BranchStatus, PrLink, SessionRecord, WorktreeState};
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Rgb, Span, SpanStyle};
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn worktree(branch: Option<&str>, primary: bool, status: BranchStatus) -> WorktreeState {
    WorktreeState {
        branch: branch.map(|b| b.to_string()),
        path: PathBuf::from("/repo/wt"),
        head: "abc1234".to_string(),
        primary,
        upstream: None,
        status,
        diff: None,
        ahead_behind: None,
        pr: Vec::new(),
        updated_at: Utc::now(),
    }
}

fn list_with(worktrees: Vec<WorktreeState>) -> WorktreeList {
    WorktreeList::new("usagi", worktrees)
}

/// A `main` worktree carrying pull request `#<number>`, for the PR-badge and
/// sidebar-click tests.
fn worktree_with_pr(number: u32) -> WorktreeState {
    let mut wt = worktree(Some("main"), false, BranchStatus::Pushed);
    wt.pr = vec![PrLink {
        number,
        url: format!("https://github.com/o/r/pull/{number}"),
    }];
    wt
}

fn state_with(worktrees: Vec<WorktreeState>) -> HomeState {
    HomeState::new("usagi", worktrees, None)
}

fn stripped(lines: &[String]) -> String {
    console::strip_ansi_codes(&lines.join("\n")).into_owned()
}

/// Attach a session with a two-tab strip (`agent` / `terminal`) whose active tab
/// is `active`, plus the 没入 geometry for a 120×24 terminal — wide enough that the
/// right pane shows both chips in full, the fixtures the `attached_tab_at` click
/// tests share.
fn attached_with_tabs(active: usize) -> (HomeState, TerminalGeometry) {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.enter_closeup(1);
    state.show_attached();
    state.set_terminal_tabs(vec!["agent".to_string(), "terminal".to_string()], active);
    (state, attached_geometry(24, 120, Sidebar::Full))
}

/// The 0-based screen column of the chip text `needle` (e.g. `"2 terminal"`), read
/// from the rendered chips row so the click tests target where it is actually
/// drawn rather than a hand-computed indent.
fn chip_column(state: &HomeState, geo: TerminalGeometry, needle: &str) -> u16 {
    let rows = right_pane_contents(state, geo.cols as usize, 8);
    let chips = console::strip_ansi_codes(&rows[0]).into_owned();
    let byte = chips.find(needle).expect("chip present in the strip row");
    // `.find` yields a byte offset; the chip layout is in display columns. The
    // header preceding the chips carries multibyte glyphs (name, status, the agent
    // label with its AI/phase icons), so measure the prefix's display width rather
    // than trust the byte index.
    let rel = console::measure_text_width(&chips[..byte]);
    geo.origin_col + rel as u16
}

fn typing(typed: &str) -> HomeState {
    let mut state = HomeState::new("usagi", Vec::new(), None);
    // The hints belong to the `:` command palette; open it first.
    state.open_command_palette();
    for c in typed.chars() {
        state.push_char(c);
    }
    state
}

fn state_with_sessions(names: &[&str]) -> HomeState {
    use crate::domain::workspace_state::SessionRecord;
    let mut state = HomeState::new("usagi", Vec::new(), None);
    let sessions = names
        .iter()
        .map(|n| SessionRecord {
            name: n.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            root: PathBuf::from(format!("/ws/{n}")),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

/// A `MarkdownLine`-bearing preview opened from `content`, titled `title`.
fn preview_state(title: &str, content: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Local)]);
    state.open_preview_result(Ok((title.to_string(), content.to_string())));
    state
}

/// A 選択 state with one session named `alpha` carrying `note`, the cursor moved
/// onto it. `state_with` seeds an unrelated `main` worktree so the root row is
/// distinct.
fn overview_state_with_note(note: &str) -> HomeState {
    let mut state = state_with(vec![worktree(Some("main"), false, BranchStatus::Local)]);
    state.restore_sessions(vec![SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some(note.to_string()),
        label_id: None,
        agent: Default::default(),
        root: PathBuf::from("/repo/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), false, BranchStatus::Local)],
        created_at: Utc::now(),
        last_active: None,
    }]);
    state.enter_overview(super::super::state::ReturnMode::Base);
    state.overview_move_down(); // root -> alpha
    state
}

mod attached_and_terminal;
mod diff;
mod env_editor;
mod input_footer;
mod notices_and_tasks;
mod overview_create_and_hints;
mod pane_helpers;
mod removal_modal;
mod render_compose;
mod right_pane;
mod rows;
