//! Durable issue-orchestration reconciliation and dispatch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use crate::domain::agent_phase::AgentPhase;
use crate::domain::orchestrator::{
    Base, Claim, Event, EventKind, Lease, NodeState, Plan, PullRequest,
};
use crate::domain::workspace_state::{SessionAgent, SessionOrigin};
use crate::infrastructure::git;
use crate::infrastructure::orchestrator_event::{self, WorkerBinding};
use crate::infrastructure::orchestrator_store::{ClaimOutcome, OrchestratorStore};
use crate::infrastructure::{agent_prompt_store, error_log, issue_store::IssueStore};
use crate::usecase::{issue, session};

const LEASE_DURATION: Duration = Duration::minutes(5);

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickOutcome {
    pub plans: usize,
    pub delegated: usize,
    pub owner_queued: usize,
    pub acknowledgements: usize,
    /// Claims that rejected this tick's delegate actions. No worker/session or
    /// prompt side effect has happened for these actions.
    pub busy: Vec<Claim>,
}

/// A dependency base that cannot safely be used for worker creation.
#[derive(Debug)]
enum DependencyBaseError {
    RepositoryCount(usize),
    Fetch {
        commit: String,
        source: anyhow::Error,
    },
    Missing(String),
    Moved {
        reference: String,
        expected: String,
        actual: String,
    },
    CheckoutMismatch {
        worktree: PathBuf,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for DependencyBaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryCount(count) => write!(
                formatter,
                "dependency base requires exactly one source repository, found {count}"
            ),
            Self::Fetch { commit, source } => {
                write!(
                    formatter,
                    "failed to fetch dependency base {commit}: {source:#}"
                )
            }
            Self::Missing(commit) => write!(
                formatter,
                "dependency base commit {commit} is unavailable after fetch"
            ),
            Self::Moved {
                reference,
                expected,
                actual,
            } => write!(
                formatter,
                "dependency base {reference} moved from {expected} to {actual}"
            ),
            Self::CheckoutMismatch {
                worktree,
                expected,
                actual,
            } => write!(
                formatter,
                "worker worktree {} checked out {actual}, expected {expected}",
                worktree.display()
            ),
        }
    }
}

