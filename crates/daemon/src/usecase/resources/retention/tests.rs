//! The ledger stays bounded, and an expired id is never admitted as fresh.

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use super::{
    GcPhase, GcPlan, GcReport, LogicalClock, RetentionLimits, admission_guard, apply_phase,
    collect_garbage, plan_gc, serialized_bytes,
};
use crate::usecase::resources::allocator::{
    AllocatorDocument, ClaimState, ExpiryClass, LaunchFailure, OperationOutcome, ResourceAllocator,
    ResourceKind,
};
use crate::usecase::resources::fixture::{
    FakeClock, MemoryFile, SharedBytes, allocator, policy, terminal,
};
use crate::usecase::resources::{CasDocument, ResourceError};

/// Operation ids in issue order, so watermark comparisons are deterministic.
fn ordered(index: u8) -> OperationId {
    OperationId::parse(&format!("018f0000-0000-7000-8000-0000000000{index:02x}")).unwrap()
}

fn limits() -> RetentionLimits {
    RetentionLimits::new(3, 1 << 20, 100, 10, 20)
}

struct Ledger {
    document: AllocatorDocument,
    owner: DaemonGeneration,
}

impl Ledger {
    fn new() -> Self {
        Self {
            document: AllocatorDocument::default(),
            owner: DaemonGeneration::new(),
        }
    }

    /// One launch that reached a released, collectable final at `sealed`.
    fn completed(&mut self, operation: &OperationId, sealed: u64) -> TerminalRef {
        let resource = self.reserve(operation);
        self.document.mark_spawned(operation, sealed).unwrap();
        self.document
            .consume_exit(self.owner, &resource, 1)
            .unwrap();
        resource
    }

    fn reserve(&mut self, operation: &OperationId) -> TerminalRef {
        let resource = terminal(self.owner);
        self.document
            .reserve(
                operation,
                "digest",
                ResourceKind::Terminal,
                self.owner,
                &resource,
                policy(8, 8),
            )
            .unwrap();
        resource
    }
}

#[test]
fn nothing_is_collected_while_the_ledger_is_small_and_young() {
    let mut ledger = Ledger::new();
    ledger.completed(&ordered(1), 0);
    let plan = plan_gc(&ledger.document, &limits(), 5);
    assert_eq!(plan, GcPlan::default());
    assert!(serialized_bytes(&ledger.document) > 0);
    admission_guard(&ledger.document, &limits(), 5).unwrap();
}

#[test]
fn the_minimum_window_replays_the_full_exact_outcome_and_only_then_expires_it() {
    let mut ledger = Ledger::new();
    let operation = ordered(2);
    let resource = ledger.completed(&operation, 0);
    let limits = limits();

    // One tick before the window closes the full record is still there.
    assert!(plan_gc(&ledger.document, &limits, 9).phases.is_empty());
    let replay = ledger
        .document
        .admit(&operation, "digest", ResourceKind::Terminal, policy(8, 8))
        .unwrap();
    assert!(matches!(
        replay,
        crate::usecase::resources::allocator::Admission::Replay {
            outcome: OperationOutcome::Spawned,
            ..
        }
    ));

    // At the window boundary the age rule alone makes it collectable.
    let plan = plan_gc(&ledger.document, &limits, 100);
    assert_eq!(plan.phases[0], GcPhase::Evict(vec![operation]));
    apply_phase(&mut ledger.document, &plan.phases[0], &limits, 100).unwrap();

    let tombstone = ledger.document.tombstone(&operation).unwrap();
    assert_eq!(tombstone.class, ExpiryClass::Spawned);
    assert_eq!(tombstone.digest, "digest");
    assert_eq!(tombstone.cutoff, 100);
    assert!(ledger.document.operation(&operation).is_none());
    assert!(
        ledger.document.claim(&resource).is_none(),
        "the released claim and its consumed event go with the record"
    );
    assert!(ledger.document.consumed.is_empty());
    assert_eq!(
        ledger
            .document
            .admit(&operation, "digest", ResourceKind::Terminal, policy(8, 8)),
        Err(ResourceError::OperationExpired)
    );
    ledger.document.validate().unwrap();
}

