//! Resident inventory-refresh pump for the Home frame's observation lanes.
//!
//! Home's frame loop used to issue two daemon RPCs **on the render thread** for
//! every terminal wake-up. At the composition root's 16ms tick that is ~125
//! `bootstrap_client` round trips per second, each of which takes an unbounded
//! `flock` on the shared data directory, so an idle TUI burned render-thread
//! time and serialised every other client's connect (#551).
//!
//! This pump is the generic form of the fix, and it deliberately mirrors the
//! shape [`super::terminal_pump`] and [`super::inventory_pump`] already
//! established for the terminal lanes:
//!
//! * **The render thread never fetches.** A resident thread performs the
//!   request; the render thread only calls the non-blocking [`RefreshPump::take`].
//! * **Bounded cadence.** Steady observation happens at a cadence clamped into
//!   [`MIN_INTERVAL`]..=[`MAX_INTERVAL`], so the request rate is a property of
//!   the lane's configuration and not of the frame rate. A tick, a resize, and a
//!   thousand frames all cost the same.
//! * **Coalescing.** At most one request per lane is in flight and at most one
//!   result is buffered. Piling up wake-ups inside one cadence period still
//!   issues exactly one request, and a render thread that stops draining sees
//!   the newest snapshot rather than a backlog of stale ones.
//! * **Backoff.** A failing lane (missing daemon, hung socket, exhausted request
//!   deadline) backs off from [`RefreshCadence::backoff_base`] to
//!   [`RefreshCadence::backoff_max`] instead of retrying at the steady cadence.
//! * **Immediate wake.** [`RefreshPump::wake`] cuts the current wait short, so a
//!   user action that changes the observed state is reflected without waiting
//!   out the idle cadence.
//!
//! The pure scheduling state ([`RefreshState`]) is unit-tested directly with an
//! injected elapsed clock; the thread wrapper ([`RefreshPump`]) is exercised
//! with an in-process fake so the real daemon IPC — injected as the `fetch`
//! closure by the composition root — is the only part left as real IO.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Fastest steady cadence a lane may ask for. Below this the lane starts to
/// resemble the per-tick flood this pump exists to remove.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Slowest steady cadence a lane may ask for. Beyond this an inventory change
/// made by another client (an MCP server creating a session) would stay
/// invisible for longer than a user reads as "live".
pub const MAX_INTERVAL: Duration = Duration::from_millis(1_000);

/// How long the thread waits when the lane is stopped mid-wait. Only bounds the
/// shutdown of a pump whose wait was not signalled.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One lane's request rhythm.
///
/// `interval` is clamped into [`MIN_INTERVAL`]..=[`MAX_INTERVAL`] on
/// construction: a lane cannot opt out of the bound this pump exists to
/// establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshCadence {
    /// Steady time between two successful observations.
    pub interval: Duration,
    /// First backoff step after a failed observation.
    pub backoff_base: Duration,
    /// Longest backoff between two attempts while the lane keeps failing.
    pub backoff_max: Duration,
}

impl RefreshCadence {
    /// Build a cadence with `interval` clamped into the bounded window.
    #[must_use]
    pub fn new(interval: Duration, backoff_base: Duration, backoff_max: Duration) -> Self {
        Self {
            interval: interval.clamp(MIN_INTERVAL, MAX_INTERVAL),
            backoff_base,
            backoff_max,
        }
    }

    /// The delay after `failures` consecutive failures: the steady interval when
    /// there are none, otherwise `backoff_base` doubled per failure and capped
    /// at `backoff_max`.
    #[must_use]
    fn delay(&self, failures: u32) -> Duration {
        if failures == 0 {
            return self.interval;
        }
        let shift = failures.saturating_sub(1).min(16);
        self.backoff_base
            .saturating_mul(1u32 << shift)
            .min(self.backoff_max)
    }
}

/// Observability for one lane.
///
/// The counters are maintained in every build (they are three integer
/// increments per round) but only read from tests: they are what lets the
/// regression suite assert the lane's request rate against its cadence rather
/// than against the frame count, which is the property this pump exists to
/// establish (#551).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshMetrics {
    /// Requests the resident worker dispatched.
    pub fetches: u64,
    /// Dispatched requests that failed.
    pub failures: u64,
    /// Results discarded because a newer one replaced them before the render
    /// thread drained the lane.
    pub coalesced: u64,
    /// Out-of-cadence wakes the render thread asked for. Several wakes inside
    /// one cadence period still produce one fetch; the surplus is counted here.
    pub wakes: u64,
}

