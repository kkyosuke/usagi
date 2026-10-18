//! Worker-side validation against both the displayed roots and fresh daemon state.

use super::{
    ClientPolicy, DaemonReply, DaemonRequest, LaneConnection, LifecycleRequestError,
    LifecycleSnapshot, SessionAction, lifecycle_snapshot,
};
use crate::runtime::file_preview::FilePreviewError;
use crate::runtime::preview_authority::PreviewAuthority;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::SessionLifecycle;
use usagi_core::infrastructure::client::DaemonClient;
use usagi_core::infrastructure::ipc::ClientWorkspace;
use usagi_tui::usecase::application::controller::Target;

pub(super) type PreviewRoots = Arc<Mutex<Vec<(SessionId, PathBuf)>>>;
type ObservationConnector =
    Box<dyn FnMut(ClientWorkspace) -> Result<Box<dyn DaemonClient + Send>, String> + Send>;

enum Observer {
    Daemon {
        lane: LaneConnection,
        workspace: ClientWorkspace,
        connect: ObservationConnector,
    },
    #[cfg(test)]
    Fixture(Box<dyn FnMut() -> Result<LifecycleSnapshot, LifecycleRequestError> + Send>),
}

impl Observer {
    fn daemon(root: &Path, connect: ObservationConnector) -> Self {
        Self::Daemon {
            lane: LaneConnection::observing(),
            workspace: ClientWorkspace::Selected {
                root: usagi_core::infrastructure::paths::wire_workspace_root(root),
            },
            connect,
        }
    }

