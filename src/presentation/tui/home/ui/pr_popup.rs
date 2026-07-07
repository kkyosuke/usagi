//! Pull-request badge and pinned popup rendering for the home screen.

use crate::presentation::theme::Palette;
use console::style;

use super::super::state::{HomeState, PendingSession, WorktreeList};
use super::sidebar::{
    digits, pr_width, sidebar_scroll_with_pending, SESSION_ROWS, UNITE_WORKSPACE_GAP_ROWS,
};
use super::{widgets, NAME_PREFIX};
use crate::domain::settings::Sidebar;
use crate::domain::workspace_state::PrLink;

/// starts on. Walks the same layout [`left_pane`] builds — in single-workspace
/// mode and 統合(unite) mode (the [`UNITE_WORKSPACE_GAP_ROWS`]-row gap and the
/// one-row group header before each later workspace, the two-row root entry, the
/// divider, then the worktree rows) — so
/// the PR badge hit-test and popup anchor agree with what is drawn without ever
/// drifting from the renderer. The global index is what the PR popup pins, so a
/// badge in any workspace (not just the first group) can open its popup.
pub(super) fn full_sidebar_worktree_entries_with_pending(
    list: &WorktreeList,
    pending_sessions: &[PendingSession],
) -> Vec<(usize, usize)> {
    let united = list.group_count() > 1;
    let mut cur = 0usize; // body line being walked
    let mut global = 0usize; // worktree index across all groups
    let mut out = Vec::new();
    for (g, group) in list.groups().iter().enumerate() {
        if united && g > 0 {
            cur += UNITE_WORKSPACE_GAP_ROWS;
        }
        if united {
            cur += 1; // the unite group header
        }
        cur += ROOT_ENTRY_LINES; // root entry (two rows) + divider
        for _ in group.worktrees() {
            out.push((global, cur));
            cur += SESSION_ROWS;
            global += 1;
        }
        cur += SESSION_ROWS
            * pending_sessions
                .iter()
                .filter(|p| p.is_create() && p.root() == group.root_path())
                .count();
        cur += 1; // the group's persistent "+ new session" row
    }
    out
}

/// The worktree (by global index across every group) whose folded `<icon>
/// <count>` PR badge the 0-based screen (`col`, `row`) lands on, or `None`
/// otherwise — the column-precise hit-test behind opening the PR popup. Clicking
/// the badge pins that session's `#<number>` popup open ([`pr_popup_placement`]);
/// only the badge columns count, so the rest of the row stays free for selection.
///
/// The geometry mirrors what [`super::render_frame`] lays out: the two-pane body
/// begins at row [`BODY_TOP`] (below the one-line header and blank separator)
/// and is [`super::body_rows_for`] rows tall; the left pane is the
/// first `left_w` columns. Within it the entries stack as [`left_pane`] builds
/// them — including the 統合(unite) gaps and group headers, walked by
/// [`full_sidebar_worktree_entries`]. The badge is the right-aligned tail of the
/// detail line's cluster, flush to the pane's right edge (`left_w`); it is the PR
/// glyph, a space, and the count's digits (see [`pr_cell`] / [`pr_width`]).
///
/// Only the full sidebar draws the badge; the collapsed rail shows no PR, so a
/// click there maps to nothing.
pub(in crate::presentation::tui::home) fn sidebar_pr_badge_at(
    state: &HomeState,
    raw_height: usize,
    raw_width: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    if state.sidebar() != Sidebar::Full {
        return None;
    }
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, _) = super::layout(width, Sidebar::Full);
    let col = col as usize;
    // The click must land inside the left pane, on a body row.
    if col >= left_w || row < BODY_TOP {
        return None;
    }
    let body_rows = super::body_rows_for(height);
    let screen_line = (row - BODY_TOP) as usize;
    if screen_line >= body_rows {
        return None;
    }
    // Lift the screen line back into the full-column layout the entries are walked
    // in, so the badge resolves correctly when the list is scrolled.
    let scroll =
        sidebar_scroll_with_pending(state.list(), true, body_rows, state.pending_sessions());
    let line = screen_line + scroll;
    // The badge only lives on each entry's detail line.
    let (idx, _) =
        full_sidebar_worktree_entries_with_pending(state.list(), state.pending_sessions())
            .into_iter()
            .find(|&(_, start)| line == start + DETAIL_LINE)?;
    let wt = state.list().worktree_by_global_index(idx)?;
    if wt.pr.is_empty() {
        return None;
    }
    // The badge seats flush to the pane's right edge. If its width does not fit the
    // detail area (a cramped pane), the cluster is clipped rather than drawn
    // flush-right, so its columns can't be placed — open nothing rather than guess.
    let start = left_w.checked_sub(pr_width(&wt.pr))?;
    if start < NAME_PREFIX {
        return None;
    }
    // The badge stands for every PR, so a click anywhere across its span opens them.
    (start..left_w).contains(&col).then_some(idx)
}

/// The widest a PR popup's content grows before its `#<number>` list wraps to
/// another line, so a session with many PRs stays a tidy box rather than one long
/// row.
pub(super) const PR_POPUP_INNER: usize = 28;

