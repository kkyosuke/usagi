//! The home (workspace) screen's event loop: read a key, dispatch it by mode,
//! repaint, repeat — until the user quits.
//!
//! This module owns the loop itself ([`event_loop`]), the modal key capture
//! (quit-confirm / removal / text modals), and the shared [`Flow`] outcome and
//! [`selected_dir`] helper. The per-mode key handlers it dispatches to — and
//! `open_pane`, which drives the embedded terminal — live in [`handlers`].

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use console::Key;
use console::Term;

use crate::domain::settings::{AgentCli, KeyScheme, SessionActionUi, Sidebar};
use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{ClickEvent, FramePainter, Input, KeyReader};

use super::oneshot::OneShot;
use super::sessions_refresh::SessionsRefreshHandle;
use super::state::{
    GroupSource, HomeState, Mode, PaneExit, ResumeLevel, SessionOutcome, SessionReorder,
};
use super::tasks::TaskHandle;
use super::terminal::pool::MonitorHandle;
use super::terminal::tabs::TabNav;
use super::terminal::view::TerminalView;
use super::ui;
use super::update::UpdateHandle;

mod handlers;

use handlers::{focus_click, focus_key, note_editor_key, palette_key, switch_click, switch_key};

/// The byte `console` reports for `Ctrl-O` on the home screen: a bare control
/// character (`0x0f`), since `console` only special-cases a handful of control
/// keys and passes the rest through as [`Key::Char`]. `Ctrl-O` zooms out one
/// engagement level (没入 → 切替) on the screen.
const CTRL_O: char = '\u{000f}';

/// The bare control characters `console` reports for `Ctrl-N` (`0x0e`) and
/// `Ctrl-P` (`0x10`) on the home screen — the same passthrough as [`CTRL_O`].
/// They move between the focused session's tabs (`Ctrl-P` previous / `Ctrl-N`
/// next) in 切替 / 在席, matching the chords 没入 uses for the same move.
const CTRL_N: char = '\u{000e}';
const CTRL_P: char = '\u{0010}';

/// The bare control character `console` reports for `Ctrl-B` (`0x02`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It toggles the left session
/// sidebar between its full width and the collapsed rail from any non-modal mode.
/// 没入 (Attached) is driven inside the embedded-terminal loop, so its `Ctrl-B` is
/// intercepted there instead (see [`super::terminal::pane`]).
const CTRL_B: char = '\u{0002}';

/// The bare control character `console` reports for `Ctrl-S` (`0x13`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It saves the session-note editor
/// (`Enter` inserts a newline there, so saving needs its own chord). 没入's
/// `Ctrl-E` (which opens the editor) is intercepted inside the embedded-terminal
/// loop instead (see [`super::terminal::pane`]).
const CTRL_S: char = '\u{0013}';

/// The bare control character `console` reports for `Ctrl-E` (`0x05`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It opens the session-note editor
/// from 在席 (Focus). 没入 (Attached) is driven inside the embedded-terminal loop,
/// so its `Ctrl-E` is intercepted there instead (see [`super::terminal::pane`]).
const CTRL_E: char = '\u{0005}';

/// The bare control character `console` reports for `Ctrl-^` (`Ctrl-Shift-6`,
/// `0x1e`) on the home screen — the same passthrough as [`CTRL_O`]. It jumps to
/// the previously focused session (vim's `Ctrl-^` / tmux's `last-window`),
/// attaching it when live, so two sessions can be toggled between without going
/// through 切替. 没入 (Attached) is driven inside the embedded-terminal loop, so
/// its `Ctrl-^` is intercepted there instead (see [`super::terminal::pane`]).
const CTRL_CARET: char = '\u{001e}';

/// The bare control character `console` reports for `Ctrl-Q` (`0x11`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It is the dedicated quit chord:
/// unlike `Ctrl-C` (which quits an idle screen outright and only confirms when a
/// session is live), `Ctrl-Q` *always* raises the quit-confirmation modal first,
/// so quitting is never a single keystroke. 没入 (Attached) is driven inside the
/// embedded-terminal loop, so its `Ctrl-Q` is intercepted there instead (see
/// [`super::terminal::pane`]) and surfaces as the same modal on the way out.
const CTRL_Q: char = '\u{0011}';

/// The callback 切替 uses to read (`None`) or navigate (`Some(nav)`) the
/// highlighted session's tabs, returning the strip's labels and active index.
/// Backed by the [`TerminalPool`](super::terminal::pool::TerminalPool) the pane
/// driver shares, so a tab moved here is the one re-attaching reveals.
pub(super) type TabOp<'a> = dyn FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize) + 'a;

/// The settings-derived values re-read when the config screen closes, so an
/// edit takes effect without reopening the home screen: the 在席 (Focus)
/// right-pane surface and whether the `ai` command is offered.
#[derive(Debug, Clone, Copy)]
pub struct ConfigReload {
    /// The effective Session Action UI (在席 mode's surface).
    pub session_action_ui: SessionActionUi,
    /// The effective 没入 key scheme (the `Ctrl-O` prefix or single `Alt`-chords),
    /// so the pane's key handling reflects the edit without reopening the screen.
    pub key_scheme: KeyScheme,
    /// Whether the local LLM is usable (enabled and its model pulled), gating
    /// the `ai` command in the 在席 menu.
    pub ai_available: bool,
}

/// A `(session name, last_active)` pair — the freshness ("heat") timestamp
/// [`Wiring::save_last_active`] flushes to `state.json` on quit.
pub(super) type SessionLastActive = (String, DateTime<Utc>);

