//! Session Garden の純粋な描画サンプル。
//!
//! daemon の状態を所有せず、表示用に閉じた [`GardenSession`] を固定 plot に並べる。
//! frame と同じ layout から [`GardenHitbox`] も返すため、後続実装は座標から session
//! identity を再計算せず click target を解決できる。

use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

use crate::presentation::theme::{Role, Style, garden_rabbit_style};

use super::agent_status;
use super::button::InlineButton;
use super::{clip_to_width, display_width, pad_to_width};

/// Garden を表示できる最小端末幅。
pub const MIN_WIDTH: usize = 64;
/// Project tab bar を除く Garden 本体を表示できる最小高さ。
pub const MIN_HEIGHT: usize = 13;

const SIDE_PADDING: usize = 2;
const HEADER_ROWS: usize = 3;
const FOOTER_ROWS: usize = 2;
const PLOT_WIDTH: usize = 28;
const PLOT_HEIGHT: usize = 7;
/// plot のうち、うさぎと label が占める行数（残り 1 行が地面）。
const PLOT_CONTENT_ROWS: usize = PLOT_HEIGHT - 1;
/// うさぎ 1 羽分の pose 行数（plot の label / status / 地面を除く）。
const SPRITE_ROWS: usize = 4;
const COMPACT_RABBIT_WIDTH: usize = 8;
/// plot の中で sprite が始まる行（nameplate と status 行の下）。うさぎの hitbox は
/// この行から [`SPRITE_ROWS`] 行ぶんで、nameplate と status 行は区画のままにする。
const SPRITE_TOP_ROW: usize = PLOT_CONTENT_ROWS - SPRITE_ROWS;
const MAX_VISIBLE_AGENTS: usize = PLOT_WIDTH / COMPACT_RABBIT_WIDTH;
const RUNNING_ACTION_CYCLE_TICKS: u64 = 25;
const RUNNING_ACTION_SEQUENCE_ROUNDS: u64 = 4;
const RUNNING_ANIMATION_CYCLE_TICKS: u64 =
    RUNNING_ACTION_CYCLE_TICKS * RUNNING_ACTION_SEQUENCE_ROUNDS;
pub(crate) const ANIMATION_CYCLE_TICKS: u64 = 300;

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

/// 地面のタイル。庭の幅いっぱいに敷き詰めるため、隣り合うタイルで草の位置を変えて
/// 同じ絵が横に並ぶ tiling に見せない。
const GROUND: [&str; 3] = [
    "--v-------v-----------v-----",
    "------v---------v----v------",
    "---v----------v-------v-----",
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
    /// this plot. Inactive project snapshots set this false instead of claiming
    /// that an empty cached list means the session owns no Agents.
    pub agents_observed: bool,
    pub agents: Vec<GardenAgent>,
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
    /// `None` は区画そのもの（nameplate・status 行・うさぎの居ない余白）で、click は
    /// session の訪問だけを意味する。
    pub agent: Option<AgentRuntimeId>,
    pub column: usize,
    pub row: usize,
    pub width: usize,
    pub height: usize,
}

impl GardenHitbox {
    /// terminal cell がこの plot に含まれるか。
    #[must_use]
    pub const fn contains(self, column: usize, row: usize) -> bool {
        column >= self.column
            && column < self.column + self.width
            && row >= self.row
            && row < self.row + self.height
    }
}

/// Garden footer の page button と、押したときに表示する page。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GardenPageHitbox {
    pub page: usize,
    pub column: usize,
    pub row: usize,
    pub width: usize,
    pub height: usize,
}

impl GardenPageHitbox {
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
    /// うさぎの rectangle は必ず、それを含む区画の rectangle より **先** に並ぶ。
    /// click 解決は最初に当たったものを採るため、うさぎが区画に優先する。
    pub hitboxes: Vec<GardenHitbox>,
    /// Footer の previous / next button。描画した span と click 範囲を共有する。
    pub page_hitboxes: Vec<GardenPageHitbox>,
    /// 描画した 0-based page。要求値が範囲外なら最後の page へ clamp する。
    pub page: usize,
    /// 空の Garden も 1 page と数える。
    pub page_count: usize,
    /// 端末に収まらず描画しなかった session 数。
    pub hidden_sessions: usize,
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
    garden_height: usize,
    capacity: usize,
}

