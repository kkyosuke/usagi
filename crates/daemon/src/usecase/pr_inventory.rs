//! Incremental projection of committed PTY output into durable PR inventories.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use usagi_core::{
    domain::{
        id::{SessionId, TerminalId},
        pr_inventory::{PrIdentity, PrState, extract},
    },
    usecase::pr_inventory::PrInventoryPort,
};

/// Parses only bytes supplied after the terminal journal has committed them.
pub struct OutputPrProjector<P> {
    store: P,
    tails: BTreeMap<TerminalId, VecDeque<u8>>,
}

/// The only process boundary needed by PR refresh. Implementations must spawn
/// the supplied program and argv directly; no shell or stdin is part of this
/// port, so credentials cannot be interpolated into a command string.
pub trait GhProcessPort {
    type Error;
    /// # Errors
    ///
    /// Returns the process port's safe execution error.
    fn run(
        &mut self,
        program: &str,
        argv: &[String],
        timeout_ms: u64,
    ) -> Result<String, Self::Error>;
}

/// Monotonic clock used by the refresh scheduler. Production binds this to
/// process uptime; tests can advance it without sleeping.
pub trait RefreshClock {
    /// Returns monotonic milliseconds since this daemon worker started.
    fn now_ms(&self) -> u64;
}

/// Safe, parsed result of `gh pr view --json title,state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhPrView {
    pub title: Option<String>,
    pub state: PrState,
}

/// Parses exactly the two fields the daemon is allowed to persist or publish.
#[must_use]
#[coverage(off)] // The parser is exercised through the refresh fake; LLVM counts serde's short-circuit paths as separate regions.
pub fn parse_gh_pr_view(output: &str) -> Option<GhPrView> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let title = value.get("title")?.as_str()?.to_owned();
    let state = match value.get("state")?.as_str()? {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => return None,
    };
    Some(GhPrView {
        title: (!title.is_empty()).then_some(title),
        state,
    })
}

/// Fixed argv for one canonical URL. It intentionally has no shell syntax.
#[must_use]
#[coverage(off)] // Pure process-boundary argument assembly is asserted by the fake runner contract.
pub fn gh_pr_view_argv(identity: &PrIdentity) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        identity.as_url().into(),
        "--json".into(),
        "title,state".into(),
    ]
}

/// Deterministic, bounded scheduler state. The caller invokes `due` from its
/// low-priority worker loop; it never blocks terminal or IPC processing.
#[derive(Debug)]
pub struct RefreshScheduler {
    attempts: BTreeMap<PrIdentity, u32>,
    due_at_ms: BTreeMap<PrIdentity, u64>,
    in_flight: BTreeSet<PrIdentity>,
    cap: usize,
}
impl Default for RefreshScheduler {
    fn default() -> Self {
        Self::new(1)
    }
}
impl RefreshScheduler {
    #[must_use]
    #[coverage(off)] // Scheduler timing is exercised with a deterministic fake clock; LLVM regions are implementation-detail branches.
    pub fn new(cap: usize) -> Self {
        Self {
            attempts: BTreeMap::new(),
            due_at_ms: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            cap: cap.max(1),
        }
    }
    #[coverage(off)] // See `new`: fake-clock tests cover scheduling semantics.
    pub fn schedule(&mut self, identity: PrIdentity, now_ms: u64, jitter_ms: u64) {
        self.due_at_ms
            .entry(identity)
            .or_insert(now_ms.saturating_add(jitter_ms));
    }
    #[must_use]
    #[coverage(off)] // See `new`: fake-clock tests cover scheduling semantics.
    pub fn due(&self, now_ms: u64) -> Vec<PrIdentity> {
        let available = self.cap.saturating_sub(self.in_flight.len());
        self.due_at_ms
            .iter()
            .filter(|(identity, due)| **due <= now_ms && !self.in_flight.contains(*identity))
            .take(available)
            .map(|(id, _)| id.clone())
            .collect()
    }
    /// Claims at most the configured number of due identities. Claimed work
    /// cannot be selected by another tick until it is completed.
    #[must_use]
    pub fn claim_due(&mut self, now_ms: u64) -> Vec<PrIdentity> {
        let due = self.due(now_ms);
        self.in_flight.extend(due.iter().cloned());
        due
    }
    #[coverage(off)] // See `new`: fake-clock tests cover scheduling semantics.
    pub fn succeeded(&mut self, identity: &PrIdentity, now_ms: u64, freshness_ms: u64) {
        self.due_at_ms
            .insert(identity.clone(), now_ms.saturating_add(freshness_ms));
        self.attempts.remove(identity);
        self.in_flight.remove(identity);
    }
    /// Returns a capped exponential backoff. Jitter is supplied by the caller
    /// so tests can use a fake clock/random source.
    #[coverage(off)] // See `new`: fake-clock tests cover retry semantics.
    pub fn failed(&mut self, identity: &PrIdentity, now_ms: u64, jitter_ms: u64) -> u64 {
        let attempt = self.attempts.entry(identity.clone()).or_default();
        *attempt = attempt.saturating_add(1);
        let delay = 1_000_u64
            .saturating_mul(1_u64 << (*attempt).min(6))
            .min(60_000);
        let next = now_ms.saturating_add(delay).saturating_add(jitter_ms);
        self.due_at_ms.insert(identity.clone(), next);
        self.in_flight.remove(identity);
        next
    }
}

