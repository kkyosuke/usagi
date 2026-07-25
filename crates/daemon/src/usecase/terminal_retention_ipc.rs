//! The daemon's single aggregate retention authority for exited terminal and
//! Agent runtime finals (#526).
//!
//! The generic terminal owner and the Agent owner each keep their own durable
//! records, but the budget those records share is daemon-wide. This service
//! holds the one [`RetentionLedger`] both owners reserve from, commit into, and
//! collect against, so a short-lived-runtime workload cannot grow memory, disk,
//! index scans, or inventory without bound no matter which owner produced it.
//!
//! Like the visibility authority it is shared behind an `Arc<Mutex<_>>`: owners
//! are constructed per connection while the budget must outlive any one of
//! them. The clock is injected so a test can drive the minimum visibility TTL
//! and the age budget deterministically.
//!
//! The ledger is derived state. Durable ownership of a final belongs to the
//! terminal / Agent owner, and a daemon rebuilds this accounting at startup
//! with [`SharedTerminalRetention::import_existing`]; that is why a crash can
//! leave neither a leaked reservation nor a half-applied eviction behind.

#![allow(clippy::must_use_candidate)] // The mutating commands take `&self` through the shared lock; their bool is a report, not a value a caller must consume.

use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use usagi_core::domain::{
    id::TerminalRef,
    terminal_launch::TerminalKind,
    terminal_retention::{
        AdmissionRejection, FinalCommit, FinalLookup, GcReport, RetainedFinal, RetentionBudget,
        RetentionLedger, RetentionMetrics,
    },
    terminal_visibility::TerminalVisibilityState,
};

/// Bytes a durable record is charged when its bounded replay is no longer in
/// memory. After a daemon restart the store still holds the exited record while
/// its output journal does not, so the tombstone is charged for the record
/// alone until it is collected.
pub const RESTORED_FINAL_BYTES: u64 = 512;

/// Reads the wall clock the retention budget ages finals against.
pub trait RetentionClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The production clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRetentionClock;

impl RetentionClock for SystemRetentionClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A cheaply-clonable handle to the daemon's single retention authority.
/// Cloning shares the same budget, reservations, and eviction markers.
#[derive(Clone)]
pub struct SharedTerminalRetention {
    ledger: Arc<Mutex<RetentionLedger>>,
    clock: Arc<dyn RetentionClock>,
}

impl Default for SharedTerminalRetention {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedTerminalRetention {
    /// Creates an authority over the shipped [`RetentionBudget`] and the system
    /// clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_budget(RetentionBudget::default())
    }

    /// Creates an authority over `budget` and the system clock.
    #[must_use]
    pub fn with_budget(budget: RetentionBudget) -> Self {
        Self::with_budget_and_clock(budget, Arc::new(SystemRetentionClock))
    }

    /// Creates an authority over `budget` whose TTL and age budget are measured
    /// with `clock`.
    #[must_use]
    pub fn with_budget_and_clock(budget: RetentionBudget, clock: Arc<dyn RetentionClock>) -> Self {
        Self {
            ledger: Arc::new(Mutex::new(RetentionLedger::new(budget))),
            clock,
        }
    }

    /// Runs `body` under the ledger lock, recovering a poisoned lock so a panic
    /// on one connection never wedges retention for the others.
    fn with<R>(&self, body: impl FnOnce(&mut RetentionLedger) -> R) -> R {
        let mut guard = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        body(&mut guard)
    }

    /// The budget in force.
    #[must_use]
    pub fn budget(&self) -> RetentionBudget {
        self.with(|ledger| ledger.budget())
    }

