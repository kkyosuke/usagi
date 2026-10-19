//! Human search borrows existing terminal owners, without attach/resize/input.
use super::{ConnectionWorkspace, SharedAgentRuntime, SharedTerminalRuntime, envelope};
use usagi_core::domain::id::WorkspaceId;
use usagi_core::infrastructure::ipc::{
    Envelope, ErrorCode, ProtocolError, RequestId, ResponseOutcome, ServerHello,
};
use usagi_core::usecase::terminal_search::{
    SearchBudget, SearchTerminalKind, TERMINALS_MAX, TerminalSearchResult, WorkspaceOutputSearch,
    valid_query,
};

#[allow(clippy::too_many_arguments)] // Borrow existing IPC owners and response context without another state owner.
pub(super) fn dispatch(
    agent: &SharedAgentRuntime,
    terminal: &SharedTerminalRuntime,
    bound: &ConnectionWorkspace,
    workspace: WorkspaceId,
    query: &str,
    request_id: RequestId,
    raw: &serde_json::Value,
    hello: &ServerHello,
) -> Envelope {
    let result = if raw.get("caller_context").is_some() || raw.get("_caller_credential").is_some() {
        Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "Terminal output search is human-only",
        ))
    } else if !valid_query(query) {
        Err(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "Search requires 1–256 characters without control characters",
        ))
    } else if bound.workspaces.workspace(workspace).is_none() {
        Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "Workspace is not adopted",
        ))
    } else {
        search(agent, terminal, workspace, query)
            .and_then(|result| serde_json::to_value(result).map_err(unavailable))
    };
    match result {
        Ok(body) => envelope(hello, request_id, ResponseOutcome::Ok, body),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

fn unavailable<T>(_: T) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        "Terminal output search is unavailable",
    )
}

fn search(
    agent: &SharedAgentRuntime,
    terminal: &SharedTerminalRuntime,
    workspace: WorkspaceId,
    query: &str,
) -> Result<WorkspaceOutputSearch, ProtocolError> {
    let mut targets = tagged(
        agent.lock().map_err(unavailable)?.search_targets(workspace),
        SearchTerminalKind::Agent,
    );
    targets.extend(tagged(
        terminal
            .lock()
            .map_err(unavailable)?
            .search_targets(workspace),
        SearchTerminalKind::Terminal,
    ));
    Ok(collect(workspace, query, targets, |reference, kind| {
        inspect(agent, terminal, reference, kind)
    }))
}

fn tagged(
    references: Vec<usagi_core::domain::id::TerminalRef>,
    kind: SearchTerminalKind,
) -> Vec<(usagi_core::domain::id::TerminalRef, SearchTerminalKind)> {
    references
        .into_iter()
        .map(|reference| (reference, kind))
        .collect()
}

// Copy each bounded checkpoint under its owner lock; scanning happens afterward.
pub(super) fn inspect(
    agent: &SharedAgentRuntime,
    terminal: &SharedTerminalRuntime,
    reference: &usagi_core::domain::id::TerminalRef,
    kind: SearchTerminalKind,
) -> Result<usagi_daemon::usecase::terminal::Snapshot, ProtocolError> {
    match kind {
        SearchTerminalKind::Agent => agent
            .lock()
            .map_err(unavailable)?
            .inspect_terminal(reference),
        SearchTerminalKind::Terminal => terminal.lock().map_err(unavailable)?.inspect(reference),
    }
}

pub(super) fn collect(
    workspace: WorkspaceId,
    query: &str,
    targets: Vec<(usagi_core::domain::id::TerminalRef, SearchTerminalKind)>,
    mut snapshot: impl FnMut(
        &usagi_core::domain::id::TerminalRef,
        SearchTerminalKind,
    ) -> Result<usagi_daemon::usecase::terminal::Snapshot, ProtocolError>,
) -> WorkspaceOutputSearch {
    let omitted_terminals = targets.len().saturating_sub(TERMINALS_MAX);
    let mut budget = SearchBudget::default();
    let mut terminals = Vec::new();
    for (reference, kind) in targets.into_iter().take(TERMINALS_MAX) {
        let mut result = TerminalSearchResult::unavailable(reference.clone(), kind);
        if budget.exhausted() {
            result.unavailable = false;
            result.limited = true;
        } else {
            let snapshot = snapshot(&reference, kind);
            if let Ok(snapshot) = snapshot {
                result.output_offset = snapshot.output_offset;
                result.exited = snapshot.exited.is_some();
                result.search(&snapshot.screen, query, &mut budget);
            }
        }
        terminals.push(result);
    }
    WorkspaceOutputSearch {
        workspace,
        query: query.to_owned(),
        terminals,
        omitted_terminals,
    }
}