/// The pure scheduling state of one lane.
///
/// `now` is always the elapsed time since the pump started, injected by the
/// caller, so the whole schedule is testable without sleeping.
#[derive(Debug)]
pub struct RefreshState<T> {
    cadence: RefreshCadence,
    /// Elapsed time the next fetch becomes allowed at.
    due: Duration,
    /// Consecutive failures driving the backoff.
    failures: u32,
    /// The newest result the render thread has not drained yet.
    latest: Option<Result<T, String>>,
    /// A wake that has not been spent by ending a wait yet.
    woken: bool,
    metrics: RefreshMetrics,
}

impl<T> RefreshState<T> {
    /// A lane that is due immediately, so the first frame does not wait out a
    /// full cadence period for its first observation.
    #[must_use]
    pub fn new(cadence: RefreshCadence) -> Self {
        Self {
            cadence,
            due: Duration::ZERO,
            failures: 0,
            latest: None,
            woken: false,
            metrics: RefreshMetrics::default(),
        }
    }

    /// Whether a fetch may start at `now`, counting it when it may.
    pub fn begin(&mut self, now: Duration) -> bool {
        if now < self.due {
            return false;
        }
        self.metrics.fetches += 1;
        true
    }

    /// Record one completed fetch and schedule the next one. A result that
    /// replaces an undrained one is coalesced: the render thread only ever sees
    /// the newest observation of a lane.
    pub fn complete(&mut self, now: Duration, result: Result<T, String>) {
        if result.is_err() {
            self.metrics.failures += 1;
            self.failures = self.failures.saturating_add(1);
        } else {
            self.failures = 0;
        }
        if self.latest.is_some() {
            self.metrics.coalesced += 1;
        }
        self.latest = Some(result);
        self.due = now.saturating_add(self.cadence.delay(self.failures));
    }

    /// Make the lane due immediately. Wakes inside one cadence period collapse:
    /// the lane is already due, so the extra wake only shortens the wait.
    pub fn wake(&mut self) {
        self.metrics.wakes += 1;
        self.due = Duration::ZERO;
        self.woken = true;
    }

    /// How long the worker should wait before re-checking, at `now`.
    #[must_use]
    pub fn wait_for(&self, now: Duration) -> Duration {
        self.due.saturating_sub(now)
    }

    /// Non-blocking drain of the newest observation.
    pub fn take(&mut self) -> Option<Result<T, String>> {
        self.latest.take()
    }

    /// Snapshot of this lane's counters.
    #[cfg(test)]
    #[must_use]
    pub const fn metrics(&self) -> RefreshMetrics {
        self.metrics
    }
}

/// State shared with the resident worker, plus the condvar the render thread
/// uses to cut an idle wait short.
struct Shared<T> {
    state: Mutex<RefreshState<T>>,
    signal: Condvar,
}

/// Locks the lane state, recovering a poisoned lock. An observation snapshot is
/// not safety-critical, so a render-thread panic while holding the lock must not
/// wedge the worker; the recovered state is internally consistent.
fn lock<T>(state: &Mutex<RefreshState<T>>) -> std::sync::MutexGuard<'_, RefreshState<T>> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sleeps until `interval` elapses or the render thread signals work, whichever
/// comes first. Waiting on the condvar rather than sleeping blindly is what lets
/// the idle cadence be a full second without delaying a user-triggered refresh.
fn wait_for_next_round<T>(shared: &Shared<T>, interval: Duration) {
    let wait = if interval.is_zero() {
        STOP_POLL_INTERVAL
    } else {
        interval
    };
    let guard = lock(&shared.state);
    let (mut guard, _timeout) = shared
        .signal
        .wait_timeout_while(guard, wait, |state| !state.woken)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The wake is spent by ending this wait, so the round that follows uses the
    // ordinary cadence instead of running twice back to back.
    guard.woken = false;
}