#[test]
fn a_watermark_covers_only_ids_no_live_operation_sits_at_or_below() {
    let mut ledger = Ledger::new();
    let old = ordered(3);
    let live = ordered(4);
    ledger.completed(&old, 0);
    ledger.reserve(&live);
    let limits = limits();

    let plan = plan_gc(&ledger.document, &limits, 100);
    assert_eq!(plan.phases[0], GcPhase::Evict(vec![old]));
    assert_eq!(plan.phases[1], GcPhase::AdvanceWatermark(old));
    for phase in &plan.phases {
        apply_phase(&mut ledger.document, phase, &limits, 100).unwrap();
    }
    assert_eq!(ledger.document.watermark, Some(old));
    assert!(ledger.document.is_expired(&old));
    assert!(
        !ledger.document.is_expired(&live),
        "a still-live operation is never declared expired"
    );

    // A newer completed operation cannot pull the watermark past the live one.
    let newer = ordered(9);
    ledger.completed(&newer, 100);
    let plan = plan_gc(&ledger.document, &limits, 200);
    assert!(
        plan.phases
            .iter()
            .all(|phase| *phase != GcPhase::AdvanceWatermark(newer)),
        "the watermark stops below the live operation"
    );
    ledger.document.validate().unwrap();
}

#[test]
fn a_tombstone_is_compacted_only_after_the_horizon_and_stays_expired_afterwards() {
    let mut ledger = Ledger::new();
    let operation = ordered(5);
    ledger.completed(&operation, 0);
    let limits = limits();
    for phase in plan_gc(&ledger.document, &limits, 100).phases {
        apply_phase(&mut ledger.document, &phase, &limits, 100).unwrap();
    }
    assert!(ledger.document.tombstone(&operation).is_some());

    // Inside the horizon the exact tombstone stays.
    assert!(
        plan_gc(&ledger.document, &limits, 110)
            .phases
            .iter()
            .all(|phase| !matches!(phase, GcPhase::Compact(_)))
    );

    let plan = plan_gc(&ledger.document, &limits, 200);
    let compact = plan
        .phases
        .iter()
        .find(|phase| matches!(phase, GcPhase::Compact(_)))
        .unwrap();
    apply_phase(&mut ledger.document, compact, &limits, 200).unwrap();
    assert!(ledger.document.tombstones.is_empty());
    assert!(
        ledger.document.is_expired(&operation),
        "the watermark keeps the id expired with no record left at all"
    );
    assert_eq!(
        ledger
            .document
            .admit(&operation, "digest", ResourceKind::Terminal, policy(8, 8)),
        Err(ResourceError::OperationExpired)
    );
    ledger.document.validate().unwrap();
}

#[test]
fn a_candidate_that_stopped_being_safe_is_refused_at_apply_time() {
    let mut ledger = Ledger::new();
    let operation = ordered(6);
    let resource = ledger.completed(&operation, 0);
    let limits = limits();

    // A retry re-claimed the capacity between planning and applying.
    let phase = GcPhase::Evict(vec![operation]);
    let mut raced = ledger.document.clone();
    raced.claims.iter_mut().for_each(|claim| {
        if claim.resource == resource {
            claim.state = ClaimState::Live;
        }
    });
    assert_eq!(
        apply_phase(&mut raced, &phase, &limits, 100),
        Err(ResourceError::WrongState)
    );

    // Evicting the same record twice converges instead of duplicating.
    apply_phase(&mut ledger.document, &phase, &limits, 100).unwrap();
    apply_phase(&mut ledger.document, &phase, &limits, 100).unwrap();
    assert_eq!(ledger.document.tombstones.len(), 1);

    // A missing record is an unknown operation, not a silent skip.
    let mut empty = AllocatorDocument::default();
    assert_eq!(
        apply_phase(&mut empty, &GcPhase::Evict(vec![ordered(7)]), &limits, 100),
        Err(ResourceError::WrongState)
    );
}

