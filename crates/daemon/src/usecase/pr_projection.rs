//! Bounded hand-off from the PTY observers to PR projection.
//!
//! PR detection is deliberately *not* part of accepting output. The observer
//! commits a chunk to the terminal journal under the runtime lock, releases that
//! lock, and then submits the same bytes here. A submit never blocks and never
//! performs IO, so the time a runtime lock is held stops depending on how much a
//! child writes.
//!
//! The queue is bounded by retained bytes. When it is full the incoming chunk is
//! dropped and a [`PrProjection::Gap`] takes its place *in order*, because
//! concatenating bytes across dropped bytes could synthesize a PR URL that never
//! appeared in the output.
//! Entries have a separate bound, including gaps and terminal exits. Exhausting
//! it discards pending work and queues an ordered reset of all parser carries.
//! This may lose PR discoveries during overload, but never joins discontinuous
//! output or retains an unbounded backlog of zero-byte metadata.

use std::{
    collections::VecDeque,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use usagi_core::domain::id::{SessionId, TerminalId};

/// How many un-projected bytes the queue retains before it drops.
///
/// At the 4 KiB granularity a PTY read uses, this is roughly a thousand chunks of
/// slack — far more than the projector needs to absorb a burst, while still a
/// hard ceiling on memory.
pub const QUEUE_BYTES_MAX: usize = 4 * 1024 * 1024;

/// Maximum queued output, gap, reset and terminal-exit entries combined.
pub const QUEUE_ITEMS_MAX: usize = 4096;

/// How large a queued chunk may grow by merging its successors.
///
/// Merging keeps the item count proportional to bytes rather than to chunk count,
/// so a child writing one byte at a time cannot inflate the queue into millions
/// of entries. The cap keeps any single merge a bounded copy.
pub const MERGE_BYTES_MAX: usize = 64 * 1024;

static DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static COALESCED_BYTES: AtomicU64 = AtomicU64::new(0);
static GAPS: AtomicU64 = AtomicU64::new(0);

/// Process-local counters for the deferred PR projection hand-off.
///
/// Byte and event counts only: never output content and never a terminal or
/// session identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrProjectionCounters {
    /// Committed bytes never scanned for PRs because the queue was full.
    pub dropped_bytes: u64,
    /// Bytes merged into an already queued chunk instead of a new entry.
    pub coalesced_bytes: u64,
    /// Discontinuities recorded so a scan never joins across dropped bytes.
    pub gaps: u64,
}

#[must_use]
pub fn pr_projection_counters() -> PrProjectionCounters {
    PrProjectionCounters {
        dropped_bytes: DROPPED_BYTES.load(Ordering::Relaxed),
        coalesced_bytes: COALESCED_BYTES.load(Ordering::Relaxed),
        gaps: GAPS.load(Ordering::Relaxed),
    }
}

/// One in-order unit of deferred projection work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrProjection {
    /// Committed output to scan.
    Output {
        terminal: TerminalId,
        session: Option<SessionId>,
        bytes: Vec<u8>,
    },
    /// Bytes were dropped before whatever follows for this terminal.
    Gap { terminal: TerminalId },
    /// Pending work was discarded under metadata pressure. Forget every partial
    /// URL before processing subsequent output; persisted discoveries remain.
    Reset,
    /// The terminal exited: flush its carry and reclaim it.
    Closed {
        terminal: TerminalId,
        session: Option<SessionId>,
    },
}

#[derive(Default)]
struct State {
    items: VecDeque<PrProjection>,
    bytes: usize,
    closed: bool,
}

/// A bounded queue whose producer never blocks.
pub struct PrProjectionQueue {
    state: Mutex<State>,
    ready: Condvar,
    byte_cap: usize,
    merge_cap: usize,
    item_cap: usize,
}

impl Default for PrProjectionQueue {
    fn default() -> Self {
        Self::with_limits(QUEUE_BYTES_MAX, MERGE_BYTES_MAX, QUEUE_ITEMS_MAX)
    }
}

