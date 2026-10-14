//! Fast cache maintenance admission and observation, independent of tree IO.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use usagi_core::domain::id::WorkspaceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheJobState {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheJob {
    pub operation_id: String,
    pub apply: bool,
    pub state: CacheJobState,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheJobRequest {
    Run { apply: bool },
    Status,
}

impl CacheJobRequest {
    /// # Errors
    /// Rejects non-boolean flags and a status query that also asks to delete.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, &'static str> {
        let flag = |name| match payload.get(name) {
            None => Ok(false),
            Some(serde_json::Value::Bool(value)) => Ok(*value),
            Some(_) => Err("cache flags must be boolean"),
        };
        let apply = flag("apply")?;
        if flag("status")? {
            if apply {
                return Err("cache status cannot apply cleanup");
            }
            Ok(Self::Status)
        } else {
            Ok(Self::Run { apply })
        }
    }
}

/// One last result per workspace bounds retention. A pending or running job
/// coalesces subsequent requests; retries cannot launch parallel tree IO.
#[derive(Clone, Default)]
pub struct CacheJobs(Arc<Mutex<BTreeMap<WorkspaceId, CacheJob>>>);

impl CacheJobs {
    /// # Errors
    /// Returns an unavailable owner or a conflicting operation mode.
    pub fn reply(
        &self,
        workspace: WorkspaceId,
        operation_id: &str,
        request: CacheJobRequest,
    ) -> Result<serde_json::Value, &'static str> {
        match request {
            CacheJobRequest::Run { apply } => Ok(serde_json::json!(self.request(
                workspace,
                operation_id,
                apply,
            )?)),
            CacheJobRequest::Status => Ok(self.observe(workspace)?.map_or_else(
                || serde_json::json!({"state":"idle"}),
                |job| serde_json::json!(job),
            )),
        }
    }
    /// # Errors
    /// Rejects a poisoned owner or reuse of an operation with another mode.
    pub fn request(
        &self,
        workspace: WorkspaceId,
        operation_id: &str,
        apply: bool,
    ) -> Result<CacheJob, &'static str> {
        let mut jobs = self.0.lock().map_err(|_| "cache job owner unavailable")?;
        if let Some(job) = jobs.get(&workspace) {
            if job.operation_id == operation_id && job.apply != apply {
                return Err("cache operation mode changed");
            }
            if job.operation_id == operation_id
                || matches!(job.state, CacheJobState::Queued | CacheJobState::Running)
            {
                if job.apply != apply {
                    return Err(
                        "cache job already queued or running in another mode; check --status first",
                    );
                }
                return Ok(job.clone());
            }
        }
        let job = CacheJob {
            operation_id: operation_id.to_owned(),
            apply,
            state: CacheJobState::Queued,
            result: None,
            error: None,
        };
        jobs.insert(workspace, job.clone());
        Ok(job)
    }

    /// # Errors
    /// Rejects an unavailable job owner without starting another scan.
    pub fn observe(&self, workspace: WorkspaceId) -> Result<Option<CacheJob>, &'static str> {
        Ok(self
            .0
            .lock()
            .map_err(|_| "cache job owner unavailable")?
            .get(&workspace)
            .cloned())
    }

    /// The single maintenance worker releases this short lock before any IO.
    /// # Errors
    /// Rejects an unavailable job owner.
    pub fn take(&self) -> Result<Option<(WorkspaceId, CacheJob)>, &'static str> {
        let mut jobs = self.0.lock().map_err(|_| "cache job owner unavailable")?;
        let Some((workspace, job)) = jobs
            .iter_mut()
            .find(|(_, job)| job.state == CacheJobState::Queued)
        else {
            return Ok(None);
        };
        job.state = CacheJobState::Running;
        Ok(Some((*workspace, job.clone())))
    }

    /// Records only the exact job the worker claimed. The result is observed
    /// without inspecting the cache tree again.
    /// # Errors
    /// Rejects an unavailable job owner.
    pub fn finish(
        &self,
        workspace: WorkspaceId,
        operation_id: &str,
        result: Result<serde_json::Value, String>,
    ) -> Result<bool, &'static str> {
        let mut jobs = self.0.lock().map_err(|_| "cache job owner unavailable")?;
        let Some(job) = jobs.get_mut(&workspace) else {
            return Ok(false);
        };
        if job.operation_id != operation_id || job.state != CacheJobState::Running {
            return Ok(false);
        }
        match result {
            Ok(result) => {
                job.state = CacheJobState::Completed;
                job.result = Some(result);
            }
            Err(error) => {
                job.state = CacheJobState::Failed;
                job.error = Some(error);
            }
        }
        Ok(true)
    }
}