fn garden_layout(height: usize, width: usize) -> Option<GardenLayout> {
    if height < MIN_HEIGHT || width < MIN_WIDTH {
        return None;
    }
    let content_width = width.saturating_sub(SIDE_PADDING * 2);
    let columns = (content_width / PLOT_WIDTH).max(1);
    let garden_height = height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let plot_rows = (garden_height / PLOT_HEIGHT).max(1);
    Some(GardenLayout {
        content_width,
        columns,
        garden_height,
        capacity: columns.saturating_mul(plot_rows),
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
    render_page(
        height,
        width,
        workspace_name,
        sessions,
        0,
        tick,
        reduced_motion,
    )
}

/// Garden の指定 page を描画する。page は現在の session 数に合わせて clamp する。
#[must_use]
pub fn render_page(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    requested_page: usize,
    tick: u64,
    reduced_motion: bool,
) -> Option<GardenFrame> {
    let GardenLayout {
        content_width,
        columns,
        garden_height,
        capacity,
    } = garden_layout(height, width)?;
    let page_count = sessions.len().div_ceil(capacity).max(1);
    let page = requested_page.min(page_count - 1);
    let page_start = page.saturating_mul(capacity).min(sessions.len());
    let page_end = (page_start + capacity).min(sessions.len());
    let page_sessions = &sessions[page_start..page_end];
    let visible = page_sessions.len();
    let hidden_sessions = sessions.len().saturating_sub(visible);

    let mut rows = Vec::with_capacity(height);
    rows.push(header_line(width, workspace_name, sessions));
    rows.push(Role::Feature.style().paint(&"·".repeat(width)));
    rows.push(" ".repeat(width));

    // 使う plot 行数だけを縦中央へ寄せ、庭の下側だけが大きく空くのを避ける。
    let used_rows = visible.div_ceil(columns);
    let grid_top = HEADER_ROWS + garden_height.saturating_sub(used_rows * PLOT_HEIGHT) / 2;
    rows.resize_with(grid_top, || " ".repeat(width));

    let mut hitboxes = Vec::with_capacity(visible * (1 + MAX_VISIBLE_AGENTS));
    for plot_row in 0..used_rows {
        let start = plot_row * columns;
        let end = (start + columns).min(visible);
        // 埋まった列数ではなく、その行に実際に並ぶ数で中央へ寄せる。容量ぶんの幅で
        // 中央寄せすると、session が列数に満たない行が左へ寄って庭が偏る。
        let row_left = SIDE_PADDING + content_width.saturating_sub((end - start) * PLOT_WIDTH) / 2;
        let plots = page_sessions[start..end]
            .iter()
            .map(|session| plot(session, tick, reduced_motion))
            .collect::<Vec<_>>();
        for local_row in 0..PLOT_CONTENT_ROWS {
            let mut line = " ".repeat(row_left);
            for plot in &plots {
                line.push_str(&pad_to_width(&plot.rows[local_row], PLOT_WIDTH));
            }
            rows.push(pad_to_width(&line, width));
        }
        // 地面は plot の下だけでなく庭の幅いっぱいに敷く。うさぎの数で地面が途切れると
        // 中央の島のように見えるため。
        rows.push(ground_row(width, content_width));
        for (column, (session, plot)) in page_sessions[start..end].iter().zip(&plots).enumerate() {
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

    if visible == 0 {
        rows.push(centered(
            width,
            &Style::new().dim().paint("No sessions in the garden"),
        ));
    }

    let footer_start = height - FOOTER_ROWS;
    rows.resize_with(footer_start, || " ".repeat(width));
    let (page_footer, page_hitboxes) = page_footer(width, footer_start, page, page_count);
    rows.push(page_footer);
    let help = if page_count > 1 {
        "Garden · ←/→ page · click to visit · Esc to return"
    } else {
        "Garden · click a usagi to visit · any key to return"
    };
    rows.push(centered(width, &Style::new().dim().paint(help)));

    Some(GardenFrame {
        rows,
        hitboxes,
        page_hitboxes,
        page,
        page_count,
        hidden_sessions,
    })
}

fn page_footer(
    width: usize,
    row: usize,
    page: usize,
    page_count: usize,
) -> (String, Vec<GardenPageHitbox>) {
    if page_count <= 1 {
        return (" ".repeat(width), Vec::new());
    }

    let previous = InlineButton::new("← Prev");
    let next = InlineButton::new("Next →");
    let previous_rendered = previous.render(
        width,
        if page == 0 {
            Style::new().dim()
        } else {
            Role::Feature.style()
        },
    );
    let next_rendered = next.render(
        width,
        if page + 1 == page_count {
            Style::new().dim()
        } else {
            Role::Feature.style()
        },
    );
    let indicator = Style::new()
        .dim()
        .paint(&format!("· Page {}/{} ·", page + 1, page_count));
    let content_width = previous_rendered
        .width
        .saturating_add(display_width(&indicator))
        .saturating_add(next_rendered.width);
    let left = width.saturating_sub(content_width) / 2;
    let next_column = left
        .saturating_add(previous_rendered.width)
        .saturating_add(display_width(&indicator));
    let line = pad_to_width(
        &format!(
            "{}{}{}{}",
            " ".repeat(left),
            previous_rendered.line,
            indicator,
            next_rendered.line
        ),
        width,
    );
    (
        line,
        vec![
            GardenPageHitbox {
                page: page.saturating_sub(1),
                column: left,
                row,
                width: previous_rendered.width,
                height: 1,
            },
            GardenPageHitbox {
                page: (page + 1).min(page_count - 1),
                column: next_column,
                row,
                width: next_rendered.width,
                height: 1,
            },
        ],
    )
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
    canonical_tick_page(height, width, sessions, 0, tick, reduced_motion)
}

/// [`canonical_tick`] for the page currently visible in the Garden.
#[must_use]
pub fn canonical_tick_page(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    requested_page: usize,
    tick: u64,
    reduced_motion: bool,
) -> Option<u64> {
    let layout = garden_layout(height, width)?;
    let page_count = sessions.len().div_ceil(layout.capacity).max(1);
    let page = requested_page.min(page_count - 1);
    let start = page.saturating_mul(layout.capacity).min(sessions.len());
    let end = (start + layout.capacity).min(sessions.len());
    let sessions = &sessions[start..end];
    if reduced_motion || !sessions.iter().any(session_may_animate) {
        return Some(0);
    }
    let tick = tick % ANIMATION_CYCLE_TICKS;
    let expected = sessions
        .iter()
        .map(|session| plot(session, tick, reduced_motion))
        .collect::<Vec<_>>();
    let mut canonical = 0;
    for distance in 1..ANIMATION_CYCLE_TICKS {
        let candidate = (tick + ANIMATION_CYCLE_TICKS - distance) % ANIMATION_CYCLE_TICKS;
        let same = sessions
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

fn session_may_animate(session: &GardenSession) -> bool {
    if !session.agents_observed {
        return false;
    }
    if session.pr_merged {
        return true;
    }
    match session.lifecycle {
        SessionLifecycle::Creating
        | SessionLifecycle::Initializing
        | SessionLifecycle::Deleting => true,
        SessionLifecycle::Failed => false,
        SessionLifecycle::Available => agent_status::ordered(&session.agents)
            .into_iter()
            .take(MAX_VISIBLE_AGENTS)
            .any(|agent| {
                matches!(
                    agent.phase,
                    AgentPhase::Running
                        | AgentPhase::Waiting
                        | AgentPhase::Absent
                        | AgentPhase::Ready
                )
            }),
    }
}

fn header_line(width: usize, workspace_name: &str, sessions: &[GardenSession]) -> String {
    let running = sessions
        .iter()
        .filter(|session| session.lifecycle == SessionLifecycle::Available)
        .flat_map(|session| &session.agents)
        .filter(|agent| agent.phase == AgentPhase::Running)
        .count();
    let left = Role::Feature.style().bold().paint(&format!(
        " usagi / {}",
        clip_to_width(workspace_name, width / 2)
    ));
    let right = Style::new()
        .dim()
        .paint(&format!("{} sessions · {running} running ", sessions.len()));
    let gap = width.saturating_sub(display_width(&left) + display_width(&right));
    pad_to_width(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

fn plot(session: &GardenSession, tick: u64, reduced_motion: bool) -> Plot {
    let nameplate = clip_to_width(&session.label, PLOT_WIDTH);
    let label = Style::new().dim().paint(&nameplate);
    let ([status, ears, head, body, feet], rabbits) = if session.agents_observed {
        match session.lifecycle {
            SessionLifecycle::Available => available_plot(session, tick, reduced_motion),
            // lifecycle の pose は session そのものの姿で、agent 1 体には対応しない。
            _ => (lifecycle_plot(session, tick, reduced_motion), Vec::new()),
        }
    } else {
        (inactive_plot(session), Vec::new())
    };
    Plot {
        rows: [centered(PLOT_WIDTH, &label), status, ears, head, body, feet],
        rabbits,
    }
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

/// 庭の幅いっぱいに敷いた地面の 1 行。
///
/// [`GROUND`] のタイルを順に並べて `content_width` 桁ちょうどで切る。タイルは ASCII
/// なので 1 文字 = 1 桁で、途中で切っても桁がずれない。
fn ground_row(width: usize, content_width: usize) -> String {
    let soil = GROUND
        .iter()
        .cycle()
        .flat_map(|tile| tile.chars())
        .take(content_width)
        .collect::<String>();
    pad_to_width(
        &format!(
            "{}{}",
            " ".repeat(SIDE_PADDING),
            Style::new().dim().paint(&soil)
        ),
        width,
    )
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
                ["", "  /)/)", " _( . .)_", "__/   \\__"]
            };
            ("growing".to_owned(), Role::Warning.style(), feature, rabbit)
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
                "heading home".to_owned(),
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
                ["", " /)/)", "( x.x)", "c(\")(\")"],
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

/// `Available` な区画の status 行 + sprite 4 行と、その中のうさぎの横位置。
fn available_plot(
    session: &GardenSession,
    tick: u64,
    reduced_motion: bool,
) -> ([String; PLOT_CONTENT_ROWS - 1], Vec<PlacedRabbit>) {
    if session.pr_merged {
        let rabbit = if reduced_motion || tick.is_multiple_of(2) {
            ["  \\ /", "  /)/)", " \\(^.^)/", " c(\")(\")"]
        } else {
            [" ✨  ✨", "  /)/)", " \\(^o^)/", " c(\")(\")"]
        };
        let [ears, head, body, feet] = sprite(rabbit, Role::Feature.style().bold(), PLOT_WIDTH);
        // celebration は session の祝いの姿で、特定の agent ではない。
        return (
            [
                centered(
                    PLOT_WIDTH,
                    &Role::Success.style().bold().paint("PR merged! ✨"),
                ),
                ears,
                head,
                body,
                feet,
            ],
            Vec::new(),
        );
    }
    let agents = agent_status::ordered(&session.agents);
    if agents.is_empty() {
        return (
            [
                centered(PLOT_WIDTH, &Style::new().dim().paint("no agents")),
                " ".repeat(PLOT_WIDTH),
                " ".repeat(PLOT_WIDTH),
                " ".repeat(PLOT_WIDTH),
                " ".repeat(PLOT_WIDTH),
            ],
            Vec::new(),
        );
    }

    if agents.len() == 1 {
        let agent = agents[0];
        let (status, status_style, rabbit_style, rabbit) = agent_appearance(
            agent.phase,
            tick,
            reduced_motion,
            &agent.runtime_id.as_str(),
        );
        let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
        // 1 羽だけの区画はうさぎを大きく描くので、その 1 体が sprite 行の全幅を持つ。
        return (
            [
                centered(PLOT_WIDTH, &status_style.paint(status)),
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
    // うさぎは plot 幅の都合で先頭数体しか置けないので、status 行は sidebar と同じ
    // 記号列で **全** agent の phase を示す。畳まれたうさぎが何をしているかを、
    // `+N hidden` のような件数ではなく phase そのもので読めるようにするため。
    let status = agent_status::status_line(&agents, PLOT_WIDTH);
    let mut rows: [String; SPRITE_ROWS] = std::array::from_fn(|_| String::new());
    for agent in visible {
        let (_, _, style, rabbit) = agent_appearance(
            agent.phase,
            tick,
            reduced_motion,
            &agent.runtime_id.as_str(),
        );
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
        AgentPhase::Ended | AgentPhase::Exited => (
            "done",
            Style::new().dim(),
            feature,
            ["", " /)/)", "( -.-)", "c(\")(\")"],
        ),
        AgentPhase::Absent | AgentPhase::Ready => {
            let face = if phase == 4 { "( -.-)" } else { "( . .)" };
            (
                "available",
                Style::new().dim(),
                feature,
                ["", " /)/)", face, "c(\")(\")"],
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
            0 => ["", " /)/)", "_( o.o)_", "  / >#"],
            1 => ["", " /)/)", "_( o.o)_", " #< \\"],
            _ => ["", " /)/)", "_( o.o)_", "  # #"],
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
    use super::{
        GROUND, GardenAgent, GardenSession, MIN_HEIGHT, MIN_WIDTH, PLOT_WIDTH, render, render_page,
    };
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::id::{AgentRuntimeId, SessionId};
    use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

    /// animation offset が 0 になる id（先頭 2 桁が `00`）。tick をそのまま phase として扱える。
    const STEADY_ID: &str = "00000000-0000-4000-8000-000000000001";

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

    fn plain(frame: &super::GardenFrame) -> Vec<String> {
        frame
            .rows
            .iter()
            .map(|row| {
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
            })
            .collect()
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
            pr_merged: false,
            agents: vec![GardenAgent {
                runtime_id: AgentRuntimeId::parse(id).expect("fixture runtime id"),
                phase,
            }],
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
        for hitbox in frame.hitboxes {
            assert!(hitbox.contains(hitbox.column, hitbox.row));
            assert!(!hitbox.contains(hitbox.column + hitbox.width, hitbox.row));
        }
        let text = frame.rows.join("\n");
        assert!(text.contains("session-auth"));
        assert!(text.contains("日本語-session"));
        assert!(text.contains("running"));
        assert!(text.contains("failed"));
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
        assert_eq!(frame.hidden_sessions, 0);
    }

    #[test]
    fn overflow_is_paged_deterministically_and_every_session_is_reachable() {
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
        let first = render_page(14, 64, "x", &sessions, 0, 3, false).expect("garden fits");
        let second = render_page(14, 64, "x", &sessions, 0, 3, false).expect("garden fits");
        assert_eq!(first, second);
        assert_eq!(first.page, 0);
        assert_eq!(first.page_count, 10);
        assert!(first.rows.join("\n").contains("Page 1/10"));
        assert!(!first.rows.join("\n").contains("session-2"));

        let last =
            render_page(14, 64, "x", &sessions, usize::MAX, 3, false).expect("last page fits");
        assert_eq!(last.page, 9);
        assert!(last.rows.join("\n").contains("Page 10/10"));
        assert!(last.rows.join("\n").contains("session-19"));
        assert_eq!(last.page_hitboxes.len(), 2);
        assert_eq!(last.page_hitboxes[0].page, 8);
        assert_eq!(last.page_hitboxes[1].page, 9);
        for hitbox in &last.page_hitboxes {
            assert!(hitbox.contains(hitbox.column, hitbox.row));
            assert!(!hitbox.contains(hitbox.column + hitbox.width, hitbox.row));
        }

        let reachable = (0..first.page_count)
            .flat_map(|page| {
                let frame =
                    render_page(14, 64, "x", &sessions, page, 3, false).expect("every page fits");
                plots(&frame)
                    .into_iter()
                    .map(|hitbox| hitbox.session_id)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reachable,
            sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>()
        );
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
            pr_merged: false,
            agents,
        };
        let first = render(24, 100, "x", &[make_session(shuffled)], 2, false).expect("fits");
        let second = render(24, 100, "x", &[make_session(reversed)], 2, false).expect("fits");
        assert_eq!(first, second);
        let text = plain(&first).join("\n");
        // status 行は sidebar と同じ記号列で、うさぎに置けなかった 2 体を含む
        // **全** agent の phase を注意順に並べる。
        assert!(text.contains("◆ ● ● ○ ◦"), "{text}");
        assert!(
            text.contains("( o.o)?"),
            "the waiting agent must stay visible"
        );
        assert_eq!(text.matches("o.o").count(), super::MAX_VISIBLE_AGENTS);
    }

    /// うさぎ 1 羽は 1 agent なので、羽ごとに hitbox を返す。box は必ず区画の内側で、
    /// 描かれたうさぎの絵に重なり、注意順（`Waiting` が先頭）と同じ並びになる。
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
            pr_merged: false,
            agents: vec![folded, running, waiting, hidden, ready],
        }];
        let frame = render(24, 100, "x", &sessions, 0, true).expect("garden fits");
        let rabbits = rabbits(&frame);
        // 区画に置けるのは MAX_VISIBLE_AGENTS 羽までで、畳まれた分は box を持たない
        // （status 行の記号列だけが残る）。
        assert_eq!(rabbits.len(), super::MAX_VISIBLE_AGENTS);
        assert_eq!(
            rabbits
                .iter()
                .map(|rabbit| rabbit.agent.expect("a rabbit names its runtime"))
                .collect::<Vec<_>>(),
            vec![waiting.runtime_id, running.runtime_id, ready.runtime_id],
        );
        let plot = plots(&frame)[0];
        let rows = plain(&frame);
        for rabbit in &rabbits {
            assert_eq!(rabbit.session_id, plot.session_id);
            assert_eq!(rabbit.width, super::COMPACT_RABBIT_WIDTH);
            // 区画の内側、かつ nameplate / status 行より下（地面より上）。
            assert!(rabbit.column >= plot.column);
            assert!(rabbit.column + rabbit.width <= plot.column + plot.width);
            assert!(rabbit.row > plot.row);
            assert!(rabbit.row + rabbit.height < plot.row + plot.height);
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

    /// lifecycle の pose と celebration は session そのものの姿で、agent 1 体に
    /// 対応しない。押しても訪問先は session だけになる。
    #[test]
    fn a_session_pose_names_no_agent() {
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Deleting,
            SessionLifecycle::Failed,
        ] {
            let sessions = vec![session(STEADY_ID, "one", lifecycle, AgentPhase::Running)];
            let frame = render(24, 100, "x", &sessions, 0, true).expect("garden fits");
            assert!(rabbits(&frame).is_empty(), "{lifecycle:?}");
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
        assert!(rabbits(&frame).is_empty());
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
                pr_merged: false,
                agents: Vec::new(),
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
                pr_merged: false,
                agents: Vec::new(),
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
                pr_merged: false,
                agents: Vec::new(),
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
            assert_eq!(first.rows, later.rows, "{lifecycle:?} must not animate");
            let text = plain(&first).join("\n");
            assert!(text.contains(status), "{text}");
            assert!(!text.contains("growing"), "{text}");
            assert!(!text.contains("heading home"), "{text}");
            assert_eq!(
                super::canonical_tick(24, 100, std::slice::from_ref(&cached), 5, false),
                Some(0)
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
    fn every_lifecycle_and_agent_phase_states_itself_in_text() {
        let cases = [
            (SessionLifecycle::Creating, AgentPhase::Absent, "growing"),
            (
                SessionLifecycle::Initializing,
                AgentPhase::Absent,
                "growing",
            ),
            (
                SessionLifecycle::Deleting,
                AgentPhase::Ended,
                "heading home",
            ),
            (SessionLifecycle::Failed, AgentPhase::Absent, "failed"),
            (SessionLifecycle::Available, AgentPhase::Running, "running"),
            (SessionLifecycle::Available, AgentPhase::Waiting, "waiting"),
            (
                SessionLifecycle::Available,
                AgentPhase::Interrupted,
                "interrupted",
            ),
            (SessionLifecycle::Available, AgentPhase::Ready, "available"),
            (SessionLifecycle::Available, AgentPhase::Ended, "done"),
            (SessionLifecycle::Available, AgentPhase::Exited, "done"),
            (SessionLifecycle::Available, AgentPhase::Absent, "available"),
        ];
        for (lifecycle, phase, status) in cases {
            let text = only(lifecycle, phase, 0).join("\n");
            assert!(
                text.contains(status),
                "{lifecycle:?}/{phase:?} should read as {status}"
            );
        }
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
        let done = only(SessionLifecycle::Available, AgentPhase::Ended, 0).join("\n");
        let done_later = only(SessionLifecycle::Available, AgentPhase::Ended, 4).join("\n");
        assert_eq!(done, done_later);
        assert!(done.contains("done"));
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

        let growing = only(SessionLifecycle::Creating, AgentPhase::Absent, 0).join("\n");
        let emerged = only(SessionLifecycle::Creating, AgentPhase::Absent, 3).join("\n");
        assert_ne!(growing, emerged);
        assert!(growing.contains("__(_ _)__"));
        assert!(emerged.contains("_( . .)_"));

        let deleting = session(
            STEADY_ID,
            "leaving",
            SessionLifecycle::Deleting,
            AgentPhase::Ended,
        );
        let frame = |tick, reduced_motion| {
            render(
                24,
                100,
                "x",
                std::slice::from_ref(&deleting),
                tick,
                reduced_motion,
            )
            .expect("fits")
            .rows
        };
        assert_ne!(frame(0, false), frame(2, false));
        assert_ne!(frame(2, false), frame(4, false));

        for (lifecycle, phase) in [
            (SessionLifecycle::Available, AgentPhase::Waiting),
            (SessionLifecycle::Creating, AgentPhase::Absent),
            (SessionLifecycle::Deleting, AgentPhase::Ended),
        ] {
            assert_eq!(
                only_with_motion(lifecycle, phase, 0, true),
                only_with_motion(lifecycle, phase, 5, true),
                "{lifecycle:?}/{phase:?} must be static with reduced motion"
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
            super::canonical_tick(24, 100, std::slice::from_ref(&idle), 1, false),
            super::canonical_tick(24, 100, std::slice::from_ref(&idle), 0, false),
            "a held pose stays collapsed across the cycle boundary"
        );
        assert_eq!(
            super::canonical_tick(24, 100, std::slice::from_ref(&idle), 4, false),
            Some(4)
        );

        let running = session(
            STEADY_ID,
            "running",
            SessionLifecycle::Available,
            AgentPhase::Running,
        );
        let tick = 3;
        let canonical = super::canonical_tick(24, 100, std::slice::from_ref(&running), tick, false)
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
            super::canonical_tick(24, 100, std::slice::from_ref(&waiting), 5, false),
            Some(5)
        );
        assert_eq!(super::canonical_tick(24, 100, &[waiting], 5, true), Some(0));
        assert_eq!(super::canonical_tick(24, 100, &[], 5, false), Some(0));
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
    fn canonical_tick_knows_which_garden_states_can_move() {
        let creating = session(
            STEADY_ID,
            "creating",
            SessionLifecycle::Creating,
            AgentPhase::Absent,
        );
        let failed = session(
            STEADY_ID,
            "failed",
            SessionLifecycle::Failed,
            AgentPhase::Absent,
        );
        let done = session(
            STEADY_ID,
            "done",
            SessionLifecycle::Available,
            AgentPhase::Ended,
        );
        assert!(super::session_may_animate(&creating));
        assert!(!super::session_may_animate(&failed));
        assert!(!super::session_may_animate(&done));

        // 注意順（`agent_status::ordered`）で並べるため、動いている agent は plot の
        // 表示上限に押し出されない。押し出されるのは常に注意順で最も低い phase なので、
        // 「描かれないうさぎのために redraw する」状況そのものが起きない。
        let plot = |label: &str, agents| GardenSession {
            id: SessionId::parse(STEADY_ID).expect("fixture id"),
            label: label.to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pr_merged: false,
            agents,
        };
        let running_beyond_the_cap = plot(
            "running-beyond-the-cap",
            vec![
                agent("10000000-0000-4000-8000-000000000001", AgentPhase::Ended),
                agent("20000000-0000-4000-8000-000000000001", AgentPhase::Exited),
                agent(
                    "30000000-0000-4000-8000-000000000001",
                    AgentPhase::Interrupted,
                ),
                agent("f0000000-0000-4000-8000-000000000001", AgentPhase::Running),
            ],
        );
        assert!(
            super::session_may_animate(&running_beyond_the_cap),
            "attention order keeps the running agent inside the plot, so it animates"
        );
        let only_finished_work = plot(
            "only-finished-work",
            (0..4)
                .map(|index| {
                    agent(
                        &format!("{index:08x}-0000-4000-8000-000000000001"),
                        AgentPhase::Ended,
                    )
                })
                .collect(),
        );
        assert!(
            !super::session_may_animate(&only_finished_work),
            "finished work is the phase pushed out of the plot and it never animates"
        );
    }

    #[test]
    fn canonical_tick_ignores_sessions_outside_the_visible_capacity() {
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

        assert_eq!(
            super::canonical_tick(14, 64, &sessions, 0, false),
            super::canonical_tick(14, 64, &sessions, 1, false)
        );
        assert_ne!(
            super::canonical_tick_page(14, 64, &sessions, 1, 0, false),
            super::canonical_tick_page(14, 64, &sessions, 1, 1, false)
        );
        assert_ne!(
            super::canonical_tick(24, 100, &sessions, 0, false),
            super::canonical_tick(24, 100, &sessions, 1, false)
        );
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
    fn the_status_line_reports_agents_the_plot_cannot_draw() {
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
            label: "waiting".to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            pr_merged: false,
            agents,
        };

        let all_waiting =
            render(24, 100, "x", &[make_session(waiting.clone())], 0, false).expect("fits");
        let all_waiting_text = plain(&all_waiting).join("\n");
        // うさぎは 3 体までだが、記号列は 4 体すべてを数え上げる。
        assert!(
            all_waiting_text.contains("◆ ◆ ◆ ◆  4 wait"),
            "{all_waiting_text}"
        );
        assert_eq!(
            all_waiting_text.matches("( o.o)?").count(),
            super::MAX_VISIBLE_AGENTS
        );

        let mut mixed = waiting;
        mixed.push(agent(
            "f0000000-0000-4000-8000-000000000001",
            AgentPhase::Running,
        ));
        let mixed = render(24, 100, "x", &[make_session(mixed)], 0, false).expect("fits");
        let mixed_text = plain(&mixed).join("\n");
        assert!(
            mixed_text.contains("◆ ◆ ◆ ◆ ●  4 wait · 1 run"),
            "{mixed_text}"
        );
    }

    #[test]
    fn a_pose_keeps_its_ears_over_its_head() {
        // 行ごとの中央寄せは、行幅が違う pose の耳を頭から横へずらしてしまう。
        // 最も差が出る `Creating`（耳 `/)/)` と頭 `__(_ _)__`）で崩れないことを固定する。
        let rows = only(SessionLifecycle::Creating, AgentPhase::Absent, 0);
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
    fn a_partly_filled_row_stays_centered_on_a_wide_terminal() {
        // 145 桁は plot 5 列ぶん入るが、session は 2 つしかない。容量ぶんの幅で中央寄せ
        // すると 2 羽が左へ寄って庭が偏るので、その行に実際に並ぶ数で中央へ寄せる。
        let sessions = fixtures()[..2].to_vec();
        let frame = render(41, 145, "x", &sessions, 1, false).expect("garden fits");
        let plots = plots(&frame);
        assert_eq!(plots.len(), 2);
        let left = plots[0].column;
        let right_gap = 145 - (plots[1].column + plots[1].width);
        assert!(
            left.abs_diff(right_gap) <= 1,
            "the row is off-centre: {left} left vs {right_gap} right"
        );

        // 地面は庭の幅いっぱいに伸び、うさぎの数で途切れない。
        let ground = plain(&frame)
            .into_iter()
            .find(|row| row.contains("--v"))
            .expect("the garden draws ground");
        assert_eq!(display_width(&ground), 145);
        assert_eq!(ground.trim_end().len(), 145 - super::SIDE_PADDING);
        assert!(!ground.trim().contains("  "));
    }

    #[test]
    fn the_ground_joins_across_neighbouring_plots() {
        for pattern in GROUND {
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
        let frame = render(24, 100, "x", &sessions, 0, false).expect("garden fits");
        let ground = plain(&frame)
            .into_iter()
            .find(|row| row.contains("--v"))
            .expect("the garden draws ground");
        // 3 plot 分の地面が途切れずつながる（plot 間に空白が入らない）。
        assert!(!ground.trim().contains("  "), "ground broke: {ground:?}");
    }

    #[test]
    fn a_merged_pr_gives_its_session_a_short_reduced_motion_safe_celebration() {
        let mut merged = session(
            STEADY_ID,
            "merged",
            SessionLifecycle::Available,
            AgentPhase::Ready,
        );
        merged.pr_merged = true;
        assert!(super::session_may_animate(&merged));
        let animated =
            plain(&render(24, 100, "x", &[merged.clone()], 1, false).unwrap()).join("\n");
        let reduced = plain(&render(24, 100, "x", &[merged], 1, true).unwrap()).join("\n");
        assert!(animated.contains("PR merged!"));
        assert!(animated.contains("^o^"));
        assert!(reduced.contains("^.^"));
    }
}