    fn snapshot(&mut self) -> Result<LifecycleSnapshot, LifecycleRequestError> {
        match self {
            Self::Daemon {
                lane,
                workspace,
                connect,
            } => {
                if lane.client.is_none() {
                    lane.client =
                        Some(connect(workspace.clone()).map_err(LifecycleRequestError::Decode)?);
                }
                let reply = lane
                    .request(DaemonRequest::Session {
                        action: SessionAction::List,
                        operation_id: usagi_core::domain::id::OperationId::new().to_string(),
                        payload: serde_json::json!({}),
                    })
                    .map_err(LifecycleRequestError::Decode)?;
                match reply {
                    DaemonReply::Ok(value) => {
                        lifecycle_snapshot(&value).map_err(LifecycleRequestError::Decode)
                    }
                    DaemonReply::Accepted { .. } => Err(LifecycleRequestError::Decode(
                        "daemon did not return a session snapshot".into(),
                    )),
                }
            }
            #[cfg(test)]
            Self::Fixture(observe) => observe(),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=worker_observations_bind_selected_root_on_connect_and_reconnect
fn connect_observer(workspace: ClientWorkspace) -> Result<Box<dyn DaemonClient + Send>, String> {
    // This adapter only attaches to an existing daemon. Capturing the explicit
    // workspace also keeps PolicyClient reconnects independent of a concurrent
    // inactive Garden refresh changing the process-wide selected workspace.
    let client = crate::runtime::daemon::existing_policy_client(ClientPolicy::tui(), workspace)
        .map_err(super::daemon_error_reason)?;
    Ok(Box::new(client))
}

pub(super) fn lock_roots(roots: &PreviewRoots) -> MutexGuard<'_, Vec<(SessionId, PathBuf)>> {
    roots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) struct PreviewAccess {
    workspace: WorkspaceId,
    root: PathBuf,
    sessions: PreviewRoots,
    observe: Observer,
}

impl PreviewAccess {
    pub(super) fn new(workspace: WorkspaceId, root: PathBuf, sessions: PreviewRoots) -> Self {
        let observe = Observer::daemon(&root, Box::new(connect_observer));
        Self {
            workspace,
            root,
            sessions,
            // Reuse this worker's observation connection. Previewing must not
            // bootstrap a missing daemon or pay for a new handshake per file.
            observe,
        }
    }

    #[cfg(test)]
    pub(super) fn with_observer(
        workspace: WorkspaceId,
        root: PathBuf,
        sessions: PreviewRoots,
        observe: Box<dyn FnMut() -> Result<LifecycleSnapshot, LifecycleRequestError> + Send>,
    ) -> Self {
        Self {
            workspace,
            root,
            sessions,
            observe: Observer::Fixture(observe),
        }
    }

    fn registered(&self, target: Target, root: &Path) -> bool {
        match target {
            Target::Root(workspace) => workspace == self.workspace && root == self.root,
            Target::Session(session) => lock_roots(&self.sessions)
                .iter()
                .any(|(id, path)| *id == session && path == root),
        }
    }
}

impl PreviewAuthority for PreviewAccess {
    fn validate(&mut self, target: Target, root: &Path) -> Result<(), FilePreviewError> {
        if !self.registered(target, root) {
            return Err(FilePreviewError::ScopeUnavailable);
        }
        let Target::Session(id) = target else {
            return Ok(());
        };
        // The TUI's inventory can lag a different client's remove/create. A
        // fresh observation on this worker fences that window too.
        let snapshot = self
            .observe
            .snapshot()
            .map_err(|_| FilePreviewError::ScopeUnavailable)?;
        let available = snapshot.workspace_id == self.workspace
            && snapshot.sessions.iter().any(|session| {
                session.session_id == id
                    && session.lifecycle == SessionLifecycle::Available
                    && self.root.join(".usagi/sessions").join(&session.name) == root
            });
        if available && self.registered(target, root) {
            Ok(())
        } else {
            Err(FilePreviewError::ScopeUnavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use usagi_core::domain::id::WorktreeId;
    use usagi_core::domain::session_lifecycle::ManagedSession;
    use usagi_core::infrastructure::client::DaemonClient;

    struct Replies(std::collections::VecDeque<Result<DaemonReply, super::super::ClientError>>);

    impl DaemonClient for Replies {
        fn request(
            &mut self,
            request: DaemonRequest,
        ) -> Result<DaemonReply, super::super::ClientError> {
            assert!(matches!(
                request,
                DaemonRequest::Session {
                    action: SessionAction::List,
                    ..
                }
            ));
            self.0.pop_front().unwrap()
        }
    }

    #[test]
    fn worker_observations_reuse_the_connection_and_require_a_fresh_valid_snapshot() {
        let value = serde_json::json!({
            "workspace_id": WorkspaceId::new(), "root_worktree_id": WorktreeId::new(),
            "revision": 1, "sessions": []
        });
        let mut second = value.clone();
        second["revision"] = 2.into();
        let replies = [
            Ok(DaemonReply::Ok(value)),
            Ok(DaemonReply::Ok(second)),
            Ok(DaemonReply::Ok(serde_json::Value::Null)),
            Ok(DaemonReply::Accepted {
                operation_id: "unused".into(),
                revision: 3,
                body: serde_json::Value::Null,
            }),
            Err(super::super::ClientError::Unavailable("offline".into())),
        ];
        let mut observer = Observer::Daemon {
            lane: LaneConnection {
                client: Some(Box::new(Replies(replies.into()))),
                cold_start_budget: 0,
            },
            workspace: ClientWorkspace::Selected {
                root: "/workspace".into(),
            },
            connect: Box::new(connect_observer),
        };
        assert_eq!(
            observer
                .snapshot()
                .map_err(LifecycleRequestError::reason)
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            observer
                .snapshot()
                .map_err(LifecycleRequestError::reason)
                .unwrap()
                .revision,
            2
        );
        for _ in 0..3 {
            assert!(observer.snapshot().is_err());
        }
        assert!(matches!(
            observer,
            Observer::Daemon {
                lane: LaneConnection {
                    client: None,
                    cold_start_budget: 0,
                },
                ..
            }
        ));
    }

    #[test]
    fn worker_observations_bind_selected_root_on_connect_and_reconnect() {
        let roots = Arc::new(Mutex::new(Vec::new()));
        let claimed = Arc::clone(&roots);
        let mut connections = 0;
        let value = serde_json::json!({
            "workspace_id": WorkspaceId::new(), "root_worktree_id": WorktreeId::new(),
            "revision": 1, "sessions": []
        });
        let mut observer = Observer::daemon(
            Path::new("/workspace/owned"),
            Box::new(move |workspace| {
                claimed.lock().unwrap().push(workspace);
                connections += 1;
                if connections == 1 {
                    return Err("daemon temporarily unavailable".into());
                }
                Ok(Box::new(Replies(
                    [
                        Ok(DaemonReply::Ok(value.clone())),
                        Err(super::super::ClientError::Unavailable(
                            "disconnected".into(),
                        )),
                    ]
                    .into(),
                )))
            }),
        );
        assert!(observer.snapshot().is_err());
        assert!(observer.snapshot().is_ok());
        assert!(observer.snapshot().is_err());
        assert!(observer.snapshot().is_ok());
        assert_eq!(
            *roots.lock().unwrap(),
            vec![
                ClientWorkspace::Selected {
                    root: "/workspace/owned".into(),
                };
                3
            ]
        );
    }

    fn snapshot(
        workspace: WorkspaceId,
        id: SessionId,
        name: &str,
        lifecycle: SessionLifecycle,
    ) -> LifecycleSnapshot {
        let mut session = ManagedSession::adopt_available(name.into(), Utc::now());
        session.session_id = id;
        session.lifecycle = lifecycle;
        LifecycleSnapshot {
            workspace_id: workspace,
            root_worktree_id: WorktreeId::new(),
            revision: 1,
            sessions: vec![session],
            agent_resumes: BTreeMap::new(),
            session_roles: BTreeMap::new(),
        }
    }

    #[test]
    fn fresh_daemon_identity_lifecycle_and_path_are_required_even_before_ui_refresh() {
        let workspace = WorkspaceId::new();
        let id = SessionId::new();
        let root = PathBuf::from("/workspace");
        let path = root.join(".usagi/sessions/same");
        let roots = Arc::new(Mutex::new(vec![(id, path.clone())]));
        for (observed_workspace, observed_id, name, lifecycle, expected) in [
            (workspace, id, "same", SessionLifecycle::Available, Ok(())),
            (
                WorkspaceId::new(),
                id,
                "same",
                SessionLifecycle::Available,
                Err(FilePreviewError::ScopeUnavailable),
            ),
            (
                workspace,
                SessionId::new(),
                "same",
                SessionLifecycle::Available,
                Err(FilePreviewError::ScopeUnavailable),
            ),
            (
                workspace,
                id,
                "same",
                SessionLifecycle::Deleting,
                Err(FilePreviewError::ScopeUnavailable),
            ),
            (
                workspace,
                id,
                "other",
                SessionLifecycle::Available,
                Err(FilePreviewError::ScopeUnavailable),
            ),
        ] {
            let mut access = PreviewAccess::with_observer(
                workspace,
                root.clone(),
                Arc::clone(&roots),
                Box::new(move || Ok(snapshot(observed_workspace, observed_id, name, lifecycle))),
            );
            assert_eq!(access.validate(Target::Session(id), &path), expected);
        }
    }

    #[test]
    fn registered_roots_are_checked_before_and_after_daemon_observation() {
        let workspace = WorkspaceId::new();
        let id = SessionId::new();
        let root = PathBuf::from("/workspace");
        let path = root.join(".usagi/sessions/same");
        let roots = Arc::new(Mutex::new(vec![]));
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let changed = Arc::clone(&roots);
        let mut access = PreviewAccess::with_observer(
            workspace,
            root.clone(),
            Arc::clone(&roots),
            Box::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                lock_roots(&changed).clear();
                Ok(snapshot(workspace, id, "same", SessionLifecycle::Available))
            }),
        );
        assert_eq!(
            access.validate(Target::Session(id), &path),
            Err(FilePreviewError::ScopeUnavailable)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        lock_roots(&roots).push((id, path.clone()));
        assert_eq!(
            access.validate(Target::Session(id), &path),
            Err(FilePreviewError::ScopeUnavailable)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut access = PreviewAccess::new(workspace, root.clone(), roots);
        assert_eq!(access.validate(Target::Root(workspace), &root), Ok(()));
        assert_eq!(
            access.validate(Target::Root(WorkspaceId::new()), &root),
            Err(FilePreviewError::ScopeUnavailable)
        );
        assert_eq!(
            access.validate(Target::Root(workspace), &path),
            Err(FilePreviewError::ScopeUnavailable)
        );
    }

    #[test]
    fn daemon_observation_failure_is_closed_and_poisoned_inventory_is_recovered() {
        let workspace = WorkspaceId::new();
        let id = SessionId::new();
        let path = PathBuf::from("/workspace/.usagi/sessions/same");
        let roots = Arc::new(Mutex::new(vec![(id, path.clone())]));
        let mut access = PreviewAccess::with_observer(
            workspace,
            PathBuf::from("/workspace"),
            Arc::clone(&roots),
            Box::new(|| Err(LifecycleRequestError::Decode("unavailable".into()))),
        );
        assert_eq!(
            access.validate(Target::Session(id), &path),
            Err(FilePreviewError::ScopeUnavailable)
        );
        let poison = Arc::clone(&roots);
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("poison fixture");
            })
            .join()
            .is_err()
        );
        assert_eq!(lock_roots(&roots).len(), 1);
    }
}
