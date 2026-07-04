//! The two-pane body: the worktree list (left) and the mode-dependent right
//! pane (a switch preview, the focus menu/prompt, or the embedded terminal).
//! All functions take plain data and return styled lines.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::presentation::theme::Palette;
use chrono::{DateTime, Duration, Utc};
use console::{style, Style};

use super::super::command::{CommandInfo, Hint};
use super::super::state::{
    CreateInput, DiffView, HomeState, LineKind, LogLine, Mode, Preview, RenameInput, WorktreeList,
    ROOT_NAME,
};
use super::super::terminal::tabs::TabStrip;
use super::super::terminal::view::TerminalView;
use super::{
    clip_to_width, pad_to_width, ACTIVE_COL, DETACHED, DIRTY_ICON, EMPTY_MESSAGE, HINT_INDENT,
    HINT_MAX, LOCAL_ICON, NAME_PREFIX, NEW_ICON, NOTE_ICON, PUSHED_ICON, RAIL_WIDTH, ROOT_DETAIL,
    STATUS_COL, SYNCED_ICON, TERMINAL_STARTING,
};
use crate::domain::resource::{Load, ResourceUsage};
use crate::domain::settings::{
    AgentCli, LabelColor, SessionActionUi, SessionLabelDef, SessionLabelMaster, Sidebar,
};
use crate::domain::workspace_state::{AheadBehind, BranchStatus, DiffStat, PrLink, WorktreeState};
use crate::presentation::tui::diff::{split_rows, DiffRow, DiffSpan, RowKind, SplitRow};
use crate::presentation::tui::markdown::{LineStyle, MarkdownLine, Rgb, Span, SpanStyle};
use crate::presentation::tui::widgets;
use unicode_width::UnicodeWidthChar;

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
        BranchStatus::New => Style::new().info(),
        BranchStatus::Dirty => Style::new().feature(),
        BranchStatus::Local => Style::new().warning(),
        BranchStatus::Pushed => Style::new().success(),
        BranchStatus::Synced => Style::new().accent(),
    }
}

/// The colour-coded `<icon> <word>` label for a branch's lifecycle status, shown
/// in the right-pane header ([`preview_header`]). The icon gives an at-a-glance
/// read; the word keeps it legible without a Nerd Font and disambiguates the
/// colour.
pub(super) fn status_label(status: BranchStatus) -> String {
    let text = format!("{} {}", status_icon(status), status.as_str());
    status_style(status).apply_to(text).to_string()
}

/// The largest column width the manual-status label cell may claim on the full
/// sidebar, so a long user-defined label name cannot crowd out the branch name;
/// a longer label is clipped to this. Only reserved when a visible session
/// actually carries a label (else the column is dropped entirely).
const LABEL_COL_MAX: usize = 12;

/// The [`Style`] a manual-status [`LabelColor`] paints in, resolved through the
/// semantic [`Palette`] so the label column follows a theme retune like every
/// other coloured element (`Gray` reads as a dim, unobtrusive tag).
fn label_style(color: LabelColor) -> Style {
    match color {
        LabelColor::Gray => Style::new().dim(),
        LabelColor::Red => Style::new().danger(),
        LabelColor::Green => Style::new().success(),
        LabelColor::Yellow => Style::new().warning(),
        LabelColor::Blue => Style::new().info(),
        LabelColor::Magenta => Style::new().feature(),
        LabelColor::Cyan => Style::new().accent(),
    }
}

/// The manual-status label column on a full-sidebar row: a leading space
/// separator then the colour-coded `<glyph> <name>` for `label`, clipped/padded
/// to fill `col` columns so the right-edge note field stays aligned down the list.
/// An unset row (or `col == 0`, when no visible session carries a label) fills
/// the same width with blanks, holding the column.
fn label_cell(label: Option<&SessionLabelDef>, col: usize) -> String {
    if col == 0 {
        return String::new();
    }
    // One column is the separating space before the label; the rest is its body.
    let inner = col - 1;
    let body = match label {
        None => " ".repeat(inner),
        Some(def) => {
            let text = format!("{} {}", def.glyph(), def.name);
            // Reserve the column by plain display width (full-width CJK = 2,
            // ambiguous glyphs = 1, matching what the terminal paints) so the
            // right-edge note field stays aligned down the list (see [`name_cell`]).
            let padded = pad_to_width(clip_to_width(&text, inner), inner);
            label_style(def.color).apply_to(padded).to_string()
        }
    };
    format!(" {body}")
}

/// The width the manual-status label column claims: the widest resolved
/// `<glyph> <name>` **in the master** (capped at [`LABEL_COL_MAX`]) plus one
/// separating space, or `0` when no visible session carries a label — in which
/// case the column is dropped and the sidebar is byte-for-byte what it was before
/// the feature.
///
/// The reserve is sized to the widest label the user *could* pick, not the widest
/// currently *shown*: cycling a session's label with `Tab` / the digit keys swaps
/// between defs of different lengths (`○ Todo` ↔ `▸ Doing` ↔ `✕ Blocked`), and
/// sizing to the visible set would resize the column — and shift every row's name
/// and label glyph — on each toggle. Holding the column at the master's widest
/// decouples its width from which label is applied, so only adding/removing the
/// first label moves anything (the same anti-shift rule the freshness and PR
/// columns follow; see [`TIME_RESERVE_WIDTH`] / [`PR_RESERVE_WIDTH`]).
fn label_col_width(list: &WorktreeList, master: &SessionLabelMaster) -> usize {
    let any_labelled = list.groups().iter().any(|g| {
        (0..g.worktrees().len()).any(|i| g.row_label_id(i).and_then(|id| master.get(id)).is_some())
    });
    if !any_labelled {
        return 0;
    }
    // A labelled row resolves through the master, so it is non-empty here and the
    // widest label always has a real width (a glyph plus a space, at least). Reserve
    // to it, capped at [`LABEL_COL_MAX`], plus one separating space.
    let widest = master
        .labels()
        .iter()
        .map(|def| console::measure_text_width(&format!("{} {}", def.glyph(), def.name)))
        .max()
        .unwrap_or(0)
        .min(LABEL_COL_MAX);
    widest + 1
}

/// The single colour-coded glyph for a session's manual-status label on the
/// collapsed rail (the icon from [`label_cell`] without the name), or `None` when
/// the session carries no label — the rail then keeps its agent / kind glyph.
fn rail_label_glyph(label: Option<&SessionLabelDef>) -> Option<String> {
    label.map(|def| label_style(def.color).apply_to(def.glyph()).to_string())
}

/// The state of a session's embedded agent, shown by an icon on the row's first
/// line and spelled out on its detail line.
/// Nerd Font glyph flagging that the row's detail line reports an **AI agent's**
/// state (rather than a plain shell). It leads the agent label — `<robot> ☾ ready`
/// — so an agent-backed session reads at a glance. Kept in the Font Awesome **4**
/// range (like the git-status glyphs above), where every Nerd Font — old or partial
/// — carries it: the FA5 `nf-fa-robot` (U+F544) is absent from those and shows a `?`
/// fallback (the same trap noted for [`MEM_ICON`]). Without any Nerd Font the phase
/// glyph and word beside it still carry the meaning.
const AGENT_ICON: char = '\u{f17b}'; // nf-fa-android — an AI agent (robot) drives this session

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

    /// The detail-line content: an [AI-agent glyph](AGENT_ICON), then a phase icon
    /// together with its label — `<robot> ☾ ready` (dim), `<robot> ▶ running`
    /// (green), `<robot> ◆ waiting` (yellow), or `<robot> ✓ done` (cyan) — clipped
    /// to `width`, or `None` when absent (the row has no agent in use). The AI glyph
    /// rides inside the same styled span, so it takes the phase's colour.
    fn detail(self, width: usize) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ☾ ready"), width))
                    .dim()
                    .to_string(),
            ),
            AgentState::Running => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ▶ running"), width))
                    .success()
                    .bold()
                    .to_string(),
            ),
            AgentState::Waiting => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ◆ waiting"), width))
                    .warning()
                    .bold()
                    .to_string(),
            ),
            AgentState::Done => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ✓ done"), width))
                    .accent()
                    .bold()
                    .to_string(),
            ),
        }
    }

    /// The full-sidebar detail-line label as **icons only**: the
    /// [AI-agent glyph](AGENT_ICON) then the phase icon, with the spelled-out word
    /// dropped — `<robot> ☾` (dim), `<robot> ▶` (green), `<robot> ◆` (yellow), or
    /// `<robot> ✓` (cyan) — clipped to `width`, or `None` when absent. Unlike
    /// [`detail`](Self::detail) (which keeps the word for the roomier right-pane
    /// header), the sidebar row shows only the glyphs so the state reads at a glance
    /// without spending columns on the label.
    fn icon_label(self, width: usize) -> Option<String> {
        match self {
            AgentState::Absent => None,
            AgentState::Ready => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ☾"), width))
                    .dim()
                    .to_string(),
            ),
            AgentState::Running => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ▶"), width))
                    .success()
                    .bold()
                    .to_string(),
            ),
            AgentState::Waiting => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ◆"), width))
                    .warning()
                    .bold()
                    .to_string(),
            ),
            AgentState::Done => Some(
                style(clip_to_width(&format!("{AGENT_ICON} ✓"), width))
                    .accent()
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
            AgentState::Running => Some(style("▶").success().bold().to_string()),
            AgentState::Waiting => Some(style("◆").warning().bold().to_string()),
            AgentState::Done => Some(style("✓").accent().bold().to_string()),
        }
    }
}

/// The one-cell usagi glyph used as the first line of a selected session's
/// gutter stack. It is in Nerd Font's PUA range, not an emoji, so it stays
/// one-column wide in the sidebar when the user's terminal font supports it.
const SELECTED_SESSION_GLYPH: char = '\u{f0907}';

/// The far-left gutter cell used by root/action rows. In 切替 (Switch) the
/// keyboard is on the list, so the selected non-session row shows a red `>`
/// cursor. The **active** session — the one subsequent commands operate on — is
/// marked by a green `▎` accent bar that runs down its row. Outside Switch there
/// is no cursor, so the gutter only ever carries the active bar; when the cursor
/// and the active row coincide in Switch, the cursor takes the column.
fn gutter_cell(selected: bool, active: bool, in_switch: bool) -> String {
    if in_switch && selected {
        style(">").danger().bold().to_string()
    } else if active {
        style("▎").success().bold().to_string()
    } else {
        " ".to_string()
    }
}

/// The three-line gutter stack for a selected session row:
///
/// ```text
/// 󰤇
/// ▎
/// ▎
/// ```
///
/// Session entries occupy three fixed rows, so the marker can span the whole
/// entry and remain visible after the side menu has selected the session. It is
/// red while the cursor is in 切替, then green after the session is selected. The
/// root and the "+ new session" action keep the compact `>` cursor in 切替
/// because they are not sessions and do not have a three-row body.
fn session_gutter_cell(selected: bool, active: bool, in_switch: bool, row: usize) -> String {
    if selected {
        let mark = if row == 0 {
            SELECTED_SESSION_GLYPH.to_string()
        } else {
            "▎".to_string()
        };
        let style = if in_switch {
            Style::new().danger().bold()
        } else {
            Style::new().success().bold()
        };
        style.apply_to(mark).to_string()
    } else {
        gutter_cell(false, active, in_switch)
    }
}

