//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal` / `agent`, the right pane switches to a live
//! shell (没入) drawn while the whole workspace frame — sidebar and all — stays
//! on screen. The shell itself is owned by the [`TerminalPool`] (so it survives
//! leaving the pane); this module borrows it and runs the render/input loop.
//! Keystrokes are forwarded to the shell as raw bytes.
//!
//! Which keys the pane **reserves** for its own navigation (rather than
//! forwarding to the shell) depends on the configured `KeyScheme`, and is decided
//! purely by [`classify`](super::super::pane_input::classify): the default prefix
//! scheme claims only the `Ctrl-O` leader (the action is the next key), while the
//! `Alt` scheme claims a single `Alt`-chord per action and no bare Ctrl key. The
//! navigation actions, however the scheme spells them, are: zoom out to 選択
//! (Overview) ([`PaneStep::Detach`]) — leaving the pane on the left pane while every
//! pane stays alive in the pool, where the user moves between sessions (`↑`/`↓`),
//! between this session's tabs (`←`/`→`), re-attaches (`Enter`), adds a pane
//! (`t`), or summons the `:` command palette; next / previous tab in place
//! ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]), as does a left click on a tab
//! chip ([`PaneStep::ToTab`]); zoom out to 集中 (Closeup) — the session's action menu
//! — ([`PaneStep::ToCloseup`]); add an agent tab ([`PaneStep::NewAgentTab`]) without
//! leaving 没入; close the active tab in place ([`PaneStep::CloseTab`]); open the
//! session-note editor ([`PaneStep::OpenNote`]); and collapse
//! / expand the left sidebar in place (it never leaves 没入). `Ctrl-^` jumps to the
//! previously focused session ([`PaneStep::PrevSession`]) and `Ctrl-Q` (prefix
//! scheme) / `Alt-q` leaves 没入 to quit usagi ([`PaneStep::Quit`]), raising the
//! quit-confirmation modal on the home screen. `Esc` and `Ctrl-W` (the universal
//! shell "delete previous word") always flow to the shell; closing a tab is
//! `Ctrl-O x` / `Alt-x` here, or `x` from 選択. The shell exiting on its own
//! reports [`PaneStep::Closed`].
//!
//! `agent` reuses the same machinery: the pool sends the configured agent CLI to
//! the shell on first spawn, so the pane lands the user straight in the agent.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the input translation — which chord a key is, how far to
//! scroll, which cell the pointer hit, and the bytes a key/paste becomes
//! ([`super::super::pane_input`]); the layout geometry and frame ([`super::super::ui`]); the
//! screen snapshot ([`super::view`]); and the [`PaneExit`] vocabulary
//! ([`super::super::state`]).
//!
//! [`TerminalPool`]: super::pool::TerminalPool

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use console::Term;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;
use crate::presentation::tui::io::clipboard;
use crate::presentation::tui::io::screen::diff_frame_with_columns;

use super::super::pane_input::{
    apply_scroll, classify, classify_menu_key, classify_rename_key, encode_key, encode_mouse_wheel,
    encode_paste, is_copy, is_double_click, is_press, key_scroll_lines, pane_cell, pointer_shape,
    prefix_alive, wheel_arrows, wheel_delta, KeyAction, MenuVerdict, PointerShape, RenameEdit,
    RenameVerdict, Reserved, DOUBLE_CLICK,
};
use super::super::sessions_refresh::SessionsRefreshHandle;
use super::super::state::{HomeState, SurfaceOwner, TabMenuItem};
use super::super::ui;
use super::link;
use super::pool::{MonitorHandle, TerminalPool};
use super::selection::{Cell, Selection};
use super::tabs::TabSwap;
use super::view::TerminalView;

/// Why the embedded terminal loop handed control back, so the pool-driven loop
/// in [`super::super::run`](super) can act on it: the user zoomed out (to 選択 or 集中),
/// switched tabs, added / closed a tab, or the shell closed. Tab switching and
/// agent-tab / close management are handled in place without leaving 没入 — the
/// same actions are also reachable from 選択 (Overview) via `Detach`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneStep {
    /// `Ctrl-O`: zoom out one level (→ 選択), leaving every pane alive in the pool.
    Detach,
    /// `Ctrl-E`: leave the pane to open the session-note editor over it. The
    /// caller re-attaches the pane once the editor closes.
    OpenNote,
    /// `Ctrl-N`: switch to the next tab without leaving 没入. The caller advances
    /// the pool's active pane and re-drives it.
    NextTab,
    /// `Ctrl-P`: switch to the previous tab without leaving 没入.
    PrevTab,
    /// `Ctrl+Shift+N`: move the active tab one slot right, keeping it active.
    SwapTabRight,
    /// `Ctrl+Shift+P`: move the active tab one slot left, keeping it active.
    SwapTabLeft,
    /// A left click on a tab chip: switch to that (0-based) tab without leaving
    /// 没入. Like [`NextTab`](Self::NextTab) / [`PrevTab`](Self::PrevTab), the
    /// caller makes it active and re-drives the pane.
    ToTab(usize),
    /// A drag/drop from one tab chip to another: move the source tab to the
    /// target slot and keep that moved pane active.
    MoveTab { from: usize, to: usize },
    /// `Ctrl-T`: zoom out to 集中 (Closeup) — the session's action menu — leaving
    /// every pane alive in the pool. Adding a terminal is then a menu choice.
    ToCloseup,
    /// `Ctrl-G`: add a new agent tab and make it active, without leaving 没入.
    NewAgentTab,
    /// `Ctrl-O x` / `Alt-x`: close the active tab in place, killing its shell.
    /// The caller drives the next surviving tab, or drops back to 集中 when this
    /// was the last one (the same handling as a shell that exits on its own).
    CloseTab,
    /// A right-click tab menu's `Move left` / `Move right`: reorder the (0-based)
    /// `tab` and keep the moved pane active, without leaving 没入. The caller
    /// mutates the pool and re-drives (like [`MoveTab`](Self::MoveTab)).
    MenuMoveTab { tab: usize, swap: TabSwap },
    /// A right-click tab menu's `Rename`: relabel the (0-based) `tab` to `label`
    /// (empty resets to the generated default), without leaving 没入.
    MenuRenameTab { tab: usize, label: String },
    /// A right-click tab menu's `Close`: close the (0-based) `tab`, killing its
    /// shell. The caller drives the surviving pane, or drops to 集中 when it was
    /// the last one (the same handling as [`CloseTab`](Self::CloseTab)).
    MenuCloseTab { tab: usize },
    /// `Ctrl-^`: leave 没入 to jump to the previously focused session (vim's
    /// `Ctrl-^` / tmux's `last-window`), attaching it when live. The caller
    /// re-roots the pane on that session.
    PrevSession,
    /// A double click on a selectable sidebar row: leave 没入 to act on that row —
    /// attaching a session when live, or opening inline creation when the row is
    /// `+ new session`. The caller handles it like [`PrevSession`](Self::PrevSession).
    ToSession(usize),
    /// `Ctrl-Q`: leave 没入 to quit usagi. Every pane stays alive in the pool; the
    /// caller raises the quit-confirmation modal on the home screen rather than
    /// closing outright, so the running agents are never dropped by accident.
    Quit,
    /// The active pane's shell exited on its own (e.g. `exit`).
    Closed,
}

/// How finely the loop samples for fresh shell output while it waits for a
/// keystroke. Output, and the echo of typed keys, appear within this slice — so
/// the pane stays responsive instead of trailing a fixed redraw timer.
const POLL_SLICE: Duration = Duration::from_millis(4);

/// Once the pane has been completely quiet for this long, the wait loop backs
/// off to [`QUIET_POLL_SLICE`]. `event::poll` still wakes immediately for user
/// input; the trade-off is only that a brand-new PTY output burst may wait up to
/// the longer slice before the generation check sees it.
const QUIET_AFTER: Duration = Duration::from_secs(1);

/// Poll interval while fully idle. This cuts the idle attached-pane wakeups from
/// roughly 250/s (4 ms) to ~31/s without affecting active output/input bursts,
/// which stay on [`POLL_SLICE`].
const QUIET_POLL_SLICE: Duration = Duration::from_millis(32);

