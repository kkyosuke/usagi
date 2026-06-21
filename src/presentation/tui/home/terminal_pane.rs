//! Driving the live terminal embedded in the workspace screen's right pane.
//!
//! When the user runs `terminal` / `agent`, the right pane switches to a live
//! shell (没入) drawn while the whole workspace frame — sidebar and all — stays
//! on screen. The shell itself is owned by the [`TerminalPool`] (so it survives
//! leaving the pane); this module borrows it and runs the render/input loop.
//! Keystrokes are forwarded to the shell as raw bytes.
//!
//! The **reserved keys** are `Ctrl-O`, `Ctrl-N`/`Ctrl-P`, `Ctrl-T`/`Ctrl-G`,
//! `Ctrl-W`, and `Ctrl-B`: everything else, including `Esc`, flows to the shell.
//! `Ctrl-B` collapses / expands the left sidebar in place (it never leaves 没入).
//! A single
//! `Ctrl-O` zooms out one engagement level by returning [`PaneStep::Detach`]
//! immediately, leaving the pane for 切替 (Switch) on the left pane while every
//! pane stays alive in the pool — there the user moves between sessions
//! (`↑`/`↓`), between this session's tabs (`←`/`→`), re-attaches (`Enter`), adds a
//! pane (`t`), or zooms further out to 統括 (`Ctrl-O`). `Ctrl-N`/`Ctrl-P` switch to
//! the next / previous tab in place ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]);
//! `Ctrl-T`/`Ctrl-G` add a terminal / agent tab ([`PaneStep::NewTerminalTab`] /
//! [`PaneStep::NewAgentTab`]) and `Ctrl-W` closes the active tab
//! ([`PaneStep::CloseTab`]) — all without leaving 没入. The shell exiting on its
//! own reports [`PaneStep::Closed`].
//!
//! `agent` reuses the same machinery: the pool sends the configured agent CLI to
//! the shell on first spawn, so the pane lands the user straight in the agent.
//!
//! This is pure terminal I/O and threading, so it is excluded from coverage (cf.
//! `term_reader.rs` / the screen `mod.rs` wirings). The pieces it leans on are
//! tested elsewhere: the layout geometry and frame ([`super::ui`]), the screen
//! snapshot ([`super::terminal_view`]), and the [`PaneExit`] vocabulary
//! ([`super::state`]).
//!
//! [`TerminalPool`]: super::terminal_pool::TerminalPool

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use console::Term;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::infrastructure::pty::PtySession;
use crate::presentation::tui::clipboard;
use crate::presentation::tui::screen::diff_frame;

use super::state::HomeState;
use super::terminal_link;
use super::terminal_pool::MonitorHandle;
use super::terminal_selection::{Cell, Selection};
use super::terminal_view::TerminalView;
use super::ui;

/// Why the embedded terminal loop handed control back, so the pool-driven loop
/// in [`super::run`](super) can act on it: the user detached, switched tabs,
/// added / closed a tab, or the shell closed. Tab switching and pane management
/// (add / close) are all handled in place without leaving 没入 — the same actions
/// are also reachable from 切替 (Switch) via `Detach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStep {
    /// `Ctrl-O`: zoom out one level (→ 切替), leaving every pane alive in the pool.
    Detach,
    /// `Ctrl-N`: switch to the next tab without leaving 没入. The caller advances
    /// the pool's active pane and re-drives it.
    NextTab,
    /// `Ctrl-P`: switch to the previous tab without leaving 没入.
    PrevTab,
    /// `Ctrl-T`: add a new terminal tab and make it active, without leaving 没入.
    NewTerminalTab,
    /// `Ctrl-G`: add a new agent tab and make it active, without leaving 没入.
    NewAgentTab,
    /// `Ctrl-W`: close the active tab without leaving 没入. The caller drops it,
    /// then keeps driving the next pane or falls to 在席 when none remain.
    CloseTab,
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

