//! Conversation-scoped decision authority, independent of retained run history.

use super::{
    AgentRuntime, BTreeSet, DispatchBinding, ErrorCode, ProtocolError, RunStatus,
    UserDecisionOwner, dispatch_binding_unavailable, map_dispatch_storage_error,
    unknown_caller_provenance,
};
use crate::usecase::runtime::DurableRuntimeRecord;

pub(super) struct ConversationDecisionAuthority {
    pub owner: UserDecisionOwner,
    pub aliases: Vec<UserDecisionOwner>,
}

impl ConversationDecisionAuthority {
    fn new(owner: UserDecisionOwner, mut aliases: Vec<UserDecisionOwner>) -> Self {
        aliases.retain(|alias| alias != &owner);
        Self { owner, aliases }
    }

    fn into_owners(mut self) -> Vec<UserDecisionOwner> {
        self.aliases.push(self.owner);
        self.aliases
    }
}

impl AgentRuntime {
    /// Resolves decision authority only after authenticating the current live
    /// runtime and dispatch run. Exact resume retains the conversation's immutable
    /// owner; the credential and report/completion fence still name the new run.
    ///
    /// # Panics
    /// Panics if an internal authority resolver returns an empty owner set.
    pub fn mcp_user_decision_owner(
        &self,
        credential: &str,
    ) -> Result<UserDecisionOwner, ProtocolError> {
        Ok(self
            .mcp_user_decision_owners(credential)?
            .into_iter()
            .next()
            .expect("resolved conversation authority includes its canonical owner"))
    }

    /// Returns the primary owner followed by any other immutable owners used by
    /// retained legacy incarnations of the same exact conversation. New runs
    /// copy this set; they never acquire an alias merely by reusing an Agent.
    ///
    /// # Panics
    /// Panics if an authenticated dispatch context loses its runtime during
    /// this immutable lookup, which would violate the owner invariant.
    pub fn mcp_user_decision_owners(
        &self,
        credential: &str,
    ) -> Result<Vec<UserDecisionOwner>, ProtocolError> {
        let context = self
            .mcp_dispatch_context(credential)
            .ok_or_else(unknown_caller_provenance)?;
        let record = self
            .coordinator
            .record_for(&context.runtime)
            .expect("authenticated dispatch context verified this runtime");
        let run = self
            .dispatch
            .run(context.run_id)
            .map_err(map_dispatch_storage_error)?
            .filter(|run| {
                run.status == RunStatus::Running && run.agent_id == context.caller.agent_id
            })
            .ok_or_else(unknown_caller_provenance)?;
        if record.operation.operation_id != run.run_id {
            return Err(unknown_caller_provenance());
        }
        let authority = if record.user_decision_owner.is_some() {
            self.user_decision_authority_for_record(record, &[])
        } else {
            self.user_decision_authority_for_record(record, &self.coordinator.snapshot().records)
        }?;
        Ok(authority.into_owners())
    }

    /// Legacy records have no anchor. Follow only the retained, fully fenced
    /// exact-resume chain; never infer authority from a reused Agent identity or
    /// its creation audit. If older history was collected, the oldest verifiable
    /// binding is the boundary of the authority we can recover.
    pub(super) fn user_decision_authority_for_record(
        &self,
        record: &DurableRuntimeRecord,
        records: &[DurableRuntimeRecord],
    ) -> Result<ConversationDecisionAuthority, ProtocolError> {
        let mut cursor = record;
        let mut seen = BTreeSet::new();
        let mut binding = self
            .decision_binding(record)?
            .ok_or_else(dispatch_binding_unavailable)?;
        let initial_worker = binding.worker.clone();
        let mut aliases = Vec::new();
        loop {
            if !seen.insert(cursor.runtime.agent_runtime_id) {
                return Err(decision_lineage_unavailable());
            }
            if binding.worker != initial_worker {
                return Err(decision_lineage_unavailable());
            }
            if let Some(owner) = &cursor.user_decision_owner {
                if owner.workspace_id != record.runtime.terminal.workspace_id
                    || owner.session_id != binding.worker.session_id
                    || owner.caller != binding.caller
                    || (cursor.resumed_from.is_none()
                        && owner.run_id != cursor.operation.operation_id)
                    || cursor.user_decision_owner_aliases.iter().any(|alias| {
                        alias.workspace_id != owner.workspace_id
                            || alias.session_id != owner.session_id
                    })
                {
                    return Err(decision_lineage_unavailable());
                }
                for alias in &cursor.user_decision_owner_aliases {
                    if !aliases.contains(alias) {
                        aliases.push(alias.clone());
                    }
                }
                return Ok(ConversationDecisionAuthority::new(owner.clone(), aliases));
            }
            let fallback = UserDecisionOwner {
                workspace_id: record.runtime.terminal.workspace_id,
                session_id: binding.worker.session_id,
                caller: binding.caller.clone(),
                run_id: cursor.operation.operation_id,
            };
            if !aliases.contains(&fallback) {
                aliases.push(fallback.clone());
            }
            let source = cursor.resumed_from.and_then(|source_id| {
                records
                    .iter()
                    .find(|candidate| candidate.resume_source == Some(source_id))
            });
            let Some(source) = source else {
                return Ok(ConversationDecisionAuthority::new(fallback, aliases));
            };
            if cursor.continuation.is_none()
                || source.continuation != cursor.continuation
                || source.superseded_by != Some(cursor.runtime.agent_runtime_id)
                || source.runtime.terminal.workspace_id != cursor.runtime.terminal.workspace_id
                || source.runtime.session_id != cursor.runtime.session_id
                || source.runtime.terminal.worktree_id != cursor.runtime.terminal.worktree_id
                || source.launch.request.scope != cursor.launch.request.scope
            {
                return Err(decision_lineage_unavailable());
            }
            let Some(source_binding) = self.decision_binding(source)? else {
                // Runtime and dispatch histories have independent retention.
                // A missing ancestor binding ends recovery without authorizing
                // a guess or blocking this source's otherwise valid resume.
                return Ok(ConversationDecisionAuthority::new(fallback, aliases));
            };
            binding = source_binding;
            cursor = source;
        }
    }

    fn decision_binding(
        &self,
        record: &DurableRuntimeRecord,
    ) -> Result<Option<DispatchBinding>, ProtocolError> {
        let Some(binding) = self
            .dispatch
            .binding(record.operation.operation_id)
            .map_err(map_dispatch_storage_error)?
        else {
            return Ok(None);
        };
        if binding.run_id != record.operation.operation_id
            || binding.worker.session_id != record.runtime.session_id
            || self
                .dispatch
                .workspace_for_agent(binding.worker.agent_id)
                .map_err(map_dispatch_storage_error)?
                != Some(record.runtime.terminal.workspace_id)
        {
            return Err(decision_lineage_unavailable());
        }
        Ok(Some(binding))
    }
}

fn decision_lineage_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::OwnershipUnknown,
        "conversation decision authority is unavailable",
    )
}