impl std::error::Error for DependencyBaseError {}

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

    for node in next.nodes.values_mut() {
        let timed_out = node
            .deadline
            .is_some_and(|deadline| deadline <= observation.observed_at);
        if timed_out && matches!(node.state, NodeState::Delegating | NodeState::Running) {
            node.state = NodeState::RetryWait;
            node.lease = None;
            node.next_retry = Some(observation.observed_at + Duration::seconds(30));
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

pub fn reconcile_workspace_tick(
    workspace: &Path,
    global_slots_remaining: usize,
    now: DateTime<Utc>,
) -> Result<TickOutcome> {
    let store = OrchestratorStore::new(workspace);
    let mut outcome = TickOutcome::default();
    let mut slots_remaining = global_slots_remaining;
    for plan_id in store.plan_ids()? {
        let stamped = store
            .load_plan(&plan_id)?
            .context("orchestrator plan disappeared during tick")?;
        outcome.plans += 1;
        let mut observation = observe(workspace, &stamped.value, now)?;
        let plan_active = stamped
            .value
            .nodes
            .values()
            .filter(|node| node.state.occupies_worker())
            .count();
        let plan_remaining = stamped.value.max_parallel.saturating_sub(plan_active);
        let allowed_new = plan_remaining.min(slots_remaining);
        let mut capacity_plan = stamped.value.clone();
        capacity_plan.max_parallel = plan_active + allowed_new;
        let mut result = reconcile(&capacity_plan, &observation, LEASE_DURATION);
        result.plan.max_parallel = stamped.value.max_parallel;

        let reobserved = reobserve_absent(&result.actions);
        if !reobserved.is_empty() {
            observation.sessions.extend(reobserved);
            result = reconcile(&result.plan, &observation, LEASE_DURATION);
        }

        let saved = store.save_plan(&result.plan, Some(stamped.revision), now)?;
        release_reconciled_claims(&store, &stamped.value, &result.plan, &observation, now)?;
        for event_id in &result.acknowledgements {
            if store.acknowledge_event(&result.plan.id, event_id)? {
                outcome.acknowledgements += 1;
            }
        }
        let (actions, busy) =
            admit_delegate_actions(workspace, &store, &mut result.plan, result.actions, now)?;
        result.actions = actions;
        outcome.busy.extend(busy);
        let resolution_blocked =
            resolve_delegate_bases(workspace, &mut result.plan, &mut result.actions);
        for issue in resolution_blocked {
            release_plan_claim(&store, &result.plan, issue, now)?;
        }
        let mut saved_revision = saved.revision;
        if result.plan != saved.value {
            saved_revision = store
                .save_plan(&result.plan, Some(saved_revision), now)?
                .revision;
        }
        let owner_worktree = owner_worktree(workspace, &result.plan.owner);
        if owner_needs_wakeup(workspace, &result.plan, &result.actions)? {
            queue_owner_prompt(&owner_worktree, &result.plan, &result.actions)?;
            outcome.owner_queued += 1;
        }
        let (delegated, blocked, failed) = dispatch_actions(
            workspace,
            &owner_worktree,
            &result.plan,
            result.actions,
            &store,
            now,
        )?;
        if !blocked.is_empty() || !failed.is_empty() {
            for issue in blocked {
                block_delegation(&mut result.plan, issue);
            }
            for issue in failed {
                retry_delegation(&mut result.plan, issue, now);
            }
            store.save_plan(&result.plan, Some(saved_revision), now)?;
        }
        outcome.delegated += delegated;
        slots_remaining = slots_remaining.saturating_sub(delegated);
        if slots_remaining == 0 {
            continue;
        }
    }
    Ok(outcome)
}

fn release_reconciled_claims(
    store: &OrchestratorStore,
    previous: &Plan,
    next: &Plan,
    observation: &Observation,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut releases = BTreeSet::new();
    for event in &observation.events {
        if event.plan == next.id
            && !matches!(event.kind, EventKind::PrOpened)
            && previous
                .nodes
                .get(&event.issue)
                .is_some_and(|node| node.generation == event.generation)
        {
            releases.insert((event.issue, event.generation));
        }
    }
    for (&issue, node) in &next.nodes {
        let timed_out = previous
            .nodes
            .get(&issue)
            .is_some_and(|old| old.state.occupies_worker() && node.state == NodeState::RetryWait);
        if node.state.terminal() || timed_out {
            releases.insert((issue, node.generation));
        }
    }
    for (issue, generation) in releases {
        store.release_claim(issue, &next.id, &next.owner, generation, now)?;
    }
    Ok(())
}

fn admit_delegate_actions(
    workspace: &Path,
    store: &OrchestratorStore,
    plan: &mut Plan,
    actions: Vec<Action>,
    now: DateTime<Utc>,
) -> Result<(Vec<Action>, Vec<Claim>)> {
    let mut admitted = Vec::with_capacity(actions.len());
    let mut busy = Vec::new();
    for action in actions {
        let Action::Delegate {
            issue, generation, ..
        } = &action
        else {
            admitted.push(action);
            continue;
        };
        let lease = plan.nodes[issue]
            .lease
            .clone()
            .context("delegate action has no lease")?;
        let claim = Claim {
            workspace: store.workspace_key().into(),
            issue: *issue,
            plan: plan.id.clone(),
            owner: plan.owner.clone(),
            generation: *generation,
            lease,
        };
        let current = store.load_claims()?.value.by_issue.get(issue).cloned();
        let absent = match current.as_ref() {
            Some(current) if current.lease.expires_at <= now => {
                claim_owner_absent(workspace, store, current)?.then_some(current)
            }
            _ => None,
        };
        match store.claim(claim, now, absent)? {
            ClaimOutcome::Acquired => admitted.push(action),
            ClaimOutcome::Busy(current) => {
                let node = plan.nodes.get_mut(issue).unwrap();
                node.state = NodeState::Runnable;
                node.lease = None;
                node.worker = None;
                busy.push(current);
            }
        }
    }
    Ok((admitted, busy))
}

fn claim_owner_absent(workspace: &Path, store: &OrchestratorStore, claim: &Claim) -> Result<bool> {
    for status in session::statuses(workspace)? {
        if orchestrator_event::binding(&status.root)?.is_some_and(|binding| {
            binding.plan == claim.plan
                && binding.issue == claim.issue
                && binding.generation == claim.generation
        }) {
            return Ok(false);
        }
    }
    let has_pr = store
        .load_plan(&claim.plan)?
        .and_then(|stamped| stamped.value.nodes.get(&claim.issue).cloned())
        .is_some_and(|node| {
            node.generation == claim.generation && node.pull_request.is_some_and(|pr| !pr.merged)
        });
    Ok(!has_pr)
}

fn resolve_delegate_bases(
    workspace: &Path,
    plan: &mut Plan,
    actions: &mut Vec<Action>,
) -> Vec<u64> {
    let mut blocked = Vec::new();
    actions.retain_mut(|action| {
        let Action::Delegate { issue, base, .. } = action else {
            return true;
        };
        match resolve_dependency_base(workspace, base) {
            Ok(resolved) => {
                *base = resolved.clone();
                plan.nodes.get_mut(issue).unwrap().base = Some(resolved);
                true
            }
            Err(error) => {
                error_log::ErrorLog::record(&format!(
                    "orchestrator {} blocked issue #{}: {error}",
                    plan.id, issue
                ));
                block_delegation(plan, *issue);
                blocked.push(*issue);
                false
            }
        }
    });
    blocked
}

fn resolve_dependency_base(
    workspace: &Path,
    base: &Base,
) -> std::result::Result<Base, DependencyBaseError> {
    let repositories = session::source_repositories(workspace);
    let [repo] = repositories.as_slice() else {
        return Err(DependencyBaseError::RepositoryCount(repositories.len()));
    };

    let is_default_base = base.reference == "main" && base.commit == "main";
    let revision = if is_default_base {
        session::configured_base_ref(repo).unwrap_or_else(|| base.commit.clone())
    } else {
        base.commit.clone()
    };
    let expected = match git::resolve_commit(repo, &revision) {
        Some(commit) => commit,
        None => {
            git::fetch(repo).map_err(|source| DependencyBaseError::Fetch {
                commit: revision.clone(),
                source,
            })?;
            git::resolve_commit(repo, &revision)
                .ok_or_else(|| DependencyBaseError::Missing(revision.clone()))?
        }
    };
    if !is_default_base {
        let actual = git::resolve_commit(repo, &base.reference)
            .ok_or_else(|| DependencyBaseError::Missing(base.reference.clone()))?;
        if actual != expected {
            return Err(DependencyBaseError::Moved {
                reference: base.reference.clone(),
                expected,
                actual,
            });
        }
    }
    Ok(Base {
        reference: base.reference.clone(),
        commit: expected,
    })
}

fn block_delegation(plan: &mut Plan, issue: u64) {
    if let Some(node) = plan.nodes.get_mut(&issue) {
        node.state = NodeState::Blocked;
        node.lease = None;
        node.worker = None;
    }
}

fn retry_delegation(plan: &mut Plan, issue: u64, now: DateTime<Utc>) {
    if let Some(node) = plan.nodes.get_mut(&issue) {
        node.state = NodeState::RetryWait;
        node.lease = None;
        node.worker = None;
        node.next_retry = Some(now + Duration::seconds(30));
    }
}

fn release_plan_claim(
    store: &OrchestratorStore,
    plan: &Plan,
    issue: u64,
    now: DateTime<Utc>,
) -> Result<()> {
    if let Some(node) = plan.nodes.get(&issue) {
        store.release_claim(issue, &plan.id, &plan.owner, node.generation, now)?;
    }
    Ok(())
}

fn observe(workspace: &Path, plan: &Plan, now: DateTime<Utc>) -> Result<Observation> {
    let mut observations = BTreeMap::new();
    for status in session::statuses(workspace)? {
        let Some(binding) = orchestrator_event::binding(&status.root)? else {
            continue;
        };
        if binding.plan != plan.id {
            continue;
        }
        let Some(node) = plan.nodes.get(&binding.issue) else {
            continue;
        };
        if binding.generation != node.generation {
            continue;
        }
        let merged = status.worktrees.iter().all(|worktree| worktree.merged);
        let pull_request = node.pull_request.as_ref().map(|pr| PullRequest {
            merged,
            ..pr.clone()
        });
        observations.insert(
            binding.issue,
            SessionObservation {
                worker: Some(status.name),
                base: node.base.clone(),
                pull_request,
            },
        );
    }
    Ok(Observation {
        observed_at: now,
        sessions: observations,
        events: OrchestratorStore::new(workspace).load_events(&plan.id)?,
    })
}

fn reobserve_absent(actions: &[Action]) -> BTreeMap<u64, SessionObservation> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Reobserve { issue, .. } => Some((*issue, SessionObservation::default())),
            Action::Delegate { .. } => None,
        })
        .collect()
}