/// How many lines one wheel notch scrolls the embedded terminal's history.
const WHEEL_LINES: i32 = 3;

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
    enable_raw_mode().context("failed to enter raw mode for the embedded terminal")?;
    // Capture pastes as a single `Event::Paste` so a multi-line paste reaches the
    // shell as one block instead of a key stream whose embedded Enters each
    // submit a line to the agent (see `pump_input`).
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    // Turn on button-less motion reporting so links light up on hover; restored
    // on the way out (see [`ENABLE_MOTION`]).
    let _ = term.write_str(ENABLE_MOTION);
    let _ = term.flush();
    let _ = term.clear_screen();
    let result = drive(term, state, pty, monitor);
    let _ = term.write_str(DISABLE_MOTION);
    let _ = term.flush();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = term.show_cursor();
    result
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
            // The cursor belongs to the live screen, so hide it while the user is
            // viewing scrolled-back history.
            let cursor = if scrollback == 0 { view.cursor() } else { None };
            state.set_terminal_view(view);
            state.apply_badges(badges);
            render(term, state, cursor, geo, &mut prev)?;
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
/// [`open_clicked_url`]). Button-less motion updates `hover` so the link under
/// the pointer lights up. `Ctrl-O` detaches to 切替 ([`PaneStep::Detach`]),
/// leaving every pane alive in the pool; `Ctrl-N` / `Ctrl-P` switch to the next /
/// previous tab in place ([`PaneStep::NextTab`] / [`PaneStep::PrevTab`]);
/// `Ctrl-T` / `Ctrl-G` add a terminal / agent tab ([`PaneStep::NewTerminalTab`] /
/// [`PaneStep::NewAgentTab`]) and `Ctrl-W` closes the active tab
/// ([`PaneStep::CloseTab`]). Other events are ignored so the next redraw picks up
/// any new size.
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
                // `Ctrl-T` / `Ctrl-G` add a terminal / agent tab and `Ctrl-W`
                // closes the active one — all in place, so the pool-driven loop
                // applies the change and keeps driving without leaving 没入. (Like
                // the tab chords above, this claims these from the shell/agent.)
                if is_new_terminal_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::NewTerminalTab));
                }
                if is_new_agent_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::NewAgentTab));
                }
                if is_close_tab(&key) {
                    *scrollback = 0;
                    *selection = None;
                    return Ok(Some(PaneStep::CloseTab));
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
                // A left press starts a fresh selection at the clicked cell;
                // pressing outside the pane just clears any existing one.
                MouseEventKind::Down(MouseButton::Left) => {
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
                // The wheel scrolls the history when it is over the terminal
                // pane; the view shifts, so any selection is dropped.
                kind => {
                    if let Some(delta) = wheel_delta(kind) {
                        if (mouse.column as usize) >= geo.origin_col as usize {
                            *selection = None;
                            apply_scroll(scrollback, delta);
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

/// Translate an absolute mouse position (0-based screen `col`/`row`) to a cell
/// in the terminal pane's grid, or `None` when the pointer is outside the pane.
fn pane_cell(col: u16, row: u16, geo: ui::TerminalGeometry) -> Option<Cell> {
    let rel_col = col.checked_sub(geo.origin_col)?;
    let rel_row = row.checked_sub(geo.origin_row)?;
    if rel_col >= geo.cols || rel_row >= geo.rows {
        return None;
    }
    Some(Cell::new(rel_row, rel_col))
}

/// The history scroll a key requests, in lines (negative scrolls up toward older
/// output), or `None` for a key the shell should receive. `Shift` distinguishes
/// the scroll keys from the `PageUp`/`PageDown`/arrows the shell expects.
fn key_scroll_lines(key: &KeyEvent, geo: ui::TerminalGeometry) -> Option<i32> {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    // A page keeps one row of overlap for context; at least one line.
    let page = (geo.rows as i32 - 1).max(1);
    match key.code {
        KeyCode::PageUp => Some(-page),
        KeyCode::PageDown => Some(page),
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        _ => None,
    }
}

/// The history scroll a mouse wheel turn requests, in lines, or `None` for a
/// non-wheel mouse event.
fn wheel_delta(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollUp => Some(-WHEEL_LINES),
        MouseEventKind::ScrollDown => Some(WHEEL_LINES),
        _ => None,
    }
}

/// Move the scrollback offset by `delta` lines (negative scrolls up toward
/// older output). The upper bound is enforced by `set_scrollback` on the next
/// redraw, so this only has to keep the offset from underflowing past the live
/// screen.
fn apply_scroll(scrollback: &mut usize, delta: i32) {
    *scrollback = if delta < 0 {
        scrollback.saturating_add(delta.unsigned_abs() as usize)
    } else {
        scrollback.saturating_sub(delta as usize)
    };
}

/// Draw the workspace frame (sidebar + terminal pane), repainting only the rows
/// that changed since `prev` and batching them into a single write — so an
/// update lands in one pass without the flicker of clearing the whole screen.
/// Finally, park the real cursor over the shell's cursor cell so it tracks the
/// embedded terminal.
fn render(
    term: &Term,
    state: &HomeState,
    cursor: Option<(u16, u16)>,
    geo: ui::TerminalGeometry,
    prev: &mut Vec<String>,
) -> Result<()> {
    let (height, width) = term.size();
    let frame = ui::render_frame(height as usize, width as usize, state);

    // Repaint only the changed rows (see [`diff_frame`]); the cursor is hidden
    // for the repaint and re-shown below over the shell's cell.
    let mut buf = diff_frame(prev, &frame);

    if let Some((row, col)) = cursor {
        // Translate the pane-relative cursor to a 1-based screen position
        // (clamping a deferred-wrap column back onto the pane) and reveal it.
        let (x, y) = geo.cursor_screen_pos(row, col);
        let _ = write!(buf, "\x1b[{y};{x}H\x1b[?25h");
    }

    term.write_str(&buf)?;
    term.flush()?;
    *prev = frame;
    Ok(())
}

/// Only forward real key presses (and auto-repeats), never key releases (which
/// some platforms / the kitty protocol report).
fn is_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// Whether `key` is the Ctrl chord for `letter`, accepting both forms crossterm
/// may report — so a chord binds the same regardless of how the terminal /
/// keyboard protocol delivers it:
///
/// - `letter` + `CONTROL` — crossterm's usual decoding of `Ctrl-<letter>`.
/// - the bare control codepoint `raw` — some terminals/keyboard protocols
///   deliver the chord as the raw control char with no `CONTROL` modifier (this
///   is how `console` reports it on the other home-screen surfaces). The raw
///   control char only ever comes from that chord, so it is accepted regardless
///   of the reported modifiers — otherwise [`encode_key`] would forward the raw
///   byte to the agent, which renders the unprintable control char as a
///   `?`-like placeholder instead of acting on the chord.
///
/// `Ctrl+Shift+<letter>` is deliberately *not* matched: its `Char` is the
/// uppercase letter, so it flows through to the agent unchanged.
fn chord(key: &KeyEvent, raw: char, letter: char) -> bool {
    key.code == KeyCode::Char(raw)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(letter))
}

/// Whether this key is the reserved `Ctrl-O` leader (zoom out one engagement
/// level), as the raw `0x0f` (SI) char or `'o'` + `CONTROL`.
fn is_leader(key: &KeyEvent) -> bool {
    chord(key, '\u{0f}', 'o')
}

/// Whether this key is `Ctrl-N` (next tab), as the raw `0x0e` (SO) char or
/// `'n'` + `CONTROL`.
fn is_next_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{0e}', 'n')
}

/// Whether this key is `Ctrl-P` (previous tab), as the raw `0x10` (DLE) char or
/// `'p'` + `CONTROL`.
fn is_prev_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{10}', 'p')
}

/// Whether this key is `Ctrl-T` (add a terminal tab), as the raw `0x14` (DC4)
/// char or `'t'` + `CONTROL`.
fn is_new_terminal_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{14}', 't')
}

