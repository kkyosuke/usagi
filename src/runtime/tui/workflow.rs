//! Owned, bounded background lane for session Workflow requests.

use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;

use usagi_core::domain::workflow::WorkflowSnapshot;
use usagi_core::infrastructure::client::{ClientPolicy, DaemonClient};
use usagi_core::infrastructure::ipc::RetryMode;
use usagi_core::infrastructure::ipc::{ClientError, DaemonReply, DaemonRequest};
use usagi_tui::usecase::application::controller::{AppEvent, BackendEvent};
use usagi_tui::usecase::application::daemon_backend::Completions;
use usagi_tui::usecase::application::workflow::{WorkflowError, WorkflowJob, WorkflowPort};

type Task = (WorkflowJob, Completions);
type Runner = fn(&WorkflowJob) -> Result<WorkflowSnapshot, WorkflowError>;

pub(super) struct DaemonWorkflowPort {
    sender: Option<SyncSender<Task>>,
    worker: Option<JoinHandle<()>>,
    runner: Runner,
}

impl Default for DaemonWorkflowPort {
    fn default() -> Self {
        Self {
            sender: None,
            worker: None,
            runner: perform,
        }
    }
}

impl DaemonWorkflowPort {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=workflow_worker_owns_and_reaps_its_lane
    fn start(&mut self) {
        let (sender, receiver) = mpsc::sync_channel::<Task>(16);
        let run = self.runner;
        self.worker = Some(std::thread::spawn(move || {
            while let Ok((job, completions)) = receiver.recv() {
                let result = run(&job);
                completions.emit(AppEvent::Backend(BackendEvent::Workflow {
                    job,
                    result: result.map(Box::new),
                }));
            }
        }));
        self.sender = Some(sender);
    }
}

impl WorkflowPort for DaemonWorkflowPort {
    fn dispatch(&mut self, job: WorkflowJob, completions: Completions) {
        if self.sender.is_none() {
            self.start();
        }
        if let Err(error) = self
            .sender
            .as_ref()
            .expect("started workflow worker")
            .try_send((job, completions))
        {
            let (mpsc::TrySendError::Full((job, completions))
            | mpsc::TrySendError::Disconnected((job, completions))) = error;
            completions.emit(AppEvent::Backend(BackendEvent::Workflow {
                job,
                result: Err(WorkflowError {
                    message: "Workflow request queue is unavailable; retry".into(),
                    unconfirmed: false,
                }),
            }));
        }
    }
}