/// The longest the loop sits idle before *re-evaluating* — re-reading the
/// terminal size and the sidebar badges to decide whether anything changed.
/// Output and key presses wake it far sooner; this only bounds how stale a
/// background resize or badge change can get while nothing else happens. It no
/// longer forces a repaint (the loop repaints only on a real change), so it is
/// paced to the watcher's own poll interval rather than a tight redraw timer.
const IDLE_REEVAL: Duration = Duration::from_millis(200);

/// The shortest gap between two attached-loop autostart passes. While 没入
/// (Attached) owns the event loop, the outer loop's per-tick autostart is not
/// running, so the pane loop picks up prompts queued for pane-less sessions (an
/// MCP `session_delegate_issue` / `session_prompt`) itself. Each pass does a
/// cheap directory listing to gate the per-session work (`any_queued`), so it is
/// paced to the idle re-evaluation cadence rather than run on every fast output
/// frame while an agent streams.
const AUTOSTART_TICK: Duration = Duration::from_millis(200);

/// The shortest gap between two repaints driven purely by fresh shell output.
/// The reader thread bumps the generation once per 64 KiB read — roughly every
/// 4 ms while an agent streams — and each repaint locks the parser and
/// re-stringifies the whole grid. Coalescing output-only frames to at most one
/// per ~60 fps keeps a flood of output from pinning the CPU on redraws the eye
/// cannot see, while interactive changes (input echo, resize, scroll, selection,
/// hover, badges) still repaint immediately so the pane stays responsive.
const MIN_FRAME: Duration = Duration::from_millis(16);

/// How often an attached pane wakes purely to advance the inline session
/// create/remove skeleton in the sidebar. The skeleton frame itself is derived
/// from wall-clock time in the renderer; this timeout only gives the otherwise
/// quiescent pane loop a chance to repaint while a lifecycle operation is
/// pending. Keep it close to the UI's 90 ms skeleton step without depending on a
/// private sidebar constant.
const SESSION_SKELETON_FRAME: Duration = Duration::from_millis(90);

/// Report mouse motion with no button held (DECSET 1003), so the pane can light
/// up the link under the pointer on hover. The global mouse modes (1000/1002/1006,
/// see [`super::super::super::io::screen`]) only report clicks and drags; this is enabled
/// while the pane is up and disabled on the way out so the management screens are
/// not flooded with motion reports they would only discard.
const ENABLE_MOTION: &str = "\x1b[?1003h";
/// Stop reporting button-less mouse motion ([`ENABLE_MOTION`]).
const DISABLE_MOTION: &str = "\x1b[?1003l";

/// Run the embedded terminal in the right pane, driving the pooled shell `pty`
/// until the user detaches / switches (`Ctrl-O`) or the shell exits. The PTY is
/// owned by the caller's [`TerminalPool`] so it survives a detach; here we own
/// only raw mode and the render/input loop. The reason for returning is the
/// [`PaneExit`] the event loop acts on.
///
/// [`TerminalPool`]: super::pool::TerminalPool
#[allow(clippy::too_many_arguments)]
pub fn run(
    term: &Term,
    state: &mut HomeState,
    pool: &RefCell<TerminalPool>,
    dir: &Path,
    monitor: &MonitorHandle,
    sessions_refresh: &SessionsRefreshHandle,
    autostart: &mut dyn FnMut(&HomeState) -> Vec<String>,
) -> Result<PaneStep> {
    // Raw mode, bracketed paste, and motion reporting are entered here and
    // restored by the guard's `Drop` — including when `drive` panics and unwinds,
    // not only on the normal return path. Restoring them on unwind matters: the
    // alternate-screen guard one frame up resets the alt screen and click/drag
    // mouse modes, but it does not own these, so without this guard a panic in the
    // render/input loop would leave the user's shell in raw mode with bracketed
    // paste and motion reporting still on.
    let _modes = PaneModeGuard::enter(term)?;
    // Do not clear the screen here. The first pane repaint starts with an empty
    // diff base, so the frame diff already emits "clear + all rows" as one
    // batched terminal write. A separate `clear_screen()` would flush an
    // all-blank frame just before the real frame and is visible as a one-frame
    // flicker when switching sessions or tabs.
    drive(term, state, pool, dir, monitor, sessions_refresh, autostart)
}

/// RAII guard owning the embedded pane's terminal modes (raw mode, bracketed
/// paste, button-less mouse motion). [`run`] turns them on via [`enter`]; `Drop`
/// turns them off and shows the cursor, so they are reset on a panic-driven
/// unwind as well as a normal return.
///
/// [`enter`]: PaneModeGuard::enter
struct PaneModeGuard<'t> {
    term: &'t Term,
}

impl<'t> PaneModeGuard<'t> {
    fn enter(term: &'t Term) -> Result<Self> {
        enable_raw_mode().context("failed to enter raw mode for the embedded terminal")?;
        // Capture pastes as a single `Event::Paste` so a multi-line paste reaches
        // the shell as one block instead of a key stream whose embedded Enters
        // each submit a line to the agent (see `pump_input`).
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        // Turn on button-less motion reporting so links light up on hover.
        let _ = term.write_str(ENABLE_MOTION);
        let _ = term.flush();
        Ok(Self { term })
    }
}

impl Drop for PaneModeGuard<'_> {
    fn drop(&mut self) {
        // Restored in the reverse order they were enabled.
        // The mouse pointer (OSC 22) is intentionally *not* reset here. This guard
        // is per-`run` — i.e. per tab — and a `Ctrl-O ←/→` tab switch (or a new /
        // closed tab) drops one guard and enters the next within the *same* 没入
        // session. Resetting the pointer on every hop flipped it back to the arrow
        // until the mouse next crossed a shape boundary, so the text caret flickered
        // away on each tab switch. The default pointer is restored once 没入 hands
        // back to a management screen (see `open_pane`) and, as a teardown backstop,
        // when the whole TUI exits (see `AlternateScreenGuard`), so a caret never
        // lingers on the home screen or the user's own shell.
        let _ = self.term.write_str(DISABLE_MOTION);
        // Reset the cursor shape to the terminal default (DECSCUSR 0): the pane
        // re-asserted whatever shape its program chose, so without this a bar or
        // underline would leak into the home screen's caret — or, on quit, into
        // the user's own shell.
        let _ = self.term.write_str("\x1b[0 q");
        let _ = self.term.flush();
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = self.term.show_cursor();
    }
}

