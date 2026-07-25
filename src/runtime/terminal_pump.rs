//! Foreground terminal output pump.
//!
//! The interactive TUI renders on a single thread that, every frame, asks the
//! daemon for the selected foreground terminal's new output. Doing that fetch
//! inline means a momentarily busy daemon stalls the whole render/input loop.
//! This pump moves the `Resume` fetch onto a background thread: it reads the
//! registered terminals into per-terminal read-ahead buffers at a bounded
//! interactive cadence, and the render thread drains those buffers without ever
//! blocking on the daemon.
//!
//! Three properties beyond "not on the render thread" are the pump's own
//! responsibility (#527):
//!
//! * **Bounded cadence.** A silent terminal backs the fetch interval off from
//!   the interactive [`ACTIVE_INTERVAL`] to [`IDLE_MAX_INTERVAL`], so an idle
//!   TUI stops generating tens of requests per second. Output, a fresh
//!   registration, and [`TerminalPollPump::wake`] (called when the user's input
//!   or a resize is about to produce output) restore the interactive cadence
//!   immediately through the condvar the fetch thread sleeps on.
//! * **Fenced completions.** Each fetch carries the exact [`TerminalRef`], the
//!   connection epoch it was registered on, the registration generation, and the
//!   requested cursor. A result whose fence no longer matches — the pane was
//!   re-registered by a focus switch, a resync, or a [connection epoch] change
//!   while the fetch was in flight — is dropped instead of applied at the wrong
//!   cursor.
//! * **Bounded buffers.** At most one fetch per terminal is in flight, and a
//!   read-ahead buffer the render thread stops draining is capped: overflowing
//!   it asks the session for a resync (the daemon's atomic snapshot) rather than
//!   growing without limit.
//!
//! [connection epoch]: ../../../document/03-tui.md
//!
//! The pure buffering state ([`PumpState`]) is unit-tested directly; the thread
//! wrapper ([`TerminalPollPump`]) is exercised with an in-process fake fetch so
//! the real daemon IPC (injected as the `fetch` closure by the composition root)
//! is the only part left as real IO.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use usagi_core::domain::id::TerminalRef;
use usagi_tui::usecase::application::terminal_session::{TerminalChunk, TerminalError};

/// Interactive cadence: how long the fetch thread sleeps between rounds while
/// the foreground terminal is producing output or the user just interacted.
/// Kept below the render frame tick so a drained buffer refills before the next
/// frame.
const ACTIVE_INTERVAL: Duration = Duration::from_millis(8);

/// Bounded idle cadence: the longest the fetch thread waits between rounds while
/// a registered terminal stays silent. Reached by doubling [`ACTIVE_INTERVAL`],
/// and left immediately when output arrives or the render thread wakes the pump.
const IDLE_MAX_INTERVAL: Duration = Duration::from_millis(64);

/// How long the fetch thread waits when nothing is registered (or every
/// registration is stalled). Registering wakes it, so this only bounds the
/// wake-up of a pump with no work at all.
const UNREGISTERED_INTERVAL: Duration = Duration::from_millis(250);

/// Read-ahead cap per terminal. The render thread drains every frame, so this is
/// only reached when it stops draining entirely (a modal loop, a stalled draw).
/// Passing it costs one resync instead of unbounded memory.
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

/// One registered terminal's read-ahead buffer.
struct TerminalBuffer {
    terminal: TerminalRef,
    /// The connection epoch the current registration was taken on (#523).
    epoch: u64,
    /// Bumped by every registration so an in-flight fetch taken before it is
    /// recognisable as stale even when the ref and epoch are unchanged.
    generation: u64,
    /// Offset the next fetch resumes from.
    fetch_offset: u64,
    /// Contiguous chunks fetched but not yet drained by the render thread.
    pending: VecDeque<TerminalChunk>,
    /// Buffered bytes in `pending`, bounded by [`MAX_PENDING_BYTES`].
    pending_bytes: usize,
    /// A fetch failure awaiting delivery once `pending` is drained.
    error: Option<TerminalError>,
    /// Set after a fetch error so the fetch thread stops fetching this terminal
    /// until the render thread re-registers it (on reattach).
    stalled: bool,
    /// Whether this terminal already has a fetch in flight. At most one is, so a
    /// slow daemon coalesces rounds instead of queueing requests.
    in_flight: bool,
}