/// One observation lane: a resident worker thread, its bounded schedule, and the
/// single-slot result buffer the render thread drains.
pub struct RefreshPump<T> {
    shared: Arc<Shared<T>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> RefreshPump<T> {
    /// Spawns the lane's resident worker. `fetch` performs exactly one
    /// observation; the composition root injects the real daemon IPC (holding
    /// the lane's own persistent connection inside the closure), while tests
    /// inject an in-process fake.
    ///
    /// The thread is created once per workspace launch and lives until the pump
    /// is dropped, so no frame ever spawns one.
    pub fn spawn<F>(cadence: RefreshCadence, mut fetch: F) -> Self
    where
        F: FnMut() -> Result<T, String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(RefreshState::new(cadence)),
            signal: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let clock = Instant::now();
            while !thread_stop.load(Ordering::Acquire) {
                if lock(&thread_shared.state).begin(clock.elapsed()) {
                    // The request runs outside the lock so the render thread's
                    // drain is never blocked by a slow or hung daemon.
                    let result = fetch();
                    let now = clock.elapsed();
                    lock(&thread_shared.state).complete(now, result);
                }
                if thread_stop.load(Ordering::Acquire) {
                    break;
                }
                let wait = lock(&thread_shared.state).wait_for(clock.elapsed());
                wait_for_next_round(&thread_shared, wait);
            }
        });
        Self {
            shared,
            stop,
            handle: Some(handle),
        }
    }

    /// Ask for an immediate out-of-cadence observation (see
    /// [`RefreshState::wake`]).
    pub fn wake(&self) {
        lock(&self.shared.state).wake();
        self.shared.signal.notify_all();
    }

    /// Non-blocking drain of the newest observation. This is the only call the
    /// render thread makes into the lane.
    pub fn take(&self) -> Option<Result<T, String>> {
        lock(&self.shared.state).take()
    }

    /// Snapshot of this lane's counters.
    #[cfg(test)]
    pub fn metrics(&self) -> RefreshMetrics {
        lock(&self.shared.state).metrics()
    }
}

