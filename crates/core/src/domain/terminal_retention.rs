//! Aggregate retention budget, launch admission reservation, and deterministic
//! garbage collection for exited terminal / Agent runtime finals.
//!
//! A single exited terminal is already bounded: its raw replay window is capped
//! and its PTY transport is released at exit. This module owns the *aggregate*
//! bound layered on top of that: how many finals a daemon (and each workspace
//! inside it) may retain at once, how many bytes they may occupy, how long they
//! stay reachable, and in which order they are evicted under pressure.
//!
//! The contract is a whole, not three separate knobs:
//!
//! - **Minimum visibility TTL** — every final, observed or not, is protected
//!   until it has been retained for the configured TTL. Nothing promises
//!   protection beyond it: past the TTL an unobserved final is an ordinary
//!   eviction candidate, because an all-unobserved workload cannot otherwise
//!   coexist with a hard cap.
//! - **Soft reserve** — crossing it starts garbage collection *and* launch
//!   backpressure while headroom still exists.
//! - **Pre-admission reservation** — a launch reserves the worst-case final
//!   budget of the runtime it is about to spawn. If that reservation does not
//!   fit inside the hard cap the launch is rejected with a typed
//!   [`AdmissionRejection`] *before* spawning. An admitted runtime's exit then
//!   always has capacity: [`RetentionLedger::commit_final`] cannot fail, so an
//!   exit result is never silently dropped to respect a cap.
//! - **Typed expiry** — an evicted final answers [`RetentionLedger::lookup`]
//!   with [`FinalLookup::Evicted`] and a compact [`EvictionMarker`], never with
//!   a fallback to some other runtime's history.
//!
//! The ledger is pure, in-memory, derived state. The durable records are owned
//! by the terminal / Agent owners; a daemon rebuilds this accounting at startup
//! with [`RetentionLedger::import_existing`], so a crash can leak neither a
//! reservation nor an eviction decision.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    id::{TerminalRef, WorkspaceId},
    terminal_launch::TerminalKind,
    terminal_visibility::TerminalVisibilityState,
};

/// Which aggregate budget a check applied to. The daemon budget is also the
/// per-user budget: one daemon owns one user's data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionScope {
    Daemon,
    Workspace,
}

/// Which dimension of a budget was exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDimension {
    Count,
    Bytes,
}

/// A launch refused before spawn because its worst-case final budget does not
/// fit inside a hard cap. It names the exhausted scope and dimension so the
/// rejection is actionable and never looks like a missing terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRejection {
    pub scope: RetentionScope,
    pub dimension: RetentionDimension,
}

/// The aggregate budget of one daemon.
///
/// Hard caps are never exceeded by admitted work: the count/byte caps bound
/// retained finals plus outstanding reservations. Soft reserves sit below them
/// and start GC and launch backpressure early. The age budget bounds how long a
/// final is retained even when nothing is under pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionBudget {
    /// Hard cap on retained finals plus outstanding reservations, daemon-wide.
    pub max_finals: usize,
    /// Hard cap on retained plus reserved final bytes, daemon-wide.
    pub max_bytes: u64,
    /// Hard cap on retained finals plus reservations in one workspace.
    pub max_finals_per_workspace: usize,
    /// Hard cap on retained plus reserved final bytes in one workspace.
    pub max_bytes_per_workspace: u64,
    /// Daemon-wide count at which GC and launch backpressure start.
    pub soft_reserve_finals: usize,
    /// Daemon-wide byte volume at which GC and launch backpressure start.
    pub soft_reserve_bytes: u64,
    /// Per-workspace count at which GC starts for that workspace.
    pub soft_reserve_finals_per_workspace: usize,
    /// Per-workspace byte volume at which GC starts for that workspace.
    pub soft_reserve_bytes_per_workspace: u64,
    /// How long every final stays protected from pressure eviction, observed or
    /// not. Zero makes a final immediately evictable.
    pub min_visibility_ttl_secs: i64,
    /// Age budget: a final older than this is collected on the next pass even
    /// with no pressure at all.
    pub max_final_age_secs: i64,
    /// Budget one launch reserves for the final its runtime will produce.
    pub worst_case_final_bytes: u64,
    /// Upper bound on evictions performed by one [`RetentionLedger::collect`]
    /// so a GC pass is bounded work.
    pub max_gc_batch: usize,
    /// How many compact eviction markers stay queryable. The oldest markers are
    /// forgotten past this bound and counted, never dropped silently.
    pub max_eviction_markers: usize,
}

impl Default for RetentionBudget {
    /// The shipped daemon budget: 512 finals / 32 MiB daemon-wide, half of that
    /// per workspace, a 10-minute minimum visibility TTL, and a 24-hour age
    /// budget. One final is worst-cased at the 64 KiB bounded replay window.
    fn default() -> Self {
        Self {
            max_finals: 512,
            max_bytes: 32 * 1024 * 1024,
            max_finals_per_workspace: 256,
            max_bytes_per_workspace: 16 * 1024 * 1024,
            soft_reserve_finals: 384,
            soft_reserve_bytes: 24 * 1024 * 1024,
            soft_reserve_finals_per_workspace: 192,
            soft_reserve_bytes_per_workspace: 12 * 1024 * 1024,
            min_visibility_ttl_secs: 600,
            max_final_age_secs: 86_400,
            worst_case_final_bytes: 64 * 1024,
            max_gc_batch: 64,
            max_eviction_markers: 1024,
        }
    }
}

impl RetentionBudget {
    /// Returns a budget whose relations hold: every cap admits at least one
    /// final, soft reserves sit at or below their hard caps, the TTL does not
    /// outlive the age budget, and a GC pass evicts at least one entry.
    ///
    /// A misconfigured budget is repaired rather than rejected: a daemon must
    /// still bound its retention, and a cap of zero would refuse every launch.
    #[must_use]
    pub fn normalized(self) -> Self {
        let max_finals = self.max_finals.max(1);
        let worst_case_final_bytes = self.worst_case_final_bytes.max(1);
        let max_bytes = self.max_bytes.max(worst_case_final_bytes);
        let max_finals_per_workspace = self.max_finals_per_workspace.clamp(1, max_finals);
        let max_bytes_per_workspace = self
            .max_bytes_per_workspace
            .clamp(worst_case_final_bytes, max_bytes);
        let max_final_age_secs = self.max_final_age_secs.max(0);
        Self {
            max_finals,
            max_bytes,
            max_finals_per_workspace,
            max_bytes_per_workspace,
            soft_reserve_finals: self.soft_reserve_finals.min(max_finals),
            soft_reserve_bytes: self.soft_reserve_bytes.min(max_bytes),
            soft_reserve_finals_per_workspace: self
                .soft_reserve_finals_per_workspace
                .min(max_finals_per_workspace),
            soft_reserve_bytes_per_workspace: self
                .soft_reserve_bytes_per_workspace
                .min(max_bytes_per_workspace),
            min_visibility_ttl_secs: self.min_visibility_ttl_secs.clamp(0, max_final_age_secs),
            max_final_age_secs,
            worst_case_final_bytes,
            max_gc_batch: self.max_gc_batch.max(1),
            max_eviction_markers: self.max_eviction_markers,
        }
    }
}

/// The eviction priority of a retained final. Lower variants are evicted first,
/// so the least valuable history goes before history the user has not seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalClass {
    /// The user explicitly closed it: nothing will surface it again.
    Dismissed,
    /// A newer runtime in the same lineage replaced it.
    Superseded,
    /// Surfaced to a client but not dismissed.
    Observed,
    /// Never surfaced. Evicted last, but not protected forever.
    Unobserved,
}

