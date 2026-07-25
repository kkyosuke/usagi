//! Background scope-inventory pump.
//!
//! The shell attaches only the selected foreground terminal (#506); every
//! background tab is detached and therefore has no `Resume` stream that could
//! report its process exiting. This pump is the client's **only** observation
//! primitive for those tabs (#527): it asks the daemon for each background
//! scope's [`TerminalAction::Inventory`] at a bounded cadence on its own thread
//! and reports which tracked terminals the daemon no longer lists as live.
//!
//! [`TerminalAction::Inventory`]: usagi_core::usecase::client::TerminalAction::Inventory
//!
//! The contract this lane deliberately keeps narrow:
//!
//! * It observes **exit metadata only**. It never sends `Attach` or a
//!   terminal-specific `Resume` to a detached background terminal, so a
//!   background tab costs no subscription and no output traffic.
//! * Final output bytes are *not* fetched here. They are reachable when the tab
//!   is brought to the foreground (a fresh attach replays the daemon snapshot)
//!   or through the explicit read-only reopen of the retained tombstone (#525);
//!   that latency is not part of this lane's bound.
//! * Requests are per **scope**, not per terminal, so a target with fifty
//!   background tabs costs the same one request as a target with one.
//! * At most one request per scope is in flight; a slow, hung, or unavailable
//!   owner backs that scope off without ever touching the render thread.
//!
//! The pure scheduling state ([`InventoryState`]) is unit-tested directly with an
//! injected clock; the thread wrapper ([`TerminalInventoryPump`]) is exercised
//! with an in-process fake so the real daemon IPC (injected as the `fetch`
//! closure by the composition root) is the only part left as real IO.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use usagi_core::domain::id::TerminalRef;
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalLaunchScope};

/// Steady cadence between two inventory observations of the same scope. It
/// bounds how long a background exit stays unnoticed: cadence + the queue delay
/// of one round + one request deadline.
const INVENTORY_INTERVAL: Duration = Duration::from_millis(2_000);

/// First backoff step after a failed observation (unavailable owner, hung
/// socket, exhausted request deadline).
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Longest backoff between two attempts while a scope's owner stays unavailable.
const BACKOFF_MAX: Duration = Duration::from_millis(8_000);

/// How long the thread waits when no scope is tracked. Watching a scope wakes it,
/// so this only bounds a pump with no work at all.
const UNWATCHED_INTERVAL: Duration = Duration::from_millis(250);

/// Most scopes observed at once. One workspace has one root scope plus one per
/// session; beyond this the extra scopes are dropped (and counted) rather than
/// letting the request rate follow an unbounded session list.
const MAX_SCOPES: usize = 32;

/// Most background terminals tracked within one scope.
const MAX_TERMINALS_PER_SCOPE: usize = 64;

/// Most exits queued for the render thread. Reaching it keeps the terminal
/// tracked, so the exit is reported by a later round instead of being lost.
const MAX_QUEUED_EXITS: usize = 32;

/// The scope one observation covers, derived from the background terminals
/// themselves: a [`TerminalRef`] already carries its full launch scope.
#[must_use]
pub fn scope_of(terminal: &TerminalRef) -> TerminalLaunchScope {
    TerminalLaunchScope {
        workspace_id: terminal.workspace_id,
        session_id: terminal.session_id,
        worktree_id: terminal.worktree_id,
    }
}

/// One scope's schedule and the background terminals whose liveness it answers.
struct WatchedScope {
    scope: TerminalLaunchScope,
    terminals: Vec<TerminalRef>,
    /// Bumped whenever the watched set or the connection epoch changes, so a
    /// result issued under the previous shape is recognisable as stale.
    generation: u64,
    /// When this scope may be observed again.
    due: Duration,
    /// Consecutive failed observations, driving the backoff.
    failures: u32,
    in_flight: bool,
}

/// One dispatched observation and the fence its completion must still match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryJob {
    pub scope: TerminalLaunchScope,
    pub epoch: u64,
    pub generation: u64,
}