/// Whether this key is `Ctrl-G` (add an agent tab), as the raw `0x07` (BEL) char
/// or `'g'` + `CONTROL`.
fn is_new_agent_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{07}', 'g')
}

/// Whether this key is `Ctrl-W` (close the active tab), as the raw `0x17` (ETB)
/// char or `'w'` + `CONTROL`.
fn is_close_tab(key: &KeyEvent) -> bool {
    chord(key, '\u{17}', 'w')
}

/// Whether this key is `Ctrl-B` (toggle the left sidebar), as the raw `0x02`
/// (STX) char or `'b'` + `CONTROL`.
fn is_toggle_sidebar(key: &KeyEvent) -> bool {
    chord(key, '\u{02}', 'b')
}

/// Whether this key is the copy shortcut (`Ctrl-C`). It only copies when a
/// selection is active; otherwise the caller forwards it to the shell as the
/// usual interrupt. `Ctrl+Shift+C` is left to the shell unchanged.
fn is_copy(key: &KeyEvent) -> bool {
    key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c')
}

/// Bracketed-paste start / end markers (DECSET 2004). A program that requested
/// the mode treats everything between them as one paste.
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Encode a paste for the shell. When the running program asked for bracketed
/// paste (`bracketed`), wrap the text in the start/end markers so it lands as a
/// single block — the agent inserts the multi-line text rather than submitting
/// on each newline. Otherwise forward the raw bytes (the program never opted in,
/// so there is nothing to wrap).
///
/// In the bracketed case any [`PASTE_END`] marker the pasted text itself contains
/// is stripped first: leaving it in would let pasted content close the paste
/// early and have its tail run as live keystrokes (paste injection), so — like
/// real terminals — we neutralise the embedded terminator.
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let body = text.replace(PASTE_END, "");
    let mut bytes = Vec::with_capacity(PASTE_START.len() + body.len() + PASTE_END.len());
    bytes.extend_from_slice(PASTE_START.as_bytes());
    bytes.extend_from_slice(body.as_bytes());
    bytes.extend_from_slice(PASTE_END.as_bytes());
    bytes
}

