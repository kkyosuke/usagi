//! Durable human workflow commands and projections of authenticated peer evidence.
use anyhow::{Context, Result, ensure};
use usagi_core::domain::agent_message::MessageKind;
use usagi_core::domain::id::{AgentId, OperationId, SessionId, WorkspaceId};
use usagi_core::domain::workflow::{Phase, WorkflowCommand, WorkflowRun, WorkflowSnapshot};
use usagi_core::infrastructure::store::dispatch::{DispatchStore, workflows::WorkflowRecord};

/// Admit once before launch/delivery effects. A conflicting retry changes nothing.
/// # Errors
/// Rejects invalid goals, absent workflows and conflicting identities.
pub fn admit(
    store: &DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
    operation: OperationId,
    command: &WorkflowCommand,
    issue: Option<u32>,
) -> Result<()> {
    store.update_workflow(workspace, session, |value| {
        match command {
            WorkflowCommand::Start {
                goal,
                agents,
                revision_limit,
            } => admit_start(value, operation, goal, *agents, *revision_limit, issue)?,
            WorkflowCommand::Instruct { recipient, body } => {
                let record = value.as_mut().context("workflow has not started")?;
                ensure!(record.finish.is_none(), "workflow has already finished");
                ensure!(
                    record.operation != operation,
                    "instruction ID conflicts with workflow start"
                );
                record
                    .run
                    .as_mut()
                    .context("workflow launch is not yet admitted")?
                    .enqueue(operation, *recipient, body.clone())
                    .map_err(anyhow::Error::msg)?;
            }
            WorkflowCommand::Finish => {
                let record = value.as_mut().context("workflow has not started")?;
                if record.finish == Some(operation) {
                    // The same request arriving twice ends the run once.
                    return Ok(());
                }
                ensure!(record.finish.is_none(), "workflow has already finished");
                record.finished.push(record.run.as_ref().map_or_else(
                    || {
                        // A start that never launched has no run to archive, so
                        // the intent itself is what the person abandoned.
                        usagi_core::domain::workflow::FinishedRun {
                            id: record.operation,
                            outcome: usagi_core::domain::workflow::Outcome::Stopped,
                            goal: record.goal.clone(),
                            phase: Phase::Starting,
                            issue: record.issue,
                            pr_url: None,
                        }
                    },
                    WorkflowRun::finished,
                ));
                let excess = record
                    .finished
                    .len()
                    .saturating_sub(usagi_core::domain::workflow::FINISHED_LIMIT);
                record.finished.drain(..excess);
                record.run = None;
                record.finish = Some(operation);
                // Progress state belongs to the run that just ended. The journal
                // cursor is the exception: keeping it is what stops the next run
                // from replaying this one's peer messages as its own evidence.
                record.suspended_phase = None;
                record.implementation_operation = None;
                record.announced = None;
            }
        }
        Ok(())
    })
}

/// The `Start` arm of [`admit`], lifted out so the admission function stays
/// readable: a start is three different intents (a new record, a retry of the
/// same one, and a fresh intent after the previous run ended) and each has its
/// own conflict rule.
fn admit_start(
    value: &mut Option<WorkflowRecord>,
    operation: OperationId,
    goal: &str,
    agents: usagi_core::domain::workflow::WorkflowAgents,
    revision_limit: u8,
    issue: Option<u32>,
) -> Result<()> {
    ensure!(
        !goal.trim().is_empty() && goal.len() <= 16384 && !goal.contains('\0'),
        "invalid workflow goal"
    );
    ensure!(
        usagi_core::domain::workflow::valid_revision_limit(revision_limit),
        "invalid workflow revision limit"
    );
    if let Some(existing) = value.as_mut().filter(|record| record.finish.is_some()) {
        // The previous run ended, so this is a new intent in the
        // same session: everything the old run owned is reset, and
        // only the archive of ended runs carries over.
        ensure!(
            !existing.finished.iter().any(|ended| ended.id == operation),
            "workflow operation has already finished"
        );
        existing.agents = agents;
        existing.revision_limit = revision_limit;
        existing.operation = operation;
        existing.goal.clear();
        existing.goal.push_str(goal);
        existing.issue = issue;
        existing.finish = None;
        existing.run = None;
        existing.initial_notified = false;
        existing.preferences_saved = false;
        existing.start_error = None;
        existing.authorized_operations.clear();
    } else if let Some(existing) = value {
        // An issue-backed start is identified by the issue, not by
        // the rendered text: the issue moves to `in-progress` as
        // soon as the run starts, so re-rendering it would make a
        // retry look like a different intent.
        let same_goal = if issue.is_some() {
            existing.issue == issue
        } else {
            existing.goal == goal && existing.issue.is_none()
        };
        ensure!(
            existing.operation == operation
                && same_goal
                && existing.agents == agents
                && existing.revision_limit == revision_limit,
            "session already has another workflow"
        );
    } else {
        *value = Some(WorkflowRecord {
            agents,
            revision_limit,
            version: 1,
            operation,
            goal: goal.to_owned(),
            run: None,
            initial_notified: false,
            preferences_saved: false,
            cursor: None,
            start_error: None,
            suspended_phase: None,
            implementation_operation: None,
            authorized_operations: Vec::new(),
            announced: None,
            issue,
            finish: None,
            finished: Vec::new(),
        });
    }
    Ok(())
}

/// Bind only the actual admitted implementation participant.
/// # Errors
/// Rejects absent intent or a mismatched launch.
pub fn bind(
    store: &DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
    operation: OperationId,
    implementer: AgentId,
) -> Result<()> {
    store.update_workflow(workspace, session, |value| {
        let record = value.as_mut().context("workflow intent is missing")?;
        let issue = record.issue;
        // A launch that was still resolving when the person gave up on it must
        // not come back. `start` releases the runtime lock while it proves the
        // participant is ready, so a finish can be admitted in between, and
        // binding then would leave a record that is finished *and* running:
        // instructions and a second finish are both refused, and nothing can
        // move it again.
        ensure!(record.finish.is_none(), "workflow has already finished");
        ensure!(record.operation == operation, "workflow launch conflict");
        if let Some(run) = &record.run {
            ensure!(
                run.implementer == implementer,
                "workflow implementer changed"
            );
            return Ok(());
        }
        record.run = Some(WorkflowRun {
            agents: record.agents,
            id: operation,
            session,
            goal: record.goal.clone(),
            implementer,
            reviewer: None,
            phase: Phase::Implementing,
            revision_limit: record.revision_limit,
            revisions: 0,
            review: None,
            waiting_reason: None,
            pr_url: None,
            issue,
            instructions: Vec::new(),
            history: Vec::new(),
        });
        record.initial_notified = true;
        Ok(())
    })
}

