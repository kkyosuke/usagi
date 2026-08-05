//! The shutdown request every daemon worker waits on.
//!
//! Workers used to approximate "wait for the tick, but notice shutdown quickly"
//! with a 10 ms sleep loop. That made an idle daemon wake a hundred times a
//! second per worker to observe a flag that almost never changes.
//!
//! [`ShutdownRequest`] separates the two concerns: the flag is the authority, and
//! a condvar carries the *edge*. A worker parks until either its tick elapses or
//! shutdown is requested, so an idle daemon wakes once per intended tick and
//! shutdown is still observed immediately.
//!
//! The flag is a plain [`AtomicBool`] because `signal_hook::flag::register`
//! writes it straight from a signal handler. A handler cannot notify a condvar,
//! so whoever turns a delivered signal into a *request* must call
//! [`ShutdownRequest::request`] from ordinary code.

use std::{
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};

/// A shared "please stop" flag with edge notification.
#[derive(Debug, Default)]
pub struct ShutdownRequest {
    requested: Arc<AtomicBool>,
    background_worker_failures: Arc<AtomicU16>,
    // The mutex guards nothing but the notification itself: `requested` is the
    // authority. Locking it inside `request` before notifying is what orders the
    // store against a waiter's re-check, so no wakeup is lost.
    guard: Mutex<()>,
    changed: Condvar,
}