/// The popup box's title, embedded in its top border by [`widgets::boxed`] as
/// `─ PR `. The box must stay at least this wide so the title keeps its closing
/// frame instead of butting against the corner.
pub(super) const PR_POPUP_TITLE: &str = "PR";

/// Greedily packs a session's `prs` into the popup's rows: each `#<number>` token
/// is `#` + its digits wide, joined by a one-space gap, and a row never grows past
/// [`PR_POPUP_INNER`]. Shared by the popup's renderer ([`pr_popup_box`]) and its
/// click hit-test ([`pr_popup_click`]) so they agree on which token sits where.
pub(super) fn pr_popup_pack(prs: &[PrLink]) -> Vec<Vec<&PrLink>> {
    let mut rows: Vec<Vec<&PrLink>> = Vec::new();
    let mut cur: Vec<&PrLink> = Vec::new();
    let mut cur_w = 0usize;
    for pr in prs {
        let tok = 1 + digits(pr.number as usize);
        if cur.is_empty() {
            cur_w = tok;
        } else if cur_w + 1 + tok > PR_POPUP_INNER {
            rows.push(std::mem::take(&mut cur));
            cur_w = tok;
        } else {
            cur_w += 1 + tok;
        }
        cur.push(pr);
    }
    rows.push(cur);
    rows
}

/// The popup box's inner content width: as wide as its widest packed row, never
/// past [`PR_POPUP_INNER`], and at least wide enough to keep the title readable.
pub(super) fn pr_popup_inner(rows: &[Vec<&PrLink>]) -> usize {
    // `boxed` frames the title as `─ {title} ` inside the `inner + 2`-wide top
    // border, so the inner width must clear `title + 1` columns or the trailing
    // space (and the title itself) gets clipped — most visibly for a single
    // narrow `#<n>` token, where the content alone would size the box smaller
    // than its own title.
    let title_floor = PR_POPUP_TITLE.chars().count() + 1;
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|pr| 1 + digits(pr.number as usize))
                .sum::<usize>()
                + r.len().saturating_sub(1)
        })
        .max()
        .unwrap_or(0)
        .min(PR_POPUP_INNER)
        .max(title_floor)
}

