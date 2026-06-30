//! The two-pane body: the worktree list (left) and the mode-dependent right
//! pane (a switch preview, the focus menu/prompt, or the embedded terminal).
//! All functions take plain data and return styled lines.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use console::{style, Style};

use super::super::command::{CommandInfo, Hint};
use super::super::state::{
    CreateInput, HomeState, LineKind, LogLine, Mode, Preview, RenameInput, WorktreeList, ROOT_NAME,
};
use super::super::terminal::tabs::TabStrip;
use super::super::terminal::view::TerminalView;
use super::{
    clip_to_width, clip_to_width_cow, pad_to_width, ACTIVE_COL, DETACHED, DIRTY_ICON,
    EMPTY_MESSAGE, HINT_INDENT, HINT_MAX, LOCAL_ICON, NAME_PREFIX, NEW_ICON, NOTE_ICON,
    PUSHED_ICON, RAIL_WIDTH, ROOT_DETAIL, STATUS_COL, SYNCED_ICON, TERMINAL_STARTING,
};
use crate::domain::resource::{Load, ResourceUsage};
use crate::domain::settings::{AgentCli, SessionActionUi, Sidebar};
use crate::domain::workspace_state::{AheadBehind, BranchStatus, DiffStat, PrLink, WorktreeState};
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Rgb, Span, SpanStyle};
use crate::presentation::tui::widgets;

/// The Nerd Font git glyph for a branch lifecycle status.
fn status_icon(status: BranchStatus) -> char {
    match status {
        BranchStatus::New => NEW_ICON,
        BranchStatus::Dirty => DIRTY_ICON,
        BranchStatus::Local => LOCAL_ICON,
        BranchStatus::Pushed => PUSHED_ICON,
        BranchStatus::Synced => SYNCED_ICON,
    }
}

/// The colour each branch lifecycle status is drawn in, shared by the full
/// sidebar's `<icon> <word>` label and the rail's single status glyph so they
/// never drift apart.
fn status_style(status: BranchStatus) -> Style {
    match status {
        BranchStatus::New => Style::new().blue(),
        BranchStatus::Dirty => Style::new().magenta(),
        BranchStatus::Local => Style::new().yellow(),
        BranchStatus::Pushed => Style::new().green(),
        BranchStatus::Synced => Style::new().cyan(),
    }
}

/// The colour-coded `<icon> <word>` label for a branch's lifecycle status. The
/// icon gives an at-a-glance read; the word keeps it legible without a Nerd
/// Font and disambiguates the colour.
pub(super) fn status_label(status: BranchStatus) -> String {
    let text = format!("{} {}", status_icon(status), status.as_str());
    status_style(status).apply_to(text).to_string()
}

/// The single colour-coded git-status glyph shown on the collapsed rail (the icon
/// from [`status_label`] without the word), in the same colour.
fn rail_status_glyph(status: BranchStatus) -> String {
    status_style(status)
        .apply_to(status_icon(status))
        .to_string()
}

/// The line-1 right-edge status field: the colour-coded `<icon> <word>` label
/// right-aligned within [`STATUS_COL`] columns, or all blanks when there is no
/// status (the root row).
pub(super) fn status_cell(status: Option<BranchStatus>) -> String {
    match status {
        None => " ".repeat(STATUS_COL),
        Some(status) => {
            let label = status_label(status);
            let pad = STATUS_COL.saturating_sub(console::measure_text_width(&label));
            format!("{}{label}", " ".repeat(pad))
        }
    }
}

/// The state of a session's embedded agent, shown by an icon on the row's first
/// line and spelled out on its detail line.
#[derive(Clone, Copy)]
enum AgentState {
    /// No live embedded session: the row carries no agent detail.
    Absent,
    /// A live session whose agent has started but not begun a turn yet — idle,
    /// awaiting the first prompt. Displayed as `☾ ready`.
    Ready,
    /// A live session whose agent is working a turn. Displayed as `▶ running`.
    Running,
    /// A live session whose agent paused mid-turn and awaits the user's input or
    /// permission. Displayed as `◆ waiting`.
    Waiting,
    /// A session whose agent has finished — a turn completed or its process
    /// exited; the bare shell it ran in may still be alive. Displayed as `✓ done`.
    Done,
}

impl AgentState {
    /// Pick the state from the live / running / waiting / done flags, in
    /// precedence order: done (the agent exited) wins over waiting, which wins
    /// over running, which wins over a plain live session (ready) — every state
    /// but [`Absent`](Self::Absent) is necessarily live.
    fn from_flags(live: bool, running: bool, waiting: bool, done: bool) -> Self {
        if done {
            AgentState::Done
        } else if waiting {
            AgentState::Waiting
        } else if running {
            AgentState::Running
        } else if live {
            AgentState::Ready
        } else {
            AgentState::Absent
        }
    }

    /// The detail-line content: an icon together with its label — `☾ ready`
    /// (dim), `▶ running` (green), `◆ waiting` (yellow), or `✓ done` (cyan) —
    /// clipped to `width`, or `None` when absent (the row has no agent in use).
    fn detail(self, width: usize) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(style(clip_to_width_cow("☾ ready", width)).dim().to_string()),
            AgentState::Running => Some(
                style(clip_to_width_cow("▶ running", width))
                    .green()
                    .bold()
                    .to_string(),
            ),
            AgentState::Waiting => Some(
                style(clip_to_width_cow("◆ waiting", width))
                    .yellow()
                    .bold()
                    .to_string(),
            ),
            AgentState::Done => Some(
                style(clip_to_width_cow("✓ done", width))
                    .cyan()
                    .bold()
                    .to_string(),
            ),
        }
    }

    /// The single, colour-matched glyph for the collapsed rail — the same icon
    /// the [`detail`](Self::detail) label leads with (`☾`/`▶`/`◆`/`✓`), or `None`
    /// when no agent is in use (the rail then falls back to the worktree's kind
    /// dot, so the row is never blank).
    fn rail_icon(self) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(style("☾").dim().to_string()),
            AgentState::Running => Some(style("▶").green().bold().to_string()),
            AgentState::Waiting => Some(style("◆").yellow().bold().to_string()),
            AgentState::Done => Some(style("✓").cyan().bold().to_string()),
        }
    }
}