    /// The clock reading the owners age their finals against.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// The same clock as milliseconds, for the age bound of the durable terminal
    /// input operation ledger (#519).
    ///
    /// Reusing the retention clock keeps the ledger's expiry deterministic under
    /// the fake a test already drives, instead of introducing a second time
    /// source into the terminal owners.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now().timestamp_millis()).unwrap_or(0)
    }

    /// Reserves the worst-case final budget of a runtime about to be spawned.
    ///
    /// # Errors
    ///
    /// Returns the exhausted scope and dimension. The caller must refuse the
    /// launch before spawning rather than deleting a protected final.
    pub fn reserve(&self, terminal: &TerminalRef) -> Result<(), AdmissionRejection> {
        let now = self.clock.now();
        self.with(|ledger| ledger.reserve(now, terminal))
    }

    /// Releases the reservation of a runtime that never produced a final.
    pub fn release(&self, terminal: &TerminalRef) -> bool {
        self.with(|ledger| ledger.release(terminal))
    }

    /// Stores an admitted runtime's final into its reserved capacity. This
    /// never fails, so an exit result is never dropped to respect a cap.
    pub fn commit_final(
        &self,
        terminal: &TerminalRef,
        kind: TerminalKind,
        bytes: u64,
    ) -> FinalCommit {
        let now = self.clock.now();
        self.with(|ledger| ledger.commit_final(terminal, kind, bytes, now))
    }

    /// Re-imports a final that a durable store already holds, without a
    /// reservation. Startup reconciliation and the migration of previously
    /// unbounded records use this.
    pub fn import_existing(&self, record: RetainedFinal) {
        self.with(|ledger| ledger.import_existing(record));
    }

    /// Mirrors a workspace-global visibility raise into the retention class of
    /// the matching final.
    pub fn note_visibility(&self, terminal: &TerminalRef, state: TerminalVisibilityState) -> bool {
        self.with(|ledger| ledger.note_visibility(terminal, state))
    }

    /// Lowers a final's retention priority because a newer runtime in the same
    /// lineage replaced it.
    pub fn mark_superseded(&self, terminal: &TerminalRef) -> bool {
        self.with(|ledger| ledger.mark_superseded(terminal))
    }

    /// Protects a final an owner still needs — one a client is still draining —
    /// from age and pressure collection.
    pub fn set_pinned(&self, terminal: &TerminalRef, pinned: bool) -> bool {
        self.with(|ledger| ledger.set_pinned(terminal, pinned))
    }

    /// Runs one bounded collection pass.
    pub fn collect(&self) -> GcReport {
        let now = self.clock.now();
        self.with(|ledger| ledger.collect(now))
    }

    /// Answers what happened to one exact runtime's final.
    #[must_use]
    pub fn lookup(&self, terminal: &TerminalRef) -> FinalLookup {
        self.with(|ledger| ledger.lookup(terminal))
    }

    /// The operator-visible retention snapshot.
    #[must_use]
    pub fn metrics(&self) -> RetentionMetrics {
        let now = self.clock.now();
        self.with(|ledger| ledger.metrics(now))
    }
}