/// Observability for the background lane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InventoryMetrics {
    /// Scope observations dispatched.
    pub observations: u64,
    /// Observations that failed (unavailable owner, hung socket, deadline).
    pub failures: u64,
    /// Observations whose reply carried an entry outside the requested scope.
    pub scope_mismatches: u64,
    /// Completions dropped because their fence no longer matched.
    pub fenced_drops: u64,
    /// Rounds where a scope was skipped because its observation was in flight.
    pub coalesced: u64,
    /// Background terminals observed as no longer live.
    pub exits_observed: u64,
    /// Exits that did not fit the bounded queue and await a later round.
    pub queue_drops: u64,
    /// Watched scopes / terminals refused by the bounds above.
    pub watch_drops: u64,
}

impl InventoryMetrics {
    /// One line describing this lane, or `None` while nothing degraded.
    ///
    /// A failed or wrongly scoped observation, a bound that refused work, and an
    /// exit that did not fit the queue all delay the moment a background exit
    /// becomes visible, so those are what the composition root records when a
    /// workspace closes. Fenced drops and coalesced rounds are ordinary
    /// steady-state events and are reported as context.
    #[must_use]
    pub fn degradation_summary(&self) -> Option<String> {
        if self.failures == 0
            && self.scope_mismatches == 0
            && self.queue_drops == 0
            && self.watch_drops == 0
        {
            return None;
        }
        Some(format!(
            "background inventory lane: {} observations, {} failures, \
             {} scope mismatches, {} fenced drops, {} coalesced, {} exits, \
             {} queue drops, {} watch drops",
            self.observations,
            self.failures,
            self.scope_mismatches,
            self.fenced_drops,
            self.coalesced,
            self.exits_observed,
            self.queue_drops,
            self.watch_drops,
        ))
    }
}

/// Pure scheduling state shared between the render thread and the observation
/// thread. Time is passed in, so the cadence and backoff are tested exactly.
#[derive(Default)]
pub struct InventoryState {
    scopes: Vec<WatchedScope>,
    /// Connection epoch the current watch set belongs to (#523).
    epoch: u64,
    next_generation: u64,
    /// Exits observed and not yet drained by the render thread.
    exited: Vec<TerminalRef>,
    /// Set when the render thread changes the watch set (or the pump shuts
    /// down), so an idle wait ends as soon as there is something to do.
    woken: bool,
    metrics: InventoryMetrics,
}

impl InventoryState {
    /// Replaces the watched background set. Scopes that are still watched keep
    /// their cadence, newly watched scopes are due immediately, and dropped
    /// scopes stop being observed. A changed connection epoch re-arms every
    /// scope, so the observation bound applies again from the moment the shared
    /// transport is available.
    ///
    /// Returns whether something now needs observing sooner than the sleeping
    /// thread planned. The render thread calls this every frame, so an unchanged
    /// set must not wake the thread 60 times a second.
    fn watch(&mut self, epoch: u64, terminals: &[TerminalRef], now: Duration) -> bool {
        let epoch_changed = epoch != self.epoch;
        self.epoch = epoch;
        let mut grouped: Vec<(TerminalLaunchScope, Vec<TerminalRef>)> = Vec::new();
        for terminal in terminals {
            let scope = scope_of(terminal);
            if let Some((_, tracked)) = grouped.iter_mut().find(|(known, _)| *known == scope) {
                if tracked.len() >= MAX_TERMINALS_PER_SCOPE {
                    self.metrics.watch_drops = self.metrics.watch_drops.saturating_add(1);
                    continue;
                }
                tracked.push(terminal.clone());
            } else if grouped.len() >= MAX_SCOPES {
                self.metrics.watch_drops = self.metrics.watch_drops.saturating_add(1);
            } else {
                grouped.push((scope, vec![terminal.clone()]));
            }
        }
        self.scopes
            .retain(|watched| grouped.iter().any(|(scope, _)| *scope == watched.scope));
        let mut needs_observation = false;
        for (scope, terminals) in grouped {
            let known = self
                .scopes
                .iter()
                .position(|watched| watched.scope == scope);
            let Some(index) = known else {
                self.next_generation = self.next_generation.saturating_add(1);
                self.scopes.push(WatchedScope {
                    scope,
                    terminals,
                    generation: self.next_generation,
                    due: now,
                    failures: 0,
                    in_flight: false,
                });
                needs_observation = true;
                continue;
            };
            let next_generation = self.next_generation.saturating_add(1);
            let watched = &mut self.scopes[index];
            let changed = watched.terminals != terminals;
            // A tab that just moved into the background must be observed within
            // the bound measured from now, not from this scope's last round.
            // Losing one (its tab closed) needs no extra observation at all.
            let arrived = terminals
                .iter()
                .any(|terminal| !watched.terminals.contains(terminal));
            watched.terminals = terminals;
            if changed || epoch_changed {
                self.next_generation = next_generation;
                watched.generation = next_generation;
                // The in-flight observation of the previous shape can no longer
                // complete this scope: its result is fenced out, so the slot is
                // free for an observation of the new one.
                watched.in_flight = false;
            }
            if arrived || epoch_changed {
                watched.due = now;
                needs_observation = true;
            }
            if epoch_changed {
                watched.failures = 0;
            }
        }
        needs_observation
    }

