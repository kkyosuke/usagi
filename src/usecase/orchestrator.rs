//! Pure reconciliation of a durable orchestration plan.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use crate::domain::orchestrator::{Base, Event, EventKind, Lease, NodeState, Plan, PullRequest};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    pub observed_at: DateTime<Utc>,
    pub sessions: BTreeMap<u64, SessionObservation>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionObservation {
    pub worker: Option<String>,
    pub base: Option<Base>,
    pub pull_request: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Delegate {
        id: String,
        issue: u64,
        generation: u64,
        base: Base,
    },
    Reobserve {
        id: String,
        issue: u64,
        worker: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    pub plan: Plan,
    pub actions: Vec<Action>,
    /// Event ids safe to acknowledge after `plan` has been durably committed.
    pub acknowledgements: Vec<String>,
}

pub fn work_ready(plan: &Plan, issue: u64) -> Option<Base> {
    let node = plan.nodes.get(&issue)?;
    if node.dependencies.is_empty() {
        return Some(Base {
            reference: "main".into(),
            commit: "main".into(),
        });
    }
    let mut unmerged = Vec::new();
    for dependency in &node.dependencies {
        let dependency = plan.nodes.get(dependency)?;
        if dependency.state == NodeState::Merged {
            continue;
        }
        let pr = dependency.pull_request.as_ref()?;
        unmerged.push(pr);
    }
    match unmerged.as_slice() {
        [] => Some(Base {
            reference: "main".into(),
            commit: "main".into(),
        }),
        [pr] => Some(Base {
            reference: pr.head.clone(),
            commit: pr.head.clone(),
        }),
        _ => None,
    }
}

pub fn reconcile(
    plan: &Plan,
    observation: &Observation,
    lease_duration: Duration,
) -> ReconcileResult {
    let mut next = plan.clone();
    let mut actions = Vec::new();
    let mut acknowledgements = Vec::new();

    for event in &observation.events {
        if event.plan == next.id {
            // Unknown and stale generations are intentionally acknowledged too:
            // they cannot affect this plan now or in a future generation.
            acknowledgements.push(event.id.clone());
        }
        let Some(node) = next.nodes.get_mut(&event.issue) else {
            continue;
        };
        if event.plan != next.id || event.generation != node.generation {
            continue;
        }
        match event.kind {
            EventKind::PrOpened => node.state = NodeState::PrOpen,
            EventKind::Succeeded => node.state = NodeState::MergeWait,
            EventKind::Failed | EventKind::Interrupted | EventKind::TimedOut => {
                node.state = NodeState::RetryWait;
                node.next_retry = Some(observation.observed_at + Duration::seconds(30));
            }
        }
    }

    for (&issue, seen) in &observation.sessions {
        let Some(node) = next.nodes.get_mut(&issue) else {
            continue;
        };
        if let Some(pr) = &seen.pull_request {
            node.pull_request = Some(pr.clone());
            node.state = if pr.merged {
                NodeState::Merged
            } else {
                NodeState::PrOpen
            };
        } else if node.state == NodeState::Delegating && seen.worker.is_some() {
            node.worker.clone_from(&seen.worker);
            node.base.clone_from(&seen.base);
            node.state = NodeState::Running;
        }
    }

    // An expired lease is never reclaimed from time alone. A fresh session/PR
    // observation first moves it to runnable; delegation happens on a later tick.
    let mut reclaimed = BTreeSet::new();
    for (&issue, node) in &mut next.nodes {
        let expired = node
            .lease
            .as_ref()
            .is_some_and(|l| l.expires_at <= observation.observed_at);
        if !expired || !matches!(node.state, NodeState::Delegating | NodeState::Running) {
            continue;
        }
        match observation.sessions.get(&issue) {
            None => actions.push(Action::Reobserve {
                id: format!("{}-{issue}-{}-reobserve", next.id, node.generation),
                issue,
                worker: node.worker.clone(),
            }),
            Some(seen) if seen.worker.is_none() && seen.pull_request.is_none() => {
                node.state = NodeState::Runnable;
                node.lease = None;
                node.worker = None;
                reclaimed.insert(issue);
            }
            Some(_) => {}
        }
    }

    let mut capacity = next.max_parallel.saturating_sub(
        next.nodes
            .values()
            .filter(|n| n.state.occupies_worker())
            .count(),
    );
    let runnable: Vec<u64> = next
        .nodes
        .iter()
        .filter_map(|(&issue, node)| {
            let retry_due = node.state == NodeState::RetryWait
                && node
                    .next_retry
                    .is_some_and(|at| at <= observation.observed_at);
            ((node.state == NodeState::Runnable || retry_due) && !reclaimed.contains(&issue))
                .then_some(issue)
        })
        .collect();
    for issue in runnable {
        if capacity == 0 {
            break;
        }
        let Some(base) = work_ready(&next, issue) else {
            next.nodes.get_mut(&issue).unwrap().state = NodeState::Blocked;
            continue;
        };
        let node = next.nodes.get_mut(&issue).unwrap();
        node.generation += 1;
        node.attempt += 1;
        node.state = NodeState::Delegating;
        node.base = Some(base.clone());
        node.lease = Some(Lease {
            owner: next.owner.clone(),
            expires_at: observation.observed_at + lease_duration,
        });
        node.next_retry = None;
        actions.push(Action::Delegate {
            id: format!("{}-{issue}-{}-delegate", next.id, node.generation),
            issue,
            generation: node.generation,
            base,
        });
        capacity -= 1;
    }
    ReconcileResult {
        plan: next,
        actions,
        acknowledgements,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orchestrator::Node;

    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn node(issue: u64, state: NodeState) -> Node {
        Node {
            issue,
            dependencies: vec![],
            state,
            attempt: 0,
            generation: 0,
            lease: None,
            deadline: None,
            next_retry: None,
            worker: None,
            base: None,
            pull_request: None,
        }
    }
    fn plan(node: Node) -> Plan {
        Plan {
            id: "p".into(),
            owner: "owner".into(),
            max_parallel: 1,
            nodes: [(node.issue, node)].into(),
        }
    }

    #[test]
    fn same_snapshot_does_not_delegate_twice() {
        let observation = Observation {
            observed_at: now(),
            ..Default::default()
        };
        let first = reconcile(
            &plan(node(1, NodeState::Runnable)),
            &observation,
            Duration::minutes(5),
        );
        assert!(matches!(
            first.actions.as_slice(),
            [Action::Delegate { .. }]
        ));
        let second = reconcile(&first.plan, &observation, Duration::minutes(5));
        assert!(second.actions.is_empty());
    }

    #[test]
    fn crash_after_session_creation_converges_to_running() {
        let first = reconcile(
            &plan(node(1, NodeState::Runnable)),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        let seen = SessionObservation {
            worker: Some("issue-1".into()),
            base: first.plan.nodes[&1].base.clone(),
            pull_request: None,
        };
        let result = reconcile(
            &first.plan,
            &Observation {
                observed_at: now(),
                sessions: [(1, seen)].into(),
                events: vec![],
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::Running);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn stale_lease_requires_observation_and_a_later_tick() {
        let mut n = node(1, NodeState::Delegating);
        n.generation = 1;
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now(),
        });
        let missing = reconcile(
            &plan(n.clone()),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert!(matches!(
            missing.actions.as_slice(),
            [Action::Reobserve { .. }]
        ));
        let absent = SessionObservation::default();
        let recovered = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                sessions: [(1, absent)].into(),
                events: vec![],
            },
            Duration::minutes(5),
        );
        assert_eq!(recovered.plan.nodes[&1].state, NodeState::Runnable);
        assert!(recovered.actions.is_empty());
    }

    #[test]
    fn join_is_not_work_ready_until_dependencies_merge() {
        let mut p = plan(node(3, NodeState::Runnable));
        p.nodes.get_mut(&3).unwrap().dependencies = vec![1, 2];
        for issue in [1, 2] {
            let mut dependency = node(issue, NodeState::PrOpen);
            dependency.pull_request = Some(PullRequest {
                number: issue,
                url: format!("pr/{issue}"),
                head: format!("head-{issue}"),
                merged: false,
            });
            p.nodes.insert(issue, dependency);
        }
        assert_eq!(work_ready(&p, 3), None);
    }

    #[test]
    fn stale_generation_event_is_ignored() {
        let mut n = node(1, NodeState::Running);
        n.generation = 2;
        let event = Event {
            id: "old".into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            kind: EventKind::Succeeded,
            terminal_revision: 0,
            observed_at: now(),
        };
        let result = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                sessions: BTreeMap::new(),
                events: vec![
                    event,
                    Event {
                        id: "unknown-issue".into(),
                        plan: "p".into(),
                        issue: 99,
                        generation: 0,
                        kind: EventKind::Succeeded,
                        terminal_revision: 0,
                        observed_at: now(),
                    },
                ],
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::Running);
    }

    #[test]
    fn work_ready_uses_main_or_one_dependency_head() {
        let mut p = plan(node(2, NodeState::Runnable));
        p.nodes.get_mut(&2).unwrap().dependencies = vec![1];
        p.nodes.insert(1, node(1, NodeState::Merged));
        assert_eq!(work_ready(&p, 2).unwrap().reference, "main");

        {
            let dependency = p.nodes.get_mut(&1).unwrap();
            dependency.state = NodeState::PrOpen;
            dependency.pull_request = Some(PullRequest {
                number: 1,
                url: "pr/1".into(),
                head: "abc".into(),
                merged: false,
            });
        }
        assert_eq!(work_ready(&p, 2).unwrap().reference, "abc");
        p.nodes.get_mut(&1).unwrap().pull_request = None;
        assert_eq!(work_ready(&p, 2), None);
        assert_eq!(work_ready(&p, 99), None);
    }

    #[test]
    fn current_events_drive_each_lifecycle_transition() {
        for (kind, expected) in [
            (EventKind::PrOpened, NodeState::PrOpen),
            (EventKind::Succeeded, NodeState::MergeWait),
            (EventKind::Failed, NodeState::RetryWait),
            (EventKind::Interrupted, NodeState::RetryWait),
            (EventKind::TimedOut, NodeState::RetryWait),
        ] {
            let mut n = node(1, NodeState::Running);
            n.generation = 2;
            let event = Event {
                id: "event".into(),
                plan: "p".into(),
                issue: 1,
                generation: 2,
                kind,
                terminal_revision: 0,
                observed_at: now(),
            };
            let result = reconcile(
                &plan(n),
                &Observation {
                    observed_at: now(),
                    sessions: BTreeMap::new(),
                    events: vec![event],
                },
                Duration::minutes(5),
            );
            assert_eq!(result.plan.nodes[&1].state, expected);
        }
    }

    #[test]
    fn unknown_events_and_sessions_are_ignored() {
        let event = Event {
            id: "other".into(),
            plan: "other-plan".into(),
            issue: 1,
            generation: 0,
            kind: EventKind::Succeeded,
            terminal_revision: 0,
            observed_at: now(),
        };
        let result = reconcile(
            &plan(node(1, NodeState::Running)),
            &Observation {
                observed_at: now(),
                sessions: [(99, SessionObservation::default())].into(),
                events: vec![event],
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::Running);
    }

    #[test]
    fn observed_pr_wins_and_records_merged_or_open() {
        for merged in [false, true] {
            let pr = PullRequest {
                number: 1,
                url: "pr/1".into(),
                head: "abc".into(),
                merged,
            };
            let result = reconcile(
                &plan(node(1, NodeState::Running)),
                &Observation {
                    observed_at: now(),
                    sessions: [(
                        1,
                        SessionObservation {
                            worker: None,
                            base: None,
                            pull_request: Some(pr.clone()),
                        },
                    )]
                    .into(),
                    events: vec![],
                },
                Duration::minutes(5),
            );
            assert_eq!(result.plan.nodes[&1].pull_request, Some(pr));
            assert_eq!(
                result.plan.nodes[&1].state,
                if merged {
                    NodeState::Merged
                } else {
                    NodeState::PrOpen
                }
            );
        }
    }

    #[test]
    fn live_session_prevents_stale_lease_recovery() {
        let mut n = node(1, NodeState::Running);
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now(),
        });
        let result = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                sessions: [(
                    1,
                    SessionObservation {
                        worker: Some("issue-1".into()),
                        ..Default::default()
                    },
                )]
                .into(),
                events: vec![],
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::Running);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn due_retry_delegates_but_capacity_and_blocked_dependencies_wait() {
        let mut retry = node(1, NodeState::RetryWait);
        retry.next_retry = Some(now());
        let retried = reconcile(
            &plan(retry),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert!(matches!(
            retried.actions.as_slice(),
            [Action::Delegate { .. }]
        ));

        let mut full = plan(node(1, NodeState::Running));
        full.nodes.insert(2, node(2, NodeState::Runnable));
        let waiting = reconcile(
            &full,
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert!(waiting.actions.is_empty());

        let mut blocked = plan(node(2, NodeState::Runnable));
        blocked.nodes.get_mut(&2).unwrap().dependencies = vec![99];
        let result = reconcile(
            &blocked,
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&2].state, NodeState::Blocked);
    }
}
