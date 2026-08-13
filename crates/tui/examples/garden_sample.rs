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
    let frame = render(24, 100, "my-project", &sessions, 1, false)
        .expect("the sample uses a Garden-compatible terminal size");
    println!("{}", frame.rows.join("\n"));
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