/// Read the stored projection without replaying the peer journal.
///
/// A control request answers with this after its own reconcile pass, so the
/// response reflects the command it just applied without paying for a second
/// replay of the same journal.
/// # Errors
/// Returns store read failures.
pub fn projection(
    store: &DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<WorkflowSnapshot> {
    let Some(record) = store.workflow(workspace, session)? else {
        let defaults = store.workflow_defaults(workspace)?;
        return Ok(WorkflowSnapshot {
            agents: defaults.agents,
            revision_limit: defaults.revision_limit,
            session,
            run: None,
            pending_start: None,
            finished: Vec::new(),
        });
    };
    // No run and no finish is the one record that is still trying to launch.
    // Once it has finished, the session is free rather than mid-start.
    let pending_start = (record.run.is_none() && record.finish.is_none()).then(|| {
        usagi_core::domain::workflow::WorkflowPendingStart {
            agents: record.agents,
            revision_limit: record.revision_limit,
            operation_id: record.operation,
            goal: record.goal.clone(),
            error: record.start_error.clone(),
            issue: record.issue,
        }
    });
    Ok(WorkflowSnapshot {
        agents: record.agents,
        revision_limit: record.revision_limit,
        session,
        run: record.run,
        pending_start,
        finished: record.finished,
    })
}

/// Reconcile replayable peer evidence without consuming either Agent's inbox.
/// # Errors
/// Returns journal read/write failures rather than inventing progress.
pub fn snapshot(
    store: &DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<WorkflowSnapshot> {
    replay(store, workspace, session)?;
    projection(store, workspace, session)
}

/// Apply the peer journal to the stored run. Reading what it produced is
/// [`projection`]'s job, so there is one place that decides how a record
/// projects.
fn replay(store: &DispatchStore, workspace: WorkspaceId, session: SessionId) -> Result<()> {
    if store.workflow(workspace, session)?.is_none() {
        return Ok(());
    }
    let messages = store.workflow_messages(workspace, session)?;
    let agents = store.agents_in_workspace(workspace)?;
    let bindings = store.bindings()?;
    store.update_workflow(workspace, session, |value| {
        let record = value.as_mut().context("workflow disappeared")?;
        // Nothing to replay onto: the start has not launched, or the run ended.
        let Some(run) = record.run.as_mut() else {
            return Ok(());
        };
        if record.suspended_phase.is_some() {
            return Ok(());
        }
        let offset = journal_offset(record.cursor, &messages);
        for entry in messages.iter().skip(offset) {
            let message = &entry.message;
            let previous = (run.phase, run.review.clone());
            if entry.from_agent_id == run.implementer
                && (entry.from_run_id == run.id
                    || Some(entry.from_run_id) == record.implementation_operation
                    || record
                        .authorized_operations
                        .contains(&(entry.from_agent_id, entry.from_run_id)))
                && message.kind == MessageKind::ReviewRequest
            {
                let assigned = agents.iter().any(|agent| {
                    agent.agent_id == message.to_agent_id
                        && agent.session_id == Some(session)
                        && agent.runtime.as_str() == run.agents.reviewer.profile_id()
                }) && bindings.iter().any(|binding| {
                    binding.worker.agent_id == message.to_agent_id
                        && binding.worker.session_id == Some(session)
                        && binding.caller.agent_id == run.implementer
                        && binding.caller.session_id == Some(session)
                });
                if assigned
                    && run
                        .reviewer
                        .is_none_or(|reviewer| reviewer == message.to_agent_id)
                    && let Some(target) = &message.review
                    && run
                        .request_review(message.message_id, target.clone())
                        .is_ok()
                {
                    run.reviewer = Some(message.to_agent_id);
                }
            } else if message.to_agent_id == run.implementer
                && (record
                    .authorized_operations
                    .contains(&(entry.from_agent_id, entry.from_run_id))
                    || bindings
                        .iter()
                        .any(|binding| original_reviewer_binding(binding, entry, run)))
                && matches!(
                    message.kind,
                    MessageKind::Approved | MessageKind::ChangesRequested
                )
                && let (Some(request), Some(target)) = (message.in_reply_to, &message.review)
            {
                let _ = run.verdict(
                    entry.from_agent_id,
                    request,
                    target,
                    message.kind == MessageKind::Approved,
                );
            }
            // Every workflow-scope message is recorded, not only the ones that
            // moved the run. A run spends most of its life exchanging messages
            // that change no phase — the plan coming back, the implementer
            // reporting — and the pane was blank for all of it.
            append_history(run, entry, previous != (run.phase, run.review.clone()));
            record.cursor = Some(message.message_id);
        }
        Ok(())
    })
}

fn journal_offset(
    cursor: Option<OperationId>,
    messages: &[usagi_core::domain::agent_message::AgentMessage],
) -> usize {
    cursor
        .and_then(|cursor| {
            messages
                .iter()
                .position(|entry| entry.message.message_id == cursor)
        })
        .map_or(0, |index| index + 1)
}

fn original_reviewer_binding(
    binding: &usagi_core::domain::agent::DispatchBinding,
    entry: &usagi_core::domain::agent_message::AgentMessage,
    run: &WorkflowRun,
) -> bool {
    binding.run_id == entry.from_run_id
        && binding.worker.agent_id == entry.from_agent_id
        && binding.worker.session_id == Some(run.session)
        && binding.caller.agent_id == run.implementer
        && binding.caller.session_id == Some(run.session)
}

fn append_history(
    run: &mut WorkflowRun,
    entry: &usagi_core::domain::agent_message::AgentMessage,
    advanced: bool,
) {
    let message = &entry.message;
    // The reviewer is bound at its first review request, so before that the
    // only other participant that speaks is the planner the implementer
    // launched. Attributing it to the reviewer named a participant that had not
    // said anything yet.
    let actor = if entry.from_agent_id == run.implementer {
        run.agents.implementer.profile_id()
    } else if run.reviewer == Some(entry.from_agent_id) {
        run.agents.reviewer.profile_id()
    } else {
        run.agents.planner.profile_id()
    };
    run.history
        .push(usagi_core::domain::workflow::WorkflowHistoryEntry {
            id: message.message_id,
            actor: actor.into(),
            body: message.body.chars().take(512).collect(),
            at: Some(entry.created_at),
            kind: message.kind,
            advanced,
        });
    if run.history.len() > HISTORY_LIMIT {
        run.history.remove(0);
    }
}

/// How many history entries one run keeps. Recording every message rather than
/// only the phase-moving ones makes this bound load-bearing.
const HISTORY_LIMIT: usize = 100;

/// The repository conventions a run started from an issue has to satisfy before
/// its PR counts as ready.
///
/// The daemon verifies both independently at `PR ready`, so stating them in the
/// launch prompt is what lets the implementer satisfy them in the same PR rather
/// than discovering the refusal afterwards.
#[must_use]
pub fn issue_conventions(issue: u32) -> String {
    format!(
        " This run implements issue #{issue}. Write `Internal-Issue: #{issue}` in the PR body, and put the same PR's diff in charge of marking that issue done: update `.usagi/issues/` in this worktree so the issue's status is `done` before you open the PR. Both are verified before the workflow reports the PR as ready."
    )
}

#[must_use]
pub fn initial_prompt(
    goal: &str,
    agents: usagi_core::domain::workflow::WorkflowAgents,
    revision_limit: u8,
) -> String {
    let implementer = agents.implementer.profile_id();
    let reviewer = agents.reviewer.profile_id();
    let planner = agents.planner.profile_id();
    format!(
        "Session Workflow: implementation and review. You are {implementer}, the implementation owner. Work only in this session. First use agent_handoff to launch a separate planning Agent (runtime={planner}, model=default) INSIDE THIS SAME SESSION with planning-only instructions. Ask it to inspect the goal and return a concrete implementation plan via agent_message; no edits. Wait for its plan and acknowledge its message before implementing. The planner is not the reviewer; launch a separate review Agent later. Implement, test, and commit the user's goal. Use agent_handoff to launch the reviewer (runtime={reviewer}, model=default) INSIDE THIS SAME SESSION, with review-only instructions. Do not create a review session. Send review_request via agent_message to that exact Agent with full base_sha and head_sha. The reviewer must reply approved or changes_requested with the identical review target and in_reply_to request ID; no edits. Read agent_messages, acknowledge processed messages, fix and commit findings then request another review. Stop and ask the user after {revision_limit} revision rounds. After approval verify latest HEAD and checks, prepare the PR, report its URL. Never merge. Treat later Workflow instruction IDs as idempotent: process each ID at most once. If authentication or policy blocks handoff, report the error; do not claim success.\n\nUser goal:\n{goal}"
    )
}

/// Shortest gap between two `gh pr view` reads for the same approved HEAD.
const VERIFICATION_TTL_MS: u64 = 15_000;
/// Longest gap the backoff may grow to while the answer stays "still waiting".
const VERIFICATION_MAX_BACKOFF_MS: u64 = 300_000;
/// How many sessions the cache remembers at once.
const MAX_CACHED_SESSIONS: usize = 64;

/// One remembered `gh pr view` read.
struct CachedRead {
    /// The approved HEAD the read was made for. A different HEAD is different
    /// evidence, never a cache hit.
    head_sha: String,
    /// The PR the read was made against. Two PRs can share a head commit (a
    /// backport opened from the same HEAD), and the entry chosen from the
    /// inventory can change between passes, so serving one PR's checks as
    /// another's would publish a URL whose checks were never read.
    url: String,
    output: String,
    read_at_ms: u64,
    /// Consecutive reads that answered "still waiting". This is what the backoff
    /// lengthens: a PR whose checks are running says the same thing every time,
    /// and asking GitHub every sweep buys nothing.
    waits: u32,
}

/// Rate-limits the one expensive part of verification.
///
/// `Verifying` and `Ready` re-verify on every sweep *and* on every snapshot the
/// open tab asks for, and each one shelled out to `gh pr view`. The local git
/// probes are cheap and stay on every pass — they are also the TOCTOU fence — so
/// only the GitHub read is cached.
#[derive(Default)]
pub struct VerificationCache {
    entries: std::collections::BTreeMap<SessionId, CachedRead>,
}

impl VerificationCache {
    /// The remembered read for this HEAD, if another one is not due yet.
    #[must_use]
    pub fn fresh(
        &self,
        session: SessionId,
        head_sha: &str,
        url: &str,
        now_ms: u64,
    ) -> Option<&str> {
        let entry = self.entries.get(&session)?;
        if entry.head_sha != head_sha || entry.url != url {
            return None;
        }
        let wait = VERIFICATION_TTL_MS
            .saturating_mul(1_u64.checked_shl(entry.waits).unwrap_or(u64::MAX))
            .min(VERIFICATION_MAX_BACKOFF_MS);
        (now_ms.saturating_sub(entry.read_at_ms) < wait).then_some(entry.output.as_str())
    }

    /// Remember what GitHub answered. `waiting` lengthens the next gap.
    pub fn record(
        &mut self,
        session: SessionId,
        head_sha: &str,
        url: &str,
        now_ms: u64,
        output: String,
        waiting: bool,
    ) {
        let waits = match self.entries.get(&session) {
            // A streak only continues for the same evidence and the same answer.
            Some(entry) if entry.head_sha == head_sha && entry.url == url && waiting => {
                entry.waits.saturating_add(1)
            }
            _ => 0,
        };
        // One entry per session, and a session that disappeared while its run was
        // live is never enumerated again. Shed the least recently read rather
        // than letting a daemon's lifetime accumulate payloads.
        if self.entries.len() >= MAX_CACHED_SESSIONS
            && !self.entries.contains_key(&session)
            && let Some(stalest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.read_at_ms)
                .map(|(session, _)| *session)
        {
            self.entries.remove(&stalest);
        }
        self.entries.insert(
            session,
            CachedRead {
                head_sha: head_sha.to_owned(),
                url: url.to_owned(),
                output,
                read_at_ms: now_ms,
                waits,
            },
        );
    }

    /// Drop what is remembered for a session that is no longer being verified,
    /// so re-entering `Verifying` reads GitHub rather than an old answer.
    pub fn forget(&mut self, session: SessionId) {
        self.entries.remove(&session);
    }
}

/// Whether a verification refusal is "not yet" rather than "no".
///
/// Only the former is worth backing off: the others are answered by changing
/// something, and the next pass will see that change through the local probes.
#[must_use]
pub fn is_waiting(reason: &str) -> bool {
    reason.starts_with("Waiting")
}

/// Independently verify an approved HEAD against a clean worktree and GitHub.
/// Unknown, stale and missing evidence remain a concrete pending reason.
/// # Errors
/// Returns a safe pending reason for every absent or mismatched proof.
pub fn verify_pr(
    git: &dyn usagi_core::infrastructure::git::GitRunner,
    view: &mut dyn FnMut(&str) -> Result<String, &'static str>,
    directory: &std::path::Path,
    target: &usagi_core::domain::agent_message::ReviewTarget,
    entries: &[usagi_core::domain::pr_inventory::PrEntry],
    issue: Option<u32>,
) -> Result<String, &'static str> {
    let head = git
        .run(directory, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "Could not read worktree HEAD")?;
    if !head.success || head.stdout.trim() != target.head_sha {
        return Err("Worktree HEAD changed; a new review is required");
    }
    let status = git
        .run(
            directory,
            &["status", "--porcelain", "--untracked-files=all"],
        )
        .map_err(|_| "Could not inspect worktree changes")?;
    if !status.success || !status.stdout.trim().is_empty() {
        return Err("Worktree has uncommitted changes");
    }
    let entry = entries
        .iter()
        .find(|entry| entry.head_oid.as_deref() == Some(target.head_sha.as_str()))
        .ok_or("Waiting for a PR for the approved HEAD")?;
    let output = view(entry.url())?;
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|_| "PR verification response is invalid")?;
    let view = super::pr_inventory::parse_gh_pr_view(&output)
        .ok_or("PR verification response is incomplete")?;
    if view.head_oid != target.head_sha {
        return Err("PR HEAD changed; a new review is required");
    }
    if view.draft {
        return Err("Waiting for the PR to be marked ready for review");
    }
    if view.checks != Some(usagi_core::domain::pr_inventory::PrChecksState::Passing) {
        return Err("Waiting for successful PR checks");
    }
    if view.state != usagi_core::domain::pr_inventory::PrState::Merged
        && (view.state != usagi_core::domain::pr_inventory::PrState::Open
            || value.get("mergeable").and_then(serde_json::Value::as_str) != Some("MERGEABLE"))
    {
        return Err("PR is neither merged nor open and conflict-free");
    }
    // A rollup contains only checks that already exist. GitHub's merge state
    // also accounts for required checks whose status has not been reported.
    if view.state == usagi_core::domain::pr_inventory::PrState::Open
        && !matches!(
            value
                .get("mergeStateStatus")
                .and_then(serde_json::Value::as_str),
            Some("CLEAN" | "HAS_HOOKS")
        )
    {
        return Err("Waiting for GitHub merge requirements to pass");
    }
    if view.review == Some(usagi_core::domain::pr_inventory::PrReviewDecision::ChangesRequested) {
        return Err("PR has unresolved review requests");
    }
    if let Some(issue) = issue {
        verify_issue_conventions(
            directory,
            issue,
            value.get("body").and_then(serde_json::Value::as_str),
        )?;
    }
    let status = git
        .run(
            directory,
            &["status", "--porcelain", "--untracked-files=all"],
        )
        .map_err(|_| "Could not recheck worktree changes")?;
    if !status.success || !status.stdout.trim().is_empty() {
        return Err("Worktree has uncommitted changes after verification");
    }
    let current = git
        .run(directory, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "Could not recheck worktree HEAD")?;
    if !current.success || current.stdout.trim() != target.head_sha {
        return Err("Worktree HEAD changed during verification");
    }
    Ok(entry.url().to_owned())
}