/// Builds the pinned PR popup for a session's `prs`: its `#<number>` links
/// (soft link blue, underlined), space-joined and wrapped to [`PR_POPUP_INNER`]
/// columns, wrapped in a titled box ready to float beside the session's row (see
/// [`pr_popup_placement`]). Empty `prs` yields no box (the popup only shows for a
/// PR-bearing session), so the overlay is a no-op.
pub(in crate::presentation::tui::home) fn pr_popup_box(prs: &[PrLink]) -> Vec<String> {
    if prs.is_empty() {
        return Vec::new();
    }
    let rows = pr_popup_pack(prs);
    let inner = pr_popup_inner(&rows);
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|pr| {
                    style(format!("#{}", pr.number))
                        .info()
                        .underlined()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    widgets::boxed(PR_POPUP_TITLE, inner, &lines)
}

/// The pinned PR popup's box and where [`super::render_frame`] floats it — its
/// `(lines, top, left)` already clamped exactly as [`widgets::overlay_at`] would,
/// so the renderer and the click hit-test ([`pr_popup_click`]) agree on the box's
/// on-screen rectangle. `None` when no popup is pinned, the session it names is
/// gone or carries no PR, the sidebar is collapsed to the rail, the 統合(unite) view
/// is stacked, or the box cannot fit the width.
///
/// The anchor mirrors [`super::render_frame`]: the box's top rides the session's
/// first body row — the body opens at [`BODY_TOP`], past the root entry's
/// [`ROOT_ENTRY_LINES`] and `idx` × [`SESSION_ROWS`] earlier rows — and its left
/// edge sits just past the `left_w`-wide pane and the [`super::SEP_WIDTH`] divider,
/// pulled back so a box anchored near an edge still shows in full.
pub(in crate::presentation::tui::home) fn pr_popup_placement(
    state: &HomeState,
    raw_height: usize,
    raw_width: usize,
) -> Option<(Vec<String>, usize, usize)> {
    let idx = state.pr_popup()?;
    if state.sidebar() != Sidebar::Full {
        return None;
    }
    let wt = state.list().worktree_by_global_index(idx)?;
    if wt.pr.is_empty() {
        return None;
    }
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let (left_w, _) = super::layout(width, Sidebar::Full);
    let popup = pr_popup_box(&wt.pr);
    let block_w = popup
        .iter()
        .map(|l| console::measure_text_width(l))
        .max()
        .unwrap_or(0);
    if block_w == 0 || block_w > width {
        return None;
    }
    // The body line the pinned session's entry starts on — walked the same way the
    // sidebar is drawn so the box floats beside it even in 統合(unite) mode, where
    // gaps, headers, and earlier groups push it down.
    let (_, entry_line) =
        full_sidebar_worktree_entries_with_pending(state.list(), state.pending_sessions())
            .into_iter()
            .find(|&(global, _)| global == idx)?;
    // Lift the entry into screen space by the sidebar's scroll offset; if the
    // session's row has scrolled off the top of the pane, its badge is not on
    // screen, so pin nothing rather than float the box over an unrelated row.
    let body_rows = super::body_rows_for(height);
    let scroll =
        sidebar_scroll_with_pending(state.list(), true, body_rows, state.pending_sessions());
    let screen_line = entry_line.checked_sub(scroll).filter(|&l| l < body_rows)?;
    // `render_frame` overlays the box while `lines` holds only the chrome above the
    // body (`BODY_TOP` rows) and the body itself, so the anchor clamps against that
    // same length — and the left edge against the width — exactly as `overlay_at`.
    let base_len = BODY_TOP as usize + body_rows;
    let raw_top = BODY_TOP as usize + screen_line;
    let top = raw_top.min(base_len.saturating_sub(popup.len()));
    let left = (left_w + super::SEP_WIDTH).min(width - block_w);
    Some((popup, top, left))
}

/// What a left click at the 0-based screen (`col`, `row`) does to the pinned PR
/// popup (see [`pr_popup_placement`]): open a specific PR, fall inside the box on no
/// token, or land outside it. The home and immersive loops drive clicks through
/// this so the popup behaves the same in either.
pub(in crate::presentation::tui::home) enum PopupClick {
    /// The click landed on a `#<number>` token: open this URL in the browser.
    Open(String),
    /// The click landed inside the box but not on a token: keep the popup pinned.
    Inside,
    /// The click landed outside the box (or no popup is pinned): dismiss it.
    Outside,
}

/// Resolve a left click against the pinned PR popup. A click on a `#<number>`
/// token yields [`PopupClick::Open`] with that PR's URL; elsewhere inside the box
/// [`PopupClick::Inside`] (the box stays); anywhere else (or with no popup pinned)
/// [`PopupClick::Outside`]. The token columns are recomputed from the same
/// [`pr_popup_pack`] the box is drawn from, offset by the box's `│ ` border and
/// padding, so a click lands on exactly the number the user sees.
pub(in crate::presentation::tui::home) fn pr_popup_click(
    state: &HomeState,
    raw_height: usize,
    raw_width: usize,
    col: u16,
    row: u16,
) -> PopupClick {
    let Some((idx, popup, top, left)) = state.pr_popup().and_then(|idx| {
        pr_popup_placement(state, raw_height, raw_width).map(|(p, t, l)| (idx, p, t, l))
    }) else {
        return PopupClick::Outside;
    };
    let (col, row) = (col as usize, row as usize);
    let block_w = console::measure_text_width(&popup[0]);
    // Outside the box's rectangle: dismiss.
    if row < top || row >= top + popup.len() || col < left || col >= left + block_w {
        return PopupClick::Outside;
    }
    // The first row is the box's top border; content rows follow, the last being the
    // bottom border. `checked_sub` drops a click on the top border, and `pack.get`
    // drops one on the bottom border (its index runs one past the packed rows).
    // `pr_popup_placement` above resolved this same index to a worktree, so it is
    // in range here; re-fetch its PRs to map the token columns.
    let wt = state
        .list()
        .worktree_by_global_index(idx)
        .expect("the pinned index placement already resolved");
    let pack = pr_popup_pack(&wt.pr);
    let Some(tokens) = row.checked_sub(top + 1).and_then(|i| pack.get(i)) else {
        return PopupClick::Inside;
    };
    // `boxed` prefixes each content row with `│ ` (border + a pad space), so the
    // tokens start two columns in from the box's left edge.
    let Some(mut inner_col) = col.checked_sub(left + 2) else {
        return PopupClick::Inside;
    };
    for pr in tokens {
        let w = 1 + digits(pr.number as usize);
        if inner_col < w {
            return PopupClick::Open(pr.url.clone());
        }
        // Step past the token and the one-space gap to the next; a click in the gap
        // (or past the last token) underflows and falls through to `Inside`.
        match inner_col.checked_sub(w + 1) {
            Some(rest) => inner_col = rest,
            None => return PopupClick::Inside,
        }
    }
    PopupClick::Inside
}

/// The 0-based line, within a list entry's [`SESSION_ROWS`] rows, that carries the
/// detail line — the row [`worktree_row`] draws the `#<number>` PR badges on (after
/// the identity line, before the CPU / memory line). The badge hit-test
/// ([`sidebar_pr_badge_at`]) and the renderer share it so they agree on where the
/// badges sit.
pub(super) const DETAIL_LINE: usize = 1;

/// The 0-based screen row the two-pane body begins at, matching the one-line
/// header and blank separator [`super::render_frame`] stacks above it (and the
/// `origin_row` of [`super::terminal_geometry`]).
pub(super) const BODY_TOP: u16 = 2;

/// Lines the left pane spends before the first worktree row: the root entry (two
/// rows) and the divider beneath it. Worktree `i` then occupies the
/// [`SESSION_ROWS`] lines starting at `ROOT_ENTRY_LINES + SESSION_ROWS * i`.
pub(super) const ROOT_ENTRY_LINES: usize = 3;