#[test]
fn compaction_requires_a_watermark_that_actually_covers_the_ids() {
    let limits = limits();
    let mut ledger = Ledger::new();
    let operation = ordered(8);
    ledger.completed(&operation, 0);
    apply_phase(
        &mut ledger.document,
        &GcPhase::Evict(vec![operation]),
        &limits,
        100,
    )
    .unwrap();

    assert_eq!(
        apply_phase(
            &mut ledger.document,
            &GcPhase::Compact(vec![operation]),
            &limits,
            200
        ),
        Err(ResourceError::WrongState),
        "no watermark means no compaction"
    );

    apply_phase(
        &mut ledger.document,
        &GcPhase::AdvanceWatermark(ordered(1)),
        &limits,
        200,
    )
    .unwrap();
    assert_eq!(
        apply_phase(
            &mut ledger.document,
            &GcPhase::Compact(vec![operation]),
            &limits,
            200
        ),
        Err(ResourceError::WrongState),
        "a watermark below the id does not cover it"
    );

    // The watermark only ever moves forward.
    apply_phase(
        &mut ledger.document,
        &GcPhase::AdvanceWatermark(ordered(8)),
        &limits,
        200,
    )
    .unwrap();
    apply_phase(
        &mut ledger.document,
        &GcPhase::AdvanceWatermark(ordered(2)),
        &limits,
        200,
    )
    .unwrap();
    assert_eq!(ledger.document.watermark, Some(ordered(8)));
    apply_phase(
        &mut ledger.document,
        &GcPhase::Compact(vec![operation]),
        &limits,
        200,
    )
    .unwrap();
    assert!(ledger.document.tombstones.is_empty());
}

#[test]
fn a_hard_cap_of_uncollectable_records_refuses_fresh_admission_instead_of_evicting() {
    let mut ledger = Ledger::new();
    let limits = RetentionLimits::new(3, 1 << 20, 100, 10, 20);
    // A live reservation, an ambiguous final, and an unreleased spawn: three
    // records that fill the cap and none of which may ever be collected.
    ledger.reserve(&ordered(0x10));
    let ambiguous = ordered(0x11);
    ledger.reserve(&ambiguous);
    ledger.document.mark_ambiguous(&ambiguous, 0).unwrap();
    let live = ordered(0x12);
    ledger.reserve(&live);
    ledger.document.mark_spawned(&live, 0).unwrap();

    let plan = plan_gc(&ledger.document, &limits, 1_000);
    assert!(plan.phases.is_empty());
    assert!(plan.backpressure);
    assert_eq!(
        admission_guard(&ledger.document, &limits, 1_000),
        Err(ResourceError::RetentionBackpressure)
    );
    assert_eq!(ledger.document.operations.len(), 3);
    assert!(
        ledger.document.operation(&ambiguous).is_some(),
        "an ambiguous final is never evicted for space"
    );
}

#[test]
fn a_byte_cap_collects_eligible_records_before_it_refuses() {
    let mut ledger = Ledger::new();
    let limits = RetentionLimits::new(64, 1, 1_000, 0, 0);
    ledger.completed(&ordered(0x20), 0);
    let plan = plan_gc(&ledger.document, &limits, 1);
    assert!(!plan.backpressure);
    assert!(matches!(plan.phases[0], GcPhase::Evict(_)));
}

#[test]
fn a_full_pass_walks_the_phases_and_reports_what_it_did() {
    let bytes = SharedBytes::default();
    let allocator = allocator(&bytes, policy(8, 8));
    let clock = FakeClock::at(0);
    let limits = limits();
    let owner = DaemonGeneration::new();
    let operation = ordered(0x30);
    let resource = terminal(owner);
    allocator
        .update(|document| {
            document.reserve(
                &operation,
                "digest",
                ResourceKind::Terminal,
                owner,
                &resource,
                policy(8, 8),
            )?;
            document.mark_spawned(&operation, 0)?;
            document.consume_exit(owner, &resource, 1).map(|_| ())
        })
        .unwrap();

    assert_eq!(
        collect_garbage(&allocator, &limits, &clock).unwrap(),
        GcReport::default(),
        "inside the window nothing is collected"
    );

    clock.advance(100);
    assert_eq!(clock.now(), 100);
    let report = collect_garbage(&allocator, &limits, &clock).unwrap();
    assert_eq!(report.evicted, 1);
    assert!(report.watermark_advanced);
    assert_eq!(report.compacted, 0);

    clock.advance(200);
    let report = collect_garbage(&allocator, &limits, &clock).unwrap();
    assert_eq!(report.compacted, 1);
    let document = allocator.load().unwrap().to_document();
    assert!(document.operations.is_empty());
    assert!(document.tombstones.is_empty());
    assert!(document.is_expired(&operation));
    assert_eq!(document.watermark, Some(operation));
}

/// A file that lets another writer commit in the window between planning a
/// collection and applying its first phase: the second read returns a document
/// whose capacity has been claimed again.
struct RacingFile {
    bytes: SharedBytes,
    reads: std::sync::atomic::AtomicUsize,
}