/// The workspace root and the impure callbacks the home event loop and its key
/// handlers drive, bundled so they thread one value instead of a dozen separate
/// closures. [`super::run`] builds this against the real terminal, shell pool,
/// and session store; the tests pass fakes. Every field is a side-effecting hook
/// except `workspace_root`, the directory the screen operates in.
///
/// Each `dispatch_*` hook returns at once, having handed its git / filesystem
/// work to a background worker; the loop drains the finished ones each frame.
/// `rename_display` stays synchronous (no git work) and returns its outcome.
pub(super) struct Wiring<'a> {
    /// A monotonically increasing counter for user input handled by the home
    /// loop. Create tasks copy it at dispatch time, and only auto-focus when it
    /// is unchanged at completion time.
    pub interaction_epoch: u64,
    /// The workspace root: where the root row's pane is rooted, and the base
    /// [`selected_dir`] falls back to when the cursor is on the root row.
    pub workspace_root: &'a Path,
    /// Append a run command to the workspace history (best-effort; tests no-op).
    pub persist: &'a mut dyn FnMut(&str),
    /// Dispatch `session create <name>` to a background worker, in the workspace
    /// rooted at the given path (the cursor's group in 統合/unite mode).
    pub dispatch_create: &'a mut dyn FnMut(&Path, &str, u64),
    /// Rename a session's sidebar label in the given workspace, returning the
    /// outcome to apply inline.
    pub rename_display: &'a mut dyn FnMut(&Path, &str, &str) -> SessionOutcome,
    /// Save (or clear) a session's note in the given workspace, returning the
    /// outcome to apply inline.
    pub set_note: &'a mut dyn FnMut(&Path, &str, &str) -> SessionOutcome,
    /// Reorder the selected session one row up/down (`bool` = up), persisting the
    /// new order and returning the reloaded list to refresh. Stays synchronous
    /// (no git work) like `rename_display` / `set_note`.
    pub reorder_session: &'a mut dyn FnMut(&str, bool) -> SessionReorder,
    /// Dispatch `session remove <name>` to a background worker (`bool` = force),
    /// in the workspace rooted at the given path.
    pub dispatch_remove: &'a mut dyn FnMut(&Path, &str, bool),
    /// Resolve a registered workspace by name and load it into a [`GroupSource`]
    /// to stack into the 統合(unite) view (`unite add <name>`), or an error message
    /// to log when no such workspace is registered.
    pub unite_resolve: &'a mut dyn FnMut(&str) -> std::result::Result<GroupSource, String>,
    /// Launch the self-update on a background thread (replace the installed
    /// binary with the latest release). Called when the user confirms the
    /// update-confirmation modal raised by clicking the mascot's update notice;
    /// returns at once, the progress showing as the shared loading rabbit.
    pub dispatch_update: &'a mut dyn FnMut(),
    /// Evict a removed session's pooled shell, run on the loop thread (the pool
    /// is not `Send`).
    pub evict_pool: &'a mut dyn FnMut(&Path),
    /// The branch names already taken across the workspace, read fresh so the
    /// inline create input can validate against duplicates.
    pub existing_branches: &'a mut dyn FnMut() -> Vec<String>,
    /// Embed a live shell in the right pane (没入) and drive it: the first `bool`
    /// is `agent` vs plain `terminal`, the second `new_pane` vs re-attach.
    pub open_terminal: &'a mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    /// Open `url` in the platform's default browser — the side effect behind
    /// clicking a `#<number>` in a session's pinned PR popup. [`super::run`] wires
    /// the real launcher (the same detached spawn the immersive pane uses); tests
    /// pass a capture or a no-op so the loop's open path runs without shelling out.
    pub open_url: &'a mut dyn FnMut(&str),
    /// Open the settings screen, re-reading the affected settings on return
    /// (`None` when the user quit the app from it).
    pub open_config: &'a mut dyn FnMut(&Term) -> Result<Option<ConfigReload>>,
    /// Snapshot a session's live terminal for the 切替 preview, or `None` when it
    /// has no running shell — also the live/idle test the focus handlers use. The
    /// snapshot is sized to the given sidebar state (the preview widens when the
    /// rail is collapsed); the liveness test passes the current state and ignores
    /// the geometry.
    pub preview: &'a mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    /// Read (`None`) or navigate (`Some(nav)`) the highlighted session's tabs
    /// from 切替.
    pub tab_op: &'a mut TabOp<'a>,
    /// Close the highlighted session's active tab (pane) from 切替.
    pub close_tab: &'a mut dyn FnMut(&mut HomeState, &Path),
    /// Persist the engagement to restore on the next launch — the focused
    /// session's name and how deeply it was engaged — called when a quit is
    /// confirmed. [`super::run`] writes it to the resume-focus store (gated by the
    /// restore setting); tests pass a capture or a no-op.
    pub save_resume: &'a mut dyn FnMut(&str, ResumeLevel),
    /// Flush the freshness ("heat") timestamps accumulated this run — the
    /// `(session name, last_active)` pairs — so the sidebar dots survive a
    /// restart. Called alongside [`save_resume`](Self::save_resume) on a confirmed
    /// quit. [`super::run`] merges them into `state.json`; tests no-op.
    pub save_last_active: &'a mut dyn FnMut(&[SessionLastActive]),
}

/// What the user chose to do on the home (workspace) screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the project selection screen without acting on a worktree.
    /// Retained for the screens that open the home screen ([`super::super::open`]
    /// / [`super::super::app`]); the home loop itself no longer emits it, since
    /// `Esc` is inert here and the only way out is quitting via `Ctrl-C`.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the home screen against the given terminal and key source until the