/// The identity one fetch was issued under. A completion is applied only while
/// every field still describes the current registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFence {
    pub terminal: TerminalRef,
    pub epoch: u64,
    pub generation: u64,
    pub after_offset: u64,
}

/// Observability for the foreground lane. Counters only ever grow; the render
/// thread snapshots them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PumpMetrics {
    /// Fetch rounds issued to the daemon.
    pub fetches: u64,
    /// Fetches that returned at least one chunk.
    pub fetches_with_output: u64,
    /// Fetch failures (including a request that exhausted its deadline).
    pub errors: u64,
    /// Completions dropped because their fence no longer matched.
    pub fenced_drops: u64,
    /// Rounds where a terminal was skipped because its fetch was still in
    /// flight, i.e. cadence coalesced into the outstanding request.
    pub coalesced: u64,
    /// Read-ahead overflows converted into a resync request.
    pub overflow_resyncs: u64,
    /// Explicit interactive wake-ups (input / resize) that reset the cadence.
    pub wakes: u64,
}

impl PumpMetrics {
    /// One line describing this lane, or `None` while nothing degraded.
    ///
    /// Fetch failures (an unavailable owner, a hung socket, an exhausted request
    /// deadline) and read-ahead overflows are what a user feels as a stalled
    /// pane, so those are what the composition root records when a workspace
    /// closes. Fenced drops and coalesced rounds are ordinary steady-state
    /// events — a focus switch or a resync produces them — so they are reported
    /// as context, never as the reason.
    #[must_use]
    pub fn degradation_summary(&self) -> Option<String> {
        if self.errors == 0 && self.overflow_resyncs == 0 {
            return None;
        }
        Some(format!(
            "foreground poll lane: {} fetches ({} with output), {} errors, \
             {} fenced drops, {} coalesced, {} overflow resyncs, {} wakes",
            self.fetches,
            self.fetches_with_output,
            self.errors,
            self.fenced_drops,
            self.coalesced,
            self.overflow_resyncs,
            self.wakes,
        ))
    }
}

/// Pure buffering state shared between the render thread and the fetch thread.
/// Every method is deterministic; the mutex lives in [`TerminalPollPump`].
#[derive(Default)]
pub struct PumpState {
    terminals: Vec<TerminalBuffer>,
    /// Monotonic registration counter handing out [`TerminalBuffer::generation`].
    next_generation: u64,
    /// Set by [`Self::wake`] and by a fresh registration; makes the next round
    /// run immediately at the interactive cadence.
    woken: bool,
    /// Consecutive rounds that fetched something but produced no output, used to
    /// back the cadence off towards [`IDLE_MAX_INTERVAL`].
    idle_rounds: u32,
    metrics: PumpMetrics,
}

impl PumpState {
    /// Registers a terminal to fetch from `offset` on connection `epoch`, or
    /// resets an existing one to that offset. Reattach (after a reconnect or
    /// resync) re-registers with the snapshot's output offset, which clears any
    /// buffered output and error, fences out the in-flight fetch of the previous
    /// registration, and resumes fetching at the interactive cadence.
    fn register(&mut self, terminal: &TerminalRef, offset: u64, epoch: u64) {
        self.next_generation = self.next_generation.saturating_add(1);
        let generation = self.next_generation;
        self.woken = true;
        self.idle_rounds = 0;
        if let Some(buffer) = self
            .terminals
            .iter_mut()
            .find(|buffer| buffer.terminal.fences(terminal))
        {
            buffer.epoch = epoch;
            buffer.generation = generation;
            buffer.fetch_offset = offset;
            buffer.pending.clear();
            buffer.pending_bytes = 0;
            buffer.error = None;
            buffer.stalled = false;
            // The in-flight fetch of the previous registration can no longer
            // complete this buffer — its result is fenced out — so the slot is
            // free for a fetch at the fresh cursor.
            buffer.in_flight = false;
        } else {
            self.terminals.push(TerminalBuffer {
                terminal: terminal.clone(),
                epoch,
                generation,
                fetch_offset: offset,
                pending: VecDeque::new(),
                pending_bytes: 0,
                error: None,
                stalled: false,
                in_flight: false,
            });
        }
    }