pub trait CacheJobExecutor {
    /// # Errors
    /// Returns a failed scan or cleanup, preserved for later observation.
    fn execute(&mut self, workspace: WorkspaceId, apply: bool)
    -> Result<serde_json::Value, String>;
}

/// Runs jobs serially outside the queue lock. Admission and status remain fast
/// even while an injected scan/removal takes longer than an IPC deadline.
/// # Errors
/// Returns an unavailable queue owner, without launching additional jobs.
pub fn drain(
    jobs: &CacheJobs,
    executor: &mut impl CacheJobExecutor,
) -> Result<usize, &'static str> {
    let mut completed = 0;
    while let Some((workspace, job)) = jobs.take()? {
        let result = executor.execute(workspace, job.apply);
        jobs.finish(workspace, &job.operation_id, result)?;
        completed += 1;
    }
    Ok(completed)
}

/// Manual jobs are checked frequently without changing automatic retention's
/// five-minute cadence. The clock is injected by the composition root.
pub struct MaintenanceSchedule {
    next: u64,
    interval: u64,
}

impl MaintenanceSchedule {
    #[must_use]
    pub fn new(now: u64, interval: u64) -> Self {
        Self {
            next: now.saturating_add(interval),
            interval,
        }
    }

    pub fn due(&mut self, now: u64) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now.saturating_add(self.interval);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_coalesce_without_waiting_for_io_and_results_are_workspace_scoped() {
        let jobs = CacheJobs::default();
        let workspace = WorkspaceId::new();
        let other = WorkspaceId::new();
        assert_eq!(jobs.observe(workspace).unwrap(), None);
        assert_eq!(jobs.take().unwrap(), None);
        let queued = jobs.request(workspace, "a", false).unwrap();
        assert_eq!(queued.state, CacheJobState::Queued);
        assert_eq!(jobs.request(workspace, "b", false).unwrap(), queued);
        assert!(jobs.request(workspace, "b", true).is_err());
        assert!(jobs.request(workspace, "a", true).is_err());
        assert!(
            !jobs
                .finish(workspace, "a", Ok(serde_json::json!({})))
                .unwrap()
        );
        let (claimed, running) = jobs.take().unwrap().unwrap();
        assert_eq!(claimed, workspace);
        assert_eq!(running.state, CacheJobState::Running);
        assert_eq!(jobs.request(workspace, "b", false).unwrap(), running);
        assert_eq!(jobs.take().unwrap(), None);
        assert!(!jobs.finish(other, "a", Ok(serde_json::json!({}))).unwrap());
        assert!(
            !jobs
                .finish(workspace, "b", Ok(serde_json::json!({})))
                .unwrap()
        );
        assert!(
            jobs.finish(workspace, "a", Ok(serde_json::json!({"bytes":42})))
                .unwrap()
        );
        let completed = jobs.observe(workspace).unwrap().unwrap();
        assert_eq!(completed.state, CacheJobState::Completed);
        assert_eq!(completed.result, Some(serde_json::json!({"bytes":42})));
        assert_eq!(
            serde_json::to_value(&completed).unwrap()["state"],
            "completed"
        );
        assert_eq!(jobs.request(workspace, "a", false).unwrap(), completed);
        jobs.request(workspace, "b", true).unwrap();
        jobs.request(other, "c", false).unwrap();
        while let Some((workspace, job)) = jobs.take().unwrap() {
            assert!(
                jobs.finish(workspace, &job.operation_id, Err("retry".into()))
                    .unwrap()
            );
        }
        let failed = jobs.observe(workspace).unwrap().unwrap();
        assert_eq!(failed.state, CacheJobState::Failed);
        assert_eq!(failed.error.as_deref(), Some("retry"));
    }

