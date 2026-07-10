//! Pull-request badge and pinned popup rendering for the home screen.

use crate::presentation::theme::Palette;
use console::style;

use super::super::state::{HomeState, PendingSession, WorktreeList};
use super::sidebar::{
    pr_width, sidebar_scroll_with_pending, SESSION_ROWS, UNITE_WORKSPACE_GAP_ROWS,
};
use super::{widgets, NAME_PREFIX};
use crate::domain::settings::Sidebar;
use crate::domain::workspace_state::{PrLink, PrState};

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

/// The widest a PR popup's content grows before a long title is clipped, so a
/// session's PR list stays a tidy box rather than spanning the whole screen. The
/// popup groups PRs by repository (`owner/repo` header + one indented `#<number>
/// <title>` row each), so this is the row's full width budget — wide enough for a
/// title to read comfortably.
pub(super) const PR_POPUP_INNER: usize = 72;

/// The popup box's title, embedded in its top border by [`widgets::boxed`] as
/// `─ PR `. The box must stay at least this wide so the title keeps its closing
/// frame instead of butting against the corner.
pub(super) const PR_POPUP_TITLE: &str = "PR";

/// Leading spaces that indent a PR row under its `owner/repo` header.
const INDENT: usize = 1;

/// Display columns reserved for the `#<number>` cell, so every title starts in the
/// **same** column no matter how many digits a PR number has: `#` + up to five
/// digits. A rare longer number simply pushes its own title right without shifting
/// the others.
const NUM_FIELD: usize = 6;

/// Spaces between the `#<number>` cell and the title. With [`INDENT`], the glyph
/// and its trailing space, and [`NUM_FIELD`], this fixes the column every title
/// starts in so they line up down the list.
const TITLE_GAP: usize = 2;

/// Columns at the **start** of a PR row that form its state-glyph hit zone (the
/// indent, the `○`/`●`/`⨯` glyph, and the space after it): a click here toggles
/// open↔merged, or restores a dismissed PR to open.
const GLYPH_ZONE: usize = INDENT + 2;

/// Columns at the **end** of a PR row that form its action hit zone (the `✕`/`↺`
/// glyph and the separating space before it): a click here hides an active PR, or
/// restores a dismissed one. The renderer always leaves the last-but-one column a
/// space, so this zone never overlaps title text.
const ACTION_ZONE: usize = 2;

/// The state glyph shown at the head of a PR row: `○` open, `●` merged, `⨯`
/// dismissed. Colour is applied by [`render_row`]; this is the bare glyph so width
/// measurement and rendering agree.
fn state_glyph(state: PrState) -> char {
    match state {
        PrState::Open => '○',
        PrState::Merged => '●',
        PrState::Dismissed => '⨯',
    }
}

/// The PR state a click on the row's glyph moves it to: open and merged toggle each
/// other, and a dismissed PR restores to open.
fn glyph_target(state: PrState) -> PrState {
    match state {
        PrState::Open => PrState::Merged,
        PrState::Merged => PrState::Open,
        PrState::Dismissed => PrState::Open,
    }
}

/// The `owner/repo` a PR belongs to, taken from the two path segments right before
/// `/pull/<N>` in its URL (so `https://github.com/o/r/pull/9` → `o/r`). Used as the
/// group header. A URL without that shape falls back to everything before `/pull/`.
fn pr_repo(url: &str) -> String {
    let before = url.split("/pull/").next().unwrap_or(url);
    let mut segs = before.rsplit('/');
    let repo = segs.next().unwrap_or("");
    let owner = segs.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() {
        before.to_string()
    } else {
        format!("{owner}/{repo}")
    }
}

/// One rendered line of the popup: an `owner/repo` group header, a PR row beneath
/// it (active in the default view, or dismissed when the "hidden" view is
/// expanded), or the toggle footer that reveals the dismissed PRs. [`popup_rows`]
/// builds the sequence so the renderer and the click hit-test map screen rows to
/// the same thing.
enum PopupRow<'a> {
    Repo(String),
    Pr(&'a PrLink),
    Footer { hidden: usize, expanded: bool },
}