/// The branch / root name cell: clipped and padded to `width`, cyan, and bold
/// when the row is active or under the cursor.
fn name_cell(text: &str, width: usize, emphasised: bool) -> String {
    // Pad by *display* width, not char count: `format!("{:<width$}")` counts
    // `char`s, so a full-width (CJK) name — the app's own UI is Japanese — would
    // overrun the cell. `pad_to_width` measures display columns instead, counting
    // full-width CJK as two and ambiguous glyphs (`→ ↑ ★` …) as one, matching what
    // usagi's terminals paint, so the row's following fixed fields stay put.
    let padded = pad_to_width(clip_to_width(text, width), width);
    if emphasised {
        style(padded).accent().bold().to_string()
    } else {
        style(padded).accent().to_string()
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
/// in place of spelling out `CPU` / `MEM` — the same icon-led style the fixed
/// header/status labels use. They need a patched [Nerd Font](https://www.nerdfonts.com/)
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

/// The persistent row kept at the foot of the left pane to create a session
/// without remembering the `c` shortcut. It is a navigation target, not a
/// session, and turns into the inline `+ new: <name>` input when activated.
const CREATE_ROW_LABEL: &str = "+ new session";

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
        Load::Busy => style(field).warning(),
        Load::Hot => style(field).danger(),
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
    selected: bool,
    active: bool,
    in_switch: bool,
) -> String {
    let detail = style(clip_to_width(&resource_inline_label(usage), detail_width))
        .dim()
        .to_string();
    detail_line(&session_gutter_cell(selected, active, in_switch, 2), detail)
}

/// The line-1 memo cell at the row's right edge: a yellow [`NOTE_ICON`] when the
/// session carries a note, else blank. Three display columns wide either way (a
/// leading and trailing space frame the glyph) so the rows line up whether or not
/// a note is present — it reuses the column the old active marker left blank.
fn note_cell(has_note: bool) -> String {
    if has_note {
        format!(" {} ", style(NOTE_ICON).warning())
    } else {
        " ".repeat(ACTIVE_COL + 1)
    }
}

/// Builds a worktree's first two lines. The far-left gutter carries the selected
/// session's `󰤇` / `▎` stack, falling back to a green `▎` accent bar down the
/// active worktree's lines when the session is not selected; line 1 then has the freshness ("heat") kind dot
/// (`●`/`◐`/`○`, fading by time since the session was last touched, measured
/// against `now`), the branch name, and a memo marker (`NOTE_ICON`, when
/// `has_note`) at the right edge. Line 2 is indented under the name and, when an
/// agent is in use, carries its icons (`<robot> ☾` / `<robot> ▶` / `<robot> ◆` /
/// `<robot> ✓`).
#[allow(clippy::too_many_arguments)]
pub(super) fn worktree_row(
    worktree: &WorktreeState,
    label: &str,
    status_label: Option<&SessionLabelDef>,
    label_col: usize,
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
    // While inline-renaming this (selected) session in 切替, the label being typed
    // and the caret's byte offset into it: line 1's name cell becomes that
    // editable field in place, so the rename happens on the row itself rather than
    // in a separate input at the list foot.
    rename: Option<(&str, usize)>,
) -> (String, String) {
    let kind = kind_dot(heat_of(worktree.updated_at, now));
    let gutter = session_gutter_cell(selected, active, in_switch, 0);
    let line1 = if let Some((value, cursor)) = rename {
        // Inline rename: the session's own name line turns into the editable label
        // with a block caret. The gutter cursor and kind dot stay put so the row
        // does not shift, and the field runs across where the note and status
        // fields sat (dropped while editing) so a longer name has room to type.
        let (before, after) = value.split_at(cursor);
        let field = widgets::block_caret(before, after, &Style::new().accent().bold());
        let field_width = name_width + label_col + ACTIVE_COL + 1;
        format!("{gutter} {kind} {}", clip_to_width(&field, field_width))
    } else {
        // The session's sidebar label (its custom display name, or the branch when
        // unset); a detached worktree with no label falls back to the placeholder.
        let name = if label.is_empty() {
            worktree.branch.as_deref().unwrap_or(DETACHED)
        } else {
            label
        };
        let branch = name_cell(name, name_width, active || selected);
        // The manual-status label sits between the branch name and the memo marker
        // in its own fixed-width column (blank when this row has none, dropped
        // entirely when no visible session carries a label — `label_col == 0`), so
        // the note field stays aligned down the list.
        let status_tag = label_cell(status_label, label_col);
        // Three columns at the row's right edge (the old active-marker cell, now
        // home to the memo marker — the active bar lives in the gutter). The cell is
        // a constant width whether or not a note is present.
        let note = note_cell(has_note);
        format!("{gutter} {kind} {branch}{status_tag}{note}")
    };

    // Line 2 spells out the agent state with its icon (blank when absent) on the
    // left, and a right-aligned cluster of the freshness label (`Nm ago`), the
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
    let line2 = detail_line(&session_gutter_cell(selected, active, in_switch, 1), detail);
    (line1, line2)
}

/// A compact, dimmed freshness label for how long ago `then` was relative to
/// `now`: `now` under a minute, then `Nm ago` / `Nh ago` / `Nd ago`. A `then`
/// in the future (clock skew) clamps to `now`. Shown on line 2 so a glance
/// tells the stale sessions from the freshly-touched ones. The minute unit is
/// abbreviated to a single `m` (not `min`) to keep the column narrow and spend
/// the freed width on the rest of the detail line.
fn relative_time(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    let label = if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
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
    /// Display width of the freshness (`Nm ago`) cell; 0 drops it. Reserved at
    /// [`TIME_RESERVE_WIDTH`] so the cell keeps a constant width as a session ages
    /// across the `now` / `Nm ago` / `Nh ago` buckets — otherwise the label's
    /// width would wander with the clock, flipping the "drop when narrow" decision
    /// and shifting the detail line purely with the passage of time.
    time: usize,
    /// Digit width of the `↑N` (ahead) count; 0 = no visible session is ahead.
    ahead: usize,
    /// Digit width of the `↓N` (behind) count; 0 = no visible session is behind.
    behind: usize,
    /// Digit widths of the diff `+N` / `-M` counts; `added == 0` drops the badge.
    added: usize,
    removed: usize,
    /// Display width of the `<icon> <count>` PR badge (the glyph, a space, and the
    /// widest count's digits). Reserved at [`PR_RESERVE_WIDTH`] even when no visible
    /// session has a PR, so the column never collapses and shifts the diff beside it.
    pr: usize,
}

/// Nerd Font glyph leading the pull-request badge — a git pull-request icon in
/// place of spelling out `PR`, the same icon-led style the header/status and
/// resource fields use. Needs a patched [Nerd Font](https://www.nerdfonts.com/) to render;
/// without one the terminal shows a fallback box, but the count beside it still
/// carries the meaning.
pub(super) const PR_ICON: char = '\u{ea64}'; // nf-cod-git_pull_request

/// The fixed-width pull-request cell for a worktree's [`PrLink`]s: a single
/// `<icon> <count>` badge (soft link blue, underlined to read as a link) — the PR glyph and
/// how many PRs the session carries — right-aligned in `width` display columns so
/// the badges line up down the list. Folding several PRs into one count keeps the
/// detail line from being crowded out by a long `#442 #447 …` run (the full list is
/// one badge click away; see [`pr_popup_placement`]). A row with no PR fills the
/// same width with blanks, holding the column — which stays reserved
/// ([`PR_RESERVE_WIDTH`]) even when no visible session has a PR, so the column never
/// collapses and shifts the diff beside it.
fn pr_cell(prs: &[PrLink], width: usize) -> String {
    if prs.is_empty() {
        return " ".repeat(width);
    }
    let badge = style(format!("{PR_ICON} {}", prs.len()))
        .info()
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

/// The PR column is reserved at (at least) this width on every render — the glyph,
/// a space, and a single-digit count — even when no visible session currently
/// carries a PR. Holding the slot open keeps the `+N -M` diff (and the freshness /
/// commit fields to its left) from shifting right when a session gains or loses its
/// last PR: reserving space for content that may appear is the sidebar's standing
/// rule against layout shift (the note and status columns do the same), and the PR
/// badge follows it. A wider count (two+ digits) still grows the column, but the
/// common appear/disappear no longer moves anything.
const PR_RESERVE_WIDTH: usize = 3;

/// The freshness column is reserved at (at least) this width on every render — the
/// display width of the widest label the [`relative_time`] format produces,
/// `59m ago` (7 columns), which also holds every `Nh ago` / `Nd ago` up to
/// thousands of days. Unlike the diff / commit / PR fields, which only change when
/// the user commits or opens a PR, the freshness label's width changes on its own
/// as the clock advances (`now` → `12m ago` → `1h ago`). Sizing the column to the
/// live label would let that width wander between frames — flipping the "drop the
/// freshness first when narrow" decision and shifting the whole detail line purely
/// with the passage of time (a cumulative layout shift). Holding the slot at a
/// constant width decouples the layout from the clock, the same anti-shift rule the
/// PR column follows ([`PR_RESERVE_WIDTH`]).
const TIME_RESERVE_WIDTH: usize = 7;

/// Columns the `↑` / `↓` commit-divergence arrows occupy: one each. The arrows are
/// East Asian *Ambiguous*, which usagi's terminals paint one column wide — the
/// width [`console::measure_text_width`] counts and the width every sidebar cell is
/// reserved and clipped at (see [`name_cell`]). Keeping line 2's `↑N ↓M` math on
/// that same plain width is what pins the `│` divider to `left_w` on the detail
/// rows instead of jogging it left.
const COMMIT_ARROW_WIDTH: usize = 1;

impl DetailCols {
    /// Width of the `↑N ↓M` commit cell — only the sides some visible session uses
    /// are reserved (a pane with nothing behind spends no columns on `↓`), with a
    /// one-space gap when both sides are present. Each arrow is
    /// [`COMMIT_ARROW_WIDTH`] column (plain width, matching the terminal).
    fn commits_width(self) -> usize {
        let up = if self.ahead > 0 {
            COMMIT_ARROW_WIDTH + self.ahead
        } else {
            0
        };
        let down = if self.behind > 0 {
            COMMIT_ARROW_WIDTH + self.behind
        } else {
            0
        };
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
    // Always hold the PR slot open (even when no visible session has a PR) so a
    // session gaining or losing its last PR never shifts the diff to its left.
    cols.pr = cols.pr.max(PR_RESERVE_WIDTH);
    // Hold the freshness slot at a constant width so the label growing or shrinking
    // as a session ages (`now` → `12m ago` → `1h ago`) never changes the column —
    // otherwise the passing clock alone would flip the trim decision below and shift
    // the detail line. The per-session max above still wins for the (unreachable in
    // practice) label wider than the reserve.
    cols.time = cols.time.max(TIME_RESERVE_WIDTH);
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
/// edges line up down the list. Width is measured with [`console::measure_text_width`]
/// (plain display width), matching the columns the detail line reserves for each
/// field — the `↑` / `↓` arrows included, painted one column wide.
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
            let added = style(format!("+{:>added_w$}", diff.added)).success();
            let removed = style(format!("-{:>removed_w$}", diff.removed)).danger();
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
            style(format!("↑{ahead:>ahead_w$}")).accent().to_string()
        } else {
            " ".repeat(COMMIT_ARROW_WIDTH + ahead_w)
        }
    });
    let down = (behind_w > 0).then(|| {
        if behind > 0 {
            style(format!("↓{behind:>behind_w$}")).feature().to_string()
        } else {
            " ".repeat(COMMIT_ARROW_WIDTH + behind_w)
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
        return agent.icon_label(width).unwrap_or_default();
    }
    let cluster = cells.join(" ");
    // Measure and clip by plain display width, matching the columns the cluster's
    // fields reserve (the `↑` / `↓` arrows painted one wide — see
    // [`COMMIT_ARROW_WIDTH`]) and the width the sidebar composes the row at.
    let cluster_w = console::measure_text_width(&cluster);
    if cluster_w >= width {
        // No room for both: the cluster alone, clipped to the cell.
        return clip_to_width(&cluster, width);
    }
    // Reserve the cluster's columns (plus a one-space gap) and clip the agent
    // label to what's left, so it is styled already-clipped (clean ANSI) rather
    // than truncated after the fact.
    let agent = agent.icon_label(width - cluster_w - 1).unwrap_or_default();
    let pad = width - console::measure_text_width(&agent) - cluster_w;
    format!("{agent}{}{cluster}", " ".repeat(pad))
}

/// Builds the root's two lines: the workspace itself, belonging to no session.
/// The far-left gutter carries the `>` cursor (in 切替 (Switch)) or the green `▎`
/// active bar; line 1 then has a `⌂` kind icon, the [`ROOT_NAME`] label, and a
/// memo marker (`NOTE_ICON`, when `has_note`) — the root carries its own note,
/// like a session. Line 2 carries a `workspace root` detail.
pub(super) fn root_row(
    name_width: usize,
    label_col: usize,
    detail_width: usize,
    has_note: bool,
    selected: bool,
    active: bool,
    in_switch: bool,
) -> (String, String) {
    let kind = root_glyph();
    let name = name_cell(ROOT_NAME, name_width, active || selected);
    let gutter = gutter_cell(selected, active, in_switch);
    // The root belongs to no session, so it carries no manual-status label — but it
    // reserves the same (blank) label column as the sessions below, so the memo
    // field stays aligned.
    let status_tag = label_cell(None, label_col);
    // The same constant-width memo cell a worktree row uses, so the rows line up
    // with the sessions below whether or not a note is present.
    let note = note_cell(has_note);
    let line1 = format!("{gutter} {kind} {name}{status_tag}{note}");

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
        Heat::Fresh => style("●").success().to_string(),
        Heat::Warm => style("◐").to_string(),
        Heat::Cold => style("○").dim().to_string(),
    }
}

/// The workspace root's kind glyph (`⌂`, magenta) — shown in the slot where a
/// worktree shows its [`kind_dot`], by both the full sidebar ([`root_row`]) and
/// the collapsed rail ([`rail_pane`]).
fn root_glyph() -> String {
    style("⌂").feature().to_string()
}

/// Builds one collapsed-rail **entry** as the same [`SESSION_ROWS`] lines a
/// full-sidebar entry spans, so toggling the sidebar never moves a session to a
/// different row (no layout shift) — only the width changes. The glyphs form a
/// 2×2 grid beside the gutter, and a third (blank) row matches the full sidebar's
/// resource line — the narrow rail has no room for a CPU / memory figure, so it
/// keeps the row's height without the number:
///
/// ```text
/// ▎ <kind>           row 1: identity dot (⌂/●/○)
/// ▎ <label> <agent>  row 2: manual-status glyph + agent-state glyph (▶/◆/☾/✓)
/// ▎                  row 3: blank (the full sidebar's resource line has no rail twin)
/// ```
///
/// `label` / `agent` are blank when the session carries no manual-status label /
/// no live agent. The active `▎` bar runs down all three rows; a selected session
/// uses the same `󰤇` / `▎` / `▎` stack as the full sidebar, while the root keeps
/// the compact `>` cursor in 切替.
#[allow(clippy::too_many_arguments)]
fn rail_entry(
    selected: bool,
    session: bool,
    active: bool,
    in_switch: bool,
    kind: &str,
    label: Option<&str>,
    agent: Option<&str>,
) -> (String, String, String) {
    let gutter = if session {
        session_gutter_cell(selected, active, in_switch, 0)
    } else {
        gutter_cell(selected, active, in_switch)
    };
    let detail_gutter = if session {
        session_gutter_cell(selected, active, in_switch, 1)
    } else {
        gutter_cell(false, active, in_switch)
    };
    let resource_gutter = if session {
        session_gutter_cell(selected, active, in_switch, 2)
    } else {
        gutter_cell(false, active, in_switch)
    };
    // Columns: gutter @0, kind @2 — the kind dot alone on row 1.
    let top = pad_to_width(format!("{gutter} {kind}"), RAIL_WIDTH);
    // Row 2: the manual-status glyph sits at column 2 (under the kind dot, blank
    // when the session has no label) and the agent glyph at column 4.
    let detail = pad_to_width(
        format!(
            "{detail_gutter} {} {}",
            label.unwrap_or(" "),
            agent.unwrap_or(" ")
        ),
        RAIL_WIDTH,
    );
    // The resource row's rail twin: the active bar runs down it, but the rail has
    // no room for the CPU / memory figure, so the rest is blank.
    let resource = pad_to_width(resource_gutter, RAIL_WIDTH);
    (top, detail, resource)
}

/// The persistent create row in the full sidebar. It uses the same gutter grammar
/// as real rows (`>` only in 切替 when selected) but has no heat/status/resource
/// lines because it is an action target rather than a session. The row is still
/// always present at the list foot so keyboard focus and mouse clicks can enter
/// the create input from a visible affordance.
fn create_row(selected: bool, in_switch: bool, width: usize) -> String {
    let gutter = gutter_cell(selected, false, in_switch);
    let label = if selected && in_switch {
        style(CREATE_ROW_LABEL).green().bold().to_string()
    } else {
        style(CREATE_ROW_LABEL).green().to_string()
    };
    clip_to_width(&format!("{gutter} {label}"), width)
}

/// The rail twin of [`create_row`]: the `+` glyph in the same row position. The
/// input itself moves to the right pane while the sidebar is collapsed, but the
/// click / focus target remains visible at the rail's bottom.
fn rail_create_row(selected: bool, in_switch: bool) -> String {
    let gutter = gutter_cell(selected, false, in_switch);
    let label = if selected && in_switch {
        style("+").green().bold().to_string()
    } else {
        style("+").green().to_string()
    };
    pad_to_width(format!("{gutter} {label}"), RAIL_WIDTH)
}

fn push_unite_workspace_gap(win: &mut LineWindow, width: usize) {
    for _ in 0..UNITE_WORKSPACE_GAP_ROWS {
        win.push(pad_to_width(String::new(), width));
    }
}

fn line_hits_unite_workspace_gap(line: usize, cur: &mut usize) -> bool {
    if line < *cur + UNITE_WORKSPACE_GAP_ROWS {
        return true;
    }
    *cur += UNITE_WORKSPACE_GAP_ROWS;
    false
}

/// The number of body lines one workspace group occupies in the sidebar: the
/// (統合(unite)) inter-workspace gap and group header, the two-row root entry, the
/// one-row divider, then either the single empty-workspace message or
/// [`SESSION_ROWS`] rows per session. `with_headers` matches the full sidebar,
/// which heads each 統合 group with its name; the collapsed rail draws no header
/// (but keeps the gap), so it passes `false`. Shared by the create/rename insert
/// anchor ([`group_inline_insert_line`]) and the scroll maths
/// ([`sidebar_total_lines`] / [`selected_row_span`]) so every caller walks the one
/// layout [`left_pane`] / [`rail_pane`] draw.
fn group_block_rows(
    list: &WorktreeList,
    group_index: usize,
    worktree_count: usize,
    with_headers: bool,
) -> usize {
    let united = list.group_count() > 1;
    let gap = usize::from(united && group_index > 0) * UNITE_WORKSPACE_GAP_ROWS;
    // A folded workspace draws a single header line in place of its whole block.
    if list.is_collapsed(group_index) {
        return gap + 1;
    }
    let header = usize::from(united && with_headers);
    let body = if worktree_count == 0 {
        1
    } else {
        SESSION_ROWS * worktree_count
    };
    // Each expanded workspace ends with its own "+ new session" row (the final
    // `+ 1`), so creating a session lands in the workspace it sits under.
    gap + header + 2 + 1 + body + 1
}

/// The total body lines the sidebar draws for `list`: every group's block (each
/// expanded one already includes its own trailing "+ new session" row).
/// `with_headers` matches the sidebar variant (full draws 統合 group headers, the
/// rail does not). The scroll offset clamps against this so the window never runs
/// past the list's foot.
fn sidebar_total_lines(list: &WorktreeList, with_headers: bool) -> usize {
    list.groups()
        .iter()
        .enumerate()
        .map(|(i, g)| group_block_rows(list, i, g.worktrees().len(), with_headers))
        .sum::<usize>()
}

/// The `(start line, height)` the selected row occupies in the full-column
/// layout: the single folded header line of a collapsed workspace, the two-row
/// root entry, one [`SESSION_ROWS`] block per session, or a group's "+ new session"
/// row. Walks the same layout as [`sidebar_row_at_line_walk`] so the scroll offset
/// reveals exactly the row the renderer draws as selected.
fn selected_row_span(list: &WorktreeList, with_headers: bool) -> (usize, usize) {
    let united = list.group_count() > 1;
    let sel = list.selected_index();
    let mut cur = 0usize;
    let mut flat = 0usize;
    for (g, group) in list.groups().iter().enumerate() {
        if united && g > 0 {
            cur += UNITE_WORKSPACE_GAP_ROWS;
        }
        if list.is_collapsed(g) {
            if flat == sel {
                return (cur, 1); // the folded header line (the root slot)
            }
            cur += 1;
            flat += 1;
            continue;
        }
        if with_headers && united {
            cur += 1;
        }
        if flat == sel {
            return (cur, 2); // the root entry (its divider is not part of the row)
        }
        cur += 2 + 1; // root entry + divider
        flat += 1;
        if group.worktrees().is_empty() {
            cur += 1; // the empty-workspace message
        } else {
            for _ in group.worktrees() {
                if flat == sel {
                    return (cur, SESSION_ROWS);
                }
                cur += SESSION_ROWS;
                flat += 1;
            }
        }
        // The group's own "+ new session" row.
        if flat == sel {
            return (cur, 1);
        }
        cur += 1;
        flat += 1;
    }
    (cur, 1)
}

/// The body-line scroll offset for a sidebar taller than its `viewport_rows`,
/// chosen so the selected row stays visible. Zero while the list fits (top
/// pinned); once the selected row's foot drops below the fold it scrolls just far
/// enough to reveal that row at the bottom, clamped so the window never runs past
/// the list's end. A pure function of the list, the sidebar variant, and the
/// viewport height, so the renderer ([`left_pane`] / [`rail_pane`]) and every
/// hit-test compute the identical offset and never disagree on what is on screen.
pub(super) fn sidebar_scroll(
    list: &WorktreeList,
    with_headers: bool,
    viewport_rows: usize,
) -> usize {
    let total = sidebar_total_lines(list, with_headers);
    if viewport_rows == 0 || total <= viewport_rows {
        return 0;
    }
    let (start, len) = selected_row_span(list, with_headers);
    let end = start + len;
    let scroll = end.saturating_sub(viewport_rows);
    scroll.min(total - viewport_rows)
}

/// Accumulates a scrolled window of body lines: only lines whose full-column
/// index lands in `[scroll, scroll + cap)` are kept, so the sidebar can show a
/// slice of a list taller than the pane while the builders still walk the whole
/// layout — keeping every row's flat index (and therefore its selected / active
/// styling) correct. Lines above the window are counted but not stored; a caller
/// skips *building* an entire off-window entry via [`Self::above`] / [`Self::full`]
/// so the per-frame styling cost stays bound to the visible rows even with many
/// sessions open.
struct LineWindow {
    scroll: usize,
    cap: usize,
    seen: usize,
    out: Vec<String>,
}

impl LineWindow {
    fn new(scroll: usize, cap: usize) -> Self {
        Self {
            scroll,
            cap,
            seen: 0,
            out: Vec::new(),
        }
    }

    /// Record one built line, keeping it only when it falls inside the window.
    fn push(&mut self, line: String) {
        if self.seen >= self.scroll && self.out.len() < self.cap {
            self.out.push(line);
        }
        self.seen += 1;
    }

    /// Advance past `n` lines without building them (they sit above the window).
    fn skip(&mut self, n: usize) {
        self.seen += n;
    }

    /// Whether the next `n` lines all sit above the window, so the caller can skip
    /// building them and just [`Self::skip`] ahead.
    fn above(&self, n: usize) -> bool {
        self.seen + n <= self.scroll
    }

    /// Whether the window is already filled — every remaining line is below it, so
    /// the caller can stop building entirely.
    fn full(&self) -> bool {
        self.seen >= self.scroll + self.cap
    }

    fn into_lines(self) -> Vec<String> {
        self.out
    }
}

/// Builds the collapsed-rail sidebar ([`Sidebar::Rail`]): the root entry first, a
/// divider, then one entry per worktree — each the same two rows as the full
/// sidebar (kind glyph on row 1, agent state on row 2), so the rail and the full
/// list share the exact same row layout and toggling between them only changes
/// the width. The active session keeps its green `▎` gutter bar (down both rows)
/// and, in 切替, the `>` cursor and the dimming of the other entries, so the rail
/// still shows which session is selected and what its agent is doing without
/// spelling out their names.
#[allow(clippy::too_many_arguments)]
fn rail_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
    label_master: &SessionLabelMaster,
    rows: usize,
    in_switch: bool,
    now: DateTime<Utc>,
) -> Vec<String> {
    let root = root_glyph();
    // Scroll so the selected entry stays on screen once the list outgrows the rail
    // (the rail draws no 統合 group header, so `with_headers` is false).
    let scroll = sidebar_scroll(list, false, rows);
    let mut win = LineWindow::new(scroll, rows);
    // The flat selectable-row index, matching `WorktreeList`'s row space (the
    // 統合 group separators are pure decoration and do not advance it).
    let mut flat_row = 0usize;
    let united = list.group_count() > 1;
    'groups: for (g, group) in list.groups().iter().enumerate() {
        if win.full() {
            break;
        }
        // In 統合(unite) mode two blank rows separate each workspace's block.
        if united && g > 0 {
            push_unite_workspace_gap(&mut win, RAIL_WIDTH);
        }
        // A folded workspace collapses to a single fold-marker line (its root slot),
        // matching the full sidebar's [`collapsed_group_row`] so toggling the rail
        // never shifts which row a workspace sits on.
        if list.is_collapsed(g) {
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let mut row = rail_collapsed_group_row(selected, active, in_switch);
            if in_switch && !selected {
                row = dim_row(&row);
            }
            win.push(row);
            flat_row += 1;
            continue;
        }
        // The root entry is two rows (then a divider), matching the full sidebar's
        // [`root_row`]; only worktree entries carry the third resource row, so the
        // root drops the rail entry's (blank) third line.
        let (mut root_top, mut root_detail, _) = rail_entry(
            flat_row == list.selected_index(),
            false,
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
        win.push(root_top);
        win.push(root_detail);
        flat_row += 1;
        win.push(style("─".repeat(RAIL_WIDTH)).dim().to_string());
        if group.worktrees().is_empty() {
            // Mirror the full sidebar's single empty-message row so the row count
            // matches and toggling never shifts the layout.
            win.push(pad_to_width(String::new(), RAIL_WIDTH));
            let selected = flat_row == list.selected_index();
            let mut row = rail_create_row(selected, in_switch);
            if in_switch && !selected {
                row = dim_row(&row);
            }
            win.push(row);
            flat_row += 1;
            continue;
        }
        for (i, w) in group.worktrees().iter().enumerate() {
            // Stop once the window is filled: everything past it is off screen, so
            // building it is wasted work (same bound as the full sidebar).
            if win.full() {
                break 'groups;
            }
            // The whole entry sits above the scrolled window — skip building it and
            // just advance the line / flat-row counters.
            if win.above(SESSION_ROWS) {
                win.skip(SESSION_ROWS);
                flat_row += 1;
                continue;
            }
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let kind = kind_dot(heat_of(w.updated_at, now));
            let label = rail_label_glyph(group.row_label_id(i).and_then(|id| label_master.get(id)));
            let agent = AgentState::from_flags(
                live.contains(&w.path),
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
            )
            .rail_icon();
            let (mut top, mut detail, mut resource) = rail_entry(
                selected,
                true,
                active,
                in_switch,
                &kind,
                label.as_deref(),
                agent.as_deref(),
            );
            if in_switch && !selected {
                top = dim_row(&top);
                detail = dim_row(&detail);
                resource = dim_row(&resource);
            }
            win.push(top);
            win.push(detail);
            win.push(resource);
            flat_row += 1;
        }
        // The group's own "+ new session" row (rail twin), at the foot of its
        // sessions — one per workspace in 統合(unite) mode.
        if win.full() {
            break;
        }
        let selected = flat_row == list.selected_index();
        let mut row = rail_create_row(selected, in_switch);
        if in_switch && !selected {
            row = dim_row(&row);
        }
        win.push(row);
        flat_row += 1;
    }
    win.into_lines()
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
        // A folded workspace is a single header line, the group's root slot.
        if list.is_collapsed(g) {
            if line == cur {
                return Some(flat);
            }
            cur += 1;
            flat += 1;
            continue;
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
        } else {
            for _ in group.worktrees() {
                if line >= cur && line < cur + SESSION_ROWS {
                    return Some(flat);
                }
                cur += SESSION_ROWS;
                flat += 1;
            }
        }
        // The group's own "+ new session" row.
        if line == cur {
            return Some(flat);
        }
        cur += 1;
        flat += 1;
    }
    None
}