    #[test]
    fn unavailable_owner_fails_closed_for_admission_observation_and_completion() {
        let jobs = CacheJobs::default();
        let owner = Arc::clone(&jobs.0);
        let _ = std::thread::spawn(move || {
            let _locked = owner.lock().unwrap();
            panic!("poison fixture");
        })
        .join();
        let workspace = WorkspaceId::new();
        assert!(jobs.request(workspace, "a", false).is_err());
        assert!(jobs.observe(workspace).is_err());
        assert!(jobs.take().is_err());
        assert!(
            jobs.finish(workspace, "a", Ok(serde_json::json!({})))
                .is_err()
        );
        assert!(jobs.reply(workspace, "a", CacheJobRequest::Status).is_err());
        assert!(
            drain(
                &jobs,
                &mut Executor {
                    jobs: &jobs,
                    poison: false
                }
            )
            .is_err()
        );
    }

    #[test]
    fn manual_polling_preserves_the_automatic_cadence_without_catchup_passes() {
        let mut schedule = MaintenanceSchedule::new(10, 300_000);
        assert!(!schedule.due(10));
        assert!(!schedule.due(1000));
        assert!(!schedule.due(300_009));
        assert!(schedule.due(300_010));
        assert!(!schedule.due(300_010));
        assert!(schedule.due(1_000_000));
        assert!(!schedule.due(1_000_000));
    }

    #[test]
    fn flags_and_observation_do_not_start_another_scan() {
        use CacheJobRequest::{Run, Status};
        assert_eq!(
            CacheJobRequest::from_payload(&serde_json::json!({})),
            Ok(Run { apply: false })
        );
        assert_eq!(
            CacheJobRequest::from_payload(&serde_json::json!({"apply":true})),
            Ok(Run { apply: true })
        );
        assert_eq!(
            CacheJobRequest::from_payload(&serde_json::json!({"status":true})),
            Ok(Status)
        );
        for payload in [
            serde_json::json!({"apply":1}),
            serde_json::json!({"status":null}),
            serde_json::json!({"apply":true,"status":true}),
        ] {
            assert!(CacheJobRequest::from_payload(&payload).is_err());
        }
        let jobs = CacheJobs::default();
        let workspace = WorkspaceId::new();
        assert_eq!(
            jobs.reply(workspace, "status", Status).unwrap()["state"],
            "idle"
        );
        assert_eq!(
            jobs.reply(workspace, "scan", Run { apply: false }).unwrap()["state"],
            "queued"
        );
        assert_eq!(
            jobs.reply(workspace, "status", Status).unwrap()["operation_id"],
            "scan"
        );
        assert!(jobs.reply(workspace, "scan", Run { apply: true }).is_err());
    }

    struct Executor<'a> {
        jobs: &'a CacheJobs,
        poison: bool,
    }

    impl CacheJobExecutor for Executor<'_> {
        fn execute(
            &mut self,
            workspace: WorkspaceId,
            apply: bool,
        ) -> Result<serde_json::Value, String> {
            assert_eq!(
                self.jobs.observe(workspace).unwrap().unwrap().state,
                CacheJobState::Running
            );
            assert_eq!(
                self.jobs
                    .request(workspace, "parallel", apply)
                    .unwrap()
                    .state,
                CacheJobState::Running
            );
            if self.poison {
                let owner = Arc::clone(&self.jobs.0);
                let _ = std::thread::spawn(move || {
                    let _locked = owner.lock().unwrap();
                    panic!("effect poisoned owner");
                })
                .join();
            }
            if apply {
                Err("removal interrupted".into())
            } else {
                Ok(serde_json::json!({"bytes":42}))
            }
        }
    }

    #[test]
    fn worker_runs_serially_without_holding_admission_or_observation_locks() {
        let jobs = CacheJobs::default();
        let scan = WorkspaceId::new();
        let cleanup = WorkspaceId::new();
        jobs.request(scan, "scan", false).unwrap();
        jobs.request(cleanup, "cleanup", true).unwrap();
        assert_eq!(
            drain(
                &jobs,
                &mut Executor {
                    jobs: &jobs,
                    poison: false
                }
            )
            .unwrap(),
            2
        );
        assert_eq!(
            jobs.observe(scan).unwrap().unwrap().result,
            Some(serde_json::json!({"bytes":42}))
        );
        assert_eq!(
            jobs.observe(cleanup).unwrap().unwrap().error.as_deref(),
            Some("removal interrupted")
        );
        jobs.request(scan, "poison", false).unwrap();
        assert!(
            drain(
                &jobs,
                &mut Executor {
                    jobs: &jobs,
                    poison: true
                }
            )
            .is_err()
        );
    }
}