impl ShutdownRequest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps an existing flag, for a caller that must hand the same `Arc` to a
    /// signal handler registration.
    #[must_use]
    pub fn with_flag(requested: Arc<AtomicBool>) -> Self {
        Self {
            requested,
            background_worker_failures: Arc::new(AtomicU16::new(0)),
            guard: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    /// The raw flag, for registering an async-signal-safe handler against it.
    ///
    /// A handler that writes this flag does **not** wake condvar waiters; see
    /// the module docs.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.requested)
    }

    /// A lock-free view of long-lived worker failures for the metrics broker.
    #[must_use]
    pub fn background_worker_health(&self) -> BackgroundWorkerHealth {
        BackgroundWorkerHealth(Arc::clone(&self.background_worker_failures))
    }

    /// Runs one long-lived worker behind the daemon's failure detector.
    ///
    /// A panic or return not marked with
    /// [`BackgroundWorkerMonitor::finish_planned`] is retained until restart.
    /// The process panic hook remains responsible for its ordinary diagnostic.
    pub fn monitor_background_worker(&self, worker: BackgroundWorker) -> BackgroundWorkerMonitor {
        BackgroundWorkerMonitor {
            failures: Arc::clone(&self.background_worker_failures),
            worker,
            planned: false,
        }
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Requests shutdown and wakes every waiter. Idempotent.
    pub fn request(&self) {
        // Take the lock before storing so a waiter cannot evaluate the predicate
        // and start waiting in between the store and the notification.
        //
        // Poisoning is recovered from rather than branched on: this mutex guards no
        // invariant — `requested` is the authority — so a panic elsewhere must not
        // stop shutdown from being requested.
        let locked = self.guard.lock().unwrap_or_else(PoisonError::into_inner);
        self.requested.store(true, Ordering::Release);
        drop(locked);
        self.changed.notify_all();
    }

    /// Parks until `tick` elapses or shutdown is requested. Returns whether
    /// shutdown was requested, so a worker can `break` on `true`.
    ///
    /// An idle daemon wakes once per `tick`, not once per poll interval.
    pub fn wait_for_tick(&self, tick: Duration) -> bool {
        let deadline = Instant::now() + tick;
        // The predicate is evaluated while the lock is held, so a request that
        // lands between the check and the wait cannot be missed.
        let mut locked = self.guard.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_requested() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            locked = self
                .changed
                .wait_timeout(locked, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Parks until shutdown is requested.
    ///
    /// A caller that also needs to observe a flag written by a signal handler
    /// must arrange for that signal to reach [`request`](Self::request); this
    /// wait is edge-driven and does not poll.
    pub fn wait_until_requested(&self) {
        let mut locked = self.guard.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_requested() {
            locked = self
                .changed
                .wait(locked)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The fixed set of daemon workers whose unexpected exit disables a product
/// feature until restart.
///
/// This is the single authority for the health bitset and its cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundWorker {
    PrRefresh,
    SessionTeardown,
    Custody,
    RetentionGc,
    DrainingCollection,
    DecisionMaintenance,
    AgentObserver,
    TerminalObserver,
    PrProjection,
}

impl BackgroundWorker {
    pub const ALL: [Self; 9] = [
        Self::PrRefresh,
        Self::SessionTeardown,
        Self::Custody,
        Self::RetentionGc,
        Self::DrainingCollection,
        Self::DecisionMaintenance,
        Self::AgentObserver,
        Self::TerminalObserver,
        Self::PrProjection,
    ];

    pub const COUNT: usize = Self::ALL.len();

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// A worker-lifetime guard that records every exit not explicitly completed as
/// planned.
#[derive(Debug)]
pub struct BackgroundWorkerMonitor {
    failures: Arc<AtomicU16>,
    worker: BackgroundWorker,
    planned: bool,
}

impl BackgroundWorkerMonitor {
    /// Marks the guarded worker's return as part of planned daemon shutdown.
    pub fn finish_planned(mut self) {
        self.planned = true;
    }
}

impl Drop for BackgroundWorkerMonitor {
    fn drop(&mut self) {
        if !self.planned {
            self.failures.fetch_or(self.worker.bit(), Ordering::Release);
        }
    }
}

/// Cloneable, lock-free failure gauge consumed by display-only metrics.
#[derive(Debug, Clone, Default)]
pub struct BackgroundWorkerHealth(Arc<AtomicU16>);

impl BackgroundWorkerHealth {
    /// Number of distinct daemon workers that exited unexpectedly in this process.
    #[must_use]
    pub fn failed_count(&self) -> u8 {
        u8::try_from(self.0.load(Ordering::Acquire).count_ones()).unwrap_or(u8::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn a_tick_elapses_without_shutdown_and_reports_no_request() {
        let shutdown = ShutdownRequest::new();
        assert!(!shutdown.wait_for_tick(Duration::from_millis(1)));
        assert!(!shutdown.is_requested());
    }

    #[test]
    fn an_already_requested_shutdown_returns_without_waiting_out_the_tick() {
        let shutdown = ShutdownRequest::new();
        shutdown.request();
        let start = Instant::now();
        // A 30 s tick must not delay a worker that is already asked to stop.
        assert!(shutdown.wait_for_tick(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_request_wakes_a_parked_tick_wait_rather_than_letting_it_time_out() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let waiter = Arc::clone(&shutdown);
        let start = Instant::now();
        let handle = std::thread::spawn(move || waiter.wait_for_tick(Duration::from_secs(30)));
        // Park first, then request: the wakeup must come from the notification.
        while !handle.is_finished() {
            shutdown.request();
        }
        assert!(handle.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn a_request_wakes_an_unbounded_wait() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let waiter = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || waiter.wait_until_requested());
        while !handle.is_finished() {
            shutdown.request();
        }
        handle.join().unwrap();
        assert!(shutdown.is_requested());
    }

    #[test]
    fn an_already_requested_shutdown_returns_from_an_unbounded_wait() {
        let shutdown = ShutdownRequest::new();
        shutdown.request();
        shutdown.wait_until_requested();
        assert!(shutdown.is_requested());
    }

    #[test]
    fn a_handler_written_flag_is_observed_by_the_predicate() {
        // `signal_hook::flag::register` writes the shared flag directly. It
        // cannot notify, so a bounded wait observes it on its next tick.
        let flag = Arc::new(AtomicBool::new(false));
        let shutdown = ShutdownRequest::with_flag(Arc::clone(&flag));
        assert!(!shutdown.flag().load(Ordering::Acquire));
        flag.store(true, Ordering::Release);
        assert!(shutdown.is_requested());
        assert!(shutdown.wait_for_tick(Duration::from_secs(30)));
    }

    #[test]
    fn every_background_worker_failure_is_retained_once() {
        let shutdown = ShutdownRequest::new();
        for worker in BackgroundWorker::ALL {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _monitor = shutdown.monitor_background_worker(worker);
                panic!("injected");
            }));
            assert!(result.is_err());
        }
        assert_eq!(
            shutdown.background_worker_health().failed_count(),
            u8::try_from(BackgroundWorker::COUNT).unwrap()
        );

        let duplicate = catch_unwind(AssertUnwindSafe(|| {
            let _monitor = shutdown.monitor_background_worker(BackgroundWorker::Custody);
            panic!("again");
        }));
        assert!(duplicate.is_err());
        assert_eq!(
            shutdown.background_worker_health().failed_count(),
            u8::try_from(BackgroundWorker::COUNT).unwrap()
        );
    }

    #[test]
    fn planned_worker_completion_is_not_a_failure() {
        let shutdown = ShutdownRequest::new();
        shutdown
            .monitor_background_worker(BackgroundWorker::AgentObserver)
            .finish_planned();
        assert_eq!(shutdown.background_worker_health().failed_count(), 0);
    }
}