/// The flat selectable row a 0-based *screen* body `line` maps to, given the
/// sidebar's current `scroll` offset (see [`sidebar_scroll`]). The screen line is
/// lifted back into the full-column layout by adding `scroll` before the walk, so a
/// click resolves to the right row even when the list is scrolled.
pub(super) fn sidebar_row_at_line_for_sidebar(
    list: &WorktreeList,
    line: usize,
    sidebar: Sidebar,
    scroll: usize,
) -> Option<usize> {
    let line = line + scroll;
    match sidebar {
        Sidebar::Full => sidebar_row_at_line_walk(list, line, true),
        Sidebar::Rail => sidebar_row_at_line_walk(list, line, false),
    }
}

/// The 0-based body line of `group`'s own "+ new session" row — the last line of
/// its block. 切替's inline create input replaces this row so it renders at the
/// foot of the targeted workspace's sessions (before the next group's gap/header)
/// rather than at the foot of the whole column, which matters in 統合(unite) mode
/// where several workspaces stack. The create flow expands a folded group first,
/// so `group` is always expanded here. Walks the same layout as
/// [`sidebar_row_at_line_for_sidebar`].
pub(super) fn group_inline_insert_line(list: &WorktreeList, group: usize) -> usize {
    let before: usize = list
        .groups()
        .iter()
        .enumerate()
        .take(group)
        .map(|(i, g)| group_block_rows(list, i, g.worktrees().len(), true))
        .sum();
    let block = list
        .groups()
        .get(group)
        .map(|g| group_block_rows(list, group, g.worktrees().len(), true))
        .unwrap_or(0);
    // The create row is the final line of the group's block.
    before + block.saturating_sub(1)
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

/// Builds the single line a **folded** 統合(unite) workspace draws in place of its
/// whole block: the workspace name behind a left bar with a `▸` fold marker and a
/// `(N)` session count, so a collapsed workspace still shows what it holds. It is
/// the group's navigable root slot, so it carries the same gutter cursor / active
/// bar the root entry would ([`gutter_cell`]); the caller dims it in 切替 when it
/// is not the selected row, exactly like every other row.
fn collapsed_group_row(
    name: &str,
    sessions: usize,
    selected: bool,
    active: bool,
    in_switch: bool,
    width: usize,
) -> String {
    let gutter = gutter_cell(selected, active, in_switch);
    // Reserve the gutter cell + its trailing space, then clip-then-style the header
    // text (matching [`group_header`], whose clip runs before styling).
    let text = format!("▸ {name}  ({sessions})");
    let head = style(clip_to_width(&text, width.saturating_sub(2))).bold();
    format!("{gutter} {head}")
}

/// The rail twin of [`collapsed_group_row`]: the fold marker `▸` in the gutter
/// position, keeping the folded workspace visible (and its root slot clickable) in
/// the narrow rail, which has no room for the name or count.
fn rail_collapsed_group_row(selected: bool, active: bool, in_switch: bool) -> String {
    let gutter = gutter_cell(selected, active, in_switch);
    let marker = style("▸").bold();
    pad_to_width(format!("{gutter} {marker}"), RAIL_WIDTH)
}

/// Builds the left pane: the root entry (two lines) first, then a divider, then
/// one [`SESSION_ROWS`]-line entry per worktree — an identity line, a detail
/// line, and a CPU / memory line (`CPU 0%  MEM 0MB` when unsampled, so the entry
/// is a fixed height and the list never reflows) — or the empty message when none
/// are recorded, trimmed to the available `rows`. `live` holds
/// the worktree paths with an embedded session (a live-but-idle one shows the
/// sidebar icons `<robot> ☾`), `running` the ones working a turn (`<robot> ▶`),
/// `waiting` the ones whose agent awaits input (`<robot> ◆`), and `done` the
/// finished ones (`<robot> ✓`); precedence is done > waiting > running > ready.
/// When `in_switch`
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
    label_master: &SessionLabelMaster,
    left_w: usize,
    rows: usize,
    in_switch: bool,
    sidebar: Sidebar,
    now: DateTime<Utc>,
    // The inline rename being typed (its label and caret offset) when 切替's rename
    // input is open, so the selected session's name line renders as that editable
    // field in place. `None` when not renaming. Only the full sidebar edits inline;
    // the rail has no room and renders the input in the right pane instead.
    rename: Option<(&str, usize)>,
) -> Vec<String> {
    if sidebar == Sidebar::Rail {
        // The 5-column rail has no room for a CPU / memory figure, so the rail
        // shows only the agent glyph; the resource line belongs to the full list.
        return rail_pane(
            list,
            live,
            running,
            waiting,
            done,
            label_master,
            rows,
            in_switch,
            now,
        );
    }
    // The manual-status label column, reserved across every group's rows so the
    // labels line up; `0` (no visible session carries a label) drops it, leaving
    // the sidebar exactly as it was before the feature.
    let label_col = label_col_width(list, label_master);
    // Line 1: prefix + name + the manual-status label column + the right-edge memo
    // cell (the old active-marker cell + a space).
    let name_width = left_w.saturating_sub(NAME_PREFIX + label_col + ACTIVE_COL + 1);
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
            if let Some(label) = agent.icon_label(detail_width) {
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

    // Scroll so the selected row stays on screen once the list outgrows the pane;
    // the window keeps only the visible slice while the loop walks the whole layout
    // so every flat index (and its selected / active styling) stays correct.
    let scroll = sidebar_scroll(list, true, rows);
    let mut win = LineWindow::new(scroll, rows);
    // The flat selectable-row index, matching `WorktreeList`'s row space (group
    // headers are pure decoration and do not advance it).
    let mut flat_row = 0usize;
    'groups: for (g, group) in list.groups().iter().enumerate() {
        if win.full() {
            break;
        }
        if united && g > 0 {
            push_unite_workspace_gap(&mut win, left_w);
        }
        // A folded workspace collapses to its single header line (its root slot).
        if list.is_collapsed(g) {
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let mut row = collapsed_group_row(
                group.name(),
                group.worktrees().len(),
                selected,
                active,
                in_switch,
                left_w,
            );
            if in_switch && !selected {
                row = dim_row(&row);
            }
            win.push(row);
            flat_row += 1;
            continue;
        }
        if united {
            win.push(group_header(group.name(), left_w));
        }
        let (mut root_top, mut root_detail) = root_row(
            name_width,
            label_col,
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
        win.push(root_top);
        win.push(root_detail);
        flat_row += 1;
        win.push(
            style(format!("{indent}{}", "─".repeat(inner_w)))
                .dim()
                .to_string(),
        );
        if group.worktrees().is_empty() {
            // No sessions yet in this workspace — show the empty message under the
            // divider, then the group's own create row so a session can be started
            // straight into this (otherwise empty) workspace.
            win.push(
                style(format!("{indent}{}", clip_to_width(EMPTY_MESSAGE, inner_w)))
                    .dim()
                    .to_string(),
            );
            let selected = flat_row == list.selected_index();
            let mut row = create_row(selected, in_switch, left_w);
            if in_switch && !selected {
                row = dim_row(&row);
            }
            win.push(row);
            flat_row += 1;
            continue;
        }
        for (i, w) in group.worktrees().iter().enumerate() {
            // Stop once the window is filled: everything past it is off screen, so
            // building it is wasted work. With many sessions open this bounds the
            // per-frame cost (styling, dimming, ANSI rewriting) to the visible rows.
            if win.full() {
                break 'groups;
            }
            // The whole entry sits above the scrolled window — skip building it and
            // just advance the line / flat-row counters.
            if win.above(SESSION_ROWS) {
                win.skip(SESSION_ROWS);
                flat_row += 1;
                continue;
            }
            let selected = flat_row == list.selected_index();
            let active = flat_row == list.active_index();
            let status_label = group.row_label_id(i).and_then(|id| label_master.get(id));
            let (mut top, mut detail) = worktree_row(
                w,
                group.display_label(i),
                status_label,
                label_col,
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
                // The rename input targets the selected session, so its editable
                // label rides that row only; every other row renders normally.
                if selected { rename } else { None },
            );
            // Every session draws a third CPU / memory line at a fixed height, so
            // the list never reflows as a session goes live or idle. An unsampled
            // session shows `CPU 0%  MEM 0MB` (a default usage) rather than dropping
            // the row.
            let usage = resources.get(&w.path).copied().unwrap_or_default();
            let mut resource = resource_line(usage, detail_width, selected, active, in_switch);
            if in_switch && !selected {
                top = dim_row(&top);
                detail = dim_row(&detail);
                resource = dim_row(&resource);
            }
            win.push(top);
            win.push(detail);
            win.push(resource);
            flat_row += 1;
        }
        // The group's own "+ new session" row, at the foot of its sessions, so a
        // create lands in the workspace it sits under (統合(unite) mode stacks one
        // per workspace instead of a single row at the whole column's foot).
        if win.full() {
            break;
        }
        let selected = flat_row == list.selected_index();
        let mut row = create_row(selected, in_switch, left_w);
        if in_switch && !selected {
            row = dim_row(&row);
        }
        win.push(row);
        flat_row += 1;
    }
    win.into_lines()
}

/// Renders one log line, coloured by kind. Command lines get a `❯` prompt.
pub(super) fn log_line(line: &LogLine, width: usize) -> String {
    let raw = match line.kind {
        LineKind::Command => format!("❯ {}", line.text),
        _ => line.text.clone(),
    };
    let clipped = clip_to_width(&raw, width);
    match line.kind {
        LineKind::Command => style(clipped).accent().bold().to_string(),
        LineKind::Output => clipped,
        LineKind::Error => style(clipped).danger().to_string(),
        LineKind::Notice => style(clipped).warning().to_string(),
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
            marker.push_str(&style("▔".repeat(width)).accent().bold().to_string());
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

/// The tab a pointer event at the 0-based screen (`col`, `row`) lands on while
/// 没入 (Attached), including the active tab. Returns `None` for an event off the
/// strip rows, off every chip (the indent, the gaps, past the last chip), or
/// when no tab strip is published.
pub(in crate::presentation::tui::home) fn attached_tab_hit(
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
    tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))
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
    let target = attached_tab_hit(state, col, row, geo)?;
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

pub(in crate::presentation::tui::home) fn focus_tab_hit(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    focus_tab_hit_inner(state, col, row, raw_height, raw_width)
}

fn focus_tab_hit_inner(
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
    tab_chip_ranges(&header, &combined)
        .into_iter()
        .position(|range| range.contains(&rel_col))
        .filter(|target| *target < strip.labels.len())
}

/// The live-pane tab (0-based, matching [`TabStrip::labels`]) a left click at the
/// 0-based screen (`col`, `row`) lands on while 切替 (Switch), or `None` when the
/// click is not on a changeable pane tab.
///
/// 切替's right pane draws the highlighted session's preview and exposes the
/// same tab strip that `←`/`→` navigate by keyboard. This mirrors the renderer's
/// header/geometry so a click on an inactive chip moves the preview — and the
/// pane that `Enter` re-attaches — to that tab without entering 在席 first.
pub(in crate::presentation::tui::home) fn switch_tab_at(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    if strip.labels.is_empty() {
        return None;
    }
    let (header, live) = switch_preview_header(state);
    if !live {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    let target = tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))?;
    // Clicking the active tab is a no-op; inactive chips select that pane.
    (target != strip.active).then_some(target)
}

pub(in crate::presentation::tui::home) fn switch_tab_hit(
    state: &HomeState,
    col: u16,
    row: u16,
    raw_height: usize,
    raw_width: usize,
) -> Option<usize> {
    let strip = state.terminal_tabs()?;
    if strip.labels.is_empty() {
        return None;
    }
    let (header, live) = switch_preview_header(state);
    if !live {
        return None;
    }
    let geo = super::terminal_geometry(raw_height, raw_width, state.sidebar());
    if row < geo.origin_row || row >= geo.origin_row + super::TAB_BAR_ROWS as u16 {
        return None;
    }
    let rel_col = col.checked_sub(geo.origin_col)? as usize;
    tab_chip_ranges(&header, strip)
        .into_iter()
        .position(|range| range.contains(&rel_col))
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
    let body_rows = super::body_rows_for(height);
    let screen_line = (row - BODY_TOP) as usize;
    if screen_line >= body_rows {
        return None;
    }
    // Lift the screen line back into the full-column layout the entries are walked
    // in, so the badge resolves correctly when the list is scrolled.
    let scroll = sidebar_scroll(state.list(), true, body_rows);
    let line = screen_line + scroll;
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
    let (_, entry_line) = full_sidebar_worktree_entries(state.list())
        .into_iter()
        .find(|&(global, _)| global == idx)?;
    // Lift the entry into screen space by the sidebar's scroll offset; if the
    // session's row has scrolled off the top of the pane, its badge is not on
    // screen, so pin nothing rather than float the box over an unrelated row.
    let body_rows = super::body_rows_for(height);
    let scroll = sidebar_scroll(state.list(), true, body_rows);
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
/// Wide enough for the longest agent label with its leading [AI glyph](AGENT_ICON)
/// and phase icon: `<robot> ▶ running` measures 11 columns.
const HEADER_AGENT_COL: usize = 11;
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
    // Measure by plain display width (matching [`name_cell`]) so a name carrying
    // wide characters keeps the identity block a constant width instead of pushing
    // the status / agent fields — and the tab strip laid beside it — sideways.
    let name = pad_to_width(
        style(clip_to_width(name, HEADER_NAME_COL))
            .accent()
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
        style("›").danger().bold().to_string()
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
        style(format!("{name:<9}")).accent().bold().to_string()
    } else {
        style(format!("{name:<9}")).accent().to_string()
    };
    let desc_budget = width.saturating_sub(HINT_INDENT + 9);
    let desc = style(clip_to_width(desc, desc_budget)).dim();
    clip_to_width(&format!("  {marker} {name}{desc}"), width)
}

/// The 在席 menu's `agent` row: like a plain command row but its description
/// names the agent a plain launch uses (the configured default) and carries an
/// expand affordance — `▾` while the picker is open, `▸` when the cursor is on
/// this row and it can open (more than one CLI installed). When more than one
/// CLI is installed but the cursor is elsewhere the slot is held with blanks so
/// the description never shifts as the cursor moves on/off the row; with a
/// single CLI (the chevron can never show) no slot is reserved.
fn focus_agent_command_row(state: &HomeState, selected: bool, width: usize) -> String {
    let chevron = if state.focus_menu_agent_cursor().is_some() {
        "▾ "
    } else if state.focus_menu_agent_can_expand() {
        "▸ "
    } else if state.installed_agents().len() > 1 {
        "  "
    } else {
        ""
    };
    let desc = format!("{chevron}Launch {}", state.default_agent().display_name());
    menu_row("agent", &desc, selected, width)
}

/// The 在席 menu's `terminal` row: like a plain command row but it can expand
/// into the `open` / `new` picker. `open` is the default and preserves the
/// existing embedded-tab behaviour. The row always reserves the same 2-column
/// chevron slot as `agent` and `close` so descriptions never shift (no CLS).
fn focus_terminal_command_row(state: &HomeState, selected: bool, width: usize) -> String {
    let chevron = if state.focus_menu_terminal_expanded() {
        "▾ "
    } else if state.focus_menu_terminal_can_expand() {
        "▸ "
    } else {
        "  "
    };
    let desc = format!("{chevron}Open a shell");
    menu_row("terminal", &desc, selected, width)
}

/// One agent-picker sub-row, indented under the expanded `agent` row: a `›`
/// cursor on the highlighted CLI, its display name, and a dimmed `(default)` tag
/// on the configured agent.
fn focus_agent_pick_row(cli: AgentCli, selected: bool, is_default: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{:<10}", cli.display_name()))
            .accent()
            .bold()
            .to_string()
    } else {
        style(format!("{:<10}", cli.display_name()))
            .accent()
            .to_string()
    };
    let tag = if is_default {
        style("(default)").dim().to_string()
    } else {
        String::new()
    };
    clip_to_width(&format!("      {marker} {name}{tag}"), width)
}