impl crate::usecase::resources::CasFile for RacingFile {
    fn read(&self) -> std::io::Result<Option<String>> {
        let reads = self
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if reads == 2
            && let Some(stored) = self.bytes.get()
        {
            let mut document: AllocatorDocument = serde_json::from_str(&stored).unwrap();
            document.revision += 1;
            for claim in &mut document.claims {
                claim.state = ClaimState::Live;
            }
            self.bytes.set(&serde_json::to_string(&document).unwrap());
        }
        Ok(self.bytes.get())
    }

    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> std::io::Result<bool> {
        MemoryFile::new(&self.bytes).compare_and_write(expected, contents)
    }
}

#[test]
fn a_phase_whose_record_was_reclaimed_between_plan_and_apply_is_dropped() {
    let bytes = SharedBytes::default();
    let clock = FakeClock::at(100);
    let limits = limits();
    let owner = DaemonGeneration::new();
    let operation = ordered(0x60);
    let resource = terminal(owner);
    allocator(&bytes, policy(8, 8))
        .update(|document| {
            document.reserve(
                &operation,
                "digest",
                ResourceKind::Terminal,
                owner,
                &resource,
                policy(8, 8),
            )?;
            document.mark_spawned(&operation, 0)?;
            document.consume_exit(owner, &resource, 1).map(|_| ())
        })
        .unwrap();

    let racing = ResourceAllocator::new(
        RacingFile {
            bytes: bytes.clone(),
            reads: std::sync::atomic::AtomicUsize::new(0),
        },
        policy(8, 8),
    );
    let report = collect_garbage(&racing, &limits, &clock).unwrap();
    assert_eq!(
        report,
        GcReport::default(),
        "the retry that reclaimed the capacity wins; nothing is collected"
    );
    let document = allocator(&bytes, policy(8, 8))
        .load()
        .unwrap()
        .to_document();
    assert!(
        document.operation(&operation).is_some(),
        "the full outcome is still replayable"
    );
    assert!(document.tombstones.is_empty());
}

#[test]
fn a_phase_that_lost_its_race_is_skipped_rather_than_forced() {
    let bytes = SharedBytes::default();
    let allocator = allocator(&bytes, policy(8, 8));
    let clock = FakeClock::at(100);
    let limits = limits();
    let owner = DaemonGeneration::new();
    let operation = ordered(0x40);
    let resource = terminal(owner);
    allocator
        .update(|document| {
            document.reserve(
                &operation,
                "digest",
                ResourceKind::Terminal,
                owner,
                &resource,
                policy(8, 8),
            )?;
            document.mark_spawned(&operation, 0)?;
            document.consume_exit(owner, &resource, 1).map(|_| ())
        })
        .unwrap();

    // Between planning and applying, the record's capacity is claimed again.
    let plan = plan_gc(&allocator.load().unwrap().to_document(), &limits, 100);
    assert!(matches!(plan.phases[0], GcPhase::Evict(_)));
    allocator
        .update(|document| {
            document
                .claims
                .iter_mut()
                .for_each(|claim| claim.state = ClaimState::Live);
            Ok(())
        })
        .unwrap();
    let report = collect_garbage(&allocator, &limits, &clock).unwrap();
    assert_eq!(report.evicted, 0);
    assert!(
        allocator
            .load()
            .unwrap()
            .document()
            .operation(&operation)
            .is_some()
    );

    let broken = ResourceAllocator::new(
        MemoryFile::faulty(
            &bytes,
            crate::usecase::resources::fixture::FileFault::ReadFails,
        ),
        policy(8, 8),
    );
    assert!(
        collect_garbage(&broken, &limits, &clock)
            .unwrap_err()
            .refusal()
            .is_none()
    );
}

#[test]
fn a_failed_final_compacts_into_the_failed_class_and_the_horizon_has_a_floor() {
    let mut ledger = Ledger::new();
    let operation = ordered(0x50);
    ledger.reserve(&operation);
    ledger
        .document
        .mark_failed(&operation, LaunchFailure::Reservation, 0)
        .unwrap();
    let limits = RetentionLimits::new(3, 1 << 20, 10, 5, 1);
    assert_eq!(
        limits.expiry_horizon, 5,
        "the horizon can never be shorter than the replay window"
    );
    apply_phase(
        &mut ledger.document,
        &GcPhase::Evict(vec![operation]),
        &limits,
        50,
    )
    .unwrap();
    assert_eq!(
        ledger.document.tombstone(&operation).unwrap().class,
        ExpiryClass::Failed
    );
}