/// The two repository conventions an issue-backed PR has to satisfy.
///
/// Both are checked against what is actually there — the PR body GitHub returns
/// and the issue as it stands in this worktree — rather than the implementer's
/// report, for the same reason the rest of the verification is independent.
fn verify_issue_conventions(
    directory: &std::path::Path,
    issue: u32,
    body: Option<&str>,
) -> Result<(), &'static str> {
    let body = body.ok_or("Could not read the PR body")?;
    // The same shape the repository's own checker accepts: the marker starts the
    // line, tolerates spaces around the value, and appears exactly once. Reading
    // it more loosely here would report a PR as ready that CI then rejects.
    let markers = body
        .lines()
        .filter(|line| line.starts_with("Internal-Issue:"))
        .collect::<Vec<_>>();
    let [marker] = markers.as_slice() else {
        return Err("PR body needs exactly one Internal-Issue line");
    };
    if marker.trim_start_matches("Internal-Issue:").trim() != format!("#{issue}") {
        return Err("PR body names a different issue than this run implements");
    }
    let issue = usagi_core::usecase::issue::get(
        &usagi_core::infrastructure::store::issue::IssueStore::new(directory),
        issue,
    )
    .map_err(|_| "Could not read the issue this run implements")?
    .ok_or("The issue this run implements is missing from the worktree")?;
    if issue.status != usagi_core::domain::issue::IssueStatus::Done {
        return Err("The issue this run implements is not marked done in this PR");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::workflow::Recipient;

    #[test]
    fn workflow_agents_bind_to_intent_prompt_and_retry() {
        use usagi_core::domain::{settings::DefaultModel, workflow::WorkflowAgents};
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let agents = WorkflowAgents {
            planner: DefaultModel::Agy,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::OpenAi,
        };
        store
            .remember_workflow_defaults(
                workspace,
                usagi_core::domain::workflow::WorkflowDefaults {
                    agents,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(snapshot(&store, workspace, session).unwrap().agents, agents);
        assert!(store.remember_workflow_start(workspace, session).is_err());
        let command = WorkflowCommand::Start {
            goal: "Task".into(),
            agents,
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        admit(&store, workspace, session, operation, &command, None).unwrap();
        assert!(store.remember_workflow_start(workspace, session).is_err());
        let snapshot = snapshot(&store, workspace, session).unwrap();
        assert_eq!(snapshot.agents, agents);
        assert_eq!(snapshot.pending_start.unwrap().agents, agents);
        let conflict = WorkflowCommand::Start {
            goal: "Task".into(),
            agents: WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        assert!(admit(&store, workspace, session, operation, &conflict, None).is_err());
        bind(&store, workspace, session, operation, AgentId::new()).unwrap();
        let defaults = dir
            .path()
            .join("workflows")
            .join(workspace.as_str())
            .join("defaults.json");
        std::fs::remove_file(&defaults).unwrap();
        std::fs::create_dir(&defaults).unwrap();
        assert!(store.remember_workflow_start(workspace, session).is_err());
        assert!(
            !store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .preferences_saved
        );
        std::fs::remove_dir(&defaults).unwrap();
        store.remember_workflow_start(workspace, session).unwrap();
        assert_eq!(store.workflow_defaults(workspace).unwrap().agents, agents);
        store
            .remember_workflow_defaults(
                workspace,
                usagi_core::domain::workflow::WorkflowDefaults::default(),
            )
            .unwrap();
        store.remember_workflow_start(workspace, session).unwrap();
        assert_eq!(
            store.workflow_defaults(workspace).unwrap().agents,
            WorkflowAgents::default()
        );
        let run = store
            .workflow(workspace, session)
            .unwrap()
            .unwrap()
            .run
            .unwrap();
        assert_eq!(run.agents, agents);
        let prompt = initial_prompt(
            &run.goal,
            agents,
            usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        );
        assert!(prompt.contains("You are claude"));
        assert!(prompt.contains("runtime=agy"));
        assert!(prompt.contains("runtime=codex"));
        assert!(prompt.contains("Wait for its plan"));
        let legacy: WorkflowCommand =
            serde_json::from_str(r#"{"kind":"start","goal":"Task"}"#).unwrap();
        assert_eq!(legacy, conflict);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One session's whole finish/restart life stays in one place.
    fn finishing_frees_the_session_and_keeps_a_bounded_history() {
        use usagi_core::domain::workflow::{FINISHED_LIMIT, Outcome, WorkflowAgents};
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let start = |goal: &str| WorkflowCommand::Start {
            goal: goal.to_owned(),
            agents: WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        let record = || store.workflow(workspace, session).unwrap().unwrap();

        // Nothing to finish before anything started.
        assert!(
            admit(
                &store,
                workspace,
                session,
                OperationId::new(),
                &WorkflowCommand::Finish,
                None,
            )
            .is_err()
        );

        // A start that never launched is abandoned as the intent it was.
        let first = OperationId::new();
        admit(&store, workspace, session, first, &start("never ran"), None).unwrap();
        let finish = OperationId::new();
        admit(
            &store,
            workspace,
            session,
            finish,
            &WorkflowCommand::Finish,
            None,
        )
        .unwrap();
        let ended = record();
        assert_eq!(ended.finish, Some(finish));
        assert!(ended.run.is_none());
        assert_eq!(ended.finished.len(), 1);
        assert_eq!(ended.finished[0].id, first);
        assert_eq!(ended.finished[0].outcome, Outcome::Stopped);
        assert_eq!(ended.finished[0].phase, Phase::Starting);
        // A finished record is not mid-start, so nothing offers to retry it.
        assert!(
            projection(&store, workspace, session)
                .unwrap()
                .pending_start
                .is_none()
        );

        // The same request arriving twice ends it once; a different one is told
        // there is nothing left to end.
        admit(
            &store,
            workspace,
            session,
            finish,
            &WorkflowCommand::Finish,
            None,
        )
        .unwrap();
        assert_eq!(record().finished.len(), 1);
        assert!(
            admit(
                &store,
                workspace,
                session,
                OperationId::new(),
                &WorkflowCommand::Finish,
                None,
            )
            .is_err()
        );
        // So is an instruction: the run it would have joined is over.
        assert!(
            admit(
                &store,
                workspace,
                session,
                OperationId::new(),
                &WorkflowCommand::Instruct {
                    recipient: Recipient::Automatic,
                    body: "keep going".into(),
                },
                None,
            )
            .is_err()
        );

        // A launch still resolving when the person abandoned it cannot come
        // back: binding it would leave a record that is finished and running at
        // once, which nothing could move again.
        assert!(bind(&store, workspace, session, first, AgentId::new()).is_err());
        assert!(record().run.is_none());

        // A finished session takes a new start, and the stale retry of the run
        // it already buried does not resurrect it.
        assert!(admit(&store, workspace, session, first, &start("again"), None).is_err());
        let second = OperationId::new();
        admit(&store, workspace, session, second, &start("again"), None).unwrap();
        let started = record();
        assert!(started.finish.is_none());
        assert_eq!(started.operation, second);
        assert_eq!(started.goal, "again");
        assert_eq!(started.finished.len(), 1, "the archive carries over");
        bind(&store, workspace, session, second, AgentId::new()).unwrap();
        assert!(record().run.is_some());

        // Ending a run that reached `Ready` is a completion, and the archive
        // never grows past its cap.
        store
            .update_workflow(workspace, session, |value| {
                let record = value.as_mut().unwrap();
                let run = record.run.as_mut().unwrap();
                run.phase = Phase::Ready;
                run.pr_url = Some("https://example.test/pr/2".into());
                Ok(())
            })
            .unwrap();
        admit(
            &store,
            workspace,
            session,
            OperationId::new(),
            &WorkflowCommand::Finish,
            None,
        )
        .unwrap();
        let completed = record();
        assert_eq!(completed.finished.len(), 2);
        assert_eq!(completed.finished[1].outcome, Outcome::Completed);
        assert_eq!(
            completed.finished[1].pr_url.as_deref(),
            Some("https://example.test/pr/2")
        );
        assert_eq!(
            projection(&store, workspace, session).unwrap().finished,
            completed.finished
        );

        for round in 0..FINISHED_LIMIT {
            let operation = OperationId::new();
            admit(
                &store,
                workspace,
                session,
                operation,
                &start(&format!("round {round}")),
                None,
            )
            .unwrap();
            admit(
                &store,
                workspace,
                session,
                OperationId::new(),
                &WorkflowCommand::Finish,
                None,
            )
            .unwrap();
        }
        let capped = record();
        assert_eq!(capped.finished.len(), FINISHED_LIMIT);
        assert_eq!(capped.finished[0].goal, "round 0", "the oldest fall off");
        assert_eq!(
            capped.finished[FINISHED_LIMIT - 1].goal,
            format!("round {}", FINISHED_LIMIT - 1)
        );
    }

    #[test]
    fn an_issue_backed_retry_is_the_same_intent_even_after_the_issue_moved() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let start = |goal: &str| WorkflowCommand::Start {
            goal: goal.to_owned(),
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        admit(
            &store,
            workspace,
            session,
            operation,
            &start("issue #7\nstatus: todo"),
            Some(7),
        )
        .unwrap();

        // The run marks the issue `in-progress`, so the same request re-renders
        // a different goal. It is still the same intent, and the retry must not
        // look like a second workflow.
        admit(
            &store,
            workspace,
            session,
            operation,
            &start("issue #7\nstatus: in-progress"),
            Some(7),
        )
        .unwrap();
        // A different issue under the same operation is a conflict, and so is
        // dropping the issue reference.
        assert!(
            admit(
                &store,
                workspace,
                session,
                operation,
                &start("issue #8"),
                Some(8)
            )
            .is_err()
        );
        assert!(admit(&store, workspace, session, operation, &start("typed"), None).is_err());

        // The launch reads the issue from the admitted record, so a start that
        // is retried after a failure keeps implementing the same issue.
        bind(&store, workspace, session, operation, AgentId::new()).unwrap();
        assert_eq!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .issue,
            Some(7)
        );
    }

    #[test]
    fn issue_backed_runs_prove_both_repository_conventions() {
        use usagi_core::domain::issue::{IssuePriority, IssueStatus};
        use usagi_core::infrastructure::store::issue::IssueStore;
        use usagi_core::usecase::issue::{IssuePatch, NewIssue};
        let worktree = tempfile::tempdir().unwrap();
        let store = IssueStore::new(worktree.path());
        let issue = usagi_core::usecase::issue::create(
            &store,
            NewIssue {
                title: "fix(daemon): close the loop".into(),
                priority: IssuePriority::High,
                body: "body".into(),
                ..Default::default()
            },
            chrono::Utc::now(),
        )
        .unwrap();
        let body = format!("Internal-Issue: #{}\n", issue.number);

        // A PR body that never names the issue, and a PR body that does while
        // the issue is still open, are both incomplete.
        assert_eq!(
            verify_issue_conventions(worktree.path(), issue.number, Some("no marker")),
            Err("PR body needs exactly one Internal-Issue line")
        );
        // Two markers are what the repository's checker rejects outright, so the
        // workflow must not report such a PR as ready either.
        assert_eq!(
            verify_issue_conventions(
                worktree.path(),
                issue.number,
                Some(&format!("{body}Internal-Issue: none\n"))
            ),
            Err("PR body needs exactly one Internal-Issue line")
        );
        assert_eq!(
            verify_issue_conventions(worktree.path(), issue.number, Some("Internal-Issue: none")),
            Err("PR body names a different issue than this run implements")
        );
        assert_eq!(
            verify_issue_conventions(worktree.path(), issue.number, None),
            Err("Could not read the PR body")
        );
        assert_eq!(
            verify_issue_conventions(worktree.path(), issue.number, Some(&body)),
            Err("The issue this run implements is not marked done in this PR")
        );
        // An issue that is not in this worktree at all cannot be proven done.
        assert_eq!(
            verify_issue_conventions(
                worktree.path(),
                issue.number + 1,
                Some(&format!("Internal-Issue: #{}\n", issue.number + 1))
            ),
            Err("The issue this run implements is missing from the worktree")
        );

        usagi_core::usecase::issue::update(
            &store,
            issue.number,
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(
            verify_issue_conventions(worktree.path(), issue.number, Some(&body)),
            Ok(())
        );
        // The marker starts its line: a mention inside prose is not one, and a
        // sentence trailing on the same line is not the marker either.
        assert_eq!(
            verify_issue_conventions(
                worktree.path(),
                issue.number,
                Some(&format!(
                    "see Internal-Issue: #{} for context",
                    issue.number
                ))
            ),
            Err("PR body needs exactly one Internal-Issue line")
        );
        assert_eq!(
            verify_issue_conventions(
                worktree.path(),
                issue.number,
                Some(&format!("Internal-Issue: #{} and more", issue.number))
            ),
            Err("PR body names a different issue than this run implements")
        );
        // Spaces around the value are what the checker tolerates, so accept them.
        assert_eq!(
            verify_issue_conventions(
                worktree.path(),
                issue.number,
                Some(&format!("Internal-Issue:   #{}  ", issue.number))
            ),
            Ok(())
        );
    }

    #[test]
    fn history_retains_only_a_bounded_tail_and_bounded_bodies() {
        let mut run = WorkflowRun {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            id: OperationId::new(),
            session: SessionId::new(),
            goal: "task".into(),
            implementer: AgentId::new(),
            reviewer: None,
            phase: Phase::Implementing,
            revision_limit: 3,
            revisions: 0,
            review: None,
            waiting_reason: None,
            pr_url: None,
            issue: None,
            instructions: Vec::new(),
            history: Vec::new(),
        };
        let entry = usagi_core::domain::agent_message::AgentMessage {
            from_agent_id: run.implementer,
            from_run_id: run.id,
            message: usagi_core::domain::agent_message::SendMessage {
                message_id: OperationId::new(),
                to_agent_id: AgentId::new(),
                kind: MessageKind::Message,
                body: "あ".repeat(600),
                in_reply_to: None,
                review: None,
            },
            created_at: chrono::Utc::now(),
            acknowledged: false,
        };
        for index in 0..101 {
            append_history(&mut run, &entry, index % 2 == 0);
        }
        assert_eq!(run.history.len(), 100);
        assert_eq!(run.history[0].body.chars().count(), 512);
        // Every entry now carries when it happened and what it was, and the
        // ones that moved the run stay distinguishable from the ones that did
        // not.
        assert!(run.history.iter().all(|item| item.at.is_some()));
        assert!(run.history.iter().any(|item| item.advanced));
        assert!(run.history.iter().any(|item| !item.advanced));

        // Each participant is named by the role it actually holds. Before the
        // reviewer is bound, the only other agent that speaks is the planner the
        // implementer launched; attributing it to the reviewer would name a
        // participant that has not said anything yet.
        let agents = usagi_core::domain::workflow::WorkflowAgents {
            planner: usagi_core::domain::settings::DefaultModel::Agy,
            implementer: usagi_core::domain::settings::DefaultModel::Claude,
            reviewer: usagi_core::domain::settings::DefaultModel::OpenAi,
        };
        run.agents = agents;
        let reviewer = AgentId::new();
        let planner = AgentId::new();
        // Bound before the closure so it does not hold a borrow of `run` across
        // the `&mut run` calls below.
        let implementer = run.implementer;
        let from = |agent: AgentId| usagi_core::domain::agent_message::AgentMessage {
            from_agent_id: agent,
            from_run_id: OperationId::new(),
            message: usagi_core::domain::agent_message::SendMessage {
                message_id: OperationId::new(),
                to_agent_id: implementer,
                kind: MessageKind::Message,
                body: "spoke".into(),
                in_reply_to: None,
                review: None,
            },
            created_at: chrono::Utc::now(),
            acknowledged: false,
        };

        run.history.clear();
        append_history(&mut run, &from(implementer), false);
        // Not the implementer and not the bound reviewer: the planner.
        append_history(&mut run, &from(planner), false);
        run.reviewer = Some(reviewer);
        append_history(&mut run, &from(reviewer), false);
        assert_eq!(
            run.history
                .iter()
                .map(|entry| entry.actor.as_str())
                .collect::<Vec<_>>(),
            vec![
                agents.implementer.profile_id(),
                agents.planner.profile_id(),
                agents.reviewer.profile_id(),
            ]
        );
    }

    #[test]
    fn workflow_peer_review_reconciles_exact_bindings_once_without_acknowledging() {
        for provider in usagi_core::domain::settings::DefaultModel::ALL {
            check_workflow_peer_review(provider);
        }
    }

    #[allow(clippy::too_many_lines)] // One end-to-end journal fixture preserves request/verdict identity.
    fn check_workflow_peer_review(reviewer_provider: usagi_core::domain::settings::DefaultModel) {
        use usagi_core::domain::agent::{
            Agent, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun,
            ModelSelector, RunStatus, WorkerRef,
        };
        use usagi_core::domain::agent_message::{ReviewTarget, SendMessage};
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let reviewer_run = OperationId::new();
        let implementer = AgentId::new();
        let reviewer = AgentId::new();
        for (id, runtime, run) in [
            (implementer, "codex", operation),
            (reviewer, reviewer_provider.profile_id(), reviewer_run),
        ] {
            store
                .upsert_agent(
                    workspace,
                    Agent {
                        agent_id: id,
                        session_id: Some(session),
                        runtime: AgentProfileId::new(runtime).unwrap(),
                        model: ModelSelector::new("default").unwrap(),
                        status: AgentStatus::Running,
                        current_run: Some(run),
                    },
                )
                .unwrap();
            store
                .upsert_run(DispatchRun {
                    run_id: run,
                    agent_id: id,
                    prompt: "task".into(),
                    started_at: chrono::Utc::now(),
                    ended_at: None,
                    status: RunStatus::Running,
                })
                .unwrap();
        }
        let caller = CallerRef {
            agent_id: implementer,
            session_id: Some(session),
        };
        store
            .upsert_binding(DispatchBinding {
                run_id: reviewer_run,
                caller: caller.clone(),
                worker: WorkerRef {
                    agent_id: reviewer,
                    session_id: Some(session),
                },
            })
            .unwrap();
        admit(
            &store,
            workspace,
            session,
            operation,
            &WorkflowCommand::Start {
                goal: "Task".into(),
                revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                agents: usagi_core::domain::workflow::WorkflowAgents {
                    reviewer: reviewer_provider,
                    ..usagi_core::domain::workflow::WorkflowAgents::default()
                },
            },
            None,
        )
        .unwrap();
        bind(&store, workspace, session, operation, implementer).unwrap();
        let target = ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        };
        let request = OperationId::new();
        store
            .send_message(
                workspace,
                &caller,
                operation,
                SendMessage {
                    message_id: request,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "Review changes".into(),
                    in_reply_to: None,
                    review: Some(target.clone()),
                },
            )
            .unwrap();
        let reviewing = snapshot(&store, workspace, session).unwrap().run.unwrap();
        assert_eq!(reviewing.phase, Phase::Reviewing);
        assert_eq!(reviewing.reviewer, Some(reviewer));
        assert_eq!(reviewing.history.len(), 1);
        assert_eq!(
            snapshot(&store, workspace, session)
                .unwrap()
                .run
                .unwrap()
                .history
                .len(),
            1
        );
        store
            .send_message(
                workspace,
                &CallerRef {
                    agent_id: reviewer,
                    session_id: Some(session),
                },
                reviewer_run,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: implementer,
                    kind: MessageKind::Approved,
                    body: "Approved".into(),
                    in_reply_to: Some(request),
                    review: Some(target),
                },
            )
            .unwrap();
        let approved = snapshot(&store, workspace, session).unwrap().run.unwrap();
        assert_eq!(approved.phase, Phase::Verifying);
        assert_eq!(approved.history.len(), 2);
        let next_request = OperationId::new();
        let next_target = ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "c".repeat(40),
        };
        store
            .send_message(
                workspace,
                &caller,
                operation,
                SendMessage {
                    message_id: next_request,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "Review follow-up implementation".into(),
                    in_reply_to: None,
                    review: Some(next_target.clone()),
                },
            )
            .unwrap();
        store
            .send_message(
                workspace,
                &CallerRef {
                    agent_id: reviewer,
                    session_id: Some(session),
                },
                reviewer_run,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: implementer,
                    kind: MessageKind::Approved,
                    body: "Approved follow-up".into(),
                    in_reply_to: Some(next_request),
                    review: Some(next_target),
                },
            )
            .unwrap();
        let latest = OperationId::new();
        store
            .send_message(
                workspace,
                &caller,
                operation,
                SendMessage {
                    message_id: latest,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "One more change while tab closed".into(),
                    in_reply_to: None,
                    review: Some(ReviewTarget {
                        base_sha: "a".repeat(40),
                        head_sha: "d".repeat(40),
                    }),
                },
            )
            .unwrap();
        let current = snapshot(&store, workspace, session).unwrap().run.unwrap();
        assert_eq!(current.phase, Phase::Reviewing);
        assert_eq!(current.review.unwrap().request, latest);
        assert_eq!(current.history.len(), 5);
        assert!(
            store
                .workflow_messages(workspace, session)
                .unwrap()
                .iter()
                .all(|message| !message.acknowledged)
        );
        let resumed = OperationId::new();
        let reviewer_caller = CallerRef {
            agent_id: reviewer,
            session_id: Some(session),
        };
        let verdict = |id| SendMessage {
            message_id: id,
            to_agent_id: implementer,
            kind: MessageKind::Approved,
            body: "Resumed reviewer verdict".into(),
            in_reply_to: Some(latest),
            review: Some(ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: "d".repeat(40),
            }),
        };
        store
            .send_message(
                workspace,
                &reviewer_caller,
                resumed,
                verdict(OperationId::new()),
            )
            .unwrap();
        assert_eq!(
            snapshot(&store, workspace, session)
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Reviewing
        );
        store
            .update_workflow(workspace, session, |value| {
                let record = value.as_mut().unwrap();
                record.authorized_operations.push((reviewer, resumed));
                // Replay the identical authenticated journal against the trusted
                // lineage, without appending a second verdict for one request.
                record.cursor = Some(latest);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            snapshot(&store, workspace, session)
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Verifying
        );
        store
            .send_message(
                workspace,
                &reviewer_caller,
                resumed,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: implementer,
                    kind: MessageKind::Message,
                    body: "Additional context, not a verdict".into(),
                    in_reply_to: None,
                    review: None,
                },
            )
            .unwrap();
        assert_eq!(
            snapshot(&store, workspace, session)
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Verifying
        );
    }

    struct Git;
    impl usagi_core::infrastructure::git::GitRunner for Git {
        fn run(
            &self,
            _: &std::path::Path,
            args: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            Ok(usagi_core::infrastructure::git::GitOutput {
                success: true,
                stdout: if args[0] == "status" {
                    String::new()
                } else {
                    "a".repeat(40)
                },
                stderr: String::new(),
            })
        }
    }
    /// The PR view `verify_pr` asks for, as the caller now supplies it.
    ///
    /// `verify_pr` no longer runs `gh` itself: the caller decides whether to
    /// shell out or answer from [`VerificationCache`]. These tests are about the
    /// judgement applied to the answer, so they supply it directly.
    fn gh_view(output: &str) -> impl FnMut(&str) -> Result<String, &'static str> {
        let output = output.to_owned();
        move |url: &str| {
            assert!(url.starts_with("https://github.com/"));
            if output == "unavailable" {
                return Err("Could not refresh PR checks");
            }
            Ok(output.clone())
        }
    }
    struct FailingGit {
        calls: std::cell::Cell<usize>,
        at: usize,
        mode: u8,
    }
    impl usagi_core::infrastructure::git::GitRunner for FailingGit {
        fn run(
            &self,
            path: &std::path::Path,
            args: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call == self.at {
                if self.mode == 0 {
                    anyhow::bail!("Git unavailable");
                }
                return Ok(usagi_core::infrastructure::git::GitOutput {
                    success: self.mode != 1,
                    stdout: "changed".into(),
                    stderr: String::new(),
                });
            }
            Git.run(path, args)
        }
    }
    #[test]
    fn workflow_verification_waits_for_missing_required_checks() {
        let target = usagi_core::domain::agent_message::ReviewTarget {
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
        };
        let mut entry = usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::extract(b"https://github.com/owner/repo/pull/1")
                .remove(0),
        );
        entry.head_oid = Some(target.head_sha.clone());
        // Lint exists and passes, but coverage has not published any check yet.
        let mut value = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"statusCheckRollup":[{"name":"lint","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE"});
        for state in [
            serde_json::Value::Null,
            serde_json::json!("BLOCKED"),
            serde_json::json!("UNKNOWN"),
            serde_json::json!("BEHIND"),
            serde_json::json!("UNSTABLE"),
        ] {
            value["mergeStateStatus"] = state;
            assert_eq!(
                verify_pr(
                    &Git,
                    &mut gh_view(&value.to_string()),
                    std::path::Path::new("/fixture"),
                    &target,
                    std::slice::from_ref(&entry),
                    None
                ),
                Err("Waiting for GitHub merge requirements to pass")
            );
        }
        for state in ["CLEAN", "HAS_HOOKS"] {
            value["mergeStateStatus"] = serde_json::json!(state);
            assert_eq!(
                verify_pr(
                    &Git,
                    &mut gh_view(&value.to_string()),
                    std::path::Path::new("/fixture"),
                    &target,
                    std::slice::from_ref(&entry),
                    None
                ),
                Ok(entry.url().to_owned())
            );
        }
    }

    #[test]
    fn workflow_verification_finds_untracked_files_hidden_by_git_config() {
        use usagi_core::infrastructure::git::GitRunner;
        let directory = tempfile::tempdir().unwrap();
        let git = crate::infrastructure::session_worktree::SystemGit;
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["config", "user.name", "Review"],
            vec!["config", "user.email", "review@example.invalid"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "status.showUntrackedFiles", "no"],
            vec!["commit", "--allow-empty", "--quiet", "-m", "base"],
        ] {
            assert!(git.run(directory.path(), &args).unwrap().success);
        }
        let head = git
            .run(directory.path(), &["rev-parse", "HEAD"])
            .unwrap()
            .stdout
            .trim()
            .to_owned();
        let target = usagi_core::domain::agent_message::ReviewTarget {
            base_sha: head.clone(),
            head_sha: head,
        };
        let mut entry = usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::extract(b"https://github.com/owner/repo/pull/1")
                .remove(0),
        );
        entry.head_oid = Some(target.head_sha.clone());
        let value = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"statusCheckRollup":[{"conclusion":"SUCCESS"}],"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}).to_string();
        let file = directory.path().join("uncommitted.rs");
        // Hidden untracked files must be caught on both sides of the remote read.
        for after_read in [false, true] {
            if !after_read {
                std::fs::write(&file, "implementation").unwrap();
            }
            let mut view = |_: &str| {
                std::fs::write(&file, "implementation").unwrap();
                Ok(value.clone())
            };
            let result = verify_pr(
                &git,
                &mut view,
                directory.path(),
                &target,
                std::slice::from_ref(&entry),
                None,
            );
            assert_eq!(
                result,
                Err(if after_read {
                    "Worktree has uncommitted changes after verification"
                } else {
                    "Worktree has uncommitted changes"
                })
            );
            std::fs::remove_file(&file).unwrap();
        }
    }

    #[test]
    fn workflow_verification_rejects_every_git_probe_failure_or_race() {
        let target = usagi_core::domain::agent_message::ReviewTarget {
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
        };
        let mut entry = usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::extract(b"https://github.com/owner/repo/pull/1")
                .remove(0),
        );
        entry.head_oid = Some(target.head_sha.clone());
        let output=serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}).to_string();
        for at in 0..4 {
            for mode in 0..3 {
                assert!(
                    verify_pr(
                        &FailingGit {
                            calls: std::cell::Cell::new(0),
                            at,
                            mode
                        },
                        &mut gh_view(&output.clone()),
                        std::path::Path::new("/fixture"),
                        &target,
                        std::slice::from_ref(&entry),
                        None,
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn workflow_verification_rechecks_cleanliness_after_the_pr_response() {
        struct EditedWorktreeGit(std::cell::Cell<bool>);
        impl usagi_core::infrastructure::git::GitRunner for EditedWorktreeGit {
            fn run(
                &self,
                path: &std::path::Path,
                args: &[&str],
            ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
                let mut output = Git.run(path, args)?;
                if args[0] == "status" && self.0.get() {
                    output.stdout = " M tracked.txt\n".into();
                }
                Ok(output)
            }
        }
        let target = usagi_core::domain::agent_message::ReviewTarget {
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
        };
        let mut entry = usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::extract(b"https://github.com/owner/repo/pull/1")
                .remove(0),
        );
        entry.head_oid = Some(target.head_sha.clone());
        let git = EditedWorktreeGit(std::cell::Cell::new(false));
        let mut view = |_: &str| {
            // Both initial probes succeeded. A writer changes the worktree
            // while this request is outstanding, without moving HEAD.
            git.0.set(true);
            Ok(serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"statusCheckRollup":[{"conclusion":"SUCCESS"}],"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}).to_string())
        };
        assert_eq!(
            verify_pr(
                &git,
                &mut view,
                std::path::Path::new("/fixture"),
                &target,
                &[entry],
                None
            ),
            Err("Worktree has uncommitted changes after verification")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One verification fixture walks every refusal a PR can earn.
    fn workflow_verification_requires_fresh_matching_ready_pr_evidence() {
        let target = usagi_core::domain::agent_message::ReviewTarget {
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
        };
        let mut entry = usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::extract(b"https://github.com/owner/repo/pull/1")
                .remove(0),
        );
        entry.head_oid = Some(target.head_sha.clone());
        let mut value = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"});
        let directory = std::path::Path::new("/fixture");
        // Verification names the PR it matched, so the notice a human reads can
        // link to it.
        assert_eq!(
            verify_pr(
                &Git,
                &mut gh_view(&value.to_string()),
                directory,
                &target,
                std::slice::from_ref(&entry),
                None,
            ),
            Ok(entry.url().to_owned())
        );
        // An issue-backed run has to satisfy the repository conventions too, and
        // this PR body names no issue.
        let mut without_marker = value.clone();
        without_marker["body"] = serde_json::json!("no marker here");
        assert_eq!(
            verify_pr(
                &Git,
                &mut gh_view(&without_marker.to_string()),
                directory,
                &target,
                std::slice::from_ref(&entry),
                Some(742),
            ),
            Err("PR body needs exactly one Internal-Issue line")
        );
        assert!(
            verify_pr(
                &Git,
                &mut gh_view(&value.to_string()),
                directory,
                &target,
                &[],
                None
            )
            .is_err()
        );
        assert!(
            verify_pr(
                &Git,
                &mut gh_view("{}"),
                directory,
                &target,
                std::slice::from_ref(&entry),
                None,
            )
            .is_err()
        );
        assert!(
            verify_pr(
                &Git,
                &mut gh_view("unavailable"),
                directory,
                &target,
                std::slice::from_ref(&entry),
                None,
            )
            .is_err()
        );
        value["state"] = serde_json::json!("MERGED");
        value["mergeable"] = serde_json::json!("UNKNOWN");
        assert!(
            verify_pr(
                &Git,
                &mut gh_view(&value.to_string()),
                directory,
                &target,
                std::slice::from_ref(&entry),
                None,
            )
            .is_ok()
        );
        value["state"] = serde_json::json!("OPEN");
        value["mergeable"] = serde_json::json!("MERGEABLE");
        for (field, bad) in [
            ("headRefOid", serde_json::json!("c".repeat(40))),
            ("isDraft", serde_json::json!(true)),
            ("statusCheckRollup", serde_json::json!([])),
            ("mergeable", serde_json::json!("CONFLICTING")),
            ("state", serde_json::json!("CLOSED")),
            ("reviewDecision", serde_json::json!("CHANGES_REQUESTED")),
        ] {
            let old = value[field].clone();
            value[field] = bad;
            assert!(
                verify_pr(
                    &Git,
                    &mut gh_view(&value.to_string()),
                    directory,
                    &target,
                    std::slice::from_ref(&entry),
                    None,
                )
                .is_err()
            );
            value[field] = old;
        }
        assert!(
            verify_pr(
                &Git,
                &mut gh_view("invalid"),
                directory,
                &target,
                &[entry],
                None,
            )
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One command fixture covers admission, replay and refusal together.
    fn workflow_commands_are_durable_and_idempotent_and_do_not_invent_review() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        assert!(snapshot(&store, workspace, session).unwrap().run.is_none());
        let start = WorkflowCommand::Start {
            goal: "implement authentication".into(),
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        for goal in [String::new(), "\0".into(), "x".repeat(16385)] {
            assert!(
                admit(
                    &store,
                    workspace,
                    session,
                    operation,
                    &WorkflowCommand::Start {
                        goal,
                        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                        revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                    },
                    None,
                )
                .is_err()
            );
        }
        let instruct = WorkflowCommand::Instruct {
            recipient: Recipient::Automatic,
            body: "check errors".into(),
        };
        assert!(admit(&store, workspace, session, operation, &instruct, None).is_err());
        admit(&store, workspace, session, operation, &start, None).unwrap();
        admit(&store, workspace, session, operation, &start, None).unwrap();
        store
            .update_workflow(workspace, session, |record| {
                record.as_mut().unwrap().start_error = Some("authentication needed".into());
                Ok(())
            })
            .unwrap();
        let pending = snapshot(&DispatchStore::new(dir.path()), workspace, session)
            .unwrap()
            .pending_start
            .unwrap();
        assert_eq!(pending.operation_id, operation);
        assert_eq!(pending.goal, "implement authentication");
        assert_eq!(pending.error.as_deref(), Some("authentication needed"));
        admit(
            &store,
            workspace,
            session,
            pending.operation_id,
            &WorkflowCommand::Start {
                goal: pending.goal,
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
            },
            None,
        )
        .unwrap();
        assert!(admit(&store, workspace, session, OperationId::new(), &start, None).is_err());
        assert!(snapshot(&store, workspace, session).unwrap().run.is_none());
        assert!(
            admit(
                &store,
                workspace,
                session,
                OperationId::new(),
                &instruct,
                None
            )
            .is_err()
        );
        let implementer = AgentId::new();
        bind(&store, workspace, session, operation, implementer).unwrap();
        bind(&store, workspace, session, operation, implementer).unwrap();
        assert!(bind(&store, workspace, session, operation, AgentId::new()).is_err());
        assert!(bind(&store, workspace, session, OperationId::new(), implementer).is_err());
        assert!(bind(&store, workspace, SessionId::new(), operation, implementer).is_err());
        assert!(admit(&store, workspace, session, operation, &instruct, None).is_err());
        let instruction = OperationId::new();
        admit(&store, workspace, session, instruction, &instruct, None).unwrap();
        admit(&store, workspace, session, instruction, &instruct, None).unwrap();
        let run = snapshot(&store, workspace, session).unwrap().run.unwrap();
        assert_eq!(run.instructions.len(), 1);
        assert_eq!(run.instructions[0].recipient, implementer);
        assert_eq!(run.reviewer, None);
        assert_eq!(run.phase, Phase::Implementing);
        assert!(
            initial_prompt(
                &run.goal,
                run.agents,
                usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT
            )
            .contains("agent_handoff")
        );
        assert!(
            initial_prompt(
                &"x".repeat(16384),
                usagi_core::domain::workflow::WorkflowAgents::default(),
                usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
            )
            .len()
                < 24 * 1024
        );
    }

    #[test]
    fn a_start_outside_the_revision_range_is_refused_and_a_valid_one_is_bound() {
        use usagi_core::domain::workflow::{MAX_REVISION_LIMIT, WorkflowAgents};
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let start = |limit: u8| WorkflowCommand::Start {
            goal: "Task".into(),
            agents: WorkflowAgents::default(),
            revision_limit: limit,
        };
        for refused in [0, MAX_REVISION_LIMIT + 1] {
            let session = SessionId::new();
            assert!(
                admit(
                    &store,
                    workspace,
                    session,
                    OperationId::new(),
                    &start(refused),
                    None
                )
                .is_err(),
                "{refused} is outside the domain's range"
            );
            // Nothing durable was created for a refused number.
            assert!(store.workflow(workspace, session).unwrap().is_none());
        }

        // A number inside the range reaches the run the launch binds, instead of
        // the constant every construction site used to hard-code.
        let session = SessionId::new();
        let operation = OperationId::new();
        admit(&store, workspace, session, operation, &start(9), None).unwrap();
        bind(&store, workspace, session, operation, AgentId::new()).unwrap();
        let run = store
            .workflow(workspace, session)
            .unwrap()
            .and_then(|record| record.run)
            .expect("the run is bound");
        assert_eq!(run.revision_limit, 9);
        assert!(run.is_valid());
        // The launch prompt states the same number it will actually stop at.
        assert!(
            initial_prompt(&run.goal, run.agents, run.revision_limit)
                .contains("after 9 revision rounds")
        );

        // A retry that asks for a different number is a different intent, not the
        // same one: admitting it must not quietly change a live run's limit.
        assert!(
            admit(&store, workspace, session, operation, &start(4), None).is_err(),
            "the limit is part of the start's identity"
        );
    }

    #[test]
    fn the_verification_cache_spends_one_github_read_per_window_and_backs_off() {
        use usagi_core::domain::id::SessionId;
        let session = SessionId::new();
        let head = "a".repeat(40);
        let url = "https://github.com/owner/repo/pull/1";
        let mut cache = VerificationCache::default();

        // Nothing remembered: the first pass has to read.
        assert!(cache.fresh(session, &head, url, 0).is_none());
        cache.record(session, &head, url, 0, "first".into(), false);

        // Inside the window every further pass is answered without GitHub.
        for now in [0, 1, VERIFICATION_TTL_MS - 1] {
            assert_eq!(cache.fresh(session, &head, url, now), Some("first"));
        }
        // At the window's edge another read is due.
        assert!(
            cache
                .fresh(session, &head, url, VERIFICATION_TTL_MS)
                .is_none()
        );

        // A "still waiting" answer doubles the next gap, and keeps doubling.
        let mut at = VERIFICATION_TTL_MS;
        let mut expected = VERIFICATION_TTL_MS * 2;
        for _ in 0..3 {
            cache.record(session, &head, url, at, "waiting".into(), true);
            assert_eq!(
                cache.fresh(session, &head, url, at + expected - 1),
                Some("waiting")
            );
            assert!(cache.fresh(session, &head, url, at + expected).is_none());
            at += expected;
            expected *= 2;
        }

        // The backoff stops growing at the cap rather than running away.
        for _ in 0..40 {
            cache.record(session, &head, url, at, "waiting".into(), true);
        }
        assert_eq!(
            cache.fresh(session, &head, url, at + VERIFICATION_MAX_BACKOFF_MS - 1),
            Some("waiting")
        );
        assert!(
            cache
                .fresh(session, &head, url, at + VERIFICATION_MAX_BACKOFF_MS)
                .is_none()
        );

        // An answer that is not "waiting" ends the streak.
        cache.record(session, &head, url, at, "settled".into(), false);
        assert!(
            cache
                .fresh(session, &head, url, at + VERIFICATION_TTL_MS)
                .is_none()
        );

        // A different HEAD is different evidence, never a hit.
        cache.record(session, &head, url, at, "settled".into(), false);
        assert!(cache.fresh(session, &"b".repeat(40), url, at).is_none());
        // So is a different PR: two PRs can share a head commit, and serving one
        // PR's checks as another's would publish a URL nothing was read for.
        assert!(
            cache
                .fresh(session, &head, "https://github.com/owner/repo/pull/2", at)
                .is_none()
        );
        assert_eq!(cache.fresh(session, &head, url, at), Some("settled"));

        // Leaving verification drops what was remembered.
        cache.forget(session);
        assert!(cache.fresh(session, &head, url, at).is_none());
        // Another session never reads this one's answer.
        cache.record(session, &head, url, at, "settled".into(), false);
        assert!(cache.fresh(SessionId::new(), &head, url, at).is_none());

        // Sessions removed while their run was live are never enumerated again,
        // so the map sheds the least recently read instead of growing for the
        // daemon's lifetime.
        for index in 0..MAX_CACHED_SESSIONS {
            cache.record(
                SessionId::new(),
                &head,
                url,
                at + 1 + index as u64,
                "settled".into(),
                false,
            );
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_SESSIONS);
        assert!(
            cache.fresh(session, &head, url, at).is_none(),
            "the stalest entry was shed"
        );

        // Only "not yet" refusals are worth backing off.
        assert!(is_waiting("Waiting for successful PR checks"));
        assert!(!is_waiting("PR has unresolved review requests"));
    }
}
