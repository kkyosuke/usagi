//! Installed binary synchronization with the published daemon and live Agents.

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::domain::id::WorkspaceId;
use usagi_core::infrastructure::client::{ClientPolicy, DaemonClient as _};
use usagi_core::infrastructure::ipc::{ClientError, DaemonRequest};
use usagi_daemon::usecase::authority::routing::RESTART_AGENTS_REMEDY;

use super::{
    current_agent_integrations, current_build, diagnostic_client, managed_update_diagnostic_client,
    replace_running_daemon_during_update,
};

/// Whether an installed artifact is serving or awaits live Agent completion.
pub(crate) enum ManagedUpdateSync {
    Complete,
    Deferred,
}

/// Synchronize the published daemon with the exact installed binary while the
/// installer still owns `update.lock`. An absent daemon stays stopped; live
/// Agent credentials defer replacement while preserving the current owner.
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
    let mut current = match diagnostic_client(policy, true) {
        Ok(client) => client,
        Err(error) => return Ok(Err(error)),
    };
    if current.server_build() != &expected_build {
        return Ok(Err(ClientError::Lifecycle(
            "daemon synchronization returned before the installed build was serving".to_owned(),
        )));
    }
    if let Err(error) = current.request(DaemonRequest::Tenant {
        action: usagi_core::infrastructure::ipc::TenantAction::Inventory,
        root: None,
        force: false,
    }) {
        return Ok(Err(error));
    }
    writeln!(out, "daemon sync: installed build is current and serving")?;
    Ok(Ok(ManagedUpdateSync::Complete))
}
