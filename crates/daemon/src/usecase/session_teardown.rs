//! Daemon-owned session teardown: the effect side of `session remove`.
//!
//! Removing a session worktree is unbounded work — a session that ran a
//! coverage build holds several GB under `target/` — so it must not run inside
//! the IPC connection that requested it: every client attempt deadline would
//! expire first. The daemon admits the removal durably (`Deleting` plus a
//! `DeletePlan`) and answers immediately; this worker owns the worktree effect
//! from that point on and finalizes the durable outcome afterwards.
//!
//! The pending set is **derived from durable state** instead of being kept in a
//! separate queue: a record whose lifecycle is `Deleting` and which carries a
//! delete plan *is*, by definition, an unfinished teardown. There is therefore
//! no second source of truth to drift, and a daemon that died mid-teardown
//! resumes simply by reading its own state again on the next start.
//!
//! One worker drains serially, so N concurrent removals never saturate the
//! filesystem, and the queue depth is naturally bounded by the session count.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use usagi_core::domain::id::{OperationId, SessionId};

/// One unfinished teardown, derived from the durable lifecycle record.
///
/// It carries the stable session identity and the operation that admitted the
/// removal, so the durable outcome is fenced against a record that a later
/// attempt has already replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTeardown {
    pub session_id: SessionId,
    pub operation_id: OperationId,
    pub name: String,
    pub repository_root: PathBuf,
    pub data_home: PathBuf,
    pub session_container: PathBuf,
    pub session_root: PathBuf,
    pub force: bool,
    /// Whether the session branch is deleted after the worktree, taken from the
    /// durable delete plan so a resumed teardown undoes exactly as much as the
    /// admission promised.
    pub delete_branch: bool,
    /// Whether branch deletion may discard unmerged commits. This is reserved
    /// for daemon-owned compensation; requested deletion remains safe.
    pub force_delete_branch: bool,
}

/// The durable side of a teardown: which teardowns are unfinished, and how one
/// is finalized.
///
/// Both methods are expected to take the shared session lock only briefly, so
/// concurrent readers (session list, terminal poll, user-decision list) keep
/// making progress while a removal runs.
pub trait TeardownJournal {
    /// Every unfinished teardown, in durable record order.
    fn pending(&self) -> Vec<PendingTeardown>;
    /// Records the teardown outcome durably: completion removes the record,
    /// failure leaves a diagnosable `Failed` row that still owns the name.
    ///
    /// # Errors
    ///
    /// Returns a safe message when the outcome could not be persisted at all.
    /// The record then stays `Deleting`, so the teardown is retried; a *recorded*
    /// teardown failure is a success here, not an error.
    fn finish(&self, teardown: &PendingTeardown, outcome: Result<(), String>)
    -> Result<(), String>;
}

/// The worktree effect.
///
/// It must be idempotent: a resumed teardown re-runs it over a tree that a
/// previous attempt already partially removed.
pub trait TeardownEffect {
    /// # Errors
    ///
    /// Returns a safe message describing why the worktree could not be removed.
    fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String>;
}

/// What one drained teardown did. The composition root owns the error log, so
/// the worker reports the diagnosis instead of logging it here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownReport {
    pub name: String,
    /// The safe effect error when the worktree could not be removed.
    pub effect_error: Option<String>,
    /// The durable error when the outcome itself could not be recorded. The
    /// record then stays `Deleting`, so the next drain retries it.
    pub finalize_error: Option<String>,
}

/// Drains the teardowns that are pending right now, one at a time.
///
/// The pending set is re-derived on every call, which is what makes crash
/// resume free: a `Deleting` record left behind by a dead daemon is simply
/// pending again. `cancelled` is consulted before each teardown so a shutdown
/// stops taking new work; a teardown whose effect already ran is always
/// finalized, because dropping it would leave a removed tree recorded as
/// unfinished.
pub fn drain_pending_teardowns(
    journal: &dyn TeardownJournal,
    effect: &dyn TeardownEffect,
    cancelled: &dyn Fn() -> bool,
) -> Vec<TeardownReport> {
    let mut reports = Vec::new();
    for teardown in journal.pending() {
        if cancelled() {
            break;
        }
        let outcome = effect.tear_down(&teardown);
        let effect_error = outcome.as_ref().err().cloned();
        let finalize_error = journal.finish(&teardown, outcome).err();
        reports.push(TeardownReport {
            name: teardown.name,
            effect_error,
            finalize_error,
        });
    }
    reports
}

/// Wakes the teardown worker as soon as a removal is admitted.
///
/// The notification is latched until the worker consumes it, so an admission
/// racing with the worker's wait cannot be missed. The worker also derives the
/// pending set once on startup to resume work admitted by a previous daemon.
#[derive(Debug, Default)]
pub struct TeardownSignal {
    woken: Mutex<bool>,
    admitted: Condvar,
}