/// The far-left gutter cell shared by both of a row's lines. In 切替 (Switch) the
/// keyboard is on the list, so the selected row shows a red `>` cursor. The
/// **active** session — the one subsequent commands operate on — is marked by a
/// green `▎` accent bar that runs down both of its lines (this replaces the old
/// `*` marker, whose meaning and mid-row position read poorly). Outside Switch
/// there is no cursor, so the gutter only ever carries the active bar; when the
/// cursor and the active row coincide in Switch, the cursor takes the column.
fn gutter_cell(selected: bool, active: bool, in_switch: bool) -> String {
    if in_switch && selected {
        style(">").red().bold().to_string()
    } else if active {
        style("▎").green().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// The branch / root name cell: clipped and padded to `width`, cyan, and bold
/// when the row is active or under the cursor.
fn name_cell(text: &str, width: usize, emphasised: bool) -> String {
    // Pad by *display* width, not char count: `format!("{:<width$}")` counts
    // `char`s, so a full-width (CJK) branch / session name — the app's own UI is
    // Japanese — would be padded to `width` chars (≈2×`width` columns), overrun
    // the cell, and shove the status column sideways. `pad_to_width` measures
    // display columns, matching the rest of the layout.
    let padded = pad_to_width(clip_to_width(text, width), width);
    if emphasised {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    }
}

/// Builds a row's second (detail) line: the row's `gutter` cell at the far-left
/// column (so the active accent bar runs down both lines), padded to sit under
/// the branch name, then the already-styled, already-clipped `detail`.
fn detail_line(gutter: &str, detail: String) -> String {
    let indent = " ".repeat(NAME_PREFIX - 1);
    format!("{gutter}{indent}{detail}")
}

/// Display width the CPU figure is left-padded to inside the `<cpu icon> … <mem
/// icon> …` label, so the memory column lands in the same place whether CPU reads
/// `0%` or `100%` — the CPU digit count never shifts MEM. Holds up to `100%`; a
/// rarer larger reading just nudges MEM right for that one line.
const CPU_LABEL_WIDTH: usize = 4;

/// Nerd Font glyphs labelling the CPU and memory figures on the resource line,
/// in place of spelling out `CPU` / `MEM` — the same icon-led style the git
/// status field uses. They need a patched [Nerd Font](https://www.nerdfonts.com/)
/// to render; without one the terminal shows a fallback box, but the number
/// beside each glyph still carries the meaning.
const CPU_ICON: char = '\u{f2db}'; // nf-fa-microchip — processor use
                                   // nf-fa-server — resident memory. Kept in the Font Awesome 4 range (like the git
                                   // status icons) so it renders on older/partial Nerd Fonts; the FA5 nf-fa-memory
                                   // (U+F538) is missing from those and shows a `?` fallback.
const MEM_ICON: char = '\u{f233}';

/// Rows every list entry (the root and each session) spans, fixed so the list
/// never reflows as a session goes live or idle: an identity line, a detail
/// line, and the CPU / memory line. Shared by the full sidebar, the collapsed
/// rail, and the click hit-tests so the renderer and the hit-tests never
/// disagree on where a session's rows are.
pub(super) const SESSION_ROWS: usize = 3;

/// Blank rows inserted between workspace groups in 統合(unite) mode. The gap is
/// pure decoration: it does not advance the flat selectable-row index and click
/// hit-tests skip over it.
const UNITE_WORKSPACE_GAP_ROWS: usize = 2;

/// The icon-led `<cpu> <pct>  <mem> <bytes>` resource label shared by a session's
/// resource line and the workspace total beside the mascot. The CPU figure leads
/// with [`CPU_ICON`] and the memory with [`MEM_ICON`] (in place of the words
/// `CPU` / `MEM`), and the CPU field is left-padded to [`CPU_LABEL_WIDTH`] so the
/// memory figure stays column-aligned both across rows and from frame to frame as
/// the percentages change.
pub(super) fn resource_inline_label(usage: ResourceUsage) -> String {
    format!(
        "{CPU_ICON} {cpu:<width$}  {MEM_ICON} {mem}",
        cpu = usage.format_cpu(),
        mem = usage.format_memory(),
        width = CPU_LABEL_WIDTH,
    )
}

/// The same icon-led resource label as [`resource_inline_label`], but with the
/// CPU and memory fields each **tinted by their own load band** — dim when calm,
/// yellow when busy, red when hot — so a heavy figure stands out beside the
/// mascot. Used for the workspace total only; the per-session rows stay uniformly
/// dim via [`resource_inline_label`].
pub(super) fn resource_inline_label_tinted(usage: ResourceUsage) -> String {
    let cpu = tint_by_load(
        format!(
            "{CPU_ICON} {cpu:<width$}",
            cpu = usage.format_cpu(),
            width = CPU_LABEL_WIDTH,
        ),
        usage.cpu_load(),
    );
    let mem = tint_by_load(
        format!("{MEM_ICON} {mem}", mem = usage.format_memory()),
        usage.memory_load(),
    );
    format!("{cpu}  {mem}")
}

/// Paint a resource field by its [`Load`] band: dim (calm), yellow (busy), or red
/// (hot), so the colour rises with the figure.
fn tint_by_load(field: String, load: Load) -> String {
    match load {
        Load::Calm => style(field).dim(),
        Load::Busy => style(field).yellow(),
        Load::Hot => style(field).red(),
    }
    .to_string()
}

/// Builds an entry's **third** line — the CPU / memory its process tree is using —
/// indented under the name like the detail line, with the row's `gutter` (so the
/// active accent bar runs down it too). Drawn icon-led ( `8%`  `120MB`,
/// the CPU / memory glyphs in place of the words) in dim text and clipped to the
/// cell. Every session draws this row at a fixed height, so an unsampled or idle
/// session reads `0%` / `0MB` (the caller passes a default usage) rather than
/// dropping the row and reflowing the list.
fn resource_line(
    usage: ResourceUsage,
    detail_width: usize,
    active: bool,
    in_switch: bool,
) -> String {
    let detail = style(clip_to_width(&resource_inline_label(usage), detail_width))
        .dim()
        .to_string();
    detail_line(&gutter_cell(false, active, in_switch), detail)
}

/// The line-1 cell between the session name and the right-edge git status: a
/// yellow [`NOTE_ICON`] when the session carries a note, else blank. Three
/// display columns wide either way (a leading and trailing space frame the
/// glyph) so the status field stays aligned whether or not a note is present —
/// it reuses the column the old active marker left blank.
fn note_cell(has_note: bool) -> String {
    if has_note {
        format!(" {} ", style(NOTE_ICON).yellow())
    } else {
        " ".repeat(ACTIVE_COL + 1)
    }
}

/// Builds a worktree's two lines. The far-left gutter carries a `>` cursor for
/// the selected entry in 切替 (Switch) or a green `▎` accent bar down the active
/// worktree's two lines; line 1 then has the freshness ("heat") kind dot
/// (`●`/`◐`/`○`, fading by time since the session was last touched, measured
/// against `now`), the branch name, a memo marker (`NOTE_ICON`, when `has_note`),
/// and the git `status` at the right edge. Line 2 is indented under the name and,
/// when an agent is in use, carries its icon + label (`☾ ready` / `▶ running` /
/// `◆ waiting` / `✓ done`).
#[allow(clippy::too_many_arguments)]
pub(super) fn worktree_row(
    worktree: &WorktreeState,
    label: &str,
    name_width: usize,
    detail_width: usize,
    cols: DetailCols,
    has_note: bool,
    now: DateTime<Utc>,
    selected: bool,
    active: bool,
    in_switch: bool,
    live: bool,
    running: bool,
    waiting: bool,
    done: bool,
) -> (String, String) {
    let kind = kind_dot(heat_of(worktree.updated_at, now));
    // The session's sidebar label (its custom display name, or the branch when
    // unset); a detached worktree with no label falls back to the placeholder.
    let name = if label.is_empty() {
        worktree.branch.as_deref().unwrap_or(DETACHED)
    } else {
        label
    };
    let branch = name_cell(name, name_width, active || selected);
    let status = status_cell(Some(worktree.status));
    let gutter = gutter_cell(selected, active, in_switch);
    // Three columns sit between the name and the right-edge status (the old
    // active-marker cell, now home to the memo marker — the active bar lives in
    // the gutter). The cell is a constant width whether or not a note is present,
    // so the status field never shifts.
    let note = note_cell(has_note);
    let line1 = format!("{gutter} {kind} {branch}{note}{status}");

    // Line 2 spells out the agent state with its icon (blank when absent) on the
    // left, and a right-aligned cluster of the freshness label (`Nmin ago`), the
    // commit-divergence marker (`↑N ↓M`), the `+N -M` diff badge, and the
    // `<icon> <count>` PR badge. Each field sits in a fixed-width column sized once per render (`cols`),
    // so a session's time, commits, diff, and PR always land in the same place no
    // matter how many changed lines or how long ago it was touched. Only the active
    // bar runs down to it — the `>` cursor stays a single point on line 1, so the
    // detail-line gutter ignores the cursor.
    let agent = AgentState::from_flags(live, running, waiting, done);
    let mut cells = Vec::new();
    if cols.time > 0 {
        cells.push(rpad(&relative_time(now, worktree.updated_at), cols.time));
    }
    if cols.ahead > 0 || cols.behind > 0 {
        cells.push(commits_cell(worktree.ahead_behind, cols.ahead, cols.behind));
    }
    if cols.added > 0 {
        cells.push(diff_cell(worktree.diff, cols.added, cols.removed));
    }
    if cols.pr > 0 {
        cells.push(pr_cell(&worktree.pr, cols.pr));
    }
    let detail = detail_content(agent, &cells, detail_width);
    let line2 = detail_line(&gutter_cell(false, active, in_switch), detail);
    (line1, line2)
}

/// A compact, dimmed freshness label for how long ago `then` was relative to
/// `now`: `now` under a minute, then `Nmin ago` / `Nh ago` / `Nd ago`. A `then`
/// in the future (clock skew) clamps to `now`. Shown on line 2 so a glance
/// tells the stale sessions from the freshly-touched ones.
fn relative_time(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    let label = if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    };
    style(label).dim().to_string()
}

/// Number of decimal digits in `n`, at least 1 (so `0` is one column wide). Used
/// to size the fixed-width diff / commit columns so every row's counts align.
fn digits(mut n: usize) -> usize {
    let mut d = 1;
    n /= 10;
    while n > 0 {
        n /= 10;
        d += 1;
    }
    d
}

/// The fixed-width column sizes for the detail line's right cluster, measured once
/// across the visible sessions (see [`detail_cols`]) so every row draws its
/// freshness, commit-divergence, and diff fields in the **same** columns — a
/// session's time / commits / `+N -M` never shifts because another row has more
/// changed lines or a longer "ago". A `0` width drops that field for the pane.
#[derive(Clone, Copy, Default)]
pub(super) struct DetailCols {
    /// Display width of the freshness (`Nmin ago`) cell; 0 drops it.
    time: usize,
    /// Digit width of the `↑N` (ahead) count; 0 = no visible session is ahead.
    ahead: usize,
    /// Digit width of the `↓N` (behind) count; 0 = no visible session is behind.
    behind: usize,
    /// Digit widths of the diff `+N` / `-M` counts; `added == 0` drops the badge.
    added: usize,
    removed: usize,
    /// Display width of the `<icon> <count>` PR badge (the glyph, a space, and the
    /// widest count's digits); 0 = no visible session has a PR, so the column is
    /// dropped.
    pr: usize,
}

/// Nerd Font glyph leading the pull-request badge — a git pull-request icon in
/// place of spelling out `PR`, the same icon-led style the git status and resource
/// fields use. Needs a patched [Nerd Font](https://www.nerdfonts.com/) to render;
/// without one the terminal shows a fallback box, but the count beside it still
/// carries the meaning.
pub(super) const PR_ICON: char = '\u{ea64}'; // nf-cod-git_pull_request

/// The fixed-width pull-request cell for a worktree's [`PrLink`]s: a single
/// `<icon> <count>` badge (bright blue, underlined to read as a link) — the PR glyph and
/// how many PRs the session carries — right-aligned in `width` display columns so
/// the badges line up down the list. Folding several PRs into one count keeps the
/// detail line from being crowded out by a long `#442 #447 …` run (the full list is
/// one badge click away; see [`pr_popup_placement`]). A row with no PR fills the
/// same width with blanks, holding the column. `width` is 0 (and the cell omitted)
/// when no visible session carries a PR.
fn pr_cell(prs: &[PrLink], width: usize) -> String {
    if prs.is_empty() {
        return " ".repeat(width);
    }
    let badge = style(format!("{PR_ICON} {}", prs.len()))
        .blue()
        .bright()
        .underlined()
        .to_string();
    rpad(&badge, width)
}

/// The display width the `<icon> <count>` PR badge occupies: the glyph, a space,
/// and the count's digits. `0` for no PR. Used to size the fixed
/// [`DetailCols::pr`] column.
fn pr_width(prs: &[PrLink]) -> usize {
    if prs.is_empty() {
        return 0;
    }
    // icon (1 column) + space + the count's digits.
    2 + digits(prs.len())
}

impl DetailCols {
    /// Width of the `↑N ↓M` commit cell — only the sides some visible session uses
    /// are reserved (a pane with nothing behind spends no columns on `↓`), with a
    /// one-space gap when both sides are present.
    fn commits_width(self) -> usize {
        let up = if self.ahead > 0 { 1 + self.ahead } else { 0 };
        let down = if self.behind > 0 { 1 + self.behind } else { 0 };
        up + usize::from(up > 0 && down > 0) + down
    }

    /// Width of the `+N -M` diff cell (`+`, added digits, space, `-`, removed
    /// digits), or 0 when no visible session carries a diff.
    fn badge_width(self) -> usize {
        if self.added > 0 {
            self.added + self.removed + 3
        } else {
            0
        }
    }

    /// Total width of the right cluster: every active field plus a one-space gap
    /// between each pair of adjacent fields.
    fn cluster_width(self) -> usize {
        let parts = [self.time, self.commits_width(), self.badge_width(), self.pr];
        let active = parts.iter().filter(|w| **w > 0).count();
        parts.iter().sum::<usize>() + active.saturating_sub(1)
    }
}

/// One visible session's inputs to [`detail_cols`]: when it was last touched, its
/// diff against the default, its commit divergence, and the display width of its
/// `<icon> <count>` PR badge (see [`pr_width`]).
type ClusterData = (DateTime<Utc>, Option<DiffStat>, Option<AheadBehind>, usize);

/// Measures the fixed [`DetailCols`] for a render: the widest freshness label, the
/// widest `↑` / `↓` counts, the widest `+` / `-` diff counts, and the widest PR
/// badge set across the `worktrees` (already trimmed to the rows that will be
/// drawn), then drops the low-priority columns — time first, then commits — until
/// the cluster fits beside the widest agent label (`max_agent_w`) within
/// `detail_width`. The diff badge and PR are always kept (they may instead clip a
/// long agent label, the established priority). Sizing once and handing the same
/// widths to every row is what stops the columns from wandering between sessions.
fn detail_cols(
    worktrees: &[ClusterData],
    now: DateTime<Utc>,
    max_agent_w: usize,
    detail_width: usize,
) -> DetailCols {
    let mut cols = DetailCols::default();
    for (updated_at, diff, ab, pr_w) in worktrees {
        cols.time = cols.time.max(console::measure_text_width(&relative_time(
            now,
            *updated_at,
        )));
        if let Some(diff) = diff {
            cols.added = cols.added.max(digits(diff.added));
            cols.removed = cols.removed.max(digits(diff.removed));
        }
        if let Some(ab) = ab {
            if ab.ahead > 0 {
                cols.ahead = cols.ahead.max(digits(ab.ahead));
            }
            if ab.behind > 0 {
                cols.behind = cols.behind.max(digits(ab.behind));
            }
        }
        // The widest `<icon> <count>` PR badge across the visible sessions.
        cols.pr = cols.pr.max(*pr_w);
    }
    // Trim low-priority columns (time, then commits) until the cluster fits beside
    // the widest agent label; the badge is always kept (it clips the agent first).
    let gap = usize::from(max_agent_w > 0);
    if max_agent_w + gap + cols.cluster_width() > detail_width {
        cols.time = 0;
    }
    if max_agent_w + gap + cols.cluster_width() > detail_width {
        cols.ahead = 0;
        cols.behind = 0;
    }
    cols
}

/// Right-aligns the already-styled `content` within `width` display columns by
/// left-padding with spaces, so a field seats at its column's right edge and the
/// edges line up down the list.
fn rpad(content: &str, width: usize) -> String {
    let pad = width.saturating_sub(console::measure_text_width(content));
    format!("{}{content}", " ".repeat(pad))
}

/// The `+N -M` diff cell for a worktree's [`DiffStat`] — additions green,
/// deletions red — laid out in fixed `added_w` / `removed_w` digit columns so the
/// `+` and `-` align down the list regardless of each session's change count. A
/// row with no diff fills the same width with blanks, holding the column.
fn diff_cell(diff: Option<DiffStat>, added_w: usize, removed_w: usize) -> String {
    match diff {
        Some(diff) => {
            let added = style(format!("+{:>added_w$}", diff.added)).green();
            let removed = style(format!("-{:>removed_w$}", diff.removed)).red();
            format!("{added} {removed}")
        }
        None => " ".repeat(added_w + removed_w + 3),
    }
}

/// The `↑N ↓M` commit-divergence cell for a worktree's [`AheadBehind`] — `↑N`
/// (ahead, cyan) the commits the branch has that the default lacks, `↓M` (behind,
/// magenta) the ones it lacks — in fixed `ahead_w` / `behind_w` digit columns so
/// the arrows line up. Only the sides the pane uses are drawn; a side this row is
/// even on fills its width with blanks, holding the column. An empty side reads as
/// blanks rather than `↑0` / `↓0`.
fn commits_cell(ab: Option<AheadBehind>, ahead_w: usize, behind_w: usize) -> String {
    let ahead = ab.map_or(0, |ab| ab.ahead);
    let behind = ab.map_or(0, |ab| ab.behind);
    let up = (ahead_w > 0).then(|| {
        if ahead > 0 {
            style(format!("↑{ahead:>ahead_w$}")).cyan().to_string()
        } else {
            " ".repeat(1 + ahead_w)
        }
    });
    let down = (behind_w > 0).then(|| {
        if behind > 0 {
            style(format!("↓{behind:>behind_w$}")).magenta().to_string()
        } else {
            " ".repeat(1 + behind_w)
        }
    });
    match (up, down) {
        (Some(up), Some(down)) => format!("{up} {down}"),
        (Some(side), None) | (None, Some(side)) => side,
        (None, None) => String::new(),
    }
}

/// Compose the detail line: the `agent` state label on the left and the
/// right-aligned `cells` cluster (each already a fixed-width column, in display
/// order `time commits badge`) flush to the cell's right edge, joined by single
/// spaces, within `width`. The cluster keeps the right edge; the agent label is
/// clipped to the room left of it (the badge can thus clip a long agent label, the
/// established rule). With no cells the agent label fills the cell; when the
/// cluster alone overflows it is clipped to the cell.
fn detail_content(agent: AgentState, cells: &[String], width: usize) -> String {
    if cells.is_empty() {
        return agent.detail(width).unwrap_or_default();
    }
    let cluster = cells.join(" ");
    let cluster_w = console::measure_text_width(&cluster);
    if cluster_w >= width {
        // No room for both: the cluster alone, clipped to the cell.
        return clip_to_width(&cluster, width);
    }
    // Reserve the cluster's columns (plus a one-space gap) and clip the agent
    // label to what's left, so it is styled already-clipped (clean ANSI) rather
    // than truncated after the fact.
    let agent = agent.detail(width - cluster_w - 1).unwrap_or_default();
    let pad = width - console::measure_text_width(&agent) - cluster_w;
    format!("{agent}{}{cluster}", " ".repeat(pad))
}

/// Builds the root's two lines: the workspace itself, belonging to no session.
/// The far-left gutter carries the `>` cursor (in 切替 (Switch)) or the green `▎`
/// active bar; line 1 then has a `⌂` kind icon, the [`ROOT_NAME`] label, a memo
/// marker (`NOTE_ICON`, when `has_note`) — the root carries its own note, like a
/// session — and a blank status field (the root has no git status). Line 2
/// carries a `workspace root` detail.
pub(super) fn root_row(
    name_width: usize,
    detail_width: usize,
    has_note: bool,
    selected: bool,
    active: bool,
    in_switch: bool,
) -> (String, String) {
    let kind = root_glyph();
    let name = name_cell(ROOT_NAME, name_width, active || selected);
    let status = status_cell(None);
    let gutter = gutter_cell(selected, active, in_switch);
    // The same constant-width memo cell a worktree row uses, so the (blank) status
    // field stays aligned with the sessions below whether or not a note is present.
    let note = note_cell(has_note);
    let line1 = format!("{gutter} {kind} {name}{note}{status}");

    // Only the active bar reaches line 2; the cursor stays a point on line 1.
    let detail = style(clip_to_width(ROOT_DETAIL, detail_width))
        .dim()
        .to_string();
    let line2 = detail_line(&gutter_cell(false, active, in_switch), detail);
    (line1, line2)
}

/// A session's freshness, derived from how long ago it was last touched —
/// switched to, or seen producing terminal/agent activity. Drives the sidebar
/// kind dot's glyph and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heat {
    /// Touched within the last [`HEAT_FRESH`].
    Fresh,
    /// Touched within the last [`HEAT_WARM`] (but not [`HEAT_FRESH`]).
    Warm,
    /// Touched longer ago than [`HEAT_WARM`], or never since creation.
    Cold,
}