/// The render/input loop: when something actually changed — fresh shell output,
/// a resize, a scroll, or a sidebar badge — snapshot the shell screen and draw
/// the rows that differ; otherwise wait without repainting. Returns the
/// [`PaneExit`] reason when the shell exits or the user detaches / switches.
///
/// The earlier loop snapshotted and repainted the whole pane on a fixed 100 ms
/// timer even when nothing had moved, re-reading the entire grid (and contending
/// for the parser lock) ten times a second while idle. Tracking what was last
/// applied / drawn lets each pass do only the work a real change demands, so a
/// quiescent terminal costs almost nothing.
#[allow(clippy::too_many_arguments)]
fn drive(
    term: &Term,
    state: &mut HomeState,
    pool: &RefCell<TerminalPool>,
    dir: &Path,
    monitor: &MonitorHandle,
    sessions_refresh: &SessionsRefreshHandle,
    autostart: &mut dyn FnMut(&HomeState) -> Vec<String>,
) -> Result<PaneStep> {
    // The frame drawn last pass, so we only repaint the rows that changed.
    let mut prev: Vec<String> = Vec::new();
    // How many lines the pane is scrolled back into the shell's history; `0` is
    // the live screen. The wheel and `Shift`+`PageUp`/`PageDown` move it, typing
    // snaps it back, and `set_scrollback` clamps it to the buffered output.
    let mut scrollback: usize = 0;
    // The in-progress / just-finished mouse selection, drawn inverted over the
    // pane. A drag builds it, releasing copies it, and typing or scrolling
    // clears it.
    let mut selection: Option<Selection> = None;
    // The cell the pointer last moved over, so the link under it (if any) lights
    // up; `None` while the pointer is outside the pane.
    let mut hover: Option<Cell> = None;
    // A tab chip pressed with the left mouse button, held until release so a
    // press/release on the same chip remains click-to-switch while a release on
    // another chip reorders tabs.
    let mut drag_tab: Option<usize> = None;
    // When a `Ctrl-O` leader press is awaiting its action key (prefix `KeyScheme`
    // only): the instant it arrived, so the wait can lapse after `PREFIX_TIMEOUT`
    // (a forgotten leader must not turn a later `Ctrl-O` into a literal sent to
    // the agent). Held across `pump_input` calls so the two keystrokes of a prefix
    // sequence can arrive in separate input drains; `None` when nothing is pending.
    let mut pending_prefix: Option<Instant> = None;
    // What we last published as the prefix-pending hint, so the footer repaints
    // when the leader is pressed or lapses but not every idle pass.
    let mut last_prefix_pending: Option<bool> = None;
    // What we last told the PTY and last drew, so a pass that finds them
    // unchanged skips the resize ioctl, the grid snapshot, and the repaint. The
    // sentinels (a `None` geometry / scrollback / selection, a first-pass flag)
    // force the opening pass to draw.
    let mut last_geo: Option<ui::TerminalGeometry> = None;
    let mut applied_scrollback: Option<usize> = None;
    let mut last_selection: Option<Selection> = None;
    let mut last_hover: Option<Cell> = None;
    // The pinned PR popup last drawn, so opening / closing it (or moving it to
    // another session) repaints the floating box over the live pane just once.
    let mut last_pr_popup: Option<usize> = None;
    // The tab context menu / inline rename last drawn (its cursor row, and the
    // rename buffer + caret), so opening / closing the overlay, moving its cursor,
    // or typing in the rename field repaints the box over the live pane — the same
    // one-shot repaint the PR popup gets. `render_frame` draws the overlay itself
    // (shared with the home screens); this only decides when to repaint.
    let mut last_menu_cursor: Option<usize> = None;
    let mut last_rename: Option<(String, usize)> = None;
    let mut drawn_gen = match pool.borrow_mut().active_pty(dir) {
        Some(pty) => pty.generation(),
        // The session has no pane to drive (every one already closed): fall to 集中.
        None => return Ok(PaneStep::Closed),
    };
    // When the last repaint landed, so a flood of output-only frames coalesces to
    // at most one per [`MIN_FRAME`]; `None` until the first paint, which never
    // throttles.
    let mut last_paint: Option<Instant> = None;
    // The screen's URL cells cached against the generation they were detected at,
    // so hover-only / throttled frames skip the O(all cells) re-scan and reuse
    // them until the shell's output actually changes (see [`link`]).
    let mut links_cache: Option<(u64, std::collections::HashSet<Cell>)> = None;
    // The pull-request URLs last harvested from this pane's output, so the agent
    // printing them is recorded for the sidebar (and persisted) only when the set
    // changes rather than on every output frame they stay on screen (see
    // [`link::pr_links`]).
    let mut last_prs: Vec<crate::domain::workspace_state::PrLink> = Vec::new();
    // The cursor shape (DECSCUSR `Ps`) last emitted to the host terminal, so a
    // shape is re-asserted only when the program changes it. `None` until the
    // first paint, which always emits — restoring this pane's shape over whatever
    // the previously active tab left on the terminal.
    let mut last_shape: Option<u16> = None;
    // The sidebar's session create/remove skeleton animates from wall-clock time.
    // Track the frame last painted while attached so a quiet shell still repaints
    // the sidebar when the skeleton wave advances, without touching the pane when
    // no lifecycle operation is pending.
    let mut last_session_skeleton: Option<usize> = None;
    // The previous left click on a sidebar session row and when it landed, so a
    // second click on the same row within [`DOUBLE_CLICK`] confirms it (switching
    // to that session) — the same double-click-to-confirm 選択 uses. Held across
    // `pump_input` calls; `None` when no click is pending a partner.
    let mut last_click: Option<(usize, Instant)> = None;
    // The mouse pointer shape (OSC 22) last written to the host terminal, so it is
    // re-emitted only when the pointer crosses between selectable text, a clickable
    // target, and plain chrome — not on every motion report. `None` until the first
    // mouse event sets a shape.
    let mut last_pointer: Option<PointerShape> = None;
    // The wait loop polls at 4 ms while the pane is active, then backs off after a
    // quiet spell. This preserves stream responsiveness while removing most idle
    // wakeups from an attached-but-quiescent shell.
    let mut last_activity = Instant::now();
    // The monitor snapshot version last applied to `state`. When unchanged, the
    // loop skips `monitor.snapshot()` entirely — avoiding the clone of every badge
    // set on each idle/live-frame pass.
    let mut seen_badge_version = u64::MAX;
    // When the attached-loop autostart pass last ran; `None` until the first pass,
    // which fires at once so a prompt already queued when this pane was attached is
    // picked up without waiting out the first [`AUTOSTART_TICK`].
    let mut last_autostart: Option<Instant> = None;
    let mut first = true;
    loop {
        let (height, width) = term.size();
        // The embedded terminal sits below the tab strip, so it uses the
        // tab-reserved geometry (matching what `render` lays out below). It also
        // tracks the sidebar state, so collapsing the sidebar (`Ctrl-B`) widens
        // the live terminal on the very next pass.
        let geo = ui::attached_geometry(height as usize, width as usize, state.sidebar());

        let now = Instant::now();
        // Pick up prompts queued for pane-less sessions while this pane is attached
        // (an MCP `session_delegate_issue` / `session_prompt` from the coordinator
        // agent running here) and start their agent panes in the background — the
        // job the outer event loop's `apply_autostart` does each tick, which does
        // not run while 没入 (Attached) owns the loop. Run *before* the pool borrow
        // below is taken: the previous iteration dropped its borrow at the loop's
        // end, so `autostart` (which spawns via `pool.borrow_mut()`) sees no live
        // borrow. A started pane logs a line and forces a repaint so its new sidebar
        // badge shows at once. Paced to [`AUTOSTART_TICK`] so a fast stream of output
        // frames does not run the gating directory listing on every pass.
        let autostart_started =
            if last_autostart.is_none_or(|t| now.duration_since(t) >= AUTOSTART_TICK) {
                last_autostart = Some(now);
                super::super::event::apply_autostart(state, autostart)
            } else {
                false
            };

        // Borrow the pool for this iteration's render / wait / input work, then let
        // it drop at the loop's end so the next pass's autostart runs unborrowed.
        // The active pane is fixed for the duration of this call (tab switches
        // return to the caller, which re-enters with the new pane), so re-resolving
        // it each pass yields the same shell.
        let mut pool_guard = pool.borrow_mut();
        let pty = match pool_guard.active_pty(dir) {
            Some(pty) => pty,
            // Every pane closed out from under the loop: fall to 集中.
            None => return Ok(PaneStep::Closed),
        };

        // Drop a leader that has waited past `PREFIX_TIMEOUT` for its action key,
        // so the footer hint clears and the next `Ctrl-O` starts a fresh sequence
        // rather than completing a stale one (`pump_input` makes the same check
        // exactly when a key arrives; this only catches a leader nobody followed).
        if !prefix_alive(pending_prefix, now) {
            pending_prefix = None;
        }

        // Interactive changes (input echo, resize, scroll, selection, hover,
        // badges) always repaint at once to stay responsive; fresh shell output
        // is tracked separately so a flood of it can be coalesced below. A pane
        // just autostarted in the background also repaints, so its sidebar badge
        // and command-log line show without waiting for the next real change.
        let mut interactive = first || autostart_started;
        // Surface the leader-pending state to the footer; repaint when it flips.
        let prefix_pending = pending_prefix.is_some();
        state.set_prefix_pending(prefix_pending);
        if last_prefix_pending != Some(prefix_pending) {
            interactive = true;
        }
        // Inform the PTY of a new size only when it actually changed; the old
        // loop took the parser lock (and issued a TIOCSWINSZ ioctl) every pass.
        if last_geo != Some(geo) {
            pty.resize(geo.rows, geo.cols);
            last_geo = Some(geo);
            interactive = true;
        }
        // Re-apply the scroll position only when the requested offset changed,
        // re-reading what the parser allows so an over-scroll past the oldest
        // line settles at the top.
        if applied_scrollback != Some(scrollback) {
            scrollback = pty.set_scrollback(scrollback);
            applied_scrollback = Some(scrollback);
            interactive = true;
        } else if scrollback > 0 {
            // No user scroll this pass, but output streaming in while scrolled
            // back advances the parser's offset on its own to keep the viewed
            // region pinned (the `scroll_up` patch in `third_party/vt100` bumps
            // the offset as lines enter the scrollback). Adopt that advance so
            // the next wheel / `Shift`+`PageUp` notch scrolls relative to where
            // the view actually sits. Without it the tracked offset goes stale
            // and a single notch snaps the view across every line that streamed
            // in — the scroll jumps and older/newer content briefly overlap.
            let actual = pty.scrollback();
            if actual != scrollback {
                scrollback = actual;
                applied_scrollback = Some(actual);
            }
        }
        // Fresh shell output (or the shell exiting) bumps the generation.
        let gen = pty.generation();
        let output_changed = gen != drawn_gen;
        if output_changed {
            last_activity = now;
        }
        // A change to the mouse selection — a new drag position, or clearing it —
        // must repaint so the inverted highlight tracks the pointer.
        if last_selection != selection {
            interactive = true;
        }
        // The pointer moved onto / off a different cell: repaint so the hovered
        // link's highlight follows it — but only when a link cell is actually
        // involved. A hover change over a screen with no link under either the old
        // or new pointer cell leaves the underline/highlight identical, so skipping
        // the full-grid re-stringify there keeps sweeping the pointer across plain
        // output (DECSET 1003 reports every crossed cell) from pinning the CPU.
        // `links_cache` holds the current screen's link cells, refreshed each paint.
        if last_hover != hover {
            let hovered_link = |c: &Option<Cell>| {
                c.as_ref().is_some_and(|cell| {
                    links_cache
                        .as_ref()
                        .is_some_and(|(_, set)| set.contains(cell))
                })
            };
            if hovered_link(&last_hover) || hovered_link(&hover) {
                interactive = true;
            }
        }
        // The pinned PR popup opened, closed, or moved to another session: repaint
        // so the floating box appears / clears / relocates over the live pane.
        if last_pr_popup != state.pr_popup() {
            interactive = true;
        }
        // The tab context menu / inline rename opened, closed, moved its cursor, or
        // took a keystroke: repaint so the overlay tracks it over the live pane.
        let menu_cursor = state.tab_menu().map(|m| m.cursor());
        let rename_sig = state
            .tab_rename()
            .map(|r| (r.value().to_string(), r.cursor()));
        if last_menu_cursor != menu_cursor || last_rename != rename_sig {
            interactive = true;
        }
        let session_skeleton = if state.pending_sessions().is_empty() {
            last_session_skeleton = None;
            None
        } else {
            let render_now = Utc::now();
            state.set_now(render_now);
            let frame = ui::skeleton_frame(render_now);
            if last_session_skeleton != Some(frame) {
                interactive = true;
            }
            Some(frame)
        };
        // While 没入 (Attached) owns the event loop, the outer home loop is not
        // running, so it cannot drain the terminal-pool watcher updates that
        // background panes harvest. Drain them here too and apply them to the
        // shared `HomeState`: a PR printed by another session should appear in the
        // left sidebar immediately, without first zooming out to 選択 (Switch).
        // Attached-pane PRs still update their own row below from the fresh
        // terminal snapshot; this drain handles the detached/background path.
        for (root, prs) in monitor.take_pr_link_updates() {
            if state.set_pr_links(&root, prs) {
                interactive = true;
            }
        }
        // While 没入 (Attached) owns the event loop, the outer home loop cannot
        // drain the state.json watcher that reflects session lifecycle changes
        // made out of process. MCP `session_delegate_issue` is commonly issued
        // by a coordinator agent running in this very attached pane; without
        // draining here, the delegated `issue-<n>` row does not appear in the
        // left sidebar until the user detaches back to 選択. Apply the same
        // refresh slot the outer loop drains so external `session_create` /
        // `session_delegate_issue` updates show immediately while attached too.
        for (root, sessions) in sessions_refresh.take_all() {
            state.refresh_sessions_for(&root, sessions);
            interactive = true;
        }
        // The sidebar's running / waiting / live-agent / finished markers, read
        // together under a single lock; repaint when they move so sessions
        // (including this one) keep their current state.
        let badge_version = monitor.badge_version();
        let badges = if badge_version != seen_badge_version {
            Some(monitor.snapshot())
        } else {
            None
        };
        if let Some(badges) = badges.as_ref() {
            if state.badges() != badges {
                interactive = true;
            } else {
                seen_badge_version = badge_version;
            }
        }

        // Coalesce pure-output frames: an output-only change repaints only once
        // [`MIN_FRAME`] has elapsed since the last paint, so a stream of 8 KiB
        // chunks cannot drive a full-grid redraw faster than the screen refreshes.
        // Anything interactive bypasses the throttle.
        let throttled = output_changed
            && !interactive
            && last_paint.is_some_and(|t| now.duration_since(t) < MIN_FRAME);

        if interactive || (output_changed && !throttled) {
            drawn_gen = gen;
            // Hold the parser lock just long enough to detect links (only when the
            // content changed) and snapshot the grid into an owned view.
            // Snapshot the grid (and, on a fresh-output frame, rescan its links)
            // under the parser lock, then release it. Any pull-request URLs to
            // persist are lifted out as owned data and written *after* the lock is
            // dropped: `pr_link_store::add`/`get` do synchronous disk IO (atomic
            // write + read-back), and doing that while holding the parser Mutex
            // would stall the reader thread — and, once the PTY buffer fills, the
            // shell itself — for the duration of the write.
            let (view, fresh_prs) = {
                let parser = pty.parser();
                let screen = parser.screen();
                let fresh_prs = if links_cache.as_ref().map(|(g, _)| *g) != Some(gen) {
                    // One whole-screen scan yields both the link cells (to
                    // underline) and the URL text — computing them together avoids
                    // a second full-grid walk under the parser lock.
                    let scan = link::scan_links(screen);
                    // Fresh output: harvest the pull-request URLs the agent may have
                    // printed so the attached session records them (sidebar
                    // `#<number>` badges, click-to-reopen). Return them to persist
                    // off the lock; only when the visible set actually changed.
                    let prs = link::pr_links_from(&scan.urls);
                    links_cache = Some((gen, scan.cells));
                    (!prs.is_empty() && prs != last_prs).then_some(prs)
                } else {
                    None
                };
                let links = &links_cache.as_ref().expect("links cache set above").1;
                let view =
                    TerminalView::from_screen_with_links(screen, selection.as_ref(), hover, links);
                (view, fresh_prs)
            };
            // Parser lock released. Persist any newly-seen PR URLs now, keyed by the
            // session root (the dir the agent runs in), matching what `sync` reads;
            // the store accumulates distinct URLs over time. Reflect the badge in the
            // sidebar *now* instead of waiting for the next (slow) workspace re-sync
            // to fold `pr-links/` into `state.json`: read back the store's
            // accumulated, deduped set (the same value the re-sync computes) and set
            // it on the in-memory row so the `#N` badge shows on this frame.
            if let Some(prs) = fresh_prs {
                if let Some(root) = state.list().active().map(|wt| wt.path.clone()) {
                    let _ = crate::infrastructure::pr_link_store::add(&root, &prs);
                    let merged = crate::infrastructure::pr_link_store::get(&root);
                    state.set_pr_links(&root, merged);
                }
                last_prs = prs;
            }
            // The cursor belongs to the live screen, so don't park it while the
            // user is viewing scrolled-back history. When live, park it on the
            // program's cursor cell even if the program hid it (so the IME's
            // preedit lands there) and mirror the program's show/hide. While the
            // tab context menu / rename overlay is up it draws its own caret and
            // sits over the pane, so the shell cursor is hidden — otherwise it
            // would blink through the box.
            let overlay_open = state.tab_menu().is_some() || state.tab_rename().is_some();
            let cursor = (scrollback == 0 && !overlay_open)
                .then(|| view.cursor())
                .flatten();
            let cursor_visible = view.cursor_visible() && !overlay_open;
            // Re-assert the shape only when it moved off what we last emitted, so
            // a stream of output frames doesn't keep re-poking the cursor. The
            // first paint (`last_shape == None`) always emits, claiming this
            // pane's shape from the previously active tab.
            let shape = pty.cursor_shape();
            let cursor_shape = (last_shape != Some(shape)).then_some(shape);
            state.surface_writer(SurfaceOwner::Attached).set_view(view);
            if let Some(badges) = badges {
                state.badge_writer(SurfaceOwner::Attached).apply(badges);
                seen_badge_version = badge_version;
            }
            render(
                term,
                state,
                CursorFrame {
                    pos: cursor,
                    visible: cursor_visible,
                    shape: cursor_shape,
                },
                geo,
                (height, width),
                &mut prev,
            )?;
            last_shape = Some(shape);
            last_selection = selection;
            last_hover = hover;
            last_pr_popup = state.pr_popup();
            last_menu_cursor = menu_cursor;
            last_rename = rename_sig;
            last_prefix_pending = Some(prefix_pending);
            last_session_skeleton = session_skeleton;
            last_paint = Some(now);
            first = false;
        }

        // The shell closed (e.g. the user typed `exit`): leave the pane.
        if !pty.is_alive() {
            return Ok(PaneStep::Closed);
        }

        // When throttled, ask `wait` to wake by the end of the current frame so
        // the deferred output lands as soon as the interval lets it.
        let redraw_deadline = if throttled {
            last_paint.map(|t| t + MIN_FRAME)
        } else if session_skeleton.is_some() {
            Some(now + SESSION_SKELETON_FRAME)
        } else {
            None
        };
        match wait(pty, drawn_gen, redraw_deadline, throttled, last_activity)? {
            // New output, or the idle re-evaluation tick (a possible resize /
            // badge change): loop and let the checks above decide whether to
            // repaint — an unchanged tick redraws nothing.
            Wake::Output => {}
            // Input is queued: forward every pending key (or scroll the
            // history), then loop and repaint.
            Wake::Input => {
                last_activity = Instant::now();
                if let Some(step) = pump_input(
                    term,
                    state,
                    pty,
                    geo,
                    (height, width),
                    &mut scrollback,
                    &mut selection,
                    &mut drag_tab,
                    &mut hover,
                    links_cache.as_ref().map(|(_, set)| set),
                    &mut pending_prefix,
                    &mut last_click,
                    &mut last_pointer,
                )? {
                    return Ok(step);
                }
            }
        }
    }
}

