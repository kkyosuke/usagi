//! Shared, presentation-safe projection of daemon-owned Work Runs.
//!
//! Home and Director deliberately consume this same value. It owns ordering,
//! progress aggregation, and observation freshness so the two surfaces cannot
//! disagree about which run is primary or present cached data as live.

use usagi_core::domain::supervisor::{SupervisorRunQuery, SupervisorRunState, TaskState};

/// Whether the daemon observation behind the projection is current.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkRunFreshness {
    /// No observation has completed yet. An empty projection stays visually
    /// quiet during the first frame.
    #[default]
    Pending,
    /// The last observation completed coherently.
    Fresh,
    /// The last observation failed. Existing runs are cached and must be
    /// labelled as such; an empty value means progress is unavailable.
    Unavailable,
}

/// Counts used by every Work Run summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkRunProgress {
    pub succeeded_tasks: usize,
    pub total_tasks: usize,
    /// Tasks currently consuming (or reserving) daemon concurrency. This is
    /// intentionally the same `Dispatched | Running` definition enforced by
    /// supervisor admission, rather than a view-specific list of busy-looking
    /// states.
    pub active_agents: usize,
    pub max_agents: usize,
}

impl WorkRunProgress {
    #[must_use]
    pub fn from_run(run: &SupervisorRunQuery) -> Self {
        Self {
            succeeded_tasks: run
                .tasks
                .iter()
                .filter(|task| task.state == TaskState::Succeeded)
                .count(),
            total_tasks: run.tasks.len(),
            active_agents: run
                .tasks
                .iter()
                .filter(|task| matches!(task.state, TaskState::Dispatched | TaskState::Running))
                .count(),
            max_agents: run.policy.max_concurrency,
        }
    }
}

/// Canonically ordered Work Run rows plus their observation state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkRunProjection {
    runs: Vec<SupervisorRunQuery>,
    freshness: WorkRunFreshness,
}

impl WorkRunProjection {
    /// Replace cached rows with one coherent daemon observation.
    #[must_use]
    pub fn fresh(mut runs: Vec<SupervisorRunQuery>) -> Self {
        sort_runs(&mut runs);
        Self {
            runs,
            freshness: WorkRunFreshness::Fresh,
        }
    }

    /// Preserve the last coherent rows while making their staleness explicit.
    #[must_use]
    pub fn unavailable(mut self) -> Self {
        self.freshness = WorkRunFreshness::Unavailable;
        self
    }

    #[must_use]
    pub fn primary(&self) -> Option<&SupervisorRunQuery> {
        self.runs.first()
    }

    #[must_use]
    pub const fn freshness(&self) -> WorkRunFreshness {
        self.freshness
    }

    #[must_use]
    pub fn runs(&self) -> &[SupervisorRunQuery] {
        &self.runs
    }

    /// Applies the daemon-authoritative result of a human control before the
    /// next observation. The result replaces only an already-observed exact
    /// run and is sorted through the same `SSoT` as a full snapshot. An
    /// unexpected identity is ignored so a single response cannot grow the
    /// bounded snapshot or invent a row.
    pub fn apply_control(&mut self, run: SupervisorRunQuery) {
        if let Some(existing) = self
            .runs
            .iter_mut()
            .find(|existing| existing.supervisor_run_id == run.supervisor_run_id)
            && (run.state_revision > existing.state_revision || run == *existing)
        {
            *existing = run;
        }
        sort_runs(&mut self.runs);
        self.freshness = WorkRunFreshness::Fresh;
    }
}

fn sort_runs(runs: &mut [SupervisorRunQuery]) {
    runs.sort_by_key(|run| {
        (
            run_priority(run.state),
            std::cmp::Reverse(run.supervisor_run_id),
        )
    });
}

