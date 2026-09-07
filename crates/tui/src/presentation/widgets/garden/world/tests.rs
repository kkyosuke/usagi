use super::*;
use crate::presentation::widgets::{garden::GardenAgent, strip_ansi};
use std::collections::BTreeSet;
use usagi_core::domain::id::{AgentRuntimeId, SessionId};

fn sessions(count: usize) -> Vec<GardenSession> {
    (0..count)
        .map(|index| GardenSession {
            id: SessionId::parse(&format!("{index:08x}-0000-4000-8000-000000000001")).unwrap(),
            label: format!("project / 日本語-{index}"),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            agents: vec![GardenAgent {
                runtime_id: AgentRuntimeId::parse(&format!(
                    "{index:08x}-0000-4000-8000-000000000002"
                ))
                .unwrap(),
                phase: AgentPhase::Running,
            }],
            agent_status: None,
            pending_decisions: 0,
            pr_merged: false,
        })
        .collect()
}

fn text(frame: &GardenFrame) -> String {
    frame
        .rows
        .iter()
        .map(|row| strip_ansi(row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_frame(frame: &GardenFrame, height: usize, width: usize, sessions: &[GardenSession]) {
    assert_eq!(frame.rows.len(), height);
    assert!(frame.rows.iter().all(|row| display_width(row) == width));
    let expected = sessions
        .iter()
        .filter(|session| session.agents_observed)
        .flat_map(|session| {
            session
                .agents
                .iter()
                .map(|agent| (session.id, agent.runtime_id))
        })
        .collect::<BTreeSet<_>>();
    let actual = frame
        .hitboxes
        .iter()
        .filter_map(|hitbox| hitbox.agent.map(|agent| (hitbox.session_id, agent)))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        frame
            .hitboxes
            .iter()
            .filter(|hitbox| hitbox.agent.is_some())
            .count(),
        expected.len()
    );
    for (index, hitbox) in frame.hitboxes.iter().enumerate() {
        assert!(hitbox.width > 0 && hitbox.height > 0);
        assert!(
            hitbox.column >= SIDE_PADDING && hitbox.column + hitbox.width <= width - SIDE_PADDING
        );
        assert!(hitbox.row >= HEADER_ROWS && hitbox.row + hitbox.height <= height - FOOTER_ROWS);
        // Every target can be selected without another rabbit/home intercepting it.
        for other in &frame.hitboxes[index + 1..] {
            assert!(
                hitbox.column + hitbox.width <= other.column
                    || other.column + other.width <= hitbox.column
                    || hitbox.row + hitbox.height <= other.row
                    || other.row + other.height <= hitbox.row,
                "overlapping targets: {hitbox:?} and {other:?}"
            );
        }
        if hitbox.agent.is_some() {
            let visible = frame.rows[hitbox.row..hitbox.row + hitbox.height]
                .iter()
                .map(|row| strip_ansi(row))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                visible.contains("/)/)")
                    || visible.contains("/)(/")
                    || visible.contains("(\\(\\")
                    || visible.contains('兎')
            );
        }
    }
}

#[test]
fn every_runtime_fits_and_remains_clickable_across_sizes_and_densities() {
    for (height, width) in [(18, 80), (24, 100), (24, 120), (40, 160)] {
        for count in [0, 1, 6, 16, 37, 80, 128] {
            let sessions = sessions(count);
            for tick in [0, 17, 61, 99, 299] {
                let frame = render(height, width, "atlas", &sessions, tick, false);
                assert_frame(&frame, height, width, &sessions);
                let text = text(&frame);
                for feature in ["~~~~~~~~", "Y  v  Y", "&&&"] {
                    assert!(text.contains(feature), "{text}");
                }
                assert!(!text.contains("scroll") && !text.contains("more"));
            }
        }
    }
}

#[test]
fn a_full_activity_cycle_preserves_all_agents_including_more_than_six_in_one_home() {
    let mut sessions = sessions(16);
    let agents = sessions
        .iter()
        .flat_map(|session| session.agents.clone())
        .collect();
    sessions.truncate(1);
    sessions[0].agents = agents;
    let mut positions = BTreeSet::new();
    for tick in 0..ANIMATION_CYCLE_TICKS {
        let frame = render(24, 120, "atlas", &sessions, tick, false);
        assert_frame(&frame, 24, 120, &sessions);
        let first = frame.hitboxes[0];
        positions.insert((first.column, first.row));
    }
    assert!(positions.len() > 8);
    let frame = render(24, 120, "atlas", &sessions, 0, true);
    assert_eq!(frame, render(24, 120, "atlas", &sessions, 217, true));
}

#[test]
fn lifecycle_and_dispatch_overrides_keep_runtime_identity_and_safe_home_status() {
    let mut fixtures = sessions(1);
    let phases = [
        AgentPhase::Absent,
        AgentPhase::Ready,
        AgentPhase::Running,
        AgentPhase::Waiting,
        AgentPhase::Interrupted,
        AgentPhase::Sleeping,
        AgentPhase::Ended,
        AgentPhase::Exited,
    ];
    for lifecycle in [
        SessionLifecycle::Available,
        SessionLifecycle::Creating,
        SessionLifecycle::Initializing,
        SessionLifecycle::Deleting,
        SessionLifecycle::Failed,
    ] {
        fixtures[0].lifecycle = lifecycle;
        for status in [
            None,
            Some(DispatchAgentStatus::Starting),
            Some(DispatchAgentStatus::Running),
            Some(DispatchAgentStatus::Idle),
            Some(DispatchAgentStatus::Exited),
            Some(DispatchAgentStatus::Failed),
        ] {
            fixtures[0].agent_status = status;
            for phase in phases {
                fixtures[0].agents[0].phase = phase;
                for reduced in [false, true] {
                    let first = render(24, 120, "atlas", &fixtures, 0, reduced);
                    assert_frame(&first, 24, 120, &fixtures);
                    if reduced {
                        assert_eq!(first, render(24, 120, "atlas", &fixtures, 63, true));
                    }
                }
            }
        }
    }
    fixtures[0].failure_summary = Some("safe failure".to_owned());
    assert!(text(&render(24, 120, "atlas", &fixtures, 0, false)).contains("failed · safe failure"));
    fixtures[0].pending_decisions = 2;
    assert!(text(&render(24, 120, "atlas", &fixtures, 0, false)).contains("action · 2 decisions"));
    fixtures[0].pending_decisions = 1;
    let single_decision = text(&render(24, 120, "atlas", &fixtures, 0, false));
    assert!(single_decision.contains("action · 1 decision"));
    assert!(!single_decision.contains("1 decisions"));
    fixtures[0].pending_decisions = 0;
    fixtures[0].lifecycle = SessionLifecycle::Available;
    fixtures[0].pr_merged = true;
    fixtures[0].agent_status = None;
    for tick in [0, 1] {
        assert!(text(&render(24, 120, "atlas", &fixtures, tick, false)).contains("PR merged!"));
    }
    fixtures[0].pr_merged = false;
    fixtures[0].agents_observed = false;
    let frame = render(24, 120, "atlas", &fixtures, 0, false);
    assert_frame(&frame, 24, 120, &fixtures);
    assert!(text(&frame).contains("project inactive"));
    fixtures[0].agents_observed = true;
    fixtures[0].agents.clear();
    assert!(text(&render(24, 120, "atlas", &fixtures, 0, false)).contains("No agent activity."));
}

#[test]
fn unobserved_homes_keep_cached_lifecycles_and_do_not_revive_stale_runtime_state() {
    let mut fixtures = sessions(1);
    fixtures[0].agents_observed = false;
    fixtures[0].failure_summary = Some("old snapshot".to_owned());
    for (lifecycle, expected) in [
        (SessionLifecycle::Available, "project inactive"),
        (SessionLifecycle::Creating, "cached · creating"),
        (SessionLifecycle::Initializing, "cached · creating"),
        (SessionLifecycle::Deleting, "cached · deleting"),
        (SessionLifecycle::Failed, "cached · failed"),
    ] {
        fixtures[0].lifecycle = lifecycle;
        for status in [
            None,
            Some(DispatchAgentStatus::Starting),
            Some(DispatchAgentStatus::Failed),
        ] {
            fixtures[0].agent_status = status;
            for tick in [0, 63] {
                let frame = super::super::render(24, 120, "atlas", &fixtures, tick, false)
                    .expect("shared meadow fits");
                assert_frame(&frame, 24, 120, &fixtures);
                let text = text(&frame);
                assert!(text.contains(expected), "{text}");
                assert!(!text.contains("old snapshot"), "{text}");
                assert!(!text.contains("starting"), "{text}");
                assert_eq!(home_status(&fixtures[0]).1, Style::new().dim());
            }
        }
    }
}

#[test]
fn clocks_route_and_fold_only_identical_frames() {
    assert!(fits(18, 80));
    assert!(!fits(17, 80));
    assert!(!fits(18, 79));
    for count in [0, 1, 80] {
        let sessions = sessions(count);
        for tick in 0..ANIMATION_CYCLE_TICKS {
            let canonical = canonical_tick(24, 120, &sessions, tick, false);
            assert_eq!(
                render(24, 120, "atlas", &sessions, canonical, false),
                render(24, 120, "atlas", &sessions, tick, false)
            );
        }
        assert_eq!(canonical_tick(24, 120, &sessions, 91, true), 0);
    }
}

#[test]
fn canvas_clips_invalid_cells_and_repairs_wide_glyph_overwrites() {
    let mut canvas = Canvas::new(8, 2);
    let style = Role::Info.style();
    for (x, y, ch) in [
        (-1, 0, 'x'),
        (8, 0, 'x'),
        (0, -1, 'x'),
        (0, 2, 'x'),
        (7, 0, '兎'),
        (0, 0, '\u{301}'),
    ] {
        canvas.put(x, y, ch, style);
    }
    canvas.text(0, 0, "兎兎", style);
    canvas.put(1, 0, 'x', style);
    canvas.put(2, 0, 'y', style);
    canvas.put_if_empty(1, 0, 'z', style);
    canvas.put_if_empty(-1, 0, 'z', style);
    canvas.put_if_empty(0, -1, 'z', style);
    canvas.put_if_empty(0, 2, 'z', style);
    canvas.text(4, 0, "\u{301}兎", style);
    canvas.put(6, 1, '兎', style);
    canvas.clear_cell(1, 6);
    let rows = canvas.rows();
    assert_eq!(strip_ansi(&rows[0]), "   xy 兎    ");
    assert!(rows.iter().all(|row| display_width(row) == 12));
}

#[test]
fn sprites_keep_the_original_walk_directions_and_activity_illustrations() {
    let places = Places {
        home: Point { x: 0, y: 0 },
        water: Point { x: 20, y: 5 },
        food: Point { x: 40, y: 0 },
        shade: Point { x: 60, y: 5 },
    };
    assert_eq!(lifestyle_motion(places, 5).facing, Facing::Right);
    assert_eq!(lifestyle_motion(places, 85).facing, Facing::Left);
    for (tick, expected) in [
        (15, Activity::Drinking),
        (45, Activity::Eating),
        (70, Activity::Sleeping),
    ] {
        assert_eq!(lifestyle_motion(places, tick).activity, expected);
    }
    for activity in [
        Activity::Walking,
        Activity::Drinking,
        Activity::Eating,
        Activity::Sleeping,
        Activity::Waiting,
        Activity::Interrupted,
        Activity::Working,
        Activity::Celebrating,
    ] {
        for facing in [Facing::Left, Facing::Right] {
            for tick in 0..6 {
                let sprite = rabbit_sprite(
                    Motion {
                        point: places.home,
                        facing,
                        activity,
                    },
                    tick,
                );
                assert!(sprite.iter().all(|row| display_width(row) <= 9));
                assert!(
                    sprite.join("\n").contains("/)/)")
                        || sprite.join("\n").contains("/)(/")
                        || sprite.join("\n").contains("(\\(\\")
                );
            }
        }
    }
}

#[test]
#[should_panic(expected = "lifestyle tick is reduced modulo its cycle")]
fn lifestyle_rejects_a_tick_outside_its_cycle() {
    lifestyle_motion(
        roaming_places(
            Area {
                x: 0,
                y: 0,
                width: 10,
                height: 5,
            },
            (9, 4),
        ),
        LIFESTYLE_CYCLE_TICKS,
    );
}

#[test]
fn crowded_homes_never_displace_agents_and_physical_limits_do_not_overlap_targets() {
    for count in [300, 418, 450] {
        let fixtures = sessions(count);
        let frame = render(18, 80, "atlas", &fixtures, 19, false);
        assert_frame(&frame, 18, 80, &fixtures);
        if count <= 418 {
            assert!(text(&frame).contains("~~~~~~~~"));
        }
    }
    let mut fixtures = sessions(700);
    for fixture in &mut fixtures[..699] {
        fixture.agents.clear();
    }
    let frame = render(24, 120, "atlas", &fixtures, 0, false);
    assert_frame(&frame, 24, 120, &fixtures);
}
