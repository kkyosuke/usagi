//! Session Garden の純粋な描画サンプル。
//!
//! daemon の状態を所有せず、表示用に閉じた [`GardenSession`] を画面内へ並べる。
//! 通常は大きな plot、件数が増えたら Agent ごとの compact card へ密度を切り替える。
//! frame と同じ layout から [`GardenHitbox`] も返すため、
//! 後続実装は座標から session identity を再計算せず click target を解決できる。

use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

use crate::presentation::theme::{Role, Style, garden_rabbit_style};

use super::agent_status;
use super::{clip_to_width, display_width, pad_to_width};
use usagi_core::domain::agent::AgentStatus as DispatchAgentStatus;

/// Garden を表示できる最小端末幅。
pub const MIN_WIDTH: usize = 64;
/// Project tab bar を除く Garden 本体を表示できる最小高さ。
pub const MIN_HEIGHT: usize = 13;

const SIDE_PADDING: usize = 2;
const HEADER_ROWS: usize = 2;
const FOOTER_ROWS: usize = 1;
const PLOT_WIDTH: usize = 28;
const GROUND_ROWS: usize = 2;
const PLOT_HEIGHT: usize = 8;
/// plot のうち、うさぎと label が占める行数（残り 2 行が草地と土）。
const PLOT_CONTENT_ROWS: usize = PLOT_HEIGHT - GROUND_ROWS;
/// うさぎ 1 羽分の pose 行数（plot の label / status / 地面を除く）。
const SPRITE_ROWS: usize = 4;
const COMPACT_RABBIT_WIDTH: usize = 8;
/// plot の中で sprite が始まる行（nameplate と status 行の下）。うさぎの hitbox は
/// この行から [`SPRITE_ROWS`] 行ぶんで、nameplate と status 行は区画のままにする。
const SPRITE_TOP_ROW: usize = PLOT_CONTENT_ROWS - SPRITE_ROWS;
const MAX_VISIBLE_AGENTS: usize = PLOT_WIDTH / COMPACT_RABBIT_WIDTH;
const DENSE_CARD_HEIGHT: usize = 2;
const DENSE_CARD_MIN_WIDTH: usize = 14;
const TINY_CARD_MIN_WIDTH: usize = 8;
/// The interactive shell advances its logical clock every 16 ms. Holding one
/// Garden frame for eight of those ticks yields a brisk ~8 fps terminal
/// animation without rebuilding at the 62.5 Hz input-pump cadence.
const RUNTIME_TICKS_PER_ANIMATION_FRAME: u64 = 8;
const RUNNING_ACTION_CYCLE_TICKS: u64 = 25;
const RUNNING_ACTION_SEQUENCE_ROUNDS: u64 = 4;
const RUNNING_ANIMATION_CYCLE_TICKS: u64 =
    RUNNING_ACTION_CYCLE_TICKS * RUNNING_ACTION_SEQUENCE_ROUNDS;
pub(crate) const ANIMATION_CYCLE_TICKS: u64 = 300;
const AMBIENT_PHASE_TICKS: u64 = 2;
const AMBIENT_PHASES: u64 = 6;
const TWINKLE: [char; 6] = ['.', '*', '+', '*', '.', '·'];

/// Running のうさぎが繰り返す基本動作。各動作の長さを変え、runtime identity から
/// 並び順を shuffle することで、同じ phase のうさぎも一斉に同じ動きをしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RunningAction {
    Hop,
    Bound,
    Sniff,
    Dig,
    Look,
}

impl RunningAction {
    const ALL: [Self; 5] = [Self::Hop, Self::Bound, Self::Sniff, Self::Dig, Self::Look];

    const fn duration(self) -> u64 {
        match self {
            Self::Hop => 3,
            Self::Bound => 4,
            Self::Sniff => 5,
            Self::Dig => 6,
            Self::Look => 7,
        }
    }
}

/// 草地のタイル。庭の幅いっぱいに敷き詰めるため、隣り合うタイルで草の位置を変えて
/// 同じ絵が横に並ぶ tiling に見せない。
const GRASS: [&str; 3] = [
    "--v-------v-----------v-----",
    "------v---------v----v------",
    "---v----------v-------v-----",
];
/// 草地の下に薄く見せる土。ASCII だけで構成し、端末の表示幅に依存しない。
const SOIL: [&str; 3] = [
    "  .     .       .   .       ",
    "     .      .          .    ",
    " .         .    .           ",
];

/// Garden に渡す、表示に必要な session 情報だけの projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GardenSession {
    pub id: SessionId,
    pub label: String,
    pub lifecycle: SessionLifecycle,
    pub selected: bool,
    pub failure_summary: Option<String>,
    /// Whether the active workspace controller observed Agent membership for
    /// this session. Inactive project snapshots set this false instead of claiming
    /// that an empty cached list means the session owns no Agents.
    pub agents_observed: bool,
    pub agents: Vec<GardenAgent>,
    /// Daemon-owned dispatch availability from `session list`. Runtime phases
    /// remain the per-Agent detail, but a terminal dispatch state (idle,
    /// exited, or failed) overrides a stale/coarse `live -> running` badge.
    pub agent_status: Option<DispatchAgentStatus>,
    /// Pending human decisions owned by this managed session.
    ///
    /// Only the active workspace can project this value. Inactive project
    /// caches keep it at zero rather than claiming that an unobserved workspace
    /// has no questions waiting for a person.
    pub pending_decisions: usize,
    /// A short, non-blocking celebration after one of the session's PRs merges.
    pub pr_merged: bool,
}

/// Garden に描く 1 agent。runtime identity は並び順と animation sequence に使う。
///
/// sidebar の agent 行と同じ値を同じ順序で描くため、型と語彙そのものを共有する
/// （[`agent_status`]）。庭とサイドバーが別々の projection を持つと、同じ session の
/// Agent 数や優先順位が画面の 2 か所で食い違う。
pub type GardenAgent = agent_status::AgentStatus;

/// 0-based terminal cell rectangle。右端・下端は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GardenHitbox {
    pub session_id: SessionId,
    /// この rectangle が 1 羽のうさぎ（= 1 agent）なら、その stable runtime identity。
    /// `None` は session の巣穴（compact 表示では区画そのもの）で、click は session
    /// の訪問だけを意味する。
    pub agent: Option<AgentRuntimeId>,
    pub column: usize,
    pub row: usize,
    pub width: usize,
    pub height: usize,
}

impl GardenHitbox {
    /// terminal cell がこの target に含まれるか。
    #[must_use]
    pub const fn contains(self, column: usize, row: usize) -> bool {
        column >= self.column
            && column < self.column + self.width
            && row >= self.row
            && row < self.row + self.height
    }
}

/// Garden の描画結果と、同じ layout から得た click target。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GardenFrame {
    pub rows: Vec<String>,
    /// うさぎの rectangle は必ず session の巣穴／区画より **先** に並ぶ。
    /// click 解決は最初に当たったものを採るため、重なった絵では最後に描いた
    /// うさぎが優先し、すべてのうさぎは巣穴／区画に優先する。
    pub hitboxes: Vec<GardenHitbox>,
}

/// 1 区画の描画結果と、その中に置いたうさぎの横位置。
///
/// 描画と hit test が同じ 1 度の layout 計算を共有するための型である。座標を
/// あとから再計算しないので、羽数・表示上限・端末幅が変わっても click target が
/// 絵とずれない。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plot {
    rows: [String; PLOT_CONTENT_ROWS],
    rabbits: Vec<PlacedRabbit>,
}

/// 区画の左端からの offset で表した、うさぎ 1 羽ぶんの列範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlacedRabbit {
    runtime_id: AgentRuntimeId,
    offset: usize,
    width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GardenLayout {
    content_width: usize,
    columns: usize,
    plot_rows: usize,
    body_height: usize,
}

fn garden_layout(height: usize, width: usize) -> Option<GardenLayout> {
    if height < MIN_HEIGHT || width < MIN_WIDTH {
        return None;
    }
    let content_width = width.saturating_sub(SIDE_PADDING * 2);
    let columns = (content_width / PLOT_WIDTH).max(1);
    let body_height = height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let plot_rows = (body_height / PLOT_HEIGHT).max(1);
    Some(GardenLayout {
        content_width,
        columns,
        plot_rows,
        body_height,
    })
}

