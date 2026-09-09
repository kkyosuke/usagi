use super::*;
use crate::presentation::widgets::strip_ansi;
use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

fn session(index: usize) -> GardenSession {
    GardenSession {
        sidebar: SessionDetails {
            project: Some((
                WorkspaceId::parse("00000000-0000-4000-8000-000000000001").unwrap(),
                "acme-web".into(),
            )),
            name: format!("session-{index}"),
            branch: format!("feature/session-{index}"),
        },
        id: SessionId::parse(&format!("{index:08x}-0000-4000-8000-000000000001")).unwrap(),
        label: format!("acme-web / session-{index}"),
        lifecycle: SessionLifecycle::Available,
        selected: false,
        failure_summary: None,
        agents_observed: true,
        agents: vec![super::super::GardenAgent {
            runtime_id: AgentRuntimeId::parse(&format!("{index:08x}-0000-4000-8000-000000000002"))
                .unwrap(),
            phase: AgentPhase::Running,
        }],
        agent_status: None,
        pending_decisions: 0,
        pr_merged: false,
    }
}

fn plain(view: &GardenView) -> String {
    view.rows
        .iter()
        .map(|row| strip_ansi(row))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn grouped_list_keeps_project_identity_and_exact_session_agent_targets() {
    let mut sessions = vec![session(0), session(1), session(2)];
    sessions[1].selected = true;
    sessions[2].sidebar.project.as_mut().unwrap().0 = WorkspaceId::new();
    let view = render(32, 160, "2 projects", &sessions, ViewOptions::default()).unwrap();
    let sidebar = view.sidebar.unwrap();
    let text = plain(&view);
    assert_eq!(
        text.matches("▱ acme-web").count(),
        2,
        "same labels do not merge different projects"
    );
    assert!(text.contains("feature/session-1"), "{text}");
    assert!(text.contains("╭ ● session-1"), "{text}");
    assert!(text.contains("running  00000001"), "{text}");
    for session in &sessions {
        let targets = view
            .hitboxes
            .iter()
            .filter(|hitbox| hitbox.column == sidebar.column && hitbox.session_id == session.id)
            .collect::<Vec<_>>();
        assert!(targets.iter().any(|target| target.agent.is_none()));
        let agent = targets
            .iter()
            .find(|target| target.agent == Some(session.agents[0].runtime_id))
            .unwrap();
        assert!(strip_ansi(&view.rows[agent.row]).contains("running"));
        assert!(agent.contains(sidebar.column + sidebar.width - 1, agent.row));
    }
}

#[test]
fn scrolling_reaches_every_target_without_moving_any_rabbit() {
    let sessions = (0..20).map(session).collect::<Vec<_>>();
    let first = render(24, 120, "repo", &sessions, ViewOptions::default()).unwrap();
    let viewport = first.sidebar.unwrap();
    let mut visited = std::collections::BTreeSet::new();
    let rabbits = first
        .hitboxes
        .iter()
        .filter(|h| h.column < viewport.column)
        .copied()
        .collect::<Vec<_>>();
    for scroll in 0..=viewport.max_scroll {
        let frame = render(
            24,
            120,
            "repo",
            &sessions,
            ViewOptions {
                scroll,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            frame
                .hitboxes
                .iter()
                .filter(|h| h.column < viewport.column)
                .copied()
                .collect::<Vec<_>>(),
            rabbits
        );
        for target in frame
            .hitboxes
            .iter()
            .filter(|h| h.column == viewport.column)
        {
            visited.insert((target.session_id, target.agent));
            assert!((CONTENT_TOP..23).contains(&target.row));
        }
    }
    for session in &sessions {
        assert!(visited.contains(&(session.id, None)));
        assert!(visited.contains(&(session.id, Some(session.agents[0].runtime_id))));
    }
    let last = render(
        24,
        120,
        "repo",
        &sessions,
        ViewOptions {
            scroll: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(last.sidebar.unwrap().scroll, viewport.max_scroll);
    assert!(!strip_ansi(&last.rows[23]).contains("Next"));
    let shrunk = render(
        24,
        120,
        "repo",
        &sessions[..1],
        ViewOptions {
            scroll: viewport.max_scroll,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(shrunk.sidebar.unwrap().scroll, 0);
}

#[test]
fn sidebar_breakpoint_preserves_small_garden_and_clips_cjk_names() {
    let mut value = session(0);
    value.sidebar.name = "長い日本語のセッション名".repeat(20);
    value.sidebar.branch = "feature/日本語".repeat(20);
    value.sidebar.project.as_mut().unwrap().1 = "非常に長いプロジェクト".repeat(20);
    for width in [64, 98, 99, 100, 120, 160, 240] {
        let view = render(
            24,
            width,
            "repo",
            std::slice::from_ref(&value),
            ViewOptions::default(),
        )
        .unwrap();
        assert_eq!(view.rows.len(), 24);
        assert!(
            view.rows
                .iter()
                .all(|row| crate::presentation::widgets::display_width(row) == width)
        );
        assert_eq!(view.sidebar.is_some(), width >= 99);
        if width < 99 {
            assert_eq!(
                view.frame,
                super::super::render(24, width, "repo", std::slice::from_ref(&value), 0, false)
                    .unwrap()
            );
        }
    }
    assert!(render(12, 160, "repo", &[], ViewOptions::default()).is_none());
    assert!(render(24, 63, "repo", &[], ViewOptions::default()).is_none());
}

#[test]
fn inactive_empty_and_dispatch_states_are_explicit() {
    let mut value = session(0);
    value.agents.clear();
    value.agents_observed = false;
    value.sidebar.branch.clear();
    value.sidebar.name.clear();
    value.sidebar.project = None;
    let inactive = render(
        24,
        120,
        "fallback-project",
        &[value.clone()],
        ViewOptions::default(),
    )
    .unwrap();
    assert!(plain(&inactive).contains("project inactive"));
    assert!(plain(&inactive).contains("▱ fallback-project"));
    value.agents_observed = true;
    assert!(
        plain(&render(24, 120, "repo", &[value.clone()], ViewOptions::default()).unwrap())
            .contains("No agent activity.")
    );
    value.agents = session(0).agents;
    value.agent_status = Some(usagi_core::domain::agent::AgentStatus::Idle);
    assert!(
        plain(&render(24, 120, "repo", &[value.clone()], ViewOptions::default()).unwrap())
            .contains("completed  00000000")
    );
    value.agent_status = Some(usagi_core::domain::agent::AgentStatus::Failed);
    assert!(
        plain(&render(24, 120, "repo", &[value], ViewOptions::default()).unwrap())
            .contains("failed  00000000")
    );
    assert!(
        plain(&render(24, 120, "repo", &[], ViewOptions::default()).unwrap())
            .contains("No sessions")
    );
}

#[test]
fn list_uses_attention_order_and_reduced_motion_is_deterministic() {
    let mut value = session(0);
    let mut waiting = session(1).agents[0];
    waiting.phase = AgentPhase::Waiting;
    value.agents.push(waiting);
    let first = render(
        24,
        120,
        "repo",
        &[value.clone()],
        ViewOptions {
            reduced_motion: true,
            ..Default::default()
        },
    )
    .unwrap();
    value.agents.reverse();
    let next = render(
        24,
        120,
        "repo",
        &[value],
        ViewOptions {
            tick: 77,
            reduced_motion: true,
            scroll: 0,
        },
    )
    .unwrap();
    assert_eq!(first, next);
    let text = plain(&first);
    assert!(text.find("waiting  00000001").unwrap() < text.find("running  00000000").unwrap());
    assert!(text.contains("2 agents"));
}