/// user quits. Assumes the alternate screen is already active (it is owned by
/// the orchestrator, several levels up).
///
/// `Ctrl-C` is the only way out: it closes the app, but when a session is still
/// live (an agent/shell is running) it first raises a quit-confirmation modal so
/// an accidental press does not drop running work — confirming it quits.
///
/// The screen is a three-step engagement ladder, with the workspace command
/// line summoned on top as a `:` palette:
///
/// - **切替 (Switch)** — the default: pick a session in the left pane. `↑`/`↓`
///   (or `k`/`j`) move between sessions, `←`/`→` (or `h`/`l`, or `Ctrl-P`/
///   `Ctrl-N`) move between the highlighted session's tabs, `Enter` focuses
///   (attaching when the session is live), `t` opens the action surface to add a
///   pane, `c` creates a session inline, `:` summons the command palette. `Esc`
///   is inert at the base Switch (the home screen is not left by backing out).
///   Switch is also re-entered from Focus / Attached via `Ctrl-O`, where `Esc`
///   then backs out to where it was opened from.
/// - **在席 (Focus)** — a session is selected and operated in the right pane,
///   either as a menu of its runnable commands or a session-scoped prompt
///   (chosen by the [`SessionActionUi`] setting). Launching `terminal` / `agent`
///   adds a pane and attaches it; `Esc` returns to Switch; `Ctrl-O` opens
///   Switch; `:` summons the command palette; `Ctrl-P`/`Ctrl-N` move the focused
///   session's active tab.
/// - **没入 (Attached)** — the embedded shell / agent is live in the right pane
///   and keys flow to it. The reserved keys are `Ctrl-O` (zoom out to Switch,
///   where panes are added) and `Ctrl-P`/`Ctrl-N` (switch to the previous / next
///   tab in place, without detaching); everything else, including `Esc`, goes to
///   the shell. The shell exiting returns to Focus.
///
/// The **command palette** (`:`, from Switch or Focus) floats the workspace
/// command line over the panes (`session` / `config` / `doctor` / `man` / …);
/// results render in its own band, `Esc` closes it, and a command with a
/// transitioning effect closes it as it acts.
///
/// The workspace root and every side-effecting hook the loop drives — appending
/// run commands to history, dispatching background session create / remove,
/// embedding the terminal pane, previewing / navigating tabs, opening the
/// settings screen — are bundled into [`Wiring`]; see its fields for each hook's
/// contract. Tests build a `Wiring` of fakes (via [`event_loop_compat`]) so the
/// loop's logic is exercised without a real terminal or shell pool.
/// Apply a session list a background sync produced (a pane-exit detach, or the
/// entry re-sync), if one has landed, refreshing the worktree statuses without
/// yanking the cursor; a slot with no sync yet leaves the state untouched.
/// Returns whether a list was applied, so the loop forces a repaint (the new git
/// statuses are not part of the badge snapshot the skip-paint check compares).
/// Split out of [`event_loop`] so the apply is exercised directly rather than
/// only through a full loop run.
/// Persist the engagement to restore on the next launch, just before a confirmed
/// quit: the focused (cursor) session's name and how deeply it was engaged
/// ([`HomeState::resume_level`], which consumes any 没入 arm and otherwise reads
/// the current mode). Routed through [`Wiring::save_resume`] so the disk write
/// lives in [`super::run`] and tests observe it through a capture.
fn save_resume_focus(state: &mut HomeState, wiring: &mut Wiring) {
    let session = state.list().selected_name().to_string();
    let level = state.resume_level();
    (wiring.save_resume)(&session, level);
    (wiring.save_last_active)(&state.last_active_flush());
}

/// Whether a left click should act on the session list: in 切替 (Switch), where it
/// is the picker, and in 在席 (Focus), where the list still shows beside the action
/// surface so a click re-focuses onto another session (see [`focus_click`]). Not in
/// 没入 (Attached) — there the right pane owns the pointer. In either acting mode a
/// click while a modal / palette / note editor / inline create / rename is open is
/// ignored, mirroring how those overlays capture every key in the loop below — so a
/// stray click never reaches the session list beneath them.
fn click_selects_session(state: &HomeState) -> bool {
    matches!(state.mode(), Mode::Switch | Mode::Focus)
        && !state.quit_confirm()
        && state.remove_modal().is_none()
        && state.text_modal().is_none()
        && state.preview().is_none()
        && state.note_editor().is_none()
        && !state.command_palette_open()
        && !state.is_creating()
        && !state.is_renaming()
}

fn apply_pending_refresh(state: &mut HomeState, refresh: &SessionsRefreshHandle) -> bool {
    match refresh.take() {
        Some(sessions) => {
            state.refresh_sessions(sessions);
            true
        }
        None => false,
    }
}