impl PrProjectionQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    fn with_limits(byte_cap: usize, merge_cap: usize, item_cap: usize) -> Self {
        assert!(item_cap >= 2, "a reset and its successor need two entries");
        Self {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            byte_cap,
            merge_cap,
            item_cap,
        }
    }

    /// Runs `apply` against the queue state.
    ///
    /// A poisoned lock reports "not accepted" instead of panicking, and does so
    /// on the same line as the lock so this failure mode needs no branch of its
    /// own to leave unexercised.
    fn with_state(&self, apply: impl FnOnce(&mut State) -> bool) -> bool {
        self.state.lock().is_ok_and(|mut state| apply(&mut state))
    }

    /// Submits committed output. Returns whether it was queued rather than
    /// dropped, so a caller can assert the bound in a test.
    pub fn submit_output(
        &self,
        terminal: TerminalId,
        session: Option<SessionId>,
        bytes: Vec<u8>,
    ) -> bool {
        let queued = self.with_state(|state| {
            if state.closed {
                return false;
            }
            if state.bytes.saturating_add(bytes.len()) > self.byte_cap {
                DROPPED_BYTES.fetch_add(
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                self.push_gap(state, terminal);
                return false;
            }
            // Merging into the pending chunk keeps the entry count proportional
            // to bytes. The merged bytes are contiguous, so this changes nothing
            // a scan can observe.
            if let Some(PrProjection::Output {
                terminal: pending,
                session: pending_session,
                bytes: pending_bytes,
            }) = state.items.back_mut()
                && *pending == terminal
                && *pending_session == session
                && pending_bytes.len() < self.merge_cap
            {
                state.bytes += bytes.len();
                COALESCED_BYTES.fetch_add(
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                pending_bytes.extend_from_slice(&bytes);
                return true;
            }
            self.reserve_entry(state);
            state.bytes += bytes.len();
            state.items.push_back(PrProjection::Output {
                terminal,
                session,
                bytes,
            });
            true
        });
        // A drop is woken too: the gap it recorded is itself work to apply.
        self.ready.notify_one();
        queued
    }

    /// Records that bytes for `terminal` were dropped outside this queue.
    pub fn submit_gap(&self, terminal: TerminalId) {
        if let Ok(mut state) = self.state.lock()
            && !state.closed
        {
            self.push_gap(&mut state, terminal);
            self.ready.notify_one();
        }
    }

    /// Records an exited terminal so its carry is flushed and reclaimed.
    pub fn submit_closed(&self, terminal: TerminalId, session: Option<SessionId>) {
        if let Ok(mut state) = self.state.lock()
            && !state.closed
        {
            self.reserve_entry(&mut state);
            state
                .items
                .push_back(PrProjection::Closed { terminal, session });
            self.ready.notify_one();
        }
    }

    /// Appends a gap unless the newest entry is already this terminal's gap.
    fn push_gap(&self, state: &mut State, terminal: TerminalId) {
        if matches!(
            state.items.back(),
            Some(PrProjection::Gap { terminal: pending }) if *pending == terminal
        ) {
            return;
        }
        self.reserve_entry(state);
        GAPS.fetch_add(1, Ordering::Relaxed);
        state.items.push_back(PrProjection::Gap { terminal });
    }

    /// Keeps the reset before new work, including an exit that could otherwise
    /// flush a now-truncated URL. A single consumer also processes any item it
    /// already received before observing this reset.
    fn reserve_entry(&self, state: &mut State) {
        if state.items.len() < self.item_cap {
            return;
        }
        DROPPED_BYTES.fetch_add(
            u64::try_from(state.bytes).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        GAPS.fetch_add(1, Ordering::Relaxed);
        state.items.clear();
        state.bytes = 0;
        state.items.push_back(PrProjection::Reset);
    }

    /// Blocks until work is available. Returns `None` once the queue is closed
    /// and drained, which is how the worker thread learns to exit; there is no
    /// timer and no polling.
    #[must_use]
    pub fn recv(&self) -> Option<PrProjection> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(item) = state.items.pop_front() {
                if let PrProjection::Output { bytes, .. } = &item {
                    state.bytes -= bytes.len();
                }
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    /// Wakes the worker and stops accepting work. Idempotent.
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.ready.notify_all();
    }

    /// Retained un-projected bytes. Tests assert the bound.
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.state.lock().map_or(0, |state| state.bytes)
    }

    /// Pending entry count, including zero-byte metadata.
    #[must_use]
    pub fn queued_items(&self) -> usize {
        self.state.lock().map_or(0, |state| state.items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn queue() -> PrProjectionQueue {
        PrProjectionQueue::with_limits(64, 16, QUEUE_ITEMS_MAX)
    }

    #[test]
    fn submits_and_receives_in_order() {
        let queue = queue();
        let terminal = TerminalId::new();
        let session = SessionId::new();
        assert!(queue.submit_output(terminal, Some(session), b"first ".to_vec()));
        queue.submit_closed(terminal, Some(session));
        assert_eq!(queue.queued_items(), 2);
        assert_eq!(
            queue.recv(),
            Some(PrProjection::Output {
                terminal,
                session: Some(session),
                bytes: b"first ".to_vec()
            })
        );
        assert_eq!(queue.queued_bytes(), 0, "receiving releases the bytes");
        assert_eq!(
            queue.recv(),
            Some(PrProjection::Closed {
                terminal,
                session: Some(session)
            })
        );
        queue.close();
        assert_eq!(queue.recv(), None);
    }

    #[test]
    fn merges_adjacent_chunks_for_the_same_terminal_up_to_the_merge_cap() {
        let queue = queue();
        let terminal = TerminalId::new();
        let session = SessionId::new();
        for _ in 0..4 {
            assert!(queue.submit_output(terminal, Some(session), b"abcde".to_vec()));
        }
        // The cap is tested before merging, so a merge may finish past it: 5, 10,
        // 15 and 20 all land in one entry.
        assert_eq!(queue.queued_items(), 1);
        assert_eq!(queue.queued_bytes(), 20);
        // The next chunk finds the pending entry past the cap and starts a new one.
        assert!(queue.submit_output(terminal, Some(session), b"fg".to_vec()));
        assert_eq!(queue.queued_items(), 2);
        // A different terminal never merges into another terminal's chunk.
        assert!(queue.submit_output(TerminalId::new(), Some(session), b"xy".to_vec()));
        assert_eq!(queue.queued_items(), 3);
        // Neither does a different session on the same terminal.
        assert!(queue.submit_output(terminal, None, b"z".to_vec()));
        assert_eq!(queue.queued_items(), 4);
        assert_eq!(queue.queued_bytes(), 25);
    }

    #[test]
    fn a_full_queue_drops_bytes_and_records_a_gap_in_order() {
        let queue = PrProjectionQueue::with_limits(8, 0, QUEUE_ITEMS_MAX);
        let terminal = TerminalId::new();
        let session = SessionId::new();
        assert!(queue.submit_output(terminal, Some(session), b"12345678".to_vec()));
        // Over the cap: dropped, and a gap stands in for the missing bytes.
        assert!(!queue.submit_output(terminal, Some(session), b"9".to_vec()));
        // Consecutive drops for the same terminal collapse into one gap.
        assert!(!queue.submit_output(terminal, Some(session), b"0".to_vec()));
        assert_eq!(queue.queued_items(), 2);
        assert_eq!(queue.queued_bytes(), 8, "dropped bytes are not retained");
        assert!(matches!(queue.recv(), Some(PrProjection::Output { .. })));
        assert_eq!(queue.recv(), Some(PrProjection::Gap { terminal }));
    }

    #[test]
    fn an_explicit_gap_also_collapses_and_a_closed_queue_accepts_nothing() {
        let queue = queue();
        let terminal = TerminalId::new();
        queue.submit_gap(terminal);
        queue.submit_gap(terminal);
        assert_eq!(queue.queued_items(), 1);
        queue.submit_gap(TerminalId::new());
        assert_eq!(queue.queued_items(), 2);
        queue.close();
        assert!(!queue.submit_output(terminal, None, b"x".to_vec()));
        queue.submit_gap(terminal);
        queue.submit_closed(terminal, None);
        assert_eq!(
            queue.queued_items(),
            2,
            "a closed queue accepts no more work"
        );
    }

    #[test]
    fn recv_blocks_until_a_submit_wakes_it_rather_than_polling() {
        let queue = Arc::new(queue());
        let terminal = TerminalId::new();
        let waiter = Arc::clone(&queue);
        let handle = std::thread::spawn(move || waiter.recv());
        // The worker is parked on the condvar; the submit is what wakes it. The
        // queue is empty, so this submit cannot be refused.
        assert!(queue.submit_output(terminal, None, b"wake".to_vec()));
        assert!(matches!(
            handle.join().unwrap(),
            Some(PrProjection::Output { .. })
        ));
    }

    #[test]
    fn close_wakes_a_parked_worker() {
        let queue = Arc::new(queue());
        let waiter = Arc::clone(&queue);
        let handle = std::thread::spawn(move || waiter.recv());
        queue.close();
        assert_eq!(handle.join().unwrap(), None);
    }

    #[test]
    fn counters_are_byte_and_event_counts_only() {
        let before = pr_projection_counters();
        let queue = PrProjectionQueue::with_limits(4, 4, QUEUE_ITEMS_MAX);
        let terminal = TerminalId::new();
        assert!(queue.submit_output(terminal, None, b"ab".to_vec()));
        assert!(queue.submit_output(terminal, None, b"cd".to_vec()));
        assert!(!queue.submit_output(terminal, None, b"efgh".to_vec()));
        let after = pr_projection_counters();
        assert!(after.dropped_bytes >= before.dropped_bytes + 4);
        assert!(after.coalesced_bytes >= before.coalesced_bytes + 2);
        assert!(after.gaps > before.gaps);
    }

    #[test]
    fn alternating_overflow_and_terminal_churn_bound_all_metadata() {
        let queue = PrProjectionQueue::new();
        let terminals = [TerminalId::new(), TerminalId::new()];
        for _ in 0..(QUEUE_BYTES_MAX / 4096) {
            assert!(queue.submit_output(terminals[0], None, vec![b'x'; 4096]));
        }
        let mut dropped = 0;
        for index in 0..10_000 {
            dropped +=
                usize::from(!queue.submit_output(terminals[index % 2], None, vec![b'x'; 4096]));
            assert!(queue.queued_bytes() <= QUEUE_BYTES_MAX);
            assert!(queue.queued_items() <= QUEUE_ITEMS_MAX);
        }
        assert!(dropped > 0);
        for _ in 0..10_000 {
            queue.submit_closed(TerminalId::new(), None);
            assert!(queue.queued_items() <= QUEUE_ITEMS_MAX);
        }
        assert_eq!(queue.queued_bytes(), 0);
        assert_eq!(queue.recv(), Some(PrProjection::Reset));
    }

    #[test]
    fn metadata_reset_precedes_new_output_and_exit() {
        let queue = PrProjectionQueue::with_limits(64, 0, 2);
        let terminal = TerminalId::new();
        assert!(queue.submit_output(terminal, None, b"discarded".to_vec()));
        queue.submit_gap(terminal);
        assert!(queue.submit_output(terminal, None, b"new".to_vec()));
        assert_eq!(queue.queued_bytes(), 3);
        assert_eq!(queue.recv(), Some(PrProjection::Reset));
        assert!(
            matches!(queue.recv(), Some(PrProjection::Output { bytes, .. }) if bytes == b"new")
        );

        queue.submit_gap(terminal);
        queue.submit_gap(TerminalId::new());
        queue.submit_closed(terminal, None);
        queue.close();
        assert_eq!(queue.recv(), Some(PrProjection::Reset));
        assert_eq!(
            queue.recv(),
            Some(PrProjection::Closed {
                terminal,
                session: None
            })
        );
        assert_eq!(queue.recv(), None);
    }

    #[test]
    fn gaps_separated_by_output_remain_distinct_and_reset_is_ordered() {
        let queue = PrProjectionQueue::with_limits(1, 0, 4);
        let terminal = TerminalId::new();
        queue.submit_gap(terminal);
        assert!(queue.submit_output(terminal, None, b"a".to_vec()));
        assert!(!queue.submit_output(terminal, None, b"b".to_vec()));
        assert_eq!(queue.recv(), Some(PrProjection::Gap { terminal }));
        assert!(matches!(queue.recv(), Some(PrProjection::Output { .. })));
        assert_eq!(queue.recv(), Some(PrProjection::Gap { terminal }));
        for _ in 0..5 {
            queue.submit_gap(TerminalId::new());
        }
        assert_eq!(queue.queued_items(), 2);
        assert_eq!(queue.recv(), Some(PrProjection::Reset));
        assert!(matches!(queue.recv(), Some(PrProjection::Gap { .. })));
    }

    #[test]
    #[should_panic(expected = "a reset and its successor need two entries")]
    fn metadata_limit_reserves_room_for_the_reset() {
        let _ = PrProjectionQueue::with_limits(1, 0, 1);
    }
}