/// A session touched more recently than this reads as [`Heat::Fresh`].
const HEAT_FRESH_MINUTES: i64 = 15;
/// A session touched more recently than this — but not [`HEAT_FRESH_MINUTES`] —
/// reads as [`Heat::Warm`]; anything older is [`Heat::Cold`].
const HEAT_WARM_HOURS: i64 = 4;

/// Classify a session's freshness from its last-active time and the current
/// time. A negative age (a clock that went backwards) is treated as fresh, the
/// safe side — a session is never shown colder than it is.
fn heat_of(last_active: DateTime<Utc>, now: DateTime<Utc>) -> Heat {
    let age = now.signed_duration_since(last_active);
    if age < Duration::minutes(HEAT_FRESH_MINUTES) {
        Heat::Fresh
    } else if age < Duration::hours(HEAT_WARM_HOURS) {
        Heat::Warm
    } else {
        Heat::Cold
    }
}

/// The session's freshness ("heat") dot: fresh `●` (green), warm `◐`, or cold
/// `○` (dim) — the time since the session was last touched, oldest fading out.
fn kind_dot(heat: Heat) -> String {
    match heat {
        Heat::Fresh => style("●").green().to_string(),
        Heat::Warm => style("◐").to_string(),
        Heat::Cold => style("○").dim().to_string(),
    }
}

/// The workspace root's kind glyph (`⌂`, magenta) — shown in the slot where a
/// worktree shows its [`kind_dot`], by both the full sidebar ([`root_row`]) and
/// the collapsed rail ([`rail_pane`]).
fn root_glyph() -> String {
    style("⌂").magenta().to_string()
}

/// Builds one collapsed-rail **entry** as the same [`SESSION_ROWS`] lines a
/// full-sidebar entry spans, so toggling the sidebar never moves a session to a
/// different row (no layout shift) — only the width changes. The glyphs form a
/// 2×2 grid beside the gutter, and a third (blank) row matches the full sidebar's
/// resource line — the narrow rail has no room for a CPU / memory figure, so it
/// keeps the row's height without the number:
///
/// ```text
/// ▎ <kind> <git>     row 1: identity dot (⌂/●/○) + git-status glyph
/// ▎       <agent>    row 2: agent-state glyph (▶/◆/☾/✓), under the git column
/// ▎                  row 3: blank (the full sidebar's resource line has no rail twin)
/// ```
///
/// `git` is blank on the root (no git status); `agent` is blank when no agent is
/// in use. The active `▎` bar runs down all three rows; the 切替 `>` cursor stays
/// a point on row 1, matching the full sidebar.
fn rail_entry(
    selected: bool,
    active: bool,
    in_switch: bool,
    kind: &str,
    git: Option<&str>,
    agent: Option<&str>,
) -> (String, String, String) {
    let gutter = gutter_cell(selected, active, in_switch);
    let bar = gutter_cell(false, active, in_switch);
    // Columns: gutter @0, kind @2, git/agent @4 — so the agent glyph sits under
    // the git glyph and the column under the kind dot stays blank.
    let top = pad_to_width(
        format!("{gutter} {kind} {}", git.unwrap_or(" ")),
        RAIL_WIDTH,
    );
    let detail = pad_to_width(format!("{bar}   {}", agent.unwrap_or(" ")), RAIL_WIDTH);
    // The resource row's rail twin: the active bar runs down it, but the rail has
    // no room for the CPU / memory figure, so the rest is blank.
    let resource = pad_to_width(bar, RAIL_WIDTH);
    (top, detail, resource)
}

fn push_unite_workspace_gap(lines: &mut Vec<String>, width: usize) {
    for _ in 0..UNITE_WORKSPACE_GAP_ROWS {
        lines.push(pad_to_width(String::new(), width));
    }
}

fn line_hits_unite_workspace_gap(line: usize, cur: &mut usize) -> bool {
    if line < *cur + UNITE_WORKSPACE_GAP_ROWS {
        return true;
    }
    *cur += UNITE_WORKSPACE_GAP_ROWS;
    false
}

fn group_block_rows(list: &WorktreeList, group_index: usize, worktree_count: usize) -> usize {
    let united = list.group_count() > 1;
    let gap = usize::from(united && group_index > 0) * UNITE_WORKSPACE_GAP_ROWS;
    let header = usize::from(united);
    let body = if worktree_count == 0 {
        1
    } else {
        SESSION_ROWS * worktree_count
    };
    gap + header + 2 + 1 + body
}

/// Builds the collapsed-rail sidebar ([`Sidebar::Rail`]): the root entry first, a
/// divider, then one entry per worktree — each the same two rows as the full
/// sidebar (kind glyph on row 1, agent state on row 2), so the rail and the full
/// list share the exact same row layout and toggling between them only changes
/// the width. The active session keeps its green `▎` gutter bar (down both rows)
/// and, in 切替, the `>` cursor and the dimming of the other entries, so the rail
/// still shows which session is selected, its git state, and what its agent is
/// doing without spelling out their names.
#[allow(clippy::too_many_arguments)]
fn rail_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
    rows: usize,
    in_switch: bool,
    now: DateTime<Utc>,
) -> Vec<String> {
    let root = root_glyph();
    let mut lines: Vec<String> = Vec::new();
    // The flat selectable-row index, matching `WorktreeList`'s row space (the
    // 統合 group separators are pure decoration and do not advance it).
    let mut flat_row = 0usize;
    let united = list.group_count() > 1;
    'groups: for (g, group) in list.groups().iter().enumerate() {
        if lines.len() >= rows {
            break;
        }
        // In 統合(unite) mode two blank rows separate each workspace's block.
        if united && g > 0 {
            push_unite_workspace_gap(&mut lines, RAIL_WIDTH);
        }
        // The root entry is two rows (then a divider), matching the full sidebar's
        // [`root_row`]; only worktree entries carry the third resource row, so the
        // root drops the rail entry's (blank) third line.
        let (mut root_top, mut root_detail, _) = rail_entry(
            flat_row == list.selected_index(),
            flat_row == list.active_index(),
            in_switch,
            &root,
            None,
            None,
        );
        if in_switch && flat_row != list.selected_index() {
            root_top = dim_row(&root_top);
            root_detail = dim_row(&root_detail);
        }
        lines.push(root_top);
        lines.push(root_detail);
        flat_row += 1;
        lines.push(style("─".repeat(RAIL_WIDTH)).dim().to_string());
        if group.worktrees().is_empty() {
            // Mirror the full sidebar's single empty-message row so the row count
            // matches and toggling never shifts the layout.
            lines.push(pad_to_width(String::new(), RAIL_WIDTH));
            continue;
        }
        for w in group.worktrees() {
            // Stop once the built rows fill the rail: the trailing `truncate(rows)`
            // discards the rest, so building beyond the visible height is wasted
            // work (same bound as the full sidebar above).
            if lines.len() >= rows {
                break 'groups;
            }
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let kind = kind_dot(heat_of(w.updated_at, now));
            let git = rail_status_glyph(w.status);
            let agent = AgentState::from_flags(
                live.contains(&w.path),
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
            )
            .rail_icon();
            let (mut top, mut detail, mut resource) = rail_entry(
                selected,
                active,
                in_switch,
                &kind,
                Some(&git),
                agent.as_deref(),
            );
            if in_switch && !selected {
                top = dim_row(&top);
                detail = dim_row(&detail);
                resource = dim_row(&resource);
            }
            lines.push(top);
            lines.push(detail);
            lines.push(resource);
            flat_row += 1;
        }
    }
    lines.truncate(rows);
    lines
}

/// Re-renders an already-styled row uniformly dimmed: strips its colours and
/// wraps the plain text in `dim`. Used to fade the rows the cursor is *not* on
/// in 切替 (Switch), so the highlighted session stands out without a box.
pub(super) fn dim_row(line: &str) -> String {
    // `strip_ansi_codes` borrows the input when it carries no escapes (the common
    // case for a plain session row), so styling the `Cow` directly avoids the
    // extra owned copy `into_owned` would force before the single styled string is
    // built.
    style(console::strip_ansi_codes(line)).dim().to_string()
}

/// The flat selectable row (root rows included, matching `WorktreeList`'s row
/// space) a 0-based body `line` lands on, or `None` for a group header, a divider,
/// a unite workspace gap, an empty-workspace message, or a line past the last
/// group. Replays the exact layout [`left_pane`] / [`rail_pane`] build so a click
/// maps back to its row without the renderer and the hit test ever disagreeing.
///
/// `with_headers` matches the full sidebar (which heads each 統合(unite) group
/// with its name); the rail draws no header, so it walks the same layout minus
/// that one line per group.
fn sidebar_row_at_line_walk(list: &WorktreeList, line: usize, with_headers: bool) -> Option<usize> {
    let united = list.group_count() > 1;
    let mut cur = 0usize; // body line being walked
    let mut flat = 0usize; // flat selectable-row index
    for (g, group) in list.groups().iter().enumerate() {
        // The visual gap between workspace blocks in unite mode.
        if united && g > 0 && line_hits_unite_workspace_gap(line, &mut cur) {
            return None;
        }
        // The unite group header — only the full sidebar draws it.
        if with_headers && united {
            if line == cur {
                return None;
            }
            cur += 1;
        }
        // The root entry spans two rows, then a one-row divider.
        if line == cur || line == cur + 1 {
            return Some(flat);
        }
        cur += 2;
        flat += 1;
        if line == cur {
            return None; // the divider
        }
        cur += 1;
        if group.worktrees().is_empty() {
            if line == cur {
                return None; // the empty-workspace message
            }
            cur += 1;
            continue;
        }
        for _ in group.worktrees() {
            if line >= cur && line < cur + SESSION_ROWS {
                return Some(flat);
            }
            cur += SESSION_ROWS;
            flat += 1;
        }
    }
    None
}

pub(super) fn sidebar_row_at_line_for_sidebar(
    list: &WorktreeList,
    line: usize,
    sidebar: Sidebar,
) -> Option<usize> {
    match sidebar {
        Sidebar::Full => sidebar_row_at_line_walk(list, line, true),
        Sidebar::Rail => sidebar_row_at_line_walk(list, line, false),
    }
}

/// The 0-based body line just past `group`'s block in the layout [`left_pane`]
/// builds — its (optional unite) gap and header, the two-row root entry, the
/// divider, and then either the empty-workspace message or [`SESSION_ROWS`] rows
/// per session. 切替's inline create / rename input is spliced in here so it
/// renders within the targeted workspace's block (after that workspace's
/// sessions, before the next group's gap/header) rather than at the foot of the
/// whole column — which matters in 統合(unite) mode where several workspaces
/// stack. Walks the same layout as [`sidebar_row_at_line_for_sidebar`].
pub(super) fn group_inline_insert_line(list: &WorktreeList, group: usize) -> usize {
    list.groups()
        .iter()
        .enumerate()
        .take(group + 1)
        .map(|(i, g)| group_block_rows(list, i, g.worktrees().len()))
        .sum()
}

/// Builds a 統合(unite) group header: the workspace name in bold behind a left
/// bar, clipped to the sidebar width. Drawn above each workspace's rows only when
/// more than one workspace is shown, so single-workspace mode is byte-for-byte
/// unchanged.
fn group_header(name: &str, width: usize) -> String {
    style(clip_to_width(&format!("▌ {name}"), width))
        .bold()
        .to_string()
}

