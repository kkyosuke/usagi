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

use crate::domain::settings::AgentCli;

/// What an embedded pane runs: the configured AI agent CLI, or a plain shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// The agent CLI (launched once on spawn); its lifecycle drives the sidebar
    /// badge for the session.
    Agent,
    /// A plain interactive shell.
    Terminal,
}

/// Stable tab-label identity for a live pane: its kind, the agent CLI it runs
/// (for an agent pane), plus a monotonically assigned creation id. Labels are
/// emitted in pane order, but duplicate ordinals are derived from creation-id
/// order so dragging tabs does not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneTab {
    /// What the pane runs.
    pub kind: PaneKind,
    /// For an agent pane, which CLI it runs — so the tab reads `Claude` /
    /// `Codex` / `sakana.ai` / `Gemini` rather than a bare `agent`. `None` for a
    /// terminal pane (and defensively falls back to `agent` for an agent pane
    /// with no recorded CLI).
    pub cli: Option<AgentCli>,
    /// Monotonic id assigned when the pane is spawned/restored.
    pub id: u64,
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

/// How the user asked to *reorder* the active tab within the session — moving
/// the pane itself one slot, rather than moving the cursor between panes
/// ([`TabNav`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabSwap {
    /// Swap the active tab with its left neighbour (toward the first tab).
    Left,
    /// Swap the active tab with its right neighbour (toward the last tab).
    Right,
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

/// The two pane indices to swap to move the active tab one slot in `swap`'s
/// direction, given a session of `len` panes whose active tab is `active`. The
/// active tab follows the pane, so the second index of the pair is its new
/// position. Returns `None` when the move is impossible — fewer than two panes,
/// or the active tab already sits at the edge it would move toward — so
/// reordering does **not** wrap around the ends (unlike [`resolve_nav`]) and a
/// no-op lets 没入 skip the repaint.
pub fn resolve_swap(active: usize, len: usize, swap: TabSwap) -> Option<(usize, usize)> {
    if len < 2 || active >= len {
        return None;
    }
    match swap {
        TabSwap::Left => {
            let target = active.checked_sub(1)?;
            Some((active, target))
        }
        TabSwap::Right => {
            let target = active + 1;
            (target < len).then_some((active, target))
        }
    }
}

/// The source and destination indices for moving a tab to an arbitrary slot
/// (drag/drop). Both indices are clamped to the live tab range; moving to the
/// same slot is a no-op. Use [`active_after_move`] to keep the active cursor on
/// the same pane after applying the move.
pub fn resolve_move(from: usize, to: usize, len: usize) -> Option<(usize, usize)> {
    if len < 2 || from >= len {
        return None;
    }
    let target = to.min(len - 1);
    (from != target).then_some((from, target))
}

/// The active tab index after a pane is moved from `from` to `to` (the clamped
/// pair [`resolve_move`] returns), tracking whichever tab was active *before*
/// the move rather than the moved pane itself.
///
/// Keyboard reordering ([`resolve_swap`]) always moves the active tab, so the
/// active index simply follows it. Drag/drop, though, moves the pane the pointer
/// grabbed — which need not be the active one — so the active tab must instead
/// stay on the same pane, whose slot shifts when a *different* pane is pulled out
/// of `from` and pushed in at `to`:
/// - `from == active`: the active pane is the one moved, so it lands on `to`.
/// - `from < active <= to`: a pane to its left was pulled past it, so it slides
///   one slot left.
/// - `to <= active < from`: a pane to its right was inserted before it, so it
///   slides one slot right.
/// - otherwise the move happens entirely on one side of the active pane and its
///   index is unchanged.
pub fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if from == active {
        to
    } else if from < active && active <= to {
        active - 1
    } else if to <= active && active < from {
        active + 1
    } else {
        active
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

/// One label per pane, in pane order: an agent pane by its CLI display name
/// (`Claude` / `Codex` / `sakana.ai` / `Gemini`), a plain shell as `terminal`,
/// with a 1-based ordinal appended only when a label word appears more than once
/// (`terminal 1`, `terminal 2`, or `Claude 1`, `Claude 2`) so duplicate tabs
/// stay distinguishable while a lone tab reads cleanly. Ordinals are grouped by
/// label word — a Claude and a Codex tab are each unique and stay bare — and
/// assigned by stable creation id, not current tab-strip order, so reordering
/// tabs does not rename the panes.
pub fn tab_labels(panes: &[PaneTab]) -> Vec<String> {
    // The base word for each pane, before numbering (agents differ by CLI, so a
    // Claude and a Codex tab fall into separate groups).
    let words: Vec<&str> = panes.iter().map(pane_word).collect();
    let (ordinals, totals) = ordinals_by_word(panes, &words);
    (0..panes.len())
        .map(|index| label(words[index], ordinals[index], totals[index]))
        .collect()
}

/// The label word a pane groups under: an agent pane by its CLI display name
/// (falling back to `agent` when no CLI is recorded), a terminal pane as
/// `terminal`.
fn pane_word(pane: &PaneTab) -> &'static str {
    match pane.kind {
        PaneKind::Agent => pane.cli.map_or("agent", AgentCli::display_name),
        PaneKind::Terminal => "terminal",
    }
}

