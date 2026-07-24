//! Background terminal output pump.
//!
//! The interactive TUI renders on a single thread that, every frame, asks the
//! daemon for each live terminal's new output. Doing that fetch inline means a
//! momentarily busy daemon stalls the whole render/input loop. This pump moves
//! the `Resume` fetch onto a background thread: it continuously reads registered
//! terminals into per-terminal read-ahead buffers, and the render thread drains
//! those buffers without ever blocking on the daemon.
//!
//! The pure buffering state ([`PumpState`]) is unit-tested directly; the thread
//! wrapper ([`TerminalPollPump`]) is exercised with an in-process fake fetch so
//! the real daemon IPC (injected as the `fetch` closure by the composition root)
//! is the only part left as real IO.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use usagi_core::domain::id::TerminalRef;
use usagi_tui::usecase::application::terminal_session::{TerminalChunk, TerminalError};

/// How long the background thread sleeps between fetch rounds while at least one
/// terminal is registered. Kept below the render frame tick so a drained buffer
/// refills before the next frame.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// How long the background thread sleeps when nothing is registered, avoiding a
/// busy spin while no terminal is attached.
const IDLE_INTERVAL: Duration = Duration::from_millis(25);

/// One registered terminal's read-ahead buffer.
struct TerminalBuffer {
    terminal: TerminalRef,
    /// Offset the next background fetch resumes from.
    fetch_offset: u64,
    /// Contiguous chunks fetched but not yet drained by the render thread.
    pending: VecDeque<TerminalChunk>,
    /// A fetch failure awaiting delivery once `pending` is drained.
    error: Option<TerminalError>,
    /// Set after a fetch error so the background thread stops fetching this
    /// terminal until the render thread re-registers it (on reattach).
    stalled: bool,
}

/// Pure buffering state shared between the render thread and the background
/// fetch thread. Every method is deterministic and lock-free; the mutex lives in
/// [`TerminalPollPump`].
#[derive(Default)]
pub struct PumpState {
    terminals: Vec<TerminalBuffer>,
}

impl PumpState {
    /// Registers a terminal to fetch from `offset`, or resets an existing one to
    /// that offset. Reattach (after a reconnect or resync) re-registers with the
    /// snapshot's output offset, which clears any buffered output and error and
    /// resumes fetching.
    fn register(&mut self, terminal: &TerminalRef, offset: u64) {
        if let Some(buffer) = self
            .terminals
            .iter_mut()
            .find(|buffer| buffer.terminal.fences(terminal))
        {
            buffer.fetch_offset = offset;
            buffer.pending.clear();
            buffer.error = None;
            buffer.stalled = false;
        } else {
            self.terminals.push(TerminalBuffer {
                terminal: terminal.clone(),
                fetch_offset: offset,
                pending: VecDeque::new(),
                error: None,
                stalled: false,
            });
        }
    }

    /// Stops tracking a terminal; a later fetch result for it is discarded.
    fn unregister(&mut self, terminal: &TerminalRef) {
        self.terminals
            .retain(|buffer| !buffer.terminal.fences(terminal));
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
                buffer.pending.pop_front();
                continue;
            }
            chunks.push(buffer.pending.pop_front().expect("front was just observed"));
        }
        if chunks.is_empty()
            && let Some(error) = buffer.error
        {
            return Err(error);
        }
        Ok(chunks)
    }

    /// The terminals eligible for a background fetch and the offset each resumes
    /// from. Stalled terminals (failed until re-registered) are skipped.
    fn fetch_targets(&self) -> Vec<(TerminalRef, u64)> {
        self.terminals
            .iter()
            .filter(|buffer| !buffer.stalled)
            .map(|buffer| (buffer.terminal.clone(), buffer.fetch_offset))
            .collect()
    }

    /// Records one background fetch outcome. Output advances the fetch offset and
    /// appends to the buffer; an error is retained and stalls further fetches. A
    /// result for a terminal unregistered mid-fetch is dropped.
    fn apply_fetch(
        &mut self,
        terminal: &TerminalRef,
        result: Result<Vec<TerminalChunk>, TerminalError>,
    ) {
        let Some(buffer) = self
            .terminals
            .iter_mut()
            .find(|buffer| buffer.terminal.fences(terminal))
        else {
            return;
        };
        match result {
            Ok(chunks) => {
                if let Some(last) = chunks.last() {
                    buffer.fetch_offset = last.end_offset;
                }
                buffer.pending.extend(chunks);
            }
            Err(error) => {
                buffer.error = Some(error);
                buffer.stalled = true;
            }
        }
    }
}

/// Runs the `fetch` closure against every registered terminal, updating the
/// shared state. Returns whether any terminal was fetched, so the caller can
/// pick a shorter sleep while work is flowing. The state lock is never held
/// across a `fetch` call, so the render thread's drain never waits on IO.
fn run_round<F>(state: &Mutex<PumpState>, fetch: &mut F) -> bool
where
    F: FnMut(&TerminalRef, u64) -> Result<Vec<TerminalChunk>, TerminalError>,
{
    let targets = lock(state).fetch_targets();
    let worked = !targets.is_empty();
    for (terminal, offset) in targets {
        let result = fetch(&terminal, offset);
        lock(state).apply_fetch(&terminal, result);
    }
    worked
}

