//! Reporting a daemon-wide lock that stays unavailable, and client capacity
//! that is running out.
//!
//! The daemon has stopped serving because one worker parked while holding a
//! shared runtime lock. Nothing said so: every later client worker queued
//! behind the lock until client capacity ran out, and the only symptom was
//! "daemon endpoint is unavailable" on the client side. Finding the cause took
//! a process sample.
//!
//! A probe thread acquires each shared lock once per tick and publishes when
//! the current acquisition started. A separate watch thread reads those
//! timestamps and reports an acquisition that has waited past the threshold,
//! then reports again once the lock becomes available. The probe itself parks
//! on a deadlocked lock, which is why the watch runs on its own thread.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::authority::workers::ClientWorkers;
use super::shutdown::ShutdownRequest;

/// One shared lock the probe can acquire.
pub trait ProbeTarget: Send {
    /// Acquires and releases the lock. Returns `false` once the lock's owner
    /// is gone, so the probe stops instead of keeping the owner alive.
    fn acquire(&self) -> bool;
}

/// A lock owned through a weak reference. A probe must not keep a runtime
/// alive past the shutdown that drops its last strong reference.
impl<T: Send> ProbeTarget for Weak<Mutex<T>> {
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=a_weak_lock_is_acquired_while_its_owner_lives_and_reports_when_it_is_gone
    fn acquire(&self) -> bool {
        let Some(lock) = self.upgrade() else {
            return false;
        };
        // A poisoned lock is still available; only waiting is reported.
        drop(lock.lock().unwrap_or_else(PoisonError::into_inner));
        true
    }
}

/// A named shared lock to watch.
pub struct LockTarget {
    name: &'static str,
    target: Box<dyn ProbeTarget>,
}

impl LockTarget {
    #[must_use]
    pub fn new(name: &'static str, target: Box<dyn ProbeTarget>) -> Self {
        Self { name, target }
    }
}

/// When the current acquisition of one lock started, if one is in progress.
#[derive(Debug)]
pub struct LockProbe {
    name: &'static str,
    waiting_since: Mutex<Option<Instant>>,
}

impl LockProbe {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            waiting_since: Mutex::new(None),
        }
    }

    /// Runs one acquisition, publishing its start until it returns.
    pub fn measure(&self, target: &dyn ProbeTarget) -> bool {
        self.publish(Some(Instant::now()));
        let acquired = target.acquire();
        self.publish(None);
        acquired
    }

    fn publish(&self, waiting_since: Option<Instant>) {
        *self
            .waiting_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = waiting_since;
    }

    fn waiting_since(&self) -> Option<Instant> {
        *self
            .waiting_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A change in whether a watched lock is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockWatchEvent {
    /// One acquisition has waited at least the threshold.
    Stalled {
        lock: &'static str,
        waited: Duration,
    },
    /// The stalled acquisition finished.
    Recovered {
        lock: &'static str,
        stalled_for: Duration,
    },
}

impl fmt::Display for LockWatchEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stalled { lock, waited } => write!(
                formatter,
                "daemon lock `{lock}` has not been available for {}s; requests that need it are parked and the daemon may be deadlocked",
                waited.as_secs()
            ),
            Self::Recovered { lock, stalled_for } => write!(
                formatter,
                "daemon lock `{lock}` became available again after about {}s",
                stalled_for.as_secs()
            ),
        }
    }
}

/// Turns probe timestamps into one report per stall and one per recovery.
#[derive(Debug)]
pub struct LockWatch {
    threshold: Duration,
    watched: Vec<Watched>,
}

#[derive(Debug)]
struct Watched {
    probe: Arc<LockProbe>,
    reported: Option<Instant>,
}

impl LockWatch {
    #[must_use]
    pub fn new(threshold: Duration, probes: impl IntoIterator<Item = Arc<LockProbe>>) -> Self {
        Self {
            threshold,
            watched: probes
                .into_iter()
                .map(|probe| Watched {
                    probe,
                    reported: None,
                })
                .collect(),
        }
    }

    /// Reports what changed since the previous observation.
    pub fn observe(&mut self, now: Instant) -> Vec<LockWatchEvent> {
        let mut events = Vec::new();
        for watched in &mut self.watched {
            let waiting_since = watched.probe.waiting_since();
            // The reported acquisition is over once the probe publishes
            // anything else: no wait at all, or a later one.
            if let Some(reported) = watched.reported
                && waiting_since != Some(reported)
            {
                events.push(LockWatchEvent::Recovered {
                    lock: watched.probe.name,
                    stalled_for: now.saturating_duration_since(reported),
                });
                watched.reported = None;
            }
            if watched.reported.is_none()
                && let Some(since) = waiting_since
                && now.saturating_duration_since(since) >= self.threshold
            {
                events.push(LockWatchEvent::Stalled {
                    lock: watched.probe.name,
                    waited: now.saturating_duration_since(since),
                });
                watched.reported = Some(since);
            }
        }
        events
    }
}

/// A change in how close client workers are to the connection limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityEvent {
    /// At least three quarters of the limit is in use.
    High { outstanding: usize, limit: usize },
    /// Usage fell back to half of the limit or less.
    Normal { outstanding: usize, limit: usize },
}

impl fmt::Display for CapacityEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::High { outstanding, limit } => write!(
                formatter,
                "daemon client workers reached {outstanding}/{limit} of capacity; new connections are refused at the limit"
            ),
            Self::Normal { outstanding, limit } => write!(
                formatter,
                "daemon client workers are back to {outstanding}/{limit} of capacity"
            ),
        }
    }
}

/// Reports when client workers approach the connection limit, before the
/// accept loop starts refusing connections.
///
/// The warning is raised at three quarters of the limit and cleared at half,
/// so a count hovering near one threshold does not repeat it.
#[derive(Debug)]
pub struct CapacityWatch {
    limit: usize,
    high: bool,
}

