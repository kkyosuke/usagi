//! Installed binary synchronization while preserving live workspace connections.

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::domain::id::WorkspaceId;
use usagi_core::infrastructure::client::{ClientPolicy, DaemonClient as _};
use usagi_core::infrastructure::ipc::{
    ClientError, ClientWorkspace, DaemonReply, DaemonRequest, TenantAction, TenantInventory,
};
use usagi_daemon::usecase::authority::routing::RESTART_AGENTS_REMEDY;

use super::{
    current_agent_integrations, current_build, diagnostic_client, existing_policy_client,
    managed_update_diagnostic_client, replace_running_daemon_during_update,
};

/// Whether an installed artifact is serving or awaits safe runtime handoff.
pub(crate) enum ManagedUpdateSync {
    Complete,
    Deferred,
}

/// Synchronize the published daemon with the exact installed binary while the
/// installer still owns `update.lock`. An absent daemon stays stopped; live
/// Agent credentials and retained multi-workspace fences defer replacement
/// while preserving the current owner.
// One lock-held owner observation, deferral, replacement, and readiness transaction.
#[allow(clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=managed_update_with_a_live_generic_pty_keeps_the_draining_owner
pub(crate) fn sync_after_update(
    out: &mut dyn Write,
    policy: ClientPolicy,
    info: &AppInfo,
) -> std::io::Result<Result<ManagedUpdateSync, ClientError>> {
    let expected_build = current_build();
    let (lock, client) = match managed_update_diagnostic_client(policy) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let Some(mut owner) = client else {
        writeln!(out, "daemon sync: daemon is not running; left it stopped")?;
        return Ok(Ok(ManagedUpdateSync::Complete));
    };
    let published_build = owner.server_build().clone();
    if published_build != expected_build {
        // A seamless successor hydrates only the startup workspace. The
        // predecessor retains its other fences while serving PTYs, so replacing
        // a multi-workspace owner would strand those projects on the next open.
        // This observation shares bootstrap/lifecycle custody with replacement;
        // ordinary connecting clients cannot adopt a new workspace between them.
        // It precedes Agent diagnosis so a multi-workspace deferral never
        // suggests the single-workspace Agent handoff remedy.
        let retained = owner
            .request(DaemonRequest::Tenant {
                action: TenantAction::Inventory,
                root: None,
                force: false,
            })
            .and_then(retained_workspace_count);
        match retained {
            Ok(Some(workspaces)) => {
                writeln!(
                    out,
                    "daemon sync: deferred to preserve {workspaces} workspace connection(s); \
                     finish live runtimes, then run 'usagi daemon restart' to switch builds"
                )?;
                return Ok(Ok(ManagedUpdateSync::Deferred));
            }
            Ok(None) => {}
            Err(error) => return Ok(Err(error)),
        }
        let workspace = owner
            .request(DaemonRequest::Session {
                action: usagi_core::infrastructure::ipc::SessionAction::List,
                operation_id: usagi_core::domain::id::OperationId::new().to_string(),
                payload: serde_json::json!({}),
            })
            .and_then(|reply| {
                let body = match reply {
                    usagi_core::infrastructure::ipc::DaemonReply::Ok(body)
                    | usagi_core::infrastructure::ipc::DaemonReply::Accepted { body, .. } => body,
                };
                serde_json::from_value::<WorkspaceId>(body["workspace_id"].clone()).map_err(|_| {
                    ClientError::Lifecycle(
                        "daemon returned an invalid workspace identity".to_owned(),
                    )
                })
            });
        let workspace = match workspace {
            Ok(workspace) => workspace,
            Err(error) => return Ok(Err(error)),
        };
        let diagnosis = owner
            .request(DaemonRequest::DiagnoseAgents {
                workspace,
                expected: current_agent_integrations(),
            })
            .and_then(|reply| {
                let body = match reply {
                    usagi_core::infrastructure::ipc::DaemonReply::Ok(body)
                    | usagi_core::infrastructure::ipc::DaemonReply::Accepted { body, .. } => body,
                };
                serde_json::from_value::<usagi_core::domain::agent::AgentIntegrationDiagnosis>(body)
                    .map_err(|_| {
                        ClientError::Lifecycle(
                            "daemon cannot prove server-side handoff fencing".to_owned(),
                        )
                    })
            });
        let diagnosis = match diagnosis {
            Ok(diagnosis) => diagnosis,
            Err(error) => return Ok(Err(error)),
        };
        match diagnosis.provisioned_mcp_callers {
            Some(0) => {}
            Some(credentials) => {
                writeln!(
                    out,
                    "daemon sync: deferred to preserve {credentials} Agent connection(s); {RESTART_AGENTS_REMEDY}"
                )?;
                return Ok(Ok(ManagedUpdateSync::Deferred));
            }
            None => {
                return Ok(Err(ClientError::Lifecycle(
                    "daemon cannot prove server-side handoff fencing".to_owned(),
                )));
            }
        }
        drop(owner);
        if let Err(error) = replace_running_daemon_during_update(out, policy, info, &lock)? {
            return Ok(Err(error));
        }
    }
    // Cold replacement confirms process registration before its endpoint is
    // published. Wait for a read-only serving answer through the existing
    // deadline-bounded client, then verify the exact build under the same locks.
    // This lane never bootstraps or replaces a daemon while waiting.
    let mut serving = match existing_policy_client(policy, ClientWorkspace::Unbound) {
        Ok(client) => client,
        Err(error) => return Ok(Err(error)),
    };
    if let Err(error) = serving.request(DaemonRequest::Tenant {
        action: TenantAction::Inventory,
        root: None,
        force: false,
    }) {
        return Ok(Err(error));
    }
    drop(serving);
    let current = match diagnostic_client(policy, true) {
        Ok(client) => client,
        Err(error) => return Ok(Err(error)),
    };
    if current.server_build() != &expected_build {
        return Ok(Err(ClientError::Lifecycle(
            "daemon synchronization returned before the installed build was serving".to_owned(),
        )));
    }
    writeln!(out, "daemon sync: installed build is current and serving")?;
    Ok(Ok(ManagedUpdateSync::Complete))
}