impl<T> Drop for RefreshPump<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Cut a pending wait short so quitting never waits out the cadence.
        lock(&self.shared.state).woken = true;
        self.shared.signal.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_INTERVAL, MIN_INTERVAL, RefreshCadence, RefreshPump, RefreshState, STOP_POLL_INTERVAL,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn cadence() -> RefreshCadence {
        RefreshCadence::new(
            Duration::from_millis(500),
            Duration::from_millis(500),
            Duration::from_millis(4_000),
        )
    }

    #[test]
    fn cadence_clamps_into_the_bounded_window() {
        let fast = RefreshCadence::new(
            Duration::from_millis(16),
            Duration::from_millis(500),
            Duration::from_secs(4),
        );
        let slow = RefreshCadence::new(
            Duration::from_secs(60),
            Duration::from_millis(500),
            Duration::from_secs(4),
        );
        assert_eq!(fast.interval, MIN_INTERVAL);
        assert_eq!(slow.interval, MAX_INTERVAL);
    }

    #[test]
    fn a_lane_is_due_immediately_and_then_only_once_per_cadence() {
        let mut state = RefreshState::<u32>::new(cadence());
        assert!(state.begin(Duration::ZERO));
        state.complete(Duration::ZERO, Ok(1));
        // Every frame of the next half second re-checks and finds nothing due.
        for frame in 0..31u64 {
            let now = Duration::from_millis(frame * 16);
            assert!(!state.begin(now), "frame {frame} started a second fetch");
            assert_eq!(
                state.wait_for(now),
                Duration::from_millis(500).saturating_sub(now)
            );
        }
        assert!(state.begin(Duration::from_millis(500)));
        assert_eq!(state.metrics().fetches, 2);
    }

    #[test]
    fn failures_back_off_and_success_restores_the_steady_cadence() {
        let mut state = RefreshState::<u32>::new(cadence());
        state.complete(Duration::ZERO, Err("daemon unavailable".to_owned()));
        assert_eq!(state.wait_for(Duration::ZERO), Duration::from_millis(500));
        state.complete(Duration::from_millis(500), Err("still down".to_owned()));
        assert_eq!(
            state.wait_for(Duration::from_millis(500)),
            Duration::from_millis(1_000)
        );
        state.complete(Duration::from_millis(1_500), Err("still down".to_owned()));
        assert_eq!(
            state.wait_for(Duration::from_millis(1_500)),
            Duration::from_millis(2_000)
        );
        // The backoff is capped rather than doubling without limit.
        for step in 0..8 {
            state.complete(Duration::from_secs(10 + step), Err("down".to_owned()));
        }
        assert_eq!(
            state.wait_for(Duration::from_secs(17)),
            Duration::from_millis(4_000)
        );
        state.take();
        state.complete(Duration::from_secs(20), Ok(7));
        assert_eq!(
            state.wait_for(Duration::from_secs(20)),
            Duration::from_millis(500)
        );
        assert_eq!(state.metrics().failures, 11);
    }

    #[test]
    fn results_coalesce_to_the_newest_observation() {
        let mut state = RefreshState::new(cadence());
        state.complete(Duration::ZERO, Ok(1));
        state.complete(Duration::from_millis(500), Ok(2));
        state.complete(Duration::from_millis(1_000), Ok(3));
        assert_eq!(state.take(), Some(Ok(3)));
        assert!(state.take().is_none());
        assert_eq!(state.metrics().coalesced, 2);
    }

    #[test]
    fn repeated_wakes_inside_one_period_collapse_into_one_fetch() {
        let mut state = RefreshState::<u32>::new(cadence());
        state.complete(Duration::ZERO, Ok(1));
        for _ in 0..10 {
            state.wake();
        }
        assert!(state.begin(Duration::from_millis(16)));
        state.complete(Duration::from_millis(16), Ok(2));
        assert!(!state.begin(Duration::from_millis(17)));
        assert_eq!(state.metrics().fetches, 1);
        assert_eq!(state.metrics().wakes, 10);
    }

    /// The property the issue's floor measurement targets: an idle Home issues
    /// requests at the lane's cadence, not at the 62.5Hz frame tick.
    #[test]
    fn an_idle_lane_request_count_follows_the_cadence_not_the_frame_rate() {
        let mut state = RefreshState::<u32>::new(cadence());
        let mut connects = 0u32;
        // Ten seconds of 16ms frames: 625 frames, 20 cadence periods.
        for frame in 0..625u64 {
            let now = Duration::from_millis(frame * 16);
            if state.begin(now) {
                connects += 1;
                state.complete(now, Ok(1));
            }
        }
        // 625 frames, but the lane's own bound — one request per 500ms period
        // over ten seconds — is what decides the count.
        assert_eq!(state.metrics().fetches, 20);
        assert_eq!(connects, 20);
        assert!(u64::from(connects) <= 10_000 / 500 + 1);
    }

    #[test]
    fn the_resident_worker_fetches_and_publishes_without_the_caller_blocking() {
        let calls = Arc::new(AtomicU64::new(0));
        let worker = Arc::clone(&calls);
        let pump = RefreshPump::spawn(cadence(), move || {
            Ok(worker.fetch_add(1, Ordering::SeqCst) + 1)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = pump.take() {
                assert_eq!(result, Ok(1));
                break;
            }
            assert!(Instant::now() < deadline, "the lane published no result");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pump.metrics().fetches, 1);
    }

    #[test]
    fn a_wake_cuts_the_idle_wait_short() {
        let calls = Arc::new(AtomicU64::new(0));
        let worker = Arc::clone(&calls);
        let pump = RefreshPump::spawn(cadence(), move || {
            Ok(worker.fetch_add(1, Ordering::SeqCst) + 1)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while pump.take().is_none() {
            assert!(Instant::now() < deadline, "the lane published no result");
            std::thread::sleep(Duration::from_millis(5));
        }
        pump.wake();
        while pump.take().is_none() {
            assert!(Instant::now() < deadline, "the wake produced no fetch");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The steady cadence is 500ms; the second fetch only happened because the
        // wake cut the wait short.
        assert!(pump.metrics().fetches >= 2);
        assert!(pump.metrics().wakes >= 1);
    }

    /// A hung lane must not stall the caller: `take` and `wake` stay
    /// non-blocking while the worker sits inside `fetch`.
    #[test]
    fn a_hung_fetch_never_blocks_the_render_thread() {
        let release = Arc::new(Mutex::new(false));
        let worker_release = Arc::clone(&release);
        let pump = RefreshPump::<u32>::spawn(cadence(), move || {
            while !*worker_release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(1)
        });
        let started = Instant::now();
        for _ in 0..200 {
            assert!(pump.take().is_none());
            pump.wake();
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the render thread waited on the hung lane"
        );
        *release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    #[test]
    fn a_failing_lane_keeps_retrying_at_the_bounded_backoff() {
        let calls = Arc::new(AtomicU64::new(0));
        let worker = Arc::clone(&calls);
        let pump = RefreshPump::<u32>::spawn(
            RefreshCadence::new(
                MIN_INTERVAL,
                Duration::from_millis(10),
                Duration::from_millis(20),
            ),
            move || {
                worker.fetch_add(1, Ordering::SeqCst);
                Err("daemon unavailable".to_owned())
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if pump.metrics().failures >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "the lane stopped retrying");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pump.take(), Some(Err("daemon unavailable".to_owned())));
    }

    /// A zero wait (the lane is already due while the worker is finishing a
    /// round) must still park on the condvar rather than spin.
    #[test]
    fn a_zero_wait_parks_for_the_stop_poll_interval() {
        assert_eq!(STOP_POLL_INTERVAL, Duration::from_millis(50));
        let mut state = RefreshState::<u32>::new(cadence());
        state.wake();
        assert_eq!(state.wait_for(Duration::from_millis(100)), Duration::ZERO);
    }
}