/// The 在席 menu's `close` row: like a plain command row but carries a `▾`/`▸`
/// expand affordance to open the close picker (plain close vs. close --force) —
/// `▾` while the picker is open, `▸` when the cursor is on this row (it can
/// always expand). When the cursor is elsewhere the slot is held with blanks so
/// the description never shifts as the cursor moves on/off the row (no CLS),
/// mirroring the `agent` row.
fn focus_close_command_row(
    state: &HomeState,
    info: &CommandInfo,
    selected: bool,
    width: usize,
) -> String {
    let chevron = if state.focus_close_expanded() {
        "▾ "
    } else if state.focus_close_can_expand() {
        "▸ "
    } else {
        "  "
    };
    let desc = format!("{chevron}{}", info.description);
    menu_row(info.name, &desc, selected, width)
}

/// One close-picker sub-row, indented under the expanded `close` row: a `›`
/// cursor on the highlighted option, the command label, and a dimmed hint.
/// `force = false` → plain close; `force = true` → close --force.
fn focus_close_pick_row(force: bool, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let label = if force { "close --force" } else { "close" };
    let hint = if force {
        "(discard uncommitted changes)"
    } else {
        "(safe)"
    };
    let name = if selected {
        style(format!("{label:<14}")).cyan().bold().to_string()
    } else {
        style(format!("{label:<14}")).cyan().to_string()
    };
    let hint = style(hint).dim();
    clip_to_width(&format!("      {marker} {name}{hint}"), width)
}