/// How many workspaces need the old active owner while any runtime remains.
/// Cold replacement releases every fence when the old process exits; a single
/// workspace can use the existing seamless handoff. Unknown runtime ownership
/// is already included in the server's `live_runtimes` count.
fn retained_workspace_count(reply: DaemonReply) -> Result<Option<usize>, ClientError> {
    let body = match reply {
        DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. } => body,
    };
    let inventory: TenantInventory = serde_json::from_value(body).map_err(|_| {
        ClientError::Lifecycle("daemon returned an invalid workspace inventory".to_owned())
    })?;
    let workspaces = inventory.tenants.len();
    Ok((workspaces > 1
        && inventory
            .tenants
            .iter()
            .any(|tenant| tenant.live_runtimes != 0))
    .then_some(workspaces))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_live_multi_workspace_owners_defer_synchronization() {
        for (counts, expected) in [
            (vec![], None),
            (vec![0], None),
            (vec![1], None),
            (vec![0, 0], None),
            (vec![1, 0], Some(2)),
            (vec![0, 1], Some(2)),
            (vec![0, 2, 0], Some(3)),
        ] {
            let tenants: Vec<_> = counts
                .into_iter()
                .enumerate()
                .map(|(index, live_runtimes)| {
                    serde_json::json!({
                        "root": format!("/workspace/{index}"),
                        "sessions": 1,
                        "live_runtimes": live_runtimes,
                    })
                })
                .collect();
            let body = serde_json::json!({"tenants": tenants});
            for reply in [
                DaemonReply::Ok(body.clone()),
                DaemonReply::Accepted {
                    operation_id: "inventory".to_owned(),
                    revision: 1,
                    body,
                },
            ] {
                assert_eq!(retained_workspace_count(reply).unwrap(), expected);
            }
        }
    }

    #[test]
    fn an_unusable_inventory_refuses_replacement_instead_of_assuming_no_work() {
        for body in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"tenants": [{"root": "/workspace", "sessions": 1}]}),
        ] {
            assert_eq!(
                retained_workspace_count(DaemonReply::Ok(body)),
                Err(ClientError::Lifecycle(
                    "daemon returned an invalid workspace inventory".to_owned()
                ))
            );
        }
    }
}
