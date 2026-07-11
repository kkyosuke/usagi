//! Durable issue-orchestration state shared by the reconciler and file store.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamped<T> {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub written_at: DateTime<Utc>,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub owner: String,
    pub max_parallel: usize,
    pub nodes: BTreeMap<u64, Node>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub issue: u64,
    #[serde(default)]
    pub dependencies: Vec<u64>,
    pub state: NodeState,
    pub attempt: u32,
    pub generation: u64,
    pub lease: Option<Lease>,
    pub deadline: Option<DateTime<Utc>>,
    pub next_retry: Option<DateTime<Utc>>,
    pub worker: Option<String>,
    pub base: Option<Base>,
    pub pull_request: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Blocked,
    Runnable,
    Delegating,
    Running,
    PrOpen,
    ReviewWait,
    CiWait,
    CiFailed,
    RetryWait,
    MergeWait,
    Merged,
    Failed,
    Cancelled,
}

impl NodeState {
    pub fn occupies_worker(&self) -> bool {
        matches!(self, Self::Delegating | Self::Running)
    }

    pub fn terminal(&self) -> bool {
        matches!(self, Self::Merged | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub reference: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub merged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub issue: u64,
    pub plan: String,
    pub owner: String,
    pub generation: u64,
    pub lease: Lease,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub by_issue: BTreeMap<u64, Claim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub plan: String,
    pub issue: u64,
    pub generation: u64,
    pub kind: EventKind,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PrOpened,
    Succeeded,
    Failed,
    Interrupted,
    TimedOut,
}

impl Event {
    pub fn deterministic_id(plan: &str, issue: u64, generation: u64, kind: &EventKind) -> String {
        let kind = match kind {
            EventKind::PrOpened => "pr_opened",
            EventKind::Succeeded => "succeeded",
            EventKind::Failed => "failed",
            EventKind::Interrupted => "interrupted",
            EventKind::TimedOut => "timed_out",
        };
        format!("{plan}-{issue}-{generation}-{kind}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_state_classifiers_cover_active_and_terminal_states() {
        assert!(NodeState::Delegating.occupies_worker());
        assert!(NodeState::Running.occupies_worker());
        assert!(!NodeState::PrOpen.occupies_worker());
        for state in [NodeState::Merged, NodeState::Failed, NodeState::Cancelled] {
            assert!(state.terminal());
        }
        assert!(!NodeState::Running.terminal());
    }

    #[test]
    fn event_id_uses_the_serialized_kind() {
        for (kind, name) in [
            (EventKind::PrOpened, "pr_opened"),
            (EventKind::Succeeded, "succeeded"),
            (EventKind::Failed, "failed"),
            (EventKind::Interrupted, "interrupted"),
            (EventKind::TimedOut, "timed_out"),
        ] {
            assert_eq!(
                Event::deterministic_id("plan", 1, 2, &kind),
                format!("plan-1-2-{name}")
            );
        }
    }
}