const fn run_priority(state: SupervisorRunState) -> u8 {
    match state {
        SupervisorRunState::WaitingForDecision | SupervisorRunState::Escalated => 0,
        SupervisorRunState::Failed => 1,
        SupervisorRunState::Running | SupervisorRunState::Verifying => 2,
        SupervisorRunState::Planning => 3,
        SupervisorRunState::Succeeded | SupervisorRunState::Cancelled => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use usagi_core::domain::supervisor::{
        ArtifactContract, ExecutionPolicy, SupervisorRunId, TaskId, TaskQuery,
    };

    fn run(state: SupervisorRunState, task_states: &[TaskState]) -> SupervisorRunQuery {
        SupervisorRunQuery {
            supervisor_run_id: SupervisorRunId::new(),
            state_revision: 1,
            state,
            terminal_at: None,
            terminal_reason: None,
            policy: ExecutionPolicy::default(),
            escalation: None,
            tasks: task_states
                .iter()
                .copied()
                .enumerate()
                .map(|(index, state)| TaskQuery {
                    task_id: TaskId::new(format!("task-{index}")).unwrap(),
                    parent_task_id: None,
                    dependencies: BTreeSet::new(),
                    instruction_digest: format!("digest-{index}"),
                    required_artifact_contract: ArtifactContract::default(),
                    attempt: 1,
                    generation: 1,
                    assigned_dispatch_run: None,
                    verification_attempt: 0,
                    verification_retry_at: None,
                    state,
                })
                .collect(),
            provenance: Vec::new(),
        }
    }

    #[test]
    fn projection_is_the_single_source_for_priority_and_freshness() {
        let states = [
            SupervisorRunState::Succeeded,
            SupervisorRunState::Running,
            SupervisorRunState::Failed,
            SupervisorRunState::Escalated,
        ];
        let projection =
            WorkRunProjection::fresh(states.into_iter().map(|state| run(state, &[])).collect());
        assert_eq!(projection.freshness(), WorkRunFreshness::Fresh);
        assert_eq!(
            projection
                .runs()
                .iter()
                .map(|run| run.state)
                .collect::<Vec<_>>(),
            vec![
                SupervisorRunState::Escalated,
                SupervisorRunState::Failed,
                SupervisorRunState::Running,
                SupervisorRunState::Succeeded,
            ]
        );

        let unavailable = projection.clone().unavailable();
        assert_eq!(unavailable.freshness(), WorkRunFreshness::Unavailable);
        assert_eq!(unavailable.runs(), projection.runs());
        assert_eq!(
            WorkRunProjection::default().freshness(),
            WorkRunFreshness::Pending
        );
    }

    #[test]
    fn progress_matches_supervisor_concurrency_admission() {
        let run = run(
            SupervisorRunState::Running,
            &[
                TaskState::Pending,
                TaskState::Ready,
                TaskState::Dispatched,
                TaskState::Running,
                TaskState::AwaitingDecision,
                TaskState::Retrying,
                TaskState::Verifying,
                TaskState::Succeeded,
            ],
        );
        assert_eq!(
            WorkRunProgress::from_run(&run),
            WorkRunProgress {
                succeeded_tasks: 1,
                total_tasks: 8,
                active_agents: 2,
                max_agents: run.policy.max_concurrency,
            }
        );
    }

    #[test]
    fn control_results_replace_exact_runs_monotonically_and_resort() {
        let mut running = run(SupervisorRunState::Running, &[]);
        running.state_revision = 4;
        let id = running.supervisor_run_id;
        let mut projection = WorkRunProjection::fresh(vec![running.clone()]);

        let mut cancelled = running.clone();
        cancelled.state_revision = 5;
        cancelled.state = SupervisorRunState::Cancelled;
        projection.apply_control(cancelled.clone());
        assert_eq!(projection.runs(), std::slice::from_ref(&cancelled));
        assert_eq!(projection.freshness(), WorkRunFreshness::Fresh);

        projection.apply_control(running);
        assert_eq!(projection.runs(), std::slice::from_ref(&cancelled));

        let failed = run(SupervisorRunState::Failed, &[]);
        projection.apply_control(failed);
        assert_eq!(projection.runs(), std::slice::from_ref(&cancelled));
        assert_eq!(projection.runs()[0].supervisor_run_id, id);
    }
}