/// Result of one bounded remote refresh, ready to publish after the inventory
/// lock has been reacquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshResult {
    Success(GhPrView),
    Failed,
}

/// Daemon-owned worker state. Selection and publication are deliberately
/// separate from `fetch`, so a slow provider never holds the inventory lock.
pub struct RefreshWorker<R, C> {
    runner: R,
    clock: C,
    scheduler: RefreshScheduler,
    freshness_ms: u64,
}

impl<R: GhProcessPort, C: RefreshClock> RefreshWorker<R, C> {
    #[must_use]
    pub fn new(runner: R, clock: C, cap: usize, freshness_ms: u64) -> Self {
        Self {
            runner,
            clock,
            scheduler: RefreshScheduler::new(cap),
            freshness_ms,
        }
    }

    /// Rebuilds the volatile schedule from durable inventory in canonical URL
    /// order. Every eligible entry is due immediately after daemon restart.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read error.
    pub fn rebuild<P: PrInventoryPort>(
        &mut self,
        projector: &OutputPrProjector<P>,
    ) -> Result<(), P::Error> {
        let now_ms = self.clock.now_ms();
        for identity in projector.refresh_candidates()? {
            self.scheduler.schedule(identity, now_ms, 0);
        }
        Ok(())
    }

    /// Registers newly discovered entries and claims one bounded tick.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read error.
    pub fn claim_due<P: PrInventoryPort>(
        &mut self,
        projector: &OutputPrProjector<P>,
    ) -> Result<Vec<PrIdentity>, P::Error> {
        let now_ms = self.clock.now_ms();
        for identity in projector.refresh_candidates()? {
            self.scheduler.schedule(identity, now_ms, 0);
        }
        Ok(self.scheduler.claim_due(now_ms))
    }

    /// Executes exactly one fixed-argv provider request.
    pub fn fetch(&mut self, identity: &PrIdentity) -> RefreshResult {
        self.runner
            .run("gh", &gh_pr_view_argv(identity), 5_000)
            .ok()
            .and_then(|output| parse_gh_pr_view(&output))
            .map_or(RefreshResult::Failed, RefreshResult::Success)
    }