/// One terminal-picker sub-row, indented under the expanded `terminal` row:
/// `open` adds an embedded usagi tab (the default), while `new` opens a native
/// terminal app rooted at the same directory.
fn focus_terminal_pick_row(action: &str, selected: bool, width: usize) -> String {
    let marker = menu_marker(selected);
    let name = if selected {
        style(format!("{action:<10}")).cyan().bold().to_string()
    } else {
        style(format!("{action:<10}")).cyan().to_string()
    };
    let desc = if action == "new" {
        "new terminal"
    } else {
        "add tab"
    };
    let tag = if action == "open" {
        style("(default)").dim().to_string()
    } else {
        style(desc).dim().to_string()
    };
    clip_to_width(&format!("      {marker} {name}{tag}"), width)
}

/// A hard floor on the 在席 (Focus) menu's command-area height, so even a tiny
/// menu keeps a usable window. The real target is the widest expansion (see
/// [`focus_menu_target`]); this only guards degenerate menus with fewer rows.
const FOCUS_MENU_MIN_VISIBLE: usize = 5;

/// The 在席 menu box's chrome rows around the windowed command area: the two box
/// borders plus the `Run a command:` label, the blank spacer, and the key hint.
/// The command window may take up to `avail_rows - FOCUS_MENU_CHROME` rows before
/// the box would overrun the right pane.
const FOCUS_MENU_CHROME: usize = 5;