fn dispatch_actions(
    workspace: &Path,
    owner_worktree: &Path,
    plan: &Plan,
    actions: Vec<Action>,
    store: &OrchestratorStore,
    now: DateTime<Utc>,
) -> Result<(usize, Vec<u64>, Vec<u64>)> {
    let mut delegated = 0;
    let mut blocked = Vec::new();
    let mut failed = Vec::new();
    for action in actions {
        let Action::Delegate {
            issue,
            generation,
            base,
            ..
        } = action
        else {
            continue;
        };
        match delegate_worker(workspace, owner_worktree, plan, issue, generation, &base) {
            Ok(()) => delegated += 1,
            Err(error) => {
                if error.downcast_ref::<DependencyBaseError>().is_some() {
                    blocked.push(issue);
                } else {
                    failed.push(issue);
                }
                release_plan_claim(store, plan, issue, now)?;
                error_log::ErrorLog::record(&format!(
                    "orchestrator {} failed to delegate issue #{}: {error:#}",
                    plan.id, issue
                ));
            }
        }
    }
    Ok((delegated, blocked, failed))
}

fn delegate_worker(
    workspace: &Path,
    owner_worktree: &Path,
    plan: &Plan,
    issue_number: u64,
    generation: u64,
    base: &Base,
) -> Result<()> {
    let name = worker_session_name(&plan.owner, issue_number);
    let created = match session::create_with_agent_at_base(
        workspace,
        &name,
        SessionAgent::default(),
        SessionOrigin::Mcp,
        Some(plan.owner.clone()),
        &base.commit,
    ) {
        Ok(created) => created,
        Err(error) if error.to_string().contains("already exists") => {
            let existing = session::list(workspace)?
                .into_iter()
                .find(|session| session.name == name)
                .context(format!(
                    "session \"{name}\" already exists but is not recorded"
                ))?;
            session::CreatedSession {
                name: existing.name,
                root: existing.root,
                worktrees: existing
                    .worktrees
                    .into_iter()
                    .map(|worktree| worktree.path)
                    .collect(),
            }
        }
        Err(error) => return Err(error),
    };
    for worktree in &created.worktrees {
        let actual = git::worktree_status(worktree)
            .map(|status| status.head)
            .unwrap_or_default();
        if actual != base.commit {
            return Err(DependencyBaseError::CheckoutMismatch {
                worktree: worktree.clone(),
                expected: base.commit.clone(),
                actual,
            }
            .into());
        }
    }
    let binding = WorkerBinding {
        workspace: workspace.to_path_buf(),
        plan: plan.id.clone(),
        issue: issue_number,
        generation,
        owner_worktree: owner_worktree.to_path_buf(),
    };
    orchestrator_event::register(&created.root, &binding)?;
    let issue = IssueStore::new(workspace)
        .read(issue_number as u32)?
        .context(format!("no issue #{issue_number}"))?;
    let prompt = format!(
        "{}\n\n## Orchestrator context\n\n- plan: {}\n- worker generation: {}\n- base reference: {}\n- base commit: {}\n- owner session: {}\n\nDo not create sub-sessions. Work only in this session and report via the normal PR/completion flow.\n",
        issue::to_prompt(&issue),
        plan.id,
        generation,
        base.reference,
        base.commit,
        plan.owner
    );
    agent_prompt_store::set(&created.root, &prompt)?;
    Ok(())
}

fn owner_needs_wakeup(workspace: &Path, plan: &Plan, actions: &[Action]) -> Result<bool> {
    if actions.is_empty() {
        return Ok(false);
    }
    let owner = owner_worktree(workspace, &plan.owner);
    let mut owner_phase = None;
    for status in session::statuses(workspace)? {
        if status.name == plan.owner {
            owner_phase = status.agent_phase;
            break;
        }
    }
    let ended_or_absent = match owner_phase {
        Some(phase) => matches!(phase, AgentPhase::Ended | AgentPhase::Exited),
        None => true,
    };
    Ok(ended_or_absent && !owner.as_os_str().is_empty())
}

fn queue_owner_prompt(owner_worktree: &Path, plan: &Plan, actions: &[Action]) -> Result<()> {
    let mut lines = vec![format!(
        "Orchestrator plan {} has {} pending action(s). Reconcile the durable plan and inspect worker progress.",
        plan.id,
        actions.len()
    )];
    for action in actions {
        match action {
            Action::Delegate {
                issue, generation, ..
            } => lines.push(format!(
                "- delegate issue #{issue}, generation {generation}"
            )),
            Action::Reobserve { issue, worker, .. } => lines.push(format!(
                "- reobserve issue #{issue} ({})",
                worker.as_deref().unwrap_or("no worker")
            )),
        }
    }
    agent_prompt_store::set(owner_worktree, &lines.join("\n"))?;
    Ok(())
}

fn owner_worktree(workspace: &Path, owner: &str) -> PathBuf {
    if owner == ":root" || owner == "root" {
        return workspace.to_path_buf();
    }
    session::list(workspace)
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|session| session.name == owner)
                .map(|session| session.root)
        })
        .unwrap_or_else(|| workspace.join(".usagi").join("sessions").join(owner))
}