impl Drop for DaemonWorkflowPort {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=workflow_worker_owns_and_reaps_its_lane
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=workflow_request_preserves_exact_scope_and_control_identity
fn perform(job: &WorkflowJob) -> Result<WorkflowSnapshot, WorkflowError> {
    let mut client = crate::runtime::daemon::policy_client(ClientPolicy::tui()).map_err(failure)?;
    execute(job, &mut client)
}

fn execute(
    job: &WorkflowJob,
    client: &mut impl DaemonClient,
) -> Result<WorkflowSnapshot, WorkflowError> {
    let request = match &job.control {
        Some((operation_id, command)) => DaemonRequest::WorkflowControl {
            workspace: job.workspace,
            session: job.session,
            operation_id: *operation_id,
            command: command.clone(),
        },
        None => DaemonRequest::WorkflowSnapshot {
            workspace: job.workspace,
            session: job.session,
        },
    };
    let reply = client.request(request).map_err(failure)?;
    let DaemonReply::Ok(body) = reply else {
        return Err(WorkflowError {
            message: "Workflow result is unconfirmed; retry the same request".into(),
            unconfirmed: true,
        });
    };
    let snapshot: WorkflowSnapshot = serde_json::from_value(body).map_err(|_| WorkflowError {
        message: "Invalid workflow response; result unconfirmed".into(),
        unconfirmed: true,
    })?;
    if snapshot.session != job.session {
        return Err(WorkflowError {
            message: "Workflow response belongs to another session".into(),
            unconfirmed: true,
        });
    }
    Ok(snapshot)
}

/// Classify a failed request as "may have taken effect" or not.
///
/// The daemon already says this on every protocol error: `RetryMode::Never`
/// means resending the same operation cannot succeed, which is only reported
/// for a request that did nothing. Reading that instead of matching an
/// allowlist of error codes keeps the retry contract in one place — the code
/// list silently classified every new code as unconfirmed, which is how a
/// refused start stayed pending in the pane forever.
///
/// Anything that is not a protocol error is a transport failure: the request
/// may have been applied before the answer was lost, so it must be retried
/// under the same operation ID rather than minted again.
fn failure(error: ClientError) -> WorkflowError {
    let message =
        usagi_core::domain::presentation_text::sanitize_presentation_line(&error.to_string());
    let unconfirmed = !matches!(error, ClientError::Protocol(error)
        if error.retry_mode == RetryMode::Never);
    WorkflowError {
        message,
        unconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
    use usagi_core::domain::workflow::WorkflowCommand;
    use usagi_core::infrastructure::ipc::ErrorCode;

    struct Fake {
        requests: Vec<DaemonRequest>,
        reply: Option<Result<DaemonReply, ClientError>>,
    }
    impl DaemonClient for Fake {
        fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
            self.requests.push(request);
            self.reply.take().unwrap()
        }
    }
    fn job() -> WorkflowJob {
        WorkflowJob {
            workspace: WorkspaceId::new(),
            session: SessionId::new(),
            control: None,
        }
    }
    #[test]
    fn workflow_request_preserves_exact_scope_and_control_identity() {
        let mut job = job();
        for control in [
            None,
            Some((
                OperationId::new(),
                WorkflowCommand::Start {
                    goal: "Review login".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                },
            )),
        ] {
            job.control = control;
            let snapshot = WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session: job.session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            };
            let mut fake = Fake {
                requests: vec![],
                reply: Some(Ok(DaemonReply::Ok(
                    serde_json::to_value(&snapshot).unwrap(),
                ))),
            };
            assert_eq!(execute(&job, &mut fake).unwrap(), snapshot);
            assert_eq!(fake.requests.len(), 1);
            let expected = if let Some((operation_id, command)) = &job.control {
                DaemonRequest::WorkflowControl {
                    workspace: job.workspace,
                    session: job.session,
                    operation_id: *operation_id,
                    command: command.clone(),
                }
            } else {
                DaemonRequest::WorkflowSnapshot {
                    workspace: job.workspace,
                    session: job.session,
                }
            };
            assert_eq!(fake.requests[0], expected);
        }
        let mut fake = Fake {
            requests: vec![],
            reply: Some(Ok(DaemonReply::Ok(serde_json::Value::Null))),
        };
        assert!(execute(&job, &mut fake).unwrap_err().unconfirmed);
        let snapshot = WorkflowSnapshot {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            session: SessionId::new(),
            run: None,
            pending_start: None,
            finished: Vec::new(),
        };
        fake.reply = Some(Ok(DaemonReply::Ok(serde_json::to_value(snapshot).unwrap())));
        assert!(execute(&job, &mut fake).unwrap_err().unconfirmed);
        fake.reply = Some(Err(ClientError::Unavailable("disconnected".into())));
        assert!(execute(&job, &mut fake).unwrap_err().unconfirmed);
        fake.reply = Some(Ok(DaemonReply::Accepted {
            operation_id: "pending".into(),
            revision: 1,
            body: serde_json::Value::Null,
        }));
        assert!(execute(&job, &mut fake).unwrap_err().unconfirmed);
        // `Busy` is what a session that already runs an Agent answers a start
        // with. It has to read as confirmed, or the pane keeps a pending start
        // nothing can ever complete.
        for code in [
            ErrorCode::InvalidArgument,
            ErrorCode::PermissionDenied,
            ErrorCode::OwnershipUnknown,
            ErrorCode::IdempotencyConflict,
            ErrorCode::Busy,
            ErrorCode::Unavailable,
            ErrorCode::DeadlineExceeded,
            ErrorCode::ResyncRequired,
        ] {
            let protocol = usagi_core::infrastructure::ipc::ProtocolError::new(code, "rejected");
            let retryable = protocol.retry_mode != RetryMode::Never;
            let error = failure(ClientError::Protocol(protocol));
            assert_eq!(error.unconfirmed, retryable, "{code:?} keeps its pending");
        }
        // A transport failure is always unconfirmed: the request may have been
        // applied before its answer was lost.
        assert!(
            failure(ClientError::Unavailable("socket closed".into())).unconfirmed,
            "a lost answer must retry the same operation"
        );
    }
    fn fake_run(job: &WorkflowJob) -> Result<WorkflowSnapshot, WorkflowError> {
        if job.control.is_some() {
            return Err(WorkflowError {
                message: "fake control failure".into(),
                unconfirmed: true,
            });
        }
        Ok(WorkflowSnapshot {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            session: job.session,
            run: None,
            pending_start: None,
            finished: Vec::new(),
        })
    }
    #[test]
    fn workflow_worker_owns_and_reaps_its_lane() {
        let default_port = DaemonWorkflowPort::default();
        assert!(default_port.sender.is_none());
        assert!(default_port.worker.is_none());
        drop(default_port);
        let mut port = DaemonWorkflowPort {
            sender: None,
            worker: None,
            runner: fake_run,
        };
        let (completions, events) = Completions::channel();
        let job = job();
        port.dispatch(job.clone(), completions);
        assert!(
            matches!(events.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), AppEvent::Backend(BackendEvent::Workflow { job: returned, result: Ok(_) }) if returned == job)
        );
        let (completions, events) = Completions::channel();
        let control = WorkflowJob {
            control: Some((
                OperationId::new(),
                WorkflowCommand::Start {
                    goal: "task".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                },
            )),
            ..job
        };
        port.dispatch(control, completions);
        assert!(matches!(
            events
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            AppEvent::Backend(BackendEvent::Workflow { result: Err(_), .. })
        ));
        drop(port);
    }

    #[test]
    fn workflow_queue_refusal_is_reported_without_losing_the_request() {
        let (sender, receiver) = mpsc::sync_channel(0);
        let mut port = DaemonWorkflowPort {
            sender: Some(sender),
            worker: None,
            runner: fake_run,
        };
        for disconnected in [false, true] {
            let (completions, events) = Completions::channel();
            port.dispatch(job(), completions);
            assert!(matches!(
                events
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                AppEvent::Backend(BackendEvent::Workflow {
                    result: Err(WorkflowError {
                        unconfirmed: false,
                        ..
                    }),
                    ..
                })
            ));
            if !disconnected {
                let (sender, ignored) = mpsc::sync_channel(0);
                drop(ignored);
                port.sender = Some(sender);
            }
        }
        drop(receiver);
    }
}