impl FinalClass {
    /// The eviction rank used to order candidates; lower goes first.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Dismissed => 0,
            Self::Superseded => 1,
            Self::Observed => 2,
            Self::Unobserved => 3,
        }
    }
}

/// One retained final: the tombstone accounting of an exited generic terminal
/// or an Agent runtime. It carries no output, argv, environment, or provider
/// identity — only the exact key, its size, and its retention state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedFinal {
    pub terminal: TerminalRef,
    pub kind: TerminalKind,
    /// Bytes the retained replay and record occupy.
    pub bytes: u64,
    /// When the runtime exited; the minimum TTL and age budget count from here.
    pub exited_at: DateTime<Utc>,
    /// The workspace-global visibility of this exact tombstone.
    pub visibility: TerminalVisibilityState,
    /// Whether a newer runtime in the same lineage replaced it.
    pub superseded: bool,
    /// Whether an owner still needs this final: an eligible provider resume
    /// source, or lineage a live tab may reopen. Pressure GC never takes it.
    pub pinned: bool,
}

impl RetainedFinal {
    /// A newly committed, never-surfaced, unpinned final.
    #[must_use]
    pub fn new(
        terminal: TerminalRef,
        kind: TerminalKind,
        bytes: u64,
        exited_at: DateTime<Utc>,
    ) -> Self {
        Self {
            terminal,
            kind,
            bytes,
            exited_at,
            visibility: TerminalVisibilityState::Unobserved,
            superseded: false,
            pinned: false,
        }
    }

    /// The eviction priority derived from visibility and lineage.
    #[must_use]
    pub fn class(&self) -> FinalClass {
        match self.visibility {
            TerminalVisibilityState::Dismissed => FinalClass::Dismissed,
            _ if self.superseded => FinalClass::Superseded,
            TerminalVisibilityState::Observed => FinalClass::Observed,
            TerminalVisibilityState::Unobserved => FinalClass::Unobserved,
        }
    }

    /// Seconds retained at `now`, clamped at zero so a clock that steps
    /// backwards cannot make a final look older than it is.
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        now.signed_duration_since(self.exited_at)
            .num_seconds()
            .max(0)
    }
}

/// Why a final left the ledger. Every eviction has one; there is no silent
/// deletion path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionReason {
    /// Past the age budget, collected without any pressure.
    AgeExpired,
    /// Past the minimum TTL and collected to relieve a soft reserve.
    Pressure,
    /// Over a hard cap with no eligible candidate left — only reachable after a
    /// migration or a shrunk budget imported more than the cap allows.
    Emergency,
}

/// The compact typed tombstone of an evicted final. It replaces the final in
/// the ledger so a later query is answered `Evicted`, not "missing".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvictionMarker {
    pub terminal: TerminalRef,
    pub kind: TerminalKind,
    pub reason: EvictionReason,
    pub evicted_at: DateTime<Utc>,
    /// Bytes the evicted final had occupied.
    pub bytes: u64,
}

/// The answer to "what happened to this runtime's final?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalLookup {
    /// Still retained and reachable.
    Retained(RetainedFinal),
    /// Collected. The marker says when and why; it never points at another
    /// runtime's history.
    Evicted(EvictionMarker),
    /// Never committed here, or its marker has aged out of the bounded marker
    /// window ([`RetentionBudget::max_eviction_markers`]).
    Unknown,
}

impl FinalLookup {
    /// The final itself, when it is still retained.
    #[must_use]
    pub fn retained(self) -> Option<RetainedFinal> {
        match self {
            Self::Retained(record) => Some(record),
            _ => None,
        }
    }

    /// The typed eviction marker, when the final was collected.
    #[must_use]
    pub fn marker(self) -> Option<EvictionMarker> {
        match self {
            Self::Evicted(marker) => Some(marker),
            _ => None,
        }
    }
}

/// The result of committing an admitted runtime's final. Committing never
/// fails, so the fields describe accounting, not success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalCommit {
    /// Whether the commit consumed a reservation taken before spawn.
    pub reserved: bool,
    /// Bytes by which the final exceeded the reserved worst case.
    pub over_reserve_bytes: u64,
}

/// What one bounded GC pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// The markers of the finals evicted in this pass, in eviction order.
    pub evicted: Vec<EvictionMarker>,
    /// Whether the pass stopped on its batch bound with work still to do. The
    /// caller runs another pass rather than doing unbounded work at once.
    pub truncated: bool,
}

impl GcReport {
    /// Whether the pass evicted nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.evicted.is_empty()
    }

    /// Total bytes released by this pass.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.evicted
            .iter()
            .fold(0, |sum, marker| sum.saturating_add(marker.bytes))
    }
}

/// Operator-visible retention state. It is counts, bytes, and ages only: no
/// terminal output, argv, or provider identity ever reaches a metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionMetrics {
    pub retained_finals: usize,
    pub retained_bytes: u64,
    /// Age of the oldest retained final in seconds, zero when none is retained.
    pub oldest_retained_age_secs: i64,
    pub reserved_finals: usize,
    pub reserved_bytes: u64,
    /// Whether a soft reserve is currently crossed daemon-wide or in any
    /// workspace, i.e. GC and launch backpressure are active.
    pub soft_pressure: bool,
    pub admission_rejections: u64,
    pub evicted_finals: u64,
    pub evicted_bytes: u64,
    pub emergency_evictions: u64,
    /// Finals committed without a live reservation (a restart-crossing exit or
    /// a migrated record).
    pub unreserved_commits: u64,
    /// Bytes committed beyond the reserved worst case.
    pub over_reserve_bytes: u64,
    /// Markers dropped from the bounded marker window.
    pub forgotten_markers: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Usage {
    finals: usize,
    bytes: u64,
    reservations: usize,
    reserved_bytes: u64,
}

impl Usage {
    fn count(self) -> usize {
        self.finals.saturating_add(self.reservations)
    }

    fn total_bytes(self) -> u64 {
        self.bytes.saturating_add(self.reserved_bytes)
    }

    fn is_idle(self) -> bool {
        self.count() == 0 && self.total_bytes() == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    admission_rejections: u64,
    evicted_finals: u64,
    evicted_bytes: u64,
    emergency_evictions: u64,
    unreserved_commits: u64,
    over_reserve_bytes: u64,
    forgotten_markers: u64,
}

/// Which candidates one eviction pass may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Past the age budget, unpinned.
    Aged,
    /// Past the minimum visibility TTL, unpinned.
    Pressure,
    /// Anything, pinned and inside-TTL entries last.
    Emergency,
}

/// The daemon's aggregate retention authority: budgets, live reservations,
/// retained finals, and the bounded eviction markers they leave behind.
///
/// It is pure state. The owner of the durable records drives it: reserve before
/// spawn, commit at exit, collect on pressure, and apply the returned evictions
/// to the store and the output journal.
#[derive(Debug, Clone)]
pub struct RetentionLedger {
    budget: RetentionBudget,
    finals: BTreeMap<TerminalRef, RetainedFinal>,
    reservations: BTreeMap<TerminalRef, u64>,
    markers: BTreeMap<TerminalRef, EvictionMarker>,
    marker_order: VecDeque<TerminalRef>,
    daemon: Usage,
    workspaces: BTreeMap<WorkspaceId, Usage>,
    counters: Counters,
}

impl Default for RetentionLedger {
    fn default() -> Self {
        Self::new(RetentionBudget::default())
    }
}

impl RetentionLedger {
    /// Creates an empty ledger over `budget`, normalized so its caps and
    /// reserves are consistent.
    #[must_use]
    pub fn new(budget: RetentionBudget) -> Self {
        Self {
            budget: budget.normalized(),
            finals: BTreeMap::new(),
            reservations: BTreeMap::new(),
            markers: BTreeMap::new(),
            marker_order: VecDeque::new(),
            daemon: Usage::default(),
            workspaces: BTreeMap::new(),
            counters: Counters::default(),
        }
    }