impl CapacityWatch {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self { limit, high: false }
    }

    /// Passes a report to `report` when usage crossed a threshold.
    pub fn observe(&mut self, outstanding: usize, report: &dyn Fn(&str)) {
        let event = if !self.high && outstanding.saturating_mul(4) >= self.limit.saturating_mul(3) {
            CapacityEvent::High {
                outstanding,
                limit: self.limit,
            }
        } else if self.high && outstanding.saturating_mul(2) <= self.limit {
            CapacityEvent::Normal {
                outstanding,
                limit: self.limit,
            }
        } else {
            return;
        };
        self.high = matches!(event, CapacityEvent::High { .. });
        report(&event.to_string());
    }
}

/// How often the watchdog probes and observes, and when a wait is a stall.
#[derive(Debug, Clone, Copy)]
pub struct LockWatchTiming {
    pub probe_tick: Duration,
    pub watch_tick: Duration,
    pub threshold: Duration,
    /// How long the watch thread waits, after shutdown, for the probes to end.
    pub probe_exit_grace: Duration,
}

impl LockWatchTiming {
    /// A bounded holder keeps a shared runtime lock for a few seconds at most:
    /// a PTY write gives up once it has made no progress for two. A write that
    /// keeps progressing (a long paste to a slow reader) can hold it longer, and
    /// a wait that long is worth reporting too.
    pub const SHIPPING: Self = Self {
        probe_tick: Duration::from_secs(5),
        watch_tick: Duration::from_secs(2),
        threshold: Duration::from_secs(10),
        probe_exit_grace: Duration::from_secs(1),
    };
}

/// Where watchdog reports go.
pub type LockWatchReport = Arc<dyn Fn(&str) + Send + Sync>;

/// The threads [`start_lock_watch`] started.
///
/// The watch thread always exits on shutdown, so a daemon joins it with its
/// other workers. Before it exits it joins every probe that ends within the
/// grace period; a probe that is still parked on a stalled lock is left in
/// `parked` rather than joined, so a stalled lock cannot turn into a shutdown
/// hang. Each probe holds only a weak reference to its lock's owner.
#[derive(Debug)]
pub struct LockWatchThreads {
    pub watch: JoinHandle<()>,
    /// Filled when the watch thread exits.
    pub parked: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

/// Starts one probe thread per target and the watch thread.
///
/// # Errors
///
/// Returns an error when a thread cannot be spawned.
pub fn start_lock_watch(
    targets: Vec<LockTarget>,
    workers: Arc<ClientWorkers>,
    shutdown: &Arc<ShutdownRequest>,
    timing: LockWatchTiming,
    report: LockWatchReport,
) -> std::io::Result<LockWatchThreads> {
    let mut probes = Vec::with_capacity(targets.len());
    let mut probe_threads = Vec::with_capacity(targets.len());
    for target in targets {
        let probe = Arc::new(LockProbe::new(target.name));
        probes.push(Arc::clone(&probe));
        let shutdown = Arc::clone(shutdown);
        let probe_thread = std::thread::Builder::new()
            .name("usagi-lock-probe".to_owned())
            .spawn(move || run_lock_probe(&probe, &*target.target, &shutdown, timing.probe_tick));
        // `?` shares its line with the push: a line holding only the `?` would
        // count as unexecuted whenever every spawn succeeds.
        probe_threads.push(probe_thread?);
    }
    let mut watch = LockWatch::new(timing.threshold, probes);
    let shutdown = Arc::clone(shutdown);
    let parked = Arc::new(Mutex::new(Vec::new()));
    let left = Arc::clone(&parked);
    let watch = std::thread::Builder::new()
        .name("usagi-lock-watch".to_owned())
        .spawn(move || {
            run_lock_watch(&mut watch, &workers, &*report, &shutdown, timing.watch_tick);
            // A probe left running past its test would keep writing coverage
            // counters while the process exits; a daemon gets the same tidy end.
            *left.lock().unwrap_or_else(PoisonError::into_inner) =
                join_ended_probes(probe_threads, timing.probe_exit_grace);
        });
    Ok(LockWatchThreads {
        watch: watch?,
        parked,
    })
}

/// Joins the probes that end within `grace` and returns the ones still parked.
fn join_ended_probes(probes: Vec<JoinHandle<()>>, grace: Duration) -> Vec<JoinHandle<()>> {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && probes.iter().any(|probe| !probe.is_finished()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let (ended, parked): (Vec<_>, Vec<_>) = probes.into_iter().partition(JoinHandle::is_finished);
    for probe in ended {
        // A probe only acquires and releases a lock; there is nothing to report.
        let _ = probe.join();
    }
    parked
}

/// Acquires one lock per tick until shutdown or until its owner is gone.
fn run_lock_probe(
    probe: &LockProbe,
    target: &dyn ProbeTarget,
    shutdown: &ShutdownRequest,
    tick: Duration,
) {
    // One condition, so how the loop ends (shutdown before or after the
    // acquisition, or the owner gone) never decides which lines run.
    while !shutdown.is_requested() && probe.measure(target) && !shutdown.wait_for_tick(tick) {}
}

/// Reports every lock change once per tick until shutdown.
fn run_lock_watch(
    watch: &mut LockWatch,
    workers: &ClientWorkers,
    report: &(dyn Fn(&str) + Send + Sync),
    shutdown: &ShutdownRequest,
    tick: Duration,
) {
    while !shutdown.wait_for_tick(tick) {
        for event in watch.observe(Instant::now()) {
            report(&format!(
                "{event} ({} client workers not yet reaped)",
                workers.outstanding()
            ));
        }
    }
}

#[cfg(test)]
mod tests;