/// Two vectors keyed by pane index: each pane's 1-based ordinal within its label
/// word (in creation-id order) and the size of that word's group. A word with a
/// single member gets ordinal 1 / total 1 so [`label`] renders it bare.
fn ordinals_by_word(panes: &[PaneTab], words: &[&str]) -> (Vec<usize>, Vec<usize>) {
    let mut ordinals = vec![0; panes.len()];
    let mut totals = vec![0; panes.len()];
    let mut distinct: Vec<&str> = Vec::new();
    for &word in words {
        if !distinct.contains(&word) {
            distinct.push(word);
        }
    }
    for word in distinct {
        let mut members: Vec<usize> = (0..panes.len()).filter(|&i| words[i] == word).collect();
        members.sort_by_key(|&i| (panes[i].id, i));
        let total = members.len();
        for (ordinal, index) in members.into_iter().enumerate() {
            ordinals[index] = ordinal + 1;
            totals[index] = total;
        }
    }
    (ordinals, totals)
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
    fn swap_moves_the_active_tab_one_slot_without_wrapping() {
        assert_eq!(resolve_swap(1, 3, TabSwap::Left), Some((1, 0)));
        assert_eq!(resolve_swap(1, 3, TabSwap::Right), Some((1, 2)));
        assert_eq!(
            resolve_swap(0, 3, TabSwap::Left),
            None,
            "left edge does not wrap to the right edge"
        );
        assert_eq!(
            resolve_swap(2, 3, TabSwap::Right),
            None,
            "right edge does not wrap to the left edge"
        );
    }

    #[test]
    fn swap_is_a_noop_without_a_valid_active_tab() {
        assert_eq!(resolve_swap(0, 0, TabSwap::Right), None);
        assert_eq!(resolve_swap(0, 1, TabSwap::Right), None);
        assert_eq!(resolve_swap(2, 2, TabSwap::Left), None);
    }

    #[test]
    fn move_reorders_to_a_clamped_target_slot() {
        assert_eq!(resolve_move(0, 2, 3), Some((0, 2)));
        assert_eq!(resolve_move(2, 0, 3), Some((2, 0)));
        assert_eq!(
            resolve_move(0, 99, 3),
            Some((0, 2)),
            "a drop past the end lands on the last tab"
        );
    }

    #[test]
    fn move_is_a_noop_for_same_or_invalid_slots() {
        assert_eq!(resolve_move(1, 1, 3), None);
        assert_eq!(resolve_move(0, 0, 0), None);
        assert_eq!(resolve_move(0, 0, 1), None);
        assert_eq!(resolve_move(3, 0, 3), None);
    }

    #[test]
    fn active_follows_the_moved_pane_when_dragging_the_active_tab() {
        assert_eq!(active_after_move(1, 1, 3), 3);
        assert_eq!(active_after_move(3, 3, 0), 0);
    }

    #[test]
    fn active_stays_on_the_same_unmoved_pane_after_drag_drop() {
        assert_eq!(
            active_after_move(2, 0, 3),
            1,
            "pulling a tab from the left past the active pane shifts it left"
        );
        assert_eq!(
            active_after_move(1, 3, 0),
            2,
            "inserting a tab from the right before the active pane shifts it right"
        );
        assert_eq!(
            active_after_move(0, 2, 3),
            0,
            "moves entirely to the right leave the active index alone"
        );
        assert_eq!(
            active_after_move(3, 0, 1),
            3,
            "moves entirely to the left leave the active index alone"
        );
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
    fn labels_name_an_agent_by_its_cli_and_stay_bare_when_unique() {
        let labels = tab_labels(&[
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::Claude),
                id: 1,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 2,
            },
        ]);
        assert_eq!(
            labels,
            vec!["Claude".to_string(), "terminal".to_string()],
            "the lone agent reads as its CLI name; the lone terminal stays bare"
        );
    }

    #[test]
    fn labels_keep_different_cli_agents_bare_and_distinct() {
        let labels = tab_labels(&[
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::Claude),
                id: 1,
            },
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::SakanaAi),
                id: 2,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 3,
            },
        ]);
        assert_eq!(
            labels,
            vec![
                "Claude".to_string(),
                "sakana.ai".to_string(),
                "terminal".to_string(),
            ],
            "distinct agent CLIs are each their own group, so all stay bare"
        );
    }

    #[test]
    fn labels_fall_back_to_agent_without_a_recorded_cli() {
        let labels = tab_labels(&[PaneTab {
            kind: PaneKind::Agent,
            cli: None,
            id: 1,
        }]);
        assert_eq!(labels, vec!["agent".to_string()]);
    }

    #[test]
    fn labels_number_duplicates_of_a_word() {
        let labels = tab_labels(&[
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::Claude),
                id: 1,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 2,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 3,
            },
        ]);
        assert_eq!(
            labels,
            vec![
                "Claude".to_string(),
                "terminal 1".to_string(),
                "terminal 2".to_string(),
            ],
            "the unique agent stays bare; the two terminals are numbered"
        );
    }

    #[test]
    fn labels_number_same_cli_agents() {
        let labels = tab_labels(&[
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::Claude),
                id: 1,
            },
            PaneTab {
                kind: PaneKind::Agent,
                cli: Some(AgentCli::Claude),
                id: 2,
            },
        ]);
        assert_eq!(
            labels,
            vec!["Claude 1".to_string(), "Claude 2".to_string()],
            "two agents of the same CLI are numbered within their word group"
        );
    }

    #[test]
    fn labels_keep_duplicate_ordinals_after_reordering_tabs() {
        let labels = tab_labels(&[
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 7,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 2,
            },
            PaneTab {
                kind: PaneKind::Terminal,
                cli: None,
                id: 5,
            },
        ]);
        assert_eq!(
            labels,
            vec![
                "terminal 3".to_string(),
                "terminal 1".to_string(),
                "terminal 2".to_string(),
            ],
            "the output follows tab order, but the numbers follow creation-id order"
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