    /// The normalized budget in force.
    #[must_use]
    pub fn budget(&self) -> RetentionBudget {
        self.budget
    }

    /// Reserves the worst-case final budget of a runtime that is about to be
    /// spawned, garbage collecting first when a soft reserve is crossed.
    ///
    /// Reserving the same `terminal` twice is idempotent, so a retried launch
    /// can neither double-count nor leak capacity.
    ///
    /// # Errors
    ///
    /// Returns the exhausted scope and dimension when the reservation does not
    /// fit inside a hard cap even after collection. The caller must reject the
    /// launch before spawning; capacity is never made by deleting a final that
    /// is still inside its minimum visibility TTL.
    pub fn reserve(
        &mut self,
        now: DateTime<Utc>,
        terminal: &TerminalRef,
    ) -> Result<(), AdmissionRejection> {
        if self.reservations.contains_key(terminal) {
            return Ok(());
        }
        let workspace = terminal.workspace_id;
        if self.headroom(workspace).is_err() || self.under_soft_pressure() {
            let _ = self.collect(now);
        }
        if let Err(rejection) = self.headroom(workspace) {
            self.counters.admission_rejections =
                self.counters.admission_rejections.saturating_add(1);
            return Err(rejection);
        }
        let bytes = self.budget.worst_case_final_bytes;
        self.reservations.insert(terminal.clone(), bytes);
        self.adjust(workspace, |usage| {
            usage.reservations = usage.reservations.saturating_add(1);
            usage.reserved_bytes = usage.reserved_bytes.saturating_add(bytes);
        });
        Ok(())
    }

    /// Releases a reservation whose runtime never reached a final (a rejected
    /// or failed spawn). Releasing an unknown or already-released reservation
    /// is a no-op, so a retried compensation cannot double-release.
    pub fn release(&mut self, terminal: &TerminalRef) -> bool {
        let Some(bytes) = self.reservations.remove(terminal) else {
            return false;
        };
        self.adjust(terminal.workspace_id, |usage| {
            usage.reservations = usage.reservations.saturating_sub(1);
            usage.reserved_bytes = usage.reserved_bytes.saturating_sub(bytes);
        });
        true
    }

    /// Whether a reservation is currently held for `terminal`.
    #[must_use]
    pub fn is_reserved(&self, terminal: &TerminalRef) -> bool {
        self.reservations.contains_key(terminal)
    }

    /// Stores the final of an exited runtime into its reserved capacity.
    ///
    /// This never fails and never evicts: an admitted runtime's exit result is
    /// recorded even when it exceeds the reserved worst case. The overflow is
    /// accounted and relieved by the next [`collect`](Self::collect).
    pub fn commit_final(
        &mut self,
        terminal: &TerminalRef,
        kind: TerminalKind,
        bytes: u64,
        exited_at: DateTime<Utc>,
    ) -> FinalCommit {
        let reserved = self.release(terminal);
        if !reserved {
            self.counters.unreserved_commits = self.counters.unreserved_commits.saturating_add(1);
        }
        let over_reserve_bytes = bytes.saturating_sub(self.budget.worst_case_final_bytes);
        self.counters.over_reserve_bytes = self
            .counters
            .over_reserve_bytes
            .saturating_add(over_reserve_bytes);
        let mut record = RetainedFinal::new(terminal.clone(), kind, bytes, exited_at);
        // A duplicate exit keeps whatever visibility and lineage the first one
        // accumulated instead of resurrecting an unobserved entry.
        if let Some(previous) = self.take_final(terminal) {
            record.visibility = record.visibility.merge(previous.visibility);
            record.superseded = previous.superseded;
            record.pinned = previous.pinned;
        }
        self.insert_final(record);
        FinalCommit {
            reserved,
            over_reserve_bytes,
        }
    }

    /// Re-imports a final that already exists in a durable store, without a
    /// reservation. Startup reconciliation and migration of previously
    /// unbounded records use this; the import is admitted even when it puts the
    /// ledger over a cap, and the next collection brings it back inside.
    pub fn import_existing(&mut self, record: RetainedFinal) {
        let _ = self.take_final(&record.terminal);
        self.markers.remove(&record.terminal);
        self.marker_order.retain(|key| key != &record.terminal);
        self.insert_final(record);
    }

    /// Raises the recorded visibility of a retained final to at least `state`,
    /// mirroring the workspace-global visibility ledger. Returns whether the
    /// final is still retained.
    pub fn note_visibility(
        &mut self,
        terminal: &TerminalRef,
        state: TerminalVisibilityState,
    ) -> bool {
        let Some(record) = self.finals.get_mut(terminal) else {
            return false;
        };
        record.visibility = record.visibility.merge(state);
        true
    }

    /// Marks a final as replaced by a newer runtime in the same lineage, which
    /// lowers its retention priority. Returns whether it is still retained.
    pub fn mark_superseded(&mut self, terminal: &TerminalRef) -> bool {
        let Some(record) = self.finals.get_mut(terminal) else {
            return false;
        };
        record.superseded = true;
        true
    }

    /// Pins or unpins a final an owner still needs — an eligible provider
    /// resume source, or lineage a client may still reopen. A pinned final is
    /// never taken by age or pressure eviction. Returns whether it is retained.
    pub fn set_pinned(&mut self, terminal: &TerminalRef, pinned: bool) -> bool {
        let Some(record) = self.finals.get_mut(terminal) else {
            return false;
        };
        record.pinned = pinned;
        true
    }

    /// Answers what happened to one exact runtime's final.
    #[must_use]
    pub fn lookup(&self, terminal: &TerminalRef) -> FinalLookup {
        if let Some(record) = self.finals.get(terminal) {
            return FinalLookup::Retained(record.clone());
        }
        self.markers
            .get(terminal)
            .cloned()
            .map_or(FinalLookup::Unknown, FinalLookup::Evicted)
    }

    /// The retained finals of one kind, in deterministic key order.
    #[must_use]
    pub fn retained(&self, kind: TerminalKind) -> Vec<&RetainedFinal> {
        self.finals
            .values()
            .filter(|record| record.kind == kind)
            .collect()
    }

    /// Whether a soft reserve is crossed daemon-wide or in any workspace.
    #[must_use]
    pub fn under_soft_pressure(&self) -> bool {
        self.daemon_soft_pressure()
            || self
                .workspaces
                .keys()
                .any(|id| self.workspace_pressure(*id))
    }

    /// Runs one bounded garbage-collection pass and returns what it evicted.
    ///
    /// The passes run in a fixed order — age budget, then soft-reserve
    /// pressure, then a hard-cap emergency — and each takes candidates in a
    /// deterministic order, so the same ledger and clock always evict the same
    /// entries. Finals inside their minimum visibility TTL are taken only by
    /// the emergency pass, which is reachable only when an import or a shrunk
    /// budget already put the ledger over a hard cap.
    pub fn collect(&mut self, now: DateTime<Utc>) -> GcReport {
        let mut report = GcReport::default();
        self.run_pass(now, Pass::Aged, &mut report);
        self.run_pass(now, Pass::Pressure, &mut report);
        self.run_pass(now, Pass::Emergency, &mut report);
        report
    }