/// Why a [`wait`] ended: input is queued, or the shell produced output (or the
/// idle re-evaluation tick elapsed) and the loop should re-check for changes.
enum Wake {
    Input,
    Output,
}

/// Block until a key (or other input event) is queued, the shell's output moves
/// past `drawn_gen`, or the idle re-evaluation tick elapses — whichever comes
/// first. The tick only returns control to the loop so it can notice a resize or
/// a badge change; the loop repaints only when something actually moved.
///
/// When the caller throttled an output-only frame it passes a `redraw_deadline`
/// with `hold_output_until_deadline`: pending output is then held back (while
/// still answering input at once) until the deadline passes, so coalesced output
/// lands exactly at the frame boundary rather than immediately re-waking the
/// loop into a busy spin. A deadline without `hold_output_until_deadline` is a
/// pure animation wake (for the sidebar's session skeleton): fresh shell output
/// still wakes immediately, and the deadline wakes even when the shell stays
/// quiet.
fn wait(
    pty: &PtySession,
    drawn_gen: u64,
    redraw_deadline: Option<Instant>,
    hold_output_until_deadline: bool,
    last_activity: Instant,
) -> Result<Wake> {
    let start = Instant::now();
    loop {
        let now = Instant::now();
        // Fresh output (or the shell exiting, which also bumps the counter) wakes
        // the loop — but a throttled frame waits out its deadline first.
        if pty.generation() != drawn_gen
            && (!hold_output_until_deadline
                || redraw_deadline.is_none_or(|deadline| now >= deadline))
        {
            return Ok(Wake::Output);
        }
        if !hold_output_until_deadline && redraw_deadline.is_some_and(|deadline| now >= deadline) {
            return Ok(Wake::Output);
        }
        let mut timeout = if now.duration_since(last_activity) >= QUIET_AFTER {
            QUIET_POLL_SLICE
        } else {
            POLL_SLICE
        };
        if let Some(deadline) = redraw_deadline {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        } else {
            timeout = timeout.min(IDLE_REEVAL.saturating_sub(start.elapsed()));
        }
        if event::poll(timeout)? {
            return Ok(Wake::Input);
        }
        // The idle tick only bounds how stale a resize / badge change can get; a
        // throttled wait is already bounded by its (much shorter) deadline above.
        if redraw_deadline.is_none() && start.elapsed() >= IDLE_REEVAL {
            return Ok(Wake::Output);
        }
    }
}

