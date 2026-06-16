//! The home screen's mode (the engagement ladder) and the small enums the mode
//! machinery returns: where a Switch returns to, and why an embedded pane exited.

/// The home screen's mode — the "engagement ladder" the design is built around
/// (統括 / 切替 / 在席 / 没入). Each step moves from overseeing the whole
/// workspace toward operating deeper inside one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 統括 (Overview): the workspace-wide command line, the default. The user
    /// types `session` / `config` / `doctor`; results render *below the input*
    /// and the right pane stays blank.
    Overview,
    /// 切替 (Switch): the session picker. The left pane has the keyboard for
    /// choosing a session (Enter), creating one inline (`c`), or backing out
    /// (Esc). Entered from Overview via `session switch`, and from Focus /
    /// Attached via `Ctrl-O`.
    Switch,
    /// 在席 (Focus): a session is selected and operated in the *right pane* —
    /// either a menu of its runnable commands or a session-scoped prompt
    /// (chosen by [`crate::domain::settings::SessionActionUi`]).
    Focus,
    /// 没入 (Attached): an embedded terminal / agent is live in the right pane
    /// and keys flow to it. `Ctrl-O` zooms out to Switch; `Ctrl-O` again to
    /// Overview.
    Attached,
}

/// Where a [`Mode::Switch`] should return to when cancelled (`Esc` / `h`) — the
/// mode it was opened from. `Ctrl-O` while in Switch always zooms out to
/// Overview regardless of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnMode {
    /// Opened from 統括 via `session switch`.
    Overview,
    /// Opened from 在席 via `Ctrl-O`.
    Focus,
    /// Opened from 没入 via `Ctrl-O`; cancelling re-attaches the session.
    Attached,
}

/// Why the embedded terminal pane handed control back to the event loop.
///
/// The pane is driven by the impure terminal loop (`terminal_pane`); this enum
/// is the small, testable vocabulary it returns so the event loop can decide
/// what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneExit {
    /// The shell exited on its own (e.g. the user typed `exit`); it is gone, so
    /// the pane returns to 在席 (Focus).
    Closed,
    /// The user pressed `Ctrl-O`: leave the pane to the 切替 (Switch) mode on the
    /// left pane. Re-selecting the same session re-attaches; `Ctrl-O` again zooms
    /// out to 統括 (Overview).
    ToSwitch,
}
