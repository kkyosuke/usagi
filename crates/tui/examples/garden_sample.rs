use usagi_core::domain::id::SessionId;
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};
use usagi_tui::presentation::widgets::garden::{GardenSession, render};

fn main() {
    let sessions = [
        sample(
            "00000000-0000-4000-8000-000000000001",
            "session-auth",
            SessionLifecycle::Available,
            AgentPhase::Running,
        ),
        sample(
            "01000000-0000-4000-8000-000000000002",
            "issue-647",
            SessionLifecycle::Available,
            AgentPhase::Waiting,
        ),
        sample(
            "02000000-0000-4000-8000-000000000003",
            "coder",
            SessionLifecycle::Available,
            AgentPhase::Absent,
        ),
        sample(
            "03000000-0000-4000-8000-000000000004",
            "failed-build",
            SessionLifecycle::Failed,
            AgentPhase::Absent,
        ),
        sample(
            "04000000-0000-4000-8000-000000000005",
            "new-session",
            SessionLifecycle::Creating,
            AgentPhase::Absent,
        ),
        sample(
            "05000000-0000-4000-8000-000000000006",
            "cleanup",
            SessionLifecycle::Deleting,
            AgentPhase::Ended,
        ),
    ];
    scene("100x24 · 全 lifecycle", 24, 100, &sessions, 1, false);
    scene("100x24 · reduced motion", 24, 100, &sessions, 1, true);
    scene("100x24 · session 0 件", 24, 100, &[], 1, false);
    // 最小サイズでは plot が 2 列 1 行に減り、残りは session list へ畳まれる。
    scene(
        "64x14 · 最小サイズ（表示上限超過）",
        14,
        64,
        &sessions,
        1,
        false,
    );
}

fn scene(
    caption: &str,
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) {
    let frame = render(height, width, "my-project", sessions, tick, reduced_motion)
        .expect("the sample uses Garden-compatible terminal sizes");
    println!("--- {caption} ---");
    println!("{}\n", frame.rows.join("\n"));
}

fn sample(
    id: &str,
    label: &str,
    lifecycle: SessionLifecycle,
    agent_phase: AgentPhase,
) -> GardenSession {
    GardenSession {
        id: SessionId::parse(id).expect("sample IDs are canonical UUIDs"),
        label: label.to_owned(),
        lifecycle,
        agent_phase,
    }
}