/// Builds the left pane: the root entry (two lines) first, then a divider, then
/// one [`SESSION_ROWS`]-line entry per worktree — an identity line, a detail
/// line, and a CPU / memory line (`CPU 0%  MEM 0MB` when unsampled, so the entry
/// is a fixed height and the list never reflows) — or the empty message when none
/// are recorded, trimmed to the available `rows`. `live` holds
/// the worktree paths with an embedded session (a live-but-idle one shows
/// `☾ ready`), `running` the ones working a turn (`▶ running`), `waiting` the
/// ones whose agent awaits input (`◆ waiting`), and `done` the finished ones
/// (`✓ done`); precedence is done > waiting > running > ready. When `in_switch`
/// is set (in 切替), the keyboard is on the list: the selected row shows a `>`
/// cursor and every other row is faded so the highlighted session reads first.
///
/// When `sidebar` is [`Sidebar::Rail`] the list collapses to the compact rail
/// ([`rail_pane`]) instead, and `left_w` is the rail width.
#[allow(clippy::too_many_arguments)]
pub(super) fn left_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
    resources: &HashMap<PathBuf, ResourceUsage>,
    left_w: usize,
    rows: usize,
    in_switch: bool,
    sidebar: Sidebar,
    now: DateTime<Utc>,
) -> Vec<String> {
    if sidebar == Sidebar::Rail {
        // The 5-column rail has no room for a CPU / memory figure, so the rail
        // shows only the agent glyph; the resource line belongs to the full list.
        return rail_pane(list, live, running, waiting, done, rows, in_switch, now);
    }
    // Line 1: prefix + name + the (now-blank) active-marker cell + a space + the
    // right-edge status field.
    let name_width = left_w.saturating_sub(NAME_PREFIX + ACTIVE_COL + 1 + STATUS_COL);
    // Line 2: indented under the branch name, then the detail text.
    let detail_width = left_w.saturating_sub(NAME_PREFIX);
    // Size the detail line's right-cluster columns once across every group's
    // worktrees, so a field lands in the same column on every row regardless of
    // which workspace (group) it belongs to.
    let mut max_agent_w = 0;
    let cluster_data: Vec<_> = list
        .groups()
        .iter()
        .flat_map(|g| g.worktrees())
        .map(|w| {
            let agent = AgentState::from_flags(
                live.contains(&w.path),
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
            );
            if let Some(label) = agent.detail(detail_width) {
                max_agent_w = max_agent_w.max(console::measure_text_width(&label));
            }
            (w.updated_at, w.diff, w.ahead_behind, pr_width(&w.pr))
        })
        .collect();
    let cols = detail_cols(&cluster_data, now, max_agent_w, detail_width);

    // A divider separating each workspace root from its sessions — indented to
    // start under the `root` label (past the cursor and kind-icon cells).
    let indent = " ".repeat(NAME_PREFIX);
    let inner_w = left_w.saturating_sub(NAME_PREFIX);
    // In 統合(unite) mode each workspace's rows are headed by its name.
    let united = list.group_count() > 1;

    let mut lines: Vec<String> = Vec::new();
    // The flat selectable-row index, matching `WorktreeList`'s row space (group
    // headers are pure decoration and do not advance it).
    let mut flat_row = 0usize;
    'groups: for (g, group) in list.groups().iter().enumerate() {
        if lines.len() >= rows {
            break;
        }
        if united && g > 0 {
            push_unite_workspace_gap(&mut lines, left_w);
        }
        if united {
            lines.push(group_header(group.name(), left_w));
        }
        let (mut root_top, mut root_detail) = root_row(
            name_width,
            detail_width,
            group.root_has_note(),
            flat_row == list.selected_index(),
            flat_row == list.active_index(),
            in_switch,
        );
        if in_switch && flat_row != list.selected_index() {
            root_top = dim_row(&root_top);
            root_detail = dim_row(&root_detail);
        }
        lines.push(root_top);
        lines.push(root_detail);
        flat_row += 1;
        lines.push(
            style(format!("{indent}{}", "─".repeat(inner_w)))
                .dim()
                .to_string(),
        );
        if group.worktrees().is_empty() {
            // No sessions yet in this workspace — show the empty message under the
            // divider.
            lines.push(
                style(format!("{indent}{}", clip_to_width(EMPTY_MESSAGE, inner_w)))
                    .dim()
                    .to_string(),
            );
            continue;
        }
        for (i, w) in group.worktrees().iter().enumerate() {
            // Stop once the rows built already fill the pane: the trailing
            // `truncate(rows)` would discard anything past this, so building it is
            // wasted work. With many sessions open this bounds the per-frame cost
            // (styling, dimming, ANSI rewriting) to the visible rows.
            if lines.len() >= rows {
                break 'groups;
            }
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let (mut top, mut detail) = worktree_row(
                w,
                group.display_label(i),
                name_width,
                detail_width,
                cols,
                group.has_note(i),
                now,
                selected,
                active,
                in_switch,
                live.contains(&w.path),
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
            );
            // Every session draws a third CPU / memory line at a fixed height, so
            // the list never reflows as a session goes live or idle. An unsampled
            // session shows `CPU 0%  MEM 0MB` (a default usage) rather than dropping
            // the row.
            let usage = resources.get(&w.path).copied().unwrap_or_default();
            let mut resource = resource_line(usage, detail_width, active, in_switch);
            if in_switch && !selected {
                top = dim_row(&top);
                detail = dim_row(&detail);
                resource = dim_row(&resource);
            }
            lines.push(top);
            lines.push(detail);
            lines.push(resource);
            flat_row += 1;
        }
    }
    lines.truncate(rows);
    lines
}

/// Renders one log line, coloured by kind. Command lines get a `❯` prompt.
pub(super) fn log_line(line: &LogLine, width: usize) -> String {
    let raw = match line.kind {
        LineKind::Command => format!("❯ {}", line.text),
        _ => line.text.clone(),
    };
    let clipped = clip_to_width(&raw, width);
    match line.kind {
        LineKind::Command => style(clipped).cyan().bold().to_string(),
        LineKind::Output => clipped,
        LineKind::Error => style(clipped).red().to_string(),
        LineKind::Notice => style(clipped).yellow().to_string(),
    }
}

/// Builds the tab strip's two raw (unclipped) rows: one ` N label ` chip per
/// pane (the active one reversed and bold, the rest dimmed) and the underline
/// marker beneath the active chip. Each chip is numbered (1-based) to match the
/// `←`/`→` tab order. The rows are laid beside the preview header on a shared row
/// by [`header_tab_rows`], which re-indents the marker to stay under the chips.
///
/// The chip text and the [`TAB_CHIP_GAP`] between chips are the single source of
/// truth for the strip's layout: [`tab_chip_ranges`] reconstructs the on-screen
/// column of each chip from the same recipe so a click can be mapped back to its
/// tab (没入 switches tabs on a click; see [`attached_tab_at`]).
fn tab_strip_parts(strip: &TabStrip) -> (String, String) {
    let mut chips = String::new();
    let mut marker = String::new();
    for (i, label) in strip.labels.iter().enumerate() {
        if i > 0 {
            chips.push_str(&" ".repeat(TAB_CHIP_GAP));
            marker.push_str(&" ".repeat(TAB_CHIP_GAP));
        }
        let text = tab_chip_text(i, label);
        // Display width (not char count) so the underline marker stays aligned
        // under a non-ASCII chip label, matching the hit test in
        // [`tab_chip_ranges`], which measures the same chip the same way.
        let width = console::measure_text_width(&text);
        if i == strip.active {
            chips.push_str(&style(&text).reverse().bold().to_string());
            marker.push_str(&style("▔".repeat(width)).cyan().bold().to_string());
        } else {
            chips.push_str(&style(&text).dim().to_string());
            marker.push_str(&" ".repeat(width));
        }
    }
    (chips, marker)
}

/// The divider drawn between the fixed-width header identity and the tab strip,
/// so the session's identity (name / status / agent) reads as a distinct block
/// from its tabs. It reuses the pane divider glyph ([`SEP`](super::SEP)), dimmed.
const HEADER_TAB_DIVIDER: &str = " │ ";

/// Lays the preview `header` (the fixed-width identity from [`preview_header`])
/// and the pane tab strip on a single row: the identity, a dim divider, then the
/// numbered chips, with the active-tab underline marker on the row below
/// re-indented to sit under the chips. Because the identity is a constant width,
/// the divider and the chips land in the same column whichever session is shown,
/// so the row does not jitter as the 切替 cursor moves between sessions. With no
/// `strip` (or an empty one) the identity stands alone on one row. Used by both
/// the 切替 (Switch) preview and 没入 (Attached).
pub(super) fn header_tab_rows(
    header: String,
    strip: Option<&TabStrip>,
    width: usize,
) -> Vec<String> {
    let Some(strip) = strip.filter(|s| !s.labels.is_empty()) else {
        return vec![clip_to_width(&header, width)];
    };
    let (chips, marker) = tab_strip_parts(strip);
    let divider = style(HEADER_TAB_DIVIDER).dim().to_string();
    // Push the marker right past the identity and the divider so it lands under
    // the chips on the row above. The identity is a fixed width, so this indent
    // is the same for every session.
    let indent = tab_strip_indent(&header);
    vec![
        clip_to_width(&format!("{header}{divider}{chips}"), width),
        clip_to_width(&format!("{}{marker}", " ".repeat(indent)), width),
    ]
}

/// Gap, in columns, between two chips on the strip's top row (and under it on the
/// marker row), so the chips read as separate tabs without a hard separator glyph.
const TAB_CHIP_GAP: usize = 2;

/// One chip's text: a leading space, the 1-based tab number, the pane `label`, and
/// a trailing space — ` N label `. The single recipe both the renderer
/// ([`tab_strip_parts`]) and the hit test ([`tab_chip_ranges`]) build from.
fn tab_chip_text(index: usize, label: &str) -> String {
    format!(" {} {label} ", index + 1)
}

/// The column the chips begin at, measured from the right pane's left edge: past
/// the fixed-width identity `header` and the [`HEADER_TAB_DIVIDER`]. Matches the
/// indent [`header_tab_rows`] lays the chips at, so [`tab_chip_ranges`] places
/// them where they are actually drawn.
fn tab_strip_indent(header: &str) -> usize {
    console::measure_text_width(header) + HEADER_TAB_DIVIDER.chars().count()
}

/// The column range each tab chip occupies on the strip, measured from the right
/// pane's left edge — the [`tab_strip_indent`], then one [`tab_chip_text`] chip
/// per pane with a [`TAB_CHIP_GAP`] between. Reconstructs the layout
/// [`tab_strip_parts`] / [`header_tab_rows`] draw so a click column can be mapped
/// to the tab under it (see [`attached_tab_at`]).
fn tab_chip_ranges(header: &str, strip: &TabStrip) -> Vec<std::ops::Range<usize>> {
    let mut col = tab_strip_indent(header);
    let mut ranges = Vec::with_capacity(strip.labels.len());
    for (i, label) in strip.labels.iter().enumerate() {
        if i > 0 {
            col += TAB_CHIP_GAP;
        }
        let width = console::measure_text_width(&tab_chip_text(i, label));
        ranges.push(col..col + width);
        col += width;
    }
    ranges
}

/// The tab a left click at the 0-based screen (`col`, `row`) lands on while 没入
/// (Attached), or `None` when the click is not on a switchable chip. The strip
/// occupies the [`TAB_BAR_ROWS`](super::TAB_BAR_ROWS) rows at the top of the right
/// pane — the embedded terminal `geo` is pushed down by exactly that — so a click
/// on either of those rows, in a chip's column, hits its tab. Returns `None` for a
/// click off the strip rows, off every chip (the indent, the gaps, past the last
/// chip), or on the already-active tab, so the caller only switches on a real
/// change. Mirrors what [`right_pane_contents`] draws for [`Mode::Attached`].
pub(in crate::presentation::tui::home) fn attached_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    geo: super::TerminalGeometry,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    // The strip's rows are the `TAB_BAR_ROWS` just above the terminal body.
    let strip_top = geo.origin_row.checked_sub(super::TAB_BAR_ROWS as u16)?;
    if row < strip_top || row >= geo.origin_row {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let header = active_session_header(state);
    let target = tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))?;
    // A click on the active tab is a no-op: leave it to the caller's selection
    // handling rather than re-driving the same pane.
    (target != strip.active).then_some(target)
}