fn bump_interaction_epoch(wiring: &mut Wiring) {
    wiring.interaction_epoch = wiring.interaction_epoch.saturating_add(1);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    refresh: &SessionsRefreshHandle,
    ai_available: &OneShot<bool>,
    installed_agents: &OneShot<Vec<AgentCli>>,
    tasks: &TaskHandle,
    wiring: &mut Wiring,
) -> Result<Outcome> {
    let workspace_root = wiring.workspace_root;
    let mut painter = FramePainter::new();
    // Re-attach a session restored into 没入 (Attached) from the last quit. The
    // cursor was focused synchronously at startup ([`HomeState::restore_focus`]),
    // but attaching needs this loop's terminal wiring, so it runs once here on the
    // first pass — by now `restore_open_panes` has re-spawned the session's panes,
    // so it is live to attach. A no-op when nothing was armed (the usual case).
    handlers::resume_attach(term, &mut state, &mut painter, wiring);
    // What the last paint reflected, so an idle 切替 (Switch) tick whose badges
    // and update notice are unchanged can skip rebuilding and repainting the whole
    // frame. `force_paint` keeps the first frame — and the frame after any key —
    // always repainting.
    let mut last_update = None;
    let mut force_paint = true;
    // Whether the last paint drew the mascot mid-blink, so the frame that reopens
    // its eyes (an idle tick, not a keypress) still repaints in a quiet 切替 rather
    // than being skipped — leaving the eyes stuck shut.
    let mut last_blinking = false;
    // The previous left click's session row and time, so a second click on the
    // same row within the double-click window confirms it (see [`switch_click`]).
    let mut last_click: Option<(usize, Instant)> = None;
    // The monitor snapshot version last applied to `state`. When unchanged, the
    // loop skips `monitor.snapshot()` entirely — avoiding the clone of every badge
    // set on each idle/live-frame pass.
    let mut seen_badge_version = u64::MAX;
    loop {
        // Mark each background session's agent state — running, waiting for
        // input, live (ready), and finished — before painting, applying every
        // badge set together (read under a single lock) so the frame never mixes
        // one set's fresh reading with another's stale one.
        let badge_version = monitor.badge_version();
        let badges_changed = if badge_version != seen_badge_version {
            let badges = monitor.snapshot();
            // Whether the sidebar badges moved since the last paint, decided
            // before storing them so the snapshot can be applied by move rather
            // than cloned (the loop no longer keeps its own copy alongside the one
            // in `state`).
            let changed = state.badges() != &badges;
            state.apply_badges(badges);
            seen_badge_version = badge_version;
            changed
        } else {
            false
        };
        // Surface the sidebar mascot's "update available" notice once the
        // background release check has found a newer version than this build.
        let latest_update = update.status().map(|status| status.latest);
        state.set_update(latest_update);
        // Apply a session list a background sync produced — a pane-exit detach, or
        // the one-shot entry re-sync — if one has landed, refreshing the worktree
        // statuses without yanking the cursor. Done before the task drain below so
        // a session create / remove that finished on the same frame still has the
        // last word on the list. A landed refresh changes the sidebar git statuses
        // (which the badge snapshot does not capture), so `refreshed` forces a
        // repaint below.
        let refreshed = apply_pending_refresh(&mut state, refresh);
        // Flip the `ai` command on once the background local-LLM probe confirms it
        // is usable (drained once); until then the 在席 menu simply omits it. Force a
        // repaint so the change is reflected without waiting for the next keypress.
        if let Some(available) = ai_available.take() {
            state.set_ai_available(available);
            force_paint = true;
        }
        // Fill in the installed-agent list once the background PATH probe lands
        // (drained once), so 在席's agent picker can offer the alternatives it found;
        // until then the picker simply shows none. Force a repaint so the picker
        // reflects them without waiting for the next keypress.
        if let Some(agents) = installed_agents.take() {
            state.set_installed_agents(agents);
            force_paint = true;
        }
        // Apply any background session task (create / remove) that finished since
        // the last frame: evict the removed session's pooled shell (on this
        // thread — the pool is not `Send`), then log the result and refresh the
        // session list without yanking the cursor. Then refresh the task panel
        // rows so in-flight work shows in the top-right corner.
        let mut completed_any = false;
        for completion in tasks.drain_completed() {
            completed_any = true;
            let super::tasks::Completion {
                line,
                sessions,
                target_root,
                evict,
                focus,
            } = completion;
            if let Some(path) = evict {
                (wiring.evict_pool)(&path);
            }
            state.apply_task_completion(line, sessions, target_root.as_deref());
            // A finished create asks to drop into 在席 (Focus) on its new session.
            // Done after the refresh above so the new branch is in the list to
            // match. Unlike that refresh — which deliberately keeps the cursor put
            // for background changes — this is the user's own create landing, so
            // moving the cursor onto it is the intended result.
            if let Some(focus) = focus {
                if focus.interaction_epoch == wiring.interaction_epoch {
                    state.enter_focus_named(&focus.name);
                }
            }
        }
        state.set_tasks(tasks.view(Instant::now()));
        // Drop any stale surface every frame, then refresh it for the modes that
        // draw the embedded terminal: 没入 (driven directly by `open_pane`, which
        // clears its own surface on the way out) and 切替, where the right pane
        // previews the highlighted session's live terminal — with its tab strip
        // above it, so `←`/`→` has something to act on — so the user sees the
        // actual screen re-attaching reveals.
        state.clear_terminal_surface();
        // Collapsed to the rail, 切替's create / rename input takes over the right
        // pane (no room inline in the 5-column list), so there is no preview to
        // draw then; otherwise preview the highlighted session, sized to the
        // current sidebar state so the snapshot fills the pane it is drawn into.
        let input_in_right_pane = state.sidebar() == Sidebar::Rail
            && (state.create().is_some() || state.rename().is_some());
        // 切替 previews the highlighted session; 在席 previews the focused session's
        // selected pane — both read the same live snapshot + tab strip, so the
        // focused session's panes show as tabs and the chosen one previews live
        // (an idle session has no live snapshot, so the strip stays absent and the
        // action surface shows instead).
        let drives_surface = matches!(state.mode(), Mode::Switch | Mode::Focus);
        // The note editor opened from 没入 (`Ctrl-E`) floats over the attached
        // session's pane, which keeps drawing in Attached mode while the editor is
        // open. The surface was just cleared, so refresh it here too — otherwise
        // the live terminal would not show behind the floating box, and the empty
        // fallback pane (a one-line starting hint) would be too short to hold the
        // box, clipping its bottom border as the note grows with each newline.
        let attached_note = state.mode() == Mode::Attached && state.note_editor().is_some();
        let drive_now = (drives_surface && !input_in_right_pane) || attached_note;
        // Refresh the surface for the mode that draws it, when the highlighted /
        // focused session has a live snapshot. Folded into one `if let` (rather
        // than a guard `if` wrapping an inner `if let`) so the whole refresh is a
        // single covered branch.
        if let Some((dir, view)) = drive_now
            .then(|| selected_dir(&state, workspace_root))
            .and_then(|dir| (wiring.preview)(&dir, state.sidebar()).map(|view| (dir, view)))
        {
            state.set_terminal_view(view);
            let (labels, active) = (wiring.tab_op)(&dir, None);
            state.set_terminal_tabs(labels, active);
        }
        // The task panel and the install rabbit animate on the clock, so a frame
        // showing either must repaint even when nothing else moved.
        let now = Instant::now();
        let panel_animating = install_task::handle().is_active(now) || tasks.is_active(now);
        // Refresh the sidebar mascot for this paint: reopen its eyes once a blink's
        // window has passed and advance the Working paw on the live tick. Reactive,
        // not timed — it rides paints that already happen, so a settled mascot
        // leaves `mascot_blinking` false and a truly idle 切替 still skips painting.
        state.tick_mascot(now);
        let blink_changed = state.mascot_blinking() != last_blinking;
        // In a quiet base 切替 (Switch) — no live preview in the right pane and no
        // command palette open — an idle frame's only moving parts are the sidebar
        // badges, the update notice, and those time-animated panels. When none
        // changed since the last paint — and no key was just pressed
        // (`force_paint`) and no background task just finished — skip rebuilding
        // and repainting the whole frame. Anything with a live pane (a 切替 preview
        // of a running session, 在席, 没入) or the palette open repaints as before,
        // so a live pane is never frozen stale. The cheap per-frame state updates
        // above still run, so the next paint (when something does change) is
        // correct.
        let skip_paint = state.mode() == Mode::Switch
            && state.terminal_view().is_none()
            && !state.command_palette_open()
            && !force_paint
            && !completed_any
            && !refreshed
            && !panel_animating
            && !badges_changed
            // A mascot blink (or the frame that ends one) is a moving part too, so
            // it repaints rather than freezing the eyes mid-blink.
            && !state.mascot_blinking()
            && !blink_changed
            && last_update == latest_update;
        let (height, width) = term.size();
        if !skip_paint {
            // Stamp the frame's render time so the left pane's "Nmin ago" labels track
            // real time. Only on a real paint — a skipped frame draws nothing, so
            // the label refreshes on the next change rather than ticking every
            // second (keeping the loop's repaint budget low).
            state.set_now(chrono::Utc::now());
            let frame = ui::render_frame(height as usize, width as usize, &state);
            painter.paint(term, frame)?;
        }
        last_update = latest_update;
        last_blinking = state.mascot_blinking();
        force_paint = false;

        // Read the next input event. A wheel turn is read and dropped (the TUI
        // never scrolls in place; the embedded pane scrolls its own history
        // separately), and a click only ever pokes the sidebar mascot — neither is
        // a key, so both loop without dispatching one.
        //
        // While a background install or a session task is in flight — or any
        // session is live, or the mascot is mid-animation — the read wakes every
        // `ANIM_TICK` so the loop re-iterates: re-draining finished work, re-reading
        // the monitor badges and update notice, and (when something changed)
        // repainting — which also advances the task panel's, install rabbit's, and
        // mascot's time-based animation. This is what keeps a live background
        // agent's badge moving to waiting (◆) / finished (✓) — and a click reaction
        // playing out — without the user typing. With nothing in flight and no live
        // session it blocks on the next input, so a truly idle screen costs nothing.
        let animate = panel_animating
            || state.has_live_sessions()
            || state.mascot_blinking()
            || state.mascot_reacting();
        let input = if animate {
            match reader.read_input_timeout(install_task::ANIM_TICK) {
                Ok(Some(input)) => input,
                // A tick with no input: re-iterate to drain and repaint.
                Ok(None) => continue,
                // A delivered signal (crossterm installs a SIGWINCH handler that
                // persists after the embedded pane; an exiting agent also raises
                // SIGCHLD) interrupts the blocking read with `EINTR`. That is not a
                // request to quit — a real Ctrl-C arrives as `Key::CtrlC`, handled
                // below — so swallow it and re-iterate, exactly like an idle tick.
                // Quitting here dropped the user out of the alternate screen and
                // revealed the pre-launch scrollback whenever a signal landed
                // mid-read (e.g. exiting an agent, then `Ctrl-O` while waiting).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read input")),
            }
        } else {
            match reader.read_input() {
                Ok(input) => input,
                // An interrupted read (a delivered signal) is not a quit: re-read.
                // See the animate branch above for the full rationale.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read input")),
            }
        };
        let key = match input {
            Input::Key(key) => {
                bump_interaction_epoch(wiring);
                key
            }
            // The TUI never scrolls in place: read the wheel turn and drop it.
            Input::Scroll(_) => {
                bump_interaction_epoch(wiring);
                continue;
            }
            // A click on a session row in the left pane acts on it: in 切替 (Switch)
            // it selects the row (a second click on the same row confirms it, like
            // `Enter`); in 在席 (Focus) it re-focuses onto that session (a second
            // click attaches its pane when live). A click on the resting sidebar
            // mascot makes it react; anywhere else it is ignored. The two hit
            // disjoint regions, so the session list is tried first and the mascot
            // only when it misses. No key was pressed either way, so repaint only
            // when the click actually did something.
            Input::Click(click) => {
                bump_interaction_epoch(wiring);
                // A pinned PR popup intercepts the click first: a `#<number>` opens
                // that PR in the browser, a click elsewhere in the box keeps it
                // pinned, and a click outside dismisses it — consuming the click so
                // it neither selects a row nor re-pins on the same press.
                if state.pr_popup().is_some() {
                    match ui::pr_popup_click(
                        &state,
                        height as usize,
                        width as usize,
                        click.col,
                        click.row,
                    ) {
                        ui::PopupClick::Open(url) => (wiring.open_url)(&url),
                        ui::PopupClick::Inside => {}
                        ui::PopupClick::Outside => {
                            state.set_pr_popup(None);
                            force_paint = true;
                        }
                    }
                    continue;
                }
                // No popup open: a click on a session's PR badge pins that session's
                // popup so the pointer can travel into it and click a `#<number>`.
                if let Some(idx) = ui::sidebar_pr_badge_at(
                    &state,
                    height as usize,
                    width as usize,
                    click.col,
                    click.row,
                ) {
                    state.set_pr_popup(Some(idx));
                    force_paint = true;
                    continue;
                }
                // 在席's right pane owns its tab strip too: clicking a pane tab
                // previews/activates that live pane through the same `tab_op`
                // keyboard navigation uses. 切替 deliberately keeps the right
                // pane a dim preview; 没入 handles its own tab clicks inside the
                // pane driver.
                if state.mode() == Mode::Focus {
                    if let Some(index) = ui::focus_tab_at(
                        &state,
                        click.col,
                        click.row,
                        height as usize,
                        width as usize,
                    ) {
                        if let Some(index) = state.focus_select_pane_tab(index) {
                            let dir = selected_dir(&state, wiring.workspace_root);
                            (wiring.tab_op)(&dir, Some(TabNav::To(index)));
                        }
                        force_paint = true;
                        continue;
                    }
                }
                let selected = click_selects_session(&state)
                    .then(|| {
                        ui::left_pane_session_at(
                            &state,
                            click.col,
                            click.row,
                            height as usize,
                            width as usize,
                        )
                    })
                    .flatten();
                match selected {
                    Some(row) => {
                        let now = Instant::now();
                        if state.mode() == Mode::Focus {
                            focus_click(
                                term,
                                &mut state,
                                &mut painter,
                                wiring,
                                row,
                                now,
                                &mut last_click,
                            );
                        } else {
                            switch_click(
                                term,
                                &mut state,
                                &mut painter,
                                wiring,
                                row,
                                now,
                                &mut last_click,
                            );
                        }
                        force_paint = true;
                    }
                    None => {
                        if handle_mascot_click(term, &mut state, click) {
                            force_paint = true;
                        }
                    }
                }
                continue;
            }
            // A bare pointer move no longer drives the PR popup — it is pinned by a
            // badge click and dismissed only by a click or a keypress — so motion
            // reports are ignored. Moving the pointer toward the popup to click a
            // `#<number>` must not dismiss it. No key was pressed, so it loops
            // without dispatching one.
            Input::Hover(_) => continue,
        };
        // A key was pressed: whatever it does to the state, repaint on the next
        // iteration (the skip above only applies to idle ticks that read no key).
        force_paint = true;
        // Touching the keyboard dismisses the pinned PR popup (so `Esc` — or any
        // key — closes it), so it never lingers over a screen the user has moved on
        // from: a stale popup would otherwise survive a keypress, a mode change, or
        // attaching a pane, since those paths read no click to dismiss it.
        state.set_pr_popup(None);
        // Nudge the resting mascot to blink back at the user — reactive, so the
        // rabbit reacts the moment a key lands without any idle timer. Only while
        // it shows an open-eyed face (切替 / 在席); 没入's heads-down face has no eyes
        // to blink and animates on the live tick instead. A fresh `now` (the read
        // may have blocked a while) so the blink's window starts from the keypress;
        // the call is a no-op when the mascot animation is turned off.
        if matches!(state.mode(), Mode::Switch | Mode::Focus) {
            state.kick_mascot_blink(Instant::now());
        }

        // Record the key press (and the mode it landed in) to the operation trace,
        // so a session's navigation can be analysed after the fact. `record_with`
        // builds the event — the timestamp, the allocation, and the `{mode} {key}`
        // `format!` — only once tracing is enabled, so the hot key loop pays
        // nothing for it while tracing is off (the default).
        crate::infrastructure::trace_log::TraceLog::record_with(|| {
            crate::domain::trace::TraceEvent::now(crate::domain::trace::TraceCategory::Tui, "key")
                .with_detail(format!("{:?} {:?}", state.mode(), key))
        });

        // The quit-confirmation modal, when open, captures every key: `y` /
        // `Enter` (or a second `Ctrl-C` / `Ctrl-Q`) confirms the close, `n` /
        // `Esc` cancels.
        if state.quit_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter | Key::CtrlC | Key::Char(CTRL_Q) => {
                    save_resume_focus(&mut state, wiring);
                    return Ok(Outcome::Quit);
                }
                Key::Char('n') | Key::Char('N') | Key::Escape => state.cancel_quit_confirm(),
                _ => {}
            }
            continue;
        }

        // The update-confirmation modal, when open, captures every key: `y` /
        // `Enter` launches the self-update (and closes the modal — its progress
        // then shows as the shared loading rabbit), `n` / `Esc` cancels.
        //
        // `Ctrl-C` / `Ctrl-Q` also cancel here. This block sits above the global
        // quit chords below, so without handling them they would be inert while
        // this modal is open (unlike every other overlay, which sits below those
        // handlers and so passes them through). Routing them to the global path
        // instead would raise the quit modal on top of this one, but the two are
        // documented never to coexist; cancelling first — a second press then
        // quits — keeps that invariant.
        if state.update_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => {
                    (wiring.dispatch_update)();
                    state.cancel_update_confirm();
                }
                Key::Char('n') | Key::Char('N') | Key::Escape | Key::CtrlC | Key::Char(CTRL_Q) => {
                    state.cancel_update_confirm()
                }
                _ => {}
            }
            continue;
        }

        // `Ctrl-C` closes the app from anywhere on the home screen. Quitting
        // would drop any session whose shell / agent is still running, so when
        // one is live we raise the quit-confirmation modal first instead of
        // closing outright; an idle screen quits immediately.
        if let Key::CtrlC = key {
            if state.has_live_sessions() {
                state.open_quit_confirm();
            } else {
                save_resume_focus(&mut state, wiring);
                return Ok(Outcome::Quit);
            }
            continue;
        }

        // `Ctrl-Q` is the dedicated quit chord: unlike `Ctrl-C` it *always* raises
        // the quit-confirmation modal first, idle or live, so the app never closes
        // on a single keystroke. (没入's `Ctrl-Q` lands here too: the pane detaches
        // and `open_pane` opens the same modal on the way out.)
        if let Key::Char(CTRL_Q) = key {
            state.open_quit_confirm();
            continue;
        }

        // The session-removal modal, when open, captures every key: the cursor
        // moves with the arrows (or j/k), Space toggles the row's checkbox, and
        // Enter removes every checked session (Esc cancels).
        if state.remove_modal().is_some() {
            match key {
                // Cursor moves and checkbox toggles route to the modal's own
                // methods; Enter / Esc are lifecycle on the screen state.
                Key::ArrowUp | Key::Char('k') => {
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.move_up();
                    }
                }
                Key::ArrowDown | Key::Char('j') => {
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.move_down();
                    }
                }
                Key::Char(' ') => {
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.toggle();
                    }
                }
                Key::Enter => {
                    if let Some((entries, force)) = state.submit_remove_modal() {
                        // Each checked session is dispatched to a background
                        // worker, so the loop never blocks on the git work; the
                        // task panel stacks them and the loop drains each as it
                        // finishes. Each row already carries the owning workspace
                        // root, which keeps 統合(unite) bulk-removal correct even
                        // when different workspaces contain the same session name.
                        for entry in &entries {
                            let root = entry.root_path().to_path_buf();
                            state.set_op_target(root.clone());
                            (wiring.dispatch_remove)(&root, entry.name(), force);
                        }
                    }
                }
                Key::Escape => state.cancel_remove_modal(),
                _ => {}
            }
            continue;
        }

        // The text modal (a text-dumping command's output, e.g. `man`), when open,
        // captures every key: the arrows / `j`/`k` and PageUp/PageDown scroll it,
        // and `Esc` / `Enter` / `q` dismiss it.
        if let Some(size) = state.text_modal().map(|modal| modal.size) {
            // Page by exactly the window the renderer shows, so PageUp/PageDown
            // move one screenful for both the compact and the large `man` modal.
            let (_, page) = ui::text_modal_geometry(height as usize, width as usize, size);
            match key {
                Key::ArrowUp | Key::Char('k') => state.text_modal_scroll_up(),
                Key::ArrowDown | Key::Char('j') => state.text_modal_scroll_down(page),
                Key::PageUp => {
                    for _ in 0..page {
                        state.text_modal_scroll_up();
                    }
                }
                Key::PageDown => {
                    for _ in 0..page {
                        state.text_modal_scroll_down(page);
                    }
                }
                Key::Escape | Key::Enter | Key::Char('q') => state.close_text_modal(),
                _ => {}
            }
            continue;
        }

        // The right-pane Markdown preview, when open, captures every key: the
        // arrows / `j`/`k` and PageUp/PageDown scroll within the pane, and `Esc` /
        // `Enter` / `q` dismiss it (returning the right pane to the default).
        if state.preview().is_some() {
            let page = ui::preview_visible(height as usize, width as usize, &state);
            match key {
                Key::ArrowUp | Key::Char('k') => state.preview_scroll_up(),
                Key::ArrowDown | Key::Char('j') => state.preview_scroll_down(page),
                Key::PageUp => {
                    for _ in 0..page {
                        state.preview_scroll_up();
                    }
                }
                Key::PageDown => {
                    for _ in 0..page {
                        state.preview_scroll_down(page);
                    }
                }
                Key::Escape | Key::Enter | Key::Char('q') => state.close_preview(),
                _ => {}
            }
            continue;
        }

        // The session-note editor, when open, captures every key: `Ctrl-S` saves,
        // `Esc` cancels, `Enter` inserts a newline, and the editing keys edit the
        // multi-line buffer. It is driven through a handler (not inline like the
        // pure modals above) because closing it from 没入 re-attaches the pane,
        // which needs the terminal / wiring.
        if state.note_editor().is_some() {
            note_editor_key(term, &mut state, &mut painter, key, wiring);
            continue;
        }

        // The workspace command palette (`:`), when open, captures every key:
        // editing / completion / recall and `Enter` to run a command, `Esc` to
        // close. It sits below the pure overlays above (a `man` / `session list`
        // text dump it runs layers its modal on top of the palette), and above the
        // sidebar toggle and per-mode dispatch (so `:`-typed text never leaks to
        // the session list / focus surface beneath it).
        if state.command_palette_open() {
            let flow = palette_key(term, &mut state, &mut painter, key, wiring)?;
            if let Flow::Quit = flow {
                save_resume_focus(&mut state, wiring);
                return Ok(Outcome::Quit);
            }
            continue;
        }

        // `Ctrl-B` collapses / expands the left session sidebar from anywhere on
        // the (non-modal) screen. It is a pure view toggle, so it is handled here
        // before the per-mode dispatch rather than threaded through each handler.
        // 没入's keys never reach this loop (the pane driver owns them), so its
        // Ctrl-B is handled inside `terminal::pane` instead.
        if let Key::Char(CTRL_B) = key {
            state.toggle_sidebar();
            continue;
        }

        // The per-mode handlers never quit (only the command palette's `Enter` and
        // the quit-confirm modal do — both handled above), so their `Flow` is
        // discarded rather than matched for a now-dead `Quit` arm.
        match state.mode() {
            Mode::Switch => {
                switch_key(term, &mut state, &mut painter, key, wiring);
            }
            // 没入 (Attached) is driven inside `open_pane`, which always leaves it
            // (for 切替 or 在席) before returning — so the loop only ever observes
            // 在席 here. It shares the 在席 handler to keep the match total without a
            // separate, unreachable arm.
            Mode::Focus | Mode::Attached => {
                focus_key(term, &mut state, &mut painter, key, wiring);
            }
        }
    }
}

