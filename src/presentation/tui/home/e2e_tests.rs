//! End-to-end UI tests for the home screen's *main use case*: the engagement
//! ladder 切替 (Switch) → 在席 (Focus) → 没入 (Attached).
//!
//! Where [`ui::tests`](super::ui) checks individual rendered components and
//! [`event::tests`](super::event) checks how keys drive the loop, these tie the
//! two together. They walk the screen as a user actually walks it — the real
//! [`HomeState`] transitions and, in [`event_loop_attaches_a_live_session`], the
//! live event loop — and assert on the **rendered frame** ([`render_frame`]) the
//! user sees at every rung. This is the one journey the whole design is built
//! around (the docs call it the engagement ladder), so it is the one we cover
//! end to end.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use console::{Key, Term};

use crate::domain::settings::AgentCli;
use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::io::screen::KeyReader;

use super::event::{event_loop_compat, ConfigReload, Outcome};
use super::oneshot::OneShot;
use super::state::{HomeState, LogLine, PaneExit, SessionOutcome, SessionReorder};
use super::terminal::pool::{MonitorHandle, MonitorSnapshot};
use super::terminal::tabs::TabNav;
use super::terminal::view::TerminalView;
use super::ui::render_frame;
use super::update::UpdateHandle;

/// A full-size frame: the journey is meaningless at a cramped size, and the
/// component tests already cover the narrow-terminal fallbacks.
// Tall enough for root (2 lines) + the divider + two sessions (2 lines each),
// plus the blank separator row below the mode ladder.
const ROWS: usize = 26;
const COLS: usize = 80;
/// The path of the `feat` session, marked live (a running agent) throughout.
const FEAT_PATH: &str = "/repo/feat";

fn worktree(branch: &str, path: &str, primary: bool, status: BranchStatus) -> WorktreeState {
    WorktreeState {
        branch: Some(branch.to_string()),
        path: PathBuf::from(path),
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

/// The workspace a user lands on: a pushed `main` and a local `feat`.
fn workspace() -> HomeState {
    HomeState::new(
        "usagi",
        vec![
            worktree("main", "/repo/main", true, BranchStatus::Pushed),
            worktree("feat", FEAT_PATH, false, BranchStatus::Local),
        ],
        None,
    )
}

/// The visible text of a frame, ANSI stripped, so assertions read against what
/// the user sees rather than the styling around it.
fn plain(frame: &[String]) -> String {
    console::strip_ansi_codes(&frame.join("\n")).into_owned()
}

/// Walk the engagement ladder one rung at a time, asserting the rendered frame
/// at each step shows the screen the design promises for that mode.
#[test]
fn walking_the_engagement_ladder_renders_each_rung() {
    let mut state = workspace();
    // `feat` has a live agent running in it (the monitor reports this in the
    // real screen); the ladder is most interesting against a live session.
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from(FEAT_PATH)].into(),
        running: [PathBuf::from(FEAT_PATH)].into(),
        ..Default::default()
    });

    // --- 切替 (Switch): the landing screen, pick a session -----------------
    // The default screen: the workspace title, both sessions in the left pane
    // with the live agent surfaced, the picker prompt, and the Switch footer.
    let landing = plain(&render_frame(ROWS, COLS, &state));
    assert!(landing.contains("usagi"), "title bar names the workspace");
    assert!(landing.contains("main") && landing.contains("feat"));
    assert!(landing.contains("running"), "the live agent is surfaced");
    assert!(landing.contains("switch"), "footer reports Switch");
    assert!(
        landing.contains("Pick a session"),
        "the picker prompt shows"
    );

    // The `:` command palette floats the workspace command line over the panes.
    state.open_command_palette();
    let palette = plain(&render_frame(ROWS, COLS, &state));
    assert!(palette.contains("Command"), "the palette is titled");
    assert!(palette.contains('❯'), "the palette carries the prompt");
    state.close_command_palette();

    // The cursor moves down onto the live `feat` row. The event loop feeds the
    // highlighted session's live screen into the right-pane preview, so mirror
    // that here.
    state.switch_move_down(); // root -> main
    state.switch_move_down(); // main -> feat
    state.set_terminal_view(TerminalView::from_rows(
        vec!["agent: thinking…".to_string()],
        None,
    ));
    let switch = plain(&render_frame(ROWS, COLS, &state));
    assert!(switch.contains("feat"));
    assert!(
        switch.contains("agent: thinking…"),
        "the right pane previews the highlighted session's live screen"
    );

    // --- 在席 (Focus): operate the session ---------------------------------
    // Focusing the session opens its action surface in the right pane — the
    // menu of runnable commands (`terminal` / `agent`).
    state.clear_terminal_surface();
    state.enter_focus(state.list().selected_index());
    let focus = plain(&render_frame(ROWS, COLS, &state));
    assert!(
        focus.contains("session: feat"),
        "footer scopes to the session"
    );
    assert!(
        focus.contains("terminal") && focus.contains("agent"),
        "the action menu lists the session commands"
    );

    // --- 没入 (Attached): the embedded terminal ----------------------------
    // Launching a command attaches the embedded shell/agent in the right pane;
    // its live output is what the user now sees.
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(
        vec!["$ claude".to_string(), "How can I help?".to_string()],
        None,
    ));
    let attached = plain(&render_frame(ROWS, COLS, &state));
    assert!(attached.contains("attached"), "footer reports Attached");
    assert!(
        attached.contains("live terminal"),
        "the input line yields to the pane"
    );
    assert!(
        attached.contains("$ claude") && attached.contains("How can I help?"),
        "the embedded terminal's output is rendered"
    );
}