/// Garden を描画する。最小サイズに満たない場合は `None` を返す。
#[must_use]
pub fn render(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> Option<GardenFrame> {
    let layout = garden_layout(height, width)?;
    let detailed_capacity = layout.columns.saturating_mul(layout.plot_rows);
    if sessions.len() <= detailed_capacity
        && sessions
            .iter()
            .all(|session| session.agents.len() <= MAX_VISIBLE_AGENTS)
    {
        return render_detailed(
            height,
            width,
            workspace_name,
            sessions,
            tick,
            reduced_motion,
        );
    }
    render_dense(
        height,
        width,
        workspace_name,
        sessions,
        tick,
        reduced_motion,
    )
}

/// Render roomy session plots when every plot and every Agent rabbit fits at once.
fn render_detailed(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> Option<GardenFrame> {
    let layout = garden_layout(height, width)?;
    let used_rows = sessions
        .len()
        .div_ceil(layout.columns)
        .min(layout.plot_rows);
    let grid_top = HEADER_ROWS + layout.body_height.saturating_sub(used_rows * PLOT_HEIGHT) / 2;

    let mut rows = Vec::with_capacity(height);
    rows.push(header_line(width, workspace_name, sessions));
    rows.push(sky_line(width, workspace_name, tick, reduced_motion));

    // 使う plot 行数だけを縦中央へ寄せ、庭の下側だけが大きく空くのを避ける。
    rows.resize_with(grid_top, || " ".repeat(width));

    let mut hitboxes = Vec::with_capacity(sessions.len() * (1 + MAX_VISIBLE_AGENTS));
    let plots = sessions
        .iter()
        .map(|session| plot(session, tick, reduced_motion))
        .collect::<Vec<_>>();
    for plot_row in 0..used_rows {
        let row_start = plot_row * layout.columns;
        let row_columns = sessions.len().saturating_sub(row_start).min(layout.columns);
        let row_width = row_columns.saturating_mul(PLOT_WIDTH);
        let row_left = SIDE_PADDING + layout.content_width.saturating_sub(row_width) / 2;
        for local_row in 0..PLOT_CONTENT_ROWS {
            let mut line = " ".repeat(row_left);
            for column in 0..row_columns {
                let index = row_start + column;
                let plot = &plots[index];
                line.push_str(&pad_to_width(&plot.rows[local_row], PLOT_WIDTH));
            }
            rows.push(pad_to_width(&line, width));
        }
        // 地面は plot の下だけでなく庭の幅いっぱいに敷く。うさぎの数で地面が途切れると
        // 中央の島のように見えるため。
        rows.extend(ground_rows(layout, tick, reduced_motion));
        for column in 0..row_columns {
            let index = row_start + column;
            let session = &sessions[index];
            let plot = &plots[index];
            let plot_column = row_left + column * PLOT_WIDTH;
            let plot_row_top = grid_top + plot_row * PLOT_HEIGHT;
            // うさぎは区画の内側にあるので、区画より先に積む（click 解決は最初に
            // 当たった rectangle を採る）。
            for rabbit in &plot.rabbits {
                hitboxes.push(GardenHitbox {
                    session_id: session.id,
                    agent: Some(rabbit.runtime_id),
                    column: plot_column + rabbit.offset,
                    row: plot_row_top + SPRITE_TOP_ROW,
                    width: rabbit.width,
                    height: SPRITE_ROWS,
                });
            }
            hitboxes.push(GardenHitbox {
                session_id: session.id,
                agent: None,
                column: plot_column,
                row: plot_row_top,
                width: PLOT_WIDTH,
                height: PLOT_HEIGHT,
            });
        }
    }

    if sessions.is_empty() {
        rows.push(centered(
            width,
            &Style::new().dim().paint("No sessions in the garden"),
        ));
    }

    let footer_start = height - FOOTER_ROWS;
    rows.resize_with(footer_start, || " ".repeat(width));
    rows.push(footer_line(width));

    Some(GardenFrame { rows, hitboxes })
}

#[derive(Debug, Clone, Copy)]
struct DenseItem<'a> {
    session: &'a GardenSession,
    agent: Option<GardenAgent>,
    ordinal: usize,
    total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DenseMode {
    Card,
    Line,
    Glyph,
}

impl DenseMode {
    const fn height(self) -> usize {
        match self {
            Self::Card => DENSE_CARD_HEIGHT,
            Self::Line | Self::Glyph => 1,
        }
    }

    const fn minimum_width(self) -> usize {
        match self {
            Self::Card => DENSE_CARD_MIN_WIDTH,
            Self::Line => TINY_CARD_MIN_WIDTH,
            Self::Glyph => 2,
        }
    }
}

fn dense_items(sessions: &[GardenSession]) -> Vec<DenseItem<'_>> {
    sessions
        .iter()
        .flat_map(|session| {
            let agents = if session.agents_observed {
                agent_status::ordered(&session.agents)
            } else {
                Vec::new()
            };
            let total = agents.len();
            if agents.is_empty() {
                vec![DenseItem {
                    session,
                    agent: None,
                    ordinal: 0,
                    total: 0,
                }]
            } else {
                agents
                    .into_iter()
                    .enumerate()
                    .map(|(index, agent)| DenseItem {
                        session,
                        agent: Some(agent),
                        ordinal: index + 1,
                        total,
                    })
                    .collect()
            }
        })
        .collect()
}

fn dense_mode(layout: GardenLayout, item_count: usize) -> DenseMode {
    for mode in [DenseMode::Card, DenseMode::Line, DenseMode::Glyph] {
        let columns = layout.content_width / mode.minimum_width();
        let rows = layout.body_height / mode.height();
        if item_count <= columns.saturating_mul(rows) {
            return mode;
        }
    }
    DenseMode::Glyph
}

/// Fit every observed Agent into one frame. Each Agent owns one card and one
/// rabbit hitbox; sessions without observed Agents retain a quiet status card.
fn render_dense(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> Option<GardenFrame> {
    let layout = garden_layout(height, width)?;
    let mut items = dense_items(sessions);
    let glyph_capacity = layout
        .body_height
        .saturating_mul(layout.content_width / DenseMode::Glyph.minimum_width());
    if items.len() > glyph_capacity {
        // Empty session cards are useful context, but never displace an Agent
        // rabbit. The daemon's Agent capacity is far below this final glyph
        // capacity; retaining Agents here makes that product bound explicit in
        // the presentation fallback instead of panicking on stale empty sessions.
        items.retain(|item| item.agent.is_some());
    }
    let mode = dense_mode(layout, items.len());
    let card_height = mode.height();
    let max_columns = (layout.content_width / mode.minimum_width()).max(1);
    let columns = items.len().min(max_columns).max(1);
    let card_width = (layout.content_width / columns).max(1);
    let used_rows = items.len().div_ceil(columns).min(layout.body_height);
    let grid_height = used_rows.saturating_mul(card_height);
    let grid_top = HEADER_ROWS + layout.body_height.saturating_sub(grid_height) / 2;
    let grid_width = columns.saturating_mul(card_width);
    let grid_left = SIDE_PADDING + layout.content_width.saturating_sub(grid_width) / 2;
    let mut rows = vec![" ".repeat(width); height];
    rows[0] = header_line(width, workspace_name, sessions);
    rows[1] = sky_line(width, workspace_name, tick, reduced_motion);
    rows[height - FOOTER_ROWS] = footer_line(width);
    let mut hitboxes = Vec::with_capacity(items.len());

    for row_index in 0..used_rows {
        let mut card_rows = vec![" ".repeat(grid_left); card_height];
        for column_index in 0..columns {
            let index = row_index * columns + column_index;
            let Some(item) = items.get(index).copied() else {
                for row in &mut card_rows {
                    row.push_str(&" ".repeat(card_width));
                }
                continue;
            };
            // Cards and one-line summaries keep one unpainted column between
            // neighbours. The glyph grid already has a full two-cell tile per
            // usagi and must retain both cells for the wide `兎` character.
            let content_width = if mode == DenseMode::Glyph {
                card_width
            } else {
                card_width.saturating_sub(1).max(1)
            };
            let card = dense_item_rows(item, content_width, mode);
            for (row, content) in card_rows.iter_mut().zip(card) {
                row.push_str(&pad_to_width(
                    &clip_to_width(&content, card_width),
                    card_width,
                ));
            }
            hitboxes.push(GardenHitbox {
                session_id: item.session.id,
                agent: item.agent.map(|agent| agent.runtime_id),
                column: grid_left + column_index * card_width,
                row: grid_top + row_index * card_height,
                width: card_width,
                height: card_height,
            });
        }
        for (offset, content) in card_rows.into_iter().enumerate() {
            rows[grid_top + row_index * card_height + offset] = pad_to_width(&content, width);
        }
    }
    Some(GardenFrame { rows, hitboxes })
}

fn dense_item_rows(item: DenseItem<'_>, width: usize, mode: DenseMode) -> Vec<String> {
    match mode {
        DenseMode::Card => {
            let title = dense_title(item, width);
            let activity = item.agent.map_or_else(
                || {
                    let (style, summary) = session_summary(item.session);
                    centered(width, &style.paint(&summary))
                },
                |agent| centered(width, &dense_rabbit(item.session, agent)),
            );
            vec![title, activity]
        }
        DenseMode::Line => {
            let content = item.agent.map_or_else(
                || format!("· {}", item.session.label),
                |agent| {
                    let (style, glyph, _, _) = dense_agent_appearance(item.session, agent);
                    style.paint(&format!("{glyph}兎 {}", item.session.label))
                },
            );
            vec![pad_to_width(&clip_to_width(&content, width), width)]
        }
        DenseMode::Glyph => {
            let content = item.agent.map_or_else(
                || Style::new().dim().paint("·"),
                |agent| {
                    let (style, _, _, _) = dense_agent_appearance(item.session, agent);
                    style.paint("兎")
                },
            );
            vec![centered(width, &content)]
        }
    }
}

fn dense_title(item: DenseItem<'_>, width: usize) -> String {
    let prefix = (item.total > 1).then(|| format!("{}/{} ", item.ordinal, item.total));
    let label = format!("{}{}", prefix.as_deref().unwrap_or(""), item.session.label);
    let Some(agent) = item.agent else {
        return centered(width, &Role::Feature.style().bold().paint(&label));
    };
    let (style, glyph, _, _) = dense_agent_appearance(item.session, agent);
    let marker = style.paint(glyph);
    let label_width = width.saturating_sub(display_width(&marker) + 1);
    let label = Role::Feature
        .style()
        .bold()
        .paint(&clip_to_width(&label, label_width));
    pad_to_width(&format!("{marker} {label}"), width)
}

fn dense_rabbit(session: &GardenSession, agent: GardenAgent) -> String {
    let (style, _, status, face) = dense_agent_appearance(session, agent);
    let short = match status {
        "starting" => "start",
        "completed" => "done",
        "stopped" => "stop",
        "failed" => "fail",
        "running" => "run",
        "waiting" => "wait",
        "interrupted" => "pause",
        "sleeping" => "sleep",
        "closing" => "close",
        _ => status,
    };
    style.paint(&format!("/)/){face} {short}"))
}

fn dense_agent_appearance(
    session: &GardenSession,
    agent: GardenAgent,
) -> (Style, &'static str, &'static str, &'static str) {
    match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            return (Role::Accent.style(), "○", "starting", "(. .)");
        }
        SessionLifecycle::Deleting => {
            return (Style::new().dim(), "◦", "closing", "(-.-)");
        }
        SessionLifecycle::Failed => {
            return (Role::Danger.style().bold(), "◆", "failed", "(x.x)");
        }
        SessionLifecycle::Available => {}
    }
    match session.agent_status {
        Some(DispatchAgentStatus::Starting) => (Role::Accent.style(), "○", "starting", "(. .)"),
        Some(DispatchAgentStatus::Idle) => (Style::new().dim(), "◦", "completed", "(-.-)"),
        Some(DispatchAgentStatus::Exited) => (Style::new().dim(), "◦", "stopped", "(-.-)"),
        Some(DispatchAgentStatus::Failed) => (Role::Danger.style().bold(), "◆", "failed", "(x.x)"),
        Some(DispatchAgentStatus::Running) | None => {
            let face = match agent.phase {
                AgentPhase::Waiting => "(o.o)?",
                AgentPhase::Interrupted => "(-.-)!",
                AgentPhase::Sleeping | AgentPhase::Ended | AgentPhase::Exited => "(-.-)",
                AgentPhase::Absent | AgentPhase::Ready => "(. .)",
                AgentPhase::Running => "(o.o)",
            };
            (
                agent_status::style(agent.phase),
                agent_status::glyph(agent.phase),
                agent_status::label(agent.phase),
                face,
            )
        }
    }
}

fn pending_decision_summary(count: usize) -> (Style, String) {
    let noun = if count == 1 { "decision" } else { "decisions" };
    let verb = if count == 1 { "needs" } else { "need" };
    (
        Role::Warning.style(),
        format!("{count} {noun} {verb} your input."),
    )
}

/// Explain a session whose Agent inventory has no runtime row to display.
fn session_summary(session: &GardenSession) -> (Style, String) {
    if session.pending_decisions > 0 {
        return pending_decision_summary(session.pending_decisions);
    }
    if session.pr_merged {
        return (Role::Success.style(), "PR merged.".to_owned());
    }
    match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            return (Role::Warning.style(), "Session is starting.".to_owned());
        }
        SessionLifecycle::Deleting => {
            return (Style::new().dim(), "Session is closing.".to_owned());
        }
        SessionLifecycle::Failed => {
            return (Role::Danger.style(), "Session failed.".to_owned());
        }
        SessionLifecycle::Available => {}
    }
    if !session.agents_observed {
        return (Style::new().dim(), "Status is unavailable.".to_owned());
    }
    match session.agent_status {
        Some(DispatchAgentStatus::Starting) => {
            (Role::Accent.style(), "Agent is starting.".to_owned())
        }
        Some(DispatchAgentStatus::Idle) => (Role::Success.style(), "Agent completed.".to_owned()),
        Some(DispatchAgentStatus::Exited) => (Style::new().dim(), "Agent stopped.".to_owned()),
        Some(DispatchAgentStatus::Failed) => (Role::Danger.style(), "Agent failed.".to_owned()),
        Some(DispatchAgentStatus::Running) => {
            // The daemon's durable dispatch state can arrive one refresh before
            // runtime inventory. Keep reporting the stronger known fact during
            // that short observation gap.
            (Role::Success.style(), "Agent is working.".to_owned())
        }
        None => (Style::new().dim(), "No agent activity.".to_owned()),
    }
}

/// Convert the shell's monotonic 16 ms logical clock into the Garden's own
/// animation cadence. This clock is independent from wall-clock labels.
#[must_use]
pub const fn runtime_tick(shell_tick: u64) -> u64 {
    shell_tick / RUNTIME_TICKS_PER_ANIMATION_FRAME
}