/// Locks the pump state, recovering a poisoned lock. The buffered output is not
/// safety-critical, so a render-thread panic while holding the lock must not
/// wedge the background thread; the recovered state is internally consistent.
fn lock(state: &Mutex<PumpState>) -> std::sync::MutexGuard<'_, PumpState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Background terminal output pump. Owns the fetch thread and the shared buffer;
/// the render thread registers/drains through it without blocking on IO.
pub struct TerminalPollPump {
    state: Arc<Mutex<PumpState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TerminalPollPump {
    /// Spawns the background thread. `fetch` performs one `Resume` fetch for a
    /// terminal at an offset; the composition root injects the real daemon IPC,
    /// while tests inject an in-process fake.
    pub fn spawn<F>(mut fetch: F) -> Self
    where
        F: FnMut(&TerminalRef, u64) -> Result<Vec<TerminalChunk>, TerminalError> + Send + 'static,
    {
        let state = Arc::new(Mutex::new(PumpState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let worked = run_round(&thread_state, &mut fetch);
                std::thread::sleep(if worked { POLL_INTERVAL } else { IDLE_INTERVAL });
            }
        });
        Self {
            state,
            stop,
            handle: Some(handle),
        }
    }

    /// Registers or resets a terminal (see [`PumpState::register`]).
    pub fn register(&self, terminal: &TerminalRef, offset: u64) {
        lock(&self.state).register(terminal, offset);
    }

    /// Stops tracking a terminal (see [`PumpState::unregister`]).
    pub fn unregister(&self, terminal: &TerminalRef) {
        lock(&self.state).unregister(terminal);
    }

    /// Drains buffered output for the render thread (see [`PumpState::take`]).
    pub fn take(
        &self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        lock(&self.state).take(terminal, after_offset)
    }
}

impl Drop for TerminalPollPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
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

    #[test]
    fn take_drains_buffered_output_in_order_and_advances_the_fetch_offset() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 10);
        assert_eq!(state.fetch_targets(), vec![(terminal.clone(), 10)]);

        state.apply_fetch(&terminal, Ok(vec![chunk(10, b"ab"), chunk(12, b"cd")]));
        // The next fetch resumes after the last chunk.
        assert_eq!(state.fetch_targets(), vec![(terminal.clone(), 14)]);

        let drained = state.take(&terminal, 10).unwrap();
        assert_eq!(drained, vec![chunk(10, b"ab"), chunk(12, b"cd")]);
        // Nothing buffered now.
        assert_eq!(state.take(&terminal, 14).unwrap(), Vec::new());
    }

    #[test]
    fn take_skips_output_already_consumed_by_the_render_thread() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0);
        state.apply_fetch(&terminal, Ok(vec![chunk(0, b"ab"), chunk(2, b"cd")]));
        // The render thread already applied up to offset 2, so the first chunk is
        // dropped and only the newer one is returned.
        assert_eq!(state.take(&terminal, 2).unwrap(), vec![chunk(2, b"cd")]);
    }

    #[test]
    fn an_error_surfaces_only_after_buffered_output_is_drained_and_stalls_fetching() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0);
        state.apply_fetch(&terminal, Ok(vec![chunk(0, b"ab")]));
        state.apply_fetch(&terminal, Err(TerminalError::ResyncRequired));
        // A stalled terminal is not fetched again until it is re-registered.
        assert_eq!(state.fetch_targets(), Vec::new());
        // Buffered output is delivered first.
        assert_eq!(state.take(&terminal, 0).unwrap(), vec![chunk(0, b"ab")]);
        // Then the error, repeatedly, until reattach.
        assert_eq!(state.take(&terminal, 2), Err(TerminalError::ResyncRequired));
        assert_eq!(state.take(&terminal, 2), Err(TerminalError::ResyncRequired));
    }

    #[test]
    fn reregister_resets_offset_buffer_error_and_resumes_fetching() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0);
        state.apply_fetch(&terminal, Ok(vec![chunk(0, b"ab")]));
        state.apply_fetch(&terminal, Err(TerminalError::Unavailable));
        // Reattach at a fresh snapshot offset.
        state.register(&terminal, 100);
        assert_eq!(state.fetch_targets(), vec![(terminal.clone(), 100)]);
        assert_eq!(state.take(&terminal, 100).unwrap(), Vec::new());
    }

    #[test]
    fn unregister_drops_the_terminal_and_a_late_fetch_result_is_ignored() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 0);
        state.unregister(&terminal);
        assert_eq!(state.fetch_targets(), Vec::new());
        // A fetch result that raced with unregistration is dropped silently.
        state.apply_fetch(&terminal, Ok(vec![chunk(0, b"ab")]));
        assert_eq!(state.take(&terminal, 0).unwrap(), Vec::new());
    }

    #[test]
    fn empty_ok_fetches_leave_the_offset_and_buffer_unchanged() {
        let mut state = PumpState::default();
        let terminal = terminal();
        state.register(&terminal, 7);
        state.apply_fetch(&terminal, Ok(Vec::new()));
        assert_eq!(state.fetch_targets(), vec![(terminal.clone(), 7)]);
        assert_eq!(state.take(&terminal, 7).unwrap(), Vec::new());
    }

    #[test]
    fn the_pump_thread_fetches_registered_terminals_into_the_drainable_buffer() {
        let terminal = terminal();
        // The fake fetch returns two bytes the first time it sees each offset,
        // then nothing, so the buffer converges deterministically.
        let (tx, rx) = mpsc::channel();
        let fetch_terminal = terminal.clone();
        let pump = TerminalPollPump::spawn(move |candidate, offset| {
            assert!(candidate.fences(&fetch_terminal));
            if offset == 0 {
                let _ = tx.send(());
                Ok(vec![chunk(0, b"hi")])
            } else {
                Ok(Vec::new())
            }
        });
        pump.register(&terminal, 0);
        // Wait until the background thread has fetched at least once.
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

        pump.unregister(&terminal);
    }

    #[test]
    fn the_pump_thread_propagates_a_fetch_error_to_the_drain() {
        let terminal = terminal();
        let pump = TerminalPollPump::spawn(move |_, _| Err(TerminalError::Exited));
        pump.register(&terminal, 0);
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
}