/// The live-pane tab (0-based, matching [`TabStrip::labels`]) a left click at the
/// 0-based screen (`col`, `row`) lands on while 在席 (Focus), or `None` when the
/// click is not on a changeable pane tab.
///
/// 在席 draws the same two-row header/tab block as 没入 at the top of the right
/// pane, but the terminal body is only a preview and the selector may also sit on
/// the trailing `+ new` tab. The `+ new` chip is only rendered while it is the
/// selected tab, so a click can never land on it (clicking the active tab is a
/// no-op); only the live pane chips are selectable here. This hit-test
/// reconstructs that rendered strip so the event loop can make right-pane pane
/// tabs mouse-selectable, mirroring the keyboard `Ctrl-N` / `Ctrl-P` path.
pub(in crate::presentation::tui::home) fn focus_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?.clone();
    if strip.labels.is_empty() {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let mut labels = strip.labels.clone();
    let active = if state.focus_on_new_tab() {
        labels.push(FOCUS_NEW_TAB_LABEL.to_string());
        labels.len().saturating_sub(1)
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let target = tab_chip_ranges(&header, &combined)
        .into_iter()
        .position(|range| range.contains(&rel_col))?;
    // Clicking the active tab — including the appended `+ new` chip, which only
    // shows while selected — is a no-op; every other hit is a live pane chip.
    (target != combined.active).then_some(target)
}

/// For the full sidebar, each worktree's global index (across every group, root
/// rows excluded) paired with the 0-based body line its [`SESSION_ROWS`] entry
/// starts on. Walks the same layout [`left_pane`] builds — in single-workspace
/// mode and 統合(unite) mode (the [`UNITE_WORKSPACE_GAP_ROWS`]-row gap and the
/// one-row group header before each later workspace, the two-row root entry, the
/// divider, then either the empty-workspace message or the worktree rows) — so
/// the PR badge hit-test and popup anchor agree with what is drawn without ever
/// drifting from the renderer. The global index is what the PR popup pins, so a
/// badge in any workspace (not just the first group) can open its popup.
fn full_sidebar_worktree_entries(list: &WorktreeList) -> Vec<(usize, usize)> {
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
        if group.worktrees().is_empty() {
            cur += 1; // the empty-workspace message
            continue;
        }
        for _ in group.worktrees() {
            out.push((global, cur));
            cur += SESSION_ROWS;
            global += 1;
        }
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
/// begins at row [`BODY_TOP`] (below the title bar, mode ladder, and blank
/// separator) and is [`super::body_rows_for`] rows tall; the left pane is the
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
    let line = (row - BODY_TOP) as usize;
    if line >= super::body_rows_for(height) {
        return None;
    }
    // The badge only lives on each entry's detail line.
    let (idx, _) = full_sidebar_worktree_entries(state.list())
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
const PR_POPUP_INNER: usize = 28;

/// The popup box's title, embedded in its top border by [`widgets::boxed`] as
/// `─ PR `. The box must stay at least this wide so the title keeps its closing
/// frame instead of butting against the corner.
const PR_POPUP_TITLE: &str = "PR";

/// Greedily packs a session's `prs` into the popup's rows: each `#<number>` token
/// is `#` + its digits wide, joined by a one-space gap, and a row never grows past
/// [`PR_POPUP_INNER`]. Shared by the popup's renderer ([`pr_popup_box`]) and its
/// click hit-test ([`pr_popup_click`]) so they agree on which token sits where.
fn pr_popup_pack(prs: &[PrLink]) -> Vec<Vec<&PrLink>> {
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
fn pr_popup_inner(rows: &[Vec<&PrLink>]) -> usize {
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
/// (bright blue, underlined), space-joined and wrapped to [`PR_POPUP_INNER`]
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
                        .blue()
                        .bright()
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
    let (_, entry_line) = full_sidebar_worktree_entries(state.list())
        .into_iter()
        .find(|&(global, _)| global == idx)?;
    // `render_frame` overlays the box while `lines` holds only the chrome above the
    // body (`BODY_TOP` rows) and the body itself, so the anchor clamps against that
    // same length — and the left edge against the width — exactly as `overlay_at`.
    let base_len = BODY_TOP as usize + super::body_rows_for(height);
    let raw_top = BODY_TOP as usize + entry_line;
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
const DETAIL_LINE: usize = 1;

/// The 0-based screen row the two-pane body begins at, matching the title bar,
/// mode ladder, and blank separator [`super::render_frame`] stacks above it (and
/// the `origin_row` of [`super::terminal_geometry`]).
const BODY_TOP: u16 = 3;

/// Lines the left pane spends before the first worktree row: the root entry (two
/// rows) and the divider beneath it. Worktree `i` then occupies the
/// [`SESSION_ROWS`] lines starting at `ROOT_ENTRY_LINES + SESSION_ROWS * i`.
pub(super) const ROOT_ENTRY_LINES: usize = 3;

/// Column widths for the fixed-width header identity. The session name is clipped
/// and every field is padded to a constant width, so the identity block is the
/// same size for every session: a long name or status is clipped (with an
/// ellipsis) instead of shoving the divider and tabs sideways, and the 切替
/// preview does not jitter as the cursor moves between sessions.
const HEADER_NAME_COL: usize = 16;
const HEADER_AGENT_COL: usize = 9;
/// The status + agent block that follows the name: the right-edge status field
/// ([`STATUS_COL`]) and the agent label, with a two-space gap between them.
const HEADER_DETAIL_COL: usize = STATUS_COL + 2 + HEADER_AGENT_COL;

/// Builds the fixed-width right-pane header identity shown above a session's
/// preview / terminal: the session `name` (cyan, bold, clipped to
/// [`HEADER_NAME_COL`]), then either its git `status` label and `agent` state (a
/// real session) or the workspace-root note (the root row, no status). Each field
/// is padded to a constant width so the block is the same size for every session.
/// Shared by 切替 (Switch) and 没入 (Attached) so both carry the same identity.
fn preview_header(name: &str, status: Option<BranchStatus>, agent: Option<String>) -> String {
    let name = pad_to_width(
        style(clip_to_width(name, HEADER_NAME_COL))
            .cyan()
            .bold()
            .to_string(),
        HEADER_NAME_COL,
    );
    let detail = match status {
        Some(status) => {
            let status = pad_to_width(status_label(status), STATUS_COL);
            let agent = pad_to_width(agent.unwrap_or_default(), HEADER_AGENT_COL);
            format!("{status}  {agent}")
        }
        // The root row carries no git status / agent, only its note — clipped to
        // the same detail width so the identity block stays a constant size.
        None => pad_to_width(
            style(clip_to_width(ROOT_DETAIL, HEADER_DETAIL_COL))
                .dim()
                .to_string(),
            HEADER_DETAIL_COL,
        ),
    };
    format!("{name}  {detail}")
}

/// The header line for the active (focused) session: its name, git status, and
/// agent state — or the workspace-root note when the root row is active. Shared
/// by 没入 (Attached), where it sits above the embedded terminal, and 在席
/// (Focus), where it sits above the session's pane tabs.
fn active_session_header(state: &HomeState) -> String {
    match state.list().active() {
        Some(w) => {
            let agent = AgentState::from_flags(
                state.is_live(&w.path),
                state.is_running(&w.path),
                state.is_waiting(&w.path),
                state.is_done(&w.path),
            )
            .detail(HEADER_AGENT_COL);
            preview_header(
                w.branch.as_deref().unwrap_or(DETACHED),
                Some(w.status),
                agent,
            )
        }
        None => preview_header(ROOT_NAME, None, None),
    }
}

/// Builds the right pane from an embedded terminal snapshot: each grid row,
/// clipped to the pane width, up to `rows` rows.
pub(super) fn terminal_pane(view: &TerminalView, right_w: usize, rows: usize) -> Vec<String> {
    view.rows()
        .iter()
        .take(rows)
        .map(|row| clip_to_width(row, right_w))
        .collect()
}

/// The `›` cursor cell for a highlighted action-menu row, or a blank otherwise.
fn menu_marker(selected: bool) -> String {
    if selected {
        style("›").red().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// Builds one 在席 (Focus) menu row: a `›` cursor for the highlighted command,
/// its name, and its dimmed description, clipped to `width`.
pub(super) fn focus_menu_row(info: &CommandInfo, selected: bool, width: usize) -> String {
    menu_row(info.name, info.description, selected, width)
}

/// The shared layout for an action row: a `›` cursor when `selected`, a fixed-
/// width cyan `name`, and a dimmed `desc`, clipped to `width`. Used by the plain
/// command rows ([`focus_menu_row`]) and the `agent` row, which substitutes a
/// "Launch <default>" description and an expand chevron.
fn menu_row(name: &str, desc: &str, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{name:<9}")).cyan().bold().to_string()
    } else {
        style(format!("{name:<9}")).cyan().to_string()
    };
    let desc_budget = width.saturating_sub(HINT_INDENT + 9);
    let desc = style(clip_to_width(desc, desc_budget)).dim();
    clip_to_width(&format!("  {marker} {name}{desc}"), width)
}

/// The 在席 menu's `agent` row: like a plain command row but its description
/// names the agent a plain launch uses (the configured default) and carries an
/// expand affordance — `▾` while the picker is open, `▸` when it can open (more
/// than one CLI installed), nothing otherwise.
fn focus_agent_command_row(state: &HomeState, selected: bool, width: usize) -> String {
    let chevron = if state.focus_menu_expanded() {
        "▾ "
    } else if state.focus_menu_agent_can_expand() {
        "▸ "
    } else {
        ""
    };
    let desc = format!("{chevron}Launch {}", state.default_agent().display_name());
    menu_row("agent", &desc, selected, width)
}

/// One agent-picker sub-row, indented under the expanded `agent` row: a `›`
/// cursor on the highlighted CLI, its display name, and a dimmed `(default)` tag
/// on the configured agent.
fn focus_agent_pick_row(cli: AgentCli, selected: bool, is_default: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{:<10}", cli.display_name()))
            .cyan()
            .bold()
            .to_string()
    } else {
        style(format!("{:<10}", cli.display_name()))
            .cyan()
            .to_string()
    };
    let tag = if is_default {
        style("(default)").dim().to_string()
    } else {
        String::new()
    };
    clip_to_width(&format!("      {marker} {name}{tag}"), width)
}

/// The `session: <name>` header line shown above the 在席 (Focus) action surface
/// when the session has no live panes (an idle session, no tab strip). With live
/// panes the identity rides the tab strip ([`active_session_header`]) instead.
fn focus_session_header(state: &HomeState) -> String {
    style(format!("session: {}", state.focused_session_name()))
        .cyan()
        .bold()
        .to_string()
}

/// The body of the 在席 (Focus) menu (no identity header): the `Run a command:`
/// label, one row per Session-scope command (`›` cursor on the highlighted one),
/// and a key hint. Shared by the idle-session [`focus_menu`] and the "+ new" tab.
fn focus_menu_body(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![style("Run a command:").dim().to_string()];
    let cursor = state.focus_menu_cursor();
    let expanded = state.focus_menu_expanded();
    let commands = state.focus_menu_commands();
    for (i, info) in commands.iter().enumerate() {
        let selected = i == cursor;
        if info.name == "agent" {
            // The `agent` row names the default CLI; when expanded, its installed
            // alternatives follow as indented picker sub-rows (案A).
            lines.push(focus_agent_command_row(state, selected, width));
            if expanded {
                let agent_cursor = state.focus_menu_agent_cursor();
                let default = state.default_agent();
                for (j, &cli) in state.installed_agents().iter().enumerate() {
                    lines.push(focus_agent_pick_row(
                        cli,
                        Some(j) == agent_cursor,
                        cli == default,
                        width,
                    ));
                }
            }
        } else {
            lines.push(focus_menu_row(info, selected, width));
        }
    }
    lines.push(String::new());
    // The hint follows the surface: picker keys while expanded, an extra
    // "→ pick agent" affordance when the picker can open, else the base keys.
    let hint = if expanded {
        "↑↓ move   Enter launch   ← back".to_string()
    } else if state.focus_menu_agent_can_expand() {
        "↑↓ move   Enter run   → pick agent   t terminal   a agent".to_string()
    } else {
        "↑↓ move   Enter run   t terminal   a agent".to_string()
    };
    lines.push(style(hint).dim().to_string());
    lines
}

/// The body of the 在席 (Focus) prompt surface (no identity header): the
/// session-scoped command line (`❯ <input>▏`) and the Session-scope hint below
/// it. Shared by the idle-session [`focus_prompt`] and the "+ new" tab.
fn focus_prompt_body(state: &HomeState, width: usize) -> Vec<String> {
    let prompt = style("❯").red().bold();
    // Split at the caret so ←/→/Home/End move a visible block caret through the prompt.
    let (before, after) = state.focus_prompt().split_at(state.focus_prompt_cursor());
    let value = widgets::block_caret(before, after, &Style::new().cyan());
    let mut lines = vec![clip_to_width(&format!("{prompt} {value}"), width)];
    lines.push(String::new());
    lines.extend(focus_hint_lines(state.focus_prompt_hint(), width));
    lines
}

/// Builds the 在席 (Focus) menu: a short header, one row per Session-scope
/// command (`›` cursor on the highlighted one), and a key hint.
pub(super) fn focus_menu(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![focus_session_header(state), String::new()];
    lines.extend(focus_menu_body(state, width));
    lines
}

/// Builds the 在席 (Focus) prompt surface: a header, the session-scoped command
/// line (`❯ <input>▏`), and the Session-scope hint below it.
pub(super) fn focus_prompt(state: &HomeState, width: usize) -> Vec<String> {
    let mut lines = vec![focus_session_header(state), String::new()];
    lines.extend(focus_prompt_body(state, width));
    lines
}

/// The label of 在席's trailing "+ new" tab — the action surface that launches a
/// pane. ASCII so the underline marker in [`tab_strip_parts`] (which measures
/// width in `chars`) lands exactly under it, as it does for the pane labels.
const FOCUS_NEW_TAB_LABEL: &str = "+ new";

/// Builds the 在席 (Focus) right pane. With no live panes it is the session's
/// action surface alone — the menu or prompt with its own `session:` header,
/// exactly as before. With live panes it gains a **tab strip**: one chip per live
/// pane followed by a "+ new" chip, the session identity beside it (shared with
/// 没入), and below it either the selected pane's live preview or — on the "+ new"
/// tab — the action surface (header-less, the identity already rides the strip).
fn focus_pane(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // No live panes: the action surface fills the pane, just as it did before
    // tabs existed (its own `session:` header, no strip).
    let Some(strip) = state.terminal_tabs().filter(|s| !s.labels.is_empty()) else {
        let mut lines = match state.session_action_ui() {
            SessionActionUi::Menu => focus_menu(state, width),
            SessionActionUi::Prompt => focus_prompt(state, width),
        };
        lines.truncate(rows);
        lines.resize(rows, String::new());
        return lines;
    };

    // Live panes: the session's panes as tabs. The "+ new" tab is appended only
    // while it is the selected tab — the launch surface the user is acting on —
    // so stepping off it (e.g. `Esc` after `Ctrl-T`) drops the chip rather than
    // leaving a stale "+ new" sitting on the strip. The identity rides the
    // strip's row (as in 没入), so the body below carries no header of its own.
    let on_new = state.focus_on_new_tab();
    let mut labels = strip.labels.clone();
    let active = if on_new {
        labels.push(FOCUS_NEW_TAB_LABEL.to_string());
        labels.len().saturating_sub(1)
    } else {
        strip.active
    };
    let combined = TabStrip { labels, active };
    let header = active_session_header(state);
    let mut lines = header_tab_rows(header, Some(&combined), width);

    if on_new {
        // The "+ new" tab: the action surface that launches the next pane.
        lines.push(String::new());
        match state.session_action_ui() {
            SessionActionUi::Menu => lines.extend(focus_menu_body(state, width)),
            SessionActionUi::Prompt => lines.extend(focus_prompt_body(state, width)),
        }
    } else {
        // A pane tab: preview the pane's live screen (the snapshot taken before
        // painting), so the selection shows what re-attaching reveals. Fall back
        // to a label until the first snapshot is available.
        match state.terminal_view() {
            Some(view) => {
                let body = rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").green().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    }

    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// The hint rows for the 在席 prompt's Session-scope hint: the matching commands
/// while the word is typed, or the usage / examples once arguments are given.
fn focus_hint_lines(hint: Hint, width: usize) -> Vec<String> {
    match hint {
        Hint::Commands(hints) => hints
            .iter()
            .take(HINT_MAX)
            .map(|h| {
                let name = style(format!("{:<9}", h.name)).cyan().to_string();
                let desc = style(clip_to_width(
                    h.description,
                    width.saturating_sub(HINT_INDENT + 9),
                ))
                .dim();
                clip_to_width(&format!("    {name}{desc}"), width)
            })
            .collect(),
        Hint::Usage { usage, examples } => {
            let mut lines = vec![format!(
                "  {} {}",
                style("usage").dim(),
                style(usage).cyan()
            )];
            for example in examples.iter().take(HINT_MAX) {
                let text = clip_to_width(example, width.saturating_sub(HINT_INDENT + 6));
                lines.push(format!("    {} {}", style("e.g.").dim(), style(text).dim()));
            }
            lines
        }
        Hint::None => Vec::new(),
    }
}

/// Pad `lines` to fill the right pane and pin `hint` to its bottom row. Shared by
/// the rail's create / rename right-pane inputs, whose `Enter` / `Esc` hint stays
/// in view beneath the input.
fn switch_input_pane(mut lines: Vec<String>, hint: &str, width: usize, rows: usize) -> Vec<String> {
    let body_rows = rows.saturating_sub(1);
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines.push(style(clip_to_width(hint, width)).dim().to_string());
    lines
}

/// The 切替 (Switch) name input rendered in the **right pane** while creating a
/// session with the sidebar collapsed to the rail: a header, the typed name in a
/// bordered box with a block caret, the live validation error (or a hint) below
/// it, and the key hint pinned to the bottom row. At full width the input rides
/// the left pane inline instead (see [`super::switch_create_rows`]).
fn switch_create_pane(create: &CreateInput, width: usize, rows: usize) -> Vec<String> {
    // The box draws two borders and a space of padding on each side, so its
    // content area is the pane width less those four columns.
    let inner = width.saturating_sub(4).max(1);
    let (before, after) = create.value().split_at(create.cursor());
    let value = widgets::block_caret(before, after, &Style::new().cyan());
    let mut lines = vec![style("+ new session").green().bold().to_string()];
    lines.extend(widgets::boxed("", inner, &[value]));
    // Keep the row count stable whether or not the name is currently invalid: an
    // error replaces the dim hint in place rather than adding a row.
    match create.error() {
        Some(err) => lines.push(style(clip_to_width(err, width)).red().to_string()),
        None => lines.push(
            style("空文字・重複・\"/\" は作成できません")
                .dim()
                .to_string(),
        ),
    }
    switch_input_pane(lines, "Enter 作成 / Esc 取消", width, rows)
}

/// The 切替 (Switch) display-name input rendered in the **right pane** while
/// renaming a session with the sidebar collapsed to the rail: a header naming the
/// session, the typed label in a bordered box with a block caret, a hint, and the
/// key hint pinned to the bottom row. At full width it rides the left pane inline
/// instead (see [`super::switch_rename_rows`]).
fn switch_rename_pane(rename: &RenameInput, width: usize, rows: usize) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let value = widgets::block_caret(rename.value(), "", &Style::new().cyan());
    let mut lines = vec![clip_to_width(
        &style(format!("rename {}", rename.target()))
            .cyan()
            .bold()
            .to_string(),
        width,
    )];
    lines.extend(widgets::boxed("", inner, &[value]));
    lines.push(style("空にすると既定の表示名に戻す").dim().to_string());
    switch_input_pane(lines, "Enter 確定 / Esc 取消", width, rows)
}

/// Most note lines the read-only 切替 note overlay shows before eliding the rest
/// with a `… (N more)` line — the full text lives in the editor (`n` / `Ctrl-E`).
const SWITCH_NOTE_MAX_LINES: usize = 6;

/// Most note lines the *editing* overlay shows at once, windowed around the
/// caret, so the box never hides the whole right pane while editing.
const EDIT_NOTE_MAX_LINES: usize = 12;

/// The narrowest the floating note box renders.
const NOTE_BOX_MIN_WIDTH: usize = 28;
/// The widest the floating note box renders — past this the box stops growing so
/// it stays a top-right column rather than swallowing a wide right pane.
const NOTE_BOX_MAX_WIDTH: usize = 48;
/// Columns kept clear to the left of the box (so the preview underneath — the
/// session header, the live terminal — stays readable beside it) when the pane
/// is wide enough to spare them.
const NOTE_PREVIEW_KEEP: usize = 12;

/// Width of the floating note box for a right pane `pane_w` columns wide: capped
/// to [`NOTE_BOX_MAX_WIDTH`] and kept a [`NOTE_PREVIEW_KEEP`]-column margin off
/// the pane's left so it reads as a top-right column over the preview. On a pane
/// too narrow to spare that margin it floors at [`NOTE_BOX_MIN_WIDTH`] (clamped
/// to the pane), and on a pane narrower still it takes the whole row.
fn note_box_width(pane_w: usize) -> usize {
    pane_w
        .saturating_sub(NOTE_PREVIEW_KEEP)
        .clamp(NOTE_BOX_MIN_WIDTH, NOTE_BOX_MAX_WIDTH)
        .min(pane_w)
}

/// Build the floating `note` box overlaid on the right pane. With `caret` set it
/// is the **editor** (a block caret on the cursor line, the view windowed around
/// it, and the selected span — if any — reversed); with `None` it is the
/// **read-only** note (capped, the overflow elided with `… (N more)`). `max` caps
/// the body so the box always leaves part of the right pane visible underneath.
/// Returned rows are the bordered box. The session is already named in the pane
/// header, so the title is just `note` (no session name).
fn note_box(
    lines: &[String],
    caret: Option<(usize, usize)>,
    selection: Option<((usize, usize), (usize, usize))>,
    width: usize,
    max: usize,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let max = max.max(1);
    let body: Vec<String> = match caret {
        // Read-only: the first `max` lines, then a `… (N more)` line when longer.
        None => {
            let shown = lines.len().min(max);
            let mut body = lines[..shown].to_vec();
            if lines.len() > shown {
                body.push(format!("… ({} more)", lines.len() - shown));
            }
            body
        }
        // Editing: a `max`-line window around the caret. The selected span is
        // reversed; the caret is a block on the cursor line (drawn by
        // `block_selection` at the selection's edge when a span is active, else by
        // `block_caret`), so editing happens where it shows.
        Some((caret_row, caret_col)) => {
            let start = caret_row.saturating_sub(max.saturating_sub(1));
            let base = Style::new();
            lines
                .iter()
                .enumerate()
                .skip(start)
                .take(max)
                .map(
                    |(i, line)| match selection_on_line(selection, i, line.len()) {
                        Some((sel_start, sel_end, newline_selected)) => {
                            let caret = (i == caret_row).then_some(caret_col);
                            widgets::block_selection(
                                line,
                                sel_start,
                                sel_end,
                                caret,
                                newline_selected,
                                &base,
                            )
                        }
                        None if i == caret_row => {
                            let (before, after) = line.split_at(caret_col);
                            widgets::block_caret(before, after, &base)
                        }
                        None => line.clone(),
                    },
                )
                .collect()
        }
    };
    // `boxed` clips each line (and the block-caret one, ANSI included) to `inner`.
    widgets::boxed("note", inner, &body)
}

/// The byte span `[start, end)` of `selection` that lies on line `row` (whose
/// content is `len` bytes), plus whether the line break after it is selected too
/// (the span continues onto a later line, so the renderer shows the newline as a
/// reversed trailing cell). `None` when the line is outside the selection.
fn selection_on_line(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    len: usize,
) -> Option<(usize, usize, bool)> {
    let ((sr, sc), (er, ec)) = selection?;
    if row < sr || row > er {
        return None;
    }
    let start = if row == sr { sc } else { 0 };
    let end = if row == er { ec } else { len };
    Some((start, end, row < er))
}

/// The floating note overlay for the right pane, or `None` when none applies. The
/// **editor** (when open, in any mode) wins; otherwise the highlighted session's
/// **read-only** note shows while browsing in 切替 (until `Esc` dismisses it, see
/// [`HomeState::switch_note_visible`]). The box is a narrow top-right column (see
/// [`note_box_width`]) composited over the pane by [`right_pane_contents`], so
/// the preview underneath — the session header, the live terminal — stays
/// readable to its left and below it. `rows` caps the box height so the pane
/// stays partly visible behind it; `width` is the full right-pane width (the box
/// narrows itself within it).
fn note_overlay(state: &HomeState, width: usize, rows: usize) -> Option<Vec<String>> {
    let box_w = note_box_width(width);
    if let Some(editor) = state.note_editor() {
        let cap = EDIT_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        return Some(note_box(
            editor.area().lines(),
            Some(editor.area().cursor()),
            editor.area().selection(),
            box_w,
            cap,
        ));
    }
    if let Some(note) = state.visible_switch_note() {
        let cap = SWITCH_NOTE_MAX_LINES.min(rows.saturating_sub(3)).max(1);
        let note_lines: Vec<String> = note.lines().map(str::to_string).collect();
        return Some(note_box(&note_lines, None, None, box_w, cap));
    }
    None
}

/// The 切替 (Switch) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; a session with no live shell previews its 在席 action
/// menu. The header line carries the session's status and agent state. The key
/// hints live in the footer, so the preview uses the pane's full height. The
/// highlighted session's note is drawn over the top by [`note_overlay`] (not
/// inline), so it never pushes this preview around.
pub(super) fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    let body_rows = rows;
    // Identify the highlighted row. `selected()` is `Some` for a real session
    // row and `None` on the root row (which carries no git status / tracked
    // path), so the match doubles as the root/session split.
    let (name, live, running, waiting, done, status) = match state.list().selected() {
        Some(w) => (
            w.branch.as_deref().unwrap_or(DETACHED).to_string(),
            state.is_live(&w.path),
            state.is_running(&w.path),
            state.is_waiting(&w.path),
            state.is_done(&w.path),
            Some(w.status),
        ),
        None => {
            // The root row carries no worktree, but its embedded session is
            // keyed by the workspace root path, so match it against the same
            // live / running / waiting / done sets — otherwise a running root
            // agent never previews live here (it only re-appears once selected).
            let root = state.root_path();
            (
                ROOT_NAME.to_string(),
                state.is_live(root),
                state.is_running(root),
                state.is_waiting(root),
                state.is_done(root),
                None,
            )
        }
    };

    // Header: the name, then either the git status + agent state (a session) or
    // the workspace-root note (the root row). A live session's tabs share the
    // header's row (the `←`/`→` targets), so the identity and the tabs read
    // together on one line; the preview below mirrors the active pane.
    let agent = AgentState::from_flags(live, running, waiting, done).detail(HEADER_AGENT_COL);
    let header = preview_header(&name, status, agent);
    let mut lines = header_tab_rows(
        header,
        if live { state.terminal_tabs() } else { None },
        width,
    );

    if live {
        // Selecting re-attaches the running shell / agent: preview its actual
        // screen (the live snapshot taken before painting) so the choice shows
        // what re-attaching reveals. Fall back to a label until the first
        // snapshot is available.
        match state.terminal_view() {
            Some(view) => {
                let body = body_rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").green().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    } else {
        // Selecting opens 在席 on this session: preview its action surface, which
        // mirrors the configured Session Action UI — a command menu or a prompt —
        // so the preview matches what focusing actually reveals. A blank row keeps
        // the header clear of the surface below it.
        lines.push(String::new());
        match state.session_action_ui() {
            SessionActionUi::Menu => {
                lines.push(style("Run a command:").dim().to_string());
                for (i, info) in state.preview_menu_commands().iter().enumerate() {
                    lines.push(focus_menu_row(info, i == 0, width));
                }
            }
            SessionActionUi::Prompt => {
                let prompt = style("❯").red().bold();
                let value = widgets::block_caret("", "", &Style::new().cyan());
                lines.push(clip_to_width(&format!("{prompt} {value}"), width));
            }
        }
        lines.push(String::new());
        lines.push(style("Enter で開く").dim().to_string());
    }

    // Trim the body to its budget and pad up so the pane is always full-height.
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines
}

/// The right pane's contents, by mode. A preview of the would-be session screen
/// in 切替 (the default); the session's action surface — a menu or a prompt, per
/// [`SessionActionUi`] — in 在席; and the live embedded terminal in 没入 (a
/// starting hint until the first snapshot arrives).
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    // The Markdown preview, when open, takes over the right pane regardless of
    // mode (it is opened from the `:` palette and captures the keyboard while
    // shown).
    if let Some(preview) = state.preview() {
        return preview_pane(preview, right_w, rows);
    }
    // The base pane for the current mode. The session-note overlay (the editor,
    // or the read-only note while browsing in 切替) is composited over its top
    // below, so editing / reading the note never switches the screen — the
    // preview / terminal stays visible behind the floating box.
    let mut base = match state.mode() {
        Mode::Switch => {
            // Collapsed to the rail, 切替's name input has no room inline in the
            // (5-column) list, so it takes over the wide right pane; at full width
            // it rides the left pane inline and the right pane keeps previewing the
            // highlighted session.
            if state.sidebar() == Sidebar::Rail {
                if let Some(create) = state.create() {
                    return switch_create_pane(create, right_w, rows);
                }
                if let Some(rename) = state.rename() {
                    return switch_rename_pane(rename, right_w, rows);
                }
            }
            // Fade the whole preview in 切替: the keyboard is on the session list
            // to the left, so dimming the right pane signals it is not the focus —
            // the highlighted session and its tabs are browsed there, not selected
            // from here. The note box (when open) is composited bright on top below,
            // so the deliberately-opened note still reads against the faded preview.
            switch_preview(state, right_w, rows)
                .iter()
                .map(|row| dim_row(row))
                .collect()
        }
        Mode::Focus => focus_pane(state, right_w, rows),
        Mode::Attached => {
            // The active session's identity shares the top row with its tab chips
            // (the underline marker below them), so the header reads beside the
            // tabs just as it does in 切替. This header + tab block always fills
            // exactly `TAB_BAR_ROWS`, matching `attached_geometry`, so the embedded
            // terminal below never shifts whether or not a strip is published. A
            // starting hint stands in until the first screen snapshot arrives.
            let mut lines = Vec::with_capacity(rows);
            let header = active_session_header(state);
            let mut head = header_tab_rows(header, state.terminal_tabs(), right_w);
            head.resize(super::TAB_BAR_ROWS, String::new());
            lines.extend(head);
            let body = rows.saturating_sub(lines.len());
            match state.terminal_view() {
                Some(view) => lines.extend(terminal_pane(view, right_w, body)),
                None => lines.push(
                    style(clip_to_width(TERMINAL_STARTING, right_w))
                        .dim()
                        .to_string(),
                ),
            }
            lines
        }
    };
    // Composite the floating note box onto the top-right of the base pane: only
    // the box's own columns are overwritten, so the preview / terminal to its
    // left and below stays put (no CLS), and the session header on the top row
    // keeps its leading columns beside the box.
    if let Some(overlay) = note_overlay(state, right_w, rows) {
        // Grow the base to hold the whole box when the pane beneath is shorter
        // than it (no live snapshot yet, or a partial one), so the box's bottom
        // border always lands instead of being clipped as the note grows with
        // each newline. Rows the box does not cover are left blank.
        if overlay.len() > base.len() {
            base.resize(overlay.len(), String::new());
        }
        widgets::overlay_right(&mut base, 0, right_w, &overlay);
    }
    base
}

/// Render the right-pane Markdown preview: a one-row header (the file path, plus
/// a `start-end/total` position once it scrolls) over a window of rendered
/// Markdown lines. The window is clamped so the last line stays in view, matching
/// the event loop's scroll clamp, and each row is clipped to the pane width — so
/// the preview never overruns the pane or shifts the layout (no CLS).
pub(super) fn preview_pane(preview: &Preview, width: usize, rows: usize) -> Vec<String> {
    let total = preview.lines.len();
    let body_h = rows.saturating_sub(1);
    let max_start = total.saturating_sub(body_h);
    let start = preview.scroll.min(max_start);
    let end = (start + body_h).min(total);

    let header = if total > body_h {
        format!("📄 {}  ({}-{}/{})", preview.title, start + 1, end, total)
    } else {
        format!("📄 {}", preview.title)
    };

    let mut lines = Vec::with_capacity(rows);
    lines.push(style(clip_to_width(&header, width)).bold().to_string());
    // A one-row pane (or none) shows just the header.
    if rows <= 1 {
        lines.truncate(rows);
        return lines;
    }
    for i in 0..body_h {
        match preview.lines.get(start + i) {
            Some(line) => lines.push(markdown_row(line, width)),
            None => lines.push(String::new()),
        }
    }
    lines
}

/// Render one [`MarkdownLine`] to a styled, width-clipped row: its prefix marker
/// coloured by block kind, then its inline spans styled by emphasis.
fn markdown_row(line: &MarkdownLine, width: usize) -> String {
    let mut out = String::new();
    if !line.prefix.is_empty() {
        let prefix = match line.style {
            LineStyle::Bullet | LineStyle::Number => style(&line.prefix).cyan().to_string(),
            LineStyle::Quote => style(&line.prefix).dim().to_string(),
            _ => line.prefix.clone(),
        };
        out.push_str(&prefix);
    }
    for span in &line.spans {
        out.push_str(&styled_span(span, line.style));
    }
    clip_to_width(&out, width)
}

/// Style one inline [`Span`] for terminal display. A heading colours its whole
/// content by level; a code-block line and a quote line take a uniform style;
/// every other line styles each span by its own inline emphasis.
fn styled_span(span: &Span, line_style: LineStyle) -> String {
    let text = span.text.as_str();
    match line_style {
        LineStyle::Heading(level) => heading_style(text, level),
        // Syntax-highlighted code carries a per-token colour; an uncoloured code
        // span (unknown highlight) falls back to a uniform green.
        LineStyle::Code => match span.color {
            Some(rgb) => style(text).color256(rgb_to_ansi256(rgb)).to_string(),
            None => style(text).green().to_string(),
        },
        LineStyle::Quote => style(text).dim().italic().to_string(),
        _ => match span.style {
            SpanStyle::Plain => text.to_string(),
            SpanStyle::Strong => style(text).bold().to_string(),
            SpanStyle::Emphasis => style(text).italic().to_string(),
            SpanStyle::Code => style(text).green().to_string(),
            SpanStyle::Link => style(text).blue().underlined().to_string(),
        },
    }
}

/// The bold, level-coloured styling of a heading's text: magenta (h1), cyan (h2),
/// yellow (h3), and plain bold for deeper levels.
fn heading_style(text: &str, level: u8) -> String {
    let base = style(text).bold();
    match level {
        1 => base.magenta(),
        2 => base.cyan(),
        3 => base.yellow(),
        _ => base,
    }
    .to_string()
}

/// Map a 24-bit colour to the nearest xterm-256 palette index — the broadest
/// colour depth the `console` styling exposes. Near-grey colours snap to the
/// 24-step greyscale ramp (232–255); everything else snaps to the 6×6×6 colour
/// cube (16–231). Both choose the closest step per channel.
fn rgb_to_ansi256(rgb: Rgb) -> u8 {
    let Rgb { r, g, b } = rgb;
    // Treat colours whose channels are within a small spread as grey so subtle
    // foregrounds use the finer-grained ramp instead of the coarse cube.
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min <= 8 {
        // The ramp runs grey 8..=238 in steps of 10 across indices 232..=255.
        let level = ((r as u16 + g as u16 + b as u16) / 3).saturating_sub(8) / 10;
        let level = level.min(23) as u8;
        return 232 + level;
    }
    let cube = |c: u8| -> u8 {
        // Cube steps sit at 0, 95, 135, 175, 215, 255; pick the nearest.
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0u8;
        let mut best_dist = u16::MAX;
        for (idx, &step) in STEPS.iter().enumerate() {
            let dist = (c as i16 - step as i16).unsigned_abs();
            if dist < best_dist {
                best_dist = dist;
                best = idx as u8;
            }
        }
        best
    };
    16 + 36 * cube(r) + 6 * cube(g) + cube(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_inline_label_tinted_carries_the_figures_for_every_load_band() {
        // The CPU and memory fields are tinted by their own load band (dim / yellow
        // / red); whatever the tint, both figures still read through. Cover calm,
        // busy, and hot for each field.
        for usage in [
            ResourceUsage {
                cpu_percent: 1,
                memory_bytes: 1,
            }, // calm / calm
            ResourceUsage {
                cpu_percent: 50,
                memory_bytes: 600 * 1024 * 1024,
            }, // busy / busy
            ResourceUsage {
                cpu_percent: 200,
                memory_bytes: 3 * 1024 * 1024 * 1024,
            }, // hot / hot
        ] {
            let plain =
                console::strip_ansi_codes(&resource_inline_label_tinted(usage)).into_owned();
            assert!(
                plain.contains(&usage.format_cpu()),
                "{plain:?} keeps the CPU figure"
            );
            assert!(
                plain.contains(&usage.format_memory()),
                "{plain:?} keeps the memory figure"
            );
        }
    }

    #[test]
    fn name_cell_pads_by_display_width_not_char_count() {
        // A full-width (CJK) name must fill its cell by *display* columns, not
        // char count: `あ機能` is 3 chars but 6 display columns, so padding to a
        // width-8 cell adds 2 columns (not 5 chars), and the cell measures exactly
        // 8 — the SGR style escapes have zero display width. The old
        // `format!("{:<8}")` padded by chars and overran the cell to 11 columns,
        // shoving the status column sideways (the app's own UI is Japanese).
        assert_eq!(
            console::measure_text_width(&name_cell("あ機能", 8, false)),
            8
        );
        // ASCII is unchanged: a short name still pads out to the full width.
        assert_eq!(console::measure_text_width(&name_cell("main", 8, true)), 8);
        // A name already wider than the cell is clipped back to the width.
        assert_eq!(
            console::measure_text_width(&name_cell("あ機能拡張作業", 8, false)),
            8
        );
    }

    #[test]
    fn uncoloured_code_span_falls_back_to_green() {
        // A code-block span with no highlight colour uses the uniform green arm,
        // matching the styling of inline code.
        let span = Span {
            text: "x".to_string(),
            style: SpanStyle::Code,
            color: None,
        };
        assert_eq!(
            styled_span(&span, LineStyle::Code),
            style("x").green().to_string()
        );
    }

    #[test]
    fn coloured_code_span_takes_the_palette_arm_and_keeps_its_text() {
        // A highlighted span goes through the 256-colour arm; its visible text is
        // preserved (colour escapes are stripped when the output is not a TTY).
        let span = Span {
            text: "fn".to_string(),
            style: SpanStyle::Code,
            color: Some(Rgb {
                r: 180,
                g: 120,
                b: 60,
            }),
        };
        let out = styled_span(&span, LineStyle::Code);
        assert_eq!(console::strip_ansi_codes(&out), "fn");
    }

    #[test]
    fn rgb_maps_near_grey_to_the_greyscale_ramp() {
        // Equal channels are grey: they snap into the 232–255 ramp.
        assert!((232..=255).contains(&rgb_to_ansi256(Rgb { r: 0, g: 0, b: 0 })));
        assert!((232..=255).contains(&rgb_to_ansi256(Rgb {
            r: 128,
            g: 130,
            b: 127
        })));
        assert_eq!(
            rgb_to_ansi256(Rgb {
                r: 255,
                g: 255,
                b: 255
            }),
            255
        );
    }

    #[test]
    fn rgb_maps_saturated_colour_to_the_cube() {
        // A clearly chromatic colour lands in the 16–231 colour cube. Pure red
        // is cube index (5,0,0) → 16 + 36*5 = 196.
        assert_eq!(rgb_to_ansi256(Rgb { r: 255, g: 0, b: 0 }), 196);
        let blue = rgb_to_ansi256(Rgb { r: 0, g: 0, b: 255 });
        assert!((16..=231).contains(&blue));
    }

    #[test]
    fn digits_counts_decimal_places_with_a_floor_of_one() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(999), 3);
        assert_eq!(digits(1000), 4);
    }

    #[test]
    fn rpad_left_pads_to_the_column_width_and_never_shrinks() {
        assert_eq!(rpad("ab", 5), "   ab");
        // Already at/over width → returned unchanged (rpad never truncates).
        assert_eq!(rpad("abcde", 3), "abcde");
    }

    #[test]
    fn diff_cell_pads_counts_to_fixed_columns_and_blanks_when_absent() {
        // `+N` right-aligned in 3 digit columns, `-M` in 2, so the `+`/`-` of every
        // row line up however many changed lines each session has.
        let cell = diff_cell(
            Some(DiffStat {
                added: 5,
                removed: 3,
            }),
            3,
            2,
        );
        assert_eq!(console::strip_ansi_codes(&cell), "+  5 - 3");
        let wide = diff_cell(
            Some(DiffStat {
                added: 124,
                removed: 18,
            }),
            3,
            2,
        );
        assert_eq!(console::strip_ansi_codes(&wide), "+124 -18");
        // Same width whether or not the row has a diff, so the column never moves.
        assert_eq!(
            console::measure_text_width(&cell),
            console::measure_text_width(&diff_cell(None, 3, 2)),
        );
        assert!(diff_cell(None, 3, 2).trim().is_empty());
    }

    #[test]
    fn commits_cell_aligns_arrows_in_fixed_columns_and_blanks_even_sides() {
        // Both sides drawn in this render (ahead in 2 cols, behind in 1).
        let both = commits_cell(
            Some(AheadBehind {
                ahead: 2,
                behind: 1,
            }),
            2,
            1,
        );
        assert_eq!(console::strip_ansi_codes(&both), "↑ 2 ↓1");
        // This row is even-behind → the `↓` side is blanks, holding the column so
        // the next row's `↓` still lines up.
        let ahead_only = commits_cell(
            Some(AheadBehind {
                ahead: 2,
                behind: 0,
            }),
            2,
            1,
        );
        assert!(console::strip_ansi_codes(&ahead_only).starts_with("↑ 2"));
        assert_eq!(
            console::measure_text_width(&ahead_only),
            console::measure_text_width(&both),
        );
        // No behind side anywhere in the render → only the `↑` column is spent.
        let no_behind = commits_cell(
            Some(AheadBehind {
                ahead: 3,
                behind: 0,
            }),
            1,
            0,
        );
        assert_eq!(console::strip_ansi_codes(&no_behind), "↑3");
        // No ahead side → only the `↓` column.
        let no_ahead = commits_cell(
            Some(AheadBehind {
                ahead: 0,
                behind: 2,
            }),
            0,
            1,
        );
        assert_eq!(console::strip_ansi_codes(&no_ahead), "↓2");
        // Column dropped entirely → empty.
        assert_eq!(commits_cell(None, 0, 0), "");
        // A drawn column but no measurement for this row → blanks.
        let none = commits_cell(None, 1, 0);
        assert_eq!(console::measure_text_width(&none), 2);
        assert!(none.trim().is_empty());
    }

    #[test]
    fn detail_cols_widths_reserve_only_the_used_sides() {
        let full = DetailCols {
            time: 8,
            ahead: 2,
            behind: 1,
            added: 3,
            removed: 2,
            pr: 4, // "#123"
        };
        assert_eq!(full.commits_width(), 6); // (1+2) + gap + (1+1)
        assert_eq!(full.badge_width(), 8); // 3 + 2 + 3
        assert_eq!(full.cluster_width(), 8 + 1 + 6 + 1 + 8 + 1 + 4); // four fields, three gaps

        // Only an ahead side, no diff, no time: one field, no gaps, no `↓` columns.
        let ahead_only = DetailCols {
            ahead: 2,
            ..DetailCols::default()
        };
        assert_eq!(ahead_only.commits_width(), 3);
        assert_eq!(ahead_only.badge_width(), 0);
        assert_eq!(ahead_only.cluster_width(), 3);

        // Only a behind side (covers the `up == 0` half of the commit gap).
        let behind_only = DetailCols {
            behind: 2,
            ..DetailCols::default()
        };
        assert_eq!(behind_only.commits_width(), 3);

        assert_eq!(DetailCols::default().cluster_width(), 0);
    }

    fn at(now: DateTime<Utc>, mins: i64) -> DateTime<Utc> {
        now - chrono::Duration::minutes(mins)
    }

    #[test]
    fn detail_cols_sizes_columns_to_the_widest_visible_session() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let data = vec![
            (
                at(now, 3),
                Some(DiffStat {
                    added: 5,
                    removed: 3,
                }),
                Some(AheadBehind {
                    ahead: 2,
                    behind: 0,
                }),
                pr_width(&[pr(7)]), // "<icon> 1" → 3
            ),
            (
                at(now, 12),
                Some(DiffStat {
                    added: 140,
                    removed: 8,
                }),
                Some(AheadBehind {
                    ahead: 0,
                    behind: 13,
                }),
                pr_width(&[pr(412), pr(98)]), // "<icon> 2" → 3
            ),
            // A session with neither a diff nor divergence nor PR: exercises every
            // empty arm so they contribute no columns.
            (at(now, 1), None, None, 0),
        ];
        let cols = detail_cols(&data, now, 9, 60);
        assert_eq!(cols.added, 3); // "140"
        assert_eq!(cols.removed, 1); // "8" / "3"
        assert_eq!(cols.ahead, 1); // "2"
        assert_eq!(cols.behind, 2); // "13"
        assert_eq!(cols.pr, 3); // "<icon> 2" — both sessions fold to one badge
        assert_eq!(
            cols.time,
            console::measure_text_width(&relative_time(now, at(now, 12))) // "12min ago"
        );
    }

    #[test]
    fn detail_cols_drops_time_then_commits_under_width_pressure() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let data = vec![(
            at(now, 3),
            Some(DiffStat {
                added: 1,
                removed: 2,
            }),
            Some(AheadBehind {
                ahead: 2,
                behind: 1,
            }),
            0,
        )];
        // Roomy: every field survives (full cluster needs ~30 columns beside a
        // 9-wide agent label).
        let roomy = detail_cols(&data, now, 9, 60);
        assert!(roomy.time > 0);
        assert!(roomy.ahead > 0 || roomy.behind > 0);
        assert!(roomy.added > 0);
        // Tighter: the lowest-priority time is dropped, commits + badge stay.
        let mid = detail_cols(&data, now, 9, 25);
        assert_eq!(mid.time, 0);
        assert!(mid.ahead > 0 || mid.behind > 0);
        assert!(mid.added > 0);
        // Tightest: commits also dropped, but the badge is always kept.
        let tight = detail_cols(&data, now, 9, 18);
        assert_eq!(tight.time, 0);
        assert_eq!(tight.ahead, 0);
        assert_eq!(tight.behind, 0);
        assert!(tight.added > 0);
    }

    #[test]
    fn detail_content_right_aligns_the_cluster_and_clips_the_agent() {
        let badge = diff_cell(
            Some(DiffStat {
                added: 124,
                removed: 18,
            }),
            3,
            2,
        );
        // Agent label on the left, the cluster pinned to the cell's right edge; the
        // whole cell measures exactly the width so the badges line up.
        let line = detail_content(AgentState::Running, std::slice::from_ref(&badge), 24);
        assert_eq!(console::measure_text_width(&line), 24);
        let plain = console::strip_ansi_codes(&line);
        assert!(plain.starts_with("▶ running"));
        assert!(plain.ends_with("+124 -18"));

        // With no agent the cluster still rides the right edge.
        let line = detail_content(AgentState::Absent, std::slice::from_ref(&badge), 24);
        assert_eq!(console::measure_text_width(&line), 24);
        assert_eq!(console::strip_ansi_codes(&line).trim_start(), "+124 -18");
    }

    #[test]
    fn detail_content_falls_back_to_the_agent_or_clips_a_cramped_cluster() {
        // No cells → just the agent label (blank when absent).
        assert_eq!(detail_content(AgentState::Absent, &[], 20), "");
        assert!(
            console::strip_ansi_codes(&detail_content(AgentState::Running, &[], 20))
                .contains("running")
        );
        // Cluster alone wider than the cell → clipped to the cell.
        let badge = diff_cell(
            Some(DiffStat {
                added: 124,
                removed: 18,
            }),
            3,
            2,
        );
        let line = detail_content(AgentState::Running, std::slice::from_ref(&badge), 5);
        assert!(console::measure_text_width(&line) <= 5);
    }

    #[test]
    fn detail_content_joins_the_cells_in_order_with_single_space_gaps() {
        let time = rpad(&style("3min ago").dim().to_string(), 8);
        let commits = commits_cell(
            Some(AheadBehind {
                ahead: 2,
                behind: 1,
            }),
            1,
            1,
        );
        let badge = diff_cell(
            Some(DiffStat {
                added: 1,
                removed: 2,
            }),
            1,
            1,
        );
        let cells = vec![time, commits, badge];
        let line = detail_content(AgentState::Running, &cells, 40);
        let plain = console::strip_ansi_codes(&line);
        assert!(plain.starts_with("▶ running"));
        assert!(plain.contains("3min ago ↑2 ↓1 +1 -2"));
        assert!(plain.ends_with("+1 -2"));
    }

    #[test]
    fn relative_time_buckets_by_elapsed_span() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ago = |secs: i64| {
            console::strip_ansi_codes(&relative_time(now, now - chrono::Duration::seconds(secs)))
                .into_owned()
        };
        assert_eq!(ago(5), "now"); // under a minute
        assert_eq!(ago(180), "3min ago"); // minutes
        assert_eq!(ago(7200), "2h ago"); // hours
        assert_eq!(ago(2 * 86_400), "2d ago"); // days
                                               // A future timestamp (clock skew) clamps to "now".
        assert_eq!(
            console::strip_ansi_codes(&relative_time(now, now + chrono::Duration::seconds(30))),
            "now"
        );
    }

    fn pr(number: u32) -> PrLink {
        PrLink {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    #[test]
    fn pr_cell_folds_prs_into_an_icon_and_count_and_blanks_when_absent() {
        // One PR rides the right edge of its fixed column as `<icon> 1`; a wider
        // column left-pads with spaces so badges line up down the list.
        let cell = pr_cell(&[pr(7)], 5);
        assert_eq!(console::measure_text_width(&cell), 5);
        assert_eq!(
            console::strip_ansi_codes(&cell),
            format!("  {PR_ICON} 1").as_str()
        );
        // Several PRs fold into one `<icon> <count>` badge, not a `#N #M` run.
        let many = pr_cell(&[pr(412), pr(98)], 3);
        assert_eq!(
            console::strip_ansi_codes(&many),
            format!("{PR_ICON} 2").as_str()
        );
        // No PR fills the same width with blanks, holding the column.
        assert_eq!(pr_cell(&[], 4), "    ");
    }

    #[test]
    fn pr_width_is_the_icon_space_and_count_digits() {
        assert_eq!(pr_width(&[]), 0);
        assert_eq!(pr_width(&[pr(7)]), 3); // "<icon> 1"
        assert_eq!(pr_width(&[pr(412), pr(98)]), 3); // "<icon> 2"
                                                     // A count that reaches two digits widens by one.
        let ten: Vec<PrLink> = (0..10).map(pr).collect();
        assert_eq!(pr_width(&ten), 4); // "<icon> 10"
    }

    #[test]
    fn pr_popup_box_lists_the_numbers_in_a_titled_box() {
        let popup = pr_popup_box(&[pr(442), pr(447)]);
        let plain: Vec<String> = popup
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        // The top border carries the `PR` title; a content row lists both numbers.
        assert!(plain[0].contains("PR"));
        assert!(plain
            .iter()
            .any(|l| l.contains("#442") && l.contains("#447")));
        // No PR → no box, so the overlay is a no-op for a session without one.
        assert!(pr_popup_box(&[]).is_empty());
    }

    #[test]
    fn pr_popup_box_keeps_the_title_clear_for_a_single_digit_pr() {
        let popup = pr_popup_box(&[pr(7)]);
        let plain: Vec<String> = popup
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();

        assert_eq!(plain[0], "┌─ PR ┐");
        assert_eq!(plain[1], "│ #7  │");
    }

    #[test]
    fn pr_popup_box_wraps_a_long_list_within_the_inner_cap() {
        // Twenty `#1NN` badges (4 columns each) cannot fit one capped line, so they
        // wrap onto several content rows.
        let many: Vec<PrLink> = (100u32..120).map(pr).collect();
        let popup = pr_popup_box(&many);
        // More than just the top + bottom border: the list spilled onto several rows.
        assert!(popup.len() > 3);
        // Every row (content and border alike) stays within the inner cap plus the
        // two borders and a space of padding on each side.
        for line in &popup {
            assert!(console::measure_text_width(line) <= PR_POPUP_INNER + 4);
        }
    }

    #[test]
    fn detail_content_keeps_the_pr_cell_at_the_right_edge() {
        // The PR cell, as the last in `cells`, lands flush against the right edge
        // beside the diff badge (`+1 -2 <icon> 2`).
        let badge = diff_cell(
            Some(DiffStat {
                added: 1,
                removed: 2,
            }),
            1,
            1,
        );
        let cell = pr_cell(&[pr(412), pr(98)], 3);
        let cells = vec![badge, cell];
        let line = detail_content(AgentState::Running, &cells, 40);
        let plain = console::strip_ansi_codes(&line);
        assert!(plain.starts_with("▶ running"));
        assert!(plain.contains(format!("+1 -2 {PR_ICON} 2").as_str()));
        assert!(plain.ends_with(format!("{PR_ICON} 2").as_str()));
    }
}