    /// The observations due at `now`, at most one per scope. Marks them in
    /// flight so a slow owner coalesces the rounds it overruns instead of
    /// queueing a second request for the same scope.
    fn begin_round(&mut self, now: Duration) -> Vec<InventoryJob> {
        let epoch = self.epoch;
        let mut jobs = Vec::new();
        let mut coalesced = 0_u64;
        for watched in &mut self.scopes {
            if watched.due > now {
                continue;
            }
            if watched.in_flight {
                coalesced = coalesced.saturating_add(1);
                continue;
            }
            watched.in_flight = true;
            jobs.push(InventoryJob {
                scope: watched.scope.clone(),
                epoch,
                generation: watched.generation,
            });
        }
        self.metrics.coalesced = self.metrics.coalesced.saturating_add(coalesced);
        self.metrics.observations = self
            .metrics
            .observations
            .saturating_add(jobs.len().try_into().unwrap_or(u64::MAX));
        jobs
    }

    /// Records one observation under the fence it was dispatched with.
    ///
    /// A reply is usable only when it is still current (same scope, epoch and
    /// watch generation) and every entry belongs to the requested scope. Each
    /// tracked terminal the daemon reports as no longer live is queued for the
    /// render thread exactly once and stops being tracked. A terminal the reply
    /// simply omits stays tracked: a partial or wrongly routed inventory must
    /// not be read as an exit.
    fn apply(
        &mut self,
        job: &InventoryJob,
        result: Result<Vec<TerminalInventoryEntry>, ()>,
        now: Duration,
    ) {
        if job.epoch != self.epoch {
            self.metrics.fenced_drops = self.metrics.fenced_drops.saturating_add(1);
            return;
        }
        let Some(index) = self
            .scopes
            .iter()
            .position(|watched| watched.scope == job.scope && watched.generation == job.generation)
        else {
            self.metrics.fenced_drops = self.metrics.fenced_drops.saturating_add(1);
            return;
        };
        self.scopes[index].in_flight = false;
        let entries = match result {
            Ok(entries)
                if entries
                    .iter()
                    .all(|entry| scope_of(&entry.terminal) == job.scope) =>
            {
                entries
            }
            Ok(_) => {
                self.metrics.scope_mismatches = self.metrics.scope_mismatches.saturating_add(1);
                self.fail(index, now);
                return;
            }
            Err(()) => {
                self.fail(index, now);
                return;
            }
        };
        let mut still_tracked = Vec::new();
        for terminal in std::mem::take(&mut self.scopes[index].terminals) {
            let exited = entries
                .iter()
                .find(|entry| entry.terminal.fences(&terminal))
                .is_some_and(|entry| !entry.live);
            if !exited {
                still_tracked.push(terminal);
                continue;
            }
            if self.exited.len() >= MAX_QUEUED_EXITS {
                self.metrics.queue_drops = self.metrics.queue_drops.saturating_add(1);
                still_tracked.push(terminal);
                continue;
            }
            self.metrics.exits_observed = self.metrics.exits_observed.saturating_add(1);
            self.exited.push(terminal);
        }
        let watched = &mut self.scopes[index];
        watched.terminals = still_tracked;
        watched.failures = 0;
        watched.due = now.saturating_add(INVENTORY_INTERVAL);
    }

