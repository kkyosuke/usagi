//! Session Garden の純粋な描画サンプル。
//!
//! daemon の状態を所有せず、表示用に閉じた [`GardenSession`] を固定 plot に並べる。
//! frame と同じ layout から [`GardenHitbox`] も返すため、後続実装は座標から session
//! identity を再計算せず click target を解決できる。

use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

use crate::presentation::theme::{Role, Style};

use super::{clip_to_width, display_width, pad_to_width};

/// Garden を表示できる最小端末幅。
pub const MIN_WIDTH: usize = 64;
/// Garden を表示できる最小端末高さ。
pub const MIN_HEIGHT: usize = 14;

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
const MAX_VISIBLE_AGENTS: usize = PLOT_WIDTH / COMPACT_RABBIT_WIDTH;

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
    pub agents: Vec<GardenAgent>,
}

/// Garden に描く 1 agent。runtime identity は並び順と animation offset だけに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GardenAgent {
    pub runtime_id: AgentRuntimeId,
    pub phase: AgentPhase,
}

/// 0-based terminal cell rectangle。右端・下端は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GardenHitbox {
    pub session_id: SessionId,
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

/// Garden の描画結果と、同じ layout から得た click target。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GardenFrame {
    pub rows: Vec<String>,
    pub hitboxes: Vec<GardenHitbox>,
    /// 端末に収まらず描画しなかった session 数。
    pub hidden_sessions: usize,
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
    if height < MIN_HEIGHT || width < MIN_WIDTH {
        return None;
    }

    let content_width = width.saturating_sub(SIDE_PADDING * 2);
    let columns = (content_width / PLOT_WIDTH).max(1);
    let garden_height = height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let plot_rows = (garden_height / PLOT_HEIGHT).max(1);
    let capacity = columns.saturating_mul(plot_rows);
    let visible = sessions.len().min(capacity);
    let hidden_sessions = sessions.len().saturating_sub(visible);

    let mut rows = Vec::with_capacity(height);
    rows.push(header_line(width, workspace_name, sessions));
    rows.push(Role::Feature.style().paint(&"·".repeat(width)));
    rows.push(" ".repeat(width));

    // 使う plot 行数だけを縦中央へ寄せ、庭の下側だけが大きく空くのを避ける。
    let used_rows = visible.div_ceil(columns);
    let grid_top = HEADER_ROWS + garden_height.saturating_sub(used_rows * PLOT_HEIGHT) / 2;
    rows.resize_with(grid_top, || " ".repeat(width));

    let mut hitboxes = Vec::with_capacity(visible);
    for plot_row in 0..used_rows {
        let start = plot_row * columns;
        let end = (start + columns).min(visible);
        // 埋まった列数ではなく、その行に実際に並ぶ数で中央へ寄せる。容量ぶんの幅で
        // 中央寄せすると、session が列数に満たない行が左へ寄って庭が偏る。
        let row_left = SIDE_PADDING + content_width.saturating_sub((end - start) * PLOT_WIDTH) / 2;
        let plots = sessions[start..end]
            .iter()
            .map(|session| plot(session, tick, reduced_motion))
            .collect::<Vec<_>>();
        for local_row in 0..PLOT_CONTENT_ROWS {
            let mut line = " ".repeat(row_left);
            for plot in &plots {
                line.push_str(&pad_to_width(&plot[local_row], PLOT_WIDTH));
            }
            rows.push(pad_to_width(&line, width));
        }
        // 地面は plot の下だけでなく庭の幅いっぱいに敷く。うさぎの数で地面が途切れると
        // 中央の島のように見えるため。
        rows.push(ground_row(width, content_width));
        for (column, session) in sessions[start..end].iter().enumerate() {
            hitboxes.push(GardenHitbox {
                session_id: session.id,
                column: row_left + column * PLOT_WIDTH,
                row: grid_top + plot_row * PLOT_HEIGHT,
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
    let overflow = if hidden_sessions == 0 {
        String::new()
    } else {
        Role::Warning
            .style()
            .paint(&format!("+ {hidden_sessions} more in session list"))
    };
    rows.push(centered(width, &overflow));
    rows.push(centered(
        width,
        &Style::new()
            .dim()
            .paint("Garden · click a usagi to visit · any key to return"),
    ));

    Some(GardenFrame {
        rows,
        hitboxes,
        hidden_sessions,
    })
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

fn plot(session: &GardenSession, tick: u64, reduced_motion: bool) -> [String; PLOT_CONTENT_ROWS] {
    let label = Role::Accent
        .style()
        .bold()
        .paint(&clip_to_width(&session.label, PLOT_WIDTH - 2));
    let [status, ears, head, body, feet] = match session.lifecycle {
        SessionLifecycle::Available => available_plot(session, tick, reduced_motion),
        lifecycle => lifecycle_plot(lifecycle),
    };
    [centered(PLOT_WIDTH, &label), status, ears, head, body, feet]
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

fn lifecycle_plot(lifecycle: SessionLifecycle) -> [String; PLOT_CONTENT_ROWS - 1] {
    let feature = Role::Feature.style().bold();
    let (status, status_style, rabbit_style, rabbit) = match lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => (
            "growing",
            Role::Warning.style(),
            feature,
            ["", "", "  /)/)", "__(_ _)__"],
        ),
        SessionLifecycle::Deleting => (
            "heading home",
            Style::new().dim(),
            Style::new().dim(),
            ["", " /)/)", "( . .)", "c(\")(\")"],
        ),
        SessionLifecycle::Failed => (
            "failed",
            Role::Danger.style().bold(),
            Role::Danger.style(),
            ["", " /)/)", "( x.x)", "c(\")(\")"],
        ),
        SessionLifecycle::Available => unreachable!("available sessions use agent projection"),
    };
    let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
    [
        centered(PLOT_WIDTH, &status_style.paint(status)),
        ears,
        head,
        body,
        feet,
    ]
}

fn available_plot(
    session: &GardenSession,
    tick: u64,
    reduced_motion: bool,
) -> [String; PLOT_CONTENT_ROWS - 1] {
    let agents = ordered_agents(&session.agents);
    if agents.is_empty() {
        return [
            centered(PLOT_WIDTH, &Style::new().dim().paint("no agents")),
            " ".repeat(PLOT_WIDTH),
            " ".repeat(PLOT_WIDTH),
            " ".repeat(PLOT_WIDTH),
            " ".repeat(PLOT_WIDTH),
        ];
    }

    if agents.len() == 1 {
        let agent = agents[0];
        let phase = animation_phase(tick, reduced_motion, &agent.runtime_id.as_str());
        let (status, status_style, rabbit_style, rabbit) = agent_appearance(agent.phase, phase);
        let [ears, head, body, feet] = sprite(rabbit, rabbit_style, PLOT_WIDTH);
        return [
            centered(PLOT_WIDTH, &status_style.paint(status)),
            ears,
            head,
            body,
            feet,
        ];
    }

    let hidden = agents.len().saturating_sub(MAX_VISIBLE_AGENTS);
    let visible = &agents[..agents.len().min(MAX_VISIBLE_AGENTS)];
    let status = agent_summary(&agents, hidden);
    let mut rows: [String; SPRITE_ROWS] = std::array::from_fn(|_| String::new());
    for agent in visible {
        let phase = animation_phase(tick, reduced_motion, &agent.runtime_id.as_str());
        let (_, _, style, rabbit) = agent_appearance(agent.phase, phase);
        let compact = sprite(rabbit, style, COMPACT_RABBIT_WIDTH);
        for (row, part) in rows.iter_mut().zip(compact) {
            row.push_str(&part);
        }
    }
    let [ears, head, body, feet] = rows.map(|row| centered(PLOT_WIDTH, &row));
    [
        centered(PLOT_WIDTH, &Style::new().dim().paint(&status)),
        ears,
        head,
        body,
        feet,
    ]
}

fn ordered_agents(agents: &[GardenAgent]) -> Vec<GardenAgent> {
    let mut ordered = agents.to_vec();
    ordered.sort_by_key(|agent| (agent.phase != AgentPhase::Waiting, agent.runtime_id));
    ordered
}

fn agent_appearance(
    agent_phase: AgentPhase,
    phase: u64,
) -> (&'static str, Style, Style, [&'static str; 4]) {
    let feature = Role::Feature.style().bold();
    match agent_phase {
        AgentPhase::Running => {
            let rabbit = match phase % 3 {
                0 => ["", " /)/)", "( o.o)", " / > <"],
                1 => [" /)/)", "( o.o)", " / > <", ""],
                _ => ["", "  /)/)", "_( o.o)_", "  > ^ <"],
            };
            ("running", Role::Success.style().bold(), feature, rabbit)
        }
        AgentPhase::Waiting => (
            "waiting",
            Role::Warning.style().bold(),
            feature,
            ["", " /)/)", "( o.o)?", "c(\")(\")"],
        ),
        AgentPhase::Interrupted => (
            "interrupted",
            Role::Warning.style(),
            feature,
            ["", " /)/)", "( -.-)!", "c(\")(\")"],
        ),
        AgentPhase::Absent | AgentPhase::Ready | AgentPhase::Ended | AgentPhase::Exited => {
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

fn agent_summary(agents: &[GardenAgent], hidden: usize) -> String {
    let count = |matches: fn(AgentPhase) -> bool| {
        agents.iter().filter(|agent| matches(agent.phase)).count()
    };
    let parts = [
        (count(|phase| phase == AgentPhase::Running), "run"),
        (count(|phase| phase == AgentPhase::Waiting), "wait"),
        (count(|phase| phase == AgentPhase::Ready), "ready"),
        (
            count(|phase| matches!(phase, AgentPhase::Ended | AgentPhase::Exited)),
            "done",
        ),
        (count(|phase| phase == AgentPhase::Interrupted), "int"),
        (count(|phase| phase == AgentPhase::Absent), "idle"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>();
    let summary = parts.join(" · ");
    if hidden == 0 {
        summary
    } else {
        let suffix = format!(" · +{hidden}");
        let prefix_width = PLOT_WIDTH.saturating_sub(display_width(&suffix));
        format!("{}{suffix}", clip_to_width(&summary, prefix_width))
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
    use super::{GROUND, GardenAgent, GardenSession, MIN_HEIGHT, MIN_WIDTH, PLOT_WIDTH, render};
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::id::{AgentRuntimeId, SessionId};
    use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

    /// animation offset が 0 になる id（先頭 2 桁が `00`）。tick をそのまま phase として扱える。
    const STEADY_ID: &str = "00000000-0000-4000-8000-000000000001";

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
        let frame = render(
            24,
            100,
            "x",
            &[session(STEADY_ID, "one", lifecycle, phase)],
            tick,
            false,
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
        assert_eq!(frame.hitboxes.len(), 4);
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
    }

    #[test]
    fn an_empty_garden_has_a_calm_explicit_message() {
        let frame = render(24, 100, "my-project", &[], 0, false).expect("garden fits");
        assert!(frame.rows.join("\n").contains("No sessions in the garden"));
        assert!(frame.hitboxes.is_empty());
        assert_eq!(frame.hidden_sessions, 0);
    }

    #[test]
    fn overflow_is_reported_and_deterministic() {
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
        let first = render(14, 64, "x", &sessions, 3, false).expect("garden fits");
        let second = render(14, 64, "x", &sessions, 3, false).expect("garden fits");
        assert_eq!(first, second);
        assert!(first.hidden_sessions > 0);
        assert!(first.rows.join("\n").contains("more in session list"));
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
        let ordered = super::ordered_agents(&shuffled);
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
            agents,
        };
        let first = render(24, 100, "x", &[make_session(shuffled)], 2, false).expect("fits");
        let second = render(24, 100, "x", &[make_session(reversed)], 2, false).expect("fits");
        assert_eq!(first, second);
        let text = plain(&first).join("\n");
        assert!(text.contains("2 run · 1 wait"));
        assert!(text.contains("+2"));
        assert!(
            text.contains("( o.o)?"),
            "the waiting agent must stay visible"
        );
        assert_eq!(text.matches("/)/)").count(), super::MAX_VISIBLE_AGENTS);
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
            (SessionLifecycle::Available, AgentPhase::Ended, "available"),
            (SessionLifecycle::Available, AgentPhase::Exited, "available"),
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
    fn a_running_usagi_cycles_three_poses_and_an_idle_one_blinks() {
        // phase % 3 が 0 / 1 / 2 の 3 pose すべてを踏む（offset 0 の id なので tick = phase）。
        let poses = (0..3)
            .map(|tick| only(SessionLifecycle::Available, AgentPhase::Running, tick).join("\n"))
            .collect::<Vec<_>>();
        assert_ne!(poses[0], poses[1]);
        assert_ne!(poses[1], poses[2]);
        assert_ne!(poses[0], poses[2]);

        // idle は phase 4 でだけ瞬きする。
        let open = only(SessionLifecycle::Available, AgentPhase::Ready, 0).join("\n");
        let blink = only(SessionLifecycle::Available, AgentPhase::Ready, 4).join("\n");
        assert!(open.contains("( . .)"));
        assert!(blink.contains("( -.-)"));
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
        assert_eq!(frame.hitboxes.len(), 2);
        let left = frame.hitboxes[0].column;
        let right_gap = 145 - (frame.hitboxes[1].column + frame.hitboxes[1].width);
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
}