impl std::fmt::Debug for SharedTerminalRetention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedTerminalRetention")
            .field("metrics", &self.metrics())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use usagi_core::domain::{
        id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId},
        terminal_retention::EvictionReason,
    };

    /// A clock a test steps by hand so the minimum TTL and the age budget are
    /// deterministic.
    #[derive(Debug, Default)]
    pub(crate) struct ManualClock(AtomicI64);

    impl ManualClock {
        pub(crate) fn advance(&self, secs: i64) {
            self.0.fetch_add(secs, Ordering::Relaxed);
        }
    }

    impl RetentionClock for ManualClock {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(1_700_000_000 + self.0.load(Ordering::Relaxed), 0)
                .expect("the fixed test epoch is a valid timestamp")
        }
    }

    /// A small budget with a short TTL, for the daemon-side wiring tests.
    pub(crate) fn small_budget() -> RetentionBudget {
        RetentionBudget {
            max_finals: 3,
            max_bytes: 4096,
            max_finals_per_workspace: 3,
            max_bytes_per_workspace: 4096,
            soft_reserve_finals: 2,
            soft_reserve_bytes: 4096,
            soft_reserve_finals_per_workspace: 2,
            soft_reserve_bytes_per_workspace: 4096,
            min_visibility_ttl_secs: 10,
            max_final_age_secs: 100,
            worst_case_final_bytes: 1024,
            max_gc_batch: 8,
            max_eviction_markers: 8,
        }
    }

    /// A shared authority over [`small_budget`] and a hand-driven clock.
    pub(crate) fn manual_retention() -> (SharedTerminalRetention, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::default());
        let retention =
            SharedTerminalRetention::with_budget_and_clock(small_budget(), clock.clone());
        (retention, clock)
    }

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn the_authority_converges_across_clones() {
        let (a, clock) = manual_retention();
        let b = a.clone();
        let first = terminal();
        a.reserve(&first).unwrap();
        // The clone speaks for the same budget, so it sees the reservation.
        assert_eq!(b.metrics().reserved_finals, 1);
        b.commit_final(&first, TerminalKind::Terminal, 64);
        assert_eq!(a.metrics().retained_finals, 1);
        assert_eq!(a.budget(), small_budget());

        // Past the age budget, so only the pin can keep this final.
        clock.advance(120);
        assert_eq!(a.now(), b.now());
        assert!(b.note_visibility(&first, TerminalVisibilityState::Dismissed));
        assert!(b.mark_superseded(&first));
        assert!(a.set_pinned(&first, true));
        // A pinned final is not collected even past the TTL.
        assert!(a.collect().is_empty());
        assert!(a.set_pinned(&first, false));
        assert_eq!(b.collect().evicted.len(), 1);
        assert_eq!(
            a.lookup(&first).marker().map(|marker| marker.reason),
            Some(EvictionReason::AgeExpired)
        );
        assert!(!a.release(&first));
    }

    #[test]
    fn a_saturated_budget_rejects_a_launch_before_spawn() {
        let (retention, _clock) = manual_retention();
        let mut admitted = Vec::new();
        for _ in 0..3 {
            let terminal = terminal();
            retention.reserve(&terminal).unwrap();
            retention.commit_final(&terminal, TerminalKind::Agent, 1024);
            admitted.push(terminal);
        }
        let rejected = terminal();
        assert!(retention.reserve(&rejected).is_err());
        // Nothing inside the TTL was deleted to make room.
        assert_eq!(retention.metrics().retained_finals, 3);
        assert_eq!(retention.metrics().admission_rejections, 1);
        assert_eq!(retention.lookup(&rejected), FinalLookup::Unknown);
        assert!(retention.lookup(&admitted[0]).retained().is_some());
    }

    #[test]
    fn an_imported_final_is_collected_like_a_committed_one() {
        let (retention, clock) = manual_retention();
        let restored = terminal();
        retention.import_existing(RetainedFinal::new(
            restored.clone(),
            TerminalKind::Terminal,
            32,
            retention.now(),
        ));
        assert_eq!(retention.metrics().retained_finals, 1);
        clock.advance(200);
        assert_eq!(retention.collect().evicted.len(), 1);
        assert!(retention.lookup(&restored).marker().is_some());
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let (retention, _clock) = manual_retention();
        let poisoned = retention.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.ledger.lock().unwrap();
            panic!("poison the retention lock");
        }));
        assert!(retention.ledger.is_poisoned());
        assert_eq!(retention.metrics().retained_finals, 0);
        // The Debug projection is metrics only: no terminal identity leaks.
        let rendered = format!("{retention:?}");
        assert!(rendered.contains("SharedTerminalRetention"));
        assert!(rendered.contains("retained_finals"));
    }

    #[test]
    fn the_default_authority_uses_the_shipped_budget_and_the_system_clock() {
        let retention = SharedTerminalRetention::default();
        assert_eq!(retention.budget(), RetentionBudget::default());
        // A default authority still answers; the system clock is only read here.
        assert_eq!(retention.metrics().retained_finals, 0);
        assert!(SystemRetentionClock.now() > DateTime::from_timestamp(0, 0).unwrap());
    }
}
