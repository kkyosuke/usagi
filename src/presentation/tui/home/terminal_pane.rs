//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal` / `agent`, the right pane switches to a live
//! shell (没入) drawn while the whole workspace frame — sidebar and all — stays
//! on screen. The shell itself is owned by the [`TerminalPool`] (so it survives
//! leaving the pane); this module borrows it and runs the render/input loop.
//! Keystrokes are forwarded to the shell as raw bytes.
//!
//! The **reserved keys** are `Ctrl-O`, `Ctrl-N`/`Ctrl-P`, `Ctrl-T`/`Ctrl-G`,
//! `Ctrl-^`, `Ctrl-B`, and `Ctrl-Q`: everything else, including `Esc` **and
//! `Ctrl-W`** (the universal shell "delete previous word" — closing a tab is done
//! from 切替 instead), flows to the shell.
//! `Ctrl-B` collapses / expands the left sidebar in place (it never leaves 没入).
//! A single
//! `Ctrl-O` zooms out one engagement level by returning [`PaneStep::Detach`]
//! immediately, leaving the pane for 切替 (Switch) on the left pane while every
//! pane stays alive in the pool — there the user moves between sessions
//! (`↑`/`↓`), between this session's tabs (`←`/`→`), re-attaches (`Enter`), adds a
//! pane (`t`), or summons the `:` command palette. `Ctrl-N`/`Ctrl-P` switch to
//! the next / previous tab in place ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]),
//! and a left click on a tab chip jumps straight to it ([`PaneStep::ToTab`]);
//! `Ctrl-T` zooms out to 在席 (Focus) — the session's action menu — by returning
//! [`PaneStep::ToFocus`] (every pane stays alive in the pool); `Ctrl-G` adds an
//! agent tab ([`PaneStep::NewAgentTab`]) without leaving 没入. `Ctrl-^` jumps to the previously
//! focused session ([`PaneStep::PrevSession`]). `Ctrl-Q` leaves 没入 to quit usagi
//! ([`PaneStep::Quit`]), raising the quit-confirmation modal on the home screen.
//! The shell exiting on its own reports [`PaneStep::Closed`].
//!
//! `agent` reuses the same machinery: the pool sends the configured agent CLI to
//! the shell on first spawn, so the pane lands the user straight in the agent.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the input translation — which chord a key is, how far to
//! scroll, which cell the pointer hit, and the bytes a key/paste becomes
//! ([`super::pane_input`]); the layout geometry and frame ([`super::ui`]); the
//! screen snapshot ([`super::terminal_view`]); and the [`PaneExit`] vocabulary
//! ([`super::state`]).
//!
//! [`TerminalPool`]: super::terminal_pool::TerminalPool

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use console::Term;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;
use crate::presentation::tui::clipboard;
use crate::presentation::tui::screen::diff_frame;

use super::pane_input::{
    apply_scroll, encode_key, encode_paste, is_copy, is_leader, is_new_agent_tab, is_next_tab,
    is_open_note, is_press, is_prev_session, is_prev_tab, is_quit, is_to_focus, is_toggle_sidebar,
    key_scroll_lines, pane_cell, wheel_arrows, wheel_delta,
};
use super::state::HomeState;
use super::terminal_link;
use super::terminal_pool::MonitorHandle;
use super::terminal_selection::{Cell, Selection};
use super::terminal_view::TerminalView;
use super::ui;

/// Why the embedded terminal loop handed control back, so the pool-driven loop
/// in [`super::run`](super) can act on it: the user zoomed out (to 切替 or 在席),
/// switched tabs, added / closed a tab, or the shell closed. Tab switching and
/// agent-tab / close management are handled in place without leaving 没入 — the
/// same actions are also reachable from 切替 (Switch) via `Detach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStep {
    /// `Ctrl-O`: zoom out one level (→ 切替), leaving every pane alive in the pool.
    Detach,
    /// `Ctrl-E`: leave the pane to open the session-note editor over it. The
    /// caller re-attaches the pane once the editor closes.
    OpenNote,
    /// `Ctrl-N`: switch to the next tab without leaving 没入. The caller advances
    /// the pool's active pane and re-drives it.
    NextTab,
    /// `Ctrl-P`: switch to the previous tab without leaving 没入.
    PrevTab,
    /// A left click on a tab chip: switch to that (0-based) tab without leaving
    /// 没入. Like [`NextTab`](Self::NextTab) / [`PrevTab`](Self::PrevTab), the
    /// caller makes it active and re-drives the pane.
    ToTab(usize),
    /// `Ctrl-T`: zoom out to 在席 (Focus) — the session's action menu — leaving
    /// every pane alive in the pool. Adding a terminal is then a menu choice.
    ToFocus,
    /// `Ctrl-G`: add a new agent tab and make it active, without leaving 没入.
    NewAgentTab,
    /// `Ctrl-^`: leave 没入 to jump to the previously focused session (vim's
    /// `Ctrl-^` / tmux's `last-window`), attaching it when live. The caller
    /// re-roots the pane on that session.
    PrevSession,
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

