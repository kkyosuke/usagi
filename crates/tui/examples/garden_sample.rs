use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};
use usagi_tui::presentation::widgets::garden::{GardenAgent, GardenSession, render};

fn main() {
    let sessions = sample_sessions();
    scene("120x24 · roomy Garden", 24, 120, &sessions, 1, false);
    scene("120x24 · reduced motion", 24, 120, &sessions, 1, true);
    scene("120x24 · session 0 件", 24, 120, &[], 1, false);
    let mut open_projects = sessions[..2].to_vec();
    "alpha / session-auth".clone_into(&mut open_projects[0].label);
    "alpha / issue-647".clone_into(&mut open_projects[1].label);
    let mut inactive = sample(
        "06000000-0000-4000-8000-000000000007",
        "beta / review-api",
        SessionLifecycle::Available,
        AgentPhase::Absent,
    );
    inactive.agents_observed = false;
    inactive.agents.clear();
    open_projects.push(inactive);
    scene_in_scope(
        "120x24 · 2 open projects",
        24,
        120,
        "2 open projects",
        &open_projects,
        (1, false),
    );
    // 64x14 terminal の先頭 1 行は project bar、残る 13 行へ全 Agent card が収まる。
    scene(
        "64x14 terminal · compact Garden",
        13,
        64,
        &sessions,
        1,
        false,
    );
}

fn sample_sessions() -> [GardenSession; 6] {
    [
        sample_agents(
            "00000000-0000-4000-8000-000000000001",
            "session-auth",
            SessionLifecycle::Available,
            &[
                ("10000000-0000-4000-8000-000000000001", AgentPhase::Running),
                ("11000000-0000-4000-8000-000000000002", AgentPhase::Running),
                ("12000000-0000-4000-8000-000000000003", AgentPhase::Waiting),
            ],
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
            AgentPhase::Ended,
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
    ]
}

fn scene(
    caption: &str,
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) {
    scene_in_scope(
        caption,
        height,
        width,
        "my-project",
        sessions,
        (tick, reduced_motion),
    );
}

fn scene_in_scope(
    caption: &str,
    height: usize,
    width: usize,
    scope: &str,
    sessions: &[GardenSession],
    animation: (u64, bool),
) {
    let (tick, reduced_motion) = animation;
    let frame = render(height, width, scope, sessions, tick, reduced_motion)
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
        selected: false,
        failure_summary: (lifecycle == SessionLifecycle::Failed)
            .then(|| "safe sample failure".to_owned()),
        agents_observed: true,
        pending_decisions: 0,
        pr_merged: false,
        agents: vec![GardenAgent {
            runtime_id: AgentRuntimeId::parse(id).expect("sample IDs are canonical UUIDs"),
            phase: agent_phase,
        }],
        agent_status: None,
    }
}

fn sample_agents(
    id: &str,
    label: &str,
    lifecycle: SessionLifecycle,
    agents: &[(&str, AgentPhase)],
) -> GardenSession {
    GardenSession {
        id: SessionId::parse(id).expect("sample IDs are canonical UUIDs"),
        label: label.to_owned(),
        lifecycle,
        selected: true,
        failure_summary: None,
        agents_observed: true,
        pending_decisions: 0,
        pr_merged: false,
        agents: agents
            .iter()
            .map(|(runtime_id, phase)| GardenAgent {
                runtime_id: AgentRuntimeId::parse(runtime_id)
                    .expect("sample runtime IDs are canonical UUIDs"),
                phase: *phase,
            })
            .collect(),
        agent_status: None,
    }
}