/// A key source replaying a scripted sequence, defaulting to `Ctrl-C` once
/// exhausted so the loop can never spin forever (mirrors `event::tests`).
struct ScriptedReader {
    keys: std::collections::VecDeque<Key>,
}

impl KeyReader for ScriptedReader {
    fn read_key(&mut self) -> io::Result<Key> {
        Ok(self.keys.pop_front().unwrap_or(Key::CtrlC))
    }
}

/// Drive the *real* event loop through the main journey — open the picker,
/// move to the live session, attach it, detach, and quit — and assert the loop
/// renders the attached terminal. This closes the gap the other suites leave:
/// real keystrokes in, the actual rendered frame out.
#[test]
fn event_loop_attaches_a_live_session_end_to_end() {
    let term = Term::stdout();
    let mut reader = ScriptedReader {
        keys: [
            // 切替 is the default landing mode, so no Ctrl-O is needed to reach it.
            Key::ArrowDown, // root -> main
            Key::ArrowDown, // main -> feat
            Key::Enter,     // focus feat; live -> attach the pane
            Key::Escape,    // pane closed -> Focus -> Switch
            Key::CtrlC,     // nothing live -> quit
        ]
        .into_iter()
        .collect(),
    };
    let monitor = MonitorHandle::detached();
    let update = UpdateHandle::new();

    // The attached frame the loop paints, captured the moment the pane opens —
    // that is when the screen is in 没入 with the embedded terminal showing.
    let captured = std::cell::RefCell::new(Vec::new());
    let mut open_terminal =
        |home: &mut HomeState, _dir: &Path, _agent: bool, _new_pane: bool| -> Result<PaneExit> {
            home.set_terminal_view(TerminalView::from_rows(
                vec!["$ claude".to_string(), "Working…".to_string()],
                None,
            ));
            *captured.borrow_mut() = render_frame(ROWS, COLS, home);
            // The real pane runs until the shell exits; report it closed at once.
            Ok(PaneExit::Closed)
        };
    // A non-`None` preview is what tells the loop the session is live, so
    // pressing Enter on it attaches rather than just focusing.
    let mut preview =
        |_dir: &Path, _sidebar: crate::domain::settings::Sidebar| -> Option<TerminalView> {
            Some(TerminalView::from_rows(vec!["live".to_string()], None))
        };
    let mut persist = |_: &str| {};
    let mut create = |_: &str| SessionOutcome {
        line: LogLine::output("created"),
        sessions: None,
        select: None,
        root_note: None,
    };
    let mut remove = |_: &str, _: bool| SessionOutcome {
        line: LogLine::output("removed"),
        sessions: None,
        select: None,
        root_note: None,
    };
    let mut config = |_: &Term| {
        Ok(Some(ConfigReload {
            session_action_ui: crate::domain::settings::SessionActionUi::Menu,
            key_scheme: crate::domain::settings::KeyScheme::default(),
            agent_cli: crate::domain::settings::AgentCli::default(),
        }))
    };
    let mut rename = |_: &str, _: &str| SessionOutcome {
        line: LogLine::output("renamed"),
        sessions: None,
        select: None,
        root_note: None,
    };
    let mut set_note = |_: &str, _: &str| SessionOutcome {
        line: LogLine::output("note saved"),
        sessions: None,
        select: None,
        root_note: None,
    };
    let mut tab_op =
        |_dir: &Path, _nav: Option<TabNav>| -> (Vec<String>, usize) { (Vec::new(), 0) };
    let mut close_tab = |_h: &mut HomeState, _dir: &Path| {};
    let mut reorder = |_: &str, _: bool| SessionReorder::Stationary;

    let outcome = event_loop_compat(
        &term,
        &mut reader,
        workspace(),
        Path::new("/repo"),
        &monitor,
        &update,
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut rename,
        &mut set_note,
        &mut remove,
        &mut (Vec::new as fn() -> Vec<String>),
        &mut open_terminal,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut close_tab,
        &mut reorder,
    )
    .unwrap();

    assert!(matches!(outcome, Outcome::Quit), "Ctrl-C quits the screen");
    let attached = plain(&captured.borrow());
    assert!(
        attached.contains("attached"),
        "the loop painted the Attached mode"
    );
    assert!(attached.contains("feat"), "the focused session is `feat`");
    assert!(
        attached.contains("$ claude") && attached.contains("Working…"),
        "the embedded terminal's live output is on screen"
    );
}
