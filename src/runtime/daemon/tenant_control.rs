//! Live tenant observation and explicit retirement at the IPC composition edge.
//!
//! The registry and runtime owners remain daemon-wide. This module only joins
//! them for the unbound tenant control surface, keeping that policy out of the
//! socket accept loop and the rest of the lifecycle composition.

use std::sync::Arc;

use usagi_core::infrastructure::ipc::{Envelope, ErrorCode, ProtocolError, ResponseOutcome};
use usagi_core::infrastructure::paths;
use usagi_core::usecase::client::{DaemonRequest, TenantAction, TenantInventory, TenantSummary};
use usagi_daemon::usecase::tenant::{RetireError, TenantRegistry};

use super::{
    FileWorkspaceFences, SharedAgentRuntime, SharedTerminalRuntime, SystemTenantOpener, envelope,
};

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-08-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
pub(super) fn dispatch(
    tenants: &Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    terminal: &SharedTerminalRuntime,
    agent: &SharedAgentRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> Envelope {
    let request = match serde_json::from_value::<DaemonRequest>(body.clone()) {
        Ok(DaemonRequest::Tenant {
            action,
            root,
            force,
        }) => (action, root, force),
        _ => {
            return envelope(
                hello,
                request_id,
                ResponseOutcome::Error(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "invalid tenant request",
                )),
                serde_json::Value::Null,
            );
        }
    };

    let result: Result<serde_json::Value, ProtocolError> = match request {
        (TenantAction::Inventory, None, false) => inventory(tenants, terminal, agent),
        (TenantAction::Inventory, _, _) => Err(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "tenant inventory does not accept a root or force",
        )),
        (TenantAction::Retire, Some(root), force) => retire(tenants, terminal, agent, &root, force),
        (TenantAction::Retire, None, _) => Err(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "tenant retire requires a workspace root",
        )),
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

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-08-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
fn inventory(
    tenants: &Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    terminal: &SharedTerminalRuntime,
    agent: &SharedAgentRuntime,
) -> Result<serde_json::Value, ProtocolError> {
    let mut summaries = Vec::new();
    for tenant in tenants.adopted() {
        let sessions = tenant
            .runtime()
            .lock()
            .map_err(|_| unavailable("workspace inventory is unavailable"))?
            .session_count()
            .map_err(|_| unavailable("workspace inventory is unavailable"))?;
        let workspace = tenant.workspace_id();
        let terminal_count = terminal
            .lock()
            .map_err(|_| unavailable("terminal inventory is unavailable"))?
            .retirement_blocker_count_in_workspace(workspace);
        let agent_count = agent
            .lock()
            .map_err(|_| unavailable("Agent inventory is unavailable"))?
            .retirement_blocker_count(workspace);
        summaries.push(TenantSummary {
            root: paths::wire_workspace_root(tenant.root()),
            sessions,
            live_runtimes: terminal_count.saturating_add(agent_count),
        });
    }
    serde_json::to_value(TenantInventory { tenants: summaries }).map_err(|_| {
        ProtocolError::new(ErrorCode::Internal, "tenant inventory could not be encoded")
    })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-08-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
fn retire(
    tenants: &Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    terminal: &SharedTerminalRuntime,
    agent: &SharedAgentRuntime,
    root: &str,
    force: bool,
) -> Result<serde_json::Value, ProtocolError> {
    let root = paths::canonical_workspace_root(root)
        .map_err(|_| ProtocolError::new(ErrorCode::InvalidArgument, "workspace root is invalid"))?;
    let tenant = tenants.begin_retire(&root).map_err(map_retire_error)?;
    let workspace = tenant.workspace_id();
    let retirement = (|| {
        let unfinished = tenant
            .runtime()
            .lock()
            .map_err(|_| unavailable("workspace lifecycle is unavailable"))?
            .has_unfinished_work()
            .map_err(|_| unavailable("workspace lifecycle is unavailable"))?;
        if unfinished {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "workspace tenant has unfinished lifecycle work",
            ));
        }
        let terminal_count = terminal
            .lock()
            .map_err(|_| unavailable("terminal owner is unavailable"))?
            .retirement_blocker_count_in_workspace(workspace);
        let agent_count = agent
            .lock()
            .map_err(|_| unavailable("Agent owner is unavailable"))?
            .retirement_blocker_count(workspace);
        if terminal_count.saturating_add(agent_count) != 0 && !force {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "workspace tenant has live or ownership-unknown runtimes; retry with --force",
            ));
        }
        // Cleanup is mandatory even without live processes: it removes
        // retained records and converges a store write that may have failed
        // after an earlier in-memory close.
        terminal
            .lock()
            .map_err(|_| unavailable("terminal owner is unavailable"))?
            .close_workspace(workspace)?;
        agent
            .lock()
            .map_err(|_| unavailable("Agent owner is unavailable"))?
            .close_workspace(workspace)?;
        Ok(())
    })();
    if let Err(error) = retirement {
        tenants.cancel_retire(&root);
        return Err(error);
    }
    if !tenants.complete_retire(&root) {
        tenants.cancel_retire(&root);
        return Err(ProtocolError::new(
            ErrorCode::Internal,
            "workspace retirement lost registry ownership",
        ));
    }
    Ok(serde_json::json!({ "retired": paths::wire_workspace_root(&root) }))
}

fn map_retire_error(error: RetireError) -> ProtocolError {
    match error {
        RetireError::NotFound => {
            ProtocolError::new(ErrorCode::NotFound, "workspace tenant is not held")
        }
        RetireError::Initial => ProtocolError::new(
            ErrorCode::PermissionDenied,
            "the daemon startup workspace cannot be retired",
        ),
        RetireError::Busy => ProtocolError::new(
            ErrorCode::Busy,
            "workspace tenant is serving another request",
        ),
    }
}

fn unavailable(message: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorCode::Unavailable, message)
}