    /// Stops tracking a terminal; a later fetch result for it is discarded.
    fn unregister(&mut self, terminal: &TerminalRef) {
        self.terminals
            .retain(|buffer| !buffer.terminal.fences(terminal));
    }

    /// Restores the interactive cadence because the user just acted on the
    /// foreground terminal (input, resize) and output is expected right away.
    fn wake(&mut self) {
        self.woken = true;
        self.idle_rounds = 0;
        self.metrics.wakes = self.metrics.wakes.saturating_add(1);
    }

    /// Non-blocking drain for the render thread. Returns the buffered output at
    /// or after `after_offset`; when the buffer is empty and a fetch error is
    /// pending, surfaces that error so the session state machine reacts exactly
    /// as it did when polling inline. An unregistered terminal yields no output.
    fn take(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        let Some(buffer) = self
            .terminals
            .iter_mut()
            .find(|buffer| buffer.terminal.fences(terminal))
        else {
            return Ok(Vec::new());
        };
        let mut chunks = Vec::new();
        while let Some(front) = buffer.pending.front() {
            if front.end_offset <= after_offset {
                let dropped = buffer.pending.pop_front().expect("front was just observed");
                buffer.pending_bytes = buffer.pending_bytes.saturating_sub(dropped.data.len());
                continue;
            }
            let chunk = buffer.pending.pop_front().expect("front was just observed");
            buffer.pending_bytes = buffer.pending_bytes.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        if chunks.is_empty()
            && let Some(error) = buffer.error
        {
            return Err(error);
        }
        Ok(chunks)
    }

    /// The fences of the terminals eligible for a fetch round. Stalled terminals
    /// (failed until re-registered) and terminals whose fetch is still in flight
    /// are skipped, so at most one request per terminal is ever outstanding.
    fn begin_round(&mut self) -> Vec<FetchFence> {
        let mut coalesced = 0_u64;
        let mut fences = Vec::new();
        for buffer in &mut self.terminals {
            if buffer.stalled {
                continue;
            }
            if buffer.in_flight {
                coalesced = coalesced.saturating_add(1);
                continue;
            }
            buffer.in_flight = true;
            fences.push(FetchFence {
                terminal: buffer.terminal.clone(),
                epoch: buffer.epoch,
                generation: buffer.generation,
                after_offset: buffer.fetch_offset,
            });
        }
        self.metrics.coalesced = self.metrics.coalesced.saturating_add(coalesced);
        self.metrics.fetches = self
            .metrics
            .fetches
            .saturating_add(fences.len().try_into().unwrap_or(u64::MAX));
        fences
    }

    /// Records one fetch outcome under the fence it was issued with. Output
    /// advances the fetch offset and appends to the buffer; an error is retained
    /// and stalls further fetches. A result whose registration was replaced or
    /// removed mid-fetch is dropped: applying it would rewind a resynced cursor
    /// or paste a superseded epoch's bytes into the new screen.
    fn apply_fetch(
        &mut self,
        fence: &FetchFence,
        result: Result<Vec<TerminalChunk>, TerminalError>,
    ) {
        let mut fenced_drop = false;
        let mut overflowed = false;
        let mut produced = false;
        let mut failed = false;
        if let Some(buffer) = self
            .terminals
            .iter_mut()
            .find(|buffer| buffer.terminal.fences(&fence.terminal))
        {
            if buffer.generation == fence.generation && buffer.epoch == fence.epoch {
                buffer.in_flight = false;
                match result {
                    Ok(chunks) => {
                        if let Some(last) = chunks.last() {
                            buffer.fetch_offset = last.end_offset;
                            produced = true;
                        }
                        for chunk in chunks {
                            buffer.pending_bytes =
                                buffer.pending_bytes.saturating_add(chunk.data.len());
                            buffer.pending.push_back(chunk);
                        }
                        if buffer.pending_bytes > MAX_PENDING_BYTES {
                            buffer.pending.clear();
                            buffer.pending_bytes = 0;
                            buffer.error = Some(TerminalError::ResyncRequired);
                            buffer.stalled = true;
                            overflowed = true;
                        }
                    }
                    Err(error) => {
                        buffer.error = Some(error);
                        buffer.stalled = true;
                        failed = true;
                    }
                }
            } else {
                fenced_drop = true;
            }
        } else {
            fenced_drop = true;
        }
        if fenced_drop {
            self.metrics.fenced_drops = self.metrics.fenced_drops.saturating_add(1);
            return;
        }
        if produced {
            self.metrics.fetches_with_output = self.metrics.fetches_with_output.saturating_add(1);
            self.idle_rounds = 0;
        }
        if overflowed {
            self.metrics.overflow_resyncs = self.metrics.overflow_resyncs.saturating_add(1);
        }
        if failed {
            self.metrics.errors = self.metrics.errors.saturating_add(1);
        }
    }

    /// How long the fetch thread waits before the next round, folding in this
    /// round's outcome. Zero when an interactive wake-up arrived (including
    /// during the round), the interactive cadence while output flows, a doubling
    /// backoff up to [`IDLE_MAX_INTERVAL`] while the terminal is silent, and
    /// [`UNREGISTERED_INTERVAL`] when there is nothing to fetch at all.
    fn next_interval(&mut self, fetched: bool) -> Duration {
        if std::mem::take(&mut self.woken) {
            return Duration::ZERO;
        }
        if !fetched {
            return UNREGISTERED_INTERVAL;
        }
        if self.idle_rounds == 0 {
            self.idle_rounds = 1;
            return ACTIVE_INTERVAL;
        }
        let interval = ACTIVE_INTERVAL
            .checked_mul(1_u32 << self.idle_rounds.min(8))
            .unwrap_or(IDLE_MAX_INTERVAL)
            .min(IDLE_MAX_INTERVAL);
        self.idle_rounds = self.idle_rounds.saturating_add(1);
        interval
    }

    const fn metrics(&self) -> PumpMetrics {
        self.metrics
    }
}

/// Runs the `fetch` closure against every eligible terminal, updating the shared
/// state. Returns whether any terminal was fetched, so the caller can pick the
/// interactive cadence while work is flowing. The state lock is never held
/// across a `fetch` call, so the render thread's drain never waits on IO.
fn run_round<F>(state: &Mutex<PumpState>, fetch: &mut F) -> bool
where
    F: FnMut(&FetchFence) -> Result<Vec<TerminalChunk>, TerminalError>,
{
    let fences = lock(state).begin_round();
    let worked = !fences.is_empty();
    for fence in fences {
        let result = fetch(&fence);
        lock(state).apply_fetch(&fence, result);
    }
    worked
}

/// Locks the pump state, recovering a poisoned lock. The buffered output is not
/// safety-critical, so a render-thread panic while holding the lock must not
/// wedge the fetch thread; the recovered state is internally consistent.
fn lock(state: &Mutex<PumpState>) -> std::sync::MutexGuard<'_, PumpState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sleeps until `interval` elapses or the render thread signals work, whichever
/// comes first. Waiting on the condvar (rather than sleeping blindly) is what
/// lets the idle cadence be long without delaying the output of a keystroke.
fn wait_for_next_round(shared: &Shared, interval: Duration) {
    if interval.is_zero() {
        return;
    }
    let guard = lock(&shared.state);
    let (mut guard, _timeout) = shared
        .signal
        .wait_timeout_while(guard, interval, |state| !state.woken)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The wake is spent by ending this wait, so the round that follows uses the
    // ordinary cadence instead of running twice back to back.
    guard.woken = false;
}

/// State shared with the fetch thread, plus the condvar the render thread uses
/// to cut an idle wait short.
#[derive(Default)]
struct Shared {
    state: Mutex<PumpState>,
    signal: Condvar,
}

/// Foreground terminal output pump. Owns the fetch thread and the shared buffer;
/// the render thread registers/drains through it without blocking on IO.
pub struct TerminalPollPump {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TerminalPollPump {
    /// Spawns the fetch thread. `fetch` performs one `Resume` fetch for the
    /// terminal and cursor named by the fence; the composition root injects the
    /// real daemon IPC, while tests inject an in-process fake.
    pub fn spawn<F>(mut fetch: F) -> Self
    where
        F: FnMut(&FetchFence) -> Result<Vec<TerminalChunk>, TerminalError> + Send + 'static,
    {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let worked = run_round(&thread_shared.state, &mut fetch);
                let interval = lock(&thread_shared.state).next_interval(worked);
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
        }
    }