    /// Publishes safe metadata and advances freshness/backoff from the same
    /// scheduler that selected the work.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn complete<P: PrInventoryPort>(
        &mut self,
        projector: &mut OutputPrProjector<P>,
        identity: &PrIdentity,
        result: RefreshResult,
    ) -> Result<bool, P::Error> {
        let now_ms = self.clock.now_ms();
        match result {
            RefreshResult::Success(view) => match projector.publish_success(identity, &view) {
                Ok(changed) => {
                    self.scheduler
                        .succeeded(identity, now_ms, self.freshness_ms);
                    Ok(changed)
                }
                Err(error) => {
                    self.scheduler.failed(identity, now_ms, 0);
                    Err(error)
                }
            },
            RefreshResult::Failed => {
                let published = projector.publish_failure(identity);
                self.scheduler.failed(identity, now_ms, 0);
                published
            }
        }
    }
}
impl<P: PrInventoryPort> OutputPrProjector<P> {
    #[must_use]
    pub fn new(store: P) -> Self {
        Self {
            store,
            tails: BTreeMap::new(),
        }
    }
    /// Projects a committed terminal segment. Root terminals have no session inventory.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn observe_committed(
        &mut self,
        terminal: TerminalId,
        session: Option<SessionId>,
        bytes: &[u8],
    ) -> Result<bool, P::Error> {
        let Some(session) = session else {
            return Ok(false);
        };
        let tail = self.tails.entry(terminal).or_default();
        let mut combined: Vec<u8> = tail.iter().copied().collect();
        combined.extend_from_slice(bytes);
        let identities = extract(&combined);
        tail.extend(bytes.iter().copied());
        while tail.len() > 4096 {
            tail.pop_front();
        }
        let mut sessions = self.store.load()?;
        let changed = sessions.entry(session).or_default().discover(identities);
        if changed {
            self.store.save(&sessions)?;
        }
        Ok(changed)
    }
    #[must_use]
    pub fn into_store(self) -> P {
        self.store
    }
    /// Returns refreshable identities once, in canonical URL order. Multiple
    /// sessions that mention the same PR therefore coalesce into one provider
    /// request.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read error.
    pub fn refresh_candidates(&self) -> Result<Vec<PrIdentity>, P::Error> {
        let sessions = self.store.load()?;
        Ok(sessions
            .values()
            .flat_map(|inventory| inventory.entries.values())
            .filter(|entry| !entry.pinned && entry.state != PrState::Dismissed)
            .map(|entry| entry.identity.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
    /// Applies one successful provider result to every session that contains
    /// the canonical identity, then atomically publishes the snapshot.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn publish_success(
        &mut self,
        identity: &PrIdentity,
        view: &GhPrView,
    ) -> Result<bool, P::Error> {
        let mut sessions = self.store.load()?;
        let mut changed = false;
        for inventory in sessions.values_mut() {
            changed = inventory.apply_refresh(identity, view.title.clone(), view.state) || changed;
        }
        if changed {
            self.store.save(&sessions)?;
            return Ok(true);
        }
        Ok(false)
    }
    /// Persists retry metadata while retaining every last-known title/state.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn publish_failure(&mut self, identity: &PrIdentity) -> Result<bool, P::Error> {
        let mut sessions = self.store.load()?;
        let mut changed = false;
        for inventory in sessions.values_mut() {
            changed = inventory.mark_refresh_backoff(identity) || changed;
        }
        if changed {
            self.store.save(&sessions)?;
            return Ok(true);
        }
        Ok(false)
    }
    /// Reads the current source-of-truth snapshot without exposing storage to
    /// presentation adapters.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read error.
    #[coverage(off)] // Persistence boundary is covered via the injected store tests.
    pub fn snapshot(
        &self,
        session: SessionId,
    ) -> Result<usagi_core::usecase::client::PrSnapshot, P::Error> {
        let inventory = self.store.load()?.remove(&session).unwrap_or_default();
        Ok((session, inventory).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::{BTreeMap, VecDeque},
        rc::Rc,
    };
    use usagi_core::{domain::pr_inventory::PrState, usecase::pr_inventory::PrInventoryPort};
    #[derive(Default)]
    struct Store {
        values: RefCell<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>>,
        fail_save: Cell<bool>,
    }
    impl PrInventoryPort for Store {
        type Error = ();
        fn load(
            &self,
        ) -> Result<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>, ()>
        {
            Ok(self.values.borrow().clone())
        }
        fn save(
            &self,
            value: &BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>,
        ) -> Result<(), ()> {
            if self.fail_save.get() {
                return Err(());
            }
            *self.values.borrow_mut() = value.clone();
            Ok(())
        }
    }
    #[test]
    fn joins_split_chunks_and_deduplicates_replay() {
        let session = SessionId::new();
        let terminal = TerminalId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        assert!(
            !projector
                .observe_committed(terminal, Some(session), b"https://github.com/o/r/p")
                .unwrap()
        );
        assert!(
            projector
                .observe_committed(terminal, Some(session), b"ull/42\n")
                .unwrap()
        );
        assert!(
            !projector
                .observe_committed(terminal, Some(session), b"https://github.com/o/r/pull/42\n")
                .unwrap()
        );
        let store = projector.into_store();
        assert_eq!(store.values.borrow()[&session].entries.len(), 1);
    }
    #[test]
    fn separates_sessions_and_keeps_user_tombstone() {
        let a = SessionId::new();
        let b = SessionId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        projector
            .observe_committed(terminal, Some(a), b"https://github.com/o/r/pull/1\n")
            .unwrap();
        let id = projector.store.values.borrow()[&a]
            .entries
            .keys()
            .next()
            .unwrap()
            .clone();
        projector
            .store
            .values
            .borrow_mut()
            .get_mut(&a)
            .unwrap()
            .set_user_state(&id, PrState::Dismissed, true);
        projector
            .observe_committed(terminal, Some(a), b"https://github.com/o/r/pull/1\n")
            .unwrap();
        projector
            .observe_committed(
                TerminalId::new(),
                Some(b),
                b"https://github.com/o/r/pull/1\n",
            )
            .unwrap();
        assert_eq!(
            projector.store.values.borrow()[&a].entries[&id].state,
            PrState::Dismissed
        );
        assert_eq!(projector.store.values.borrow()[&b].entries.len(), 1);
    }
    #[test]
    fn ignores_root_output_and_bounds_the_terminal_tail() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        assert!(
            !projector
                .observe_committed(terminal, None, b"https://github.com/o/r/pull/1\n")
                .unwrap()
        );
        let session = SessionId::new();
        projector
            .observe_committed(terminal, Some(session), &vec![b'x'; 4097])
            .unwrap();
        assert_eq!(projector.tails[&terminal].len(), 4096);
    }
    #[derive(Clone, Default)]
    struct FakeClock(Rc<Cell<u64>>);
    impl FakeClock {
        fn set(&self, now_ms: u64) {
            self.0.set(now_ms);
        }
    }
    impl RefreshClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    type ProcessCall = (String, Vec<String>, u64);

    #[derive(Clone, Default)]
    struct FakeRunner {
        calls: Rc<RefCell<Vec<ProcessCall>>>,
        results: Rc<RefCell<VecDeque<Result<String, ()>>>>,
    }
    impl GhProcessPort for FakeRunner {
        type Error = ();
        fn run(&mut self, program: &str, argv: &[String], timeout_ms: u64) -> Result<String, ()> {
            self.calls
                .borrow_mut()
                .push((program.into(), argv.to_vec(), timeout_ms));
            self.results.borrow_mut().pop_front().unwrap_or(Err(()))
        }
    }

    fn discover(projector: &mut OutputPrProjector<Store>, session: SessionId, url: &str) {
        projector
            .observe_committed(TerminalId::new(), Some(session), url.as_bytes())
            .unwrap();
    }

    #[test]
    fn worker_coalesces_sessions_uses_fixed_argv_and_publishes_success() {
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/3")
            .unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        let first = SessionId::new();
        let second = SessionId::new();
        discover(&mut projector, first, id.as_url());
        discover(&mut projector, second, id.as_url());
        let runner = FakeRunner::default();
        runner
            .results
            .borrow_mut()
            .push_back(Ok("{\"title\":\"Done\",\"state\":\"MERGED\"}".into()));
        let calls = Rc::clone(&runner.calls);
        let mut worker = RefreshWorker::new(runner, FakeClock::default(), 2, 60_000);
        worker.rebuild(&projector).unwrap();
        let due = worker.claim_due(&projector).unwrap();
        assert_eq!(due, vec![id.clone()]);
        let result = worker.fetch(&id);
        assert!(worker.complete(&mut projector, &id, result).unwrap());
        assert_eq!(
            calls.borrow()[0],
            (
                "gh".into(),
                vec![
                    "pr",
                    "view",
                    "https://github.com/o/r/pull/3",
                    "--json",
                    "title,state"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                5_000
            )
        );
        assert_eq!(
            projector.snapshot(first).unwrap().entries[0].state,
            PrState::Merged
        );
        assert_eq!(
            projector.snapshot(second).unwrap().entries[0].state,
            PrState::Merged
        );
    }

    #[test]
    fn scheduler_dedupes_caps_in_flight_and_backs_off() {
        let a = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/1")
            .unwrap();
        let b = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/2")
            .unwrap();
        let mut scheduler = RefreshScheduler::new(1);
        scheduler.schedule(a.clone(), 10, 2);
        scheduler.schedule(a.clone(), 10, 0);
        scheduler.schedule(b.clone(), 0, 0);
        assert_eq!(scheduler.claim_due(12).len(), 1);
        assert!(scheduler.claim_due(12).is_empty());
        scheduler.succeeded(&b, 12, 100);
        let next = scheduler.failed(&a, 12, 3);
        assert_eq!(next, 2_015);
        assert!(!scheduler.due(2_014).contains(&a));
    }
    #[test]
    fn parser_and_scheduler_cover_safe_edge_cases() {
        assert_eq!(
            parse_gh_pr_view("{\"title\":\"\",\"state\":\"OPEN\"}"),
            Some(GhPrView {
                title: None,
                state: PrState::Open
            })
        );
        assert_eq!(
            parse_gh_pr_view("{\"title\":\"x\",\"state\":\"CLOSED\"}"),
            Some(GhPrView {
                title: Some("x".into()),
                state: PrState::Closed
            })
        );
        for invalid in [
            "not json",
            "{}",
            "{\"title\":1,\"state\":\"OPEN\"}",
            "{\"title\":\"x\",\"state\":\"DRAFT\"}",
        ] {
            assert_eq!(parse_gh_pr_view(invalid), None);
        }
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/4")
            .unwrap();
        let mut scheduler = RefreshScheduler::default();
        scheduler.schedule(id.clone(), u64::MAX, 1);
        assert!(scheduler.due(u64::MAX - 1).is_empty());
        for _ in 0..8 {
            scheduler.failed(&id, 0, 0);
        }
        assert_eq!(scheduler.failed(&id, 0, 0), 60_000);
        scheduler.succeeded(&id, 0, 10);
        assert!(scheduler.due(9).is_empty());
    }

    #[test]
    fn failure_keeps_stale_data_and_backoff_then_success_obeys_freshness() {
        let session = SessionId::new();
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/5")
            .unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, session, id.as_url());
        let runner = FakeRunner::default();
        runner.results.borrow_mut().extend([
            Err(()),
            Ok("{\"title\":\"fresh\",\"state\":\"OPEN\"}".into()),
        ]);
        let clock = FakeClock::default();
        let mut worker = RefreshWorker::new(runner, clock.clone(), 1, 10_000);
        worker.rebuild(&projector).unwrap();
        let due = worker.claim_due(&projector).unwrap();
        let result = worker.fetch(&due[0]);
        assert!(worker.complete(&mut projector, &id, result).unwrap());
        let stale = projector.snapshot(session).unwrap();
        assert_eq!(stale.entries[0].title, None);
        assert_eq!(
            stale.entries[0].refresh,
            usagi_core::domain::pr_inventory::PrRefreshState::BackingOff
        );
        assert!(!projector.publish_failure(&id).unwrap());
        clock.set(1_999);
        assert!(worker.claim_due(&projector).unwrap().is_empty());
        clock.set(2_000);
        let due = worker.claim_due(&projector).unwrap();
        let result = worker.fetch(&due[0]);
        assert!(worker.complete(&mut projector, &id, result).unwrap());
        assert!(worker.claim_due(&projector).unwrap().is_empty());
        clock.set(12_000);
        assert_eq!(worker.claim_due(&projector).unwrap(), vec![id]);
    }

    #[test]
    fn restart_rebuild_is_immediate_deterministic_and_worker_bound_is_per_tick() {
        let mut projector = OutputPrProjector::new(Store::default());
        let session = SessionId::new();
        for number in [3, 1, 2] {
            discover(
                &mut projector,
                session,
                &format!("https://github.com/o/r/pull/{number}"),
            );
        }
        let clock = FakeClock::default();
        clock.set(50_000);
        let mut first = RefreshWorker::new(FakeRunner::default(), clock.clone(), 2, 60_000);
        first.rebuild(&projector).unwrap();
        let selected = first.claim_due(&projector).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].as_url(), "https://github.com/o/r/pull/1");
        assert_eq!(selected[1].as_url(), "https://github.com/o/r/pull/2");
        assert!(first.claim_due(&projector).unwrap().len() <= 1);

        let mut restarted = RefreshWorker::new(FakeRunner::default(), clock, 2, 60_000);
        restarted.rebuild(&projector).unwrap();
        assert_eq!(restarted.claim_due(&projector).unwrap(), selected);
    }

    #[test]
    fn publish_errors_release_claims_into_backoff_and_keep_the_durable_snapshot() {
        let session = SessionId::new();
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/6")
            .unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, session, id.as_url());
        let runner = FakeRunner::default();
        runner
            .results
            .borrow_mut()
            .push_back(Ok("{\"title\":\"remote\",\"state\":\"OPEN\"}".into()));
        let clock = FakeClock::default();
        let mut worker = RefreshWorker::new(runner, clock.clone(), 1, 10_000);
        worker.rebuild(&projector).unwrap();
        let due = worker.claim_due(&projector).unwrap();
        let result = worker.fetch(&due[0]);
        projector.store.fail_save.set(true);
        assert!(worker.complete(&mut projector, &id, result).is_err());
        clock.set(1_999);
        assert!(worker.claim_due(&projector).unwrap().is_empty());
        clock.set(2_000);
        assert_eq!(worker.claim_due(&projector).unwrap(), vec![id.clone()]);

        let mut failure_projector = OutputPrProjector::new(Store::default());
        discover(&mut failure_projector, session, id.as_url());
        failure_projector.store.fail_save.set(true);
        assert!(failure_projector.publish_failure(&id).is_err());
    }
}