    /// Backs one scope off after a failed observation, without stalling it: the
    /// bounded observation resumes on its own once the owner answers again.
    fn fail(&mut self, index: usize, now: Duration) {
        self.metrics.failures = self.metrics.failures.saturating_add(1);
        let watched = &mut self.scopes[index];
        watched.failures = watched.failures.saturating_add(1);
        let shift = watched.failures.saturating_sub(1).min(4);
        let delay = BACKOFF_BASE
            .checked_mul(1_u32 << shift)
            .unwrap_or(BACKOFF_MAX)
            .min(BACKOFF_MAX);
        watched.due = now.saturating_add(delay);
    }

    /// Non-blocking, bounded drain for the render thread: the exits observed so
    /// far, at most `limit` per frame.
    fn take_exited(&mut self, limit: usize) -> Vec<TerminalRef> {
        let taken = self.exited.len().min(limit);
        self.exited.drain(..taken).collect()
    }

    /// How long the observation thread waits before the next round: until the
    /// earliest scope is due again. Watching wakes the thread, so waiting out a
    /// long backoff never delays a newly opened background tab.
    fn next_wait(&self, now: Duration) -> Duration {
        self.scopes
            .iter()
            .filter(|watched| !watched.in_flight)
            .map(|watched| watched.due.saturating_sub(now))
            .min()
            .unwrap_or(UNWATCHED_INTERVAL)
    }

    const fn metrics(&self) -> InventoryMetrics {
        self.metrics
    }
}

/// Locks the state, recovering a poisoned lock: a render-thread panic while
/// holding it must not wedge the observation thread, and the recovered state is
/// internally consistent.
fn lock(state: &Mutex<InventoryState>) -> std::sync::MutexGuard<'_, InventoryState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs every due observation once. Returns nothing to the caller but the state
/// updates; the lock is never held across a `fetch` call, so the render thread's
/// watch and drain never wait on IO.
fn run_round<F>(state: &Mutex<InventoryState>, now: Duration, fetch: &mut F)
where
    F: FnMut(&InventoryJob) -> Result<Vec<TerminalInventoryEntry>, ()>,
{
    let jobs = lock(state).begin_round(now);
    for job in jobs {
        let result = fetch(&job);
        lock(state).apply(&job, result, now);
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<InventoryState>,
    signal: Condvar,
}

/// Sleeps until `interval` elapses or the render thread signals work (a new
/// background tab, or shutdown), whichever comes first. A zero interval — a
/// scope that came due during the round — returns at once.
fn wait_for_next_round(shared: &Shared, interval: Duration) {
    let guard = lock(&shared.state);
    let (mut guard, _timeout) = shared
        .signal
        .wait_timeout_while(guard, interval, |state| !state.woken)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.woken = false;
}

/// Background scope-inventory pump. Owns the observation thread and the shared
/// schedule; the render thread watches and drains through it without blocking.
pub struct TerminalInventoryPump {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    started: Instant,
}

impl TerminalInventoryPump {
    /// Spawns the observation thread. `fetch` performs one scope `Inventory`
    /// request; the composition root injects the real daemon IPC, while tests
    /// inject an in-process fake.
    pub fn spawn<F>(mut fetch: F) -> Self
    where
        F: FnMut(&InventoryJob) -> Result<Vec<TerminalInventoryEntry>, ()> + Send + 'static,
    {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                run_round(&thread_shared.state, started.elapsed(), &mut fetch);
                let interval = lock(&thread_shared.state).next_wait(started.elapsed());
                if thread_stop.load(Ordering::Acquire) {
                    break;
                }
                wait_for_next_round(&thread_shared, interval);
            }
        });
        Self {
            shared,
            stop,
            handle: Some(handle),
            started,
        }
    }

    /// Replaces the watched background set for connection `epoch`
    /// (see [`InventoryState::watch`]).
    pub fn watch(&self, epoch: u64, terminals: &[TerminalRef]) {
        let changed = lock(&self.shared.state).watch(epoch, terminals, self.started.elapsed());
        // The render thread watches every frame; only a real change ends the
        // thread's wait, so a steady set costs no wake-ups at all.
        if changed {
            self.signal();
        }
    }

    /// Drains at most `limit` observed background exits (see
    /// [`InventoryState::take_exited`]).
    pub fn take_exited(&self, limit: usize) -> Vec<TerminalRef> {
        lock(&self.shared.state).take_exited(limit)
    }

    /// Snapshot of the background lane counters.
    pub fn metrics(&self) -> InventoryMetrics {
        lock(&self.shared.state).metrics()
    }

    fn signal(&self) {
        lock(&self.shared.state).woken = true;
        self.shared.signal.notify_all();
    }
}

