//! The home screen's top-level mode and the small enums the mode machinery
//! returns.

/// The home screen's top-level mode.
///
/// The two modes answer a single question: whether the keyboard is operating the
/// session set (`Switch`) or the inside of one selected session (`Closeup`).
/// `Overview` and `Focus` are modal surfaces layered over those modes, and a
/// live embedded terminal is a `Closeup` sub-state rather than a third mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Switch: operate the session set — choose, create, rename, reorder, and
    /// switch sessions from the left pane.
    Switch,
    /// Closeup: operate inside the selected session — either the Focus modal
    /// (menu / prompt) or a live embedded terminal owned by that session.
    Closeup,
}

/// The engagement the home screen records on quit so the next launch can drop
/// the user back where they left off (see
/// [`HomeState::restore_focus`](super::HomeState::restore_focus)). Attached is
/// captured explicitly: it is a [`Mode::Closeup`] sub-state, so a quit from a
/// live pane arms this level before the pane returns to the management loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeLevel {
    /// The cursor was on the session in Switch.
    Switch,
    /// The session was focused in Closeup.
    Closeup,
    /// An embedded pane was live inside Closeup.
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
    /// the pane returns to the Focus modal in Closeup.
    Closed,
    /// The user pressed `Ctrl-O o`: leave the pane to Switch on the left pane.
    /// Re-selecting the same session re-attaches.
    ToSwitch,
    /// The user pressed `Ctrl-E`: leave the pane to open the session-note editor
    /// over it. Closing the editor (save or cancel) re-attaches the session's
    /// pane, so the user drops straight back into the live terminal.
    OpenNote,
    /// The user pressed `Ctrl-T` / `Ctrl-O a`: open the Focus modal — the
    /// session's action menu / prompt, floating over the tab the zoom left so its
    /// live preview keeps showing — leaving every pane alive in the pool. Unlike
    /// [`Self::Closed`] no pane is closed; the panes stay live just as
    /// [`Self::ToSwitch`] keeps them.
    ToFocus,
    /// The user pressed `Ctrl-^`: leave the pane to jump straight to the
    /// previously focused session (vim's `Ctrl-^` / tmux's `last-window`),
    /// attaching it when live. With no previous session recorded the pane returns
    /// to the Focus modal on the current session instead.
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
    /// The top-level modes in order, the single source of truth the mode
    /// indicator, the footer tag, and any other mode-aware chrome read.
    pub const LADDER: [Mode; 2] = [Mode::Switch, Mode::Closeup];

    /// The mode's display name shown in the indicator.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Switch => "Switch",
            Mode::Closeup => "Closeup",
        }
    }

    /// The lowercase tag the footer wraps in brackets (e.g. `[switch]`).
    pub fn tag(self) -> &'static str {
        match self {
            Mode::Switch => "switch",
            Mode::Closeup => "closeup",
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

        assert_eq!(Mode::LADDER, [Mode::Switch, Mode::Closeup]);
        assert_eq!(labels, ["Switch", "Closeup"]);
        assert_eq!(tags, ["switch", "closeup"]);
    }
}