/// The longest the loop sits idle before *re-evaluating* — re-reading the
/// terminal size and the sidebar badges to decide whether anything changed.
/// Output and key presses wake it far sooner; this only bounds how stale a
/// background resize or badge change can get while nothing else happens. It no
/// longer forces a repaint (the loop repaints only on a real change), so it is
/// paced to the watcher's own poll interval rather than a tight redraw timer.
const IDLE_REEVAL: Duration = Duration::from_millis(200);

/// The shortest gap between two repaints driven purely by fresh shell output.
/// The reader thread bumps the generation once per 8 KiB chunk — roughly every
/// 4 ms while an agent streams — and each repaint locks the parser and
/// re-stringifies the whole grid. Coalescing output-only frames to at most one
/// per ~60 fps keeps a flood of output from pinning the CPU on redraws the eye
/// cannot see, while interactive changes (input echo, resize, scroll, selection,
/// hover, badges) still repaint immediately so the pane stays responsive.
const MIN_FRAME: Duration = Duration::from_millis(16);

/// Report mouse motion with no button held (DECSET 1003), so the pane can light
/// up the link under the pointer on hover. The global mouse modes (1000/1002/1006,
/// see [`super::super::screen`]) only report clicks and drags; this is enabled
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
/// [`TerminalPool`]: super::terminal_pool::TerminalPool
pub fn run(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    monitor: &MonitorHandle,
) -> Result<PaneStep> {
    // Raw mode, bracketed paste, and motion reporting are entered here and
    // restored by the guard's `Drop` — including when `drive` panics and unwinds,
    // not only on the normal return path. Restoring them on unwind matters: the
    // alternate-screen guard one frame up resets the alt screen and click/drag
    // mouse modes, but it does not own these, so without this guard a panic in the
    // render/input loop would leave the user's shell in raw mode with bracketed
    // paste and motion reporting still on.
    let _modes = PaneModeGuard::enter(term)?;
    let _ = term.clear_screen();
    drive(term, state, pty, monitor)
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
fn drive(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    monitor: &MonitorHandle,
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
    // What we last told the PTY and last drew, so a pass that finds them
    // unchanged skips the resize ioctl, the grid snapshot, and the repaint. The
    // sentinels (a `None` geometry / scrollback / selection, a first-pass flag)
    // force the opening pass to draw.
    let mut last_geo: Option<ui::TerminalGeometry> = None;
    let mut applied_scrollback: Option<usize> = None;
    let mut last_selection: Option<Selection> = None;
    let mut last_hover: Option<Cell> = None;
    let mut drawn_gen = pty.generation();
    // When the last repaint landed, so a flood of output-only frames coalesces to
    // at most one per [`MIN_FRAME`]; `None` until the first paint, which never
    // throttles.
    let mut last_paint: Option<Instant> = None;
    // The screen's URL cells cached against the generation they were detected at,
    // so hover-only / throttled frames skip the O(all cells) re-scan and reuse
    // them until the shell's output actually changes (see [`terminal_link`]).
    let mut links_cache: Option<(u64, std::collections::HashSet<Cell>)> = None;
    // The cursor shape (DECSCUSR `Ps`) last emitted to the host terminal, so a
    // shape is re-asserted only when the program changes it. `None` until the
    // first paint, which always emits — restoring this pane's shape over whatever
    // the previously active tab left on the terminal.
    let mut last_shape: Option<u16> = None;
    let mut first = true;
    loop {
        let (height, width) = term.size();
        // The embedded terminal sits below the tab strip, so it uses the
        // tab-reserved geometry (matching what `render` lays out below). It also
        // tracks the sidebar state, so collapsing the sidebar (`Ctrl-B`) widens
        // the live terminal on the very next pass.
        let geo = ui::attached_geometry(height as usize, width as usize, state.sidebar());

        // Interactive changes (input echo, resize, scroll, selection, hover,
        // badges) always repaint at once to stay responsive; fresh shell output
        // is tracked separately so a flood of it can be coalesced below.
        let mut interactive = first;
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
        }
        // Fresh shell output (or the shell exiting) bumps the generation.
        let gen = pty.generation();
        let output_changed = gen != drawn_gen;
        // A change to the mouse selection — a new drag position, or clearing it —
        // must repaint so the inverted highlight tracks the pointer.
        if last_selection != selection {
            interactive = true;
        }
        // The pointer moved onto / off a different cell: repaint so the hovered
        // link's highlight follows it.
        if last_hover != hover {
            interactive = true;
        }
        // The sidebar's running / waiting / live-agent / finished markers, read
        // together under a single lock; repaint when they move so sessions
        // (including this one) keep their current state.
        let badges = monitor.snapshot();
        if state.badges() != &badges {
            interactive = true;
        }

        // Coalesce pure-output frames: an output-only change repaints only once
        // [`MIN_FRAME`] has elapsed since the last paint, so a stream of 8 KiB
        // chunks cannot drive a full-grid redraw faster than the screen refreshes.
        // Anything interactive bypasses the throttle.
        let now = Instant::now();
        let throttled = output_changed
            && !interactive
            && last_paint.is_some_and(|t| now.duration_since(t) < MIN_FRAME);

        if interactive || (output_changed && !throttled) {
            drawn_gen = gen;
            // Hold the parser lock just long enough to detect links (only when the
            // content changed) and snapshot the grid into an owned view.
            let view = {
                let parser = pty.parser();
                let screen = parser.screen();
                if links_cache.as_ref().map(|(g, _)| *g) != Some(gen) {
                    links_cache = Some((gen, terminal_link::link_cells(screen)));
                }
                let links = &links_cache.as_ref().expect("links cache set above").1;
                TerminalView::from_screen_with_links(screen, selection.as_ref(), hover, links)
            };
            // The cursor belongs to the live screen, so don't park it while the
            // user is viewing scrolled-back history. When live, park it on the
            // program's cursor cell even if the program hid it (so the IME's
            // preedit lands there) and mirror the program's show/hide.
            let cursor = if scrollback == 0 { view.cursor() } else { None };
            let cursor_visible = view.cursor_visible();
            // Re-assert the shape only when it moved off what we last emitted, so
            // a stream of output frames doesn't keep re-poking the cursor. The
            // first paint (`last_shape == None`) always emits, claiming this
            // pane's shape from the previously active tab.
            let shape = pty.cursor_shape();
            let cursor_shape = (last_shape != Some(shape)).then_some(shape);
            state.set_terminal_view(view);
            state.apply_badges(badges);
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
        } else {
            None
        };
        match wait(pty, drawn_gen, redraw_deadline)? {
            // New output, or the idle re-evaluation tick (a possible resize /
            // badge change): loop and let the checks above decide whether to
            // repaint — an unchanged tick redraws nothing.
            Wake::Output => {}
            // Input is queued: forward every pending key (or scroll the
            // history), then loop and repaint.
            Wake::Input => {
                if let Some(step) = pump_input(
                    term,
                    state,
                    pty,
                    geo,
                    &mut scrollback,
                    &mut selection,
                    &mut hover,
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
/// When the caller throttled an output-only frame it passes a `redraw_deadline`:
/// pending output is then held back (while still answering input at once) until
/// the deadline passes, so coalesced output lands exactly at the frame boundary
/// rather than immediately re-waking the loop into a busy spin.
fn wait(pty: &PtySession, drawn_gen: u64, redraw_deadline: Option<Instant>) -> Result<Wake> {
    let start = Instant::now();
    loop {
        // Fresh output (or the shell exiting, which also bumps the counter) wakes
        // the loop — but a throttled frame waits out its deadline first.
        if pty.generation() != drawn_gen
            && redraw_deadline.is_none_or(|deadline| Instant::now() >= deadline)
        {
            return Ok(Wake::Output);
        }
        if event::poll(POLL_SLICE)? {
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
/// no drag opens a link under the pointer in the default browser (see
/// [`open_clicked_url`]); a left click on a tab chip switches to that tab
/// ([`PaneStep::ToTab`]; see [`ui::attached_tab_at`]). Button-less motion updates
/// `hover` so the link under the pointer lights up. `Ctrl-O` detaches to 切替
/// ([`PaneStep::Detach`]),
/// leaving every pane alive in the pool; `Ctrl-N` / `Ctrl-P` switch to the next /
/// previous tab in place ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]);
/// `Ctrl-T` zooms out to 在席 (Focus) ([`PaneStep::ToFocus`]), leaving every pane
/// alive; `Ctrl-G` adds an agent tab ([`PaneStep::NewAgentTab`]); `Ctrl-^` jumps
/// to the previously focused session ([`PaneStep::PrevSession`]). `Ctrl-W` is not
/// claimed — it reaches the shell as "delete previous word"; tabs are closed from
/// 切替 (`x`). Other events are ignored so the next redraw picks up any new size.
#[allow(clippy::too_many_arguments)]
fn pump_input(
    term: &Term,
    state: &mut HomeState,
    pty: &mut PtySession,
    geo: ui::TerminalGeometry,
    scrollback: &mut usize,
    selection: &mut Option<Selection>,
    hover: &mut Option<Cell>,
) -> Result<Option<PaneStep>> {
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) => {
                if !is_press(key) {
                    continue;
                }
                // Scroll keys move the history view in place rather than going
                // to the shell; the view shifts under any selection, so drop it.
                if let Some(delta) = key_scroll_lines(&key, geo) {
                    *selection = None;
                    apply_scroll(scrollback, delta);
                    continue;
                }
                if is_leader(&key) {
                    // `Ctrl-O` zooms out to 切替, leaving every pane alive in the
                    // pool. Pane management lives there now, so a single press is
                    // all the pane handles. Typing first snaps back to the live
                    // screen.
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::Detach));
                }
                // `Ctrl-E` opens the session-note editor over the pane; the caller
                // re-attaches once it closes. (Like the tab chords, this claims
                // `Ctrl-E` from the shell/agent.)
                if is_open_note(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::OpenNote));
                }
                // `Ctrl-N` / `Ctrl-P` move between the session's tabs without
                // leaving 没入: hand the step back so the pool-driven loop advances
                // the active pane and re-drives it. (This claims the chords from the
                // shell/agent — the trade for in-pane tab switching.)
                if is_next_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::NextTab));
                }
                if is_prev_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::PrevTab));
                }
                // `Ctrl-T` zooms out to 在席 (Focus) so the user picks the next
                // action (terminal / agent / …) from the session's menu, leaving
                // every pane alive in the pool — like `Ctrl-O` but landing one
                // level shallower. (This claims `Ctrl-T` from the shell/agent.)
                if is_to_focus(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::ToFocus));
                }
                // `Ctrl-G` adds an agent tab in place, so the pool-driven loop
                // applies the change and keeps driving without leaving 没入. (Like
                // the tab chords above, this claims the chord from the shell/agent.)
                //
                // `Ctrl-W` is deliberately *not* claimed: it is the universal
                // "delete previous word" in shells and readline, so stealing it to
                // close a tab destroyed a word mid-command and killed the pane. It
                // now flows to the shell like any other key; closing a tab is done
                // from 切替 (`Ctrl-O`, then `x`).
                if is_new_agent_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::NewAgentTab));
                }
                // `Ctrl-^` leaves 没入 to jump to the previously focused session,
                // re-rooting the pane there (attaching when live). (Like the tab
                // chords, this claims `Ctrl-^` from the shell/agent.)
                if is_prev_session(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::PrevSession));
                }
                // `Ctrl-Q` leaves 没入 to quit usagi: hand it back so the home loop
                // raises the quit-confirmation modal (every pane stays alive in the
                // pool meanwhile). (Like the tab chords, this claims `Ctrl-Q` from
                // the shell/agent — the trade for a global quit key.)
                if is_quit(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::Quit));
                }
                // `Ctrl-B` collapses / expands the left sidebar in place, without
                // leaving 没入: toggle the state and let the next loop pass re-lay
                // out the frame and resize the PTY to the new pane width. (Like the
                // tab chords, this claims `Ctrl-B` from the shell/agent.)
                if is_toggle_sidebar(&key) {
                    state.toggle_sidebar();
                    continue;
                }
                // With text selected, `Ctrl-C` copies it (and clears the
                // selection) instead of sending SIGINT — the way terminals treat
                // copy while a selection is active. With nothing selected it
                // falls through to `encode_key` below and reaches the shell as
                // the usual interrupt.
                if is_copy(&key) && selection.as_ref().is_some_and(|s| !s.is_empty()) {
                    copy_selection(term, pty, selection.as_ref())?;
                    *selection = None;
                    continue;
                }
                let bytes = encode_key(&key);
                if !bytes.is_empty() {
                    // Typing returns to the live screen and ends any selection,
                    // like a real terminal.
                    *scrollback = 0;
                    *selection = None;
                    pty.write(&bytes)?;
                }
            }
            // A bracketed paste arrives as one block: forward it whole, so an
            // agent that supports bracketed paste inserts the multi-line text
            // instead of submitting on each embedded newline.
            Event::Paste(text) => {
                // Pasting returns to the live screen and ends any selection.
                *scrollback = 0;
                *selection = None;
                pty.write(&encode_paste(&text, pty.bracketed_paste()))?;
            }
            Event::Mouse(mouse) => match mouse.kind {
                // A left click on a tab chip switches to that tab in place, like
                // `Ctrl-N` / `Ctrl-P`; otherwise it starts a fresh selection at
                // the clicked cell (clearing any existing one when outside the pane).
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(tab) = ui::attached_tab_at(state, mouse.column, mouse.row, geo) {
                        *scrollback = 0;
                        *selection = None;
                        return Ok(Some(PaneStep::ToTab(tab)));
                    }
                    *selection = pane_cell(mouse.column, mouse.row, geo).map(Selection::new);
                }
                // Dragging the left button stretches the selection's loose end.
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let (Some(sel), Some(cell)) =
                        (selection.as_mut(), pane_cell(mouse.column, mouse.row, geo))
                    {
                        sel.extend(cell);
                    }
                }
                // Releasing after a drag copies the selection; a plain click
                // (no drag) on a link opens it in the default browser instead.
                MouseEventKind::Up(MouseButton::Left) => {
                    if open_clicked_url(pty, geo, mouse.column, mouse.row, selection.as_ref()) {
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
                // line. What it does depends on which grid the program drew into:
                //
                // - On the **primary** screen (a shell), scroll usagi's own history
                //   view; the view shifts, so any selection is dropped.
                // - On the **alternate** screen (a full-screen agent / pager / TUI),
                //   `vt100` keeps no scrollback, so a history scroll is a dead no-op.
                //   Emulate the terminal's alternate-scroll mode instead: forward
                //   the wheel as arrow-key presses so the program scrolls its own
                //   viewport — which is what the wheel does over such a program in a
                //   standalone terminal. (Selection is left alone; nothing scrolled
                //   in usagi.)
                kind => {
                    if let Some(delta) = wheel_delta(kind) {
                        if pane_cell(mouse.column, mouse.row, geo).is_some() {
                            // Read the grid + cursor-key mode under the parser lock,
                            // dropping the guard before the `&mut self` write below.
                            let arrows = {
                                let parser = pty.parser();
                                let screen = parser.screen();
                                screen
                                    .alternate_screen()
                                    .then(|| wheel_arrows(delta, screen.application_cursor()))
                            };
                            match arrows {
                                Some(seq) => pty.write(seq.as_bytes())?,
                                None => {
                                    *selection = None;
                                    apply_scroll(scrollback, delta);
                                }
                            }
                        }
                    }
                }
            },
            _ => {}
        }
    }
    Ok(None)
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
    if let Some(url) = terminal_link::url_at(pty.parser().screen(), cell) {
        open_url(&url);
        return true;
    }
    false
}

/// Hand `url` to the platform's default browser (see
/// [`terminal_link::open_command`]). Best-effort and detached: stdio is closed
/// and a missing opener or spawn failure is ignored, since failing to launch a
/// browser must not disturb the embedded shell.
fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let argv = terminal_link::open_command(url);
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

    // Repaint only the changed rows (see [`diff_frame`]); the cursor is hidden
    // for the repaint and re-positioned below over the shell's cell.
    let mut buf = diff_frame(prev, &frame);

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
