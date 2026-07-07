//! Rendering and hit-test layout for the home screen sidebar.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::super::state::{worktree_name, PendingSession, WorktreeList, ROOT_NAME};
use super::{
    clip_to_width, pad_to_width, ACTIVE_COL, DETACHED, DIRTY_ICON, LOCAL_ICON, NAME_PREFIX,
    NEW_ICON, NOTE_ICON, PUSHED_ICON, RAIL_WIDTH, ROOT_DETAIL, SYNCED_ICON,
};
use crate::domain::resource::{Load, ResourceUsage};
use crate::domain::settings::{LabelColor, SessionLabelDef, SessionLabelMaster, Sidebar};
use crate::domain::workspace_state::{
    AheadBehind, BranchStatus, DiffStat, PrLink, SessionOrigin, WorktreeState,
};
use crate::presentation::theme::Palette;
use crate::presentation::tui::widgets;
use chrono::{DateTime, Duration, Utc};
use console::{style, Style};

/// The Nerd Font git glyph for a branch lifecycle status.
pub(super) fn status_icon(status: BranchStatus) -> char {
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
pub(super) fn status_style(status: BranchStatus) -> Style {
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
pub(super) const LABEL_COL_MAX: usize = 12;

/// The [`Style`] a manual-status [`LabelColor`] paints in, resolved through the
/// semantic [`Palette`] so the label column follows a theme retune like every
/// other coloured element (`Gray` reads as a dim, unobtrusive tag).
pub(super) fn label_style(color: LabelColor) -> Style {
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
pub(super) fn label_cell(label: Option<&SessionLabelDef>, col: usize) -> String {
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
pub(super) fn label_col_width(list: &WorktreeList, master: &SessionLabelMaster) -> usize {
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
pub(super) fn rail_label_glyph(label: Option<&SessionLabelDef>) -> Option<String> {
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
pub(super) const AGENT_ICON: char = '\u{f17b}'; // nf-fa-android — an AI agent (robot) drives this session

#[derive(Clone, Copy)]
pub(super) enum AgentState {
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
    pub(super) fn from_flags(live: bool, running: bool, waiting: bool, done: bool) -> Self {
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
    pub(super) fn detail(self, width: usize) -> Option<String> {
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
pub(super) const SELECTED_SESSION_GLYPH: char = '\u{f0907}';

/// The far-left gutter cell used by root/action rows. In 選択 (Overview) the
/// keyboard is on the list, so the selected non-session row shows a red `>`
/// cursor. The **active** session — the one subsequent commands operate on — is
/// marked by a green `▎` accent bar that runs down its row. Outside Overview there
/// is no cursor, so the gutter only ever carries the active bar; when the cursor
/// and the active row coincide in Overview, the cursor takes the column.
pub(super) fn gutter_cell(selected: bool, active: bool, in_overview: bool) -> String {
    if in_overview && selected {
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
/// red while the cursor is in 選択, then green after the session is selected. The
/// root and the "+ new session" action keep the compact `>` cursor in 選択
/// because they are not sessions and do not have a three-row body.
pub(super) fn session_gutter_cell(
    selected: bool,
    active: bool,
    in_overview: bool,
    row: usize,
) -> String {
    if selected {
        let mark = if row == 0 {
            SELECTED_SESSION_GLYPH.to_string()
        } else {
            "▎".to_string()
        };
        let style = if in_overview {
            Style::new().danger().bold()
        } else {
            Style::new().success().bold()
        };
        style.apply_to(mark).to_string()
    } else {
        gutter_cell(false, active, in_overview)
    }
}

/// Fixed display columns reserved for a session-origin marker in front of the
/// sidebar name. Human / MCP rows draw one Font Awesome 4 glyph plus a trailing
/// space; legacy unknown rows keep the same blank field so the name column does
/// not shift between rows.
pub(super) const ORIGIN_COL: usize = 2;

/// The human-origin glyph (`nf-fa-user`) kept in the Font Awesome 4 range, where
/// older / partial Nerd Fonts are more likely to render it than FA5 glyphs.
pub(super) const HUMAN_ORIGIN_ICON: char = '\u{f007}';

/// The agent/MCP-origin glyph (`nf-fa-cogs`) kept in the Font Awesome 4 range so
/// it does not fall back to `?` on older / partial Nerd Fonts.
pub(super) const MCP_ORIGIN_ICON: char = '\u{f085}';

/// The fixed-width, dim origin field that prefixes a session's name. It is
/// intentionally separate from [`AgentState`]'s android/detail icon: this marker
/// says who *created* the session, not whether an agent is currently running in
/// it.
pub(super) fn origin_cell(origin: SessionOrigin) -> String {
    match origin {
        SessionOrigin::Unknown => " ".repeat(ORIGIN_COL),
        SessionOrigin::Human => style(format!("{HUMAN_ORIGIN_ICON} ")).dim().to_string(),
        SessionOrigin::Mcp => style(format!("{MCP_ORIGIN_ICON} ")).dim().to_string(),
    }
}

/// The branch / root name cell: clipped and padded to `width`, cyan, and bold
/// when the row is active or under the cursor.
pub(super) fn name_cell(text: &str, width: usize, emphasised: bool) -> String {
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

/// Columns a single lineage level occupies in the sidebar. Two columns (`↳ `)
/// are enough to make a child session visibly belong under its parent without
/// crowding the already-dense session row.
pub(super) const LINEAGE_INDENT_WIDTH: usize = 2;

/// Display columns consumed by `depth` levels of session lineage.
pub(super) fn lineage_width(depth: usize) -> usize {
    depth.saturating_mul(LINEAGE_INDENT_WIDTH)
}

/// Display columns before the session's actual name inside a session identity
/// cell: the optional lineage marker plus the fixed origin field. Detail and
/// resource rows use the same width so their text starts under the display name,
/// not under the origin glyph.
pub(super) fn session_name_prefix_width(depth: usize) -> usize {
    lineage_width(depth).saturating_add(ORIGIN_COL)
}

/// The dim tree prefix drawn in front of a child session's name. A depth-1 child
/// reads `↳ child`; deeper descendants add two spaces per level (`  ↳ grandchild`).
pub(super) fn lineage_prefix(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{}↳ ", " ".repeat(lineage_width(depth - 1)))
    }
}

/// A branch / root name cell with an optional lineage prefix reserved inside the
/// same `width` the plain [`name_cell`] uses. The prefix is dim and the session
/// name keeps the normal accent styling, so child rows read as subordinate while
/// preserving the existing active/selected emphasis on the name itself.
#[cfg(test)]
pub(super) fn nested_name_cell(
    text: &str,
    width: usize,
    emphasised: bool,
    nesting_depth: usize,
) -> String {
    let prefix = lineage_prefix(nesting_depth);
    let prefix_width = console::measure_text_width(&prefix);
    if prefix_width == 0 {
        return name_cell(text, width, emphasised);
    }
    if prefix_width > width {
        return style(clip_to_width(&prefix, width)).dim().to_string();
    }
    format!(
        "{}{}",
        style(prefix).dim(),
        name_cell(text, width - prefix_width, emphasised)
    )
}

fn origin_cell_for_width(origin: SessionOrigin, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => match origin {
            SessionOrigin::Unknown => " ".to_string(),
            SessionOrigin::Human => style(HUMAN_ORIGIN_ICON).dim().to_string(),
            SessionOrigin::Mcp => style(MCP_ORIGIN_ICON).dim().to_string(),
        },
        _ => origin_cell(origin),
    }
}

/// A session name cell with both the optional lineage prefix and the fixed
/// creation-origin field reserved before the display name. The lineage marker is
/// drawn first (`↳  <origin> name`) so children still read as children, while the
/// origin field remains immediately adjacent to the name it describes.
pub(super) fn nested_session_name_cell(
    text: &str,
    width: usize,
    emphasised: bool,
    nesting_depth: usize,
    origin: SessionOrigin,
) -> String {
    let prefix = lineage_prefix(nesting_depth);
    let prefix_width = console::measure_text_width(&prefix);
    if prefix_width > width {
        return style(clip_to_width(&prefix, width)).dim().to_string();
    }
    let name_area = width.saturating_sub(prefix_width);
    if name_area <= ORIGIN_COL {
        return format!(
            "{}{}",
            style(prefix).dim(),
            origin_cell_for_width(origin, name_area)
        );
    }
    format!(
        "{}{}{}",
        style(prefix).dim(),
        origin_cell(origin),
        name_cell(text, name_area - ORIGIN_COL, emphasised)
    )
}

/// Prefix an already-styled inline field with the same dim lineage marker used
/// by [`nested_name_cell`]. Unlike [`nested_name_cell`], the caller owns the body
/// styling (rename fields carry a block caret; removal skeletons shimmer), so we
/// only clip the body to the remaining width after the prefix.
pub(super) fn nested_inline_field(content: &str, width: usize, nesting_depth: usize) -> String {
    let prefix = lineage_prefix(nesting_depth);
    let prefix_width = console::measure_text_width(&prefix);
    if prefix_width == 0 {
        return clip_to_width(content, width);
    }
    if prefix_width >= width {
        return style(clip_to_width(&prefix, width)).dim().to_string();
    }
    format!(
        "{}{}",
        style(prefix).dim(),
        clip_to_width(content, width - prefix_width)
    )
}

/// Builds a row's second (detail) line: the row's `gutter` cell at the far-left
/// column (so the active accent bar runs down both lines), padded to sit under
/// the branch name, then the already-styled, already-clipped `detail`.
pub(super) fn detail_line(gutter: &str, detail: String) -> String {
    nested_detail_line(gutter, 0, detail)
}

/// A detail/resource line shifted by `lineage_width` columns so the second and
/// third line of a child session sit under its indented name.
pub(super) fn nested_detail_line(gutter: &str, lineage_width: usize, detail: String) -> String {
    let indent = " ".repeat(NAME_PREFIX - 1);
    let lineage = " ".repeat(lineage_width);
    format!("{gutter}{indent}{lineage}{detail}")
}

/// Display width the CPU figure is left-padded to inside the `<cpu icon> … <mem
/// icon> …` label, so the memory column lands in the same place whether CPU reads
/// `0%` or `100%` — the CPU digit count never shifts MEM. Holds up to `100%`; a
/// rarer larger reading just nudges MEM right for that one line.
pub(super) const CPU_LABEL_WIDTH: usize = 4;

/// Nerd Font glyphs labelling the CPU and memory figures on the resource line,
/// in place of spelling out `CPU` / `MEM` — the same icon-led style the fixed
/// header/status labels use. They need a patched [Nerd Font](https://www.nerdfonts.com/)
/// to render; without one the terminal shows a fallback box, but the number
/// beside each glyph still carries the meaning.
pub(super) const CPU_ICON: char = '\u{f2db}'; // nf-fa-microchip — processor use
                                              // nf-fa-server — resident memory. Kept in the Font Awesome 4 range (like the git
                                              // status icons) so it renders on older/partial Nerd Fonts; the FA5 nf-fa-memory
                                              // (U+F538) is missing from those and shows a `?` fallback.
pub(super) const MEM_ICON: char = '\u{f233}';

/// Rows every list entry (the root and each session) spans, fixed so the list
/// never reflows as a session goes live or idle: an identity line, a detail
/// line, and the CPU / memory line. Shared by the full sidebar, the collapsed
/// rail, and the click hit-tests so the renderer and the hit-tests never
/// disagree on where a session's rows are.
pub(super) const SESSION_ROWS: usize = 3;

/// Blank rows inserted between workspace groups in 統合(unite) mode. The gap is
/// pure decoration: it does not advance the flat selectable-row index and click
/// hit-tests skip over it.
pub(super) const UNITE_WORKSPACE_GAP_ROWS: usize = 2;

/// The persistent row kept at the foot of the left pane to create a session
/// without remembering the `c` shortcut. It is a navigation target, not a
/// session, and turns into the inline `+ new: <name>` input when activated.
pub(super) const CREATE_ROW_LABEL: &str = "+ new session";

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
pub(super) fn tint_by_load(field: String, load: Load) -> String {
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
pub(super) fn nested_resource_line(
    usage: ResourceUsage,
    detail_width: usize,
    selected: bool,
    active: bool,
    in_overview: bool,
    nesting_depth: usize,
) -> String {
    let lineage_cols = session_name_prefix_width(nesting_depth).min(detail_width);
    let detail_width = detail_width.saturating_sub(lineage_cols);
    let detail = style(clip_to_width(&resource_inline_label(usage), detail_width))
        .dim()
        .to_string();
    nested_detail_line(
        &session_gutter_cell(selected, active, in_overview, 2),
        lineage_cols,
        detail,
    )
}

/// The line-1 memo cell at the row's right edge: a yellow [`NOTE_ICON`] when the
/// session carries a note, else blank. Three display columns wide either way (a
/// leading and trailing space frame the glyph) so the rows line up whether or not
/// a note is present — it reuses the column the old active marker left blank.
pub(super) fn note_cell(has_note: bool) -> String {
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
#[cfg(test)]
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
    in_overview: bool,
    live: bool,
    running: bool,
    waiting: bool,
    done: bool,
    // While inline-renaming this (selected) session in 選択, the label being typed
    // and the caret's byte offset into it: line 1's name cell becomes that
    // editable field in place, so the rename happens on the row itself rather than
    // in a separate input at the list foot.
    rename: Option<(&str, usize)>,
) -> (String, String) {
    nested_worktree_row(
        worktree,
        label,
        status_label,
        label_col,
        name_width,
        detail_width,
        cols,
        has_note,
        now,
        selected,
        active,
        in_overview,
        live,
        running,
        waiting,
        done,
        rename,
        0,
        SessionOrigin::Unknown,
    )
}

/// [`worktree_row`] with a visual lineage depth. Depth `0` is the regular
/// top-level session row; depth `1+` draws the row as a child of a visible parent
/// session created by an agent through MCP.
#[allow(clippy::too_many_arguments)]
pub(super) fn nested_worktree_row(
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
    in_overview: bool,
    live: bool,
    running: bool,
    waiting: bool,
    done: bool,
    // While inline-renaming this (selected) session in 選択, the label being typed
    // and the caret's byte offset into it: line 1's name cell becomes that
    // editable field in place, so the rename happens on the row itself rather than
    // in a separate input at the list foot.
    rename: Option<(&str, usize)>,
    nesting_depth: usize,
    origin: SessionOrigin,
) -> (String, String) {
    let kind = kind_dot(heat_of(worktree.updated_at, now));
    let gutter = session_gutter_cell(selected, active, in_overview, 0);
    let line1 = if let Some((value, cursor)) = rename {
        // Inline rename: the session's own name line turns into the editable label
        // with a block caret. The gutter cursor and kind dot stay put so the row
        // does not shift, and the field runs across where the note and status
        // fields sat (dropped while editing) so a longer name has room to type.
        let (before, after) = value.split_at(cursor);
        let field = widgets::block_caret(before, after, &Style::new().accent().bold());
        let field_width = name_width + label_col + ACTIVE_COL + 1;
        let field = nested_inline_field(&field, field_width, nesting_depth);
        format!("{gutter} {kind} {field}")
    } else {
        // The session's sidebar label (its custom display name, or the branch when
        // unset); a detached worktree with no label falls back to the placeholder.
        let name = if label.is_empty() {
            worktree.branch.as_deref().unwrap_or(DETACHED)
        } else {
            label
        };
        let branch =
            nested_session_name_cell(name, name_width, active || selected, nesting_depth, origin);
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
    let lineage_cols = session_name_prefix_width(nesting_depth).min(detail_width);
    let detail_width = detail_width.saturating_sub(lineage_cols);
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
    let line2 = nested_detail_line(
        &session_gutter_cell(selected, active, in_overview, 1),
        lineage_cols,
        detail,
    );
    (line1, line2)
}

/// A compact, dimmed freshness label for how long ago `then` was relative to
/// `now`: `now` under a minute, then `Nm ago` / `Nh ago` / `Nd ago`. A `then`
/// in the future (clock skew) clamps to `now`. Shown on line 2 so a glance
/// tells the stale sessions from the freshly-touched ones. The minute unit is
/// abbreviated to a single `m` (not `min`) to keep the column narrow and spend
/// the freed width on the rest of the detail line.
pub(super) fn relative_time(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
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
pub(super) fn digits(mut n: usize) -> usize {
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
    pub(super) time: usize,
    /// Digit width of the `↑N` (ahead) count; 0 = no visible session is ahead.
    pub(super) ahead: usize,
    /// Digit width of the `↓N` (behind) count; 0 = no visible session is behind.
    pub(super) behind: usize,
    /// Digit widths of the diff `+N` / `-M` counts; `added == 0` drops the badge.
    pub(super) added: usize,
    pub(super) removed: usize,
    /// Display width of the `<icon> <count>` PR badge (the glyph, a space, and the
    /// widest count's digits). Reserved at [`PR_RESERVE_WIDTH`] even when no visible
    /// session has a PR, so the column never collapses and shifts the diff beside it.
    pub(super) pr: usize,
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
pub(super) fn pr_cell(prs: &[PrLink], width: usize) -> String {
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
pub(super) fn pr_width(prs: &[PrLink]) -> usize {
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
pub(super) const PR_RESERVE_WIDTH: usize = 3;

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
pub(super) const TIME_RESERVE_WIDTH: usize = 7;

/// Columns the `↑` / `↓` commit-divergence arrows occupy: one each. The arrows are
/// East Asian *Ambiguous*, which usagi's terminals paint one column wide — the
/// width [`console::measure_text_width`] counts and the width every sidebar cell is
/// reserved and clipped at (see [`name_cell`]). Keeping line 2's `↑N ↓M` math on
/// that same plain width is what pins the `│` divider to `left_w` on the detail
/// rows instead of jogging it left.
pub(super) const COMMIT_ARROW_WIDTH: usize = 1;

impl DetailCols {
    /// Width of the `↑N ↓M` commit cell — only the sides some visible session uses
    /// are reserved (a pane with nothing behind spends no columns on `↓`), with a
    /// one-space gap when both sides are present. Each arrow is
    /// [`COMMIT_ARROW_WIDTH`] column (plain width, matching the terminal).
    pub(super) fn commits_width(self) -> usize {
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
    pub(super) fn badge_width(self) -> usize {
        if self.added > 0 {
            self.added + self.removed + 3
        } else {
            0
        }
    }

    /// Total width of the right cluster: every active field plus a one-space gap
    /// between each pair of adjacent fields.
    pub(super) fn cluster_width(self) -> usize {
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
pub(super) fn detail_cols(
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
pub(super) fn rpad(content: &str, width: usize) -> String {
    let pad = width.saturating_sub(console::measure_text_width(content));
    format!("{}{content}", " ".repeat(pad))
}

/// The `+N -M` diff cell for a worktree's [`DiffStat`] — additions green,
/// deletions red — laid out in fixed `added_w` / `removed_w` digit columns so the
/// `+` and `-` align down the list regardless of each session's change count. A
/// row with no diff fills the same width with blanks, holding the column.
pub(super) fn diff_cell(diff: Option<DiffStat>, added_w: usize, removed_w: usize) -> String {
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
pub(super) fn commits_cell(ab: Option<AheadBehind>, ahead_w: usize, behind_w: usize) -> String {
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
pub(super) fn detail_content(agent: AgentState, cells: &[String], width: usize) -> String {
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
/// The far-left gutter carries the `>` cursor (in 選択 (Overview)) or the green `▎`
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
    in_overview: bool,
) -> (String, String) {
    let kind = root_glyph();
    let name = name_cell(ROOT_NAME, name_width, active || selected);
    let gutter = gutter_cell(selected, active, in_overview);
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
    let line2 = detail_line(&gutter_cell(false, active, in_overview), detail);
    (line1, line2)
}

/// A session's freshness, derived from how long ago it was last touched —
/// switched to, or seen producing terminal/agent activity. Drives the sidebar
/// kind dot's glyph and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Heat {
    /// Touched within the last [`HEAT_FRESH`].
    Fresh,
    /// Touched within the last [`HEAT_WARM`] (but not [`HEAT_FRESH`]).
    Warm,
    /// Touched longer ago than [`HEAT_WARM`], or never since creation.
    Cold,
}

/// A session touched more recently than this reads as [`Heat::Fresh`].
pub(super) const HEAT_FRESH_MINUTES: i64 = 15;
/// A session touched more recently than this — but not [`HEAT_FRESH_MINUTES`] —
/// reads as [`Heat::Warm`]; anything older is [`Heat::Cold`].
pub(super) const HEAT_WARM_HOURS: i64 = 4;

/// Classify a session's freshness from its last-active time and the current
/// time. A negative age (a clock that went backwards) is treated as fresh, the
/// safe side — a session is never shown colder than it is.
pub(super) fn heat_of(last_active: DateTime<Utc>, now: DateTime<Utc>) -> Heat {
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
pub(super) fn kind_dot(heat: Heat) -> String {
    match heat {
        Heat::Fresh => style("●").success().to_string(),
        Heat::Warm => style("◐").to_string(),
        Heat::Cold => style("○").dim().to_string(),
    }
}

/// The workspace root's kind glyph (`⌂`, magenta) — shown in the slot where a
/// worktree shows its [`kind_dot`], by both the full sidebar ([`root_row`]) and
/// the collapsed rail ([`rail_pane`]).
pub(super) fn root_glyph() -> String {
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
/// the compact `>` cursor in 選択.
#[allow(clippy::too_many_arguments)]
pub(super) fn rail_entry(
    selected: bool,
    session: bool,
    active: bool,
    in_overview: bool,
    kind: &str,
    label: Option<&str>,
    agent: Option<&str>,
) -> (String, String, String) {
    let gutter = if session {
        session_gutter_cell(selected, active, in_overview, 0)
    } else {
        gutter_cell(selected, active, in_overview)
    };
    let detail_gutter = if session {
        session_gutter_cell(selected, active, in_overview, 1)
    } else {
        gutter_cell(false, active, in_overview)
    };
    let resource_gutter = if session {
        session_gutter_cell(selected, active, in_overview, 2)
    } else {
        gutter_cell(false, active, in_overview)
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
/// as real rows (`>` only in 選択 when selected) but has no heat/status/resource
/// lines because it is an action target rather than a session. The row is still
/// always present at the list foot so keyboard focus and mouse clicks can enter
/// the create input from a visible affordance.
pub(super) fn create_row(selected: bool, in_overview: bool, width: usize) -> String {
    let gutter = gutter_cell(selected, false, in_overview);
    let label = if selected && in_overview {
        style(CREATE_ROW_LABEL).green().bold().to_string()
    } else {
        style(CREATE_ROW_LABEL).green().to_string()
    };
    clip_to_width(&format!("{gutter} {label}"), width)
}

/// How often (ms) the inline create skeleton's loading wave advances one column.
/// Fast enough to read as live motion while the background git work runs; the
/// frame is derived from the wall clock so every skeleton on screen pulses in
/// step and needs no per-row tick threaded through the state.
const SKELETON_TICK_MS: i64 = 90;

/// The animation frame for the create skeletons this paint, derived from the
/// frame's wall-clock instant so the wave advances between repaints.
pub(super) fn skeleton_frame(now: DateTime<Utc>) -> usize {
    (now.timestamp_millis().rem_euclid(1 << 30) / SKELETON_TICK_MS) as usize
}

/// Render a loading wave in a leaf-green (`success`) band rather than the
/// blue/cyan accent used for creation. Used by removal skeletons so a row being
/// pruned reads differently from a row being born.
fn leaf_loading_chip(text: &str, frame: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let period = chars.len() + 3;
    let head = frame % period;
    let mut out = String::new();
    for (i, c) in chars.into_iter().enumerate() {
        let s = c.to_string();
        if i == head || i + 1 == head {
            out.push_str(&style(s).success().bold().to_string());
        } else {
            out.push_str(&style(s).dim().to_string());
        }
    }
    out
}

/// The full-sidebar **pending session** placeholder inserted above a workspace's
/// persistent "+ new session" row while a session by `name` is being created.
/// It occupies the same three rows as a real session — name, detail, resource —
/// so the list is already at the final height before the worker finishes and the
/// real row lands. The name shimmers with the same wave as loading tab chips,
/// the detail row is intentionally blank (height only), and the third row keeps
/// the CPU/MEM field visible (at the default zero sample) with the same
/// left-to-right shimmer so the placeholder has the exact shape of a session
/// entry.
pub(super) fn pending_session_rows(
    name: &str,
    frame: usize,
    label_col: usize,
    name_width: usize,
    detail_width: usize,
    in_overview: bool,
) -> [String; SESSION_ROWS] {
    // The placeholder is not itself selectable; the persistent "+ new session"
    // row remains below it and keeps the cursor/activation target.
    let gutter = session_gutter_cell(false, false, in_overview, 0);
    let kind = super::tabs_hit::loading_chip("●", frame);
    let wave = super::tabs_hit::loading_chip(&clip_to_width(name, name_width), frame);
    let name = pad_to_width(wave, name_width);
    let status_tag = label_cell(None, label_col);
    let line1 = format!("{gutter} {kind} {name}{status_tag}{}", note_cell(false));
    let line2 = detail_line(
        &session_gutter_cell(false, false, in_overview, 1),
        String::new(),
    );
    let resource = widgets::shimmer_text(
        &clip_to_width(
            &resource_inline_label(ResourceUsage::default()),
            detail_width,
        ),
        frame,
    );
    let line3 = detail_line(&session_gutter_cell(false, false, in_overview, 2), resource);
    [line1, line2, line3]
}

/// The full-sidebar skeleton that replaces a session row while that session is
/// being removed. It keeps the same three-row footprint as the real row but uses
/// a leaf-green wave on the name line, visually separating deletion from the
/// blue/cyan create skeleton while leaving the lower two rows height-only.
#[allow(clippy::too_many_arguments)]
pub(super) fn removing_session_rows(
    label: &str,
    frame: usize,
    label_col: usize,
    name_width: usize,
    detail_width: usize,
    selected: bool,
    active: bool,
    in_overview: bool,
    nesting_depth: usize,
) -> [String; SESSION_ROWS] {
    let gutter = session_gutter_cell(selected, active, in_overview, 0);
    let kind = leaf_loading_chip("✂", frame);
    let wave = leaf_loading_chip(&clip_to_width(label, name_width), frame);
    let name = nested_inline_field(&wave, name_width, nesting_depth);
    let name = pad_to_width(name, name_width);
    let status_tag = label_cell(None, label_col);
    let line1 = format!("{gutter} {kind} {name}{status_tag}{}", note_cell(false));
    let lineage_cols = lineage_width(nesting_depth).min(detail_width);
    let line2 = nested_detail_line(
        &session_gutter_cell(selected, active, in_overview, 1),
        lineage_cols,
        String::new(),
    );
    let line3 = nested_detail_line(
        &session_gutter_cell(selected, active, in_overview, 2),
        lineage_cols,
        String::new(),
    );
    [line1, line2, line3]
}

/// The rail twin of a pending session skeleton: three rows so the collapsed rail
/// reserves the same height as a real session. The rail has no room for the name
/// or CPU/MEM text, so only the top `+` pulses and the lower rows are blanks.
pub(super) fn rail_pending_session_rows(frame: usize) -> [String; SESSION_ROWS] {
    [
        rail_create_skeleton_row(frame),
        pad_to_width(String::new(), RAIL_WIDTH),
        pad_to_width(String::new(), RAIL_WIDTH),
    ]
}

/// A pulsing `+` glyph used by [`rail_pending_session_rows`] while a session is
/// being created into this workspace. The rail twin of the full pending-session
/// skeleton has no room for the name, so only this glyph animates.
pub(super) fn rail_create_skeleton_row(frame: usize) -> String {
    let plus = super::tabs_hit::loading_chip("+", frame);
    let gutter = gutter_cell(false, false, false);
    pad_to_width(format!("{gutter} {plus}"), RAIL_WIDTH)
}

/// The rail twin of [`removing_session_rows`]: a three-row in-place placeholder
/// for a session being pruned. Only the top glyph is visible in the narrow rail.
pub(super) fn rail_removing_session_rows(
    frame: usize,
    selected: bool,
    active: bool,
    in_overview: bool,
) -> [String; SESSION_ROWS] {
    let cut = leaf_loading_chip("✂", frame);
    let top = pad_to_width(
        format!(
            "{} {cut}",
            session_gutter_cell(selected, active, in_overview, 0)
        ),
        RAIL_WIDTH,
    );
    let detail = pad_to_width(
        session_gutter_cell(selected, active, in_overview, 1),
        RAIL_WIDTH,
    );
    let resource = pad_to_width(
        session_gutter_cell(selected, active, in_overview, 2),
        RAIL_WIDTH,
    );
    [top, detail, resource]
}

/// The rail twin of [`create_row`]: the `+` glyph in the same row position. The
/// input itself moves to the right pane while the sidebar is collapsed, but the
/// click / focus target remains visible at the rail's bottom.
pub(super) fn rail_create_row(selected: bool, in_overview: bool) -> String {
    let gutter = gutter_cell(selected, false, in_overview);
    let label = if selected && in_overview {
        style("+").green().bold().to_string()
    } else {
        style("+").green().to_string()
    };
    pad_to_width(format!("{gutter} {label}"), RAIL_WIDTH)
}

pub(super) fn push_unite_workspace_gap(win: &mut LineWindow, width: usize) {
    for _ in 0..UNITE_WORKSPACE_GAP_ROWS {
        win.push(pad_to_width(String::new(), width));
    }
}

pub(super) fn line_hits_unite_workspace_gap(line: usize, cur: &mut usize) -> bool {
    if line < *cur + UNITE_WORKSPACE_GAP_ROWS {
        return true;
    }
    *cur += UNITE_WORKSPACE_GAP_ROWS;
    false
}

/// Pending **create** skeletons that belong to `root`; each one occupies a full
/// session-height placeholder above the group's persistent "+ new session" row.
/// Removals are excluded here: they replace an existing session row in place (see
/// [`is_removing`]) rather than adding a new placeholder line, so they never
/// change the group's row count.
fn pending_sessions_for_root<'a>(
    pending_sessions: &'a [PendingSession],
    root: &Path,
) -> Vec<&'a PendingSession> {
    pending_sessions
        .iter()
        .filter(|p| p.is_create() && p.root() == root)
        .collect()
}

/// Number of pending **create** skeletons currently inserted into `root`'s group
/// (the only kind that adds rows; see [`pending_sessions_for_root`]).
fn pending_session_count(pending_sessions: &[PendingSession], root: &Path) -> usize {
    pending_sessions_for_root(pending_sessions, root).len()
}

/// Whether the session named `name` under `root` is currently being removed, so
/// its existing row should render as an in-place removal skeleton instead of the
/// normal session row. Unlike a create, a removal keeps the row it replaces, so
/// this changes only how that row is drawn — never the group's row count.
fn is_removing(pending_sessions: &[PendingSession], root: &Path, name: &str) -> bool {
    pending_sessions
        .iter()
        .any(|p| p.is_remove() && p.root() == root && p.name() == name)
}

/// The number of body lines one workspace group occupies in the sidebar: the
/// (統合(unite)) inter-workspace gap and group header, the two-row root entry, the
/// one-row divider, then [`SESSION_ROWS`] rows per existing session, then any
/// pending-create skeletons (also [`SESSION_ROWS`] rows each), then the trailing
/// "+ new session" row.
/// `with_headers` matches the full sidebar, which heads each 統合 group with its
/// name; the collapsed rail draws no header (but keeps the gap), so it passes
/// `false`. Shared by the create/rename insert anchor
/// ([`group_inline_insert_line`]) and the scroll maths
/// ([`sidebar_total_lines`] / [`selected_row_span`]) so every caller walks the
/// one layout [`left_pane`] / [`rail_pane`] draw.
fn group_block_rows_with_pending(
    list: &WorktreeList,
    group_index: usize,
    worktree_count: usize,
    with_headers: bool,
    pending_count: usize,
) -> usize {
    let united = list.group_count() > 1;
    let gap = usize::from(united && group_index > 0) * UNITE_WORKSPACE_GAP_ROWS;
    // A folded workspace draws a single header line in place of its whole block.
    if list.is_collapsed(group_index) {
        return gap + 1;
    }
    let header = usize::from(united && with_headers);
    let body = SESSION_ROWS * worktree_count;
    let pending = SESSION_ROWS * pending_count;
    // Each expanded workspace ends with its own "+ new session" row (the final
    // `+ 1`), so creating a session lands in the workspace it sits under.
    gap + header + 2 + 1 + body + pending + 1
}

/// The total body lines the sidebar draws for `list`: every group's block (each
/// expanded one already includes its own trailing "+ new session" row).
/// `with_headers` matches the sidebar variant (full draws 統合 group headers, the
/// rail does not). The scroll offset clamps against this so the window never runs
/// past the list's foot.
fn sidebar_total_lines_with_pending(
    list: &WorktreeList,
    with_headers: bool,
    pending_sessions: &[PendingSession],
) -> usize {
    list.groups()
        .iter()
        .enumerate()
        .map(|(i, g)| {
            group_block_rows_with_pending(
                list,
                i,
                g.worktrees().len(),
                with_headers,
                pending_session_count(pending_sessions, g.root_path()),
            )
        })
        .sum::<usize>()
}

/// The `(start line, height)` the selected row occupies in the full-column
/// layout: the single folded header line of a collapsed workspace, the two-row
/// root entry, one [`SESSION_ROWS`] block per session, or a group's "+ new session"
/// row. Walks the same layout as [`sidebar_row_at_line_walk`] so the scroll offset
/// reveals exactly the row the renderer draws as selected.
#[cfg(test)]
pub(super) fn selected_row_span(list: &WorktreeList, with_headers: bool) -> (usize, usize) {
    selected_row_span_with_pending(list, with_headers, &[])
}

fn selected_row_span_with_pending(
    list: &WorktreeList,
    with_headers: bool,
    pending_sessions: &[PendingSession],
) -> (usize, usize) {
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
        for _ in group.worktrees() {
            if flat == sel {
                return (cur, SESSION_ROWS);
            }
            cur += SESSION_ROWS;
            flat += 1;
        }
        // Pending create skeletons are not selectable, but they reserve the exact
        // three rows the created session will occupy, keeping the following
        // persistent "+ new session" row at its final landing position.
        cur += SESSION_ROWS * pending_session_count(pending_sessions, group.root_path());
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
#[cfg(test)]
pub(super) fn sidebar_scroll(
    list: &WorktreeList,
    with_headers: bool,
    viewport_rows: usize,
) -> usize {
    sidebar_scroll_with_pending(list, with_headers, viewport_rows, &[])
}

pub(super) fn sidebar_scroll_with_pending(
    list: &WorktreeList,
    with_headers: bool,
    viewport_rows: usize,
    pending_sessions: &[PendingSession],
) -> usize {
    let total = sidebar_total_lines_with_pending(list, with_headers, pending_sessions);
    if viewport_rows == 0 || total <= viewport_rows {
        return 0;
    }
    let (start, len) = selected_row_span_with_pending(list, with_headers, pending_sessions);
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
pub(super) struct LineWindow {
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
/// and, in 選択, the `>` cursor and the dimming of the other entries, so the rail
/// still shows which session is selected and what its agent is doing without
/// spelling out their names.
#[allow(clippy::too_many_arguments)]
pub(super) fn rail_pane(
    list: &WorktreeList,
    live: &HashSet<PathBuf>,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
    pending_sessions: &[PendingSession],
    label_master: &SessionLabelMaster,
    rows: usize,
    in_overview: bool,
    now: DateTime<Utc>,
) -> Vec<String> {
    let root = root_glyph();
    let skeleton = skeleton_frame(now);
    // Scroll so the selected entry stays on screen once the list outgrows the rail
    // (the rail draws no 統合 group header, so `with_headers` is false).
    let scroll = sidebar_scroll_with_pending(list, false, rows, pending_sessions);
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
            let mut row = rail_collapsed_group_row(selected, active, in_overview);
            if in_overview && !selected {
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
            in_overview,
            &root,
            None,
            None,
        );
        if in_overview && flat_row != list.selected_index() {
            root_top = dim_row(&root_top);
            root_detail = dim_row(&root_detail);
        }
        win.push(root_top);
        win.push(root_detail);
        flat_row += 1;
        win.push(style("─".repeat(RAIL_WIDTH)).dim().to_string());
        if group.worktrees().is_empty() {
            for _pending in pending_sessions_for_root(pending_sessions, group.root_path()) {
                for row in rail_pending_session_rows(skeleton) {
                    win.push(row);
                }
            }
            let selected = flat_row == list.selected_index();
            let mut row = rail_create_row(selected, in_overview);
            if in_overview && !selected {
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
            if is_removing(pending_sessions, group.root_path(), worktree_name(w)) {
                for row in rail_removing_session_rows(skeleton, selected, active, in_overview) {
                    win.push(row);
                }
                flat_row += 1;
                continue;
            }
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
                in_overview,
                &kind,
                label.as_deref(),
                agent.as_deref(),
            );
            if in_overview && !selected {
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
        for _pending in pending_sessions_for_root(pending_sessions, group.root_path()) {
            for row in rail_pending_session_rows(skeleton) {
                win.push(row);
            }
        }
        let selected = flat_row == list.selected_index();
        let mut row = rail_create_row(selected, in_overview);
        if in_overview && !selected {
            row = dim_row(&row);
        }
        win.push(row);
        flat_row += 1;
    }
    win.into_lines()
}

/// Re-renders an already-styled row uniformly dimmed: strips its colours and
/// wraps the plain text in `dim`. Used to fade the rows the cursor is *not* on
/// in 選択 (Overview), so the highlighted session stands out without a box.
pub(super) fn dim_row(line: &str) -> String {
    // `strip_ansi_codes` borrows the input when it carries no escapes (the common
    // case for a plain session row), so styling the `Cow` directly avoids the
    // extra owned copy `into_owned` would force before the single styled string is
    // built.
    style(console::strip_ansi_codes(line)).dim().to_string()
}

/// The flat selectable row (root rows included, matching `WorktreeList`'s row
/// space) a 0-based body `line` lands on, or `None` for a group header, a divider,
/// a unite workspace gap, a pending skeleton row, or a line past the last group.
/// Replays the exact layout [`left_pane`] / [`rail_pane`] build so a click maps
/// back to its row without the renderer and the hit test ever disagreeing.
///
/// `with_headers` matches the full sidebar (which heads each 統合(unite) group
/// with its name); the rail draws no header, so it walks the same layout minus
/// that one line per group.
pub(super) fn sidebar_row_at_line_walk_with_pending(
    list: &WorktreeList,
    line: usize,
    with_headers: bool,
    pending_sessions: &[PendingSession],
) -> Option<usize> {
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
        for _ in group.worktrees() {
            if line >= cur && line < cur + SESSION_ROWS {
                return Some(flat);
            }
            cur += SESSION_ROWS;
            flat += 1;
        }
        let pending_rows =
            SESSION_ROWS * pending_session_count(pending_sessions, group.root_path());
        if line >= cur && line < cur + pending_rows {
            return None;
        }
        cur += pending_rows;
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
#[cfg(test)]
pub(super) fn sidebar_row_at_line_for_sidebar(
    list: &WorktreeList,
    line: usize,
    sidebar: Sidebar,
    scroll: usize,
) -> Option<usize> {
    sidebar_row_at_line_for_sidebar_with_pending(list, line, sidebar, scroll, &[])
}

pub(super) fn sidebar_row_at_line_for_sidebar_with_pending(
    list: &WorktreeList,
    line: usize,
    sidebar: Sidebar,
    scroll: usize,
    pending_sessions: &[PendingSession],
) -> Option<usize> {
    let line = line + scroll;
    match sidebar {
        Sidebar::Full => sidebar_row_at_line_walk_with_pending(list, line, true, pending_sessions),
        Sidebar::Rail => sidebar_row_at_line_walk_with_pending(list, line, false, pending_sessions),
    }
}

/// The 0-based body line of `group`'s own "+ new session" row — the last line of
/// its block. 選択's inline create input replaces this row so it renders at the
/// foot of the targeted workspace's sessions (before the next group's gap/header)
/// rather than at the foot of the whole column, which matters in 統合(unite) mode
/// where several workspaces stack. The create flow expands a folded group first,
/// so `group` is always expanded here. Walks the same layout as
/// [`sidebar_row_at_line_for_sidebar`].
#[cfg(test)]
pub(super) fn group_inline_insert_line(list: &WorktreeList, group: usize) -> usize {
    group_inline_insert_line_with_pending(list, group, &[])
}

pub(super) fn group_inline_insert_line_with_pending(
    list: &WorktreeList,
    group: usize,
    pending_sessions: &[PendingSession],
) -> usize {
    let before: usize = list
        .groups()
        .iter()
        .enumerate()
        .take(group)
        .map(|(i, g)| {
            group_block_rows_with_pending(
                list,
                i,
                g.worktrees().len(),
                true,
                pending_session_count(pending_sessions, g.root_path()),
            )
        })
        .sum();
    let block = list
        .groups()
        .get(group)
        .map(|g| {
            group_block_rows_with_pending(
                list,
                group,
                g.worktrees().len(),
                true,
                pending_session_count(pending_sessions, g.root_path()),
            )
        })
        .unwrap_or(0);
    // The create row is the final line of the group's block.
    before + block.saturating_sub(1)
}

/// Builds a 統合(unite) group header: the workspace name in bold behind a left
/// bar, clipped to the sidebar width. Drawn above each workspace's rows only when
/// more than one workspace is shown, so single-workspace mode is byte-for-byte
/// unchanged.
pub(super) fn group_header(name: &str, width: usize) -> String {
    style(clip_to_width(&format!("▌ {name}"), width))
        .bold()
        .to_string()
}

/// Builds the single line a **folded** 統合(unite) workspace draws in place of its
/// whole block: the workspace name behind a left bar with a `▸` fold marker and a
/// `(N)` session count, so a collapsed workspace still shows what it holds. It is
/// the group's navigable root slot, so it carries the same gutter cursor / active
/// bar the root entry would ([`gutter_cell`]); the caller dims it in 選択 when it
/// is not the selected row, exactly like every other row.
pub(super) fn collapsed_group_row(
    name: &str,
    sessions: usize,
    selected: bool,
    active: bool,
    in_overview: bool,
    width: usize,
) -> String {
    let gutter = gutter_cell(selected, active, in_overview);
    // Reserve the gutter cell + its trailing space, then clip-then-style the header
    // text (matching [`group_header`], whose clip runs before styling).
    let text = format!("▸ {name}  ({sessions})");
    let head = style(clip_to_width(&text, width.saturating_sub(2))).bold();
    format!("{gutter} {head}")
}

/// The rail twin of [`collapsed_group_row`]: the fold marker `▸` in the gutter
/// position, keeping the folded workspace visible (and its root slot clickable) in
/// the narrow rail, which has no room for the name or count.
pub(super) fn rail_collapsed_group_row(selected: bool, active: bool, in_overview: bool) -> String {
    let gutter = gutter_cell(selected, active, in_overview);
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
/// When `in_overview`
/// is set (in 選択), the keyboard is on the list: the selected row shows a `>`
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
    pending_sessions: &[PendingSession],
    resources: &HashMap<PathBuf, ResourceUsage>,
    label_master: &SessionLabelMaster,
    left_w: usize,
    rows: usize,
    in_overview: bool,
    sidebar: Sidebar,
    now: DateTime<Utc>,
    // The inline rename being typed (its label and caret offset) when 選択's rename
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
            pending_sessions,
            label_master,
            rows,
            in_overview,
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
    let max_lineage_w = list
        .groups()
        .iter()
        .flat_map(|g| {
            (0..g.worktrees().len()).map(|i| session_name_prefix_width(g.nesting_depth(i)))
        })
        .max()
        .unwrap_or(0)
        .min(detail_width);
    let detail_width_for_cols = detail_width.saturating_sub(max_lineage_w);
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
            if let Some(label) = agent.icon_label(detail_width_for_cols) {
                max_agent_w = max_agent_w.max(console::measure_text_width(&label));
            }
            (w.updated_at, w.diff, w.ahead_behind, pr_width(&w.pr))
        })
        .collect();
    let cols = detail_cols(&cluster_data, now, max_agent_w, detail_width_for_cols);

    // A divider separating each workspace root from its sessions — indented to
    // start under the `root` label (past the cursor and kind-icon cells).
    let indent = " ".repeat(NAME_PREFIX);
    let inner_w = left_w.saturating_sub(NAME_PREFIX);
    // In 統合(unite) mode each workspace's rows are headed by its name.
    let united = list.group_count() > 1;
    let skeleton = skeleton_frame(now);

    // Scroll so the selected row stays on screen once the list outgrows the pane;
    // the window keeps only the visible slice while the loop walks the whole layout
    // so every flat index (and its selected / active styling) stays correct.
    let scroll = sidebar_scroll_with_pending(list, true, rows, pending_sessions);
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
                in_overview,
                left_w,
            );
            if in_overview && !selected {
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
            in_overview,
        );
        if in_overview && flat_row != list.selected_index() {
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
            // No sessions yet in this workspace — the group's own create row sits
            // straight under the divider so a session can be started into this
            // (otherwise empty) workspace.
            for pending in pending_sessions_for_root(pending_sessions, group.root_path()) {
                for row in pending_session_rows(
                    pending.name(),
                    skeleton,
                    label_col,
                    name_width,
                    detail_width,
                    in_overview,
                ) {
                    win.push(row);
                }
            }
            let selected = flat_row == list.selected_index();
            let mut row = create_row(selected, in_overview, left_w);
            if in_overview && !selected {
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
            if is_removing(pending_sessions, group.root_path(), worktree_name(w)) {
                let nesting_depth = group.nesting_depth(i);
                for row in removing_session_rows(
                    group.display_label(i),
                    skeleton,
                    label_col,
                    name_width,
                    detail_width,
                    selected,
                    active,
                    in_overview,
                    nesting_depth,
                ) {
                    win.push(row);
                }
                flat_row += 1;
                continue;
            }
            let status_label = group.row_label_id(i).and_then(|id| label_master.get(id));
            let nesting_depth = group.nesting_depth(i);
            let (mut top, mut detail) = nested_worktree_row(
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
                in_overview,
                live.contains(&w.path),
                running.contains(&w.path),
                waiting.contains(&w.path),
                done.contains(&w.path),
                // The rename input targets the selected session, so its editable
                // label rides that row only; every other row renders normally.
                if selected { rename } else { None },
                nesting_depth,
                group.origin(i),
            );
            // Every session draws a third CPU / memory line at a fixed height, so
            // the list never reflows as a session goes live or idle. An unsampled
            // session shows `CPU 0%  MEM 0MB` (a default usage) rather than dropping
            // the row.
            let usage = resources.get(&w.path).copied().unwrap_or_default();
            let mut resource = nested_resource_line(
                usage,
                detail_width,
                selected,
                active,
                in_overview,
                nesting_depth,
            );
            if in_overview && !selected {
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
        for pending in pending_sessions_for_root(pending_sessions, group.root_path()) {
            for row in pending_session_rows(
                pending.name(),
                skeleton,
                label_col,
                name_width,
                detail_width,
                in_overview,
            ) {
                win.push(row);
            }
        }
        let selected = flat_row == list.selected_index();
        let mut row = create_row(selected, in_overview, left_w);
        if in_overview && !selected {
            row = dim_row(&row);
        }
        win.push(row);
        flat_row += 1;
    }
    win.into_lines()
}
