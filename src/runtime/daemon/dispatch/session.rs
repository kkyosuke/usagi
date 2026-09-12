//! Session-family request handling: the action table, delegation, and the
//! helpers both depend on.
//!
//! This lives beside the dispatch table rather than inside it because the table
//! itself has a line budget (`tests/architecture.rs`) that the rest of the
//! daemon has to fit inside, and because the session family is the one that
//! grows: every new session tool lands here.
//!
//! Visibility says how far each item reaches: `pub(in crate::runtime::daemon)`
//! for the four the daemon module's own recovery and tests call, `pub(super)`
//! for what only the dispatch table calls, and private for the rest.

use super::super::workflow;
use super::{
    AmbiguousIssueNumber, BTreeMap, BTreeSet, ConnectionWorkspace, DispatchStore, ErrorLog,
    SessionDispatchContext, SessionId, SessionRuntimeError, SharedAgentRuntime,
    SharedSessionRuntime, SystemGit, TeardownSignal, WorkspaceId, aggregate_agent_status,
    best_effort_merged_pr_head, bind_delegated_supervisor_dispatch, clean_orphan_session_resources,
    dispatch_agent_after_preflight, perform_compensating_remove, perform_create,
    perform_delegated_create, perform_remove_with_merged_head,
    reconcile_pending_supervisor_promotions, record_session_lineage,
    require_stable_supervisor_fence, require_supervisor_reservation_presence, scratchpad,
    session_id_by_name, supervisor_error,
};

pub(super) fn session_organization(
    id: SessionId,
    names: &BTreeMap<SessionId, String>,
    parents: &BTreeMap<SessionId, Option<SessionId>>,
) -> (Option<String>, usize, Vec<String>) {
    let parent = parents.get(&id).copied().flatten();
    let parent_name = parent.and_then(|parent| names.get(&parent).cloned());
    let mut lineage = Vec::new();
    let mut cursor = Some(id);
    let mut seen = BTreeSet::new();
    while let Some(member) = cursor
        && seen.insert(member)
    {
        lineage.push(member);
        cursor = parents
            .get(&member)
            .copied()
            .flatten()
            .filter(|parent| names.contains_key(parent));
    }
    lineage.reverse();
    let mut path = vec!["Director".to_owned()];
    path.extend(
        lineage
            .iter()
            .filter_map(|member| names.get(member).cloned()),
    );
    (parent_name, lineage.len(), path)
}