/// What handling a key (or driving the embedded pane it opened) resolved to.
enum Flow {
    /// Resume the home screen.
    Continue,
    /// Quit the application.
    Quit,
}

/// Paint the current frame immediately, outside the loop's top-of-iteration
/// paint. Used to flush a transient [`HomeState`] state — the loading rabbit —
/// to the screen just before a blocking action runs, since the action would
/// otherwise hold the loop until it returned without ever drawing the indicator.
/// Errors are the caller's to ignore: a missed transient frame must not abort
/// the action it was announcing.
pub(super) fn paint_now(term: &Term, painter: &mut FramePainter, state: &HomeState) -> Result<()> {
    let (height, width) = term.size();
    let frame = ui::render_frame(height as usize, width as usize, state);
    painter.paint(term, frame)
}

/// The directory the pane should root at for the focused list row: the selected
/// worktree's path, or the workspace root when the cursor is on a root row (which
/// belongs to no session, so `selected()` is `None`). In 統合(unite) mode a root
/// row past the first group resolves to *that group's* workspace, so a root-row
/// `terminal` / `agent` opens in the workspace the cursor is pointing at; the
/// primary group's root row uses `workspace_root` (the screen's base directory).
fn selected_dir(state: &HomeState, workspace_root: &Path) -> PathBuf {
    if let Some(w) = state.list().selected() {
        return w.path.clone();
    }
    // A root row: the primary group uses the screen's base root; an extra (unite)
    // group uses its own workspace root.
    if state.list().selected_group() == 0 {
        workspace_root.to_path_buf()
    } else {
        state.selected_workspace_root()
    }
}