/// Forward every queued key press to the shell, or — for the wheel and
/// `Shift`+`PageUp`/`PageDown` — scroll the pane's history via `scrollback`. A
/// left-button drag builds a text `selection` instead, and releasing it copies
/// the selected text to the clipboard (see [`copy_selection`]); a left click with
/// no drag opens the `#<number>` PR badge or terminal link under the pointer in the
/// default browser (see [`ui::sidebar_pr_links_at`] / [`open_clicked_url`]), or —
/// double-clicked on a sidebar session row — switches to that session
/// ([`PaneStep::ToSession`], tracked across calls via `last_click`), or opens
/// inline session creation when that row is `+ new session`; a left click on a
/// tab chip switches to that tab ([`PaneStep::ToTab`]; see
/// [`ui::attached_tab_at`]). A *right* click on a tab chip opens the tab context
/// menu ([`open_tab_menu_at`]) — the same `Menu` overlay 選択 / 集中 show, drawn
/// over the live pane by `render_frame` — and while it (or the inline rename it
/// spawns) is up the keyboard drives the overlay: `↑↓`/`j`/`k` move, `Enter` runs
/// the item, `Esc` (or a click) dismisses. `Move` / `Close` / a confirmed
/// `Rename` hand a step back ([`PaneStep::MenuMoveTab`] / [`PaneStep::MenuCloseTab`]
/// / [`PaneStep::MenuRenameTab`]) to mutate the pool and re-drive, staying in 没入.
/// Button-less motion updates
/// `hover` so the link under the pointer lights up. The navigation keys are
/// classified by the active `KeyScheme` (see
/// [`classify`](super::super::pane_input::classify)) — the prefix scheme reserves
/// only the `Ctrl-O` leader (tracked across calls via `pending_prefix`), the
/// `Alt` scheme a single `Alt`-chord each — and resolve to the steps the
/// pool-driven loop acts on: detach to 選択 ([`PaneStep::Detach`]), next / previous
/// tab in place ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]), zoom out to 集中
/// ([`PaneStep::ToCloseup`]), add an agent tab ([`PaneStep::NewAgentTab`]), open the
/// note editor ([`PaneStep::OpenNote`]), jump to the previous session
/// ([`PaneStep::PrevSession`]), switch to a sidebar-clicked session, or open
/// creation from the sidebar create row ([`PaneStep::ToSession`]); toggling the
/// sidebar stays in 没入. `Esc` and
/// `Ctrl-W` always reach the shell; tabs are closed from 選択 (`x`). Other events
/// are ignored so the next redraw picks up any new size.
/// Open the tab context menu for the chip under a pointer event at the 0-based
/// screen (`col`, `row`) while 没入, if the event lands on one. Returns `Some(())`
/// when a chip was hit (the menu opened at that spot), else `None`. The menu is
/// the same `TabMenu` overlay 選択 / 集中 raise; `render_frame` draws it over the
/// live pane. Mirrors the home event loop's right-click handling
/// ([`ui::attached_tab_hit`] is the shared hit test).
fn open_tab_menu_at(
    state: &mut HomeState,
    col: u16,
    row: u16,
    geo: ui::TerminalGeometry,
) -> Option<()> {
    let tab = ui::attached_tab_hit(state, col, row, geo)?;
    // The attached session's own dir / tab label: the menu stores them for the
    // home path, but the 没入 path acts on the tab index alone (the caller already
    // holds the driving dir), so a best-effort identity here is harmless.
    let dir = state.list().active().map(|wt| wt.path.clone())?;
    let label = state.terminal_tabs()?.labels.get(tab)?.clone();
    state.open_tab_menu(dir, tab, label, col, row);
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn pump_input(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    geo: ui::TerminalGeometry,
    size: (u16, u16),
    scrollback: &mut usize,
    selection: &mut Option<Selection>,
    drag_tab: &mut Option<usize>,
    hover: &mut Option<Cell>,
    links: Option<&std::collections::HashSet<Cell>>,
    pending_prefix: &mut Option<Instant>,
    last_click: &mut Option<(usize, Instant)>,
    last_pointer: &mut Option<PointerShape>,
) -> Result<Option<PaneStep>> {
    let now = Instant::now();
    let mut pending_bytes = Vec::new();
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) => {
                if !is_press(key) {
                    continue;
                }
                // The tab context menu / inline rename (opened by a right-click on
                // a tab chip) owns the keyboard while it is up: its keys drive the
                // overlay instead of reaching the shell. `Move` / `Close` / a
                // confirmed `Rename` hand a step back to the caller to mutate the
                // pool and re-drive, staying in 没入. `render_frame` draws the box;
                // this only routes the keys (classified purely in `pane_input`).
                if state.tab_rename().is_some() {
                    match classify_rename_key(&key) {
                        RenameVerdict::Confirm => {
                            if let Some((_, tab, label)) = state.confirm_tab_rename() {
                                return Ok(Some(PaneStep::MenuRenameTab { tab, label }));
                            }
                        }
                        RenameVerdict::Cancel => state.cancel_tab_rename(),
                        RenameVerdict::Edit(edit) => {
                            if let Some(input) = state.tab_rename_mut() {
                                match edit {
                                    RenameEdit::Backspace => input.backspace(),
                                    RenameEdit::DeleteForward => input.delete_forward(),
                                    RenameEdit::Left => input.move_left(),
                                    RenameEdit::Right => input.move_right(),
                                    RenameEdit::Home => input.move_home(),
                                    RenameEdit::End => input.move_end(),
                                    RenameEdit::Insert(c) => input.push_char(c),
                                }
                            }
                        }
                        RenameVerdict::Ignore => {}
                    }
                    continue;
                }
                if state.tab_menu().is_some() {
                    match classify_menu_key(&key) {
                        MenuVerdict::Up => {
                            if let Some(menu) = state.tab_menu_mut() {
                                menu.move_up();
                            }
                        }
                        MenuVerdict::Down => {
                            if let Some(menu) = state.tab_menu_mut() {
                                menu.move_down();
                            }
                        }
                        MenuVerdict::Cancel => state.close_tab_menu(),
                        MenuVerdict::Confirm => {
                            let (tab, item) = state
                                .tab_menu()
                                .map(|m| (m.tab(), m.item()))
                                .expect("tab menu open while confirming");
                            match item {
                                TabMenuItem::MoveLeft => {
                                    state.close_tab_menu();
                                    return Ok(Some(PaneStep::MenuMoveTab {
                                        tab,
                                        swap: TabSwap::Left,
                                    }));
                                }
                                TabMenuItem::MoveRight => {
                                    state.close_tab_menu();
                                    return Ok(Some(PaneStep::MenuMoveTab {
                                        tab,
                                        swap: TabSwap::Right,
                                    }));
                                }
                                TabMenuItem::Rename => {
                                    state.begin_tab_rename_from_menu();
                                }
                                TabMenuItem::Close => {
                                    state.close_tab_menu();
                                    return Ok(Some(PaneStep::MenuCloseTab { tab }));
                                }
                            }
                        }
                        MenuVerdict::Ignore => {}
                    }
                    continue;
                }
                *drag_tab = None;
                // A keypress dismisses a pinned PR popup (so `Esc` — or typing —
                // closes it), the same as on the home screen; the keystroke still
                // drives the shell below. The drive loop repaints on the change.
                state.set_pr_popup(None);
                // Whether a leader is genuinely still pending: set, and pressed
                // within `PREFIX_TIMEOUT`. A lapsed leader counts as none, so this
                // key starts a fresh sequence instead of completing a stale one.
                let pending = prefix_alive(*pending_prefix, now);
                // Scroll keys move the history view in place rather than going to
                // the shell; the view shifts under any selection, so drop it. Only
                // when no prefix press is pending — mid-prefix the next key is the
                // action key, classified below.
                if !pending {
                    if let Some(delta) = key_scroll_lines(&key, geo) {
                        *selection = None;
                        apply_scroll(scrollback, delta);
                        continue;
                    }
                }
                // Which keys the pane claims for navigation (vs. forwards to the
                // shell) depends on the configured `KeyScheme`; `classify` is the
                // single source of truth and `pending` carries the one-bit state a
                // `Ctrl-O` prefix sequence needs (prefix scheme only).
                match classify(state.key_scheme(), pending, &key) {
                    // The `Ctrl-O` leader was pressed: wait for the action key
                    // (stamping when, so the wait can lapse), swallowing the leader.
                    KeyAction::BeginPrefix => *pending_prefix = Some(now),
                    // An unrecognised key right after the leader: drop it.
                    KeyAction::Swallow => *pending_prefix = None,
                    // `Ctrl-O s` (prefix scheme) / `Alt-s` (alt scheme) collapses
                    // or expands the sidebar in place, without leaving 没入: the
                    // next loop pass re-lays out the frame and resizes the PTY to
                    // the new pane width. Bare `Ctrl-B` never reaches here —
                    // `classify` only maps the leader / `Alt` chords, so it is
                    // forwarded to the shell; `Ctrl-B` toggles the sidebar on
                    // usagi's own surfaces (選択 / 集中), not in 没入.
                    KeyAction::Reserved(Reserved::ToggleSidebar) => {
                        *pending_prefix = None;
                        state.toggle_sidebar();
                    }
                    // Every other navigation action hands a step back to the
                    // pool-driven loop (some leave 没入, some stay and re-drive in
                    // place); typing first snaps back to the live screen.
                    KeyAction::Reserved(action) => {
                        *pending_prefix = None;
                        *scrollback = 0;
                        *selection = None;
                        flush_pending_input(pty, &mut pending_bytes)?;
                        return Ok(Some(match action {
                            Reserved::Detach => PaneStep::Detach,
                            Reserved::ToFocus => PaneStep::ToCloseup,
                            Reserved::NextTab => PaneStep::NextTab,
                            Reserved::PrevTab => PaneStep::PrevTab,
                            Reserved::SwapTabRight => PaneStep::SwapTabRight,
                            Reserved::SwapTabLeft => PaneStep::SwapTabLeft,
                            Reserved::NewAgentTab => PaneStep::NewAgentTab,
                            Reserved::CloseTab => PaneStep::CloseTab,
                            Reserved::OpenNote => PaneStep::OpenNote,
                            Reserved::PrevSession => PaneStep::PrevSession,
                            Reserved::Quit => PaneStep::Quit,
                            Reserved::ToggleSidebar => unreachable!("handled above"),
                        }));
                    }
                    // The key belongs to the shell. With text selected, `Ctrl-C`
                    // copies it (and clears the selection) the way terminals treat
                    // copy while a selection is active; otherwise it reaches the
                    // shell as the usual interrupt.
                    KeyAction::Forward => {
                        *pending_prefix = None;
                        if is_copy(&key) && selection.as_ref().is_some_and(|s| !s.is_empty()) {
                            copy_selection(term, pty, selection.as_ref())?;
                            *selection = None;
                            continue;
                        }
                        let bytes = encode_key(&key);
                        if !bytes.is_empty() {
                            // Typing returns to the live screen and ends any
                            // selection, like a real terminal.
                            *scrollback = 0;
                            *selection = None;
                            pending_bytes.extend_from_slice(&bytes);
                        }
                    }
                }
            }
            // A bracketed paste arrives as one block: forward it whole, so an
            // agent that supports bracketed paste inserts the multi-line text
            // instead of submitting on each embedded newline.
            Event::Paste(text) => {
                // Pasting returns to the live screen, ends any selection, and
                // abandons a pending leader (the paste is the user's next intent,
                // not the leader's action key).
                *pending_prefix = None;
                *scrollback = 0;
                *selection = None;
                pending_bytes.extend_from_slice(&encode_paste(&text, pty.bracketed_paste()));
            }
            // Any mouse activity abandons a pending leader: the user reached for
            // the pointer instead of completing the chord, so a later `Ctrl-O`
            // must start fresh rather than complete this one.
            Event::Mouse(mouse) => {
                *pending_prefix = None;
                // Reshape the host pointer for the cell under it (text caret over
                // the selectable grid, hand over a clickable target), emitting OSC
                // 22 only on a change.
                update_pointer(
                    term,
                    state,
                    links,
                    geo,
                    size,
                    mouse.column,
                    mouse.row,
                    last_pointer,
                )?;
                // While the tab context menu / rename overlay is up it owns pointer
                // input: a right-click on another chip relocates the menu, and any
                // other press dismisses the overlay — nothing reaches the shell
                // selection / tab logic below. The keyboard drives the rest.
                if state.tab_menu().is_some() || state.tab_rename().is_some() {
                    if let MouseEventKind::Down(button) = mouse.kind {
                        let relocated = button == MouseButton::Right
                            && state.tab_rename().is_none()
                            && open_tab_menu_at(state, mouse.column, mouse.row, geo).is_some();
                        if !relocated {
                            state.close_tab_menu();
                            state.cancel_tab_rename();
                        }
                    }
                    continue;
                }
                match mouse.kind {
                    // A left press on a tab chip arms a tab click/drag: releasing on
                    // the same chip switches to it, while releasing on another chip
                    // reorders the source tab to that slot. Pressing outside the tab
                    // strip starts the normal terminal text selection.
                    MouseEventKind::Down(MouseButton::Left) => {
                        // A pinned PR popup owns the whole click: swallow the press
                        // (no tab switch, no selection) so its button release resolves
                        // the popup cleanly.
                        if state.pr_popup().is_none() {
                            if let Some(tab) =
                                ui::attached_tab_hit(state, mouse.column, mouse.row, geo)
                            {
                                *drag_tab = Some(tab);
                                *selection = None;
                            } else {
                                *drag_tab = None;
                                *selection =
                                    pane_cell(mouse.column, mouse.row, geo).map(Selection::new);
                            }
                        }
                    }
                    // A right press on a tab chip opens the tab context menu over
                    // the live pane — the same overlay 選択 / 集中 show — so tabs are
                    // reorderable / renamable / closable without leaving 没入. Off a
                    // chip it does nothing (there is no menu open to dismiss here).
                    MouseEventKind::Down(MouseButton::Right) => {
                        open_tab_menu_at(state, mouse.column, mouse.row, geo);
                    }
                    // Dragging the left button stretches the terminal selection
                    // unless a tab drag is armed.
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if drag_tab.is_none() {
                            if let (Some(sel), Some(cell)) =
                                (selection.as_mut(), pane_cell(mouse.column, mouse.row, geo))
                            {
                                sel.extend(cell);
                            }
                        }
                    }
                    // Releasing after a drag copies the selection; a plain click
                    // (no drag) drives the pinned PR popup if one is open, else opens
                    // a link under it in the default browser — a `#<number>` PR badge
                    // on a sidebar session row pins its popup, or a URL in the
                    // terminal opens.
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(from) = drag_tab.take() {
                            if let Some(to) =
                                ui::attached_tab_hit(state, mouse.column, mouse.row, geo)
                            {
                                *scrollback = 0;
                                *selection = None;
                                flush_pending_input(pty, &mut pending_bytes)?;
                                if from == to
                                    && ui::attached_tab_at(state, mouse.column, mouse.row, geo)
                                        .is_none()
                                {
                                    continue;
                                }
                                return Ok(Some(if from == to {
                                    PaneStep::ToTab(to)
                                } else {
                                    PaneStep::MoveTab { from, to }
                                }));
                            }
                        }
                        let dragged = selection.as_ref().is_some_and(|s| !s.is_empty());
                        let (h, w) = term.size();
                        // A pinned PR popup owns a plain click: a `#<number>` opens
                        // that PR, a click elsewhere in the box keeps it pinned, and a
                        // click outside dismisses it.
                        if !dragged && state.pr_popup().is_some() {
                            match ui::pr_popup_click(
                                state,
                                h as usize,
                                w as usize,
                                mouse.column,
                                mouse.row,
                            ) {
                                ui::PopupClick::Open(url) => open_url(&url),
                                ui::PopupClick::Inside => {}
                                ui::PopupClick::Outside => {
                                    state.set_pr_popup(None);
                                }
                            }
                            *selection = None;
                            continue;
                        }
                        // No popup open: a plain click on a session's PR badge pins
                        // its popup (so the pointer can travel in to click a number).
                        let badge = (!dragged)
                            .then(|| {
                                ui::sidebar_pr_badge_at(
                                    state,
                                    h as usize,
                                    w as usize,
                                    mouse.column,
                                    mouse.row,
                                )
                            })
                            .flatten();
                        // Otherwise a plain click on a sidebar row arms a double
                        // click; a second click on the same row within
                        // `DOUBLE_CLICK` switches to that session (attaching when
                        // live), or opens inline creation when the row is the
                        // persistent `+ new session` affordance — the same
                        // confirm 選択 / 集中 expose without first zooming out. A
                        // single click only arms.
                        let session = (badge.is_none() && !dragged)
                            .then(|| {
                                ui::left_pane_session_at(
                                    state,
                                    mouse.column,
                                    mouse.row,
                                    h as usize,
                                    w as usize,
                                )
                            })
                            .flatten();
                        if let Some(idx) = badge {
                            state.set_pr_popup(Some(idx));
                            *selection = None;
                        } else if let Some(row) = session {
                            *selection = None;
                            if is_double_click(last_click, row, now, DOUBLE_CLICK) {
                                flush_pending_input(pty, &mut pending_bytes)?;
                                return Ok(Some(PaneStep::ToSession(row)));
                            }
                        } else if open_clicked_url(
                            pty,
                            geo,
                            mouse.column,
                            mouse.row,
                            selection.as_ref(),
                        ) {
                            *selection = None;
                        } else {
                            copy_selection(term, pty, selection.as_ref())?;
                        }
                    }
                    // Button-less motion: track the cell under the pointer so the link
                    // there (if any) lights up. `pane_cell` yields `None` past the
                    // pane edges, clearing the highlight as the pointer leaves.
                    MouseEventKind::Moved => {
                        *hover = pane_cell(mouse.column, mouse.row, geo);
                    }
                    // The wheel acts only when the pointer is over the terminal pane;
                    // hit-test both axes through `pane_cell` (the same test the click
                    // and hover arms use) — a column-only check let the wheel act while
                    // the pointer was above the pane (the tab row) or below its last
                    // line. What it does depends on what the running program asked for:
                    //
                    // - The program **tracks the mouse** (DECSET 1000/1002/1003): forward
                    //   the wheel as a mouse report so it scrolls its own viewport — what
                    //   the wheel does over such a program in a standalone terminal. Some
                    //   full-screen agents (`claude`) draw into the *primary* buffer and
                    //   only this signal catches them; the alternate-screen test below
                    //   misses them, so the wheel fell through to a usagi history scroll
                    //   and surfaced old commands instead of scrolling the agent.
                    // - Else on the **alternate** screen (a pager / TUI with no mouse
                    //   tracking, e.g. `less`), `vt100` keeps no scrollback, so a history
                    //   scroll is a dead no-op. Emulate the terminal's alternate-scroll
                    //   mode: forward the wheel as arrow-key presses so the program
                    //   scrolls its own viewport.
                    // - Else (a plain shell on the **primary** screen) scroll usagi's own
                    //   history view; the view shifts, so any selection is dropped.
                    //
                    // In the first two cases the selection is left alone — nothing
                    // scrolled in usagi.
                    kind => {
                        if let Some(delta) = wheel_delta(kind) {
                            if let Some(cell) = pane_cell(mouse.column, mouse.row, geo) {
                                // Read the grid + input modes under the parser lock,
                                // dropping the guard before the `&mut self` write below.
                                let forward = {
                                    let parser = pty.parser();
                                    let screen = parser.screen();
                                    if screen.mouse_protocol_mode()
                                        != vt100::MouseProtocolMode::None
                                    {
                                        Some(encode_mouse_wheel(
                                            delta < 0,
                                            cell,
                                            screen.mouse_protocol_encoding(),
                                        ))
                                    } else if screen.alternate_screen() {
                                        Some(
                                            wheel_arrows(delta, screen.application_cursor())
                                                .into_bytes(),
                                        )
                                    } else {
                                        None
                                    }
                                };
                                match forward {
                                    Some(seq) => pending_bytes.extend_from_slice(&seq),
                                    None => {
                                        *selection = None;
                                        apply_scroll(scrollback, delta);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush_pending_input(pty, &mut pending_bytes)?;
    Ok(None)
}

/// Flush the bytes forwarded during a single input-drain pass in one PTY write.
/// A fast key repeat can queue several `Event::Key`s before the next repaint; the
/// old loop wrote and flushed once per key, while batching keeps the shell input
/// equivalent and cuts the syscall count to one write+flush per drain.
fn flush_pending_input(pty: &mut PtySession, pending: &mut Vec<u8>) -> Result<()> {
    if !pending.is_empty() {
        pty.write(pending)?;
        pending.clear();
    }
    Ok(())
}

/// Give the host terminal's mouse pointer the shape that fits the cell under it,
/// writing the OSC 22 escape only when it changes from `last` so a stream of
/// motion reports does not re-emit it every cell. A URL in the grid or a clickable
/// chrome element (a tab chip, a sidebar PR badge) shows a hand, the selectable
/// terminal grid a text caret, and everything else the terminal's default pointer
/// (see [`pointer_shape`]). Terminals without OSC 22 ignore the escape.
#[allow(clippy::too_many_arguments)]
fn update_pointer(
    term: &Term,
    state: &HomeState,
    links: Option<&std::collections::HashSet<Cell>>,
    geo: ui::TerminalGeometry,
    size: (u16, u16),
    col: u16,
    row: u16,
    last: &mut Option<PointerShape>,
) -> Result<()> {
    let cell = pane_cell(col, row, geo);
    let clickable = match cell {
        // Inside the grid only a URL cell is clickable. This runs on *every* mouse
        // report (motion, drag, wheel), so it consults the link cells already
        // scanned for the current frame rather than re-taking the parser lock and
        // re-flattening the logical line under the pointer each event. The set is
        // exactly the cells [`link::url_at`] would lift a URL from (both derive from
        // [`link::scan_links`]'s `url_spans`), so the click handler still resolves
        // the URL with `url_at`.
        Some(cell) => links.is_some_and(|set| set.contains(&cell)),
        // Off the grid, a tab chip, a sidebar PR badge, or a `#<number>` in the
        // pinned PR popup is the clickable target.
        None => {
            let (height, width) = size;
            ui::attached_tab_hit(state, col, row, geo).is_some()
                || ui::sidebar_pr_badge_at(state, height as usize, width as usize, col, row)
                    .is_some()
                || matches!(
                    ui::pr_popup_click(state, height as usize, width as usize, col, row),
                    ui::PopupClick::Open(_)
                )
        }
    };
    let shape = pointer_shape(cell.is_some(), clickable);
    if *last != Some(shape) {
        term.write_str(shape.osc22())?;
        term.flush()?;
        *last = Some(shape);
    }
    Ok(())
}

/// Copy `selection`'s text to the user's clipboard by two routes (see
/// [`clipboard`]): the local system clipboard tool (`pbcopy` etc.), which is the
/// only one that works on terminals ignoring OSC 52 such as Apple Terminal.app,
/// and an OSC 52 escape written to `term`, which reaches the user's machine over
/// SSH. A click without a drag selects nothing, so there is nothing to copy.
fn copy_selection(term: &Term, pty: &PtySession, selection: Option<&Selection>) -> Result<()> {
    let Some(sel) = selection.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let text = sel.extract_text(pty.parser().screen());
    copy_to_system_clipboard(&text);
    let seq = clipboard::osc52_copy(&text);
    if !seq.is_empty() {
        term.write_str(&seq)?;
    }
    Ok(())
}

/// Pipe `text` to the first platform clipboard command that runs (see
/// [`clipboard::system_copy_commands`]). Best-effort: a missing tool or a write
/// failure is ignored, since the OSC 52 escape still covers terminals that
/// honour it. stdin is closed (by dropping it) before `wait` so the tool sees
/// EOF and flushes.
fn copy_to_system_clipboard(text: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    for argv in clipboard::system_copy_commands() {
        let Some((cmd, rest)) = argv.split_first() else {
            continue;
        };
        let Ok(mut child) = Command::new(cmd)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return;
        }
    }
}

/// On a plain click (a press/release with no intervening drag, so `selection` is
/// still a single empty cell) that lands on a link, open it in the default
/// browser and report `true`; otherwise report `false` so the caller copies any
/// real drag selection. The release coordinates locate the clicked cell, which
/// must match the empty selection's anchor — distinguishing a click from a drag.
fn open_clicked_url(
    pty: &PtySession,
    geo: ui::TerminalGeometry,
    col: u16,
    row: u16,
    selection: Option<&Selection>,
) -> bool {
    // A drag built a non-empty selection: this is a copy, not a click.
    if !selection.is_some_and(Selection::is_empty) {
        return false;
    }
    let Some(cell) = pane_cell(col, row, geo) else {
        return false;
    };
    if let Some(url) = link::url_at(pty.parser().screen(), cell) {
        open_url(&url);
        return true;
    }
    false
}

/// Hand `url` to the platform's default browser (see
/// [`link::open_command`]). Best-effort and detached: stdio is closed
/// and a missing opener or spawn failure is ignored, since failing to launch a
/// browser must not disturb the embedded shell.
fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let argv = link::open_command(url);
    let Some((cmd, rest)) = argv.split_first() else {
        return;
    };
    let _ = Command::new(cmd)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Draw the workspace frame (sidebar + terminal pane), repainting only the rows
/// that changed since `prev` and batching them into a single write — so an
/// update lands in one pass without the flicker of clearing the whole screen.
/// Finally, park the real cursor over the shell's cursor cell so it tracks the
/// embedded terminal.
/// What the embedded pane should do with the host terminal's real cursor this
/// frame: where to park it, whether to show it, and the shape to assert.
struct CursorFrame {
    /// The cell to park the real cursor on (so an OS IME's preedit lands there),
    /// or `None` while the user is in the scrollback and the cursor is detached.
    pos: Option<(u16, u16)>,
    /// Whether to show the hardware cursor, mirroring the program's own show/hide.
    visible: bool,
    /// The cursor shape (DECSCUSR `Ps`) to re-assert, or `None` when it has not
    /// changed since the last paint so an idle pane never re-pokes the cursor.
    shape: Option<u16>,
}

fn render(
    term: &Term,
    state: &HomeState,
    cursor: CursorFrame,
    geo: ui::TerminalGeometry,
    size: (u16, u16),
    prev: &mut Vec<String>,
) -> Result<()> {
    // The terminal size is read once at the top of the `drive` loop and threaded
    // in, not re-read here: it cannot change between two synchronous calls in the
    // same loop pass, so a second `term.size()` (a TIOCGWINSZ ioctl) would be a
    // redundant syscall on every painted frame.
    let (height, width) = size;
    let frame = ui::render_frame(height as usize, width as usize, state);

    // Repaint only the changed frame segments; the cursor is hidden for the
    // repaint and re-positioned below over the shell's cell.
    let mut buf = diff_frame_with_columns(
        prev,
        &frame,
        Some(ui::column_diff(
            height as usize,
            width as usize,
            state.sidebar(),
        )),
    );

    // Re-assert the active pane's cursor shape (DECSCUSR `CSI Ps SP q`) when it
    // changed — `vt100` swallowed the program's own sequence, so without this the
    // host terminal would keep whatever shape the previously active tab left it.
    // The caller only passes `Some` on a change, so an idle pane never re-emits.
    if let Some(shape) = cursor.shape {
        let _ = write!(buf, "\x1b[{shape} q");
    }

    if let Some((row, col)) = cursor.pos {
        // Translate the pane-relative cursor to a 1-based screen position
        // (clamping a deferred-wrap column back onto the pane) and park the real
        // cursor there, so an OS IME draws its preedit on the program's cursor.
        // Mirror the program's show/hide: re-show it (the repaint hid it) when the
        // program shows it, else leave it hidden but still positioned — exactly as
        // when the program runs standalone, where the IME still follows a hidden
        // cursor's position. `\x1b[?25l` is repeated harmlessly so the intent is
        // explicit regardless of what the repaint emitted.
        let (x, y) = geo.cursor_screen_pos(row, col);
        let show = if cursor.visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        };
        let _ = write!(buf, "\x1b[{y};{x}H{show}");
    }

    term.write_str(&buf)?;
    term.flush()?;
    *prev = frame;
    Ok(())
}