#[allow(clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_immediately_dispatches_an_isolated_triage_worker
pub(super) fn dispatch_session_action(
    context: &SessionDispatchContext<'_>,
    action: usagi_core::infrastructure::client::SessionAction,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError> {
    use usagi_core::infrastructure::client::SessionAction;
    use usagi_core::infrastructure::store::issue::IssueStore;
    use usagi_core::usecase::issue;
    use usagi_daemon::usecase::agent_ipc::PromptMode;

    let bound = context.bound;
    let teardown = context.teardown;
    let agent = context.agent;
    let pr_inventory = context.pr_inventory;

    let authenticated_caller = payload
        .get("_caller_credential")
        .and_then(serde_json::Value::as_str)
        .map(|credential| {
            agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .mcp_dispatch_context(credential)
                .ok_or(SessionRuntimeError::ScopeUnavailable)
        })
        .transpose()?;

    let reply = |body: serde_json::Value| {
        let revision = bound
            .sessions()
            .lock()
            .ok()
            .and_then(|runtime| runtime.snapshot().ok())
            .and_then(|snapshot| snapshot.get("revision").and_then(serde_json::Value::as_u64))
            .unwrap_or_default();
        Ok(usagi_daemon::usecase::session_runtime::SessionReply {
            operation_id: operation_id.to_owned(),
            revision,
            body,
        })
    };
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(SessionRuntimeError::InvalidRequest)
    };
    let caller_scope = || {
        let credential = string("_caller_credential")?;
        let session_id = agent
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .caller_session(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .session_scope_by_id(session_id)
    };
    let bound_workspace = || {
        bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .workspace_id()
            .map_err(|_| SessionRuntimeError::Storage)
    };
    if let Some(authenticated) = authenticated_caller.as_ref()
        && authenticated.workspace_id != bound_workspace()?
    {
        return Err(SessionRuntimeError::ScopeUnavailable);
    }
    let caller = authenticated_caller.as_ref().map(|caller| &caller.caller);
    let target_session = |name: &str| {
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        if let Some(caller) = caller {
            sessions.created_session_id(name, caller)
        } else {
            sessions.session_id(name)
        }
    };
    let authorize_create_or_reuse = |name: &str| {
        if let Some(caller) = caller {
            bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .authorize_create_or_reuse(name, caller)?;
        }
        Ok::<(), SessionRuntimeError>(())
    };

    match action {
        SessionAction::List | SessionAction::Status | SessionAction::Overview => {
            let visible = caller
                .map(|caller| {
                    bound
                        .sessions()
                        .lock()
                        .map_err(|_| SessionRuntimeError::Storage)?
                        .created_session_ids(caller)
                })
                .transpose()?;
            let mut status = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(action, operation_id, payload)?;
            let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
            let store = runtime.dispatch_store();
            let agents = store.agents().map_err(|_| SessionRuntimeError::Storage)?;
            let runtime_observation = |id, names: &_, parents: &_| {
                use usagi_core::infrastructure::session_snapshot::SessionRuntimeObservation;

                let (parent_session_name, organization_depth, organization_path) =
                    session_organization(id, names, parents);
                let (agent_resumable, agent_resume_reason) = runtime.session_resume_status(id);
                SessionRuntimeObservation {
                    agent_phase: runtime.session_phase(id),
                    agent_resumable,
                    agent_resume_reason,
                    agent_status: aggregate_agent_status(
                        agents
                            .iter()
                            .filter(|agent| agent.session_id == Some(id))
                            .map(|agent| agent.status),
                    ),
                    parent_session_name,
                    organization_depth,
                    organization_path,
                }
            };
            status.body = match action {
                SessionAction::List | SessionAction::Overview => {
                    use usagi_core::infrastructure::session_snapshot::SessionListSnapshot;

                    let snapshot = serde_json::from_value::<SessionListSnapshot>(status.body)
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    let names = snapshot
                        .sessions
                        .iter()
                        .map(|item| (item.session.session_id, item.session.name.clone()))
                        .collect::<BTreeMap<_, _>>();
                    let parents = snapshot
                        .sessions
                        .iter()
                        .map(|item| (item.session.session_id, item.session.parent_session_id))
                        .collect::<BTreeMap<_, _>>();
                    let sessions = snapshot
                        .sessions
                        .into_iter()
                        .filter(|item| {
                            visible
                                .as_ref()
                                .is_none_or(|visible| visible.contains(&item.session.session_id))
                        })
                        .map(|mut item| {
                            item.runtime = Some(runtime_observation(
                                item.session.session_id,
                                &names,
                                &parents,
                            ))
                            .into();
                            item
                        })
                        .collect();
                    serde_json::to_value(SessionListSnapshot {
                        workspace_id: snapshot.workspace_id,
                        root_worktree_id: snapshot.root_worktree_id,
                        revision: snapshot.revision,
                        sessions,
                    })
                    .map_err(|_| SessionRuntimeError::Storage)?
                }
                SessionAction::Status => {
                    use usagi_core::infrastructure::session_snapshot::SessionStatusSnapshot;

                    let snapshot = serde_json::from_value::<SessionStatusSnapshot>(status.body)
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    let names = snapshot
                        .sessions
                        .iter()
                        .map(|item| (item.session_id, item.name.clone()))
                        .collect::<BTreeMap<_, _>>();
                    let parents = snapshot
                        .sessions
                        .iter()
                        .map(|item| (item.session_id, item.parent_session_id))
                        .collect::<BTreeMap<_, _>>();
                    let sessions = snapshot
                        .sessions
                        .into_iter()
                        .filter(|item| {
                            visible
                                .as_ref()
                                .is_none_or(|visible| visible.contains(&item.session_id))
                        })
                        .map(|mut item| {
                            item.runtime = runtime_observation(item.session_id, &names, &parents);
                            item
                        })
                        .collect();
                    serde_json::to_value(SessionStatusSnapshot {
                        workspace_id: snapshot.workspace_id,
                        revision: snapshot.revision,
                        sessions,
                    })
                    .map_err(|_| SessionRuntimeError::Storage)?
                }
                _ => unreachable!(),
            };
            Ok(status)
        }
        SessionAction::Prompt => {
            let name = string("name")?;
            let prompt = string("prompt")?;
            let target = if name == ":root" {
                if caller.is_some() {
                    return Err(SessionRuntimeError::PermissionDenied);
                }
                None
            } else {
                Some(target_session(name)?)
            };
            let mode = match payload
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("live")
            {
                "queue" => PromptMode::Queue,
                "live" => PromptMode::Live,
                _ => return Err(SessionRuntimeError::InvalidRequest),
            };
            let workspace = bound_workspace()?;
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .prompt(workspace, target, prompt, mode)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(
                serde_json::json!({"name": name, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        SessionAction::Complete => {
            let message = string("message")?;
            let credential = string("_caller_credential")?;
            let scope = caller_scope()?;
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .report_from_mcp(
                    credential,
                    None,
                    usagi_core::domain::agent::InboxKind::Completed,
                    message.to_owned(),
                    None,
                )
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(serde_json::json!({
                "session_id": scope.session_id,
                "reported_to": delivery.delivered_to,
                "delivered_to": "inbox"
            }))
        }
        // The workflow control plane is the human's, so these tools carry the
        // same ownership rule as the rest: a caller reaches a session it
        // created, never the one it is running inside. That is what keeps a
        // workflow's own Agents from driving their own workflow.
        SessionAction::WorkflowStatus
        | SessionAction::WorkflowStart
        | SessionAction::WorkflowInstruct
        | SessionAction::WorkflowFinish => {
            let name = string("name")?;
            let session = target_session(name)?;
            let workspace = bound_workspace()?;
            if caller.is_some_and(|caller| caller.session_id == Some(session)) {
                return Err(SessionRuntimeError::PermissionDenied);
            }
            // A start may name a backlog issue instead of spelling the goal.
            // The issue body becomes the goal, and the run keeps the reference
            // the PR will have to name.
            let issue = workflow::requested_issue(payload)?;
            let command = match action {
                SessionAction::WorkflowStatus => None,
                SessionAction::WorkflowStart => {
                    let goal = match issue {
                        Some(number) => {
                            workflow::issue_goal(bound, number).map_err(workflow::refusal)?
                        }
                        None => string("goal")?.to_owned(),
                    };
                    Some(usagi_core::domain::workflow::WorkflowCommand::Start {
                        goal,
                        agents: workflow::requested_agents(
                            payload,
                            workflow::remembered_agents(agent, workspace),
                        )
                        .ok_or(SessionRuntimeError::InvalidRequest)?,
                    })
                }
                SessionAction::WorkflowFinish => {
                    Some(usagi_core::domain::workflow::WorkflowCommand::Finish)
                }
                _ => Some(usagi_core::domain::workflow::WorkflowCommand::Instruct {
                    recipient: workflow::requested_recipient(payload)
                        .ok_or(SessionRuntimeError::InvalidRequest)?,
                    body: string("body")?.to_owned(),
                }),
            };
            let snapshot = match command {
                None => workflow::advance(
                    agent,
                    pr_inventory,
                    &bound.scope_resolver(),
                    workspace,
                    session,
                    workflow::Attention::Requested,
                ),
                Some(command) => workflow::control_workflow(
                    agent,
                    bound,
                    workspace,
                    session,
                    usagi_core::domain::id::OperationId::parse(operation_id)
                        .map_err(|_| SessionRuntimeError::InvalidRequest)?,
                    command,
                    issue,
                ),
            }
            .map_err(workflow::refusal)?;
            reply(serde_json::to_value(snapshot).map_err(|_| SessionRuntimeError::Storage)?)
        }
        SessionAction::Pr => {
            let (name, id) = if payload.get("name").is_some() {
                let name = string("name")?;
                (name.to_owned(), target_session(name)?)
            } else {
                let id = caller_scope()?.session_id;
                let lifecycle = bound
                    .sessions()
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .snapshot()
                    .map_err(|_| SessionRuntimeError::Storage)?;
                let name = lifecycle
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|sessions| {
                        sessions.iter().find(|session| {
                            session.get("session_id") == Some(&serde_json::json!(id))
                                && session.get("lifecycle").and_then(serde_json::Value::as_str)
                                    == Some("available")
                        })
                    })
                    .and_then(|session| session.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or(SessionRuntimeError::ScopeUnavailable)?
                    .to_owned();
                (name, id)
            };
            let snapshot = pr_inventory
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .snapshot(id)
                .map_err(|_| SessionRuntimeError::Storage)?;
            let merged = snapshot
                .entries
                .iter()
                .any(|entry| entry.state == usagi_core::domain::pr_inventory::PrState::Merged);
            reply(
                serde_json::json!({"name": name, "session_id": id, "revision": snapshot.revision, "merged": merged, "pr": snapshot.entries}),
            )
        }
        SessionAction::NoteGet
        | SessionAction::NoteUpdate
        | SessionAction::TodoList
        | SessionAction::TodoAdd
        | SessionAction::TodoUpdate
        | SessionAction::TodoRemove
        | SessionAction::DecisionList
        | SessionAction::DecisionLog => {
            let scope = caller_scope()?;
            let body = scratchpad::read_or_write(action, payload, &scope.path)?;
            reply(serde_json::json!({"session_id": scope.session_id, "scratchpad": body}))
        }
        SessionAction::DelegateBrief => reply(delegate_brief(context, operation_id, payload)?),
        SessionAction::DelegateIssue => {
            let workspace = bound_workspace()?;
            let number = payload
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            let name = payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| format!("issue-{number}"), str::to_owned);
            let _delegation_permit =
                authorize_delegation(bound, agent, caller, payload.get("role"), operation_id)?;
            authorize_create_or_reuse(&name)?;
            let root = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .repository_root()
                .to_path_buf();
            let issue = issue::get(&IssueStore::new(root), number)
                .map_err(|error| {
                    error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<AmbiguousIssueNumber>())
                        .cloned()
                        .map_or(
                            SessionRuntimeError::Storage,
                            SessionRuntimeError::AmbiguousIssue,
                        )
                })?
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            let prompt = issue::to_prompt(&issue);
            let requested_role = payload.get("role").cloned();
            let mut created = perform_create(
                bound.sessions(),
                &SystemGit,
                operation_id,
                &serde_json::json!({
                    "name": name,
                    "role": requested_role,
                    "parent_session_id": caller.and_then(|caller| caller.session_id),
                    "creator_agent_id": caller.map(|caller| caller.agent_id),
                }),
            )?;
            let id = record_session_lineage(agent, workspace, &created.body, &name)?;
            if caller.is_some()
                && let Some(sessions) = created
                    .body
                    .get_mut("sessions")
                    .and_then(serde_json::Value::as_array_mut)
            {
                sessions
                    .retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
            }
            let delivery = if let Some(caller) = caller.cloned() {
                let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
                runtime
                    .dispatch_store()
                    .queue_delegated_prompt(
                        workspace,
                        Some(id),
                        prompt,
                        chrono::Utc::now(),
                        caller,
                        usagi_core::domain::id::OperationId::parse(operation_id)
                            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
                    )
                    .map_err(|_| SessionRuntimeError::Storage)?;
                usagi_daemon::usecase::agent_ipc::PromptDelivery {
                    delivered_to: "queue",
                    queued: true,
                }
            } else {
                agent
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .prompt(workspace, Some(id), &prompt, PromptMode::Queue)
                    .map_err(|error| SessionRuntimeError::Delivery(error.message))?
            };
            reply(
                serde_json::json!({"name": name, "session_id": id, "created": created.body, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        // Create runs its heavy Git worktree build with the shared session lock
        // released, so a long `git worktree add` never freezes concurrent
        // readers (session list, terminal poll, user-decision list) on the
        // daemon. The fast durable transitions still run under the lock.
        SessionAction::Create => {
            authorize_create_or_reuse(string("name")?)?;
            let mut create_payload = payload.clone();
            create_payload["parent_session_id"] =
                serde_json::json!(caller.and_then(|caller| caller.session_id));
            create_payload["creator_agent_id"] =
                serde_json::json!(caller.map(|caller| caller.agent_id));
            let mut created =
                perform_create(bound.sessions(), &SystemGit, operation_id, &create_payload)?;
            let id =
                record_session_lineage(agent, bound_workspace()?, &created.body, string("name")?)?;
            if caller.is_some()
                && let Some(sessions) = created
                    .body
                    .get_mut("sessions")
                    .and_then(serde_json::Value::as_array_mut)
            {
                sessions
                    .retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
            }
            Ok(created)
        }
        // Remove goes further: it answers as soon as the session is durably
        // `Deleting` and hands the unbounded worktree teardown to the daemon's
        // teardown worker. Keeping the teardown on this connection would hold
        // the reply past every client attempt deadline for a session with a
        // multi-gigabyte `target/`.
        SessionAction::Remove => {
            let name = string("name")?;
            let (id, branch_head) = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .removal_identity(name)?;
            if let Some(caller) = caller {
                let target = bound
                    .sessions()
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .created_session_record_id(name, caller)?;
                if id != target {
                    return Err(SessionRuntimeError::PermissionDenied);
                }
            }
            let merged_head_oid = best_effort_merged_pr_head(pr_inventory, id, branch_head);
            let mut remove_payload = payload.clone();
            remove_payload["parent_session_id"] =
                serde_json::json!(caller.and_then(|caller| caller.session_id));
            remove_payload["creator_agent_id"] =
                serde_json::json!(caller.map(|caller| caller.agent_id));
            perform_remove_with_merged_head(
                bound.sessions(),
                teardown,
                operation_id,
                &remove_payload,
                merged_head_oid,
            )
        }
        SessionAction::Sleep => {
            let name = string("name")?;
            let id = target_session(name)?;
            let slept = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .sleep_session(id)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            let mut snapshot = dispatch_session_action(
                context,
                SessionAction::List,
                operation_id,
                &serde_json::json!({}),
            )?;
            snapshot.body["slept"] = serde_json::json!(slept);
            snapshot.body["slept_session"] = serde_json::json!(name);
            snapshot.body["session_retained"] = serde_json::json!(true);
            Ok(snapshot)
        }
        SessionAction::Clean => {
            let flag = |name| match payload.get(name) {
                None => Ok(false),
                Some(serde_json::Value::Bool(value)) => Ok(*value),
                Some(_) => Err(SessionRuntimeError::InvalidRequest),
            };
            let apply = flag("apply")?;
            let force = flag("force")?;
            if force && !apply {
                return Err(SessionRuntimeError::InvalidRequest);
            }
            reply(clean_orphan_session_resources(
                bound,
                Some(agent),
                apply,
                force,
            )?)
        }
        SessionAction::Setup => {
            target_session(string("name")?)?;
            bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(action, operation_id, payload)
        }
    }
}

/// Reads a delegation's `agent` selector, which names a runtime and model and
/// nothing else.
///
/// An `agent.id` is refused rather than resolved. No existing Agent can belong to
/// a session the same request is about to create, so the dispatch ownership check
/// would reject every such selector — after the worktree already existed. The
/// tool schema no longer advertises that branch and this is the daemon-side half
/// of the same rule.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_publishes_and_accepts_only_a_new_agent_selector
fn new_agent_selector(
    selector: Option<&serde_json::Value>,
) -> Result<
    (
        usagi_core::domain::agent::AgentProfileId,
        usagi_core::domain::agent::ModelSelector,
    ),
    SessionRuntimeError,
> {
    use usagi_core::domain::agent::{AgentProfileId, ModelSelector};

    let selector = selector
        .and_then(serde_json::Value::as_object)
        .filter(|selector| selector.len() == 2 && !selector.contains_key("id"))
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    let field = |key: &str| {
        selector
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    Ok((
        serde_json::from_value::<AgentProfileId>(field("runtime"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
        serde_json::from_value::<ModelSelector>(field("model"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
    ))
}

/// Applies configured company-role authority before any delegated side effect.
/// Catalogs without a `delegation` block keep their established permissive
/// behavior; once a block is present the daemon owns every decision.
pub(super) struct DelegationPermit {
    store: Option<usagi_core::infrastructure::store::dispatch::DispatchStore>,
    operation_id: Option<usagi_core::domain::id::OperationId>,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
impl DelegationPermit {
    const fn inert() -> Self {
        Self {
            store: None,
            operation_id: None,
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
impl Drop for DelegationPermit {
    fn drop(&mut self) {
        if let (Some(store), Some(operation_id)) = (&self.store, self.operation_id) {
            let _ = store.release_delegation(operation_id);
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
pub(super) fn authorize_delegation(
    bound: &ConnectionWorkspace,
    agent: &SharedAgentRuntime,
    caller: Option<&usagi_core::domain::agent::CallerRef>,
    requested_role: Option<&serde_json::Value>,
    operation_id: &str,
) -> Result<DelegationPermit, SessionRuntimeError> {
    use usagi_core::domain::role::{RoleId, RoleScope};
    use usagi_core::infrastructure::store::dispatch::DelegationReservationOutcome;

    let requested = requested_role
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value::<RoleId>)
        .transpose()
        .map_err(|_| SessionRuntimeError::InvalidRequest)?;
    let sessions = bound
        .sessions()
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?;
    let catalog = sessions.effective_role_catalog()?;
    let Some(caller) = caller else {
        if catalog
            .roles
            .values()
            .any(|definition| definition.delegation.is_some())
        {
            return Err(SessionRuntimeError::InvalidRole(
                "authenticated caller is required by the delegation policy".into(),
            ));
        }
        return Ok(DelegationPermit::inert());
    };
    let parent_role = match caller.session_id {
        Some(id) => sessions.session_role(id)?,
        None => catalog
            .resolve(None, RoleScope::Root)
            .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?,
    };
    let child_role = catalog
        .resolve(requested.as_ref(), RoleScope::Session)
        .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?;
    let Some(policy) = parent_role
        .as_ref()
        .and_then(|role| catalog.roles.get(role))
        .and_then(|definition| definition.delegation.as_ref())
    else {
        return Ok(DelegationPermit::inert());
    };
    if !policy.enabled {
        return Err(SessionRuntimeError::InvalidRole(
            "caller role is not allowed to delegate".into(),
        ));
    }
    let child_role = child_role.ok_or_else(|| {
        SessionRuntimeError::InvalidRole("delegation requires an authorized child role".into())
    })?;
    if !policy.child_roles.contains(&child_role) {
        return Err(SessionRuntimeError::InvalidRole(format!(
            "caller role may not delegate to role \"{child_role}\""
        )));
    }
    drop(sessions);

    let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
    let store = runtime.dispatch_store();
    let depth = store
        .delegation_depth(caller)
        .map_err(|_| SessionRuntimeError::Storage)?;
    if depth.saturating_add(1) > policy.max_depth {
        return Err(SessionRuntimeError::InvalidRole(format!(
            "delegation depth limit ({}) reached",
            policy.max_depth
        )));
    }
    let operation_id = usagi_core::domain::id::OperationId::parse(operation_id)
        .map_err(|_| SessionRuntimeError::InvalidRequest)?;
    match store
        .reserve_delegation(caller, operation_id, policy.max_concurrency)
        .map_err(|_| SessionRuntimeError::Storage)?
    {
        DelegationReservationOutcome::Reserved => Ok(DelegationPermit {
            store: Some(store.clone()),
            operation_id: Some(operation_id),
        }),
        DelegationReservationOutcome::AlreadyAdmitted => Ok(DelegationPermit::inert()),
        DelegationReservationOutcome::LimitReached => {
            Err(SessionRuntimeError::InvalidRole(format!(
                "delegation concurrency limit ({}) reached",
                policy.max_concurrency
            )))
        }
        DelegationReservationOutcome::InProgress => Err(SessionRuntimeError::InvalidRole(
            "delegation operation is already in progress".into(),
        )),
    }
}

pub(in crate::runtime::daemon) fn required_payload_string<'a>(
    payload: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, SessionRuntimeError> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SessionRuntimeError::InvalidRequest)
}

/// Creates a triage session for a brief and dispatches a fresh worker into it,
/// as one operation that either takes effect completely or leaves nothing.
///
/// The order is what makes that true. Every rejection the daemon can decide
/// without a side effect — the selector, the caller, the runtime/model
/// allowlist, the runtime executable, an operation that already owns an
/// admission — is decided before the worktree exists. Only after that does the
/// create run, and a dispatch that then fails definitively is rolled back by the
/// same durable teardown `session_remove` uses, which the daemon resumes across
/// a restart. A dispatch whose spawn outcome is *unknown* is deliberately not
/// rolled back: the worktree may already hold a running worker, so the caller
/// gets the session and run identity to reconcile instead.
#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_immediately_dispatches_an_isolated_triage_worker
#[allow(clippy::too_many_lines)] // Atomic create, reservation, spawn, compensation, and recovery stay visible as one transaction.
fn delegate_brief(
    context: &SessionDispatchContext<'_>,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, SessionRuntimeError> {
    use usagi_core::infrastructure::client::{DispatchAgentIntent, DispatchIntent};

    let bound = context.bound;
    let teardown = context.teardown;
    let agent = context.agent;
    let supervisor = context.supervisor;

    let brief = required_payload_string(payload, "brief")?;
    let suffix = operation_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>();
    let name = payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| format!("triage-{suffix}"), str::to_owned);
    let prompt = format!(
        "このセッションの worktree 内で次の依頼をトリアージし、必要なら issue 化して実装へつなげてください。リポジトリの規約に従ってください。\n\n{brief}"
    );
    let (runtime, model) = new_agent_selector(payload.get("agent"))?;

    let credential = required_payload_string(payload, "_caller_credential")?;
    let (workspace, parent_dispatch_run, caller, repository_root) = {
        let agent_runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
        let authenticated = agent_runtime
            .mcp_dispatch_context(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let workspace = sessions
            .snapshot()
            .map_err(|_| SessionRuntimeError::Storage)?
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or(SessionRuntimeError::Storage)?;
        if authenticated.workspace_id != workspace {
            return Err(SessionRuntimeError::ScopeUnavailable);
        }
        sessions.authorize_create_or_reuse(&name, &authenticated.caller)?;
        (
            workspace,
            authenticated.run_id,
            authenticated.caller,
            sessions.repository_root().to_path_buf(),
        )
    };
    let supervision_at_preflight = supervisor
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .supervision_fence(parent_dispatch_run)
        .map_err(|_| SessionRuntimeError::Storage)?;
    if supervision_at_preflight.is_some() {
        agent
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .require_same_dispatch_runtime(
                workspace,
                &caller,
                &DispatchAgentIntent::New {
                    runtime: runtime.clone(),
                    model: model.clone(),
                },
            )
            .map_err(|error| SessionRuntimeError::AgentFailure {
                code: error.code,
                message: error.message,
            })?;
    }
    let _delegation_permit = authorize_delegation(
        bound,
        agent,
        Some(&caller),
        payload.get("role"),
        operation_id,
    )?;
    // Machine-local runtime/model policy belongs to the workspace root and is
    // not copied into managed worktrees. Decide every read-only refusal here;
    // `dispatch` still re-reads the same trusted root and stays the authority.
    agent
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .preflight_dispatch(operation_id, &prompt, &runtime, &model, &repository_root)
        .map_err(|error| SessionRuntimeError::AgentFailure {
            code: error.code,
            message: error.message,
        })?;

    let mut created = perform_delegated_create(
        bound.sessions(),
        &SystemGit,
        operation_id,
        &serde_json::json!({
            "name": name,
            "role": payload.get("role").cloned(),
            "parent_session_id": caller.session_id,
            "creator_agent_id": caller.agent_id,
        }),
    )
    .map_err(|error| {
        compensate_failed_delegated_initialize(
            bound.sessions(),
            teardown,
            &caller,
            &name,
            operation_id,
            error,
        )
    })?;
    let id = session_id_by_name(&created.body, &name).ok_or(SessionRuntimeError::Storage)?;
    if record_session_lineage(agent, workspace, &created.body, &name).is_err() {
        return Err(compensate_delegation(
            bound.sessions(),
            teardown,
            id,
            &name,
            operation_id,
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                "session parentage is unavailable",
            ),
        ));
    }
    if let Some(sessions) = created
        .body
        .get_mut("sessions")
        .and_then(serde_json::Value::as_array_mut)
    {
        sessions.retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
    }
    let selected = DispatchAgentIntent::New {
        runtime: runtime.clone(),
        model: model.clone(),
    };
    let reserved_worker = if supervision_at_preflight.is_some() {
        let planned = agent
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "agent owner is unavailable",
                )
            })
            .and_then(|runtime| runtime.plan_dispatch_worker(workspace, id, &selected));
        match planned {
            Ok(worker) => Some(worker),
            Err(error) => {
                return Err(compensate_delegation(
                    bound.sessions(),
                    teardown,
                    id,
                    &name,
                    operation_id,
                    error,
                ));
            }
        }
    } else {
        None
    };
    let scope = bound.scope_resolver();
    let reservation = (|| {
        let runtime = supervisor.lock().map_err(|_| {
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                "supervisor runtime is unavailable",
            )
        })?;
        let supervision_before_reservation = runtime
            .supervision_fence(parent_dispatch_run)
            .map_err(supervisor_error)?;
        require_stable_supervisor_fence(
            supervision_at_preflight.as_ref(),
            supervision_before_reservation.as_ref(),
        )?;
        let reservation = if let Some(reserved_worker) = reserved_worker.as_ref() {
            runtime
                .reserve_delegated_dispatch_for_session(
                    parent_dispatch_run,
                    operation_id,
                    prompt.clone(),
                    id,
                    reserved_worker,
                    &name,
                    chrono::Utc::now(),
                )
                .map_err(supervisor_error)?
        } else {
            None
        };
        let supervision_after_reservation = runtime
            .supervision_fence(parent_dispatch_run)
            .map_err(supervisor_error)?;
        require_stable_supervisor_fence(
            supervision_at_preflight.as_ref(),
            supervision_after_reservation.as_ref(),
        )?;
        require_supervisor_reservation_presence(
            supervision_at_preflight.as_ref(),
            reservation.is_some(),
        )?;
        Ok(reservation)
    })()
    .map_err(|error| {
        compensate_delegation(bound.sessions(), teardown, id, &name, operation_id, error)
    })?;
    let supervised = reservation.is_some();
    let prompt = reservation.map_or(prompt, |reservation| reservation.prompt);
    let dispatch_intent = DispatchIntent {
        workspace,
        session_name: name.clone(),
        caller,
        agent: selected,
        prompt,
    };
    let admission = dispatch_agent_after_preflight(
        agent,
        operation_id,
        &dispatch_intent,
        id,
        &scope,
        reserved_worker.as_ref(),
    );
    let admission = match admission {
        Ok(admission) => admission,
        Err(error) => {
            if supervised
                && error.code != usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
                && let Ok(runtime) = supervisor.lock()
                && let Err(failure) =
                    runtime.fail_reserved_delegated_dispatch(operation_id, chrono::Utc::now())
            {
                ErrorLog::record(&format!(
                    "delegated Supervisor failure reconciliation deferred: {failure}"
                ));
            }
            return Err(compensate_delegation(
                bound.sessions(),
                teardown,
                id,
                &name,
                operation_id,
                error,
            ));
        }
    };
    if supervised
        && let Err(error) = bind_delegated_supervisor_dispatch(
            supervisor,
            &admission.operation_id,
            &admission.runtime,
        )
    {
        // The child Agent is already durable; exact-operation reconciliation
        // finishes the promotion without asking the caller to retry the spawn.
        ErrorLog::record(&format!("delegated Supervisor promotion deferred: {error}"));
        if let Err(reconcile) = reconcile_pending_supervisor_promotions(supervisor, agent) {
            ErrorLog::record(&format!(
                "delegated Supervisor promotion reconciliation deferred: {reconcile}"
            ));
        }
    }
    Ok(serde_json::json!({
        "name": name,
        "session_id": id,
        "created": created.body,
        "run_id": admission.operation_id,
        "terminal": admission.terminal,
        "completed": admission.completed,
    }))
}

/// Compensates a delegated create only when its exact durable operation failed.
///
/// Setup is part of create, so a failed configured command happens before
/// dispatch but after the worktree exists. Matching the exact failed journal
/// entry keeps that deterministic failure inside delegation's existing atomic
/// rollback contract without risking an older same-name session on a
/// pre-effect error.
pub(in crate::runtime::daemon) fn compensate_failed_delegated_initialize(
    sessions: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    caller: &usagi_core::domain::agent::CallerRef,
    name: &str,
    operation_id: &str,
    error: SessionRuntimeError,
) -> SessionRuntimeError {
    let failed_session_id = sessions.lock().ok().and_then(|runtime| {
        runtime
            .failed_delegated_initialize_id(operation_id, name, caller)
            .ok()
            .flatten()
    });
    let Some(session_id) = failed_session_id else {
        return error;
    };
    compensate_delegation(
        sessions,
        teardown,
        session_id,
        name,
        operation_id,
        usagi_core::infrastructure::ipc::ProtocolError::new(
            usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
            error.safe_message(),
        ),
    )
}

/// Rolls a delegated create back, or reports why it must not be rolled back.
///
/// The teardown is admitted under a fresh operation identity because the
/// delegation's own identity already names the create it is compensating. Once
/// admitted it is durable: the daemon's teardown worker finishes it, and a
/// daemon that dies first resumes it from the `Deleting` record on the next
/// start.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_failed_delegation_reports_its_reconcile_state_on_the_wire
pub(in crate::runtime::daemon) fn compensate_delegation(
    sessions: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    session_id: usagi_core::domain::id::SessionId,
    name: &str,
    run_operation_id: &str,
    error: usagi_core::infrastructure::ipc::ProtocolError,
) -> SessionRuntimeError {
    use usagi_daemon::usecase::session_runtime::{DelegationFailure, DelegationReconcile};

    let reconcile = if error.code == usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown {
        DelegationReconcile::Retained
    } else {
        match perform_compensating_remove(
            sessions,
            teardown,
            &usagi_core::domain::id::OperationId::new().to_string(),
            name,
        ) {
            // A session that is already gone needs no compensation: an earlier
            // attempt's teardown removed it, so nothing was left behind either
            // way.
            Ok(_) | Err(SessionRuntimeError::UnknownSession) => DelegationReconcile::Compensated,
            Err(_) => DelegationReconcile::CompensationFailed,
        }
    };
    SessionRuntimeError::Delegation(DelegationFailure {
        code: error.code,
        message: error.message,
        session_id,
        run_operation_id: run_operation_id.to_owned(),
        reconcile,
    })
}

/// Compensates delegated creates whose dispatch never became durable.
///
/// A delegation builds its worktree before it can dispatch into it, so a daemon
/// that died inside that window left an available session no caller owns and no
/// run points at. This runs before the daemon accepts connections, so no client
/// ever observes such a session, and it uses the same durable teardown a live
/// compensation does.
///
/// A reservation in the dispatch store — even one a restart already failed — is
/// not an orphan: that operation reached the dispatch side, which owns its
/// outcome. Only a create with nothing at all behind it is rolled back.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_failed_delegation_reports_its_reconcile_state_on_the_wire
pub(in crate::runtime::daemon) fn reconcile_orphan_delegations(
    bound: &ConnectionWorkspace,
    dispatch: &DispatchStore,
    teardown: &TeardownSignal,
) -> usize {
    let Ok(candidates) = bound
        .sessions()
        .lock()
        .map_err(|_| ())
        .and_then(|sessions| sessions.delegated_sessions().map_err(|_| ()))
    else {
        return 0;
    };
    candidates
        .into_iter()
        .filter(|candidate| {
            matches!(dispatch.run(candidate.operation_id), Ok(None))
                && matches!(dispatch.admission(candidate.operation_id), Ok(None))
        })
        .filter(|candidate| {
            perform_compensating_remove(
                bound.sessions(),
                teardown,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &candidate.name,
            )
            .is_ok()
        })
        .count()
}

pub(super) enum AgentDispatchRequest {
    Launch(
        String,
        usagi_core::infrastructure::client::AgentLaunchIntent,
    ),
    Goal(String, usagi_core::infrastructure::client::AgentGoalIntent),
    Inventory(WorkspaceId),
    WorkspaceObservation(WorkspaceId),
    Diagnose(
        WorkspaceId,
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
    ),
    PlanRestart(
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
        bool,
    ),
    Restart(
        WorkspaceId,
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
        Vec<usagi_core::domain::id::AgentRuntimeRef>,
        bool,
    ),
    Resume(String, usagi_core::domain::agent::AgentResumeTarget),
    RepairResume(String, usagi_core::domain::agent::AgentResumeTarget, u32),
}