    /// Registers or resets a terminal (see [`PumpState::register`]) and restores
    /// the interactive cadence.
    pub fn register(&self, terminal: &TerminalRef, offset: u64, epoch: u64) {
        lock(&self.shared.state).register(terminal, offset, epoch);
        self.shared.signal.notify_all();
    }

    /// Stops tracking a terminal (see [`PumpState::unregister`]).
    pub fn unregister(&self, terminal: &TerminalRef) {
        lock(&self.shared.state).unregister(terminal);
    }

    /// Restores the interactive cadence for an interaction that is about to
    /// produce output (see [`PumpState::wake`]).
    pub fn wake(&self) {
        lock(&self.shared.state).wake();
        self.shared.signal.notify_all();
    }

    /// Drains buffered output for the render thread (see [`PumpState::take`]).
    pub fn take(
        &self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        lock(&self.shared.state).take(terminal, after_offset)
    }

    /// Snapshot of the foreground lane counters.
    pub fn metrics(&self) -> PumpMetrics {
        lock(&self.shared.state).metrics()
    }
}

impl Drop for TerminalPollPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Cut a pending idle wait short so quitting never waits out the cadence.
        lock(&self.shared.state).woken = true;
        self.shared.signal.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    // The thread-backed tests bound their waits with retry loops whose sleep and
    // fake-fetch branches run only when timing forces a second iteration, so the
    // test bodies themselves are not line-deterministic. The production pump
    // logic they drive is fully measured; the test scaffolding is not.
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=terminal_pump_unit_contract
    use super::*;
    use std::sync::mpsc;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn chunk(start: u64, data: &[u8]) -> TerminalChunk {
        TerminalChunk {
            start_offset: start,
            end_offset: start + data.len() as u64,
            data: data.to_vec(),
        }
    }