/// The command-area height the 在席 menu reserves: the row count with the *most
/// sub-menu-heavy* picker fully expanded — the command rows plus the largest
/// picker's sub-rows. Fixing the window to this height means every picker opens in
/// place with no `↑/↓ N more` clipping and no layout shift, whatever is expanded.
/// Each command contributes its own picker's sub-row count when it can expand:
/// `agent` the installed CLIs (only when more than one, so a picker actually
/// opens), `terminal` its open/new actions, `close` its two options.
fn focus_menu_target(state: &HomeState, commands: &[CommandInfo]) -> usize {
    let widest_picker = commands
        .iter()
        .map(|info| match info.name {
            "agent" if state.installed_agents().len() > 1 => state.installed_agents().len(),
            "terminal" => state.focus_menu_terminal_actions().len(),
            "close" => 2,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    commands.len() + widest_picker
}

/// How many command rows the 在席 menu window shows for a pane `avail_rows` tall,
/// given the `target` height (the widest expansion, see [`focus_menu_target`]).
/// It reserves the full `target` — so every picker opens without scrolling and the
/// box never resizes as pickers open and close — capping only when the pane cannot
/// hold it (then the window scrolls), and never below [`FOCUS_MENU_MIN_VISIBLE`].
fn focus_menu_visible(target: usize, avail_rows: usize) -> usize {
    let max_fit = avail_rows.saturating_sub(FOCUS_MENU_CHROME);
    target.min(max_fit).max(FOCUS_MENU_MIN_VISIBLE)
}

/// Windows the 在席 menu's command `rows` to exactly `visible` output rows,
/// scrolled so the `active` row (the cursor, or the highlighted picker sub-row)
/// stays on screen. Rows hidden past an edge are summarised with a dim `↑ N` /
/// `↓ N` marker on the window's top / bottom row; rows that already fit are padded
/// with blanks. Either way the result is `visible` rows, so the box keeps the same
/// height whether or not an inline picker is expanded.
fn focus_menu_window(rows: Vec<String>, active: usize, visible: usize) -> Vec<String> {
    if rows.len() <= visible {
        let mut out = rows;
        out.resize(visible, String::new());
        return out;
    }
    let total = rows.len();
    // Reserve a marker row on each edge; the content window is what remains. The
    // clamp keeps `active` inside that window (centring it where there is room).
    let content = visible.saturating_sub(2).max(1);
    let start = active.saturating_sub(content / 2).min(total - content);
    let end = start + content;
    let mut out = Vec::with_capacity(visible);
    out.push(focus_menu_overflow(start, true));
    out.extend(rows[start..end].iter().cloned());
    out.push(focus_menu_overflow(total - end, false));
    out
}

/// A dim overflow marker for the windowed 在席 menu: `↑ N more` (`above`) or
/// `↓ N more` (below) when `hidden` rows sit past the window edge, or a blank row
/// when none do — so the window keeps its fixed height at the ends of the scroll.
fn focus_menu_overflow(hidden: usize, above: bool) -> String {
    if hidden == 0 {
        return String::new();
    }
    let arrow = if above { '↑' } else { '↓' };
    style(format!("  {arrow} {hidden} more")).dim().to_string()
}

/// The body of the 在席 (Focus) menu (no identity header): the `Run a command:`
/// label, one row per Session-scope command (`›` cursor on the highlighted one),
/// and a key hint. The command rows are windowed ([`focus_menu_window`]) to a
/// height that grows to fill the `avail_rows`-tall right pane, so a long picker
/// shows as many rows as fit before it scrolls rather than collapsing straight
/// into `↑/↓ N more` markers. Rendered as the body of the floating menu overlay
/// modal (see [`super::render_frame`] and [`HomeState::focus_action_overlay`]); the
/// `session:` identity rides the modal's title rather than a header line here.
pub(super) fn focus_menu_body(state: &HomeState, width: usize, avail_rows: usize) -> Vec<String> {
    let cursor = state.focus_menu_cursor();
    let expanded = state.focus_menu_expanded();
    let close_expanded = state.focus_close_expanded();
    let commands = state.focus_menu_commands();

    // Build the whole command area first — one row per command plus any expanded
    // picker's sub-rows — tracking the index of the *active* row (the cursor, or
    // the highlighted picker sub-row) so the window below can keep it on screen.
    let mut rows: Vec<String> = Vec::new();
    let mut active = 0usize;
    for (i, info) in commands.iter().enumerate() {
        let selected = i == cursor;
        if info.name == "agent" {
            // The `agent` row names the default CLI; when expanded, its installed
            // alternatives follow as indented picker sub-rows (案A).
            let agent_cursor = state.focus_menu_agent_cursor();
            if agent_cursor.is_none() && selected {
                active = rows.len();
            }
            rows.push(focus_agent_command_row(state, selected, width));
            if agent_cursor.is_some() {
                let default = state.default_agent();
                for (j, &cli) in state.installed_agents().iter().enumerate() {
                    if Some(j) == agent_cursor {
                        active = rows.len();
                    }
                    rows.push(focus_agent_pick_row(
                        cli,
                        Some(j) == agent_cursor,
                        cli == default,
                        width,
                    ));
                }
            }
        } else if info.name == "close" {
            // The `close` row carries a chevron affordance; when expanded the two
            // options (plain close and close --force) follow as sub-rows.
            if !close_expanded && selected {
                active = rows.len();
            }
            rows.push(focus_close_command_row(state, info, selected, width));
            if close_expanded {
                let close_cursor = state.focus_close_cursor();
                for j in 0..2usize {
                    if Some(j) == close_cursor {
                        active = rows.len();
                    }
                    rows.push(focus_close_pick_row(j == 1, Some(j) == close_cursor, width));
                }
            }
        } else if info.name == "terminal" {
            let terminal_expanded = state.focus_menu_terminal_expanded();
            if !terminal_expanded && selected {
                active = rows.len();
            }
            rows.push(focus_terminal_command_row(state, selected, width));
            if terminal_expanded {
                let terminal_cursor = state.focus_menu_terminal_cursor();
                for (j, &action) in state.focus_menu_terminal_actions().iter().enumerate() {
                    if Some(j) == terminal_cursor {
                        active = rows.len();
                    }
                    rows.push(focus_terminal_pick_row(
                        action,
                        Some(j) == terminal_cursor,
                        width,
                    ));
                }
            }
        } else {
            if selected {
                active = rows.len();
            }
            rows.push(focus_menu_row(info, selected, width));
        }
    }

    // A `/` filter that matches nothing leaves the command area blank; a dim
    // placeholder keeps the box from reading as a bug (an empty menu). Worded like
    // the Open Project picker's "No projects match the filter." for consistency.
    if commands.is_empty() {
        rows.push(style("  No commands match the filter.").dim().to_string());
    }

    // Window the command area to the widest expansion's height, so every picker
    // opens in place — its sub-rows shown, not collapsed into `↑/↓ N more` — and
    // the box never resizes as pickers open and close. Capped to the right pane so
    // it never overruns; only when the pane is too short does the window scroll.
    let visible = focus_menu_visible(focus_menu_target(state, &commands), avail_rows);
    let mut lines = vec![focus_menu_filter_line(state, width)];
    lines.extend(focus_menu_window(rows, active, visible));
    lines.push(String::new());
    // The hint is contextual: picker-navigation keys while any picker is open,
    // the `/` filter's own keys while it is live, a row-specific expand affordance
    // while the cursor can open one, else base.
    let hint = if close_expanded {
        "↑↓ move   Enter run   ← back".to_string()
    } else if expanded {
        "↑↓ move   Enter launch   ← back".to_string()
    } else if state.focus_menu_filtering() {
        "↑↓ move   Enter run   ⌫ edit   Esc clear".to_string()
    } else if state.focus_menu_agent_can_expand() {
        "↑↓ move   Enter run   → pick agent   / filter   t terminal   a agent".to_string()
    } else if state.focus_menu_terminal_can_expand() {
        "↑↓ move   Enter run   → pick terminal   / filter   t terminal   a agent".to_string()
    } else if state.focus_close_can_expand() {
        "↑↓ move   Enter run   → expand   / filter   t terminal   a agent".to_string()
    } else {
        "↑↓ move   Enter run   / filter   t terminal   a agent".to_string()
    };
    lines.push(style(hint).dim().to_string());
    lines
}

/// Rows the 在席 prompt reserves for its Session-scope hint, always filled to this
/// height (padded with blanks) so the box keeps a **fixed height** as the hint
/// changes while typing — no layout shift, the prompt sibling of the menu's
/// widest-expansion window ([`focus_menu_target`]) and the palette's
/// [`PALETTE_HINT_ROWS`](super::chrome). Sized to the tallest hint: a `usage` line
/// plus up to [`HINT_MAX`] examples.
const FOCUS_PROMPT_HINT_ROWS: usize = HINT_MAX + 1;

/// The 在席 prompt box's chrome rows around the reserved hint block: the two box
/// borders plus the `❯` command line and its blank spacer. The hint block is
/// capped to `avail_rows - FOCUS_PROMPT_CHROME` so a short pane never overruns.
const FOCUS_PROMPT_CHROME: usize = 4;

/// The 在席 menu's first line: the dim `Run a command:` label normally, or — while
/// a `/` filter is live — a `Filter: <query>` line that shows the typed text as the
/// list narrows beneath it. Mirrors the Open Project picker's filter bar (see
/// [`crate::presentation::tui::open`]) so the two search affordances read alike.
fn focus_menu_filter_line(state: &HomeState, width: usize) -> String {
    let Some(query) = state.focus_menu_filter() else {
        return style("Run a command:").dim().to_string();
    };
    let label = style("Filter:").dim();
    let value = if query.is_empty() {
        style(" type to filter").dim().to_string()
    } else {
        format!(" {}", style(query).accent())
    };
    clip_to_width(&format!("{label}{value}"), width)
}

/// The body of the 在席 (Focus) prompt surface (no identity header): the
/// session-scoped command line (`❯ <input>▏`) and a **fixed-height** Session-scope
/// hint block below it, so the box never resizes as the hint changes while typing.
/// Rendered as the body of the floating prompt overlay modal (the prompt sibling
/// of [`focus_menu_body`]; see [`super::render_frame`] and
/// [`HomeState::focus_action_overlay`]); the `session:` identity rides the modal's
/// title rather than a header line here. `avail_rows` caps the reserved block so a
/// short right pane never overruns (mirroring the menu's `avail_rows` window).
pub(super) fn focus_prompt_body(state: &HomeState, width: usize, avail_rows: usize) -> Vec<String> {
    let prompt = style("❯").danger().bold();
    // Split at the caret so ←/→/Home/End move a visible block caret through the prompt.
    let (before, after) = state.focus_prompt().split_at(state.focus_prompt_cursor());
    let value = widgets::block_caret(before, after, &Style::new().accent());
    let mut lines = vec![clip_to_width(&format!("{prompt} {value}"), width)];
    lines.push(String::new());
    // Reserve a fixed number of hint rows (padded with blanks), so the box keeps
    // one height whatever the hint is — commands, usage/examples, or none. Capped
    // to what the pane can hold so a short pane never pushes the box past it.
    let hint_rows = FOCUS_PROMPT_HINT_ROWS.min(avail_rows.saturating_sub(FOCUS_PROMPT_CHROME));
    let mut hints = focus_hint_lines(state.focus_prompt_hint(), width);
    hints.truncate(hint_rows);
    hints.resize(hint_rows, String::new());
    lines.extend(hints);
    lines
}

/// The label of 在席's trailing "+ new" tab — the action surface that launches a
/// pane. ASCII so the underline marker in [`tab_strip_parts`] (which measures
/// width in `chars`) lands exactly under it, as it does for the pane labels.
const FOCUS_NEW_TAB_LABEL: &str = "+ new";

/// A blank right pane of exactly `rows` rows — the pane behind a floating overlay
/// that owns the surface (the 在席 action modal), so the modal reads against an
/// empty pane rather than stale content showing through.
fn blank_pane(rows: usize) -> Vec<String> {
    vec![String::new(); rows]
}

/// Builds the 在席 (Focus) right pane. With no live panes it is a blank pane
/// behind the session's floating action surface — the Menu or the Prompt, both
/// composited as overlay modals by [`render_frame`] (see
/// [`HomeState::focus_action_overlay`]). With live panes it gains a **tab strip**:
/// one chip per live pane followed by a "+ new" chip while that launch surface is
/// selected, the session identity beside it (shared with 没入), and below it the
/// selected pane's live preview — on the "+ new" tab the pane stays blank because
/// the action surface again floats as an overlay. After a zoom-out that surface
/// floats over the selected pane tab instead: the preview drawn here keeps
/// showing behind it.
fn focus_pane(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    // No live panes: the action surface (Menu or Prompt) floats as an overlay
    // modal centred over the pane (composited by [`render_frame`] when
    // [`HomeState::focus_action_overlay`] holds), so the pane behind it stays
    // blank — neither surface renders inline.
    let Some(strip) = state.terminal_tabs().filter(|s| !s.labels.is_empty()) else {
        return blank_pane(rows);
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

    // On the "+ new" tab the launch surface (Menu or Prompt) floats as an overlay
    // modal (see [`HomeState::focus_action_overlay`]), so only the tab strip shows
    // behind it here. On a pane tab, preview the pane's live screen (the snapshot
    // taken before painting) so the selection shows what re-attaching reveals,
    // falling back to a label until the first snapshot is available.
    if !on_new {
        match state.terminal_view() {
            Some(view) => {
                let body = rows.saturating_sub(lines.len());
                lines.extend(terminal_pane(view, width, body));
            }
            None => {
                lines.push(style("● live terminal").success().to_string());
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
                let name = style(format!("{:<9}", h.name)).accent().to_string();
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
                style(usage).accent()
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
    let value = widgets::block_caret(before, after, &Style::new().accent());
    let mut lines = vec![style("+ new session").success().bold().to_string()];
    lines.extend(widgets::boxed("", inner, &[value]));
    // Keep the row count stable whether or not the name is currently invalid: an
    // error replaces the dim hint in place rather than adding a row.
    match create.error() {
        Some(err) => lines.push(style(clip_to_width(err, width)).danger().to_string()),
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
/// key hint pinned to the bottom row. At full width there is room to edit on the
/// row itself, so the session's own name line becomes the editable label in place
/// (see the `rename` branch of [`worktree_row`]) and this pane is not used.
fn switch_rename_pane(rename: &RenameInput, width: usize, rows: usize) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let value = widgets::block_caret(rename.value(), "", &Style::new().accent());
    let mut lines = vec![clip_to_width(
        &style(format!("rename {}", rename.target()))
            .accent()
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
    // `boxed`/`boxed_styled` clip each line (and the block-caret one, ANSI
    // included) to `inner`. While editing, paint the frame in the accent colour
    // and mark the title so the open editor is unmistakable — visually distinct
    // from the read-only note, which keeps the plain frame.
    match caret {
        Some(_) => {
            widgets::boxed_styled("note (編集中)", inner, &body, &Style::new().accent().bold())
        }
        None => widgets::boxed("note", inner, &body),
    }
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
/// **read-only** note shows while browsing in 切替 (see
/// [`HomeState::visible_switch_note`]). The box is a narrow top-right column (see
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

/// Witty English one-liners rested beneath the idle mascot in the 切替 preview
/// (and [`idle_quip`] picks one per session). They stand in for the 在席 menu's
/// choices — which selecting reveals only as a floating modal, never inline — so
/// the preview says "nothing is running here yet" without promising a surface that
/// never appears. Each nods to the usagi and to `Enter` as the way in.
const IDLE_QUIPS: [&str; 6] = [
    "Quiet as a sleeping bunny — press Enter to begin.",
    "This burrow is empty. Hop in with Enter.",
    "No tabs stirring yet. Enter starts one.",
    "A fresh warren awaits. Enter to dig in.",
    "Nothing running here. Enter wakes it up.",
    "Still and cozy — press Enter to get going.",
];

/// The [`IDLE_QUIPS`] line for the highlighted row, chosen by its name so the quip
/// stays stable per session (and differs between them). The root row keys off
/// [`ROOT_NAME`]; a detached session off [`DETACHED`].
fn idle_quip(state: &HomeState) -> &'static str {
    let name = match state.list().selected() {
        Some(w) => w.branch.as_deref().unwrap_or(DETACHED),
        None => ROOT_NAME,
    };
    let seed: usize = name.bytes().map(usize::from).sum();
    IDLE_QUIPS[seed % IDLE_QUIPS.len()]
}

/// The idle-session body: the mascot ([`widgets::rabbit_lines`]) centred in the
/// middle of a `width`×`rows` region with `quip` on a dim row below it. The block
/// (mascot + blank + caption) is centred both horizontally — each row already is —
/// and vertically, so the usagi sits in the centre of the pane whatever its
/// height. Always returns exactly `rows` rows so the pane stays full-height.
fn idle_rabbit_body(quip: &str, width: usize, rows: usize) -> Vec<String> {
    let mut block = widgets::rabbit_lines(width);
    block.push(String::new());
    block.push(widgets::dim_line(width, quip));
    let top = rows.saturating_sub(block.len()) / 2;
    let mut lines = vec![String::new(); top];
    lines.extend(block);
    lines.truncate(rows);
    lines.resize(rows, String::new());
    lines
}

/// The 切替 (Switch) right pane: a **preview of the screen that selecting the
/// session under the cursor will open**, so the choice is informed by what comes
/// next. A live session (an embedded shell / agent already running) previews the
/// live-terminal re-attach; an idle session with no live pane rests the mascot
/// with a light quip ([`idle_rabbit_body`]) on the menu UI, or previews its inline
/// prompt on the prompt UI. The header line carries the session's status and agent
/// state. The key hints live in the footer, so the preview uses the pane's full
/// height. The highlighted session's note is drawn over the top by [`note_overlay`]
/// (not inline), so it never pushes this preview around.
pub(super) fn switch_preview(state: &HomeState, width: usize, rows: usize) -> Vec<String> {
    if state.list().create_row_selected() {
        let mut lines = vec![
            style(clip_to_width("+ new session", width))
                .green()
                .bold()
                .to_string(),
            style(clip_to_width(
                "Type a name here, or press Enter, to create a session.",
                width,
            ))
            .dim()
            .to_string(),
            style(clip_to_width(
                "Esc cancels the input after it opens.",
                width,
            ))
            .dim()
            .to_string(),
        ];
        lines.truncate(rows);
        return lines;
    }

    let body_rows = rows;
    // The highlighted row's identity plus whether it has a live pane. The header
    // and the `live` flag drive both this preview and the tab hit-test
    // ([`switch_tab_at`]), so they are built once in [`switch_preview_header`]
    // and shared — the strip a click lands on is exactly the one drawn here.
    let (header, live) = switch_preview_header(state);

    // A live session's tabs share the header's row (the `←`/`→` — and click —
    // targets), so the identity and the tabs read together on one line; the
    // preview below mirrors the active pane.
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
                lines.push(style("● live terminal").success().to_string());
                lines.push(style("Enter で再アタッチ").dim().to_string());
            }
        }
    } else {
        // An idle session (no live pane, so no tab) has no screen to mirror.
        // Selecting opens 在席's action surface, which differs by Session Action
        // UI, so the preview matches each:
        match state.session_action_ui() {
            // The menu floats as an overlay *modal* over a blank pane — nothing is
            // drawn inline. Previewing its choices here would promise an inline
            // surface that never appears, so rest the mascot in the middle of the
            // pane with a light English quip instead: the preview reads as "nothing
            // is running yet — Enter to hop in" with no mismatch.
            SessionActionUi::Menu => {
                let body = body_rows.saturating_sub(lines.len());
                lines.extend(idle_rabbit_body(idle_quip(state), width, body));
            }
            // The prompt focuses *inline* (its own `session:` prompt), so previewing
            // that prompt here matches what selecting reveals.
            SessionActionUi::Prompt => {
                lines.push(String::new());
                let prompt = style("❯").danger().bold();
                let value = widgets::block_caret("", "", &Style::new().accent());
                lines.push(clip_to_width(&format!("{prompt} {value}"), width));
                lines.push(String::new());
                lines.push(style("Enter で開く").dim().to_string());
            }
        }
    }

    // Trim the body to its budget and pad up so the pane is always full-height.
    lines.truncate(body_rows);
    lines.resize(body_rows, String::new());
    lines
}

/// Header text for 切替's right-pane preview, plus whether the highlighted row is
/// live. `switch_preview` and `switch_tab_at` share this so tab chips are measured
/// from the same header text the renderer puts beside them.
fn switch_preview_header(state: &HomeState) -> (String, bool) {
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
    let agent = AgentState::from_flags(live, running, waiting, done).detail(HEADER_AGENT_COL);
    (preview_header(&name, status, agent), live)
}

/// The right pane's contents, by mode. A preview of the would-be session screen
/// in 切替 (the default); the session's action surface — a menu or a prompt, per
/// [`SessionActionUi`] — in 在席; and the live embedded terminal in 没入 (a
/// starting hint until the first snapshot arrives).
pub(super) fn right_pane_contents(state: &HomeState, right_w: usize, rows: usize) -> Vec<String> {
    // A momentary launch (terminal / agent spawn) blanks the right pane so the
    // centred loading rabbit (composited over the frame in [`super::frame`])
    // reads as a dedicated loading screen. Without this the 在席 action menu
    // (agent / terminal / … の選択肢) stays painted behind the rabbit, so the
    // choices would show through while the spawn blocks. Return an empty pane of
    // the right height and let the overlay own the whole surface.
    if state.loading().is_some() {
        return vec![String::new(); rows];
    }
    // The Markdown preview, when open, takes over the right pane regardless of
    // mode (it is opened from the `:` palette and captures the keyboard while
    // shown).
    if let Some(preview) = state.preview() {
        return preview_pane(preview, right_w, rows);
    }
    // The local-LLM chat, when open, likewise takes over the right pane
    // regardless of mode (opened from 在席's `chat`, it captures the keyboard
    // while shown). The sidebar to its left keeps rendering as usual.
    if let Some(chat) = state.chat() {
        return crate::presentation::tui::chat::ui::pane(chat, right_w, rows);
    }
    // The diff view likewise takes over the right pane (opened from the `:`
    // palette, capturing the keyboard while shown).
    if let Some(diff) = state.diff_view() {
        return diff_pane(diff, right_w, rows);
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

// Background tints for the diff view (256-colour cube), chosen to read against a
// dark terminal like GitHub's dark diff: a subtle base tint for each add/del
// line and a brighter one for the word-level changed ranges within it.
const DIFF_ADD_BG: u8 = 22; // dark green
const DIFF_ADD_EMPH_BG: u8 = 28; // brighter green
const DIFF_DEL_BG: u8 = 52; // dark red
const DIFF_DEL_EMPH_BG: u8 = 88; // brighter red
const DIFF_NUM_FG: u8 = 244; // dim grey line numbers
const DIFF_HUNK_FG: u8 = 37; // teal hunk headers

/// Render the right-pane diff view: a one-row header (title + scroll position)
/// over a window of diff rows, laid out unified or side-by-side. The window is
/// clamped so the last row stays in view (matching the event loop's scroll
/// clamp), and every row is exactly `width` columns so the layout never shifts.
pub(super) fn diff_pane(view: &DiffView, width: usize, rows: usize) -> Vec<String> {
    let body_h = rows.saturating_sub(1);
    // An empty patch (a session that changed nothing) shows a friendly line
    // instead of a blank pane.
    if view.doc.is_empty() {
        let mut lines = Vec::with_capacity(rows);
        lines.push(diff_header(view, 0, 0, 0, width));
        if rows > 1 {
            lines.push(
                style(clip_to_width(
                    "No changes against the base branch 🐰",
                    width,
                ))
                .to_string(),
            );
            lines.resize(rows, String::new());
        }
        lines.truncate(rows);
        return lines;
    }

    let num_w = num_width(&view.doc);
    // The renderable rows depend on the layout: split folds paired add/del lines
    // into one visual row.
    let split = view.split.then(|| split_rows(&view.doc));
    let total = split.as_ref().map_or(view.doc.rows.len(), Vec::len);
    let max_start = total.saturating_sub(body_h);
    let start = view.scroll.min(max_start);
    let end = (start + body_h).min(total);

    let mut lines = Vec::with_capacity(rows);
    lines.push(diff_header(view, start, end, total, width));
    if rows <= 1 {
        lines.truncate(rows);
        return lines;
    }
    for i in 0..body_h {
        let row = match &split {
            Some(split) => split
                .get(start + i)
                .map(|sr| diff_split_row(&view.doc, *sr, num_w, width)),
            None => view
                .doc
                .rows
                .get(start + i)
                .map(|r| diff_unified_row(r, num_w, width)),
        };
        lines.push(row.unwrap_or_default());
    }
    lines
}

/// The diff view's one-row header: an icon, the branch → base title, the layout
/// name, and a `start-end/total` position once it scrolls.
fn diff_header(view: &DiffView, start: usize, end: usize, total: usize, width: usize) -> String {
    let layout = if view.split { "split" } else { "unified" };
    let header = if total > end.saturating_sub(start) && total > 0 {
        format!(
            " {}  [{}]  ({}-{}/{})",
            view.title,
            layout,
            start + 1,
            end,
            total
        )
    } else {
        format!(" {}  [{}]", view.title, layout)
    };
    style(clip_to_width(&header, width)).bold().to_string()
}

/// The line-number gutter width: the digit count of the largest line number in
/// the diff (at least 2), so the gutter is as narrow as the content allows.
fn num_width(doc: &crate::presentation::tui::diff::DiffDoc) -> usize {
    let max = doc
        .rows
        .iter()
        .filter_map(|r| r.old_no.max(r.new_no))
        .max()
        .unwrap_or(0);
    (max.to_string().len()).max(2)
}

/// Render one diff row in the unified layout: a `old new` line-number gutter, a
/// `+`/`-`/space marker, then the syntax-highlighted content on its add/del/
/// context background. Header / hunk / meta rows span the full width instead.
fn diff_unified_row(row: &DiffRow, num_w: usize, width: usize) -> String {
    match row.kind {
        RowKind::FileHeader => style(clip_to_width(&row.text(), width)).bold().to_string(),
        RowKind::Hunk => style(clip_to_width(&row.text(), width))
            .color256(DIFF_HUNK_FG)
            .to_string(),
        RowKind::Meta => style(clip_to_width(&row.text(), width)).dim().to_string(),
        RowKind::Context | RowKind::Add | RowKind::Del => {
            let gutter = diff_gutter(row.old_no, row.new_no, num_w);
            let gutter_w = num_w * 2 + 2;
            let marker = match row.kind {
                RowKind::Add => '+',
                RowKind::Del => '-',
                _ => ' ',
            };
            let (base_bg, emph_bg) = diff_backgrounds(row.kind);
            // Budget: the pane width less the gutter and the one-column marker.
            let budget = width.saturating_sub(gutter_w + 1);
            let marker_styled = match base_bg {
                Some(bg) => style(marker.to_string()).on_color256(bg).to_string(),
                None => marker.to_string(),
            };
            let content = diff_content(&row.spans, &row.changed, base_bg, emph_bg, budget);
            format!("{gutter}{marker_styled}{content}")
        }
    }
}

/// Render one side-by-side row: a full-width header, or old (left) and new
/// (right) columns separated by a dim bar, each a fixed `col_w` wide so the two
/// halves always line up.
fn diff_split_row(
    doc: &crate::presentation::tui::diff::DiffDoc,
    row: SplitRow,
    num_w: usize,
    width: usize,
) -> String {
    match row {
        SplitRow::Full(i) => diff_unified_row(&doc.rows[i], num_w, width),
        SplitRow::Pair { left, right } => {
            let col_w = width.saturating_sub(1) / 2;
            let left = diff_half(left.map(|i| &doc.rows[i]), true, num_w, col_w);
            let right = diff_half(right.map(|i| &doc.rows[i]), false, num_w, col_w);
            let sep = style("│").dim().to_string();
            format!("{left}{sep}{right}")
        }
    }
}

/// One column of the split layout: a single line number + the content on its
/// tint, padded to exactly `col_w`. An absent row (the short side of a replaced
/// block) renders as blank padding.
fn diff_half(row: Option<&DiffRow>, is_left: bool, num_w: usize, col_w: usize) -> String {
    let Some(row) = row else {
        return " ".repeat(col_w);
    };
    let no = if is_left { row.old_no } else { row.new_no };
    let num = no.map(|n| n.to_string()).unwrap_or_default();
    let gutter = format!("{num:>num_w$} ");
    let gutter_styled = style(&gutter).color256(DIFF_NUM_FG).to_string();
    let (base_bg, emph_bg) = diff_backgrounds(row.kind);
    let budget = col_w.saturating_sub(num_w + 1);
    let content = diff_content(&row.spans, &row.changed, base_bg, emph_bg, budget);
    format!("{gutter_styled}{content}")
}

/// The `old new ` line-number gutter for the unified layout (each number
/// right-aligned in `num_w` columns), dimmed and blank where a number is absent.
fn diff_gutter(old: Option<usize>, new: Option<usize>, num_w: usize) -> String {
    let old = old.map(|n| n.to_string()).unwrap_or_default();
    let new = new.map(|n| n.to_string()).unwrap_or_default();
    style(format!("{old:>num_w$} {new:>num_w$} "))
        .color256(DIFF_NUM_FG)
        .to_string()
}

/// The base and word-emphasis background tints for a row kind (`None` for context
/// and headers, which take no background).
fn diff_backgrounds(kind: RowKind) -> (Option<u8>, Option<u8>) {
    match kind {
        RowKind::Add => (Some(DIFF_ADD_BG), Some(DIFF_ADD_EMPH_BG)),
        RowKind::Del => (Some(DIFF_DEL_BG), Some(DIFF_DEL_EMPH_BG)),
        _ => (None, None),
    }
}

/// Render a content line's spans into `budget` display columns: each character
/// keeps its syntax-highlight foreground, sits on the base tint, and switches to
/// the brighter emphasis tint inside a `changed` word range. Runs of like-styled
/// characters are coalesced, the line is clipped to the budget, and — when a
/// background is set — the remaining columns are padded so the tint fills the row.
fn diff_content(
    spans: &[DiffSpan],
    changed: &[(usize, usize)],
    base_bg: Option<u8>,
    emph_bg: Option<u8>,
    budget: usize,
) -> String {
    let mut out = String::new();
    let mut col = 0usize; // display columns emitted
    let mut idx = 0usize; // char index into the content (for `changed`)
    let mut run = String::new();
    let mut run_fg: Option<Rgb> = None;
    let mut run_emph = false;

    for span in spans {
        for ch in span.text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > budget {
                push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
                run.clear();
                return pad_diff(out, col, budget, base_bg);
            }
            let emph = in_changed(idx, changed);
            if !run.is_empty() && (span.color != run_fg || emph != run_emph) {
                push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
                run.clear();
            }
            if run.is_empty() {
                run_fg = span.color;
                run_emph = emph;
            }
            run.push(ch);
            col += w;
            idx += 1;
        }
    }
    push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
    pad_diff(out, col, budget, base_bg)
}

/// Append a coalesced run of like-styled characters to `out`, applying its
/// foreground colour and its background (the emphasis tint when `emph`, else the
/// base tint). A run with no background and no colour is emitted verbatim.
fn push_diff_run(
    out: &mut String,
    run: &str,
    fg: Option<Rgb>,
    emph: bool,
    base_bg: Option<u8>,
    emph_bg: Option<u8>,
) {
    if run.is_empty() {
        return;
    }
    let bg = if emph { emph_bg } else { base_bg };
    let mut styled = style(run.to_string());
    if let Some(fg) = fg {
        styled = styled.color256(rgb_to_ansi256(fg));
    }
    if let Some(bg) = bg {
        styled = styled.on_color256(bg);
    }
    out.push_str(&styled.to_string());
}

/// Pad the emitted content out to `budget` columns so a set background tint fills
/// the whole row (GitHub-style); with no background the row is left as-is.
fn pad_diff(mut out: String, col: usize, budget: usize, base_bg: Option<u8>) -> String {
    if col < budget {
        let pad = " ".repeat(budget - col);
        match base_bg {
            Some(bg) => out.push_str(&style(pad).on_color256(bg).to_string()),
            None => out.push_str(&pad),
        }
    }
    out
}

/// Whether char index `idx` falls inside any half-open `changed` range.
fn in_changed(idx: usize, changed: &[(usize, usize)]) -> bool {
    changed.iter().any(|&(s, e)| idx >= s && idx < e)
}

/// Render one [`MarkdownLine`] to a styled, width-clipped row: its prefix marker
/// coloured by block kind, then its inline spans styled by emphasis.
fn markdown_row(line: &MarkdownLine, width: usize) -> String {
    let mut out = String::new();
    if !line.prefix.is_empty() {
        let prefix = match line.style {
            LineStyle::Bullet | LineStyle::Number => style(&line.prefix).accent().to_string(),
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
        // span (unknown highlight) falls back to the palette's success colour.
        LineStyle::Code => match span.color {
            Some(rgb) => style(text).color256(rgb_to_ansi256(rgb)).to_string(),
            None => style(text).success().to_string(),
        },
        LineStyle::Quote => style(text).dim().italic().to_string(),
        _ => match span.style {
            SpanStyle::Plain => text.to_string(),
            SpanStyle::Strong => style(text).bold().to_string(),
            SpanStyle::Emphasis => style(text).italic().to_string(),
            SpanStyle::Code => style(text).success().to_string(),
            SpanStyle::Link => style(text).info().underlined().to_string(),
        },
    }
}

/// The bold, level-coloured styling of a heading's text: magenta (h1), cyan (h2),
/// yellow (h3), and plain bold for deeper levels.
fn heading_style(text: &str, level: u8) -> String {
    let base = style(text).bold();
    match level {
        1 => base.feature(),
        2 => base.accent(),
        3 => base.warning(),
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
    fn selected_row_span_falls_back_when_there_are_no_rows_to_walk() {
        // A degenerate list with no groups has nothing to walk, so the span falls
        // back to a single-line block pinned at the top.
        let list = WorktreeList::from_groups(Vec::new());
        assert_eq!(selected_row_span(&list, true), (0, 1));
    }

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

    fn label_def(id: &str, name: &str, color: LabelColor, icon: Option<&str>) -> SessionLabelDef {
        SessionLabelDef {
            id: id.to_string(),
            name: name.to_string(),
            color,
            icon: icon.map(str::to_string),
        }
    }

    fn wt(branch: &str, path: &str) -> WorktreeState {
        WorktreeState {
            branch: Some(branch.to_string()),
            path: PathBuf::from(path),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            diff: None,
            ahead_behind: None,
            pr: Vec::new(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn label_style_maps_every_colour_to_its_palette_role() {
        // Each colour renders through the semantic palette; cover all arms and pin
        // the mapping to the concrete ANSI colour it should resolve to.
        for (color, expected) in [
            (LabelColor::Gray, Style::new().dim()),
            (LabelColor::Red, Style::new().danger()),
            (LabelColor::Green, Style::new().success()),
            (LabelColor::Yellow, Style::new().warning()),
            (LabelColor::Blue, Style::new().info()),
            (LabelColor::Magenta, Style::new().feature()),
            (LabelColor::Cyan, Style::new().accent()),
        ] {
            assert_eq!(
                label_style(color)
                    .force_styling(true)
                    .apply_to("x")
                    .to_string(),
                expected.force_styling(true).apply_to("x").to_string()
            );
        }
    }

    #[test]
    fn label_cell_renders_the_glyph_and_name_pads_blank_and_drops_at_zero() {
        let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
        // A set label shows its glyph and name, and the cell is exactly `col` wide.
        let cell = label_cell(Some(&def), 10);
        let plain = console::strip_ansi_codes(&cell).into_owned();
        assert!(plain.contains("◇ Review"), "{plain:?} shows the label");
        assert_eq!(console::measure_text_width(&cell), 10);
        // An unset row holds the same width in blanks.
        let blank = label_cell(None, 10);
        assert_eq!(console::measure_text_width(&blank), 10);
        assert!(console::strip_ansi_codes(&blank).trim().is_empty());
        // A zero-width column (no visible label anywhere) draws nothing.
        assert_eq!(label_cell(Some(&def), 0), "");
        assert_eq!(label_cell(None, 0), "");
    }

    #[test]
    fn label_cell_clips_a_long_name_to_the_column() {
        let def = label_def("x", "A very long status name", LabelColor::Gray, None);
        let cell = label_cell(Some(&def), 8);
        // The cell fills its column by plain display width, ellipsis included.
        assert_eq!(console::measure_text_width(&cell), 8);
    }

    #[test]
    fn label_col_width_sizes_to_the_widest_master_label_not_the_visible_one() {
        // Two short labels: the column is sized to the wider of the two in the
        // master, regardless of which one a session actually shows.
        let master = SessionLabelMaster {
            labels: vec![
                label_def("todo", "Todo", LabelColor::Gray, Some("○")),
                label_def("blocked", "Blocked", LabelColor::Gray, Some("✕")),
            ],
        };
        // No labels assigned → the column is dropped (0), leaving the sidebar as it
        // was before the feature.
        let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
        assert_eq!(label_col_width(&list, &master), 0);
        // The narrow `todo` is shown, but the column reserves the master's widest
        // (`✕ Blocked`) + a separating space, so cycling to `blocked` will not
        // resize the column and shift the row.
        let widest = "✕ Blocked".chars().count() + 1;
        list.set_label_ids(vec![Some("todo".to_string())]);
        assert_eq!(label_col_width(&list, &master), widest);
        // Cycling to the wider label keeps the same column width — no shift.
        list.set_label_ids(vec![Some("blocked".to_string())]);
        assert_eq!(label_col_width(&list, &master), widest);
    }

    #[test]
    fn label_col_width_caps_a_long_master_label() {
        // A master label longer than the cap clamps to LABEL_COL_MAX (+1 separator).
        let master = SessionLabelMaster {
            labels: vec![label_def(
                "long",
                "Averylonglabelname",
                LabelColor::Gray,
                Some("◇"),
            )],
        };
        let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
        list.set_label_ids(vec![Some("long".to_string())]);
        assert_eq!(label_col_width(&list, &master), LABEL_COL_MAX + 1);
    }

    #[test]
    fn rail_label_glyph_shows_the_coloured_glyph_or_nothing() {
        let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
        let glyph = rail_label_glyph(Some(&def)).unwrap();
        assert!(console::strip_ansi_codes(&glyph).contains('◇'));
        assert_eq!(rail_label_glyph(None), None);
    }

    #[test]
    fn worktree_row_draws_the_manual_status_label_on_line_one() {
        let def = label_def("review", "Review", LabelColor::Magenta, Some("◇"));
        let (line1, _) = worktree_row(
            &wt("main", "/r/main"),
            "",
            Some(&def),
            "◇ Review".chars().count() + 1,
            10,
            20,
            DetailCols::default(),
            false,
            Utc::now(),
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            None,
        );
        assert!(console::strip_ansi_codes(&line1).contains("◇ Review"));
    }

    #[test]
    fn root_row_reserves_the_label_column_without_drawing_a_label() {
        // The root carries no label, but with a label column active its blank cell
        // keeps the right-edge note field aligned with the sessions below.
        let (with_col, _) = root_row(10, 8, 20, false, false, false, false);
        let (without_col, _) = root_row(10, 0, 20, false, false, false, false);
        assert_eq!(
            console::measure_text_width(&with_col),
            console::measure_text_width(&without_col) + 8
        );
    }

    #[test]
    fn left_pane_full_and_rail_draw_a_sessions_manual_label() {
        let master = SessionLabelMaster {
            labels: vec![label_def(
                "review",
                "Review",
                LabelColor::Magenta,
                Some("◇"),
            )],
        };
        let mut list = WorktreeList::new("ws", vec![wt("main", "/r/main")]);
        list.set_label_ids(vec![Some("review".to_string())]);
        let empty = HashSet::new();
        let res = HashMap::new();
        let join = |lines: Vec<String>| {
            lines
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        // The full sidebar spells out the label (glyph + name) beside the session.
        let full = join(left_pane(
            &list,
            &empty,
            &empty,
            &empty,
            &empty,
            &res,
            &master,
            60,
            40,
            false,
            Sidebar::Full,
            Utc::now(),
            None,
        ));
        assert!(full.contains("◇ Review"), "{full:?}");
        // The collapsed rail shows just the coloured glyph.
        let rail = join(left_pane(
            &list,
            &empty,
            &empty,
            &empty,
            &empty,
            &res,
            &master,
            RAIL_WIDTH,
            40,
            false,
            Sidebar::Rail,
            Utc::now(),
            None,
        ));
        assert!(rail.contains('◇'), "{rail:?}");
    }

    #[test]
    fn name_cell_pads_by_display_width_not_char_count() {
        // The cell pads by *display* columns, not char count: `あ機能` is 3 chars
        // but 6 display columns, so padding to a width-8 cell adds 2 columns (not 5
        // chars) and the cell measures exactly 8 — SGR escapes have zero display
        // width. The old `format!("{:<8}")` padded by chars and overran to 11.
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
    fn name_cell_reserves_its_width_for_ambiguous_characters() {
        // A name carrying East Asian *Ambiguous* characters (`→ ① ※`) still fills
        // exactly its cell so the following fixed fields do not shift. usagi's
        // terminals paint these one column wide, which is what
        // [`console::measure_text_width`] counts, so sizing and measuring by it
        // keeps the cell and its neighbours aligned.
        for name in ["feat→x", "review①", "対応※", "→→→→"] {
            assert_eq!(
                console::measure_text_width(&name_cell(name, 10, false)),
                10,
                "cell for {name:?} should reserve exactly its width"
            );
        }
        // Two names of equal *rendered* width — one all-ASCII, one carrying an
        // ambiguous glyph — produce the same cell width, so the fields that butt
        // against the cell land in the same place for both.
        assert_eq!(
            console::measure_text_width(&name_cell("feat→", 10, false)),
            console::measure_text_width(&name_cell("featx", 10, false)),
        );
    }

    #[test]
    fn uncoloured_code_span_falls_back_to_success() {
        // A code-block span with no highlight colour uses the palette's success
        // colour, matching the styling of inline code.
        let span = Span {
            text: "x".to_string(),
            style: SpanStyle::Code,
            color: None,
        };
        assert_eq!(
            styled_span(&span, LineStyle::Code),
            style("x").success().to_string()
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
        // Measured as painted (the `↑` / `↓` arrows are one column wide), the
        // blanked-out side holds exactly the drawn side's width.
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
        // A drawn column but no measurement for this row → blanks holding the
        // 1-wide arrow slot plus its one digit.
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
        assert_eq!(full.commits_width(), 6); // (1+2) + gap + (1+1), arrows one wide
        assert_eq!(full.badge_width(), 8); // 3 + 2 + 3
        assert_eq!(full.cluster_width(), 8 + 1 + 6 + 1 + 8 + 1 + 4); // four fields, three gaps

        // Only an ahead side, no diff, no time: one field, no gaps, no `↓` columns.
        let ahead_only = DetailCols {
            ahead: 2,
            ..DetailCols::default()
        };
        assert_eq!(ahead_only.commits_width(), 3); // 1-wide arrow + 2 digits
        assert_eq!(ahead_only.badge_width(), 0);
        assert_eq!(ahead_only.cluster_width(), 3);

        // Only a behind side (covers the `up == 0` half of the commit gap).
        let behind_only = DetailCols {
            behind: 2,
            ..DetailCols::default()
        };
        assert_eq!(behind_only.commits_width(), 3); // 1-wide arrow + 2 digits

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
            console::measure_text_width(&relative_time(now, at(now, 12))) // "12m ago"
        );
    }

    #[test]
    fn detail_cols_reserves_the_pr_slot_even_with_no_pr() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // No visible session carries a PR, yet the column holds its reserved width
        // so a session gaining or losing its last PR never shifts the diff beside it.
        let none = detail_cols(
            &[(
                at(now, 3),
                Some(DiffStat {
                    added: 1,
                    removed: 2,
                }),
                None,
                0,
            )],
            now,
            9,
            60,
        );
        assert_eq!(none.pr, PR_RESERVE_WIDTH);
        // A single-PR badge is exactly the reserve width, so appearing shifts nothing.
        let one = detail_cols(&[(at(now, 3), None, None, pr_width(&[pr(7)]))], now, 9, 60);
        assert_eq!(one.pr, PR_RESERVE_WIDTH);
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
        let mid = detail_cols(&data, now, 9, 30);
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
        // Icon only: the AI glyph + phase icon, no spelled-out word.
        assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
        assert!(!plain.contains("running"));
        assert!(plain.ends_with("+124 -18"));

        // With no agent the cluster still rides the right edge.
        let line = detail_content(AgentState::Absent, std::slice::from_ref(&badge), 24);
        assert_eq!(console::measure_text_width(&line), 24);
        assert_eq!(console::strip_ansi_codes(&line).trim_start(), "+124 -18");
    }

    #[test]
    fn detail_content_falls_back_to_the_agent_or_clips_a_cramped_cluster() {
        // No cells → just the agent icons (blank when absent, no spelled-out word).
        assert_eq!(detail_content(AgentState::Absent, &[], 20), "");
        let running = detail_content(AgentState::Running, &[], 20);
        let running = console::strip_ansi_codes(&running);
        assert!(running.contains('▶'));
        assert!(!running.contains("running"));
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
        let time = rpad(&style("3m ago").dim().to_string(), 6);
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
        assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
        assert!(plain.contains("3m ago ↑2 ↓1 +1 -2"));
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
        assert_eq!(ago(180), "3m ago"); // minutes
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
        assert!(plain.starts_with(&format!("{AGENT_ICON} ▶")));
        assert!(plain.contains(format!("+1 -2 {PR_ICON} 2").as_str()));
        assert!(plain.ends_with(format!("{PR_ICON} 2").as_str()));
    }
}