/// Emit `prs` grouped by `owner/repo` into `rows`: each distinct repository (in
/// first-seen order) gets one [`PopupRow::Repo`] header followed by its PRs as
/// [`PopupRow::Pr`] rows.
fn push_repo_groups<'a>(rows: &mut Vec<PopupRow<'a>>, prs: impl Iterator<Item = &'a PrLink>) {
    let mut groups: Vec<(String, Vec<&PrLink>)> = Vec::new();
    for pr in prs {
        let repo = pr_repo(&pr.url);
        match groups.iter_mut().find(|(r, _)| *r == repo) {
            Some((_, v)) => v.push(pr),
            None => groups.push((repo, vec![pr])),
        }
    }
    for (repo, prs) in groups {
        rows.push(PopupRow::Repo(repo));
        rows.extend(prs.into_iter().map(PopupRow::Pr));
    }
}

/// The rows the popup shows for `prs`: the **visible** PRs grouped under their
/// `owner/repo` headers, then — when any PR is dismissed — a `N 件非表示` toggle
/// footer, and (only while `show_dismissed`) the dismissed PRs grouped the same way
/// beneath it. This single source of truth is shared by [`pr_popup_box`]
/// (rendering) and [`pr_popup_click`] (hit-testing) so the two never disagree on
/// which row is which.
fn popup_rows(prs: &[PrLink], show_dismissed: bool) -> Vec<PopupRow<'_>> {
    let mut rows: Vec<PopupRow> = Vec::new();
    push_repo_groups(&mut rows, prs.iter().filter(|p| p.is_visible()));
    let hidden = prs.iter().filter(|p| p.is_dismissed()).count();
    if hidden > 0 {
        rows.push(PopupRow::Footer {
            hidden,
            expanded: show_dismissed,
        });
        if show_dismissed {
            push_repo_groups(&mut rows, prs.iter().filter(|p| p.is_dismissed()));
        }
    }
    rows
}

/// The plain (unstyled) leading text of a row, used only to size the box: the repo
/// header, the footer label, or a PR row laid out exactly as [`render_row`] draws
/// it up to the title (indent, glyph, number field, gap). The trailing action glyph
/// is accounted for by [`pr_popup_inner`]'s reserve, not here.
fn row_left_text(row: &PopupRow) -> String {
    match row {
        PopupRow::Repo(repo) => repo.clone(),
        PopupRow::Pr(pr) => {
            let glyph = state_glyph(pr.state);
            let num = format!("#{}", pr.number);
            let num_pad = NUM_FIELD.saturating_sub(console::measure_text_width(&num));
            let prefix = format!(
                "{}{glyph} {num}{}{}",
                " ".repeat(INDENT),
                " ".repeat(num_pad),
                " ".repeat(TITLE_GAP)
            );
            match pr.title.as_deref().filter(|t| !t.is_empty()) {
                Some(title) => format!("{prefix}{title}"),
                None => prefix,
            }
        }
        PopupRow::Footer { hidden, expanded } => {
            format!("{hidden} 件非表示 {}", if *expanded { '▾' } else { '▸' })
        }
    }
}

/// The popup box's inner content width: as wide as its widest row (PR rows reserve
/// [`ACTION_ZONE`] columns for their trailing `✕`/`↺`), never past
/// [`PR_POPUP_INNER`], and at least wide enough to keep the `PR` title readable.
fn pr_popup_inner(rows: &[PopupRow]) -> usize {
    // `boxed` frames the title as `─ {title} ` inside the `inner + 2`-wide top
    // border, so the inner width must clear `title + 1` columns or the trailing
    // space (and the title itself) gets clipped.
    let title_floor = PR_POPUP_TITLE.chars().count() + 1;
    rows.iter()
        .map(|row| {
            let w = console::measure_text_width(&row_left_text(row));
            match row {
                PopupRow::Pr(_) => w + ACTION_ZONE,
                PopupRow::Repo(_) | PopupRow::Footer { .. } => w,
            }
        })
        .max()
        .unwrap_or(0)
        .min(PR_POPUP_INNER)
        .max(title_floor)
}

