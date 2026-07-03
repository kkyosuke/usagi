//! The home screen's mode (the engagement ladder) and the small enums the mode
//! machinery returns: where a Switch returns to, and why an embedded pane exited.

/// The home screen's mode — the "engagement ladder" the design is built around
/// (切替 / 在席 / 没入). Each step moves from overseeing the whole workspace
/// toward operating deeper inside one session. The workspace-wide command line
/// is no longer a resident mode but a command palette overlay summoned with `:`
/// (see [`HomeState::open_command_palette`](super::HomeState::open_command_palette)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 切替 (Switch): the session picker and the default mode. The left pane has
    /// the keyboard for choosing a session (Enter), creating one inline (`c`),
    /// or summoning the command palette (`:`). Backing out (Esc) is inert at the
    /// base Switch; entered from Focus / Attached via `Ctrl-O`.
    Switch,
    /// 在席 (Focus): a session is selected and operated in the *right pane* —
    /// either a menu of its runnable commands or a session-scoped prompt
    /// (chosen by [`crate::domain::settings::SessionActionUi`]).
    Focus,
    /// 没入 (Attached): an embedded terminal / agent is live in the right pane
    /// and keys flow to it. `Ctrl-O` zooms out to Switch.
    Attached,
}

/// The engagement the home screen records on quit so the next launch can drop
/// the user back where they left off (see
/// [`HomeState::restore_focus`](super::HomeState::restore_focus)). It mirrors the
/// reachable depths of [`Mode`] for the *recorded* session: 没入 (Attached) is
/// captured here even though the live event loop never observes [`Mode::Attached`]
/// directly (a quit from a pane drops to [`Mode::Focus`] before the modal opens),
/// so it is remembered explicitly rather than read back off the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeLevel {
    /// The cursor was on the session in 切替 (Switch).
    Switch,
    /// The session was focused in 在席 (Focus).
    Focus,
    /// An embedded pane was live in 没入 (Attached).
    Attached,
}

/// Where a [`Mode::Switch`] should return to when cancelled (`Esc`) — the
/// mode it was opened from. `Ctrl-O` while in Switch always zooms out to the
/// base Switch regardless of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnMode {
    /// The base Switch (the default): `Esc` is inert here, since the home screen
    /// is not left by backing out.
    Base,
    /// Opened from 在席 via `Ctrl-O`.
    Focus,
    /// Opened from 没入 via `Ctrl-O`; cancelling re-attaches the session.
    Attached,
}

/// Why the embedded terminal pane handed control back to the event loop.
///
/// The pane is driven by the impure terminal loop (`terminal::pane`); this enum
/// is the small, testable vocabulary it returns so the event loop can decide
/// what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneExit {
    /// The shell exited on its own (e.g. the user typed `exit`); it is gone, so
    /// the pane returns to 在席 (Focus).
    Closed,
    /// The user pressed `Ctrl-O`: leave the pane to the 切替 (Switch) mode on the
    /// left pane. Re-selecting the same session re-attaches.
    ToSwitch,
    /// The user pressed `Ctrl-E`: leave the pane to open the session-note editor
    /// over it. Closing the editor (save or cancel) re-attaches the session's
    /// pane, so the user drops straight back into the live terminal.
    OpenNote,
    /// The user pressed `Ctrl-T`: zoom out to 在席 (Focus) — the session's action
    /// menu — leaving every pane alive in the pool. Unlike [`Self::Closed`] no
    /// pane is closed; the panes stay live just as [`Self::ToSwitch`] keeps them.
    ToFocus,
    /// The user pressed `Ctrl-^`: leave the pane to jump straight to the
    /// previously focused session (vim's `Ctrl-^` / tmux's `last-window`),
    /// attaching it when live. With no previous session recorded the pane returns
    /// to 在席 (Focus) on the current session instead.
    ToPreviousSession,
    /// The user double-clicked a selectable sidebar row: leave the pane to act on
    /// that focus row — attaching a session when live, or opening inline creation
    /// when the row is `+ new session`. The payload is the focus row
    /// `left_pane_session_at` reports (0 the root, `i` the worktree `i - 1`, or
    /// `create_row` for the create affordance).
    ToSession(usize),
    /// The user pressed `Ctrl-Q`: leave the pane to quit usagi. Every pane stays
    /// alive in the pool; the caller raises the quit-confirmation modal rather
    /// than closing outright, so a live agent is never dropped by accident.
    Quit,
}