    /// The operator-visible retention snapshot at `now`.
    #[must_use]
    pub fn metrics(&self, now: DateTime<Utc>) -> RetentionMetrics {
        RetentionMetrics {
            retained_finals: self.daemon.finals,
            retained_bytes: self.daemon.bytes,
            oldest_retained_age_secs: self
                .finals
                .values()
                .map(|record| record.age_secs(now))
                .max()
                .unwrap_or(0),
            reserved_finals: self.daemon.reservations,
            reserved_bytes: self.daemon.reserved_bytes,
            soft_pressure: self.under_soft_pressure(),
            admission_rejections: self.counters.admission_rejections,
            evicted_finals: self.counters.evicted_finals,
            evicted_bytes: self.counters.evicted_bytes,
            emergency_evictions: self.counters.emergency_evictions,
            unreserved_commits: self.counters.unreserved_commits,
            over_reserve_bytes: self.counters.over_reserve_bytes,
            forgotten_markers: self.counters.forgotten_markers,
        }
    }

    fn run_pass(&mut self, now: DateTime<Utc>, pass: Pass, report: &mut GcReport) {
        loop {
            if report.evicted.len() >= self.budget.max_gc_batch {
                // A later pass must not clear a bound an earlier one hit.
                report.truncated = report.truncated || self.has_work(now, pass);
                return;
            }
            let Some(terminal) = self.next_victim(now, pass) else {
                return;
            };
            let reason = match pass {
                Pass::Aged => EvictionReason::AgeExpired,
                Pass::Pressure => EvictionReason::Pressure,
                Pass::Emergency => EvictionReason::Emergency,
            };
            let marker = self.evict(&terminal, reason, now);
            report.evicted.push(marker);
        }
    }

    fn has_work(&self, now: DateTime<Utc>, pass: Pass) -> bool {
        self.next_victim(now, pass).is_some()
    }

    /// The next final this pass must take, or `None` when the pass is done.
    /// Ordering is total, so a pass is deterministic for a given ledger.
    fn next_victim(&self, now: DateTime<Utc>, pass: Pass) -> Option<TerminalRef> {
        let pressured = (pass == Pass::Pressure).then(|| self.pressured_scopes());
        let over_hard = (pass == Pass::Emergency).then(|| self.over_hard_scopes());
        self.finals
            .values()
            .filter(|record| match pass {
                Pass::Aged => {
                    !record.pinned && record.age_secs(now) >= self.budget.max_final_age_secs
                }
                Pass::Pressure => {
                    !record.pinned
                        && record.age_secs(now) >= self.budget.min_visibility_ttl_secs
                        && pressured
                            .as_ref()
                            .is_some_and(|scopes| scopes.covers(record))
                }
                Pass::Emergency => over_hard
                    .as_ref()
                    .is_some_and(|scopes| scopes.covers(record)),
            })
            .min_by(|left, right| pass_order(pass, left).cmp(&pass_order(pass, right)))
            .map(|record| record.terminal.clone())
    }

    /// The scopes currently above a soft reserve. Recomputed on every eviction
    /// so a pass stops as soon as the pressure it relieves is gone.
    fn pressured_scopes(&self) -> Scopes {
        Scopes {
            daemon: self.daemon_soft_pressure(),
            workspaces: self
                .workspaces
                .keys()
                .copied()
                .filter(|id| self.workspace_pressure(*id))
                .collect(),
        }
    }

    /// The scopes currently above a hard cap. Only an import or a shrunk budget
    /// can produce one.
    fn over_hard_scopes(&self) -> Scopes {
        Scopes {
            daemon: self.daemon.count() > self.budget.max_finals
                || self.daemon.total_bytes() > self.budget.max_bytes,
            workspaces: self
                .workspaces
                .iter()
                .filter(|(_, usage)| {
                    usage.count() > self.budget.max_finals_per_workspace
                        || usage.total_bytes() > self.budget.max_bytes_per_workspace
                })
                .map(|(id, _)| *id)
                .collect(),
        }
    }

    fn daemon_soft_pressure(&self) -> bool {
        self.daemon.count() >= self.budget.soft_reserve_finals
            || self.daemon.total_bytes() >= self.budget.soft_reserve_bytes
    }

    fn workspace_pressure(&self, workspace: WorkspaceId) -> bool {
        self.workspaces.get(&workspace).is_some_and(|usage| {
            usage.count() >= self.budget.soft_reserve_finals_per_workspace
                || usage.total_bytes() >= self.budget.soft_reserve_bytes_per_workspace
        })
    }

    /// Whether one more worst-case final fits inside every hard cap.
    fn headroom(&self, workspace: WorkspaceId) -> Result<(), AdmissionRejection> {
        let bytes = self.budget.worst_case_final_bytes;
        let workspace_usage = self.workspaces.get(&workspace).copied().unwrap_or_default();
        if self.daemon.count().saturating_add(1) > self.budget.max_finals {
            return Err(AdmissionRejection {
                scope: RetentionScope::Daemon,
                dimension: RetentionDimension::Count,
            });
        }
        if self.daemon.total_bytes().saturating_add(bytes) > self.budget.max_bytes {
            return Err(AdmissionRejection {
                scope: RetentionScope::Daemon,
                dimension: RetentionDimension::Bytes,
            });
        }
        if workspace_usage.count().saturating_add(1) > self.budget.max_finals_per_workspace {
            return Err(AdmissionRejection {
                scope: RetentionScope::Workspace,
                dimension: RetentionDimension::Count,
            });
        }
        if workspace_usage.total_bytes().saturating_add(bytes) > self.budget.max_bytes_per_workspace
        {
            return Err(AdmissionRejection {
                scope: RetentionScope::Workspace,
                dimension: RetentionDimension::Bytes,
            });
        }
        Ok(())
    }

    fn evict(
        &mut self,
        terminal: &TerminalRef,
        reason: EvictionReason,
        now: DateTime<Utc>,
    ) -> EvictionMarker {
        let record = self
            .take_final(terminal)
            .expect("an eviction victim is a retained final");
        let marker = EvictionMarker {
            terminal: record.terminal.clone(),
            kind: record.kind,
            reason,
            evicted_at: now,
            bytes: record.bytes,
        };
        self.counters.evicted_finals = self.counters.evicted_finals.saturating_add(1);
        self.counters.evicted_bytes = self.counters.evicted_bytes.saturating_add(record.bytes);
        if reason == EvictionReason::Emergency {
            self.counters.emergency_evictions = self.counters.emergency_evictions.saturating_add(1);
        }
        self.remember_marker(marker.clone());
        marker
    }

    /// Keeps the marker window bounded, counting what it forgets so a dropped
    /// marker is visible to an operator instead of silent.
    fn remember_marker(&mut self, marker: EvictionMarker) {
        if self.budget.max_eviction_markers == 0 {
            self.counters.forgotten_markers = self.counters.forgotten_markers.saturating_add(1);
            return;
        }
        self.marker_order.push_back(marker.terminal.clone());
        self.markers.insert(marker.terminal.clone(), marker);
        while self.marker_order.len() > self.budget.max_eviction_markers
            && let Some(oldest) = self.marker_order.pop_front()
        {
            self.markers.remove(&oldest);
            self.counters.forgotten_markers = self.counters.forgotten_markers.saturating_add(1);
        }
    }

    fn insert_final(&mut self, record: RetainedFinal) {
        let workspace = record.terminal.workspace_id;
        let bytes = record.bytes;
        self.finals.insert(record.terminal.clone(), record);
        self.adjust(workspace, |usage| {
            usage.finals = usage.finals.saturating_add(1);
            usage.bytes = usage.bytes.saturating_add(bytes);
        });
    }

    fn take_final(&mut self, terminal: &TerminalRef) -> Option<RetainedFinal> {
        let record = self.finals.remove(terminal)?;
        let bytes = record.bytes;
        self.adjust(terminal.workspace_id, |usage| {
            usage.finals = usage.finals.saturating_sub(1);
            usage.bytes = usage.bytes.saturating_sub(bytes);
        });
        Some(record)
    }