/// Render one [`PopupRow`] to an exactly `inner`-wide line for [`widgets::boxed`].
///
/// A repo header is a bold `owner/repo` label. A PR row is indented under it and
/// leads with its coloured state glyph (`○` dim / `●` merged-magenta / `⨯` dim),
/// the soft-blue underlined `#<number>` in a fixed-width cell (so every title starts
/// in the same column), and the `gh`-resolved title (dimmed once merged or
/// dismissed), then pads to the right edge where the action glyph sits — `✕` to hide
/// an active PR, `↺` to restore a dismissed one. The last-but-one column is always a
/// pad space, so [`ACTION_ZONE`] never lands on title text. A footer row is a dim
/// full-width `N 件非表示 ▸/▾` label (padded by `boxed`).
fn render_row(row: &PopupRow, inner: usize) -> String {
    match row {
        PopupRow::Repo(repo) => style(super::clip_to_width(repo, inner)).bold().to_string(),
        PopupRow::Footer { hidden, expanded } => {
            let text = format!("{hidden} 件非表示 {}", if *expanded { '▾' } else { '▸' });
            style(super::clip_to_width(&text, inner)).dim().to_string()
        }
        PopupRow::Pr(pr) => {
            let glyph = state_glyph(pr.state);
            let glyph = match pr.state {
                PrState::Merged => style(glyph).feature(),
                _ => style(glyph).dim(),
            }
            .to_string();
            let num_plain = format!("#{}", pr.number);
            let num_pad = NUM_FIELD.saturating_sub(console::measure_text_width(&num_plain));
            let number = style(num_plain).info().underlined().to_string();
            // Indent, glyph, space, the number in its fixed-width cell, then the gap
            // — so `TITLE_COL` is where every title begins.
            let mut left = format!(
                "{}{glyph} {number}{}{}",
                " ".repeat(INDENT),
                " ".repeat(num_pad),
                " ".repeat(TITLE_GAP)
            );
            if let Some(title) = pr.title.as_deref().filter(|t| !t.is_empty()) {
                let title = match pr.state {
                    PrState::Open => title.to_string(),
                    _ => style(title.to_string()).dim().to_string(),
                };
                left.push_str(&title);
            }
            // Reserve the action zone: clip the content to `inner - ACTION_ZONE`,
            // pad to `inner - 1` (so the column before the glyph is always a space),
            // then place the action glyph flush right.
            let left = super::clip_to_width(&left, inner.saturating_sub(ACTION_ZONE));
            let pad = inner
                .saturating_sub(1)
                .saturating_sub(console::measure_text_width(&left));
            let action = if pr.is_dismissed() { '↺' } else { '✕' };
            let action = style(action).dim().to_string();
            format!("{left}{}{action}", " ".repeat(pad))
        }
    }
}

/// Builds the pinned PR popup for a session's `prs`: the active PRs one per line
/// (glyph + `#<number>` link + title + hide/restore action), a `N 件非表示` toggle
/// footer when any are hidden, and the dismissed PRs beneath it while
/// `show_dismissed`. Stacked into a titled box ready to float beside the session's
/// row (see [`pr_popup_placement`]). Empty `prs` yields no box (the popup only shows
/// for a PR-bearing session), so the overlay is a no-op.
pub(in crate::presentation::tui::home) fn pr_popup_box(
    prs: &[PrLink],
    show_dismissed: bool,
) -> Vec<String> {
    let rows = popup_rows(prs, show_dismissed);
    if rows.is_empty() {
        return Vec::new();
    }
    let inner = pr_popup_inner(&rows);
    let lines: Vec<String> = rows.iter().map(|row| render_row(row, inner)).collect();
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
    let popup = pr_popup_box(&wt.pr, state.pr_popup_show_dismissed());
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
    /// The click landed on a PR's middle (the `#<number>`/title span): open its URL
    /// in the browser.
    Open(String),
    /// The click landed on a PR's glyph or action zone: set the PR (identified by
    /// [`PrLink::pr_key`]) to this state — toggle open↔merged, hide (dismiss), or
    /// restore a dismissed one to open.
    SetState { pr_key: String, state: PrState },
    /// The click landed on the `N 件非表示` footer: expand or collapse the dismissed
    /// PRs.
    ToggleDismissedView,
    /// The click landed inside the box but on no actionable token: keep it pinned.
    Inside,
    /// The click landed outside the box (or no popup is pinned): dismiss it.
    Outside,
}

