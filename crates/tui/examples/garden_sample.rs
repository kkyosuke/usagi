use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};
use usagi_tui::presentation::widgets::garden::{GardenAgent, GardenSession, render_page};

fn main() {
    let sessions = [
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
        "100x24 · 2 open projects",
        24,
        100,
        "2 open projects",
        &open_projects,
        0,
        (1, false),
    );
    // 64x14 terminal の先頭 1 行は project bar、残る 13 行では 2 plot ずつ
    // page に分かれる。先頭と末尾を出し、全 session へ到達できることを眺める。
    scene_page(
        "64x14 terminal · Garden page 1/3",
        13,
        64,
        &sessions,
        0,
        1,
        false,
    );
    scene_page(
        "64x14 terminal · Garden page 3/3",
        13,
        64,
        &sessions,
        2,
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
    scene_page(caption, height, width, sessions, 0, tick, reduced_motion);
}

fn scene_page(
    caption: &str,
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    page: usize,
    tick: u64,
    reduced_motion: bool,
) {
    scene_in_scope(
        caption,
        height,
        width,
        "my-project",
        sessions,
        page,
        (tick, reduced_motion),
    );
}

fn scene_in_scope(
    caption: &str,
    height: usize,
    width: usize,
    scope: &str,
    sessions: &[GardenSession],
    page: usize,
    animation: (u64, bool),
) {
    let (tick, reduced_motion) = animation;
    let frame = render_page(height, width, scope, sessions, page, tick, reduced_motion)
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
        pr_merged: false,
        agents: vec![GardenAgent {
            runtime_id: AgentRuntimeId::parse(id).expect("sample IDs are canonical UUIDs"),
            phase: agent_phase,
        }],
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
        pr_merged: false,
        agents: agents
            .iter()
            .map(|(runtime_id, phase)| GardenAgent {
                runtime_id: AgentRuntimeId::parse(runtime_id)
                    .expect("sample runtime IDs are canonical UUIDs"),
                phase: *phase,
            })
            .collect(),
    }
}