    fn adjust(&mut self, workspace: WorkspaceId, apply: impl Fn(&mut Usage)) {
        apply(&mut self.daemon);
        let usage = self.workspaces.entry(workspace).or_default();
        apply(usage);
        if usage.is_idle() {
            self.workspaces.remove(&workspace);
        }
    }
}

/// The scopes one pass may take candidates from.
#[derive(Debug, Default)]
struct Scopes {
    daemon: bool,
    workspaces: BTreeSet<WorkspaceId>,
}

impl Scopes {
    fn covers(&self, record: &RetainedFinal) -> bool {
        self.daemon || self.workspaces.contains(&record.terminal.workspace_id)
    }
}

/// The total order a pass takes its candidates in. Every key ends with the
/// exact terminal so same-age, same-class, same-size entries still have one
/// deterministic answer.
fn pass_order(pass: Pass, record: &RetainedFinal) -> (u8, u8, DateTime<Utc>, &TerminalRef) {
    match pass {
        // Oldest first: the age budget is about age alone.
        Pass::Aged => (0, 0, record.exited_at, &record.terminal),
        // Least valuable class first, then oldest.
        Pass::Pressure => (0, record.class().rank(), record.exited_at, &record.terminal),
        // Unpinned before pinned, then as pressure does.
        Pass::Emergency => (
            u8::from(record.pinned),
            record.class().rank(),
            record.exited_at,
            &record.terminal,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId};
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId::new()
    }

    fn terminal_in(workspace: WorkspaceId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    /// A small fixture: 4 finals / 4 KiB daemon-wide, soft reserve at 3, a
    /// 10-second minimum TTL and a 100-second age budget.
    fn small_budget() -> RetentionBudget {
        RetentionBudget {
            max_finals: 4,
            max_bytes: 4096,
            max_finals_per_workspace: 3,
            max_bytes_per_workspace: 3072,
            soft_reserve_finals: 3,
            soft_reserve_bytes: 3072,
            soft_reserve_finals_per_workspace: 3,
            soft_reserve_bytes_per_workspace: 3072,
            min_visibility_ttl_secs: 10,
            max_final_age_secs: 100,
            worst_case_final_bytes: 1024,
            max_gc_batch: 8,
            max_eviction_markers: 8,
        }
    }

    /// Admits, exits, and commits one runtime, returning its exact key.
    fn admit_and_exit(
        ledger: &mut RetentionLedger,
        workspace: WorkspaceId,
        kind: TerminalKind,
        now: DateTime<Utc>,
        bytes: u64,
    ) -> TerminalRef {
        let terminal = terminal_in(workspace);
        ledger.reserve(now, &terminal).expect("fixture admits");
        ledger.commit_final(&terminal, kind, bytes, now);
        terminal
    }

    #[test]
    fn a_misconfigured_budget_is_normalized_into_consistent_relations() {
        let budget = RetentionBudget {
            max_finals: 0,
            max_bytes: 0,
            max_finals_per_workspace: 99,
            max_bytes_per_workspace: 0,
            soft_reserve_finals: 100,
            soft_reserve_bytes: u64::MAX,
            soft_reserve_finals_per_workspace: 100,
            soft_reserve_bytes_per_workspace: u64::MAX,
            min_visibility_ttl_secs: 900,
            max_final_age_secs: -5,
            worst_case_final_bytes: 0,
            max_gc_batch: 0,
            max_eviction_markers: 0,
        }
        .normalized();
        assert_eq!(budget.max_finals, 1);
        assert_eq!(budget.worst_case_final_bytes, 1);
        assert_eq!(budget.max_bytes, 1);
        assert_eq!(budget.max_finals_per_workspace, 1);
        assert_eq!(budget.max_bytes_per_workspace, 1);
        assert_eq!(budget.soft_reserve_finals, 1);
        assert_eq!(budget.soft_reserve_bytes, 1);
        assert_eq!(budget.soft_reserve_finals_per_workspace, 1);
        assert_eq!(budget.soft_reserve_bytes_per_workspace, 1);
        // A TTL can never outlive the age budget, and a pass evicts at least one.
        assert_eq!(budget.max_final_age_secs, 0);
        assert_eq!(budget.min_visibility_ttl_secs, 0);
        assert_eq!(budget.max_gc_batch, 1);
        // The shipped default is already consistent.
        assert_eq!(
            RetentionBudget::default().normalized(),
            RetentionBudget::default()
        );
        assert_eq!(
            RetentionLedger::default().budget(),
            RetentionBudget::default()
        );
    }

    #[test]
    fn class_orders_dismissed_before_superseded_observed_and_unobserved() {
        let mut record =
            RetainedFinal::new(terminal_in(workspace()), TerminalKind::Terminal, 10, at(0));
        assert_eq!(record.class(), FinalClass::Unobserved);
        record.visibility = TerminalVisibilityState::Observed;
        assert_eq!(record.class(), FinalClass::Observed);
        record.superseded = true;
        assert_eq!(record.class(), FinalClass::Superseded);
        record.visibility = TerminalVisibilityState::Dismissed;
        assert_eq!(record.class(), FinalClass::Dismissed);
        // A dismissed entry outranks a superseded one even without lineage.
        record.superseded = false;
        assert_eq!(record.class(), FinalClass::Dismissed);
        assert!(FinalClass::Dismissed < FinalClass::Superseded);
        assert!(FinalClass::Superseded < FinalClass::Observed);
        assert!(FinalClass::Observed < FinalClass::Unobserved);
        // A backwards clock cannot age a final.
        assert_eq!(record.age_secs(at(-30)), 0);
        assert_eq!(record.age_secs(at(30)), 30);
    }

    #[test]
    fn reservation_is_idempotent_and_release_never_double_counts() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let terminal = terminal_in(space);
        ledger.reserve(at(0), &terminal).unwrap();
        // A retried launch reserves the same capacity once.
        ledger.reserve(at(0), &terminal).unwrap();
        assert!(ledger.is_reserved(&terminal));
        let metrics = ledger.metrics(at(0));
        assert_eq!(metrics.reserved_finals, 1);
        assert_eq!(metrics.reserved_bytes, 1024);
        assert!(ledger.release(&terminal));
        // A retried compensation releases nothing more.
        assert!(!ledger.release(&terminal));
        let metrics = ledger.metrics(at(0));
        assert_eq!(metrics.reserved_finals, 0);
        assert_eq!(metrics.reserved_bytes, 0);
        assert!(!ledger.is_reserved(&terminal));
    }

    #[test]
    fn a_launch_that_cannot_reserve_is_rejected_before_spawn_by_scope_and_dimension() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 8,
            max_bytes: 8192,
            max_finals_per_workspace: 2,
            max_bytes_per_workspace: 2048,
            soft_reserve_finals: 8,
            soft_reserve_bytes: 8192,
            soft_reserve_finals_per_workspace: 2,
            soft_reserve_bytes_per_workspace: 2048,
            ..small_budget()
        });
        let space = workspace();
        // Two fresh finals fill the workspace count cap; both are inside the TTL.
        let first = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 1024);
        let _second = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 1024);
        let blocked = terminal_in(space);
        assert_eq!(
            ledger.reserve(at(1), &blocked),
            Err(AdmissionRejection {
                scope: RetentionScope::Workspace,
                dimension: RetentionDimension::Count,
            })
        );
        // No capacity was made by deleting a final inside its TTL.
        assert!(matches!(ledger.lookup(&first), FinalLookup::Retained(_)));
        assert_eq!(ledger.metrics(at(1)).retained_finals, 2);
        assert_eq!(ledger.metrics(at(1)).admission_rejections, 1);
        // Another workspace is unaffected by this one's exhaustion.
        let other = terminal_in(workspace());
        assert!(ledger.reserve(at(1), &other).is_ok());
    }

    #[test]
    fn hard_cap_rejections_name_each_exhausted_scope_and_dimension() {
        let space = workspace();
        // Daemon count: one final and one reservation fill a two-slot daemon.
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 2,
            max_bytes: 1 << 20,
            max_finals_per_workspace: 2,
            max_bytes_per_workspace: 1 << 20,
            soft_reserve_finals: 2,
            soft_reserve_bytes: 1 << 20,
            soft_reserve_finals_per_workspace: 2,
            soft_reserve_bytes_per_workspace: 1 << 20,
            ..small_budget()
        });
        admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 16);
        ledger.reserve(at(0), &terminal_in(space)).unwrap();
        assert_eq!(
            ledger.reserve(at(1), &terminal_in(space)),
            Err(AdmissionRejection {
                scope: RetentionScope::Daemon,
                dimension: RetentionDimension::Count,
            })
        );

        // Daemon bytes: a large retained final leaves no worst-case room.
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 8,
            max_bytes: 2048,
            max_finals_per_workspace: 8,
            max_bytes_per_workspace: 2048,
            soft_reserve_finals: 8,
            soft_reserve_bytes: 2048,
            soft_reserve_finals_per_workspace: 8,
            soft_reserve_bytes_per_workspace: 2048,
            ..small_budget()
        });
        admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 2000);
        assert_eq!(
            ledger.reserve(at(1), &terminal_in(space)),
            Err(AdmissionRejection {
                scope: RetentionScope::Daemon,
                dimension: RetentionDimension::Bytes,
            })
        );

        // Workspace bytes: the daemon still has room, this workspace does not.
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 8,
            max_bytes: 1 << 20,
            max_finals_per_workspace: 8,
            max_bytes_per_workspace: 2048,
            soft_reserve_finals: 8,
            soft_reserve_bytes: 1 << 20,
            soft_reserve_finals_per_workspace: 8,
            soft_reserve_bytes_per_workspace: 2048,
            ..small_budget()
        });
        admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 2000);
        assert_eq!(
            ledger.reserve(at(1), &terminal_in(space)),
            Err(AdmissionRejection {
                scope: RetentionScope::Workspace,
                dimension: RetentionDimension::Bytes,
            })
        );
    }

    #[test]
    fn an_admitted_runtime_commits_its_final_into_reserved_capacity() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let terminal = terminal_in(space);
        ledger.reserve(at(0), &terminal).unwrap();
        // Even a final larger than the reserved worst case is stored, not dropped.
        let commit = ledger.commit_final(&terminal, TerminalKind::Agent, 1500, at(1));
        assert_eq!(
            commit,
            FinalCommit {
                reserved: true,
                over_reserve_bytes: 476,
            }
        );
        let metrics = ledger.metrics(at(2));
        assert_eq!(metrics.retained_finals, 1);
        assert_eq!(metrics.retained_bytes, 1500);
        assert_eq!(metrics.reserved_finals, 0);
        assert_eq!(metrics.over_reserve_bytes, 476);
        assert_eq!(metrics.unreserved_commits, 0);
        assert_eq!(metrics.oldest_retained_age_secs, 1);

        // A commit with no live reservation (a restart-crossing exit) is still
        // stored, and counted so an operator sees it.
        let orphan = terminal_in(space);
        let commit = ledger.commit_final(&orphan, TerminalKind::Terminal, 8, at(2));
        assert!(!commit.reserved);
        assert_eq!(ledger.metrics(at(2)).unreserved_commits, 1);
    }

    #[test]
    fn a_duplicate_exit_keeps_the_visibility_and_lineage_it_had() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let terminal = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 100);
        assert!(ledger.note_visibility(&terminal, TerminalVisibilityState::Dismissed));
        assert!(ledger.mark_superseded(&terminal));
        assert!(ledger.set_pinned(&terminal, true));
        ledger.commit_final(&terminal, TerminalKind::Agent, 200, at(1));
        let record = ledger.lookup(&terminal).retained().unwrap();
        assert_eq!(record.visibility, TerminalVisibilityState::Dismissed);
        assert!(record.superseded);
        assert!(record.pinned);
        assert_eq!(record.bytes, 200);
        // The byte accounting followed the replacement rather than doubling.
        assert_eq!(ledger.metrics(at(1)).retained_bytes, 200);
        // Mutating an unknown key reports that it is not retained.
        let unknown = terminal_in(space);
        assert!(!ledger.note_visibility(&unknown, TerminalVisibilityState::Observed));
        assert!(!ledger.mark_superseded(&unknown));
        assert!(!ledger.set_pinned(&unknown, true));
    }

    #[test]
    fn finals_inside_the_minimum_ttl_survive_pressure_and_hold_the_cap() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let a = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 512);
        let b = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 512);
        let c = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 512);
        // Three finals cross the soft reserve, but all are inside the 10s TTL.
        assert!(ledger.under_soft_pressure());
        let report = ledger.collect(at(5));
        assert!(report.is_empty());
        assert!(!report.truncated);
        assert_eq!(report.bytes(), 0);
        for terminal in [&a, &b, &c] {
            assert!(matches!(ledger.lookup(terminal), FinalLookup::Retained(_)));
        }
        // Past the TTL the same pressure collects down to the soft reserve.
        let report = ledger.collect(at(11));
        assert_eq!(report.evicted.len(), 1);
        assert_eq!(report.bytes(), 512);
        assert_eq!(ledger.metrics(at(11)).retained_finals, 2);
    }

    #[test]
    fn pressure_evicts_dismissed_then_superseded_then_observed_then_unobserved() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 8,
            max_bytes: 8192,
            max_finals_per_workspace: 8,
            max_bytes_per_workspace: 8192,
            // Pressure starts at two retained finals, so collection relieves it
            // by evicting down to one.
            soft_reserve_finals: 2,
            soft_reserve_bytes: 8192,
            soft_reserve_finals_per_workspace: 8,
            soft_reserve_bytes_per_workspace: 8192,
            ..small_budget()
        });
        let space = workspace();
        // All four exit at the same instant and have the same size, so only the
        // class and the exact key can order them.
        let unobserved = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 100);
        let observed = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 100);
        let superseded = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 100);
        let dismissed = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 100);
        ledger.note_visibility(&observed, TerminalVisibilityState::Observed);
        ledger.mark_superseded(&superseded);
        ledger.note_visibility(&dismissed, TerminalVisibilityState::Dismissed);

        let report = ledger.collect(at(20));
        let order: Vec<&TerminalRef> = report
            .evicted
            .iter()
            .map(|marker| &marker.terminal)
            .collect();
        assert_eq!(order, vec![&dismissed, &superseded, &observed]);
        // Even an all-unobserved remainder is not protected forever: it is only
        // last in line, and it survives here because the pressure is relieved.
        assert!(matches!(
            ledger.lookup(&unobserved),
            FinalLookup::Retained(_)
        ));
        assert!(
            report
                .evicted
                .iter()
                .all(|marker| marker.reason == EvictionReason::Pressure)
        );
    }

    #[test]
    fn same_class_and_size_finals_evict_oldest_first_then_by_exact_key() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            soft_reserve_finals: 2,
            max_finals_per_workspace: 8,
            max_bytes_per_workspace: 8192,
            soft_reserve_finals_per_workspace: 8,
            soft_reserve_bytes_per_workspace: 8192,
            max_finals: 8,
            max_bytes: 8192,
            soft_reserve_bytes: 8192,
            ..small_budget()
        });
        let space = workspace();
        let old = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 100);
        let new = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(1), 100);
        let report = ledger.collect(at(30));
        assert_eq!(report.evicted[0].terminal, old);
        assert!(matches!(ledger.lookup(&new), FinalLookup::Retained(_)));

        // Same instant, same size, same class: the exact key breaks the tie the
        // same way on every run.
        let mut first = RetentionLedger::new(RetentionBudget {
            soft_reserve_finals: 2,
            ..small_budget()
        });
        let tie_a = terminal_in(space);
        let tie_b = terminal_in(space);
        first.commit_final(&tie_a, TerminalKind::Terminal, 100, at(0));
        first.commit_final(&tie_b, TerminalKind::Terminal, 100, at(0));
        let mut second = RetentionLedger::new(RetentionBudget {
            soft_reserve_finals: 2,
            ..small_budget()
        });
        // Committed in the opposite order, the same victim is chosen.
        second.commit_final(&tie_b, TerminalKind::Terminal, 100, at(0));
        second.commit_final(&tie_a, TerminalKind::Terminal, 100, at(0));
        let expected = tie_a.min(tie_b.clone());
        assert_eq!(first.collect(at(30)).evicted[0].terminal, expected);
        assert_eq!(second.collect(at(30)).evicted[0].terminal, expected);
    }

    #[test]
    fn the_age_budget_collects_without_any_pressure() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let old = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 8);
        assert!(!ledger.under_soft_pressure());
        assert!(ledger.collect(at(50)).is_empty());
        let report = ledger.collect(at(100));
        assert_eq!(report.evicted.len(), 1);
        assert_eq!(report.evicted[0].reason, EvictionReason::AgeExpired);
        let marker = ledger.lookup(&old).marker().unwrap();
        assert_eq!(marker.bytes, 8);
        assert_eq!(marker.evicted_at, at(100));
        assert_eq!(marker.kind, TerminalKind::Terminal);
    }

    #[test]
    fn a_pinned_final_survives_age_and_pressure_but_not_a_hard_cap_emergency() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let pinned = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 512);
        assert!(ledger.set_pinned(&pinned, true));
        assert!(ledger.collect(at(1000)).is_empty());
        assert!(matches!(ledger.lookup(&pinned), FinalLookup::Retained(_)));

        // Unpinning makes it an ordinary candidate again.
        assert!(ledger.set_pinned(&pinned, false));
        assert_eq!(ledger.collect(at(1000)).evicted.len(), 1);
    }

    #[test]
    fn an_over_cap_import_is_relieved_by_a_marked_emergency_eviction() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        // A migration re-imports more finals than the cap allows, all fresh and
        // one of them pinned lineage.
        let mut imported = Vec::new();
        for index in 0..5 {
            let terminal = terminal_in(space);
            let mut record =
                RetainedFinal::new(terminal.clone(), TerminalKind::Terminal, 900, at(index));
            record.pinned = index == 0;
            ledger.import_existing(record);
            imported.push(terminal);
        }
        assert_eq!(ledger.metrics(at(0)).retained_finals, 5);

        let report = ledger.collect(at(1));
        assert!(!report.is_empty());
        assert!(
            report
                .evicted
                .iter()
                .any(|marker| marker.reason == EvictionReason::Emergency)
        );
        let metrics = ledger.metrics(at(1));
        assert!(metrics.retained_finals <= 3);
        assert!(metrics.retained_bytes <= 3072);
        assert!(metrics.emergency_evictions >= 1);
        assert_eq!(metrics.unreserved_commits, 0);
        // The pinned lineage was taken last, so it is the survivor.
        assert!(matches!(
            ledger.lookup(&imported[0]),
            FinalLookup::Retained(_)
        ));
        // Nothing disappeared silently: every eviction left a typed marker.
        for terminal in &imported {
            assert!(!matches!(ledger.lookup(terminal), FinalLookup::Unknown));
        }
    }

    #[test]
    fn a_reimport_supersedes_an_earlier_eviction_marker() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let terminal = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 8);
        ledger.collect(at(200));
        assert!(matches!(ledger.lookup(&terminal), FinalLookup::Evicted(_)));
        ledger.import_existing(RetainedFinal::new(
            terminal.clone(),
            TerminalKind::Terminal,
            8,
            at(200),
        ));
        assert!(matches!(ledger.lookup(&terminal), FinalLookup::Retained(_)));
        // A second import of the same key does not double-count its bytes.
        ledger.import_existing(RetainedFinal::new(
            terminal.clone(),
            TerminalKind::Terminal,
            8,
            at(200),
        ));
        assert_eq!(ledger.metrics(at(200)).retained_bytes, 8);
        assert_eq!(ledger.metrics(at(200)).retained_finals, 1);
    }

    #[test]
    fn gc_work_is_bounded_per_pass_and_converges_across_passes() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 64,
            max_bytes: 1 << 20,
            max_finals_per_workspace: 64,
            max_bytes_per_workspace: 1 << 20,
            soft_reserve_finals: 64,
            soft_reserve_bytes: 1 << 20,
            soft_reserve_finals_per_workspace: 64,
            soft_reserve_bytes_per_workspace: 1 << 20,
            max_gc_batch: 2,
            ..small_budget()
        });
        let space = workspace();
        for _ in 0..5 {
            admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 8);
        }
        // All five are past the age budget; the batch bound stops at two.
        let first = ledger.collect(at(1000));
        assert_eq!(first.evicted.len(), 2);
        assert!(first.truncated);
        let mut passes = 1;
        while !ledger.collect(at(1000)).is_empty() {
            passes += 1;
            assert!(passes < 10, "bounded passes must converge");
        }
        assert_eq!(ledger.metrics(at(1000)).retained_finals, 0);
        // The final pass reports no leftover work.
        assert!(!ledger.collect(at(1000)).truncated);
    }

    #[test]
    fn the_marker_window_is_bounded_and_reports_what_it_forgets() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_eviction_markers: 2,
            max_gc_batch: 16,
            ..small_budget()
        });
        let space = workspace();
        let mut evicted = Vec::new();
        for index in 0..4 {
            let terminal = terminal_in(space);
            ledger.import_existing(RetainedFinal::new(
                terminal.clone(),
                TerminalKind::Terminal,
                8,
                at(index),
            ));
            evicted.push(terminal);
        }
        ledger.collect(at(1000));
        assert_eq!(ledger.metrics(at(1000)).evicted_finals, 4);
        assert_eq!(ledger.metrics(at(1000)).forgotten_markers, 2);
        // The two oldest markers aged out; the newest two still answer typed.
        assert_eq!(ledger.lookup(&evicted[0]), FinalLookup::Unknown);
        assert!(matches!(
            ledger.lookup(&evicted[3]),
            FinalLookup::Evicted(_)
        ));

        // A zero-length marker window keeps nothing but still counts.
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_eviction_markers: 0,
            ..small_budget()
        });
        let terminal = terminal_in(space);
        ledger.import_existing(RetainedFinal::new(
            terminal.clone(),
            TerminalKind::Terminal,
            8,
            at(0),
        ));
        ledger.collect(at(1000));
        assert_eq!(ledger.lookup(&terminal), FinalLookup::Unknown);
        assert_eq!(ledger.metrics(at(1000)).forgotten_markers, 1);
    }

    #[test]
    fn retained_lists_each_owner_kind_separately() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let generic = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 8);
        let agent = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 8);
        let generics = ledger.retained(TerminalKind::Terminal);
        assert_eq!(generics.len(), 1);
        assert_eq!(generics[0].terminal, generic);
        let agents = ledger.retained(TerminalKind::Agent);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].terminal, agent);
    }

    #[test]
    fn workspace_pressure_collects_only_the_pressured_workspace() {
        let mut ledger = RetentionLedger::new(RetentionBudget {
            max_finals: 16,
            max_bytes: 1 << 20,
            max_finals_per_workspace: 4,
            max_bytes_per_workspace: 1 << 20,
            soft_reserve_finals: 16,
            soft_reserve_bytes: 1 << 20,
            soft_reserve_finals_per_workspace: 2,
            soft_reserve_bytes_per_workspace: 1 << 20,
            ..small_budget()
        });
        let busy = workspace();
        let quiet = workspace();
        for _ in 0..3 {
            admit_and_exit(&mut ledger, busy, TerminalKind::Terminal, at(0), 8);
        }
        let calm = admit_and_exit(&mut ledger, quiet, TerminalKind::Terminal, at(0), 8);
        let report = ledger.collect(at(50));
        assert_eq!(report.evicted.len(), 2);
        assert!(
            report
                .evicted
                .iter()
                .all(|marker| marker.terminal.workspace_id == busy)
        );
        // The quiet workspace was never under pressure, so it kept its final.
        assert!(matches!(ledger.lookup(&calm), FinalLookup::Retained(_)));
        assert!(!ledger.under_soft_pressure());
    }

    #[test]
    fn an_exhausted_workspace_recovers_capacity_through_admission_time_gc() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        for _ in 0..3 {
            admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 1024);
        }
        // Inside the TTL the workspace is full and the launch is refused.
        assert!(ledger.reserve(at(5), &terminal_in(space)).is_err());
        // Past the TTL the same launch admits: the reserve path collected first.
        assert!(ledger.reserve(at(30), &terminal_in(space)).is_ok());
        assert!(ledger.metrics(at(30)).evicted_finals >= 1);
    }

    #[test]
    fn a_hundred_thousand_short_lived_runtimes_stay_inside_the_hard_caps() {
        let budget = RetentionBudget {
            max_finals: 16,
            max_bytes: 16 * 1024,
            max_finals_per_workspace: 8,
            max_bytes_per_workspace: 8 * 1024,
            soft_reserve_finals: 12,
            soft_reserve_bytes: 12 * 1024,
            soft_reserve_finals_per_workspace: 6,
            soft_reserve_bytes_per_workspace: 6 * 1024,
            min_visibility_ttl_secs: 2,
            max_final_age_secs: 60,
            worst_case_final_bytes: 1024,
            max_gc_batch: 32,
            max_eviction_markers: 64,
        };
        let mut ledger = RetentionLedger::new(budget);
        let templates = [terminal_in(workspace()), terminal_in(workspace())];
        // Every runtime exits unobserved and is never dismissed: the workload
        // that an indefinitely protected final would make unbounded.
        for index in 0..100_000_i64 {
            let mut terminal =
                templates[usize::try_from(index).unwrap_or(0) % templates.len()].clone();
            terminal.terminal_id = TerminalId::new();
            let now = at(index);
            // Collection keeps admission flowing: nothing is rejected while the
            // TTL of the oldest finals keeps expiring.
            ledger.reserve(now, &terminal).expect("gc keeps headroom");
            ledger.commit_final(&terminal, TerminalKind::Agent, 1024, now);
            if index % 97 == 0 {
                let metrics = ledger.metrics(now);
                assert!(metrics.retained_finals <= budget.max_finals);
                assert!(metrics.retained_bytes <= budget.max_bytes);
            }
        }
        let metrics = ledger.metrics(at(100_000));
        assert!(metrics.retained_finals <= budget.max_finals);
        assert!(metrics.retained_bytes <= budget.max_bytes);
        assert!(metrics.evicted_finals > 90_000);
        assert_eq!(metrics.emergency_evictions, 0);
        assert_eq!(metrics.unreserved_commits, 0);
        assert_eq!(metrics.admission_rejections, 0);
    }

    #[test]
    fn a_saturated_ledger_backpressures_launches_instead_of_dropping_finals() {
        // Every final is pinned lineage, so GC can free nothing under pressure.
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let mut pinned = Vec::new();
        for _ in 0..3 {
            let terminal = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 1024);
            ledger.set_pinned(&terminal, true);
            pinned.push(terminal);
        }
        for attempt in 0..5 {
            assert!(
                ledger
                    .reserve(at(1000 + attempt), &terminal_in(space))
                    .is_err()
            );
        }
        assert_eq!(ledger.metrics(at(1000)).admission_rejections, 5);
        assert_eq!(ledger.metrics(at(1000)).evicted_finals, 0);
        for terminal in &pinned {
            assert!(matches!(ledger.lookup(terminal), FinalLookup::Retained(_)));
        }
    }

    #[test]
    fn an_unknown_key_is_never_answered_with_another_runtimes_history() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let known = admit_and_exit(&mut ledger, space, TerminalKind::Terminal, at(0), 8);
        let stranger = terminal_in(space);
        assert_eq!(ledger.lookup(&stranger), FinalLookup::Unknown);
        assert!(ledger.lookup(&stranger).retained().is_none());
        assert!(ledger.lookup(&stranger).marker().is_none());
        assert!(ledger.lookup(&known).marker().is_none());
        // A different incarnation of the same workspace is still unknown.
        let mut other_generation = known.clone();
        other_generation.daemon_generation = DaemonGeneration::new();
        assert_eq!(ledger.lookup(&other_generation), FinalLookup::Unknown);
    }

    #[test]
    fn the_metrics_snapshot_carries_only_counts_bytes_and_ages() {
        let mut ledger = RetentionLedger::new(small_budget());
        let space = workspace();
        let terminal = admit_and_exit(&mut ledger, space, TerminalKind::Agent, at(0), 512);
        ledger.reserve(at(0), &terminal_in(space)).unwrap();
        let metrics = ledger.metrics(at(7));
        assert_eq!(metrics.retained_finals, 1);
        assert_eq!(metrics.reserved_finals, 1);
        assert_eq!(metrics.oldest_retained_age_secs, 7);
        assert!(!metrics.soft_pressure);
        // The snapshot is plain data: it round-trips without any identity.
        let encoded = serde_json::to_string(&metrics).unwrap();
        assert!(!encoded.contains(&terminal.terminal_id.as_str()));
        assert_eq!(
            serde_json::from_str::<RetentionMetrics>(&encoded).unwrap(),
            metrics
        );
        assert_eq!(RetentionMetrics::default().retained_finals, 0);
    }

    #[test]
    fn markers_and_records_round_trip_through_json() {
        let space = workspace();
        let record = RetainedFinal::new(terminal_in(space), TerminalKind::Agent, 42, at(3));
        let encoded = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<RetainedFinal>(&encoded).unwrap(),
            record
        );
        let marker = EvictionMarker {
            terminal: record.terminal.clone(),
            kind: record.kind,
            reason: EvictionReason::Pressure,
            evicted_at: at(9),
            bytes: 42,
        };
        let encoded = serde_json::to_string(&marker).unwrap();
        assert!(encoded.contains("\"pressure\""));
        assert_eq!(
            serde_json::from_str::<EvictionMarker>(&encoded).unwrap(),
            marker
        );
        let rejection = AdmissionRejection {
            scope: RetentionScope::Workspace,
            dimension: RetentionDimension::Bytes,
        };
        let encoded = serde_json::to_string(&rejection).unwrap();
        assert!(encoded.contains("\"workspace\"") && encoded.contains("\"bytes\""));
        assert_eq!(
            serde_json::from_str::<AdmissionRejection>(&encoded).unwrap(),
            rejection
        );
        assert_eq!(
            serde_json::from_str::<FinalClass>("\"dismissed\"").unwrap(),
            FinalClass::Dismissed
        );
        // Debug and Clone participate in coverage through the ledger too.
        let ledger = RetentionLedger::new(small_budget());
        assert!(format!("{:?}", ledger.clone()).contains("RetentionLedger"));
        assert_eq!(
            format!("{:?}", GcReport::default()),
            format!("{:?}", GcReport::default())
        );
    }
}
