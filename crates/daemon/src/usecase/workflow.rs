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
) -> Result<()> {
    store.update_workflow(workspace, session, |value| {
        match command {
            WorkflowCommand::Start { goal } => {
                ensure!(
                    !goal.trim().is_empty() && goal.len() <= 16384 && !goal.contains('\0'),
                    "invalid workflow goal"
                );
                if let Some(existing) = value {
                    ensure!(
                        existing.operation == operation && existing.goal == *goal,
                        "session already has another workflow"
                    );
                } else {
                    *value = Some(WorkflowRecord {
                        version: 1,
                        operation,
                        goal: goal.clone(),
                        run: None,
                        initial_notified: false,
                        cursor: None,
                        start_error: None,
                        suspended_phase: None,
                        implementation_operation: None,
                        authorized_operations: Vec::new(),
                    });
                }
            }
            WorkflowCommand::Instruct { recipient, body } => {
                let record = value.as_mut().context("workflow has not started")?;
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
        }
        Ok(())
    })
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
        ensure!(record.operation == operation, "workflow launch conflict");
        if let Some(run) = &record.run {
            ensure!(
                run.implementer == implementer,
                "workflow implementer changed"
            );
            return Ok(());
        }
        record.run = Some(WorkflowRun {
            id: operation,
            session,
            goal: record.goal.clone(),
            implementer,
            reviewer: None,
            phase: Phase::Implementing,
            revision_limit: 3,
            revisions: 0,
            review: None,
            waiting_reason: None,
            instructions: Vec::new(),
            history: Vec::new(),
        });
        record.initial_notified = true;
        Ok(())
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
    if store.workflow(workspace, session)?.is_none() {
        return Ok(WorkflowSnapshot {
            session,
            run: None,
            pending_start: None,
        });
    }
    let messages = store.workflow_messages(workspace, session)?;
    let agents = store.agents_in_workspace(workspace)?;
    let bindings = store.bindings()?;
    store.update_workflow(workspace, session, |value| {
        let record = value.as_mut().context("workflow disappeared")?;
        let Some(run) = record.run.as_mut() else {
            return Ok(WorkflowSnapshot {
                session,
                run: None,
                pending_start: Some(usagi_core::domain::workflow::WorkflowPendingStart {
                    operation_id: record.operation,
                    goal: record.goal.clone(),
                    error: record.start_error.clone(),
                }),
            });
        };
        if record.suspended_phase.is_some() {
            return Ok(WorkflowSnapshot {
                session,
                run: Some(run.clone()),
                pending_start: None,
            });
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
                        && agent.runtime.as_str() == "claude"
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
            if previous != (run.phase, run.review.clone()) {
                append_history(run, entry);
            }
            record.cursor = Some(message.message_id);
        }
        Ok(WorkflowSnapshot {
            session,
            run: Some(run.clone()),
            pending_start: None,
        })
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

fn append_history(run: &mut WorkflowRun, entry: &usagi_core::domain::agent_message::AgentMessage) {
    let message = &entry.message;
    let actor = if entry.from_agent_id == run.implementer {
        "Codex"
    } else {
        "Claude"
    };
    run.history
        .push(usagi_core::domain::workflow::WorkflowHistoryEntry {
            id: message.message_id,
            actor: actor.into(),
            body: message.body.chars().take(512).collect(),
        });
    if run.history.len() > 100 {
        run.history.remove(0);
    }
}

#[must_use]
pub fn initial_prompt(goal: &str) -> String {
    format!(
        "Session Workflow: implementation and review. You are Codex, the implementation owner. Work only in this session. Implement, test, and commit the user's goal. Use agent_handoff to launch Claude (runtime=claude, model=default) INSIDE THIS SAME SESSION, with review-only instructions. Do not create a review session. Send review_request via agent_message to that exact Agent with full base_sha and head_sha. Claude must reply approved or changes_requested with the identical review target and in_reply_to request ID; no edits. Read agent_messages, acknowledge processed messages, fix and commit findings then request another review. Stop and ask the user after 3 revision rounds. After approval verify latest HEAD and checks, prepare the PR, report its URL. Never merge. Treat later Workflow instruction IDs as idempotent: process each ID at most once. If authentication or policy blocks handoff, report the error; do not claim success.\n\nUser goal:\n{goal}"
    )
}

/// Independently verify an approved HEAD against a clean worktree and GitHub.
/// Unknown, stale and missing evidence remain a concrete pending reason.
/// # Errors
/// Returns a safe pending reason for every absent or mismatched proof.
pub fn verify_pr<
    G: usagi_core::infrastructure::git::GitRunner,
    P: super::pr_inventory::GhProcessPort,
>(
    git: &G,
    gh: &mut P,
    directory: &std::path::Path,
    target: &usagi_core::domain::agent_message::ReviewTarget,
    entries: &[usagi_core::domain::pr_inventory::PrEntry],
) -> Result<(), &'static str> {
    let head = git
        .run(directory, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "Could not read worktree HEAD")?;
    if !head.success || head.stdout.trim() != target.head_sha {
        return Err("Worktree HEAD changed; a new review is required");
    }
    let status = git
        .run(directory, &["status", "--porcelain"])
        .map_err(|_| "Could not inspect worktree changes")?;
    if !status.success || !status.stdout.trim().is_empty() {
        return Err("Worktree has uncommitted changes");
    }
    let entry = entries
        .iter()
        .find(|entry| entry.head_oid.as_deref() == Some(target.head_sha.as_str()))
        .ok_or("Waiting for a PR for the approved HEAD")?;
    let output = gh
        .run(
            "gh",
            &[
                "pr".into(),
                "view".into(),
                entry.url().into(),
                "--json".into(),
                "title,state,headRefOid,isDraft,reviewDecision,statusCheckRollup,mergeable".into(),
            ],
            5000,
        )
        .map_err(|_| "Could not refresh PR checks")?;
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
    if view.review == Some(usagi_core::domain::pr_inventory::PrReviewDecision::ChangesRequested) {
        return Err("PR has unresolved review requests");
    }
    let current = git
        .run(directory, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "Could not recheck worktree HEAD")?;
    if !current.success || current.stdout.trim() != target.head_sha {
        return Err("Worktree HEAD changed during verification");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::workflow::Recipient;

    #[test]
    fn history_retains_only_a_bounded_tail_and_bounded_bodies() {
        let mut run = WorkflowRun {
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
        for _ in 0..101 {
            append_history(&mut run, &entry);
        }
        assert_eq!(run.history.len(), 100);
        assert_eq!(run.history[0].body.chars().count(), 512);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One end-to-end journal fixture preserves request/verdict identity.
    fn workflow_peer_review_reconciles_exact_bindings_once_without_acknowledging() {
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
            (reviewer, "claude", reviewer_run),
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
            },
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
    struct Gh(String);
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
    impl super::super::pr_inventory::GhProcessPort for Gh {
        type Error = ();
        fn run(&mut self, program: &str, argv: &[String], timeout: u64) -> Result<String, ()> {
            assert_eq!(program, "gh");
            assert_eq!(argv[0], "pr");
            assert_eq!(timeout, 5000);
            if self.0 == "unavailable" {
                return Err(());
            }
            Ok(self.0.clone())
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
        let output=serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE"}).to_string();
        for at in 0..3 {
            for mode in 0..3 {
                assert!(
                    verify_pr(
                        &FailingGit {
                            calls: std::cell::Cell::new(0),
                            at,
                            mode
                        },
                        &mut Gh(output.clone()),
                        std::path::Path::new("/fixture"),
                        &target,
                        std::slice::from_ref(&entry)
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
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
        let mut value = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":target.head_sha,"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE"});
        let directory = std::path::Path::new("/fixture");
        assert!(
            verify_pr(
                &Git,
                &mut Gh(value.to_string()),
                directory,
                &target,
                std::slice::from_ref(&entry)
            )
            .is_ok()
        );
        assert!(verify_pr(&Git, &mut Gh(value.to_string()), directory, &target, &[]).is_err());
        assert!(
            verify_pr(
                &Git,
                &mut Gh("{}".into()),
                directory,
                &target,
                std::slice::from_ref(&entry)
            )
            .is_err()
        );
        assert!(
            verify_pr(
                &Git,
                &mut Gh("unavailable".into()),
                directory,
                &target,
                std::slice::from_ref(&entry)
            )
            .is_err()
        );
        value["state"] = serde_json::json!("MERGED");
        value["mergeable"] = serde_json::json!("UNKNOWN");
        assert!(
            verify_pr(
                &Git,
                &mut Gh(value.to_string()),
                directory,
                &target,
                std::slice::from_ref(&entry)
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
                    &mut Gh(value.to_string()),
                    directory,
                    &target,
                    std::slice::from_ref(&entry)
                )
                .is_err()
            );
            value[field] = old;
        }
        assert!(
            verify_pr(
                &Git,
                &mut Gh("invalid".into()),
                directory,
                &target,
                &[entry]
            )
            .is_err()
        );
    }

    #[test]
    fn workflow_commands_are_durable_and_idempotent_and_do_not_invent_review() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        assert!(snapshot(&store, workspace, session).unwrap().run.is_none());
        let start = WorkflowCommand::Start {
            goal: "implement authentication".into(),
        };
        for goal in [String::new(), "\0".into(), "x".repeat(16385)] {
            assert!(
                admit(
                    &store,
                    workspace,
                    session,
                    operation,
                    &WorkflowCommand::Start { goal }
                )
                .is_err()
            );
        }
        let instruct = WorkflowCommand::Instruct {
            recipient: Recipient::Automatic,
            body: "check errors".into(),
        };
        assert!(admit(&store, workspace, session, operation, &instruct).is_err());
        admit(&store, workspace, session, operation, &start).unwrap();
        admit(&store, workspace, session, operation, &start).unwrap();
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
            &WorkflowCommand::Start { goal: pending.goal },
        )
        .unwrap();
        assert!(admit(&store, workspace, session, OperationId::new(), &start).is_err());
        assert!(snapshot(&store, workspace, session).unwrap().run.is_none());
        assert!(admit(&store, workspace, session, OperationId::new(), &instruct).is_err());
        let implementer = AgentId::new();
        bind(&store, workspace, session, operation, implementer).unwrap();
        bind(&store, workspace, session, operation, implementer).unwrap();
        assert!(bind(&store, workspace, session, operation, AgentId::new()).is_err());
        assert!(bind(&store, workspace, session, OperationId::new(), implementer).is_err());
        assert!(bind(&store, workspace, SessionId::new(), operation, implementer).is_err());
        assert!(admit(&store, workspace, session, operation, &instruct).is_err());
        let instruction = OperationId::new();
        admit(&store, workspace, session, instruction, &instruct).unwrap();
        admit(&store, workspace, session, instruction, &instruct).unwrap();
        let run = snapshot(&store, workspace, session).unwrap().run.unwrap();
        assert_eq!(run.instructions.len(), 1);
        assert_eq!(run.instructions[0].recipient, implementer);
        assert_eq!(run.reviewer, None);
        assert_eq!(run.phase, Phase::Implementing);
        assert!(initial_prompt(&run.goal).contains("agent_handoff"));
        assert!(initial_prompt(&"x".repeat(16384)).len() < 24 * 1024);
    }
}