/// Whether a click is allowed to poke the sidebar mascot: only on the plain home
/// view (切替 / 在席) with nothing floating over the panes. Anywhere an overlay
/// sits — the quit-confirm / removal / text modals, the Markdown preview, the note
/// editor, or the command palette — a click is meant for it (or for nothing), not
/// the rabbit drawn beneath it.
fn mascot_clickable(state: &HomeState) -> bool {
    matches!(state.mode(), Mode::Switch | Mode::Focus)
        && !state.quit_confirm()
        && !state.update_confirm()
        && state.remove_modal().is_none()
        && state.text_modal().is_none()
        && state.preview().is_none()
        && state.note_editor().is_none()
        && !state.command_palette_open()
}

/// Handle a mouse click: when it lands on the resting sidebar mascot (and the
/// screen is in a state where the rabbit is clickable), let the mascot respond
/// ([`HomeState::click_mascot`] — raise the update-confirmation modal when it is
/// announcing an update, otherwise a playful one-shot reaction) and report `true`
/// so the loop repaints. A click anywhere else — or while an overlay is up — is
/// ignored (`false`), so nothing else on the TUI is click-driven. The mascot's
/// screen rectangle is recomputed from the same layout the renderer used
/// ([`ui::mascot_hit_rect`]), so the hit-test matches exactly where the rabbit was
/// drawn.
fn handle_mascot_click(term: &Term, state: &mut HomeState, click: ClickEvent) -> bool {
    if !mascot_clickable(state) {
        return false;
    }
    let (height, width) = term.size();
    click_hits_mascot(height as usize, width as usize, state, click)
        .then(|| state.click_mascot(Instant::now()))
        .is_some()
}