    /// The single fence a round is expected to produce.
    fn only_fence(state: &mut PumpState) -> FetchFence {
        let mut fences = state.begin_round();
        assert_eq!(fences.len(), 1, "exactly one terminal is eligible");
        fences.pop().expect("length was just asserted")
    }

    #[test]
    fn take_drains_buffered_output_in_order_and_advances_the_fetch_offset() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 10, 1);
        let fence = only_fence(&mut state);
        assert_eq!(fence.after_offset, 10);
        assert_eq!(fence.epoch, 1);

        state.apply_fetch(&fence, Ok(vec![chunk(10, b"ab"), chunk(12, b"cd")]));
        // The next fetch resumes after the last chunk.
        assert_eq!(only_fence(&mut state).after_offset, 14);

        let drained = state.take(&terminal, 10).unwrap();
        assert_eq!(drained, vec![chunk(10, b"ab"), chunk(12, b"cd")]);
        // Nothing buffered now.
        assert_eq!(state.take(&terminal, 14).unwrap(), Vec::new());
    }

    #[test]
    fn take_skips_output_already_consumed_by_the_render_thread() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let fence = only_fence(&mut state);
        state.apply_fetch(&fence, Ok(vec![chunk(0, b"ab"), chunk(2, b"cd")]));
        // The render thread already applied up to offset 2, so the first chunk is
        // dropped and only the newer one is returned.
        assert_eq!(state.take(&terminal, 2).unwrap(), vec![chunk(2, b"cd")]);
    }

    #[test]
    fn an_error_surfaces_only_after_buffered_output_is_drained_and_stalls_fetching() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let first = only_fence(&mut state);
        state.apply_fetch(&first, Ok(vec![chunk(0, b"ab")]));
        let second = only_fence(&mut state);
        state.apply_fetch(&second, Err(TerminalError::ResyncRequired));
        // A stalled terminal is not fetched again until it is re-registered.
        assert_eq!(state.begin_round(), Vec::new());
        // Buffered output is delivered first.
        assert_eq!(state.take(&terminal, 0).unwrap(), vec![chunk(0, b"ab")]);
        // Then the error, repeatedly, until reattach.
        assert_eq!(state.take(&terminal, 2), Err(TerminalError::ResyncRequired));
        assert_eq!(state.take(&terminal, 2), Err(TerminalError::ResyncRequired));
        assert_eq!(state.metrics().errors, 1);
    }

    #[test]
    fn reregister_resets_offset_buffer_error_and_resumes_fetching() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let first = only_fence(&mut state);
        state.apply_fetch(&first, Ok(vec![chunk(0, b"ab")]));
        let second = only_fence(&mut state);
        state.apply_fetch(&second, Err(TerminalError::Unavailable));
        // Reattach at a fresh snapshot offset.
        state.register(&terminal, 100, 1);
        assert_eq!(only_fence(&mut state).after_offset, 100);
        assert_eq!(state.take(&terminal, 100).unwrap(), Vec::new());
    }

    #[test]
    fn unregister_drops_the_terminal_and_a_late_fetch_result_is_ignored() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let fence = only_fence(&mut state);
        state.unregister(&terminal);
        assert_eq!(state.begin_round(), Vec::new());
        // A fetch result that raced with unregistration is dropped silently.
        state.apply_fetch(&fence, Ok(vec![chunk(0, b"ab")]));
        assert_eq!(state.take(&terminal, 0).unwrap(), Vec::new());
        assert_eq!(state.metrics().fenced_drops, 1);
    }

    #[test]
    fn a_result_from_a_superseded_registration_is_not_applied_to_the_new_cursor() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let superseded = only_fence(&mut state);
        // A resync re-registers the same terminal at a fresh snapshot offset
        // while the previous fetch is still in flight.
        state.register(&terminal, 900, 1);
        state.apply_fetch(&superseded, Ok(vec![chunk(0, b"old")]));
        assert_eq!(state.take(&terminal, 900).unwrap(), Vec::new());
        // The cursor stayed at the resynced offset.
        assert_eq!(only_fence(&mut state).after_offset, 900);
        assert_eq!(state.metrics().fenced_drops, 1);
    }

    #[test]
    fn rapid_focus_switching_applies_only_the_newest_registration() {
        let mut state = PumpState::default();
        let first = terminal();
        let second = terminal();
        state.register(&first, 0, 1);
        let in_flight = only_fence(&mut state);
        // The user cycles tabs faster than the daemon answers: each switch
        // detaches one terminal and attaches the next.
        for round in 1..=5_u64 {
            let (leaving, arriving) = if round % 2 == 1 {
                (&first, &second)
            } else {
                (&second, &first)
            };
            state.unregister(leaving);
            state.register(arriving, round * 10, 1);
        }
        // The fetch issued for the first focus finally returns.
        state.apply_fetch(&in_flight, Ok(vec![chunk(0, b"stale")]));
        assert_eq!(state.metrics().fenced_drops, 1);
        assert_eq!(state.take(&first, 50).unwrap(), Vec::new());
        // Only the terminal focused last is fetched, from its own cursor.
        let fence = only_fence(&mut state);
        assert!(fence.terminal.fences(&second));
        assert_eq!(fence.after_offset, 50);
        state.apply_fetch(&fence, Ok(vec![chunk(50, b"fresh")]));
        assert_eq!(state.take(&second, 50).unwrap(), vec![chunk(50, b"fresh")]);
    }

    #[test]
    fn a_result_from_a_superseded_connection_epoch_is_dropped() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let superseded = only_fence(&mut state);
        // A reconnect advanced the shared connection epoch and the pane
        // re-attached on the new one at the same offset.
        state.register(&terminal, 0, 2);
        state.apply_fetch(&superseded, Ok(vec![chunk(0, b"old")]));
        assert_eq!(state.take(&terminal, 0).unwrap(), Vec::new());
        assert_eq!(state.metrics().fenced_drops, 1);
    }

    #[test]
    fn a_stale_error_from_a_superseded_registration_does_not_stall_the_new_one() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let superseded = only_fence(&mut state);
        state.register(&terminal, 5, 2);
        state.apply_fetch(&superseded, Err(TerminalError::Unavailable));
        // The fresh registration is still eligible and carries no error.
        assert_eq!(only_fence(&mut state).after_offset, 5);
        assert_eq!(state.take(&terminal, 5).unwrap(), Vec::new());
        assert_eq!(state.metrics().errors, 0);
    }

    #[test]
    fn at_most_one_fetch_per_terminal_is_in_flight_and_extra_rounds_coalesce() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let fence = only_fence(&mut state);
        // The daemon has not answered yet: the next round issues nothing.
        assert_eq!(state.begin_round(), Vec::new());
        assert_eq!(state.metrics().coalesced, 1);
        assert_eq!(state.metrics().fetches, 1);
        state.apply_fetch(&fence, Ok(Vec::new()));
        assert_eq!(state.begin_round().len(), 1);
    }

    #[test]
    fn empty_ok_fetches_leave_the_offset_and_buffer_unchanged() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 7, 1);
        let fence = only_fence(&mut state);
        state.apply_fetch(&fence, Ok(Vec::new()));
        assert_eq!(only_fence(&mut state).after_offset, 7);
        assert_eq!(state.take(&terminal, 7).unwrap(), Vec::new());
        assert_eq!(state.metrics().fetches_with_output, 0);
    }

    #[test]
    fn an_undrained_buffer_overflows_into_one_resync_instead_of_growing() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let mut offset = 0;
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..5 {
            let fence = only_fence(&mut state);
            state.apply_fetch(&fence, Ok(vec![chunk(offset, &block)]));
            offset += block.len() as u64;
        }
        assert_eq!(state.take(&terminal, 0), Err(TerminalError::ResyncRequired));
        assert_eq!(state.metrics().overflow_resyncs, 1);
        // The overflowed terminal stops being fetched until it re-attaches.
        assert_eq!(state.begin_round(), Vec::new());
    }

    #[test]
    fn the_cadence_backs_off_while_idle_and_returns_to_interactive_on_output() {
        let mut state = PumpState::default();
        let terminal = terminal();
        // Nothing registered: the pump waits for a registration.
        assert_eq!(state.next_interval(false), UNREGISTERED_INTERVAL);
        state.register(&terminal, 0, 1);
        // A fresh registration runs the next round immediately.
        assert_eq!(state.next_interval(true), Duration::ZERO);
        let mut intervals = Vec::new();
        for _ in 0..6 {
            let fence = only_fence(&mut state);
            state.apply_fetch(&fence, Ok(Vec::new()));
            intervals.push(state.next_interval(true));
        }
        assert_eq!(
            intervals,
            vec![
                ACTIVE_INTERVAL,
                Duration::from_millis(16),
                Duration::from_millis(32),
                Duration::from_millis(64),
                IDLE_MAX_INTERVAL,
                IDLE_MAX_INTERVAL,
            ],
            "a silent terminal backs off to the bounded idle cadence"
        );
        // Output restores the interactive cadence at once.
        let fence = only_fence(&mut state);
        state.apply_fetch(&fence, Ok(vec![chunk(0, b"hi")]));
        assert_eq!(state.next_interval(true), ACTIVE_INTERVAL);
    }

    #[test]
    fn an_interactive_wake_cancels_the_idle_backoff() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        assert_eq!(state.next_interval(true), Duration::ZERO);
        for _ in 0..4 {
            let fence = only_fence(&mut state);
            state.apply_fetch(&fence, Ok(Vec::new()));
            let _unused = state.next_interval(true);
        }
        // A keystroke is about to produce output: the next round runs at once and
        // the cadence restarts from interactive.
        state.wake();
        assert_eq!(state.next_interval(true), Duration::ZERO);
        let fence = only_fence(&mut state);
        state.apply_fetch(&fence, Ok(Vec::new()));
        assert_eq!(state.next_interval(true), ACTIVE_INTERVAL);
        assert_eq!(state.metrics().wakes, 1);
    }

    #[test]
    fn an_idle_pump_issues_far_fewer_requests_than_the_frame_rate() {
        // One second of wall clock spent entirely idle: sum the cadence until it
        // exceeds a second and count the rounds. The frame loop ticks at ~62.5 Hz,
        // so this is the request rate an idle foreground pane costs.
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let mut elapsed = Duration::ZERO;
        let mut rounds = 0_u32;
        while elapsed < Duration::from_secs(1) {
            let fence = only_fence(&mut state);
            state.apply_fetch(&fence, Ok(Vec::new()));
            elapsed += state.next_interval(true);
            rounds += 1;
        }
        assert!(
            rounds <= 20,
            "an idle foreground terminal costs at most the bounded idle cadence, got {rounds}"
        );
        assert_eq!(u64::from(rounds), state.metrics().fetches);
    }

    #[test]
    fn the_request_rate_does_not_grow_with_the_number_of_panes() {
        // Only the selected foreground terminal is ever registered (#506), so the
        // per-round request count stays at one however many panes exist.
        for panes in [1_usize, 10, 100] {
            let mut state = PumpState::default();
            let terminals = (0..panes).map(|_| terminal()).collect::<Vec<_>>();
            let foreground = terminals.first().expect("at least one pane");
            state.register(foreground, 0, 1);
            let fences = state.begin_round();
            assert_eq!(fences.len(), 1, "{panes} panes still poll one terminal");
            state.apply_fetch(&fences[0], Ok(Vec::new()));
            assert_eq!(state.metrics().fetches, 1);
        }
    }

    #[test]
    fn only_a_degraded_lane_is_summarised_for_the_failure_log() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0, 1);
        let fence = only_fence(&mut state);
        // A focus switch dropping a late result is ordinary, not degradation.
        state.register(&terminal, 4, 1);
        state.apply_fetch(&fence, Ok(vec![chunk(0, b"ab")]));
        assert_eq!(state.metrics().fenced_drops, 1);
        assert_eq!(state.metrics().degradation_summary(), None);

        let fence = only_fence(&mut state);
        state.apply_fetch(&fence, Err(TerminalError::Unavailable));
        let summary = state
            .metrics()
            .degradation_summary()
            .expect("a fetch failure is worth recording");
        assert!(summary.starts_with("foreground poll lane: "), "{summary}");
        assert!(summary.contains("1 errors"), "{summary}");
        assert!(summary.contains("1 fenced drops"), "{summary}");
    }

    #[test]
    fn the_pump_thread_fetches_registered_terminals_into_the_drainable_buffer() {
        let terminal = terminal();
        // The fake fetch returns two bytes the first time it sees each offset,
        // then nothing, so the buffer converges deterministically.
        let (tx, rx) = mpsc::channel();
        let fetch_terminal = terminal.clone();
        let pump = TerminalPollPump::spawn(move |fence| {
            assert!(fence.terminal.fences(&fetch_terminal));
            if fence.after_offset == 0 {
                let _ = tx.send(());
                Ok(vec![chunk(0, b"hi")])
            } else {
                Ok(Vec::new())
            }
        });
        pump.register(&terminal, 0, 1);
        // Wait until the fetch thread has fetched at least once.
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the pump thread fetches a registered terminal");

        let mut drained = Vec::new();
        for _ in 0..200 {
            drained = pump.take(&terminal, 0).unwrap();
            if !drained.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(drained, vec![chunk(0, b"hi")]);
        assert!(pump.metrics().fetches >= 1);

        pump.unregister(&terminal);
    }

    #[test]
    fn the_pump_thread_propagates_a_fetch_error_to_the_drain() {
        let terminal = terminal();
        let pump = TerminalPollPump::spawn(move |_| Err(TerminalError::Exited));
        pump.register(&terminal, 0, 1);
        let mut result = Ok(Vec::new());
        for _ in 0..200 {
            result = pump.take(&terminal, 0);
            if result.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(result, Err(TerminalError::Exited));
    }

    #[test]
    fn waking_the_pump_thread_cuts_an_idle_wait_short() {
        let terminal = terminal();
        let (tx, rx) = mpsc::channel();
        let pump = TerminalPollPump::spawn(move |_| {
            let _ = tx.send(());
            Ok(Vec::new())
        });
        pump.register(&terminal, 0, 1);
        // Let the cadence back off on a silent terminal.
        for _ in 0..6 {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
        while rx.try_recv().is_ok() {}
        pump.wake();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("a wake runs the next round without waiting out the idle cadence");
    }
}