/// Resolve a left click against the pinned PR popup down to the column zone it hit.
///
/// A PR row has three zones (mirroring [`render_row`]): the leading [`GLYPH_ZONE`]
/// toggles its state (open↔merged, or restore a dismissed one), the trailing
/// [`ACTION_ZONE`] hides/restores it, and the middle opens its URL. The `N 件非表示`
/// footer toggles the dismissed view. A click on the box's borders or padding keeps
/// it pinned ([`PopupClick::Inside`]); anywhere outside the box (or with no popup
/// pinned) dismisses it ([`PopupClick::Outside`]). The row → model mapping reuses
/// [`popup_rows`], so it stays in step with what was drawn.
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
    // Row `top` is the top border and the last row the bottom border; the content
    // rows sit between, indexed the same way `popup_rows` built them.
    let wt = state
        .list()
        .worktree_by_global_index(idx)
        .expect("the pinned index placement already resolved");
    let rows = popup_rows(&wt.pr, state.pr_popup_show_dismissed());
    let Some(model) = row.checked_sub(top + 1).and_then(|i| rows.get(i)) else {
        // The top or bottom border: keep the popup pinned.
        return PopupClick::Inside;
    };
    let pr = match model {
        PopupRow::Footer { .. } => return PopupClick::ToggleDismissedView,
        // A repo header is a label, not a target: keep the popup pinned.
        PopupRow::Repo(_) => return PopupClick::Inside,
        PopupRow::Pr(pr) => pr,
    };
    // The content spans `inner` columns starting two past the left border (border +
    // one pad space); a click on the border/pad falls outside `0..inner`.
    let inner = block_w.saturating_sub(4);
    let Some(content_col) = col.checked_sub(left + 2).filter(|&c| c < inner) else {
        return PopupClick::Inside;
    };
    if content_col < GLYPH_ZONE {
        PopupClick::SetState {
            pr_key: pr.pr_key().to_string(),
            state: glyph_target(pr.state),
        }
    } else if content_col >= inner.saturating_sub(ACTION_ZONE) {
        // Hide an active PR; restore a dismissed one.
        let state = if pr.is_dismissed() {
            PrState::Open
        } else {
            PrState::Dismissed
        };
        PopupClick::SetState {
            pr_key: pr.pr_key().to_string(),
            state,
        }
    } else {
        PopupClick::Open(pr.url.clone())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_repo_reads_owner_repo_before_pull_or_falls_back() {
        // The two path segments right before `/pull/<N>` are the group label.
        assert_eq!(pr_repo("https://github.com/o/r/pull/9"), "o/r");
        // A trailing `/files` (or any deeper path) does not change the owner/repo.
        assert_eq!(pr_repo("https://gh.corp/team/app/pull/1/files"), "team/app");
        // A URL without an owner/repo ahead of `/pull/` falls back to the whole
        // pre-`/pull/` portion rather than inventing a header.
        assert_eq!(pr_repo("weird"), "weird");
    }

    #[test]
    fn popup_rows_group_prs_under_their_repo_headers() {
        let a1 = PrLink::new(1, "https://github.com/o/a/pull/1");
        let a2 = PrLink::new(2, "https://github.com/o/a/pull/2");
        let b1 = PrLink::new(3, "https://github.com/o/b/pull/3");
        // A dismissed PR so the collapsed view also carries the `N 件非表示` footer.
        let mut hidden = PrLink::new(4, "https://github.com/o/b/pull/4");
        hidden.state = PrState::Dismissed;
        // Even interleaved, same-repo PRs collect under one header in first-seen
        // repo order: `o/a` (with #1 and #2) then `o/b` (with #3), then the footer.
        let prs = [a1, b1, a2, hidden];
        let rows = popup_rows(&prs, false);
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                PopupRow::Repo(r) => format!("repo:{r}"),
                PopupRow::Pr(p) => format!("pr:{}", p.number),
                PopupRow::Footer { .. } => "footer".to_string(),
            })
            .collect();
        assert_eq!(
            labels,
            vec!["repo:o/a", "pr:1", "pr:2", "repo:o/b", "pr:3", "footer"]
        );
    }
}