fn worker_session_name(owner: &str, issue: u64) -> String {
    let owner = owner
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("{owner}-issue-{issue}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orchestrator::Node;
    use crate::domain::workspace_state::BranchStatus;
    use crate::infrastructure::workspace_store::WorkspaceStore;
    use std::process::Command;
    use std::time::{Duration as StdDuration, Instant};

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
    fn durable_claim(store: &OrchestratorStore, plan: &Plan, issue: u64) -> Claim {
        let node = &plan.nodes[&issue];
        Claim {
            workspace: store.workspace_key().into(),
            issue,
            plan: plan.id.clone(),
            owner: plan.owner.clone(),
            generation: node.generation,
            lease: node.lease.clone().unwrap(),
        }
    }
    fn git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .unwrap()
            .success());
    }
    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("README.md"), "root\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }
    fn immutable_head_base(dir: &Path) -> Base {
        let commit = git_output(dir, &["rev-parse", "HEAD"]);
        Base {
            reference: commit.clone(),
            commit,
        }
    }
    fn set_worktree_status(root: &Path, name: &str, status: BranchStatus) {
        let store = WorkspaceStore::new(root);
        let mut state = store.load().unwrap().unwrap();
        let session = state.sessions.iter_mut().find(|s| s.name == name).unwrap();
        session.worktrees[0].status = status;
        store.save(&state).unwrap();
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
    fn live_lease_and_future_deadline_keep_the_worker_slot() {
        let mut n = node(1, NodeState::Running);
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now() + Duration::minutes(1),
        });
        n.deadline = Some(now() + Duration::minutes(2));

        let result = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );

        assert_eq!(result.plan.nodes[&1].state, NodeState::Running);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn future_retry_and_missing_worker_observation_do_not_dispatch() {
        let mut retry = node(1, NodeState::RetryWait);
        retry.next_retry = Some(now() + Duration::seconds(1));
        let result = reconcile(
            &plan(retry),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::RetryWait);
        assert!(result.actions.is_empty());

        let mut delegating = node(2, NodeState::Delegating);
        delegating.generation = 1;
        let result = reconcile(
            &plan(delegating),
            &Observation {
                observed_at: now(),
                sessions: [(2, SessionObservation::default())].into(),
                events: vec![],
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&2].state, NodeState::Delegating);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn expired_lease_and_deadline_do_not_move_inactive_states() {
        let mut n = node(1, NodeState::PrOpen);
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now(),
        });
        n.deadline = Some(now());

        let result = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );

        assert_eq!(result.plan.nodes[&1].state, NodeState::PrOpen);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn pr_observation_prevents_stale_lease_recovery() {
        let mut n = node(1, NodeState::Running);
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now(),
        });
        let pr = PullRequest {
            number: 1,
            url: "pr/1".into(),
            head: "head-1".into(),
            merged: false,
        };

        let result = reconcile(
            &plan(n),
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

        assert_eq!(result.plan.nodes[&1].state, NodeState::PrOpen);
        assert_eq!(result.plan.nodes[&1].pull_request, Some(pr));
        assert!(result.actions.is_empty());
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
    fn work_ready_covers_dependency_shapes() {
        let mut p = plan(node(4, NodeState::Runnable));
        p.nodes.get_mut(&4).unwrap().dependencies = vec![1, 2, 3];

        for issue in [1, 2, 3] {
            p.nodes.insert(issue, node(issue, NodeState::Merged));
        }
        let base = work_ready(&p, 4).unwrap();
        assert_eq!(base.reference, "main");
        assert_eq!(base.commit, "main");

        {
            let dependency = p.nodes.get_mut(&2).unwrap();
            dependency.state = NodeState::PrOpen;
            dependency.pull_request = Some(PullRequest {
                number: 2,
                url: "pr/2".into(),
                head: "dep-2".into(),
                merged: false,
            });
        }
        let base = work_ready(&p, 4).unwrap();
        assert_eq!(base.reference, "dep-2");
        assert_eq!(base.commit, "dep-2");

        {
            let dependency = p.nodes.get_mut(&3).unwrap();
            dependency.state = NodeState::PrOpen;
            dependency.pull_request = Some(PullRequest {
                number: 3,
                url: "pr/3".into(),
                head: "dep-3".into(),
                merged: false,
            });
        }
        assert_eq!(work_ready(&p, 4), None);

        p.nodes.get_mut(&3).unwrap().pull_request = None;
        assert_eq!(work_ready(&p, 4), None);

        p.nodes.remove(&3);
        assert_eq!(work_ready(&p, 4), None);
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

    #[test]
    fn timeout_moves_to_retry_without_emitting_delegate_until_retry_is_due() {
        let mut n = node(1, NodeState::Running);
        n.deadline = Some(now());
        let result = reconcile(
            &plan(n),
            &Observation {
                observed_at: now(),
                ..Default::default()
            },
            Duration::minutes(5),
        );
        assert_eq!(result.plan.nodes[&1].state, NodeState::RetryWait);
        assert!(!result.plan.nodes[&1].state.occupies_worker());
        assert!(result.actions.is_empty());
    }

    #[test]
    fn workspace_tick_waits_when_global_capacity_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::infrastructure::orchestrator_store::OrchestratorStore::new(tmp.path());
        store
            .save_plan(&plan(node(1, NodeState::Runnable)), None, now())
            .unwrap();
        let outcome = reconcile_workspace_tick(tmp.path(), 0, now()).unwrap();
        assert_eq!(outcome.delegated, 0);
        let saved = store.load_plan("p").unwrap().unwrap();
        assert_eq!(saved.value.nodes[&1].state, NodeState::Runnable);
    }

    #[test]
    fn workspace_tick_reports_busy_without_dispatch_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        store
            .save_plan(&plan(node(1, NodeState::Runnable)), None, now())
            .unwrap();
        let foreign = Claim {
            workspace: store.workspace_key().into(),
            issue: 1,
            plan: "foreign".into(),
            owner: "other-owner".into(),
            generation: 7,
            lease: Lease {
                owner: "other-owner".into(),
                expires_at: now() + LEASE_DURATION,
            },
        };
        store.claim(foreign.clone(), now(), None).unwrap();

        let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(outcome.busy, vec![foreign]);
        assert_eq!(outcome.delegated, 0);
        assert_eq!(outcome.owner_queued, 0);
        assert_eq!(
            store.load_plan("p").unwrap().unwrap().value.nodes[&1].state,
            NodeState::Runnable
        );
        assert!(session::list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn workspace_tick_delegates_worker_directly_under_owner() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Do work".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: "Body".into(),
            },
        )
        .unwrap();
        let store = crate::infrastructure::orchestrator_store::OrchestratorStore::new(tmp.path());
        store
            .save_plan(&plan(node(1, NodeState::Runnable)), None, now())
            .unwrap();

        let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(outcome.delegated, 1);
        assert!(outcome.busy.is_empty());
        assert_eq!(outcome.owner_queued, 1);
        let sessions = session::list(tmp.path()).unwrap();
        let worker = sessions
            .iter()
            .find(|session| session.name == "owner-issue-1")
            .unwrap();
        assert_eq!(worker.started_from.as_deref(), Some("owner"));
        assert_eq!(sessions.len(), 1);
        let binding = orchestrator_event::binding(&worker.root).unwrap().unwrap();
        assert_eq!(binding.plan, "p");
        assert_eq!(binding.issue, 1);
        assert!(agent_prompt_store::take(&worker.root)
            .unwrap()
            .contains("Do not create sub-sessions"));
        assert_eq!(
            git_output(&worker.root, &["rev-parse", "HEAD"]),
            git_output(tmp.path(), &["rev-parse", "main"])
        );
        let claim = store.load_claims().unwrap().value.by_issue[&1].clone();
        assert_eq!(claim.workspace, store.workspace_key());
        assert_eq!(claim.owner, "owner");
        assert_eq!(claim.generation, 1);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn process_claim_child() {
        let Ok(workspace) = std::env::var("USAGI_ORCHESTRATOR_CLAIM_TEST_WORKSPACE") else {
            return;
        };
        let owner = std::env::var("USAGI_ORCHESTRATOR_CLAIM_TEST_OWNER").unwrap();
        let barrier =
            PathBuf::from(std::env::var("USAGI_ORCHESTRATOR_CLAIM_TEST_BARRIER").unwrap());
        let workspace = PathBuf::from(workspace);
        std::fs::write(barrier.join(format!("ready-{owner}")), "").unwrap();
        let deadline = Instant::now() + StdDuration::from_secs(10);
        while !barrier.join("go").exists() {
            assert!(Instant::now() < deadline, "claim test barrier timed out");
            std::thread::sleep(StdDuration::from_millis(10));
        }
        let store = OrchestratorStore::new(&workspace);
        let commit = git_output(&workspace, &["rev-parse", "HEAD"]);
        let mut candidate = plan(node(1, NodeState::Delegating));
        candidate.id = format!("plan-{owner}");
        candidate.owner.clone_from(&owner);
        candidate.nodes.get_mut(&1).unwrap().generation = 1;
        candidate.nodes.get_mut(&1).unwrap().lease = Some(Lease {
            owner: owner.clone(),
            expires_at: now() + LEASE_DURATION,
        });
        let action = Action::Delegate {
            id: format!("delegate-{owner}"),
            issue: 1,
            generation: 1,
            base: Base {
                reference: commit.clone(),
                commit,
            },
        };
        let (admitted, busy) =
            admit_delegate_actions(&workspace, &store, &mut candidate, vec![action], now())
                .unwrap();
        let result = if admitted.is_empty() {
            assert_eq!(busy.len(), 1);
            "busy"
        } else {
            delegate_worker(
                &workspace,
                &workspace,
                &candidate,
                1,
                1,
                match &admitted[0] {
                    Action::Delegate { base, .. } => base,
                    Action::Reobserve { .. } => unreachable!(),
                },
            )
            .unwrap();
            "delegated"
        };
        std::fs::write(barrier.join(format!("result-{owner}")), result).unwrap();
    }

    #[test]
    fn two_process_admission_spawns_one_worker_and_loser_has_no_effect() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        let barrier = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Atomic claim".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        let test_binary = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for owner in ["a", "b"] {
            children.push(
                Command::new(&test_binary)
                    .args([
                        "--exact",
                        "usecase::orchestrator::tests::process_claim_child",
                        "--nocapture",
                    ])
                    .env(crate::infrastructure::storage::DATA_DIR_ENV, data.path())
                    .env("USAGI_ORCHESTRATOR_CLAIM_TEST_WORKSPACE", tmp.path())
                    .env("USAGI_ORCHESTRATOR_CLAIM_TEST_OWNER", owner)
                    .env("USAGI_ORCHESTRATOR_CLAIM_TEST_BARRIER", barrier.path())
                    .spawn()
                    .unwrap(),
            );
        }
        let deadline = Instant::now() + StdDuration::from_secs(10);
        while ["a", "b"]
            .iter()
            .any(|owner| !barrier.path().join(format!("ready-{owner}")).exists())
        {
            assert!(Instant::now() < deadline, "child readiness timed out");
            std::thread::sleep(StdDuration::from_millis(10));
        }
        std::fs::write(barrier.path().join("go"), "").unwrap();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let results = ["a", "b"].map(|owner| {
            std::fs::read_to_string(barrier.path().join(format!("result-{owner}"))).unwrap()
        });
        assert_eq!(
            results
                .iter()
                .filter(|result| *result == "delegated")
                .count(),
            1
        );
        assert_eq!(results.iter().filter(|result| *result == "busy").count(), 1);
        assert_eq!(session::list(tmp.path()).unwrap().len(), 1);
        assert_eq!(
            OrchestratorStore::new(tmp.path())
                .load_claims()
                .unwrap()
                .value
                .by_issue
                .len(),
            1
        );
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn dependency_pr_head_is_the_worker_checkout_and_prompt_base() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Dependent work".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: vec![2],
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        git(tmp.path(), &["switch", "-q", "-c", "dependency"]);
        std::fs::write(tmp.path().join("sentinel.txt"), "dependency only\n").unwrap();
        git(tmp.path(), &["add", "sentinel.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "sentinel"]);
        let dependency_head = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        git(tmp.path(), &["switch", "-q", "main"]);

        let mut dependent = node(1, NodeState::Runnable);
        dependent.dependencies = vec![2];
        let mut dependency = node(2, NodeState::PrOpen);
        dependency.pull_request = Some(PullRequest {
            number: 2,
            url: "https://example.test/pr/2".into(),
            head: dependency_head.clone(),
            merged: false,
        });
        let mut plan = plan(dependent);
        plan.nodes.insert(2, dependency);
        let store = OrchestratorStore::new(tmp.path());
        store.save_plan(&plan, None, now()).unwrap();

        let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(outcome.delegated, 1);
        let worker = session::list(tmp.path())
            .unwrap()
            .into_iter()
            .find(|session| session.name == "owner-issue-1")
            .unwrap();
        assert_eq!(
            git_output(&worker.root, &["rev-parse", "HEAD"]),
            dependency_head
        );
        assert_eq!(
            std::fs::read_to_string(worker.root.join("sentinel.txt")).unwrap(),
            "dependency only\n"
        );
        let prompt = agent_prompt_store::take(&worker.root).unwrap();
        assert!(prompt.contains(&format!("- base commit: {}", dependency_head)));
        let saved = store.load_plan("p").unwrap().unwrap();
        assert_eq!(
            saved.value.nodes[&1].base.as_ref().unwrap().commit,
            dependency_head
        );
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn missing_and_unfetchable_dependency_heads_block_before_session_creation() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        for with_origin in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            init_repo(tmp.path());
            let _remote = if with_origin {
                let remote = tempfile::tempdir().unwrap();
                git(remote.path(), &["init", "-q", "--bare"]);
                let remote_path = remote.path().to_string_lossy().to_string();
                git(tmp.path(), &["remote", "add", "origin", &remote_path]);
                Some(remote)
            } else {
                None
            };
            let mut dependent = node(1, NodeState::Runnable);
            dependent.dependencies = vec![2];
            let mut dependency = node(2, NodeState::PrOpen);
            dependency.pull_request = Some(PullRequest {
                number: 2,
                url: "https://example.test/pr/2".into(),
                head: "1111111111111111111111111111111111111111".into(),
                merged: false,
            });
            let mut plan = plan(dependent);
            plan.nodes.insert(2, dependency);
            let store = OrchestratorStore::new(tmp.path());
            store.save_plan(&plan, None, now()).unwrap();

            let unresolved = work_ready(&plan, 1).unwrap();
            let error = resolve_dependency_base(tmp.path(), &unresolved).unwrap_err();
            assert!(matches!(
                (with_origin, error),
                (true, DependencyBaseError::Missing(_))
                    | (false, DependencyBaseError::Fetch { .. })
            ));

            let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

            assert_eq!(outcome.delegated, 0);
            assert!(session::list(tmp.path()).unwrap().is_empty());
            assert_eq!(
                store.load_plan("p").unwrap().unwrap().value.nodes[&1].state,
                NodeState::Blocked
            );
        }
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn moved_dependency_head_is_typed_and_blocks_the_action() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(tmp.path(), &["switch", "-q", "-c", "dependency"]);
        let old = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        std::fs::write(tmp.path().join("moved.txt"), "moved\n").unwrap();
        git(tmp.path(), &["add", "moved.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "move head"]);

        let mut plan = plan(node(1, NodeState::Delegating));
        let mut actions = vec![Action::Delegate {
            id: "delegate-1".into(),
            issue: 1,
            generation: 1,
            base: Base {
                reference: "dependency".into(),
                commit: old.clone(),
            },
        }];
        assert!(matches!(
            resolve_dependency_base(
                tmp.path(),
                &Base {
                    reference: "dependency".into(),
                    commit: old.clone(),
                }
            ),
            Err(DependencyBaseError::Moved { .. })
        ));

        resolve_delegate_bases(tmp.path(), &mut plan, &mut actions);

        assert!(actions.is_empty());
        assert_eq!(plan.nodes[&1].state, NodeState::Blocked);
        assert!(session::list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn workspace_tick_observes_worker_binding_and_acks_events() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let worker = session::create_with_agent(
            tmp.path(),
            "worker",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        set_worktree_status(tmp.path(), "worker", BranchStatus::Synced);
        let mut n = node(1, NodeState::Running);
        n.pull_request = Some(PullRequest {
            number: 10,
            url: "https://example.test/pr/10".into(),
            head: "worker-head".into(),
            merged: false,
        });
        let store = crate::infrastructure::orchestrator_store::OrchestratorStore::new(tmp.path());
        let active_plan = plan(n);
        store.save_plan(&active_plan, None, now()).unwrap();
        let claim = Claim {
            workspace: store.workspace_key().into(),
            issue: 1,
            plan: active_plan.id.clone(),
            owner: active_plan.owner.clone(),
            generation: 0,
            lease: Lease {
                owner: active_plan.owner.clone(),
                expires_at: now() + LEASE_DURATION,
            },
        };
        store.claim(claim.clone(), now(), None).unwrap();
        orchestrator_event::register(
            &worker.root,
            &WorkerBinding {
                workspace: tmp.path().to_path_buf(),
                plan: "p".into(),
                issue: 1,
                generation: 0,
                owner_worktree: tmp.path().join(".usagi/sessions/owner"),
            },
        )
        .unwrap();
        let event = Event {
            id: "p-1-0-succeeded-0".into(),
            plan: "p".into(),
            issue: 1,
            generation: 0,
            kind: EventKind::Succeeded,
            terminal_revision: 0,
            observed_at: now(),
        };
        store.append_event(&event).unwrap();

        let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(outcome.acknowledgements, 1);
        assert!(store.load_events("p").unwrap().is_empty());
        let saved = store.load_plan("p").unwrap().unwrap();
        assert_eq!(saved.value.nodes[&1].worker, None);
        assert_eq!(saved.value.nodes[&1].state, NodeState::Merged);
        assert!(saved.value.nodes[&1].pull_request.as_ref().unwrap().merged);
        assert!(store.load_claims().unwrap().value.by_issue.is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn failed_worker_releases_claim_and_retry_claims_a_new_generation() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Retry claim".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        let store = OrchestratorStore::new(tmp.path());
        store
            .save_plan(&plan(node(1, NodeState::Runnable)), None, now())
            .unwrap();
        assert_eq!(
            reconcile_workspace_tick(tmp.path(), 1, now())
                .unwrap()
                .delegated,
            1
        );
        let first = store.load_claims().unwrap().value.by_issue[&1].clone();
        let event = Event {
            id: "p-1-1-failed-0".into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            kind: EventKind::Failed,
            terminal_revision: 0,
            observed_at: now(),
        };
        store.append_event(&event).unwrap();

        let released = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(released.delegated, 0);
        assert!(store.load_claims().unwrap().value.by_issue.is_empty());
        assert_eq!(
            store.load_plan("p").unwrap().unwrap().value.nodes[&1].state,
            NodeState::RetryWait
        );

        let retried =
            reconcile_workspace_tick(tmp.path(), 1, now() + Duration::seconds(31)).unwrap();

        assert_eq!(retried.delegated, 1);
        let second = store.load_claims().unwrap().value.by_issue[&1].clone();
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert_eq!(session::list(tmp.path()).unwrap().len(), 1);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn crashed_owner_is_reclaimed_only_after_stale_lease_and_absence_reobserve() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Crash recovery".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        let mut active = plan(node(1, NodeState::Delegating));
        active.nodes.get_mut(&1).unwrap().generation = 1;
        active.nodes.get_mut(&1).unwrap().lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now() + LEASE_DURATION,
        });
        let store = OrchestratorStore::new(tmp.path());
        store.save_plan(&active, None, now()).unwrap();
        store
            .claim(durable_claim(&store, &active, 1), now(), None)
            .unwrap();

        let before_expiry =
            reconcile_workspace_tick(tmp.path(), 1, now() + Duration::minutes(4)).unwrap();
        assert_eq!(before_expiry.delegated, 0);
        assert!(session::list(tmp.path()).unwrap().is_empty());

        let reobserved = reconcile_workspace_tick(tmp.path(), 1, now() + LEASE_DURATION).unwrap();
        assert_eq!(reobserved.delegated, 0);
        assert_eq!(
            store.load_plan("p").unwrap().unwrap().value.nodes[&1].state,
            NodeState::Runnable
        );
        assert_eq!(
            store.load_claims().unwrap().value.by_issue[&1].generation,
            1
        );

        let recovered =
            reconcile_workspace_tick(tmp.path(), 1, now() + LEASE_DURATION + Duration::seconds(1))
                .unwrap();
        assert_eq!(recovered.delegated, 1);
        assert!(recovered.busy.is_empty());
        assert_eq!(
            store.load_claims().unwrap().value.by_issue[&1].generation,
            2
        );
        assert_eq!(session::list(tmp.path()).unwrap().len(), 1);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn workspace_tick_reobserves_absent_worker_before_redelegating() {
        let tmp = tempfile::tempdir().unwrap();
        let mut n = node(1, NodeState::Delegating);
        n.generation = 1;
        n.lease = Some(Lease {
            owner: "owner".into(),
            expires_at: now(),
        });
        let store = crate::infrastructure::orchestrator_store::OrchestratorStore::new(tmp.path());
        store.save_plan(&plan(n), None, now()).unwrap();

        let outcome = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap();

        assert_eq!(outcome.delegated, 0);
        assert_eq!(
            store.load_plan("p").unwrap().unwrap().value.nodes[&1].state,
            NodeState::Runnable
        );
    }

    #[test]
    fn workspace_tick_reports_unreadable_plan_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        std::fs::write(tmp.path().join(".usagi/orchestrators"), "not a directory").unwrap();

        let error = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap_err();

        assert!(error.to_string().contains("failed to read"));
    }

    #[test]
    fn workspace_tick_reports_unreadable_plan_state() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_dir = tmp.path().join(".usagi/orchestrators/p");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(plan_dir.join("state.json"), "{").unwrap();

        let error = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap_err();

        assert!(error.to_string().contains("failed to parse"));
    }

    #[test]
    fn workspace_tick_surfaces_observe_and_owner_queue_errors() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let store = crate::infrastructure::orchestrator_store::OrchestratorStore::new(tmp.path());
        store
            .save_plan(&plan(node(1, NodeState::Running)), None, now())
            .unwrap();
        let created = session::create_with_agent(
            tmp.path(),
            "bad-binding",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        let binding_path = created.root.join(".usagi/orchestrator-worker.json");
        std::fs::create_dir_all(binding_path.parent().unwrap()).unwrap();
        std::fs::write(&binding_path, "{").unwrap();

        let error = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap_err();
        assert!(error.to_string().contains("failed to parse"));

        std::fs::write(&binding_path, "").unwrap();
        let bad_data = tmp.path().join("not-a-data-dir");
        std::fs::write(&bad_data, "file").unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, &bad_data);
        let mut queued = plan(node(2, NodeState::Runnable));
        queued.id = "q".into();
        store.save_plan(&queued, None, now()).unwrap();

        let error = reconcile_workspace_tick(tmp.path(), 1, now()).unwrap_err();
        assert!(!error.to_string().is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn observe_reports_bad_worker_binding_and_event_store_errors() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let created = session::create_with_agent(
            tmp.path(),
            "bad-binding",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        let binding_path = created.root.join(".usagi/orchestrator-worker.json");
        std::fs::create_dir_all(binding_path.parent().unwrap()).unwrap();
        std::fs::write(&binding_path, "{").unwrap();

        let error = observe(tmp.path(), &plan(node(1, NodeState::Running)), now()).unwrap_err();
        assert!(error.to_string().contains("failed to parse"));

        std::fs::write(&binding_path, "").unwrap();
        std::fs::create_dir_all(tmp.path().join(".usagi/orchestrators/p")).unwrap();
        std::fs::write(
            tmp.path().join(".usagi/orchestrators/p/events"),
            "not a directory",
        )
        .unwrap();
        let error = observe(tmp.path(), &plan(node(1, NodeState::Running)), now()).unwrap_err();
        assert!(!error.to_string().is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn observe_reports_session_status_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let file_workspace = tmp.path().join("not-a-workspace");
        std::fs::write(&file_workspace, "file").unwrap();

        let error =
            observe(&file_workspace, &plan(node(1, NodeState::Running)), now()).unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn observe_skips_unbound_mismatched_unknown_and_stale_sessions() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        session::create_with_agent(
            tmp.path(),
            "unbound",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        for (name, binding) in [
            (
                "other-plan",
                WorkerBinding {
                    workspace: tmp.path().to_path_buf(),
                    plan: "other".into(),
                    issue: 1,
                    generation: 0,
                    owner_worktree: tmp.path().to_path_buf(),
                },
            ),
            (
                "unknown-issue",
                WorkerBinding {
                    workspace: tmp.path().to_path_buf(),
                    plan: "p".into(),
                    issue: 99,
                    generation: 0,
                    owner_worktree: tmp.path().to_path_buf(),
                },
            ),
            (
                "stale-generation",
                WorkerBinding {
                    workspace: tmp.path().to_path_buf(),
                    plan: "p".into(),
                    issue: 1,
                    generation: 7,
                    owner_worktree: tmp.path().to_path_buf(),
                },
            ),
        ] {
            let created = session::create_with_agent(
                tmp.path(),
                name,
                Default::default(),
                crate::domain::workspace_state::SessionOrigin::Mcp,
                Some("owner".into()),
            )
            .unwrap();
            orchestrator_event::register(&created.root, &binding).unwrap();
        }

        let observation = observe(tmp.path(), &plan(node(1, NodeState::Running)), now()).unwrap();

        assert!(observation.sessions.is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn dispatch_reuses_an_existing_worker_session() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Reuse worker".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        let existing = session::create_with_agent(
            tmp.path(),
            "owner-issue-1",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        let commit = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        let action = Action::Delegate {
            id: "delegate-1".into(),
            issue: 1,
            generation: 2,
            base: Base {
                reference: commit.clone(),
                commit,
            },
        };
        let store = OrchestratorStore::new(tmp.path());

        let delegated = dispatch_actions(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            vec![action],
            &store,
            now(),
        )
        .unwrap();

        assert_eq!(delegated, (1, Vec::new(), Vec::new()));
        let binding = orchestrator_event::binding(&existing.root)
            .unwrap()
            .unwrap();
        assert_eq!(binding.generation, 2);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn dispatch_logs_delegate_failures_and_skips_reobserve_actions() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        let action = Action::Delegate {
            id: "delegate-1".into(),
            issue: 1,
            generation: 0,
            base: Base {
                reference: "main".into(),
                commit: "abc".into(),
            },
        };
        let store = OrchestratorStore::new(tmp.path());

        let delegated = dispatch_actions(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            vec![
                Action::Reobserve {
                    id: "reobserve-2".into(),
                    issue: 2,
                    worker: Some("missing".into()),
                },
                action,
            ],
            &store,
            now(),
        )
        .unwrap();

        assert_eq!(delegated, (0, Vec::new(), vec![1]));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn reobserve_absent_collects_only_reobserve_actions() {
        let actions = vec![
            Action::Delegate {
                id: "delegate-1".into(),
                issue: 1,
                generation: 1,
                base: Base {
                    reference: "main".into(),
                    commit: "main".into(),
                },
            },
            Action::Reobserve {
                id: "reobserve-2".into(),
                issue: 2,
                worker: Some("worker-2".into()),
            },
            Action::Reobserve {
                id: "reobserve-3".into(),
                issue: 3,
                worker: None,
            },
        ];

        let observations = reobserve_absent(&actions);

        assert_eq!(observations.len(), 2);
        assert!(observations.contains_key(&2));
        assert!(observations.contains_key(&3));
        assert!(!observations.contains_key(&1));
        assert_eq!(observations[&2], SessionObservation::default());
        assert_eq!(observations[&3], SessionObservation::default());
    }

    #[test]
    fn delegate_reports_register_issue_read_and_prompt_queue_errors() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let existing = session::create_with_agent(
            tmp.path(),
            "owner-issue-1",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            Some("owner".into()),
        )
        .unwrap();
        std::fs::create_dir_all(existing.root.join(".usagi/orchestrator-worker.json")).unwrap();

        let error = delegate_worker(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &immutable_head_base(tmp.path()),
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());

        std::fs::remove_dir_all(existing.root.join(".usagi/orchestrator-worker.json")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        std::fs::write(tmp.path().join(".usagi/issues"), "not a directory").unwrap();
        let error = delegate_worker(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &immutable_head_base(tmp.path()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed to read"));

        std::fs::remove_file(tmp.path().join(".usagi/issues")).unwrap();
        crate::usecase::issue::create(
            tmp.path(),
            crate::usecase::issue::NewIssue {
                title: "Prompt error".into(),
                priority: Default::default(),
                labels: Vec::new(),
                dependson: Vec::new(),
                related: Vec::new(),
                parent: None,
                milestone: None,
                body: String::new(),
            },
        )
        .unwrap();
        let bad_data = tmp.path().join("not-a-data-dir");
        std::fs::write(&bad_data, "file").unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, &bad_data);
        let error = delegate_worker(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &immutable_head_base(tmp.path()),
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn delegate_reports_exact_existing_branch_without_a_session_record() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(tmp.path(), &["branch", "usagi/owner-issue-1"]);

        let error = delegate_worker(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &immutable_head_base(tmp.path()),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("session \"owner-issue-1\" already exists but is not recorded"),
            "{error:#}"
        );
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn delegate_reports_branch_namespace_conflicts_before_registering() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(tmp.path(), &["branch", "usagi/owner-issue-1/child"]);

        let error = delegate_worker(
            tmp.path(),
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &immutable_head_base(tmp.path()),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("conflicts with the existing branch"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn delegate_reports_create_errors_before_registering() {
        let tmp = tempfile::tempdir().unwrap();
        let file_workspace = tmp.path().join("not-a-workspace");
        std::fs::write(&file_workspace, "file").unwrap();

        let error = delegate_worker(
            &file_workspace,
            tmp.path(),
            &plan(node(1, NodeState::Running)),
            1,
            0,
            &Base {
                reference: "main".into(),
                commit: "abc".into(),
            },
        )
        .unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn owner_prompt_and_names_cover_reobserve_root_and_sanitized_workers() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        let mut p = plan(node(1, NodeState::Runnable));
        p.owner = ":root".into();
        let actions = vec![Action::Reobserve {
            id: "reobserve-1".into(),
            issue: 1,
            worker: None,
        }];

        assert!(owner_needs_wakeup(tmp.path(), &p, &actions).unwrap());
        queue_owner_prompt(tmp.path(), &p, &actions).unwrap();
        assert!(agent_prompt_store::take(tmp.path())
            .unwrap()
            .contains("no worker"));
        assert_eq!(owner_worktree(tmp.path(), ":root"), tmp.path());
        assert_eq!(owner_worktree(tmp.path(), "root"), tmp.path());
        assert_eq!(worker_session_name("owner/name", 9), "owner-name-issue-9");
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn owner_prompt_renders_delegate_and_named_reobserve_actions() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        let actions = vec![
            Action::Delegate {
                id: "delegate-1".into(),
                issue: 1,
                generation: 4,
                base: Base {
                    reference: "main".into(),
                    commit: "abc".into(),
                },
            },
            Action::Reobserve {
                id: "reobserve-2".into(),
                issue: 2,
                worker: Some("worker-2".into()),
            },
        ];

        queue_owner_prompt(tmp.path(), &plan(node(1, NodeState::Runnable)), &actions).unwrap();
        let prompt = agent_prompt_store::take(tmp.path()).unwrap();

        assert!(prompt.contains("2 pending action(s)"));
        assert!(prompt.contains("- delegate issue #1, generation 4"));
        assert!(prompt.contains("- reobserve issue #2 (worker-2)"));
        assert!(!prompt.contains("no worker"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn owner_wakeup_without_actions_is_false_even_for_bad_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let file_workspace = tmp.path().join("not-a-workspace");
        std::fs::write(&file_workspace, "file").unwrap();

        let needs_wakeup =
            owner_needs_wakeup(&file_workspace, &plan(node(1, NodeState::Runnable)), &[]).unwrap();

        assert!(!needs_wakeup);
    }

    #[test]
    fn owner_worktree_prefers_a_recorded_session_root() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let owner = session::create_with_agent(
            tmp.path(),
            "owner",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            None,
        )
        .unwrap();

        assert_eq!(owner_worktree(tmp.path(), "owner"), owner.root);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn owner_wakeup_and_prompt_queue_surface_storage_errors() {
        let _guard = crate::test_support::process_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let file_workspace = tmp.path().join("not-a-workspace");
        std::fs::write(&file_workspace, "file").unwrap();
        let actions = vec![Action::Delegate {
            id: "delegate-1".into(),
            issue: 1,
            generation: 1,
            base: Base {
                reference: "main".into(),
                commit: "abc".into(),
            },
        }];

        let error = owner_needs_wakeup(
            &file_workspace,
            &plan(node(1, NodeState::Running)),
            &actions,
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());

        let bad_data = tmp.path().join("not-a-data-dir");
        std::fs::write(&bad_data, "file").unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, &bad_data);
        let error = queue_owner_prompt(tmp.path(), &plan(node(1, NodeState::Running)), &actions)
            .unwrap_err();
        assert!(!error.to_string().is_empty());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn owner_wakeup_respects_a_recorded_owner_phase() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        session::create_with_agent(
            tmp.path(),
            "other",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            None,
        )
        .unwrap();
        let owner = session::create_with_agent(
            tmp.path(),
            "owner",
            Default::default(),
            crate::domain::workspace_state::SessionOrigin::Mcp,
            None,
        )
        .unwrap();
        let p = plan(node(1, NodeState::Runnable));
        let actions = vec![Action::Delegate {
            id: "delegate-1".into(),
            issue: 1,
            generation: 0,
            base: Base {
                reference: "main".into(),
                commit: "abc".into(),
            },
        }];

        crate::infrastructure::agent_state_store::write(&owner.root, AgentPhase::Ready).unwrap();
        assert!(!owner_needs_wakeup(tmp.path(), &p, &actions).unwrap());

        crate::infrastructure::agent_state_store::write(&owner.root, AgentPhase::Ended).unwrap();
        assert!(owner_needs_wakeup(tmp.path(), &p, &actions).unwrap());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }
}
