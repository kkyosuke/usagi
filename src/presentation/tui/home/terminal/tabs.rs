//! Pure tab-strip logic for the embedded terminal panes (没入).
//!
//! A session (worktree) can hold several live panes at once — an `agent` running
//! alongside one or more plain `terminal`s — shown as a tab strip above the
//! embedded terminal and switched without leaving 没入. The panes themselves (the
//! PTYs) are owned by the [`TerminalPool`]; everything that decides *which* tab is
//! active, *what each tab is labelled*, and *where the active tab lands after a
//! navigation or a close* is pure index/label arithmetic and lives here, unit
//! tested, so the (coverage-excluded) pool and pane only have to call it.
//!
//! [`TerminalPool`]: super::pool::TerminalPool

/// What an embedded pane runs: the configured AI agent CLI, or a plain shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// The agent CLI (launched once on spawn); its lifecycle drives the sidebar
    /// badge for the session.
    Agent,
    /// A plain interactive shell.
    Terminal,
}

/// How the user asked to move the active tab within the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabNav {
    /// To the next tab, wrapping past the last back to the first.
    Next,
    /// To the previous tab, wrapping past the first back to the last.
    Prev,
    /// Jump straight to a 0-based tab index (clamped to the last tab).
    To(usize),
}

/// The tab strip the renderer draws above the embedded terminal: one label per
/// live pane and which one is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStrip {
    /// One label per pane, in pane order (see [`tab_labels`]).
    pub labels: Vec<String>,
    /// The 0-based index of the active (visible) tab.
    pub active: usize,
}

/// The active tab index after applying `nav` to a session of `len` panes whose
/// active tab is `active`. `Next` / `Prev` wrap around the ends; `To` clamps to
/// the last tab. An empty session (`len == 0`) stays at `0`.
pub fn resolve_nav(active: usize, len: usize, nav: TabNav) -> usize {
    if len == 0 {
        return 0;
    }
    match nav {
        TabNav::Next => (active + 1) % len,
        TabNav::Prev => (active + len - 1) % len,
        TabNav::To(index) => index.min(len - 1),
    }
}

/// The active tab index after the active pane is closed, given the session had
/// `len_before` panes with the closed one at `active`. Returns `None` when no
/// panes remain. The cursor stays on the same slot, clamped to the new last tab —
/// so closing the rightmost tab lands on its left neighbour, and closing any
/// other lands on the tab that shifts into its place.
pub fn active_after_close(active: usize, len_before: usize) -> Option<usize> {
    let new_len = len_before.saturating_sub(1);
    if new_len == 0 {
        return None;
    }
    Some(active.min(new_len - 1))
}

/// One label per pane, in pane order: `agent` / `terminal`, with a 1-based
/// ordinal appended only when a kind appears more than once (`terminal 1`,
/// `terminal 2`) so duplicate tabs stay distinguishable while a lone tab reads
/// cleanly.
pub fn tab_labels(kinds: &[PaneKind]) -> Vec<String> {
    let agents = kinds
        .iter()
        .filter(|k| matches!(k, PaneKind::Agent))
        .count();
    let terminals = kinds
        .iter()
        .filter(|k| matches!(k, PaneKind::Terminal))
        .count();
    let mut seen_agent = 0usize;
    let mut seen_terminal = 0usize;
    kinds
        .iter()
        .map(|kind| match kind {
            PaneKind::Agent => {
                seen_agent += 1;
                label("agent", seen_agent, agents)
            }
            PaneKind::Terminal => {
                seen_terminal += 1;
                label("terminal", seen_terminal, terminals)
            }
        })
        .collect()
}

/// `word` on its own when it is the only pane of its kind, or `word N`
/// (1-based) when several share the kind.
fn label(word: &str, ordinal: usize, total: usize) -> String {
    if total > 1 {
        format!("{word} {ordinal}")
    } else {
        word.to_string()
    }
}

/// The pane kind a first launch opens: the agent CLI when `agent`, else a plain
/// terminal.
pub fn pane_kind(agent: bool) -> PaneKind {
    if agent {
        PaneKind::Agent
    } else {
        PaneKind::Terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_and_prev_wrap_around_the_ends() {
        assert_eq!(resolve_nav(0, 3, TabNav::Next), 1);
        assert_eq!(
            resolve_nav(2, 3, TabNav::Next),
            0,
            "next wraps past the last"
        );
        assert_eq!(
            resolve_nav(0, 3, TabNav::Prev),
            2,
            "prev wraps past the first"
        );
        assert_eq!(resolve_nav(2, 3, TabNav::Prev), 1);
    }

    #[test]
    fn jump_clamps_to_the_last_tab() {
        assert_eq!(resolve_nav(0, 3, TabNav::To(1)), 1);
        assert_eq!(
            resolve_nav(0, 3, TabNav::To(9)),
            2,
            "an out-of-range jump clamps"
        );
    }

    #[test]
    fn nav_on_an_empty_session_stays_at_zero() {
        assert_eq!(resolve_nav(0, 0, TabNav::Next), 0);
        assert_eq!(resolve_nav(0, 0, TabNav::Prev), 0);
        assert_eq!(resolve_nav(0, 0, TabNav::To(5)), 0);
    }

    #[test]
    fn closing_keeps_the_slot_clamped_to_the_new_last() {
        // Closing a middle tab keeps the index (the next tab shifts into place).
        assert_eq!(active_after_close(1, 3), Some(1));
        // Closing the rightmost tab steps onto its left neighbour.
        assert_eq!(active_after_close(2, 3), Some(1));
        // Closing the first of two lands on the survivor.
        assert_eq!(active_after_close(0, 2), Some(0));
    }

    #[test]
    fn closing_the_last_pane_leaves_no_active() {
        assert_eq!(active_after_close(0, 1), None);
    }

    #[test]
    fn labels_stay_bare_when_a_kind_is_unique() {
        let labels = tab_labels(&[PaneKind::Agent, PaneKind::Terminal]);
        assert_eq!(labels, vec!["agent".to_string(), "terminal".to_string()]);
    }

    #[test]
    fn labels_number_duplicates_of_a_kind() {
        let labels = tab_labels(&[PaneKind::Agent, PaneKind::Terminal, PaneKind::Terminal]);
        assert_eq!(
            labels,
            vec![
                "agent".to_string(),
                "terminal 1".to_string(),
                "terminal 2".to_string(),
            ],
            "the unique agent stays bare; the two terminals are numbered"
        );
    }

    #[test]
    fn labels_of_an_empty_session_are_empty() {
        assert!(tab_labels(&[]).is_empty());
    }

    #[test]
    fn pane_kind_opens_an_agent_only_when_asked() {
        assert_eq!(pane_kind(true), PaneKind::Agent);
        assert_eq!(pane_kind(false), PaneKind::Terminal);
    }
}