/// Translate a key event into the bytes a shell expects on its input. Unknown
/// keys map to nothing (an empty slice), so they are simply dropped.
fn encode_key(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) => {
            let mut bytes = Vec::new();
            if alt {
                bytes.push(0x1b);
            }
            if ctrl {
                // Control characters: map the letter to its 0x00–0x1f code.
                bytes.push((c.to_ascii_uppercase() as u8) & 0x1f);
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            bytes
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn is_leader_matches_both_forms_of_ctrl_o() {
        // crossterm's usual decoding: lowercase `'o'` + `CONTROL`.
        assert!(is_leader(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
        // Some terminals deliver the bare `0x0F` (SI) codepoint instead, with
        // no `CONTROL` modifier reported — still the leader, so it must not
        // reach the agent (where it would render as `?`).
        assert!(is_leader(&key(KeyCode::Char('\u{0f}'), KeyModifiers::NONE)));
        assert!(is_leader(&key(
            KeyCode::Char('\u{0f}'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn is_leader_leaves_ctrl_shift_o_alone() {
        // `Ctrl+Shift+O` is not the leader: it flows to the agent unchanged.
        assert!(!is_leader(&key(KeyCode::Char('O'), KeyModifiers::CONTROL)));
        assert!(!is_leader(&key(
            KeyCode::Char('O'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn is_leader_rejects_non_leader_keys() {
        // No `Ctrl` modifier, or a different letter, is not the leader.
        assert!(!is_leader(&key(KeyCode::Char('o'), KeyModifiers::NONE)));
        assert!(!is_leader(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn tab_chords_match_both_forms_and_reject_others() {
        // Ctrl-N (next) / Ctrl-P (prev): crossterm's `'n'`/`'p'` + CONTROL, and
        // the bare control char some terminals deliver instead (0x0e / 0x10).
        assert!(is_next_tab(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)));
        assert!(is_next_tab(&key(
            KeyCode::Char('\u{0e}'),
            KeyModifiers::NONE
        )));
        assert!(is_prev_tab(&key(KeyCode::Char('p'), KeyModifiers::CONTROL)));
        assert!(is_prev_tab(&key(
            KeyCode::Char('\u{10}'),
            KeyModifiers::NONE
        )));
        // Plain letters, the wrong modifier, and the other chord are rejected.
        assert!(!is_next_tab(&key(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert!(!is_prev_tab(&key(KeyCode::Char('p'), KeyModifiers::NONE)));
        assert!(!is_next_tab(&key(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_prev_tab(&key(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn pane_chords_match_both_forms_and_reject_others() {
        // Ctrl-T (add terminal) / Ctrl-G (add agent) / Ctrl-W (close): crossterm's
        // letter + CONTROL, and the bare control char some terminals deliver
        // instead (0x14 / 0x07 / 0x17).
        assert!(is_new_terminal_tab(&key(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL
        )));
        assert!(is_new_terminal_tab(&key(
            KeyCode::Char('\u{14}'),
            KeyModifiers::NONE
        )));
        assert!(is_new_agent_tab(&key(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL
        )));
        assert!(is_new_agent_tab(&key(
            KeyCode::Char('\u{07}'),
            KeyModifiers::NONE
        )));
        assert!(is_close_tab(&key(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL
        )));
        assert!(is_close_tab(&key(
            KeyCode::Char('\u{17}'),
            KeyModifiers::NONE
        )));
        // Plain letters and the wrong chord are rejected (they flow to the shell).
        assert!(!is_new_terminal_tab(&key(
            KeyCode::Char('t'),
            KeyModifiers::NONE
        )));
        assert!(!is_new_agent_tab(&key(
            KeyCode::Char('g'),
            KeyModifiers::NONE
        )));
        assert!(!is_close_tab(&key(KeyCode::Char('w'), KeyModifiers::NONE)));
        assert!(!is_close_tab(&key(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn is_copy_matches_only_plain_ctrl_c() {
        // `Ctrl-C` is the copy shortcut (only meaningful with a selection).
        assert!(is_copy(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        // A bare `c`, a different letter, or `Ctrl+Shift+C` is not the shortcut
        // and flows to the shell unchanged.
        assert!(!is_copy(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_copy(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
        assert!(!is_copy(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn encode_paste_passes_raw_bytes_when_not_bracketed() {
        // No bracketed-paste mode: forward the text unwrapped, verbatim.
        assert_eq!(encode_paste("ls -la\n", false), b"ls -la\n".to_vec());
    }

    #[test]
    fn encode_paste_wraps_in_bracketed_markers() {
        assert_eq!(
            encode_paste("hi", true),
            [PASTE_START, "hi", PASTE_END].concat().into_bytes(),
        );
    }

    #[test]
    fn encode_paste_strips_an_embedded_end_marker() {
        // Paste-injection guard: an end marker inside the pasted text would
        // otherwise close the paste early and run its tail as live keystrokes.
        let malicious = format!("safe{PASTE_END}rm -rf ~\n");
        let encoded = encode_paste(&malicious, true);
        let expected = [PASTE_START, "saferm -rf ~\n", PASTE_END]
            .concat()
            .into_bytes();
        assert_eq!(encoded, expected);
        // The terminator appears exactly once — only the wrapper's own trailer.
        let needle = PASTE_END.as_bytes();
        let hits = encoded
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count();
        assert_eq!(hits, 1);
    }
}
