use usagi_core::domain::id::{AgentRuntimeId, SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};
use usagi_tui::presentation::widgets::garden::sidebar::{SessionDetails, ViewOptions, render};
use usagi_tui::presentation::widgets::garden::{GardenAgent, GardenSession};

fn main() {
    let sessions = sample_sessions();
    sidebar_scene(&sessions);
    scene("120x24 · restored meadow", 24, 120, &sessions, 1, false);
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
    for session in &mut open_projects {
        session.sidebar.project = Some((
            WorkspaceId::parse("00000000-0000-4000-8000-000000000099").unwrap(),
            "alpha".into(),
        ));
    }
    inactive.sidebar.project = Some((
        WorkspaceId::parse("00000000-0000-4000-8000-000000000098").unwrap(),
        "beta".into(),
    ));
    open_projects.push(inactive);
    scene_in_scope(
        "120x24 · 2 open projects",
        24,
        120,
        "2 open projects",
        &open_projects,
        (1, false),
    );
    let mut many = sample_sessions()[..1].to_vec();
    many[0].agents = (0..16)
        .map(|index| GardenAgent {
            runtime_id: AgentRuntimeId::parse(&format!("{index:08x}-0000-4000-8000-000000000003"))
                .expect("fixture id"),
            phase: AgentPhase::Running,
        })
        .collect();
    scene(
        "120x24 · all 16 Agents in one meadow",
        24,
        120,
        &many,
        41,
        false,
    );
    many[0].agents.extend((16..80).map(|index| {
        GardenAgent {
            runtime_id: AgentRuntimeId::parse(&format!("{index:08x}-0000-4000-8000-000000000003"))
                .expect("fixture id"),
            phase: AgentPhase::Waiting,
        }
    }));
    scene(
        "120x24 · 80 Agents with the landscape retained",
        24,
        120,
        &many,
        41,
        false,
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

fn sidebar_scene(sessions: &[GardenSession]) {
    let mut reference = sessions[..3].to_vec();
    for session in &mut reference {
        session.sidebar.project = Some((
            WorkspaceId::parse("00000000-0000-4000-8000-000000000099").unwrap(),
            "acme-web".into(),
        ));
    }
    reference[0].sidebar.name = "checkout-flow-v2".into();
    reference[0].sidebar.branch = "feature/checkout-flow-v2".into();
    reference[1].sidebar.name = "checkout-baseline".into();
    reference[1].sidebar.branch = "feature/checkout-baseline".into();
    reference[1].agents.clear();
    reference[2].sidebar.name = "Improve agent handoff summary".into();
    reference[2].sidebar.branch = "feature/agent-handoff-summary".into();
    reference[2].sidebar.project = Some((
        WorkspaceId::parse("00000000-0000-4000-8000-000000000098").unwrap(),
        "acme-internal".into(),
    ));
    reference[2].selected = true;
    reference[2].agents = (0..4)
        .map(|index| GardenAgent {
            runtime_id: AgentRuntimeId::parse(&format!("{index:08x}-0000-4000-8000-000000000088"))
                .unwrap(),
            phase: if index == 0 {
                AgentPhase::Running
            } else {
                AgentPhase::Ended
            },
        })
        .collect();
    for (index, session) in reference.iter_mut().enumerate() {
        session.selected = index == 2;
        session.label = session.sidebar.name.clone();
    }
    scene_in_scope(
        "160x32 · project/session sidebar",
        32,
        160,
        "2 open projects",
        &reference,
        (1, true),
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
    let frame = render(
        height,
        width,
        scope,
        sessions,
        ViewOptions {
            tick,
            reduced_motion,
            scroll: 0,
        },
    )
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
        sidebar: SessionDetails {
            name: label.into(),
            branch: format!("usagi/{label}"),
            project: None,
        },
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
        sidebar: SessionDetails {
            name: label.into(),
            branch: format!("usagi/{label}"),
            project: None,
        },
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