/// First tick of the current run of identical visible Garden plots.
/// Folding onto this representative lets frame material equality suppress a redraw when a slow
/// animation holds its current pose. The search normally checks only a handful of preceding ticks,
/// instead of rendering the whole 300-tick cycle. `None` means the Garden does not fit and the
/// caller must preserve the ordinary Home clock.
#[must_use]
pub fn canonical_tick(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> Option<u64> {
    let layout = garden_layout(height, width)?;
    if reduced_motion {
        return Some(0);
    }
    let tick = tick % ANIMATION_CYCLE_TICKS;
    let detailed_capacity = layout.columns.saturating_mul(layout.plot_rows);
    if sessions.len() > detailed_capacity
        || sessions
            .iter()
            .any(|session| session.agents.len() > MAX_VISIBLE_AGENTS)
    {
        return Some(tick - tick % AMBIENT_PHASE_TICKS);
    }
    let expected_ambient = ambient_phase(tick, false);
    let expected = sessions
        .iter()
        .map(|session| plot(session, tick, reduced_motion))
        .collect::<Vec<_>>();
    let mut canonical = 0;
    for distance in 1..ANIMATION_CYCLE_TICKS {
        let candidate = (tick + ANIMATION_CYCLE_TICKS - distance) % ANIMATION_CYCLE_TICKS;
        let same = ambient_phase(candidate, false) == expected_ambient
            && sessions
                .iter()
                .map(|session| plot(session, candidate, reduced_motion))
                .eq(expected.iter().cloned());
        if !same {
            canonical = (candidate + 1) % ANIMATION_CYCLE_TICKS;
            break;
        }
    }
    Some(canonical)
}

fn header_line(width: usize, workspace_name: &str, sessions: &[GardenSession]) -> String {
    let rabbits = sessions
        .iter()
        .filter(|session| session.agents_observed)
        .map(|session| session.agents.len())
        .sum::<usize>();
    let attention = sessions
        .iter()
        .filter(|session| needs_attention(session))
        .count();
    let left = Role::Feature.style().bold().paint(&format!(
        " ✦ garden / {}",
        clip_to_width(workspace_name, width / 2)
    ));
    let attention = if attention == 0 {
        Role::Success.style().paint("all clear")
    } else {
        let verb = if attention == 1 { "needs" } else { "need" };
        Role::Warning
            .style()
            .bold()
            .paint(&format!("{attention} {verb} attention"))
    };
    let session_noun = if sessions.len() == 1 {
        "session"
    } else {
        "sessions"
    };
    let right = format!(
        "{} · {attention} · {} ",
        Style::new()
            .dim()
            .paint(&format!("{} {session_noun}", sessions.len())),
        Style::new().dim().paint(&format!("{rabbits} usagi"))
    );
    let gap = width.saturating_sub(display_width(&left) + display_width(&right));
    pad_to_width(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

/// Header の下に置く静かな空。装飾位置は workspace 名だけから決まり、refresh で
/// 星が飛び回らない。ASCII の `.` / `*` だけなので端末ごとの表示幅差もない。
fn sky_line(width: usize, workspace_name: &str, tick: u64, reduced_motion: bool) -> String {
    let content_width = width.saturating_sub(SIDE_PADDING * 2);
    let mut sky = vec![' '; content_width];
    let seed = stable_hash(workspace_name);
    let ornaments = (content_width / 18).clamp(2, 6);
    let phase = usize::try_from(ambient_phase(tick, reduced_motion)).unwrap_or_default();
    for index in 0..ornaments {
        let mixed = seed.rotate_left(u32::try_from(index * 9).unwrap_or_default())
            ^ u64::try_from(index)
                .unwrap_or_default()
                .wrapping_mul(0x9e37_79b9);
        let column = usize::try_from(
            mixed % u64::try_from(content_width).expect("Garden content width is non-zero"),
        )
        .expect("sky column fits usize");
        sky[column] = TWINKLE[(phase + index * 2) % TWINKLE.len()];
    }
    let sky = sky.into_iter().collect::<String>();
    pad_to_width(
        &format!(
            "{}{}",
            " ".repeat(SIDE_PADDING),
            Style::new().dim().paint(&sky)
        ),
        width,
    )
}

fn footer_line(width: usize) -> String {
    let left = " Garden Action Center · click a usagi";
    let right = "any key · wake ";
    let left = Role::Feature.style().paint(left);
    let right = Style::new().dim().paint(right);
    let gap = width.saturating_sub(display_width(&left) + display_width(&right));
    pad_to_width(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

fn plot(session: &GardenSession, tick: u64, reduced_motion: bool) -> Plot {
    let label = signpost(&session.label);
    let ([mut status, ears, head, body, feet], rabbits) =
        if session.agents_observed && !session.agents.is_empty() {
            agent_plot(session, tick, reduced_motion)
        } else if session.agents_observed {
            match session.lifecycle {
                SessionLifecycle::Available if session.pr_merged => {
                    (celebration_plot(tick, reduced_motion), Vec::new())
                }
                SessionLifecycle::Available => match session.agent_status {
                    Some(
                        status @ (DispatchAgentStatus::Starting
                        | DispatchAgentStatus::Idle
                        | DispatchAgentStatus::Exited
                        | DispatchAgentStatus::Failed),
                    ) => (dispatch_plot(session, status), Vec::new()),
                    Some(DispatchAgentStatus::Running) | None => (empty_plot(), Vec::new()),
                },
                // lifecycle の pose は session そのものの姿で、agent 1 体には対応しない。
                _ => (lifecycle_plot(session, tick, reduced_motion), Vec::new()),
            }
        } else {
            (inactive_plot(session), Vec::new())
        };
    if session.pending_decisions > 0 {
        let noun = if session.pending_decisions == 1 {
            "decision"
        } else {
            "decisions"
        };
        status = centered(
            PLOT_WIDTH,
            &Role::Warning
                .style()
                .bold()
                .paint(&format!("action · {} {noun}", session.pending_decisions)),
        );
    }
    Plot {
        rows: [label, status, ears, head, body, feet],
        rabbits,
    }
}

/// Explain daemon dispatch state when no Agent runtime is present to own a
/// rabbit. Observed runtimes instead keep their identity in [`agent_plot`].
fn dispatch_plot(
    session: &GardenSession,
    status: DispatchAgentStatus,
) -> [String; PLOT_CONTENT_ROWS - 1] {
    let feature = rabbit_style(&session.id.as_str()).bold();
    let (label, status_style, rabbit_style, rabbit) = match status {
        DispatchAgentStatus::Starting => (
            "",
            Style::new(),
            feature,
            ["", " /)/)", "( . .)", "c(\")(\")v"],
        ),
        DispatchAgentStatus::Idle | DispatchAgentStatus::Exited => (
            "",
            Style::new().dim(),
            feature.dim(),
            [" z", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        DispatchAgentStatus::Failed => (
            "failed",
            Role::Danger.style().bold(),
            Role::Danger.style(),
            ["", " /)/)", "( x.x)", "c(\")(\")/"],
        ),
        DispatchAgentStatus::Running => unreachable!("running uses per-runtime phase"),
    };
    let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
    [
        centered(PLOT_WIDTH, &status_style.paint(label)),
        ears,
        head,
        body,
        feet,
    ]
}

/// Whether one Garden plot currently needs a person's attention.
///
/// The result is deliberately a per-session bit, not a sum of symptoms. A
/// pending decision commonly explains a waiting Agent, so adding both would
/// inflate the Action Center count for one actual place the person needs to
/// visit.
fn needs_attention(session: &GardenSession) -> bool {
    session.pending_decisions > 0
        || session.lifecycle == SessionLifecycle::Failed
        || session.agent_status == Some(DispatchAgentStatus::Failed)
        || session
            .agents
            .iter()
            .any(|agent| matches!(agent.phase, AgentPhase::Waiting | AgentPhase::Interrupted))
}

fn inactive_plot(session: &GardenSession) -> [String; PLOT_CONTENT_ROWS - 1] {
    let status = match session.lifecycle {
        SessionLifecycle::Available => "project inactive",
        SessionLifecycle::Creating | SessionLifecycle::Initializing => "cached · creating",
        SessionLifecycle::Deleting => "cached · deleting",
        SessionLifecycle::Failed => "cached · failed",
    };
    [
        centered(PLOT_WIDTH, &Style::new().dim().paint(status)),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
    ]
}

/// session 名を庭の立札として描く。左右の線も含めて固定幅で切り詰める。
fn signpost(label: &str) -> String {
    let label = clip_to_width(label, PLOT_WIDTH.saturating_sub(4));
    let rails = Style::new().dim().paint("╴");
    let label = Style::new().bold().paint(&label);
    let sign = format!("{rails} {label} {rails}");
    centered(PLOT_WIDTH, &sign)
}

/// 庭の幅いっぱいに敷いた草地と土の 2 行。
///
/// タイルを順に並べて `content_width` 桁ちょうどで切る。どちらも ASCII なので
/// 1 文字 = 1 桁で、途中で切っても桁がずれない。
fn ground_rows(layout: GardenLayout, tick: u64, reduced_motion: bool) -> [String; GROUND_ROWS] {
    let phase = usize::try_from(ambient_phase(tick, reduced_motion)).unwrap_or_default();
    let grass = GRASS
        .iter()
        .cycle()
        .flat_map(|tile| tile.chars())
        .take(layout.content_width)
        .enumerate()
        .map(|(column, ch)| {
            if ch != 'v' {
                return ch;
            }
            match (phase + column) % 4 {
                0 => 'v',
                1 => '\\',
                2 => '|',
                _ => '/',
            }
        })
        .collect::<String>();
    let soil = SOIL
        .iter()
        .cycle()
        .flat_map(|tile| tile.chars())
        .take(layout.content_width)
        .collect::<String>();
    [grass, soil].map(|layer| {
        pad_to_width(
            &format!(
                "{}{}",
                " ".repeat(SIDE_PADDING),
                Style::new().dim().paint(&layer)
            ),
            layout.content_width + SIDE_PADDING * 2,
        )
    })
}

const fn ambient_phase(tick: u64, reduced_motion: bool) -> u64 {
    if reduced_motion {
        0
    } else {
        (tick / AMBIENT_PHASE_TICKS) % AMBIENT_PHASES
    }
}

/// pose を 1 つの絵として中央へ寄せる。
///
/// 行ごとに中央寄せすると、行の表示桁数が違うぶんだけ耳と顔が横へずれる（例えば
/// `Creating` の耳は頭より 2 桁右に出ていた）。うさぎが崩れないよう、pose 全体の
/// 最大幅から左端を 1 度だけ決め、4 行に同じ padding を与える。
fn sprite(
    rabbit: [&'static str; SPRITE_ROWS],
    style: Style,
    width: usize,
) -> [String; SPRITE_ROWS] {
    let sprite_width = rabbit
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0);
    let left = " ".repeat(width.saturating_sub(sprite_width) / 2);
    rabbit.map(|row| {
        if row.is_empty() {
            // 空行に色を塗らない（意味のない escape sequence を frame へ残さない）。
            " ".repeat(width)
        } else {
            pad_to_width(&format!("{left}{}", style.paint(row)), width)
        }
    })
}

fn lifecycle_plot(
    session: &GardenSession,
    tick: u64,
    reduced_motion: bool,
) -> [String; PLOT_CONTENT_ROWS - 1] {
    let feature = rabbit_style(&session.id.as_str()).bold();
    let phase = animation_phase(tick, reduced_motion, &session.id.as_str());
    let (status, status_style, rabbit_style, rabbit) = match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            let rabbit = if phase < 3 {
                ["", "", "  /)/)", "__(_ _)__"]
            } else {
                ["", "   /)/)", " _( . .)_", "__/   \\__"]
            };
            (String::new(), Role::Warning.style(), feature, rabbit)
        }
        SessionLifecycle::Deleting => {
            let rabbit_style = if reduced_motion || phase >= 4 {
                Style::new().dim()
            } else if phase >= 2 {
                Role::Feature.style().dim()
            } else {
                Role::Feature.style()
            };
            (
                String::new(),
                Style::new().dim(),
                rabbit_style,
                ["", " /)/)", "( . .)", "c(\")(\")"],
            )
        }
        SessionLifecycle::Failed => {
            let status = session.failure_summary.as_deref().map_or_else(
                || "failed".to_owned(),
                |summary| format!("failed · {summary}"),
            );
            (
                status,
                Role::Danger.style().bold(),
                Role::Danger.style(),
                ["", " /)/)", "( x.x)", "c(\")(\")/"],
            )
        }
        SessionLifecycle::Available => unreachable!("available sessions use agent projection"),
    };
    let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
    [
        centered(
            PLOT_WIDTH,
            &status_style.paint(&clip_to_width(&status, PLOT_WIDTH)),
        ),
        ears,
        head,
        body,
        feet,
    ]
}

/// Agent のいる区画の status 行 + sprite 4 行と、その中のうさぎの横位置。
fn agent_plot(
    session: &GardenSession,
    tick: u64,
    reduced_motion: bool,
) -> ([String; PLOT_CONTENT_ROWS - 1], Vec<PlacedRabbit>) {
    let agents = agent_status::ordered(&session.agents);
    debug_assert!(!agents.is_empty());

    let session_status =
        if session.pr_merged {
            Some(Role::Success.style().bold().paint("PR merged! *"))
        } else if session.lifecycle == SessionLifecycle::Failed {
            Some(Role::Danger.style().bold().paint(
                &session.failure_summary.as_deref().map_or_else(
                    || "failed".to_owned(),
                    |summary| format!("failed · {summary}"),
                ),
            ))
        } else if session.agent_status == Some(DispatchAgentStatus::Failed) {
            Some(Role::Danger.style().bold().paint("failed"))
        } else {
            None
        };

    if agents.len() == 1 {
        let agent = agents[0];
        let (status, status_style, rabbit_style, rabbit) =
            detailed_agent_appearance(session, agent, tick, reduced_motion);
        let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
        // 1 羽だけの区画はうさぎを大きく描くので、その 1 体が sprite 行の全幅を持つ。
        return (
            [
                centered(
                    PLOT_WIDTH,
                    session_status
                        .as_deref()
                        .unwrap_or(&status_style.paint(status)),
                ),
                ears,
                head,
                body,
                feet,
            ],
            vec![PlacedRabbit {
                runtime_id: agent.runtime_id,
                offset: 0,
                width: PLOT_WIDTH,
            }],
        );
    }

    let visible = &agents[..agents.len().min(MAX_VISIBLE_AGENTS)];
    let status = session_status.unwrap_or_else(|| agent_status::status_line(&agents, PLOT_WIDTH));
    let mut rows: [String; SPRITE_ROWS] = std::array::from_fn(|_| String::new());
    for agent in visible {
        let (_, _, style, rabbit) =
            detailed_agent_appearance(session, *agent, tick, reduced_motion);
        let compact = sprite(rabbit, style, COMPACT_RABBIT_WIDTH);
        for (row, part) in rows.iter_mut().zip(compact) {
            row.push_str(&part);
        }
    }
    let [ears, head, body, feet] = rows.map(|row| centered(PLOT_WIDTH, &row));
    // 各 compact sprite は必ず COMPACT_RABBIT_WIDTH 桁へ揃うので、`centered` が
    // 与える左端は羽数だけから決まる（同じ式で hitbox の offset を出せる）。
    let left = PLOT_WIDTH.saturating_sub(visible.len() * COMPACT_RABBIT_WIDTH) / 2;
    let placed = visible
        .iter()
        .enumerate()
        .map(|(index, agent)| PlacedRabbit {
            runtime_id: agent.runtime_id,
            offset: left + index * COMPACT_RABBIT_WIDTH,
            width: COMPACT_RABBIT_WIDTH,
        })
        .collect();
    (
        [centered(PLOT_WIDTH, &status), ears, head, body, feet],
        placed,
    )
}

fn detailed_agent_appearance(
    session: &GardenSession,
    agent: GardenAgent,
    tick: u64,
    reduced_motion: bool,
) -> (&'static str, Style, Style, [&'static str; 4]) {
    let stable_id = agent.runtime_id.as_str();
    match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            return (
                "starting",
                Role::Accent.style(),
                rabbit_style(&stable_id).bold(),
                ["", " /)/)", "( . .)", "c(\")(\")v"],
            );
        }
        SessionLifecycle::Deleting => {
            return (
                "closing",
                Style::new().dim(),
                rabbit_style(&stable_id).dim(),
                [" z", " /)/)", "( -.-)", "c(\")(\")"],
            );
        }
        SessionLifecycle::Failed => {
            return (
                "failed",
                Role::Danger.style().bold(),
                Role::Danger.style(),
                ["", " /)/)", "( x.x)", "c(\")(\")/"],
            );
        }
        SessionLifecycle::Available => {}
    }
    match session.agent_status {
        Some(DispatchAgentStatus::Starting) => (
            "starting",
            Role::Accent.style(),
            rabbit_style(&stable_id).bold(),
            ["", " /)/)", "( . .)", "c(\")(\")v"],
        ),
        Some(DispatchAgentStatus::Idle) => (
            "completed",
            Style::new().dim(),
            rabbit_style(&stable_id).dim(),
            [" z", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        Some(DispatchAgentStatus::Exited) => (
            "stopped",
            Style::new().dim(),
            rabbit_style(&stable_id).dim(),
            [" z", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        Some(DispatchAgentStatus::Failed) => (
            "failed",
            Role::Danger.style().bold(),
            Role::Danger.style(),
            ["", " /)/)", "( x.x)", "c(\")(\")/"],
        ),
        Some(DispatchAgentStatus::Running) | None => {
            agent_appearance(agent.phase, tick, reduced_motion, &stable_id)
        }
    }
}

fn celebration_plot(tick: u64, reduced_motion: bool) -> [String; PLOT_CONTENT_ROWS - 1] {
    let rabbit = if reduced_motion || tick.is_multiple_of(2) {
        ["  \\ /", "  /)/)", " \\(^.^)/", " c(\")(\")"]
    } else {
        [" *  . *", "  /)/)", " \\(^o^)/", " c(\")(\")"]
    };
    let [ears, head, body, feet] = sprite(rabbit, Role::Feature.style().bold(), PLOT_WIDTH);
    [
        centered(
            PLOT_WIDTH,
            &Role::Success.style().bold().paint("PR merged! *"),
        ),
        ears,
        head,
        body,
        feet,
    ]
}

fn empty_plot() -> [String; PLOT_CONTENT_ROWS - 1] {
    [
        centered(PLOT_WIDTH, &Style::new().dim().paint("no agents")),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
        " ".repeat(PLOT_WIDTH),
    ]
}

fn agent_appearance(
    agent_phase: AgentPhase,
    tick: u64,
    reduced_motion: bool,
    stable_id: &str,
) -> (&'static str, Style, Style, [&'static str; 4]) {
    let feature = rabbit_style(stable_id).bold();
    let phase = animation_phase(tick, reduced_motion, stable_id);
    match agent_phase {
        AgentPhase::Running => {
            let rabbit = if reduced_motion {
                ["", " /)/)", "( o.o)", "c(\")(\")"]
            } else {
                let (action, progress) = running_action(tick, stable_id);
                running_pose(action, progress)
            };
            ("running", Role::Success.style().bold(), feature, rabbit)
        }
        AgentPhase::Waiting => {
            let ears = if phase == 5 { " /)(/" } else { " /)/)" };
            (
                "waiting",
                Role::Warning.style().bold(),
                feature,
                ["", ears, "( o.o)?", "c(\")(\")"],
            )
        }
        AgentPhase::Interrupted => (
            "interrupted",
            Role::Warning.style(),
            feature,
            ["", " /)/)", "( -.-)!", "c(\")(\")"],
        ),
        AgentPhase::Sleeping => (
            "sleeping",
            Style::new().dim(),
            feature,
            [" zZ", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        AgentPhase::Ended | AgentPhase::Exited => (
            "done",
            Style::new().dim(),
            feature,
            [" z", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        AgentPhase::Absent | AgentPhase::Ready => {
            let face = if phase == 4 { "( -.-)" } else { "( . .)" };
            (
                "available",
                Style::new().dim(),
                feature,
                ["", " /)/)", face, "c(\")(\")v"],
            )
        }
    }
}

/// One deterministic pseudo-random action and its local tick.
///
/// Rendering must stay pure, so this does not use process randomness. The stable runtime ID
/// selects both a starting offset and a freshly shuffled order for each 25-tick round. Every
/// round contains all five differently-sized actions exactly once, while different rabbits
/// normally get different sequences.
fn running_action(tick: u64, stable_id: &str) -> (RunningAction, u64) {
    let seed = stable_hash(stable_id);
    let timeline = (tick + seed.rotate_right(17)) % RUNNING_ANIMATION_CYCLE_TICKS;
    let round = timeline / RUNNING_ACTION_CYCLE_TICKS;
    let order = shuffled_running_actions(seed ^ round.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut local_tick = timeline % RUNNING_ACTION_CYCLE_TICKS;
    let mut selected = (order[0], 0);
    for action in order {
        if local_tick < action.duration() {
            selected = (action, local_tick);
            break;
        }
        local_tick -= action.duration();
    }
    selected
}

fn shuffled_running_actions(mut state: u64) -> [RunningAction; 5] {
    let mut actions = RunningAction::ALL;
    for index in (1..actions.len()).rev() {
        // xorshift64: small, deterministic, and sufficient for visual variety.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let choices = u64::try_from(index + 1).expect("five actions fit u64");
        let swap_index = usize::try_from(state % choices).expect("shuffle index fits usize");
        actions.swap(index, swap_index);
    }
    actions
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn rabbit_style(stable_id: &str) -> Style {
    garden_rabbit_style(stable_hash(stable_id))
}

fn running_pose(action: RunningAction, progress: u64) -> [&'static str; SPRITE_ROWS] {
    match action {
        RunningAction::Hop => {
            const POSES: [[&str; SPRITE_ROWS]; 3] = [
                ["", " /)/)", "( o.o)", " / > <"],
                [" /)/)", "( o.o)", " / > <", ""],
                ["", "  /)/)", "_( o.o)_", "  > ^ <"],
            ];
            let index = usize::from(u8::try_from(progress % 3).unwrap_or_default());
            POSES[index]
        }
        RunningAction::Bound => match progress % 4 {
            0 | 3 => ["", " /)/) __", "( o.o)/", "  /  \\"],
            _ => [" /)/)___", "( o.o)  ", " /   > ", ""],
        },
        RunningAction::Sniff => match progress {
            1 | 3 => ["", " /)/)", "( o.o)>", "c(\")(\")"],
            _ => ["", " /)/)", "( o.o)", "c(\")(\")"],
        },
        RunningAction::Dig => match progress % 3 {
            0 => ["", "  /)/)", "_( o.o)_", "  / >#"],
            1 => ["", "  /)/)", "_( o.o)_", " #< \\"],
            _ => ["", "  /)/)", "_( o.o)_", "  # #"],
        },
        RunningAction::Look => match progress {
            2 | 3 => ["", " /)/)", "(o.o )", "c(\")(\")"],
            4 | 5 => ["", " (\\(\\", "( o.o)", "c(\")(\")"],
            _ => ["", " /)/)", "( o.o)", "c(\")(\")"],
        },
    }
}

fn animation_phase(tick: u64, reduced_motion: bool, stable_id: &str) -> u64 {
    if reduced_motion {
        0
    } else {
        (tick + u64::from_str_radix(&stable_id[..2], 16).unwrap_or(0)) % 6
    }
}

fn centered(width: usize, value: &str) -> String {
    let value = clip_to_width(value, width);
    let padding = width.saturating_sub(display_width(&value)) / 2;
    pad_to_width(&format!("{}{value}", " ".repeat(padding)), width)
}

#[cfg(test)]
mod tests {
    use super::{GRASS, GardenAgent, GardenSession, MIN_HEIGHT, MIN_WIDTH, PLOT_WIDTH, SOIL};
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::agent::AgentStatus as DispatchAgentStatus;
    use usagi_core::domain::id::{AgentRuntimeId, SessionId};
    use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

    /// animation offset が 0 になる id（先頭 2 桁が `00`）。tick をそのまま phase として扱える。
    const STEADY_ID: &str = "00000000-0000-4000-8000-000000000001";

    fn render(
        height: usize,
        width: usize,
        workspace_name: &str,
        sessions: &[GardenSession],
        tick: u64,
        reduced_motion: bool,
    ) -> Option<super::GardenFrame> {
        super::render(
            height,
            width,
            workspace_name,
            sessions,
            tick,
            reduced_motion,
        )
    }

    /// 区画そのものの rectangle だけ（うさぎ 1 羽ずつの rectangle を除く）。
    fn plots(frame: &super::GardenFrame) -> Vec<super::GardenHitbox> {
        frame
            .hitboxes
            .iter()
            .filter(|hitbox| hitbox.agent.is_none())
            .copied()
            .collect()
    }

    /// うさぎ 1 羽ずつの rectangle だけ。
    fn rabbits(frame: &super::GardenFrame) -> Vec<super::GardenHitbox> {
        frame
            .hitboxes
            .iter()
            .filter(|hitbox| hitbox.agent.is_some())
            .copied()
            .collect()
    }

    fn plain_row(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn plain(frame: &super::GardenFrame) -> Vec<String> {
        frame.rows.iter().map(|row| plain_row(row)).collect()
    }

    #[test]
    fn public_render_routes_to_the_layout_selected_for_the_terminal() {
        let session = session(
            STEADY_ID,
            "world",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        let frame = super::render(24, 120, "atlas", &[session], 0, false).expect("garden fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("click a usagi"));
        assert!(text.contains("world"));
        assert!(!text.contains("scroll"));
    }

    fn grass_row(rows: &[String]) -> &str {
        rows.iter()
            .find(|row| row.trim_start().starts_with("--"))
            .map(String::as_str)
            .expect("the Garden draws a grass layer")
    }

    fn assert_rabbit_axis(name: &str, pose: &[&str]) {
        let (ears_row, ears, ears_width) = pose
            .iter()
            .enumerate()
            .find_map(|(row, line)| {
                ["/)/)", "/)(/", "(\\(\\"]
                    .into_iter()
                    .find_map(|ears| line.find(ears).map(|column| (row, column, ears.len())))
            })
            .expect("rabbit illustration has ears");
        let face = pose
            .iter()
            .skip(ears_row + 1)
            .find(|line| {
                ["o.o", ". .", "-.-", "x.x", "^.^", "^o^", "_ _"]
                    .into_iter()
                    .any(|marker| line.contains(marker))
            })
            .expect("rabbit illustration has a face below its ears");
        let face_left = face.find('(').expect("rabbit face has a left edge");
        let face_right = face.rfind(')').expect("rabbit face has a right edge");
        // Double the centres to avoid losing half-cell precision.
        let ears_axis = ears * 2 + ears_width.saturating_sub(1);
        let face_axis = face_left + face_right;
        assert!(
            ears_axis.abs_diff(face_axis) <= 1,
            "{name} ears axis {ears_axis}/2 drifted from face axis {face_axis}/2: {pose:?}"
        );
    }

    #[test]
    fn non_running_dispatch_status_restyles_each_observed_agent_rabbit() {
        for (status, label) in [
            (DispatchAgentStatus::Starting, "starting"),
            (DispatchAgentStatus::Idle, "completed"),
            (DispatchAgentStatus::Exited, "stopped"),
            (DispatchAgentStatus::Failed, "failed"),
        ] {
            let mut session = session(
                STEADY_ID,
                "status",
                SessionLifecycle::Available,
                AgentPhase::Running,
            );
            session.agent_status = Some(status);
            assert_eq!(
                super::plot(&session, 0, false),
                super::plot(&session, 17, false),
                "{status:?} must use a static session pose"
            );
            let frame = render(24, 100, "x", &[session], 17, false).expect("garden fits");
            assert_eq!(rabbits(&frame).len(), 1, "{status:?} keeps its Agent usagi");
            let rows = plain(&frame);
            let text = rows.join("\n");
            assert!(text.contains("1 session"), "{status:?}: {text}");
            assert!(text.contains("1 usagi"), "{status:?}: {text}");
            assert!(text.contains(label), "{status:?}: {text}");
            let attention = if status == DispatchAgentStatus::Failed {
                "1 needs attention"
            } else {
                "all clear"
            };
            assert!(text.contains(attention), "{status:?}: {text}");
        }

        let mut waiting = session(
            STEADY_ID,
            "status",
            SessionLifecycle::Available,
            AgentPhase::Waiting,
        );
        waiting.agent_status = Some(DispatchAgentStatus::Running);
        let frame = render(24, 100, "x", &[waiting], 0, true).expect("garden fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("( o.o)?"));
        assert!(text.contains("waiting"));
        assert!(text.contains("1 needs attention"));
    }

    #[test]
    fn action_center_counts_sessions_once_and_prioritizes_pending_decisions() {
        let mut waiting = session(
            STEADY_ID,
            "needs-human",
            SessionLifecycle::Available,
            AgentPhase::Waiting,
        );
        waiting.pending_decisions = 1;
        let frame = render(24, 100, "x", &[waiting.clone()], 0, true).expect("garden fits");
        assert!(plain(&frame).join("\n").contains("action · 1 decision"));

        waiting.pending_decisions = 2;
        let clear = session(
            "10000000-0000-4000-8000-000000000001",
            "clear",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );

        let frame = render(24, 100, "x", &[waiting, clear], 0, true).expect("garden fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("2 sessions"));
        assert!(text.contains("1 needs attention"));
        assert!(text.contains("2 usagi"));
        assert!(text.contains("action · 2 decisions"));

        let clear = session(
            STEADY_ID,
            "clear",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        let frame = render(24, 100, "x", &[clear], 0, true).expect("garden fits");
        assert!(plain(&frame).join("\n").contains("all clear"));
    }

    fn only(lifecycle: SessionLifecycle, phase: AgentPhase, tick: u64) -> Vec<String> {
        only_with_motion(lifecycle, phase, tick, false)
    }

    fn only_with_motion(
        lifecycle: SessionLifecycle,
        phase: AgentPhase,
        tick: u64,
        reduced_motion: bool,
    ) -> Vec<String> {
        let frame = render(
            24,
            100,
            "x",
            &[session(STEADY_ID, "one", lifecycle, phase)],
            tick,
            reduced_motion,
        )
        .expect("garden fits");
        plain(&frame)
    }

    fn session(
        id: &str,
        label: &str,
        lifecycle: SessionLifecycle,
        phase: AgentPhase,
    ) -> GardenSession {
        GardenSession {
            id: SessionId::parse(id).expect("fixture id"),
            label: label.to_owned(),
            lifecycle,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pending_decisions: 0,
            pr_merged: false,
            agents: vec![GardenAgent {
                runtime_id: AgentRuntimeId::parse(id).expect("fixture runtime id"),
                phase,
            }],
            agent_status: None,
        }
    }

    fn agent(id: &str, phase: AgentPhase) -> GardenAgent {
        GardenAgent {
            runtime_id: AgentRuntimeId::parse(id).expect("fixture runtime id"),
            phase,
        }
    }

    fn fixtures() -> Vec<GardenSession> {
        vec![
            session(
                "00000000-0000-4000-8000-000000000001",
                "session-auth",
                SessionLifecycle::Available,
                AgentPhase::Running,
            ),
            session(
                "01000000-0000-4000-8000-000000000002",
                "issue-647",
                SessionLifecycle::Available,
                AgentPhase::Waiting,
            ),
            session(
                "02000000-0000-4000-8000-000000000003",
                "日本語-session",
                SessionLifecycle::Available,
                AgentPhase::Absent,
            ),
            session(
                "03000000-0000-4000-8000-000000000004",
                "failed-build",
                SessionLifecycle::Failed,
                AgentPhase::Absent,
            ),
        ]
    }

    #[test]
    fn sample_frame_is_width_safe_and_exposes_matching_hitboxes() {
        let frame = render(24, 100, "my-project", &fixtures(), 0, false).expect("garden fits");
        assert_eq!(frame.rows.len(), 24);
        assert!(frame.rows.iter().all(|row| display_width(row) == 100));
        assert_eq!(plots(&frame).len(), 4);
        for hitbox in &frame.hitboxes {
            assert!(hitbox.contains(hitbox.column, hitbox.row));
            assert!(!hitbox.contains(hitbox.column + hitbox.width, hitbox.row));
        }
        let text = plain(&frame).join("\n");
        assert!(text.contains("session-auth"));
        assert!(text.contains("日本語-session"));
        assert!(text.contains("running"));
        assert!(text.contains("failed"));
        assert!(text.contains("╴ session-auth ╴"));
        assert!(text.contains("any key · wake"));
        assert!(text.contains('*') || text.contains('.'));
    }

    #[test]
    fn garden_uses_its_full_width_without_a_duplicate_agent_panel() {
        let sessions = vec![
            session(
                STEADY_ID,
                "completed-work",
                SessionLifecycle::Available,
                AgentPhase::Ended,
            ),
            session(
                "01000000-0000-4000-8000-000000000001",
                "needs-input",
                SessionLifecycle::Available,
                AgentPhase::Waiting,
            ),
        ];
        let frame = render(24, 100, "my-project", &sessions, 0, true).expect("garden fits");
        let text = plain(&frame).join("\n");
        assert!(!text.contains("Agents"), "{text}");
        assert!(!text.contains("scroll"), "{text}");
        assert!(text.contains("completed-work"), "{text}");
        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("waiting"), "{text}");
        assert_eq!(
            frame
                .hitboxes
                .iter()
                .filter(|hitbox| hitbox.agent.is_some())
                .count(),
            2
        );
    }

    #[test]
    fn empty_agent_session_summaries_cover_every_safe_garden_state() {
        let message = |session: &GardenSession| super::session_summary(session).1;
        let mut fixture = session(
            STEADY_ID,
            "state",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        fixture.agents.clear();

        fixture.pending_decisions = 1;
        assert_eq!(message(&fixture), "1 decision needs your input.");
        fixture.pending_decisions = 2;
        assert_eq!(message(&fixture), "2 decisions need your input.");
        fixture.pending_decisions = 0;
        fixture.pr_merged = true;
        assert_eq!(message(&fixture), "PR merged.");
        fixture.pr_merged = false;
        for (lifecycle, expected) in [
            (SessionLifecycle::Creating, "Session is starting."),
            (SessionLifecycle::Initializing, "Session is starting."),
            (SessionLifecycle::Deleting, "Session is closing."),
            (SessionLifecycle::Failed, "Session failed."),
        ] {
            fixture.lifecycle = lifecycle;
            assert_eq!(message(&fixture), expected);
        }
        fixture.lifecycle = SessionLifecycle::Available;
        fixture.agents_observed = false;
        assert_eq!(message(&fixture), "Status is unavailable.");
        fixture.agents_observed = true;

        for (status, expected) in [
            (DispatchAgentStatus::Starting, "Agent is starting."),
            (DispatchAgentStatus::Idle, "Agent completed."),
            (DispatchAgentStatus::Exited, "Agent stopped."),
            (DispatchAgentStatus::Failed, "Agent failed."),
        ] {
            fixture.agent_status = Some(status);
            assert_eq!(message(&fixture), expected);
        }
        fixture.agent_status = Some(DispatchAgentStatus::Running);
        assert_eq!(message(&fixture), "Agent is working.");
        fixture.agent_status = None;
        assert_eq!(message(&fixture), "No agent activity.");
    }

    #[test]
    fn empty_agent_sessions_keep_state_specific_illustrations() {
        let mut fixture = session(
            STEADY_ID,
            "state",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        fixture.agents.clear();

        fixture.pr_merged = true;
        let calm_celebration = super::plot(&fixture, 0, true).rows.join("\n");
        let lively_celebration = super::plot(&fixture, 1, false).rows.join("\n");
        assert!(calm_celebration.contains("PR merged"));
        assert!(calm_celebration.contains("^.^"));
        assert!(lively_celebration.contains("^o^"));

        fixture.pr_merged = false;
        for (status, expected) in [
            (DispatchAgentStatus::Starting, r#"c(")(")v"#),
            (DispatchAgentStatus::Idle, "( -.-)"),
            (DispatchAgentStatus::Exited, "( -.-)"),
            (DispatchAgentStatus::Failed, "( x.x)"),
        ] {
            fixture.agent_status = Some(status);
            let text = super::plot(&fixture, 0, true).rows.join("\n");
            assert!(text.contains(expected), "{status:?}: {text}");
        }
        fixture.agent_status = Some(DispatchAgentStatus::Running);
        assert!(
            super::plot(&fixture, 0, true)
                .rows
                .join("\n")
                .contains("no agents")
        );

        fixture.agent_status = None;
        fixture.lifecycle = SessionLifecycle::Failed;
        let generic_failure = super::plot(&fixture, 0, true).rows.join("\n");
        assert!(generic_failure.contains("failed"));
        fixture.failure_summary = Some("safe reason".to_owned());
        let explained_failure = super::plot(&fixture, 0, true).rows.join("\n");
        assert!(explained_failure.contains("failed · safe reason"));
    }

    #[test]
    fn dense_garden_draws_every_session_and_agent_without_overflow_copy() {
        let sessions = (0..20)
            .map(|index| {
                session(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    &format!("session-{index}"),
                    SessionLifecycle::Available,
                    AgentPhase::Ready,
                )
            })
            .collect::<Vec<_>>();
        let frame = render(14, 64, "all sessions", &sessions, 0, true).expect("fits");
        assert_eq!(frame.hitboxes.len(), sessions.len());
        assert_eq!(rabbits(&frame).len(), sessions.len());
        let text = plain(&frame).join("\n");
        assert!(!text.contains("more"), "{text}");
        assert!(!text.contains("scroll"), "{text}");
    }

    #[test]
    fn dense_layout_uses_lines_then_glyphs_before_losing_an_agent() {
        let sessions = (0..80)
            .map(|index| {
                session(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    &format!("session-{index}"),
                    SessionLifecycle::Available,
                    AgentPhase::Running,
                )
            })
            .collect::<Vec<_>>();

        let mut cards = sessions[..3].to_vec();
        cards[2].agents.clear();
        let cards = render(14, 64, "many", &cards, 0, true).expect("cards fit");
        assert_eq!(rabbits(&cards).len(), 2);
        assert!(plain(&cards).join("\n").contains("No agent activity."));

        let mut line_sessions = sessions[..22].to_vec();
        line_sessions[21].agents.clear();
        let lines = render(14, 64, "many", &line_sessions, 0, true).expect("lines fit");
        let line_text = plain(&lines).join("\n");
        assert_eq!(rabbits(&lines).len(), 21);
        assert!(lines.hitboxes.iter().all(|hitbox| hitbox.height == 1));
        assert!(lines.hitboxes.iter().any(|hitbox| hitbox.agent.is_none()));
        assert_eq!(line_text.matches('兎').count(), 21, "{line_text}");
        assert!(line_text.contains("sessi"), "{line_text}");
        assert!(!line_text.contains("(o.o)"), "{line_text}");

        let mut glyph_sessions = sessions.clone();
        glyph_sessions[79].agents.clear();
        let glyphs = render(14, 64, "many", &glyph_sessions, 0, true).expect("glyphs fit");
        let glyph_text = plain(&glyphs).join("\n");
        assert_eq!(rabbits(&glyphs).len(), sessions.len() - 1);
        assert!(glyphs.hitboxes.iter().all(|hitbox| hitbox.height == 1));
        assert!(glyphs.hitboxes.iter().any(|hitbox| hitbox.agent.is_none()));
        assert!(glyph_text.contains('兎'), "{glyph_text}");
        assert!(glyph_text.contains('·'), "{glyph_text}");

        let layout = super::garden_layout(14, 64).expect("minimum layout");
        assert_eq!(super::dense_mode(layout, 20), super::DenseMode::Card);
        assert_eq!(super::dense_mode(layout, 21), super::DenseMode::Line);
        assert_eq!(super::dense_mode(layout, 78), super::DenseMode::Glyph);
        assert_eq!(
            super::dense_mode(layout, usize::MAX),
            super::DenseMode::Glyph
        );
    }

    #[test]
    fn dense_layout_keeps_agent_rabbits_ahead_of_idle_session_cards() {
        let mut sessions = (0..331)
            .map(|index| {
                let mut session = session(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    &format!("idle-{index}"),
                    SessionLifecycle::Available,
                    AgentPhase::Ready,
                );
                session.agents.clear();
                session
            })
            .collect::<Vec<_>>();
        sessions.push(session(
            "ffff0000-0000-4000-8000-000000000001",
            "active-agent",
            SessionLifecycle::Available,
            AgentPhase::Running,
        ));

        let frame = render(14, 64, "overflow", &sessions, 0, true).expect("fallback fits");
        assert_eq!(frame.hitboxes.len(), 1);
        assert_eq!(rabbits(&frame).len(), 1);
        let text = plain(&frame).join("\n");
        assert!(text.contains("active-agent"), "{text}");
        assert!(text.contains("332 sessions"), "{text}");
    }

    #[test]
    fn dense_rabbits_keep_identity_while_status_changes() {
        let mut fixture = session(
            STEADY_ID,
            "state",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        let mut rabbit = fixture.agents[0];

        for (lifecycle, expected) in [
            (SessionLifecycle::Creating, "start"),
            (SessionLifecycle::Initializing, "start"),
            (SessionLifecycle::Deleting, "close"),
            (SessionLifecycle::Failed, "fail"),
        ] {
            fixture.lifecycle = lifecycle;
            assert!(super::dense_rabbit(&fixture, rabbit).contains(expected));
        }

        fixture.lifecycle = SessionLifecycle::Available;
        for (status, expected) in [
            (DispatchAgentStatus::Starting, "start"),
            (DispatchAgentStatus::Idle, "done"),
            (DispatchAgentStatus::Exited, "stop"),
            (DispatchAgentStatus::Failed, "fail"),
        ] {
            fixture.agent_status = Some(status);
            assert!(super::dense_rabbit(&fixture, rabbit).contains(expected));
        }

        fixture.agent_status = None;
        for (phase, expected) in [
            (AgentPhase::Waiting, "wait"),
            (AgentPhase::Running, "run"),
            (AgentPhase::Ready, "ready"),
            (AgentPhase::Interrupted, "pause"),
            (AgentPhase::Sleeping, "sleep"),
            (AgentPhase::Absent, "idle"),
            (AgentPhase::Ended, "done"),
            (AgentPhase::Exited, "done"),
        ] {
            rabbit.phase = phase;
            assert!(super::dense_rabbit(&fixture, rabbit).contains(expected));
        }
    }

    #[test]
    fn narrow_or_short_terminals_do_not_replace_home() {
        assert!(render(MIN_HEIGHT - 1, MIN_WIDTH, "x", &[], 0, false).is_none());
        assert!(render(MIN_HEIGHT, MIN_WIDTH - 1, "x", &[], 0, false).is_none());
        let minimum = render(MIN_HEIGHT, MIN_WIDTH, "x", &[], 0, false)
            .expect("the documented minimum leaves one project-bar row above the Garden");
        assert_eq!(minimum.rows.len(), MIN_HEIGHT);
        assert!(
            minimum
                .rows
                .iter()
                .all(|row| display_width(row) == MIN_WIDTH)
        );
    }

    #[test]
    fn an_empty_garden_has_a_calm_explicit_message() {
        let frame = render(24, 100, "my-project", &[], 0, false).expect("garden fits");
        assert!(frame.rows.join("\n").contains("No sessions in the garden"));
        assert!(frame.hitboxes.is_empty());
    }

    #[test]
    fn one_busy_session_keeps_every_agent_as_a_usagi() {
        let mut busy = session(
            STEADY_ID,
            "busy",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        busy.agents = (0..16)
            .map(|index| {
                agent(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    if index == 0 {
                        AgentPhase::Waiting
                    } else {
                        AgentPhase::Running
                    },
                )
            })
            .collect();
        let frame = render(14, 64, "x", &[busy], 3, false).expect("garden fits");
        assert_eq!(rabbits(&frame).len(), 16);
        assert_eq!(
            rabbits(&frame)
                .into_iter()
                .map(|rabbit| rabbit.agent.expect("rabbit owns an Agent"))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            16
        );
        assert!(!plain(&frame).join("\n").contains("scroll"));
    }

    #[test]
    fn running_motion_changes_pose_while_reduced_motion_stays_still() {
        let sessions = fixtures();
        let moving_a = render(24, 100, "x", &sessions, 0, false).expect("fits");
        let moving_b = render(24, 100, "x", &sessions, 1, false).expect("fits");
        assert_ne!(moving_a.rows, moving_b.rows);

        let still_a = render(24, 100, "x", &sessions, 0, true).expect("fits");
        let still_b = render(24, 100, "x", &sessions, 5, true).expect("fits");
        assert_eq!(still_a.rows, still_b.rows);
    }

    #[test]
    fn runtime_clock_is_brisk_but_does_not_rebuild_at_the_input_pump_rate() {
        assert_eq!(super::runtime_tick(0), 0);
        assert_eq!(super::runtime_tick(7), 0);
        assert_eq!(super::runtime_tick(8), 1);
        assert_eq!(super::runtime_tick(15), 1);
        assert_eq!(super::runtime_tick(16), 2);
    }

    #[test]
    fn multiple_agents_are_sorted_by_attention_then_runtime_identity() {
        let waiting = agent("f0000000-0000-4000-8000-000000000001", AgentPhase::Waiting);
        let early_running = agent("10000000-0000-4000-8000-000000000001", AgentPhase::Running);
        let late_running = agent("20000000-0000-4000-8000-000000000001", AgentPhase::Running);
        let ready = agent("30000000-0000-4000-8000-000000000001", AgentPhase::Ready);
        let ended = agent("40000000-0000-4000-8000-000000000001", AgentPhase::Ended);
        let shuffled = vec![ended, late_running, ready, waiting, early_running];
        let ordered = super::agent_status::ordered(&shuffled);
        assert_eq!(
            ordered
                .iter()
                .map(|agent| agent.runtime_id)
                .collect::<Vec<_>>(),
            vec![
                waiting.runtime_id,
                early_running.runtime_id,
                late_running.runtime_id,
                ready.runtime_id,
                ended.runtime_id,
            ]
        );

        let mut reversed = shuffled.clone();
        reversed.reverse();
        let make_session = |agents| GardenSession {
            id: SessionId::parse(STEADY_ID).expect("fixture id"),
            label: "many".to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pending_decisions: 0,
            pr_merged: false,
            agents,
            agent_status: None,
        };
        let first = render(24, 100, "x", &[make_session(shuffled)], 2, false).expect("fits");
        let second = render(24, 100, "x", &[make_session(reversed)], 2, false).expect("fits");
        assert_eq!(first, second);
        let text = plain(&first).join("\n");
        assert!(text.contains("wait"), "{text}");
        assert!(text.contains("run"), "{text}");
        assert!(text.contains("ready"), "{text}");
        assert!(text.contains("done"), "{text}");
        assert_eq!(rabbits(&first).len(), 5);
        assert_eq!(text.matches("o.o").count(), 3);
    }

    /// うさぎ 1 羽は 1 agent なので、羽ごとに hitbox を返す。dense card へ
    /// 切り替わっても注意順と stable identity を保つ。
    #[test]
    fn every_visible_rabbit_owns_the_cells_it_is_drawn_in() {
        let waiting = agent("f0000000-0000-4000-8000-000000000001", AgentPhase::Waiting);
        let running = agent("10000000-0000-4000-8000-000000000001", AgentPhase::Running);
        let ready = agent("30000000-0000-4000-8000-000000000001", AgentPhase::Ready);
        let folded = agent("40000000-0000-4000-8000-000000000001", AgentPhase::Ended);
        let hidden = agent("50000000-0000-4000-8000-000000000001", AgentPhase::Exited);
        let sessions = vec![GardenSession {
            id: SessionId::parse(STEADY_ID).expect("fixture id"),
            label: "many".to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pending_decisions: 0,
            pr_merged: false,
            agents: vec![folded, running, waiting, hidden, ready],
            agent_status: None,
        }];
        let frame = render(24, 100, "x", &sessions, 0, true).expect("garden fits");
        let rabbits = rabbits(&frame);
        assert_eq!(rabbits.len(), 5);
        assert_eq!(
            rabbits
                .iter()
                .map(|rabbit| rabbit.agent.expect("a rabbit names its runtime"))
                .collect::<Vec<_>>(),
            vec![
                waiting.runtime_id,
                running.runtime_id,
                ready.runtime_id,
                folded.runtime_id,
                hidden.runtime_id,
            ],
        );
        let rows = plain(&frame);
        for rabbit in &rabbits {
            assert_eq!(rabbit.session_id, sessions[0].id);
            assert_eq!(rabbit.height, super::DENSE_CARD_HEIGHT);
            // box の中に、そのうさぎの絵がある。
            let drawn = (rabbit.row..rabbit.row + rabbit.height).any(|row| {
                rows[row]
                    .chars()
                    .skip(rabbit.column)
                    .take(rabbit.width)
                    .any(|cell| cell != ' ')
            });
            assert!(drawn, "no usagi is drawn in {rabbit:?}");
        }
        // 羽は横に並び、重ならない。
        for pair in rabbits.windows(2) {
            assert_eq!(pair[0].row, pair[1].row);
            assert_eq!(pair[0].column + pair[0].width, pair[1].column);
        }
    }

    /// 1 羽だけの区画はうさぎを大きく描くので、その 1 体が sprite 行の全幅を持つ。
    #[test]
    fn a_lone_rabbit_owns_the_whole_width_of_its_plot() {
        let sessions = vec![session(
            STEADY_ID,
            "one",
            SessionLifecycle::Available,
            AgentPhase::Running,
        )];
        let frame = render(24, 100, "x", &sessions, 0, true).expect("garden fits");
        let rabbits = rabbits(&frame);
        let plot = plots(&frame)[0];
        assert_eq!(rabbits.len(), 1);
        assert_eq!(rabbits[0].column, plot.column);
        assert_eq!(rabbits[0].width, plot.width);
        assert_eq!(rabbits[0].height, super::SPRITE_ROWS);
        assert_eq!(rabbits[0].row, plot.row + super::SPRITE_TOP_ROW);
    }

    /// lifecycle や celebration が session-wide でも、観測済み Agent はその
    /// runtime identity を持つうさぎとして残る。
    #[test]
    fn lifecycle_and_celebration_keep_their_agent_usagi() {
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Deleting,
            SessionLifecycle::Failed,
        ] {
            let sessions = vec![session(STEADY_ID, "one", lifecycle, AgentPhase::Running)];
            let frame = render(24, 100, "x", &sessions, 0, true).expect("garden fits");
            assert_eq!(rabbits(&frame).len(), 1, "{lifecycle:?}");
            assert_eq!(plots(&frame).len(), 1);
        }
        let mut celebrating = session(
            STEADY_ID,
            "one",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        celebrating.pr_merged = true;
        let frame = render(24, 100, "x", &[celebrating], 0, true).expect("garden fits");
        assert_eq!(rabbits(&frame).len(), 1);
        assert!(plain(&frame).join("\n").contains("PR merged"));
    }

    #[test]
    fn an_available_session_without_an_agent_draws_an_empty_plot() {
        let frame = render(
            24,
            100,
            "x",
            &[GardenSession {
                id: SessionId::parse(STEADY_ID).expect("fixture id"),
                label: "empty".to_owned(),
                lifecycle: SessionLifecycle::Available,
                selected: false,
                failure_summary: None,
                agents_observed: true,
                pending_decisions: 0,
                pr_merged: false,
                agents: Vec::new(),
                agent_status: None,
            }],
            0,
            false,
        )
        .expect("fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("no agents"));
        assert!(!text.contains("/)/)"));
    }

    #[test]
    fn an_inactive_projects_cached_session_does_not_claim_it_has_no_agents() {
        let frame = render(
            24,
            100,
            "2 open projects",
            &[GardenSession {
                id: SessionId::parse(STEADY_ID).expect("fixture id"),
                label: "other / review".to_owned(),
                lifecycle: SessionLifecycle::Available,
                selected: false,
                failure_summary: None,
                agents_observed: false,
                pending_decisions: 0,
                pr_merged: false,
                agents: Vec::new(),
                agent_status: None,
            }],
            0,
            false,
        )
        .expect("fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("project inactive"));
        assert!(!text.contains("no agents"));
    }

    #[test]
    fn inactive_projects_cached_lifecycles_are_static_and_explicitly_cached() {
        for (lifecycle, status) in [
            (SessionLifecycle::Creating, "cached · creating"),
            (SessionLifecycle::Initializing, "cached · creating"),
            (SessionLifecycle::Deleting, "cached · deleting"),
            (SessionLifecycle::Failed, "cached · failed"),
        ] {
            let cached = GardenSession {
                id: SessionId::parse(STEADY_ID).expect("fixture id"),
                label: "other / review".to_owned(),
                lifecycle,
                selected: false,
                failure_summary: Some("old snapshot".to_owned()),
                agents_observed: false,
                pending_decisions: 0,
                pr_merged: false,
                agents: Vec::new(),
                agent_status: None,
            };
            let first = render(
                24,
                100,
                "2 open projects",
                std::slice::from_ref(&cached),
                0,
                false,
            )
            .expect("fits");
            let later = render(
                24,
                100,
                "2 open projects",
                std::slice::from_ref(&cached),
                5,
                false,
            )
            .expect("fits");
            assert_eq!(
                super::plot(&cached, 0, false),
                super::plot(&cached, 5, false),
                "{lifecycle:?} cached pose must not animate"
            );
            assert_ne!(
                first.rows, later.rows,
                "the calm Garden background should remain alive"
            );
            let text = plain(&first).join("\n");
            assert!(text.contains(status), "{text}");
            assert!(!text.contains("growing"), "{text}");
            assert!(!text.contains("heading home"), "{text}");
            let canonical = super::canonical_tick(24, 100, std::slice::from_ref(&cached), 5, false)
                .expect("fits");
            assert_eq!(
                later.rows,
                render(
                    24,
                    100,
                    "2 open projects",
                    std::slice::from_ref(&cached),
                    canonical,
                    false,
                )
                .expect("fits")
                .rows
            );
        }
    }

    #[test]
    #[should_panic(expected = "available sessions use agent projection")]
    fn lifecycle_plot_rejects_an_available_session() {
        let session = session(
            STEADY_ID,
            "available",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        let _ = super::lifecycle_plot(&session, 0, false);
    }

    #[test]
    #[should_panic(expected = "running uses per-runtime phase")]
    fn dispatch_plot_rejects_a_running_status() {
        let session = session(
            STEADY_ID,
            "running",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        let _ = super::dispatch_plot(&session, DispatchAgentStatus::Running);
    }

    #[test]
    fn ordinary_actions_avoid_long_narrative_captions() {
        for (lifecycle, phase) in [
            (SessionLifecycle::Creating, AgentPhase::Absent),
            (SessionLifecycle::Initializing, AgentPhase::Absent),
            (SessionLifecycle::Deleting, AgentPhase::Ended),
            (SessionLifecycle::Available, AgentPhase::Running),
            (SessionLifecycle::Available, AgentPhase::Waiting),
            (SessionLifecycle::Available, AgentPhase::Interrupted),
            (SessionLifecycle::Available, AgentPhase::Ready),
            (SessionLifecycle::Available, AgentPhase::Ended),
            (SessionLifecycle::Available, AgentPhase::Exited),
            (SessionLifecycle::Available, AgentPhase::Sleeping),
            (SessionLifecycle::Available, AgentPhase::Absent),
        ] {
            let session = session(STEADY_ID, "one", lifecycle, phase);
            let text = super::plot(&session, 0, false).rows.join("\n");
            for caption in ["growing", "heading home", "walking"] {
                assert!(
                    !text.contains(caption),
                    "{lifecycle:?}/{phase:?} repeated its pose as {caption:?}: {text}"
                );
            }
        }

        let failed = session(
            STEADY_ID,
            "one",
            SessionLifecycle::Failed,
            AgentPhase::Absent,
        );
        assert!(
            super::plot(&failed, 0, false)
                .rows
                .join("\n")
                .contains("failed")
        );
    }

    #[test]
    fn a_running_usagi_uses_varied_poses_and_an_idle_one_blinks() {
        let poses = (0..super::RUNNING_ACTION_CYCLE_TICKS)
            .map(|tick| only(SessionLifecycle::Available, AgentPhase::Running, tick).join("\n"))
            .collect::<std::collections::HashSet<_>>();
        assert!(
            poses.len() >= 5,
            "the five actions need visibly varied poses"
        );

        // idle は phase 4 でだけ瞬きする。
        let open = only(SessionLifecycle::Available, AgentPhase::Ready, 0).join("\n");
        let blink = only(SessionLifecycle::Available, AgentPhase::Ready, 4).join("\n");
        assert!(open.contains("( . .)"));
        assert!(blink.contains("( -.-)"));

        // 終了済みの agent は idle の瞬きへ戻さず、静止した done pose を保つ。
        let ended = session(
            STEADY_ID,
            "one",
            SessionLifecycle::Available,
            AgentPhase::Ended,
        );
        let done = super::plot(&ended, 0, false);
        let done_later = super::plot(&ended, 4, false);
        assert_eq!(done, done_later);
        assert!(done.rows.join("\n").contains(" z"));
    }

    #[test]
    fn running_actions_have_distinct_lengths_and_all_run_once_per_round() {
        let durations = super::RunningAction::ALL.map(super::RunningAction::duration);
        assert_eq!(durations, [3, 4, 5, 6, 7]);

        for action in super::RunningAction::ALL {
            let ticks = (0..super::RUNNING_ANIMATION_CYCLE_TICKS)
                .filter(|tick| super::running_action(*tick, STEADY_ID).0 == action)
                .count();
            assert_eq!(
                ticks as u64,
                action.duration() * super::RUNNING_ACTION_SEQUENCE_ROUNDS
            );
            for progress in 0..action.duration() {
                let pose = super::running_pose(action, progress);
                assert!(pose.iter().any(|row| !row.is_empty()));
                assert!(
                    pose.iter()
                        .all(|row| display_width(row) <= super::COMPACT_RABBIT_WIDTH)
                );
            }
        }
    }

    #[test]
    fn runtime_identity_randomizes_running_action_order() {
        let first = super::shuffled_running_actions(super::stable_hash(STEADY_ID));
        let second = super::shuffled_running_actions(super::stable_hash(
            "10000000-0000-4000-8000-000000000001",
        ));
        assert_ne!(first, second);

        let repeated = super::shuffled_running_actions(super::stable_hash(STEADY_ID));
        assert_eq!(first, repeated, "the same runtime must not jump on refresh");

        let next_round =
            super::shuffled_running_actions(super::stable_hash(STEADY_ID) ^ 0x9e37_79b9_7f4a_7c15);
        assert_ne!(first, next_round, "each round should reshuffle its actions");
    }

    #[test]
    fn calm_lifecycle_animation_uses_bounded_poses_and_reduced_motion_is_static() {
        let waiting = only(SessionLifecycle::Available, AgentPhase::Waiting, 0).join("\n");
        let waiting_flop = only(SessionLifecycle::Available, AgentPhase::Waiting, 5).join("\n");
        assert!(waiting.contains("/)/)"));
        assert!(waiting_flop.contains("/)(/"));

        let lifecycle_rows = |lifecycle, tick, reduced_motion| {
            let mut session = session(STEADY_ID, "one", lifecycle, AgentPhase::Absent);
            session.agents.clear();
            plain(&render(24, 100, "x", &[session], tick, reduced_motion).expect("fits")).join("\n")
        };
        let growing = lifecycle_rows(SessionLifecycle::Creating, 0, false);
        let emerged = lifecycle_rows(SessionLifecycle::Creating, 3, false);
        assert_ne!(growing, emerged);
        assert!(growing.contains("__(_ _)__"));
        assert!(emerged.contains("_( . .)_"));

        assert_ne!(
            lifecycle_rows(SessionLifecycle::Deleting, 0, false),
            lifecycle_rows(SessionLifecycle::Deleting, 2, false)
        );
        assert_ne!(
            lifecycle_rows(SessionLifecycle::Deleting, 2, false),
            lifecycle_rows(SessionLifecycle::Deleting, 4, false)
        );

        assert_eq!(
            only_with_motion(SessionLifecycle::Available, AgentPhase::Waiting, 0, true),
            only_with_motion(SessionLifecycle::Available, AgentPhase::Waiting, 5, true)
        );
        for lifecycle in [SessionLifecycle::Creating, SessionLifecycle::Deleting] {
            assert_eq!(
                lifecycle_rows(lifecycle, 0, true),
                lifecycle_rows(lifecycle, 5, true),
                "{lifecycle:?} must be static with reduced motion"
            );
        }
    }

    #[test]
    fn canonical_tick_collapses_only_visually_identical_garden_phases() {
        let idle = session(
            STEADY_ID,
            "idle",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        assert_eq!(
            super::canonical_tick(17, 100, std::slice::from_ref(&idle), 1, false),
            super::canonical_tick(17, 100, std::slice::from_ref(&idle), 0, false),
            "a held pose stays collapsed across the cycle boundary"
        );
        assert_eq!(
            super::canonical_tick(17, 100, std::slice::from_ref(&idle), 4, false),
            Some(4)
        );

        let running = session(
            STEADY_ID,
            "running",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        let tick = 3;
        let canonical = super::canonical_tick(17, 100, std::slice::from_ref(&running), tick, false)
            .expect("fits");
        assert_eq!(
            render(24, 100, "x", std::slice::from_ref(&running), tick, false)
                .expect("fits")
                .rows,
            render(
                24,
                100,
                "x",
                std::slice::from_ref(&running),
                canonical,
                false,
            )
            .expect("fits")
            .rows
        );

        let waiting = session(
            STEADY_ID,
            "waiting",
            SessionLifecycle::Available,
            AgentPhase::Waiting,
        );
        assert_eq!(
            super::canonical_tick(17, 100, std::slice::from_ref(&waiting), 5, false),
            Some(5)
        );
        assert_eq!(super::canonical_tick(17, 100, &[waiting], 5, true), Some(0));
        assert_eq!(super::canonical_tick(17, 100, &[], 5, false), Some(4));
        assert_eq!(super::canonical_tick(12, 100, &[], 5, false), None);
    }

    #[test]
    fn rabbit_colours_are_stable_per_runtime_and_vary_between_runtimes() {
        let first = super::rabbit_style(STEADY_ID);
        assert_eq!(first, super::rabbit_style(STEADY_ID));

        let styles = (0..16)
            .map(|index| super::rabbit_style(&format!("{index:08x}-0000-4000-8000-000000000001")))
            .collect::<Vec<_>>();
        assert!(styles.iter().any(|style| *style != first));
    }

    #[test]
    fn ambient_motion_advances_without_claiming_that_calm_agents_are_active() {
        let done = session(
            STEADY_ID,
            "done",
            SessionLifecycle::Available,
            AgentPhase::Ended,
        );
        assert_eq!(
            super::plot(&done, 0, false),
            super::plot(&done, 2, false),
            "the finished rabbit itself must stay still"
        );
        assert_ne!(
            render(24, 100, "x", std::slice::from_ref(&done), 0, false)
                .expect("fits")
                .rows,
            render(24, 100, "x", std::slice::from_ref(&done), 2, false)
                .expect("fits")
                .rows,
            "fixed-position stars and grass should provide the only calm motion"
        );
        assert_eq!(
            render(24, 100, "x", std::slice::from_ref(&done), 0, true)
                .expect("fits")
                .rows,
            render(24, 100, "x", &[done], 20, true).expect("fits").rows,
            "reduced motion must freeze the whole Garden"
        );
    }

    #[test]
    fn canonical_tick_for_a_dense_garden_tracks_the_ambient_frame() {
        let mut sessions = vec![
            session(
                STEADY_ID,
                "still-a",
                SessionLifecycle::Failed,
                AgentPhase::Absent,
            ),
            session(
                "01000000-0000-4000-8000-000000000001",
                "still-b",
                SessionLifecycle::Failed,
                AgentPhase::Absent,
            ),
        ];
        sessions.push(session(
            "02000000-0000-4000-8000-000000000001",
            "hidden-running",
            SessionLifecycle::Available,
            AgentPhase::Running,
        ));

        for tick in 0..super::ANIMATION_CYCLE_TICKS {
            assert_eq!(
                super::canonical_tick(14, 64, &sessions, tick, false),
                Some(tick - tick % super::AMBIENT_PHASE_TICKS),
                "dense cards are static while the sky keeps its cadence"
            );
        }
    }

    #[test]
    fn selection_does_not_change_nameplate_and_safe_failure_summary_stays_visible() {
        let mut selected = session(
            STEADY_ID,
            "chosen",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        selected.selected = true;
        let mut unselected = selected.clone();
        unselected.selected = false;
        assert_eq!(
            super::plot(&selected, 0, false),
            super::plot(&unselected, 0, false),
            "Garden does not decorate the selected session"
        );
        let mut failed = session(
            "01000000-0000-4000-8000-000000000001",
            "broken",
            SessionLifecycle::Failed,
            AgentPhase::Absent,
        );
        failed.failure_summary = Some("branch exists".to_owned());
        let frame = render(24, 100, "x", &[selected, failed], 0, false).expect("fits");
        let text = plain(&frame).join("\n");
        assert!(text.contains("chosen"));
        assert!(!text.contains("> chosen"));
        assert!(text.contains("failed · branch exists"));
        assert!(frame.rows.iter().all(|row| display_width(row) == 100));
    }

    #[test]
    fn dense_cards_keep_each_agents_status_and_rabbit() {
        let waiting = (0..4)
            .map(|index| {
                agent(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    AgentPhase::Waiting,
                )
            })
            .collect::<Vec<_>>();
        let make_session = |agents| GardenSession {
            id: SessionId::parse(STEADY_ID).expect("fixture id"),
            label: "many".to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pending_decisions: 0,
            pr_merged: false,
            agents,
            agent_status: None,
        };

        let all_waiting =
            render(24, 100, "x", &[make_session(waiting.clone())], 0, false).expect("fits");
        let all_waiting_text = plain(&all_waiting).join("\n");
        assert_eq!(rabbits(&all_waiting).len(), 4);
        assert_eq!(all_waiting_text.matches("wait").count(), 4);
        assert_eq!(all_waiting_text.matches("(o.o)?").count(), 4);

        let mut mixed = waiting;
        mixed.push(agent(
            "f0000000-0000-4000-8000-000000000001",
            AgentPhase::Running,
        ));
        let mixed = render(24, 100, "x", &[make_session(mixed)], 0, false).expect("fits");
        let mixed_text = plain(&mixed).join("\n");
        assert_eq!(rabbits(&mixed).len(), 5);
        assert_eq!(mixed_text.matches("wait").count(), 4);
        assert_eq!(mixed_text.matches("run").count(), 1);
    }

    #[test]
    fn a_pose_keeps_its_ears_over_its_head() {
        // 行ごとの中央寄せは、行幅が違う pose の耳を頭から横へずらしてしまう。
        // 最も差が出る `Creating`（耳 `/)/)` と頭 `__(_ _)__`）で崩れないことを固定する。
        let mut creating = session(
            STEADY_ID,
            "one",
            SessionLifecycle::Creating,
            AgentPhase::Absent,
        );
        creating.agents.clear();
        let rows = plain(&render(24, 100, "x", &[creating], 0, false).expect("fits"));
        let ears = rows
            .iter()
            .find_map(|row| row.find("/)/)"))
            .expect("the growing pose shows ears");
        let head = rows
            .iter()
            .find_map(|row| row.find("__(_ _)__"))
            .expect("the growing pose shows a head");
        let ears_center = ears + display_width("/)/)") / 2;
        let head_center = head + display_width("__(_ _)__") / 2;
        assert!(
            ears_center.abs_diff(head_center) <= 1,
            "ears at {ears_center} drifted off the head at {head_center}"
        );
    }

    #[test]
    fn every_rabbit_pose_keeps_its_ears_on_the_face_axis() {
        for action in super::RunningAction::ALL {
            for progress in 0..action.duration() {
                assert_rabbit_axis(
                    &format!("{action:?}/{progress}"),
                    &super::running_pose(action, progress),
                );
            }
        }
        for phase in AgentPhase::ALL {
            let (_, _, _, pose) = super::agent_appearance(phase, 5, false, STEADY_ID);
            assert_rabbit_axis(&format!("{phase:?}"), &pose);
        }
    }

    #[test]
    fn every_session_illustration_keeps_its_ears_on_the_face_axis() {
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Deleting,
            SessionLifecycle::Failed,
        ] {
            for tick in 0..6 {
                let rows = only(lifecycle, AgentPhase::Absent, tick);
                let pose = rows.iter().map(String::as_str).collect::<Vec<_>>();
                assert_rabbit_axis(&format!("{lifecycle:?}/{tick}"), &pose);
            }
        }

        let mut merged = session(
            STEADY_ID,
            "merged",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        merged.pr_merged = true;
        for tick in 0..2 {
            let rows = plain(&render(24, 100, "x", &[merged.clone()], tick, false).unwrap());
            let pose = rows.iter().map(String::as_str).collect::<Vec<_>>();
            assert_rabbit_axis(&format!("merged/{tick}"), &pose);
        }
    }

    #[test]
    fn a_partly_filled_row_is_centered_in_the_full_width_garden() {
        let mut sessions = fixtures();
        sessions.push(session(
            "04000000-0000-4000-8000-000000000001",
            "last",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        ));
        let frame = render(24, 120, "x", &sessions, 1, false).expect("garden fits");
        let plots = plots(&frame);
        assert_eq!(plots.len(), 5);
        assert_eq!(plots[0].row, plots[3].row);
        assert_ne!(plots[3].row, plots[4].row);
        assert!(plots[0].column >= super::SIDE_PADDING);
        assert_eq!(plots[0].column + super::PLOT_WIDTH, plots[1].column);
        assert_eq!(plots[4].column, (120 - super::PLOT_WIDTH) / 2);

        // 地面は左の庭領域いっぱいに伸び、うさぎの数で途切れない。
        let rows = plain(&frame);
        let ground = grass_row(&rows);
        assert_eq!(display_width(ground), 120);
        assert!(ground.trim_start().starts_with("--"));
        assert!(!ground.trim().contains("  "));
    }

    #[test]
    fn the_ground_joins_across_neighbouring_plots() {
        for pattern in GRASS.into_iter().chain(SOIL) {
            assert_eq!(display_width(pattern), PLOT_WIDTH);
        }
        let sessions = (0..3)
            .map(|index| {
                session(
                    &format!("0{index}000000-0000-4000-8000-000000000001"),
                    "s",
                    SessionLifecycle::Available,
                    AgentPhase::Ready,
                )
            })
            .collect::<Vec<_>>();
        let frame = render(17, 100, "x", &sessions, 0, false).expect("garden fits");
        let rows = plain(&frame);
        let ground = grass_row(&rows);
        assert!(!ground.trim().contains("  "), "ground broke: {ground:?}");
    }

    #[test]
    fn ambient_motion_twinkles_in_place_and_reduced_motion_is_static() {
        let sessions = fixtures();
        let first = render(24, 100, "my-project", &sessions, 0, true).expect("fits");
        let second = render(24, 100, "my-project", &sessions, 5, true).expect("fits");
        let rows = plain(&first);
        assert_eq!(
            rows[1],
            plain(&second)[1],
            "reduced motion must keep the sky static"
        );
        assert!(rows[1].contains('*') || rows[1].contains('.'));

        let grass = rows
            .iter()
            .position(|row| row.trim_start().starts_with("--"))
            .expect("grass layer");
        assert!(rows[grass + 1].contains('.'), "soil follows the grass");

        let moving_first = plain(&render(24, 100, "my-project", &sessions, 0, false).unwrap());
        let moving_second = plain(&render(24, 100, "my-project", &sessions, 2, false).unwrap());
        assert_ne!(moving_first[1], moving_second[1], "the sky should twinkle");
        assert_ne!(
            grass_row(&moving_first),
            grass_row(&moving_second),
            "the grass should sway"
        );
        let occupied = |row: &str| {
            row.chars()
                .enumerate()
                .filter_map(|(column, ch)| (!ch.is_whitespace()).then_some(column))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            occupied(&moving_first[1]),
            occupied(&moving_second[1]),
            "twinkles may change brightness but must not jump around"
        );
    }

    #[test]
    fn calm_agent_states_have_small_readable_environment_details() {
        let ready = only(SessionLifecycle::Available, AgentPhase::Ready, 0).join("\n");
        let done = only(SessionLifecycle::Available, AgentPhase::Ended, 0).join("\n");
        let failed = only(SessionLifecycle::Failed, AgentPhase::Absent, 0).join("\n");
        assert!(ready.contains("c(\")(\")v"));
        assert!(done.contains(" z"));
        assert!(failed.contains("c(\")(\")/"));
    }

    #[test]
    fn a_merged_pr_keeps_the_agent_usagi_and_marks_the_session() {
        let mut merged = session(
            STEADY_ID,
            "merged",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        merged.pr_merged = true;
        let animated =
            plain(&render(24, 100, "x", &[merged.clone()], 1, false).unwrap()).join("\n");
        let next = plain(&render(24, 100, "x", &[merged.clone()], 2, false).unwrap()).join("\n");
        let reduced = plain(&render(24, 100, "x", &[merged], 1, true).unwrap()).join("\n");
        assert!(animated.contains("PR merged!"));
        assert_ne!(animated, next);
        assert!(reduced.contains("PR merged!"));
        assert!(reduced.contains("/)/)"));
    }
}