/// Whether `click` lands on the sidebar mascot's body for a terminal of the given
/// size. Split from [`handle_mascot_click`] (which owns the mode/overlay gate and
/// the side effect) so the geometry — including the "no mascot shown" case — is
/// unit-testable at an explicit size rather than the live terminal's.
fn click_hits_mascot(height: usize, width: usize, state: &HomeState, click: ClickEvent) -> bool {
    ui::mascot_hit_rect(height, width, state)
        .is_some_and(|rect| rect.contains(click.col, click.row))
}

/// Test-only adapter that keeps the event-loop tests' synchronous shape —
/// `create_session` / `remove_session` returning a [`SessionOutcome`] — against
/// the loop's background-task model. Each dispatch runs the fake inline and
/// queues its outcome on a fresh task handle, so the loop drains it on the next
/// frame exactly as a finished worker thread would. Pool eviction is a no-op
/// (the tests have no pool).
///
/// The synchronous fakes are taken **by value** (as `impl FnMut`) so they are
/// owned locals here, sharing one lifetime with the `dispatch_*` wrappers built
/// below — which is what lets them all be bundled into a single [`Wiring`].
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn event_loop_compat(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: HomeState,
    workspace_root: &Path,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    ai_available: &OneShot<bool>,
    installed_agents: &OneShot<Vec<AgentCli>>,
    mut persist: impl FnMut(&str),
    mut create_session: impl FnMut(&str) -> SessionOutcome,
    mut rename_display: impl FnMut(&str, &str) -> SessionOutcome,
    mut set_note: impl FnMut(&str, &str) -> SessionOutcome,
    mut remove_session: impl FnMut(&str, bool) -> SessionOutcome,
    mut existing_branches: impl FnMut() -> Vec<String>,
    mut open_terminal: impl FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    mut open_config: impl FnMut(&Term) -> Result<Option<ConfigReload>>,
    mut preview: impl FnMut(&Path, Sidebar) -> Option<TerminalView>,
    mut tab_op: impl FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize),
    mut close_tab: impl FnMut(&mut HomeState, &Path),
    mut reorder_session: impl FnMut(&str, bool) -> SessionReorder,
) -> Result<Outcome> {
    let tasks = TaskHandle::new();
    // The unite target root is irrelevant to these single-workspace test fakes, so
    // the shim accepts it (matching the production [`Wiring`] signature) and drops it.
    let mut dispatch_create = |_root: &Path, name: &str, interaction_epoch: u64| {
        let id = tasks.begin(super::tasks::TaskKind::CreateSession, name);
        let outcome = create_session(name);
        // Mirror a production create, which carries the new branch to focus, so the
        // loop's auto-focus path is exercised; a fake whose `create_session` reports
        // no new sessions just won't match it.
        let focus = outcome.sessions.as_ref().map(|_| super::tasks::AutoFocus {
            name: name.to_string(),
            interaction_epoch,
        });
        tasks.complete(
            id,
            true,
            super::tasks::Completion {
                line: outcome.line,
                sessions: outcome.sessions,
                target_root: Some(_root.to_path_buf()),
                evict: None,
                focus,
            },
        );
    };
    let mut dispatch_remove = |_root: &Path, name: &str, force: bool| {
        let id = tasks.begin(super::tasks::TaskKind::RemoveSession, name);
        let outcome = remove_session(name, force);
        // Mirror a production removal, which carries the session root to evict, so
        // the loop's eviction path is exercised; the tests' `evict_pool` is a
        // no-op (they have no pool).
        let evict = outcome
            .sessions
            .as_ref()
            .map(|_| std::path::PathBuf::from(name));
        tasks.complete(
            id,
            true,
            super::tasks::Completion {
                line: outcome.line,
                sessions: outcome.sessions,
                target_root: Some(_root.to_path_buf()),
                evict,
                focus: None,
            },
        );
    };
    let mut evict_pool = |_: &Path| {};
    // The self-update spawn is real IO wired in `super::run`; here it is a no-op,
    // so the compat-shim loop tests never shell out. The dispatch path itself is
    // covered by the dedicated update-modal tests that build a capturing `Wiring`.
    let mut dispatch_update = || {};
    // The resume-focus persistence is exercised through its own state unit tests
    // ([`HomeState::resume_level`] / `restore_focus`); here it is a no-op, so a
    // quit in these loop tests does not touch the store.
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    // The fakes have no equivalent of the production pane-exit sync thread that
    // fills this, so it stays empty here; the apply path is covered directly in
    // `a_background_refresh_updates_the_session_list`.
    let refresh = SessionsRefreshHandle::new();
    // The sync rename / note fakes are single-workspace (no root arg); wrap them to
    // the production 3-arg shape, dropping the unite target root.
    let mut rename_display_w = |_root: &Path, name: &str, label: &str| rename_display(name, label);
    let mut set_note_w = |_root: &Path, name: &str, note: &str| set_note(name, note);
    // `unite add` is not exercised by the compat-shim loop tests; report no match.
    let mut unite_resolve =
        |name: &str| Err::<GroupSource, String>(format!("no workspace named \"{name}\""));
    // Opening a PR in the browser is a no-op here so the compat-shim loop tests
    // never shell out; the open path itself is covered by the dedicated popup tests
    // that build a capturing `Wiring`.
    let mut open_url = |_: &str| {};
    let mut wiring = Wiring {
        interaction_epoch: 0,
        workspace_root,
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename_display_w,
        set_note: &mut set_note_w,
        reorder_session: &mut reorder_session,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict_pool,
        existing_branches: &mut existing_branches,
        open_terminal: &mut open_terminal,
        open_url: &mut open_url,
        open_config: &mut open_config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close_tab,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };
    event_loop(
        term,
        reader,
        state,
        monitor,
        update,
        &refresh,
        ai_available,
        installed_agents,
        &tasks,
        &mut wiring,
    )
}

#[cfg(test)]
mod tests;
