//! Session Garden の純粋な描画サンプル。
//!
//! daemon の状態を所有せず、表示用に閉じた [`GardenSession`] を固定 plot に並べる。
//! frame と同じ layout から [`GardenHitbox`] も返すため、後続実装は座標から session
//! identity を再計算せず click target を解決できる。

use usagi_core::domain::id::SessionId;
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

/// Garden に渡す、表示に必要な session 情報だけの projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GardenSession {
    pub id: SessionId,
    pub label: String,
    pub lifecycle: SessionLifecycle,
    pub agent_phase: AgentPhase,
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
    let grid_width = columns * PLOT_WIDTH;
    let grid_left = SIDE_PADDING + content_width.saturating_sub(grid_width) / 2;

    let mut rows = Vec::with_capacity(height);
    rows.push(header_line(width, workspace_name, sessions));
    rows.push(Role::Feature.style().paint(&"·".repeat(width)));
    rows.push(" ".repeat(width));

    let mut hitboxes = Vec::with_capacity(visible);
    for plot_row in 0..plot_rows {
        let start = plot_row * columns;
        if start >= visible {
            break;
        }
        let end = (start + columns).min(visible);
        let plots = sessions[start..end]
            .iter()
            .map(|session| plot(session, tick, reduced_motion))
            .collect::<Vec<_>>();
        for local_row in 0..PLOT_HEIGHT {
            let mut line = " ".repeat(grid_left);
            for plot in &plots {
                line.push_str(&pad_to_width(&plot[local_row], PLOT_WIDTH));
            }
            rows.push(pad_to_width(&line, width));
        }
        for (column, session) in sessions[start..end].iter().enumerate() {
            hitboxes.push(GardenHitbox {
                session_id: session.id,
                column: grid_left + column * PLOT_WIDTH,
                row: HEADER_ROWS + plot_row * PLOT_HEIGHT,
                width: PLOT_WIDTH,
                height: PLOT_HEIGHT,
            });
        }
    }

    if visible == 0 {
        let empty_row = HEADER_ROWS + garden_height / 2;
        rows.resize_with(empty_row, || " ".repeat(width));
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
        .filter(|session| {
            session.lifecycle == SessionLifecycle::Available
                && session.agent_phase == AgentPhase::Running
        })
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

fn plot(session: &GardenSession, tick: u64, reduced_motion: bool) -> [String; PLOT_HEIGHT] {
    let phase = if reduced_motion {
        0
    } else {
        (tick + phase_offset(session.id)) % 6
    };
    let (status, status_style, rabbit_style, rabbit) = appearance(session, phase);
    let label = Role::Accent
        .style()
        .bold()
        .paint(&clip_to_width(&session.label, PLOT_WIDTH - 2));
    let ground = Style::new().dim().paint("--v---------v-----------");
    [
        centered(PLOT_WIDTH, &label),
        centered(PLOT_WIDTH, &status_style.paint(status)),
        centered(PLOT_WIDTH, &rabbit_style.paint(rabbit[0])),
        centered(PLOT_WIDTH, &rabbit_style.paint(rabbit[1])),
        centered(PLOT_WIDTH, &rabbit_style.paint(rabbit[2])),
        centered(PLOT_WIDTH, &rabbit_style.paint(rabbit[3])),
        centered(PLOT_WIDTH, &ground),
    ]
}

fn appearance(
    session: &GardenSession,
    phase: u64,
) -> (&'static str, Style, Style, [&'static str; 4]) {
    let feature = Role::Feature.style().bold();
    match session.lifecycle {
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
        SessionLifecycle::Available => available_appearance(session.agent_phase, phase, feature),
    }
}

fn available_appearance(
    agent_phase: AgentPhase,
    phase: u64,
    feature: Style,
) -> (&'static str, Style, Style, [&'static str; 4]) {
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

fn phase_offset(id: SessionId) -> u64 {
    u64::from_str_radix(&id.as_str()[..2], 16).unwrap_or(0) % 6
}

fn centered(width: usize, value: &str) -> String {
    let value = clip_to_width(value, width);
    let padding = width.saturating_sub(display_width(&value)) / 2;
    pad_to_width(&format!("{}{value}", " ".repeat(padding)), width)
}

#[cfg(test)]
mod tests {
    use super::{GardenSession, MIN_HEIGHT, MIN_WIDTH, render};
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::id::SessionId;
    use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

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
            agent_phase: phase,
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
}