impl Drop for TerminalInventoryPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.signal();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    // The thread-backed tests bound their waits with retry loops whose sleep
    // branches run only when timing forces a second iteration, so the test
    // bodies themselves are not line-deterministic. The production scheduling
    // logic they drive is fully measured; the test scaffolding is not.
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=inventory_pump_unit_contract
    use super::*;
    use std::sync::mpsc;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::terminal_launch::TerminalKind;

    fn scope_terminal(
        workspace: WorkspaceId,
        session: Option<SessionId>,
        worktree: WorktreeId,
    ) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: session,
            worktree_id: worktree,
        }
    }

    fn terminal() -> TerminalRef {
        scope_terminal(
            WorkspaceId::new(),
            Some(SessionId::new()),
            WorktreeId::new(),
        )
    }

    fn entry(terminal: &TerminalRef, live: bool) -> TerminalInventoryEntry {
        TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Terminal,
            live,
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn a_background_exit_is_observed_once_within_the_cadence_and_reported_to_the_shell() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let jobs = state.begin_round(ms(0));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].scope, scope_of(&background));
        // Still running: nothing to report.
        state.apply(&jobs[0], Ok(vec![entry(&background, true)]), ms(0));
        assert_eq!(state.take_exited(8), Vec::new());
        // Not due again until the cadence elapses.
        assert_eq!(state.begin_round(ms(1_999)), Vec::new());
        assert_eq!(state.next_wait(ms(1_999)), ms(1));

        let jobs = state.begin_round(ms(2_000));
        assert_eq!(jobs.len(), 1);
        state.apply(&jobs[0], Ok(vec![entry(&background, false)]), ms(2_000));
        assert_eq!(state.take_exited(8), vec![background.clone()]);
        // Reported exactly once: a later inventory repeating the tombstone does
        // not re-report it.
        let jobs = state.begin_round(ms(4_000));
        state.apply(&jobs[0], Ok(vec![entry(&background, false)]), ms(4_000));
        assert_eq!(state.take_exited(8), Vec::new());
        assert_eq!(state.metrics().exits_observed, 1);
    }

    #[test]
    fn one_request_per_scope_covers_every_background_terminal_in_it() {
        for panes in [1_usize, 10, 100] {
            let mut state = InventoryState::default();
            let workspace = WorkspaceId::new();
            let worktree = WorktreeId::new();
            let session = Some(SessionId::new());
            let terminals = (0..panes)
                .map(|_| scope_terminal(workspace, session, worktree))
                .collect::<Vec<_>>();
            state.watch(1, &terminals, ms(0));
            let jobs = state.begin_round(ms(0));
            assert_eq!(
                jobs.len(),
                1,
                "{panes} background panes in one scope cost one request"
            );
            let live = terminals
                .iter()
                .take(MAX_TERMINALS_PER_SCOPE)
                .map(|terminal| entry(terminal, true))
                .collect();
            state.apply(&jobs[0], Ok(live), ms(0));
            // A second scope adds exactly one more request, never one per pane.
            assert_eq!(state.metrics().observations, 1);
        }
    }

    #[test]
    fn each_scope_is_observed_independently() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let root = scope_terminal(workspace, None, WorktreeId::new());
        let session = scope_terminal(workspace, Some(SessionId::new()), WorktreeId::new());
        state.watch(1, &[root.clone(), session.clone()], ms(0));
        let jobs = state.begin_round(ms(0));
        assert_eq!(jobs.len(), 2);
        // The session scope's owner is unavailable while the root scope answers.
        state.apply(&jobs[0], Ok(vec![entry(&root, false)]), ms(0));
        state.apply(&jobs[1], Err(()), ms(0));
        assert_eq!(state.take_exited(8), vec![root]);
        // Only the failed scope backs off; the healthy one keeps its cadence.
        let due = state.begin_round(ms(500));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scope, scope_of(&session));
    }

    #[test]
    fn an_unavailable_owner_backs_off_geometrically_and_recovers_the_bound() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let mut now = ms(0);
        let mut delays = Vec::new();
        for _ in 0..6 {
            let jobs = state.begin_round(now);
            assert_eq!(jobs.len(), 1, "a failing scope keeps being retried");
            state.apply(&jobs[0], Err(()), now);
            let wait = state.next_wait(now);
            delays.push(wait);
            // Nothing is dispatched before the backoff elapses, so the failing
            // owner is never hammered.
            assert_eq!(
                state.begin_round(now.saturating_add(wait).saturating_sub(ms(1))),
                Vec::new()
            );
            now = now.saturating_add(wait);
        }
        assert_eq!(
            delays,
            vec![
                BACKOFF_BASE,
                ms(1_000),
                ms(2_000),
                ms(4_000),
                BACKOFF_MAX,
                BACKOFF_MAX,
            ],
            "an unavailable owner backs off geometrically to the capped delay"
        );
        assert_eq!(state.metrics().failures, 6);
        // Once the owner answers again, the bounded cadence applies from now.
        let jobs = state.begin_round(now);
        state.apply(&jobs[0], Ok(vec![entry(&background, false)]), now);
        assert_eq!(state.take_exited(8), vec![background]);
        assert_eq!(state.next_wait(now), INVENTORY_INTERVAL);
    }

    #[test]
    fn a_hung_scope_does_not_queue_a_second_request_and_does_not_block_its_peers() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let hung = scope_terminal(workspace, None, WorktreeId::new());
        let healthy = scope_terminal(workspace, Some(SessionId::new()), WorktreeId::new());
        state.watch(1, &[hung.clone(), healthy.clone()], ms(0));
        let first = state.begin_round(ms(0));
        assert_eq!(first.len(), 2);
        // The hung scope never answers; the healthy one completes and is due again.
        state.apply(&first[1], Ok(vec![entry(&healthy, false)]), ms(0));
        let next = state.begin_round(ms(9_000));
        assert_eq!(next.len(), 1, "the hung scope has a request in flight");
        assert_eq!(next[0].scope, scope_of(&healthy));
        assert_eq!(state.metrics().coalesced, 1);
        assert_eq!(state.take_exited(8), vec![healthy]);
    }

    #[test]
    fn a_newly_backgrounded_tab_is_observed_at_once_and_losing_one_is_not() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let worktree = WorktreeId::new();
        let first = scope_terminal(workspace, None, worktree);
        let second = scope_terminal(workspace, None, worktree);
        state.watch(1, std::slice::from_ref(&first), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        state.apply(&job, Ok(vec![entry(&first, true)]), ms(0));
        assert_eq!(state.begin_round(ms(50)), Vec::new());

        // A second tab moves into the background: its exit bound is measured from
        // now, not from this scope's last round.
        state.watch(1, &[first.clone(), second.clone()], ms(50));
        assert_eq!(state.begin_round(ms(50)).len(), 1);
        state.apply(
            &InventoryJob {
                scope: scope_of(&first),
                epoch: 1,
                generation: state.scopes[0].generation,
            },
            Ok(vec![entry(&first, true), entry(&second, true)]),
            ms(50),
        );

        // Losing a tab (the user closed it) needs no extra observation.
        assert!(!state.watch(1, std::slice::from_ref(&first), ms(60)));
        assert_eq!(state.begin_round(ms(60)), Vec::new());
    }

    #[test]
    fn a_late_result_from_a_replaced_watch_set_is_dropped() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let worktree = WorktreeId::new();
        let first = scope_terminal(workspace, None, worktree);
        let second = scope_terminal(workspace, None, worktree);
        state.watch(1, std::slice::from_ref(&first), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        // The user opened another background tab in the same scope while the
        // observation was in flight.
        state.watch(1, &[first.clone(), second.clone()], ms(10));
        state.apply(&job, Ok(vec![entry(&first, false)]), ms(20));
        assert_eq!(state.take_exited(8), Vec::new());
        assert_eq!(state.metrics().fenced_drops, 1);
        // The fresh watch set is observed again immediately.
        let job = state.begin_round(ms(20)).remove(0);
        state.apply(
            &job,
            Ok(vec![entry(&first, false), entry(&second, true)]),
            ms(20),
        );
        assert_eq!(state.take_exited(8), vec![first]);
    }

    #[test]
    fn a_connection_epoch_change_rearms_every_scope_and_fences_the_old_result() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let superseded = state.begin_round(ms(0)).remove(0);
        // The shared transport was replaced: every pane re-attaches and the
        // observation bound restarts from this moment.
        state.watch(2, std::slice::from_ref(&background), ms(100));
        state.apply(&superseded, Ok(vec![entry(&background, false)]), ms(100));
        assert_eq!(state.take_exited(8), Vec::new());
        assert_eq!(state.metrics().fenced_drops, 1);
        let fresh = state.begin_round(ms(100));
        assert_eq!(fresh.len(), 1, "the new epoch observes the scope at once");
        state.apply(&fresh[0], Ok(vec![entry(&background, false)]), ms(100));
        assert_eq!(state.take_exited(8), vec![background]);
    }

    #[test]
    fn an_unwatched_scopes_result_is_dropped() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        // The tab was foregrounded (or closed) before the reply arrived.
        state.watch(1, &[], ms(5));
        state.apply(&job, Ok(vec![entry(&background, false)]), ms(5));
        assert_eq!(state.take_exited(8), Vec::new());
        assert_eq!(state.metrics().fenced_drops, 1);
        assert_eq!(state.next_wait(ms(5)), UNWATCHED_INTERVAL);
    }

    #[test]
    fn a_reply_carrying_a_foreign_scope_is_treated_as_a_failure_not_an_exit() {
        let mut state = InventoryState::default();
        let background = terminal();
        let foreign = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        state.apply(&job, Ok(vec![entry(&foreign, false)]), ms(0));
        assert_eq!(state.take_exited(8), Vec::new());
        assert_eq!(state.metrics().scope_mismatches, 1);
        assert_eq!(state.metrics().failures, 1);
    }

    #[test]
    fn a_terminal_missing_from_the_reply_is_not_read_as_an_exit() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        // A partial inventory omits the terminal entirely.
        state.apply(&job, Ok(Vec::new()), ms(0));
        assert_eq!(state.take_exited(8), Vec::new());
        // It stays tracked, so a later inventory can still report its exit.
        let job = state.begin_round(ms(2_000)).remove(0);
        state.apply(&job, Ok(vec![entry(&background, false)]), ms(2_000));
        assert_eq!(state.take_exited(8), vec![background]);
    }

    #[test]
    fn the_watch_set_scope_count_and_terminal_count_are_bounded() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let terminals = (0..MAX_SCOPES + 4)
            .map(|_| scope_terminal(workspace, Some(SessionId::new()), WorktreeId::new()))
            .collect::<Vec<_>>();
        state.watch(1, &terminals, ms(0));
        assert_eq!(state.begin_round(ms(0)).len(), MAX_SCOPES);
        assert_eq!(state.metrics().watch_drops, 4);

        let mut state = InventoryState::default();
        let worktree = WorktreeId::new();
        let session = Some(SessionId::new());
        let crowded = (0..MAX_TERMINALS_PER_SCOPE + 3)
            .map(|_| scope_terminal(workspace, session, worktree))
            .collect::<Vec<_>>();
        state.watch(1, &crowded, ms(0));
        assert_eq!(state.begin_round(ms(0)).len(), 1);
        assert_eq!(state.metrics().watch_drops, 3);
    }

    #[test]
    fn the_exit_queue_is_bounded_and_undelivered_exits_are_retried() {
        let mut state = InventoryState::default();
        let workspace = WorkspaceId::new();
        let worktree = WorktreeId::new();
        let session = Some(SessionId::new());
        let terminals = (0..MAX_QUEUED_EXITS + 8)
            .map(|_| scope_terminal(workspace, session, worktree))
            .collect::<Vec<_>>();
        state.watch(1, &terminals, ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        let all_exited = terminals
            .iter()
            .map(|terminal| entry(terminal, false))
            .collect::<Vec<_>>();
        state.apply(&job, Ok(all_exited.clone()), ms(0));
        assert_eq!(state.metrics().queue_drops, 8);
        // The render thread drains a bounded slice per frame.
        assert_eq!(state.take_exited(16).len(), 16);
        assert_eq!(state.take_exited(1_000).len(), MAX_QUEUED_EXITS - 16);
        assert_eq!(state.take_exited(8), Vec::new());
        // The exits that did not fit are still tracked, so the next round
        // reports them instead of losing them.
        let job = state.begin_round(ms(2_000)).remove(0);
        state.apply(&job, Ok(all_exited), ms(2_000));
        assert_eq!(state.take_exited(1_000).len(), 8);
        assert_eq!(
            state.metrics().exits_observed,
            u64::try_from(terminals.len()).unwrap()
        );
    }

    #[test]
    fn only_a_degraded_lane_is_summarised_for_the_failure_log() {
        let mut state = InventoryState::default();
        let background = terminal();
        state.watch(1, std::slice::from_ref(&background), ms(0));
        let job = state.begin_round(ms(0)).remove(0);
        state.apply(&job, Ok(vec![entry(&background, false)]), ms(0));
        assert_eq!(state.take_exited(8), vec![background.clone()]);
        // A healthy lane that observed an exit has nothing to report.
        assert_eq!(state.metrics().degradation_summary(), None);

        state.watch(1, std::slice::from_ref(&background), ms(10));
        let job = state.begin_round(ms(10)).remove(0);
        state.apply(&job, Err(()), ms(10));
        let summary = state
            .metrics()
            .degradation_summary()
            .expect("a failed observation is worth recording");
        assert!(
            summary.starts_with("background inventory lane: "),
            "{summary}"
        );
        assert!(summary.contains("1 failures"), "{summary}");
        assert!(summary.contains("1 exits"), "{summary}");
    }

    #[test]
    fn the_observation_thread_reports_a_background_exit_without_attaching() {
        let background = terminal();
        let (tx, rx) = mpsc::channel();
        let observed = background.clone();
        let pump = TerminalInventoryPump::spawn(move |job| {
            assert_eq!(job.scope, scope_of(&observed));
            let _ = tx.send(job.clone());
            Ok(vec![entry(&observed, false)])
        });
        pump.watch(1, std::slice::from_ref(&background));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the pump thread observes a watched scope");
        let mut exited = Vec::new();
        for _ in 0..200 {
            exited = pump.take_exited(8);
            if !exited.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(exited, vec![background]);
        assert_eq!(pump.metrics().exits_observed, 1);
    }

    #[test]
    fn dropping_the_pump_stops_the_thread_even_mid_observation() {
        let background = terminal();
        let (started, observing) = mpsc::channel();
        let pump = TerminalInventoryPump::spawn(move |_| {
            let _ = started.send(());
            // Hold the round open long enough for the drop below to land while
            // this observation is still in flight.
            std::thread::sleep(Duration::from_millis(200));
            Err(())
        });
        pump.watch(1, std::slice::from_ref(&background));
        observing
            .recv_timeout(Duration::from_secs(5))
            .expect("the pump thread starts an observation");
        // Drop joins the thread: it must notice the stop after the in-flight
        // observation returns instead of waiting out the backoff.
        let stopping = Instant::now();
        drop(pump);
        assert!(stopping.elapsed() < BACKOFF_BASE.saturating_mul(4));
    }

    #[test]
    fn the_observation_thread_survives_a_failing_owner() {
        let background = terminal();
        let (tx, rx) = mpsc::channel();
        let pump = TerminalInventoryPump::spawn(move |_| {
            let _ = tx.send(());
            Err(())
        });
        pump.watch(1, std::slice::from_ref(&background));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a failing scope is still observed");
        assert!(pump.take_exited(8).is_empty());
    }
}
