//! The home screen's mode (the engagement ladder) and the small enums the mode
//! machinery returns: where an Overview returns to, and why an embedded pane
//! exited.

/// The home screen's mode — the "engagement ladder" the design is built around
/// (選択 / 集中 / 没入). Each step moves from overseeing the whole workspace
/// toward operating deeper inside one session. The workspace-wide command line
/// is no longer a resident mode but a command palette overlay summoned with `:`
/// (see [`HomeState::open_command_palette`](super::HomeState::open_command_palette)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 選択 (Overview): the session picker and the default mode. The left pane
    /// has the keyboard for choosing a session (Enter), creating one inline
    /// (`c`), or summoning the command palette (`:`). Backing out (Esc) is inert
    /// at the base Overview; entered from Closeup / Attached via `Ctrl-O`.
    Overview,
    /// 集中 (Closeup): a session is selected and operated in the *right pane* —
    /// either a menu of its runnable commands or a session-scoped prompt
    /// (chosen by [`crate::domain::settings::SessionActionUi`]).
    Closeup,
    /// 没入 (Attached): an embedded terminal / agent is live in the right pane
    /// and keys flow to it. `Ctrl-O` zooms out to Overview.
    Attached,
}

/// The engagement the home screen records on quit so the next launch can drop
/// the user back where they left off (see
/// [`HomeState::restore_focus`](super::HomeState::restore_focus)). It mirrors the
/// reachable depths of [`Mode`] for the *recorded* session: 没入 (Attached) is
/// captured here even though the live event loop never observes [`Mode::Attached`]
/// directly (a quit from a pane drops to [`Mode::Closeup`] before the modal opens),
/// so it is remembered explicitly rather than read back off the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeLevel {
    /// The cursor was on the session in 選択 (Overview).
    Overview,
    /// The session was focused in 集中 (Closeup).
    Closeup,
    /// An embedded pane was live in 没入 (Attached).
    Attached,
}

/// Where a [`Mode::Overview`] should return to when cancelled (`Esc`) — the
/// mode it was opened from. `Ctrl-O` while in Overview always zooms out to the
/// base Overview regardless of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnMode {
    /// The base Overview (the default): `Esc` is inert here, since the home
    /// screen is not left by backing out.
    Base,
    /// Opened from 集中 via `Ctrl-O`.
    Closeup,
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
    /// the pane returns to 集中 (Closeup).
    Closed,
    /// The user pressed `Ctrl-O`: leave the pane to the 選択 (Overview) mode on
    /// the left pane. Re-selecting the same session re-attaches.
    ToOverview,
    /// The user pressed `Ctrl-E`: leave the pane to open the session-note editor
    /// over it. Closing the editor (save or cancel) re-attaches the session's
    /// pane, so the user drops straight back into the live terminal.
    OpenNote,
    /// The user pressed `Ctrl-T`: zoom out to 集中 (Closeup) — the session's
    /// action menu, floating over the tab the zoom left so its live preview keeps
    /// showing — leaving every pane alive in the pool. Unlike [`Self::Closed`] no
    /// pane is closed; the panes stay live just as [`Self::ToOverview`] keeps them.
    ToCloseup,
    /// The user pressed `Ctrl-^`: leave the pane to jump straight to the
    /// previously focused session (vim's `Ctrl-^` / tmux's `last-window`),
    /// attaching it when live. With no previous session recorded the pane returns
    /// to 集中 (Closeup) on the current session instead.
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

impl Mode {
    /// The engagement ladder in order (base → deepest), the single source of
    /// truth the mode indicator, the footer tag, and any other mode-aware chrome
    /// read so a rename never has to be chased across the UI.
    pub const LADDER: [Mode; 3] = [Mode::Overview, Mode::Closeup, Mode::Attached];

    /// The mode's display name shown in the engagement-ladder indicator
    /// (title-cased, English).
    pub fn label(self) -> &'static str {
        match self {
            Mode::Overview => "Overview",
            Mode::Closeup => "Closeup",
            Mode::Attached => "Attached",
        }
    }

    /// The lowercase tag the footer wraps in brackets (e.g. `[overview]`), and
    /// the token the base-view render tests look for.
    pub fn tag(self) -> &'static str {
        match self {
            Mode::Overview => "overview",
            Mode::Closeup => "closeup",
            Mode::Attached => "attached",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_ordered_from_base_to_deepest_with_stable_display_names() {
        let labels: Vec<_> = Mode::LADDER.iter().map(|mode| mode.label()).collect();
        let tags: Vec<_> = Mode::LADDER.iter().map(|mode| mode.tag()).collect();

        assert_eq!(
            Mode::LADDER,
            [Mode::Overview, Mode::Closeup, Mode::Attached]
        );
        assert_eq!(labels, ["Overview", "Closeup", "Attached"]);
        assert_eq!(tags, ["overview", "closeup", "attached"]);
    }
}