impl TeardownSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks work available and wakes the worker if it is waiting.
    pub fn notify(&self) {
        if let Ok(mut woken) = self.woken.lock() {
            *woken = true;
        }
        self.admitted.notify_all();
    }

    /// Waits up to `timeout` for a notification, consuming it. Returns whether
    /// a notification was consumed, so a caller can distinguish an admitted
    /// removal from the periodic tick.
    pub fn wait(&self, timeout: Duration) -> bool {
        let mut notified = false;
        if let Ok(woken) = self.woken.lock()
            && let Ok((mut woken, _)) = self
                .admitted
                .wait_timeout_while(woken, timeout, |woken| !*woken)
        {
            notified = std::mem::replace(&mut *woken, false);
        }
        notified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn pending(name: &str) -> PendingTeardown {
        PendingTeardown {
            session_id: SessionId::new(),
            operation_id: OperationId::new(),
            name: name.to_owned(),
            repository_root: PathBuf::from("/repo"),
            data_home: PathBuf::from("/data"),
            session_container: PathBuf::from("/repo/.usagi/sessions"),
            session_root: PathBuf::from("/repo/.usagi/sessions").join(name),
            force: false,
            delete_branch: false,
            force_delete_branch: false,
        }
    }

    #[derive(Default)]
    struct FakeJournal {
        pending: Mutex<Vec<PendingTeardown>>,
        finished: Mutex<Vec<(String, Result<(), String>)>>,
        finalize_error: Option<String>,
    }
    impl TeardownJournal for FakeJournal {
        fn pending(&self) -> Vec<PendingTeardown> {
            self.pending.lock().unwrap().clone()
        }
        fn finish(
            &self,
            teardown: &PendingTeardown,
            outcome: Result<(), String>,
        ) -> Result<(), String> {
            self.finished
                .lock()
                .unwrap()
                .push((teardown.name.clone(), outcome));
            // A durable failure keeps the record `Deleting`, so the fake also
            // keeps it pending: the next drain must retry it.
            if let Some(error) = &self.finalize_error {
                return Err(error.clone());
            }
            self.pending
                .lock()
                .unwrap()
                .retain(|candidate| candidate.name != teardown.name);
            Ok(())
        }
    }

    struct FakeEffect {
        calls: Arc<Mutex<Vec<String>>>,
        error: Option<String>,
    }
    impl TeardownEffect for FakeEffect {
        fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String> {
            self.calls.lock().unwrap().push(teardown.name.clone());
            self.error.clone().map_or(Ok(()), Err)
        }
    }

    #[test]
    fn drains_every_pending_teardown_once_and_finalizes_each_completion() {
        let journal = FakeJournal {
            pending: Mutex::new(vec![pending("one"), pending("two")]),
            ..FakeJournal::default()
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let effect = FakeEffect {
            calls: Arc::clone(&calls),
            error: None,
        };

        let reports = drain_pending_teardowns(&journal, &effect, &|| false);

        assert_eq!(calls.lock().unwrap().as_slice(), ["one", "two"]);
        assert_eq!(
            reports,
            vec![
                TeardownReport {
                    name: "one".into(),
                    effect_error: None,
                    finalize_error: None,
                },
                TeardownReport {
                    name: "two".into(),
                    effect_error: None,
                    finalize_error: None,
                },
            ]
        );
        assert!(journal.pending().is_empty());
        // A second drain re-derives the (now empty) pending set: a completed
        // teardown is never re-run.
        assert!(drain_pending_teardowns(&journal, &effect, &|| false).is_empty());
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn reports_the_effect_failure_and_still_finalizes_it_durably() {
        let journal = FakeJournal {
            pending: Mutex::new(vec![pending("one")]),
            ..FakeJournal::default()
        };
        let effect = FakeEffect {
            calls: Arc::new(Mutex::new(Vec::new())),
            error: Some("worktree is busy".into()),
        };

        let reports = drain_pending_teardowns(&journal, &effect, &|| false);

        assert_eq!(reports[0].effect_error.as_deref(), Some("worktree is busy"));
        assert_eq!(reports[0].finalize_error, None);
        assert_eq!(
            journal.finished.lock().unwrap()[0],
            ("one".to_owned(), Err("worktree is busy".to_owned()))
        );
    }

    #[test]
    fn a_durable_finalization_failure_is_reported_and_retried_on_the_next_drain() {
        let journal = FakeJournal {
            pending: Mutex::new(vec![pending("one")]),
            finalize_error: Some("session owner is unavailable".into()),
            ..FakeJournal::default()
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let effect = FakeEffect {
            calls: Arc::clone(&calls),
            error: None,
        };

        let reports = drain_pending_teardowns(&journal, &effect, &|| false);
        assert_eq!(
            reports[0].finalize_error.as_deref(),
            Some("session owner is unavailable")
        );

        // The record is still `Deleting`, so the next drain re-runs the
        // idempotent effect rather than abandoning the removal.
        drain_pending_teardowns(&journal, &effect, &|| false);
        assert_eq!(calls.lock().unwrap().as_slice(), ["one", "one"]);
    }

    #[test]
    fn cancellation_stops_taking_new_teardowns() {
        let journal = FakeJournal {
            pending: Mutex::new(vec![pending("one"), pending("two")]),
            ..FakeJournal::default()
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let effect = FakeEffect {
            calls: Arc::clone(&calls),
            error: None,
        };
        let cancelled = AtomicBool::new(false);

        let reports = drain_pending_teardowns(&journal, &effect, &|| {
            // Cancel only after the first teardown has been taken.
            cancelled.swap(true, Ordering::SeqCst)
        });

        assert_eq!(calls.lock().unwrap().as_slice(), ["one"]);
        assert_eq!(reports.len(), 1);
        assert_eq!(journal.pending().len(), 1);
    }

    #[test]
    fn the_signal_consumes_one_admission_and_otherwise_returns_on_the_tick() {
        let signal = TeardownSignal::new();

        assert!(!signal.wait(Duration::from_millis(1)));
        signal.notify();
        assert!(signal.wait(Duration::from_secs(30)));
        assert!(!signal.wait(Duration::from_millis(1)));
    }

    #[test]
    fn the_signal_wakes_a_waiting_worker_from_another_thread() {
        let signal = Arc::new(TeardownSignal::new());
        let waiter = Arc::clone(&signal);
        let worker = std::thread::spawn(move || waiter.wait(Duration::from_secs(30)));

        // The admission either latches before the worker sleeps or wakes it
        // through the condvar; both orderings observe the same notification.
        std::thread::sleep(Duration::from_millis(10));
        signal.notify();

        assert!(worker.join().unwrap());
    }
}
