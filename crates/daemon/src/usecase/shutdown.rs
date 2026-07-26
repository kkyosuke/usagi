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
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A shared "please stop" flag with edge notification.
#[derive(Debug, Default)]
pub struct ShutdownRequest {
    requested: Arc<AtomicBool>,
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

    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Requests shutdown and wakes every waiter. Idempotent.
    pub fn request(&self) {
        // Take the lock before storing so a waiter cannot evaluate the predicate
        // and start waiting in between the store and the notification.
        let locked = self.guard.lock();
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
        let Ok(mut locked) = self.guard.lock() else {
            return true;
        };
        while !self.is_requested() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let Ok((next, _)) = self.changed.wait_timeout(locked, remaining) else {
                return true;
            };
            locked = next;
        }
        true
    }

    /// Parks until shutdown is requested.
    ///
    /// A caller that also needs to observe a flag written by a signal handler
    /// must arrange for that signal to reach [`request`](Self::request); this
    /// wait is edge-driven and does not poll.
    pub fn wait_until_requested(&self) {
        let Ok(mut locked) = self.guard.lock() else {
            return;
        };
        while !self.is_requested() {
            let Ok(next) = self.changed.wait(locked) else {
                return;
            };
            locked = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(shutdown.flag().load(Ordering::Acquire), false);
        flag.store(true, Ordering::Release);
        assert!(shutdown.is_requested());
        assert!(shutdown.wait_for_tick(Duration::from_secs(30)));
    }
}
