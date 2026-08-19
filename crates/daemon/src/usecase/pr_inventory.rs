//! Incremental projection of committed PTY output into durable PR inventories.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
use usagi_core::{
    domain::{
        id::{SessionId, TerminalId},
        pr_inventory::{
            CANDIDATE_PREFIX_MAX, PrChecksState, PrIdentity, PrInventory, PrRefreshMetadata,
            PrReviewDecision, PrState, canonicalize, extract, is_candidate_terminator,
        },
    },
    usecase::pr_inventory::PrInventoryPort,
};

/// How many terminals may hold a carry buffer at once.
///
/// [`OutputPrProjector::release_terminal`] reclaims eagerly on exit, so this only
/// backstops a terminal whose exit this projector never observes. Each carry is
/// at most [`CANDIDATE_PREFIX_MAX`] bytes.
const CARRY_TERMINALS_MAX: usize = 256;

/// Parses only bytes supplied after the terminal journal has committed them.
///
/// The durable snapshot is cached in memory and written through, so projecting a
/// chunk performs no read. A chunk that mentions no PR at all — nearly every
/// chunk — does not touch the store in either direction.
///
/// Detection is incremental: only the region up to the last candidate terminator
/// is extracted, and the unterminated remainder is carried into the next chunk.
/// A truncated token is therefore never canonicalized, so a chunk that cuts
/// `pull/423` after `pull/42` cannot record the wrong PR.
pub struct OutputPrProjector<P> {
    store: P,
    hydrated: bool,
    sessions: BTreeMap<SessionId, PrInventory>,
    carries: BTreeMap<TerminalId, Vec<u8>>,
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

/// Safe, parsed result of `gh pr view`'s allowlisted presentation fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhPrView {
    pub title: Option<String>,
    pub state: PrState,
    pub head_oid: String,
    pub draft: bool,
    pub checks: Option<PrChecksState>,
    pub review: Option<PrReviewDecision>,
}

/// Parses exactly the fields the daemon is allowed to persist or publish.
#[must_use]
pub fn parse_gh_pr_view(output: &str) -> Option<GhPrView> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let title = value.get("title")?.as_str()?.to_owned();
    let state = match value.get("state")?.as_str()? {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => return None,
    };
    let head_oid = value.get("headRefOid")?.as_str()?.to_owned();
    if !((head_oid.len() == 40 || head_oid.len() == 64)
        && head_oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    let draft = value
        .get("isDraft")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let review = match value
        .get("reviewDecision")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
    {
        "" => None,
        "APPROVED" => Some(PrReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(PrReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(PrReviewDecision::ReviewRequired),
        _ => return None,
    };
    let checks = match value.get("statusCheckRollup") {
        Some(value) => parse_checks(value).ok()?,
        None => None,
    };
    Some(GhPrView {
        title: (!title.is_empty()).then_some(title),
        state,
        head_oid,
        draft,
        checks,
        review,
    })
}

fn parse_checks(value: &serde_json::Value) -> Result<Option<PrChecksState>, ()> {
    let checks = value.as_array().ok_or(())?;
    if checks.is_empty() {
        return Ok(None);
    }
    let mut pending = false;
    for check in checks {
        let token = check
            .get("conclusion")
            .and_then(serde_json::Value::as_str)
            .or_else(|| check.get("state").and_then(serde_json::Value::as_str));
        match token.unwrap_or("PENDING") {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" | "STALE" => return Ok(Some(PrChecksState::Failing)),
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            _ => pending = true,
        }
    }
    Ok(Some(if pending {
        PrChecksState::Pending
    } else {
        PrChecksState::Passing
    }))
}

/// Fixed argv for one canonical URL. It intentionally has no shell syntax.
#[must_use]
pub fn gh_pr_view_argv(identity: &PrIdentity) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        identity.as_url().into(),
        "--json".into(),
        "title,state,headRefOid,isDraft,reviewDecision,statusCheckRollup".into(),
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
    pub fn new(cap: usize) -> Self {
        Self {
            attempts: BTreeMap::new(),
            due_at_ms: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            cap: cap.max(1),
        }
    }
    pub fn schedule(&mut self, identity: PrIdentity, now_ms: u64, jitter_ms: u64) {
        self.due_at_ms
            .entry(identity)
            .or_insert(now_ms.saturating_add(jitter_ms));
    }
    /// Drops work that is no longer present in the durable eligible set.
    pub fn retain(&mut self, eligible: &BTreeSet<PrIdentity>) {
        self.due_at_ms
            .retain(|identity, _| eligible.contains(identity));
        self.attempts
            .retain(|identity, _| eligible.contains(identity));
        self.in_flight
            .retain(|identity| eligible.contains(identity));
    }
    #[must_use]
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
    pub fn succeeded(&mut self, identity: &PrIdentity, now_ms: u64, freshness_ms: u64) {
        self.due_at_ms
            .insert(identity.clone(), now_ms.saturating_add(freshness_ms));
        self.attempts.remove(identity);
        self.in_flight.remove(identity);
    }
    pub fn retire(&mut self, identity: &PrIdentity) {
        self.due_at_ms.remove(identity);
        self.attempts.remove(identity);
        self.in_flight.remove(identity);
    }
    /// Returns a capped exponential backoff. Jitter is supplied by the caller
    /// so tests can use a fake clock/random source.
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
        projector: &mut OutputPrProjector<P>,
    ) -> Result<(), P::Error> {
        let now_ms = self.clock.now_ms();
        let candidates = projector
            .refresh_candidates()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        self.scheduler.retain(&candidates);
        for identity in candidates {
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
        projector: &mut OutputPrProjector<P>,
    ) -> Result<Vec<PrIdentity>, P::Error> {
        let now_ms = self.clock.now_ms();
        let candidates = projector
            .refresh_candidates()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        self.scheduler.retain(&candidates);
        for identity in candidates {
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

    /// Executes one claimed tick concurrently. The scheduler cap remains the
    /// single bound, so a slow provider costs one timeout window rather than one
    /// timeout per identity.
    pub fn fetch_many(&self, identities: Vec<PrIdentity>) -> Vec<(PrIdentity, RefreshResult)>
    where
        R: Clone + Send,
    {
        std::thread::scope(|scope| {
            identities
                .into_iter()
                .map(|identity| {
                    let mut runner = self.runner.clone();
                    let worker_identity = identity.clone();
                    let handle = scope.spawn(move || {
                        let result = runner
                            .run("gh", &gh_pr_view_argv(&worker_identity), 5_000)
                            .ok()
                            .and_then(|output| parse_gh_pr_view(&output))
                            .map_or(RefreshResult::Failed, RefreshResult::Success);
                        (worker_identity, result)
                    });
                    (identity, handle)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|(identity, handle)| {
                    handle.join().unwrap_or((identity, RefreshResult::Failed))
                })
                .collect()
        })
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
                    if view.state == PrState::Merged {
                        self.scheduler.retire(identity);
                    } else {
                        let freshness = if view.state == PrState::Closed {
                            self.freshness_ms.saturating_mul(15)
                        } else {
                            self.freshness_ms
                        };
                        self.scheduler.succeeded(identity, now_ms, freshness);
                    }
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
            hydrated: false,
            sessions: BTreeMap::new(),
            carries: BTreeMap::new(),
        }
    }

    /// Reads the durable snapshot once per process. Every later access is served
    /// from memory.
    fn hydrate(&mut self) -> Result<(), P::Error> {
        if !self.hydrated {
            self.sessions = self.store.load()?;
            self.hydrated = true;
        }
        Ok(())
    }

    /// Writes the cache through to the durable snapshot.
    ///
    /// A failed write drops the cache: memory must never stay ahead of what
    /// actually persisted, or a restart would silently lose discoveries this
    /// process still believes are durable.
    fn save(&mut self) -> Result<(), P::Error> {
        if let Err(error) = self.store.save(&self.sessions) {
            self.hydrated = false;
            self.sessions = BTreeMap::new();
            return Err(error);
        }
        Ok(())
    }

    /// Records `identities` against `session`, persisting only a real change.
    fn discover(
        &mut self,
        session: SessionId,
        identities: Vec<(PrIdentity, bool)>,
    ) -> Result<bool, P::Error> {
        if identities.is_empty() {
            return Ok(false);
        }
        self.hydrate()?;
        let changed = self
            .sessions
            .entry(session)
            .or_default()
            .discover_with_auto_open(identities);
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    /// Extracts from the terminated region of `carry + bytes` and carries the
    /// unterminated remainder into the next chunk.
    fn scan(&mut self, terminal: TerminalId, bytes: &[u8]) -> Vec<(PrIdentity, bool)> {
        let carry = self.carries.remove(&terminal).unwrap_or_default();
        let scanned: Cow<'_, [u8]> = if carry.is_empty() {
            Cow::Borrowed(bytes)
        } else {
            let mut combined = carry;
            combined.extend_from_slice(bytes);
            Cow::Owned(combined)
        };
        let boundary = scanned
            .iter()
            .rposition(|byte| is_candidate_terminator(*byte))
            .map_or(0, |index| index + 1);
        let complete = &scanned[..boundary];
        let identities = extract(complete)
            .into_iter()
            .map(|identity| {
                let standalone = complete
                    .split(|byte| matches!(byte, b'\n' | b'\r'))
                    .any(|line| {
                        std::str::from_utf8(line)
                            .ok()
                            .map(str::trim)
                            .and_then(canonicalize)
                            .is_some_and(|candidate| candidate == identity)
                    });
                (identity, standalone)
            })
            .collect();
        self.remember_carry(terminal, &scanned[boundary..]);
        identities
    }

    /// Retains an unterminated trailing token so the next chunk can complete it.
    fn remember_carry(&mut self, terminal: TerminalId, carry: &[u8]) {
        // A run longer than the longest prefix a detection can need cannot become
        // a candidate this carry would rescue, so it is dropped rather than grown.
        if carry.is_empty() || carry.len() > CANDIDATE_PREFIX_MAX {
            return;
        }
        if self.carries.len() >= CARRY_TERMINALS_MAX && !self.carries.contains_key(&terminal) {
            return;
        }
        self.carries.insert(terminal, carry.to_vec());
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
        let identities = self.scan(terminal, bytes);
        self.discover(session, identities)
    }

    /// Forgets the carry for `terminal` because bytes between the carry and the
    /// next chunk were dropped.
    ///
    /// Joining across a gap could synthesize a PR URL that never appeared in the
    /// output, so a gap discards the carry instead of completing it.
    pub fn mark_gap(&mut self, terminal: TerminalId) {
        self.carries.remove(&terminal);
    }

    /// Reclaims the carry for an exited terminal, crediting a candidate that the
    /// output never terminated.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn release_terminal(
        &mut self,
        terminal: TerminalId,
        session: Option<SessionId>,
    ) -> Result<bool, P::Error> {
        let carry = self.carries.remove(&terminal).unwrap_or_default();
        let Some(session) = session else {
            return Ok(false);
        };
        self.discover(
            session,
            extract(&carry)
                .into_iter()
                // The scan boundary may already have discarded prose preceding
                // this unterminated suffix, so it cannot prove standalone intent.
                .map(|identity| (identity, false))
                .collect(),
        )
    }

    /// How many terminals currently hold a carry buffer. Tests assert the bound.
    #[must_use]
    pub fn carried_terminals(&self) -> usize {
        self.carries.len()
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
    pub fn refresh_candidates(&mut self) -> Result<Vec<PrIdentity>, P::Error> {
        self.hydrate()?;
        Ok(self
            .sessions
            .values()
            .flat_map(|inventory| inventory.entries.values())
            .filter(|entry| {
                !entry.pinned
                    && entry.state != PrState::Dismissed
                    && (entry.state != PrState::Merged || entry.head_oid.is_none())
            })
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
        self.hydrate()?;
        let mut changed = false;
        for inventory in self.sessions.values_mut() {
            changed = inventory.apply_refresh(
                identity,
                view.title.clone(),
                view.state,
                PrRefreshMetadata {
                    head_oid: Some(view.head_oid.clone()),
                    draft: view.draft,
                    checks: view.checks,
                    review: view.review,
                },
            ) || changed;
        }
        if changed {
            self.save()?;
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
        self.hydrate()?;
        let mut changed = false;
        for inventory in self.sessions.values_mut() {
            changed = inventory.mark_refresh_backoff(identity) || changed;
        }
        if changed {
            self.save()?;
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
    pub fn snapshot(
        &mut self,
        session: SessionId,
    ) -> Result<usagi_core::usecase::client::PrSnapshot, P::Error> {
        self.hydrate()?;
        let inventory = self.sessions.get(&session).cloned().unwrap_or_default();
        Ok((session, inventory).into())
    }

    /// Reads several session snapshots after one hydrate operation.
    ///
    /// # Errors
    /// Returns the durable inventory port's read error.
    pub fn snapshots(
        &mut self,
        sessions: &[SessionId],
    ) -> Result<Vec<usagi_core::usecase::client::PrSnapshot>, P::Error> {
        self.hydrate()?;
        Ok(sessions
            .iter()
            .map(|session| {
                (
                    *session,
                    self.sessions.get(session).cloned().unwrap_or_default(),
                )
                    .into()
            })
            .collect())
    }

    /// Hides one exact canonical identity for a session and pins the tombstone.
    ///
    /// # Errors
    /// Returns the durable inventory port's read or write error.
    pub fn dismiss(&mut self, session: SessionId, url: &str) -> Result<bool, P::Error> {
        let Some(identity) = canonicalize(url) else {
            return Ok(false);
        };
        self.hydrate()?;
        let changed = self
            .sessions
            .get_mut(&session)
            .is_some_and(|inventory| inventory.set_user_state(&identity, PrState::Dismissed, true));
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    /// Prunes inventories whose stable session identity no longer exists.
    ///
    /// # Errors
    /// Returns the durable inventory port's read or write error.
    pub fn retain_sessions(&mut self, retained: &BTreeSet<SessionId>) -> Result<bool, P::Error> {
        self.hydrate()?;
        let before = self.sessions.len();
        self.sessions
            .retain(|session, _| retained.contains(session));
        let changed = self.sessions.len() != before;
        if !changed {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const HEAD_OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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
        fail_load: Cell<bool>,
        loads: Cell<usize>,
        saves: Cell<usize>,
    }
    impl PrInventoryPort for Store {
        type Error = ();
        fn load(
            &self,
        ) -> Result<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>, ()>
        {
            self.loads.set(self.loads.get() + 1);
            if self.fail_load.get() {
                return Err(());
            }
            Ok(self.values.borrow().clone())
        }
        fn save(
            &self,
            value: &BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>,
        ) -> Result<(), ()> {
            self.saves.set(self.saves.get() + 1);
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
        let id = projector.sessions[&a]
            .entries
            .keys()
            .next()
            .unwrap()
            .clone();
        // The cache is the in-process authority, so a user-owned transition is
        // applied through it rather than behind it.
        projector
            .sessions
            .get_mut(&a)
            .unwrap()
            .set_user_state(&id, PrState::Dismissed, true);
        projector.save().unwrap();
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
        // A user-owned tombstone survives re-detection, and it survives it in the
        // durable snapshot too, not only in memory.
        assert_eq!(
            projector.snapshot(a).unwrap().entries[0].state,
            PrState::Dismissed
        );
        let store = projector.into_store();
        assert_eq!(
            store.values.borrow()[&a].entries[&id].state,
            PrState::Dismissed
        );
        assert_eq!(store.values.borrow()[&b].entries.len(), 1);
    }
    #[test]
    fn ignores_root_output_and_bounds_the_carry() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        assert!(
            !projector
                .observe_committed(terminal, None, b"https://github.com/o/r/pull/1\n")
                .unwrap()
        );
        // Root output must not even reserve a carry.
        assert_eq!(projector.carried_terminals(), 0);
        let session = SessionId::new();
        // An unterminated run longer than the longest prefix a detection needs is
        // dropped rather than carried.
        projector
            .observe_committed(
                terminal,
                Some(session),
                &vec![b'x'; CANDIDATE_PREFIX_MAX + 1],
            )
            .unwrap();
        assert_eq!(projector.carried_terminals(), 0);
        // A short unterminated run is carried.
        projector
            .observe_committed(terminal, Some(session), b" short")
            .unwrap();
        assert_eq!(projector.carried_terminals(), 1);
    }
    #[test]
    fn a_plain_output_chunk_never_touches_the_durable_store() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        let session = SessionId::new();
        for _ in 0..64 {
            assert!(
                !projector
                    .observe_committed(terminal, Some(session), b"compiling usagi-core ... ok\n")
                    .unwrap()
            );
        }
        let store = projector.into_store();
        assert_eq!(store.loads.get(), 0, "a chunk with no PR must not read");
        assert_eq!(store.saves.get(), 0);
    }
    #[test]
    fn the_durable_snapshot_is_read_once_however_many_chunks_arrive() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        let session = SessionId::new();
        for number in 1..=32 {
            projector
                .observe_committed(
                    terminal,
                    Some(session),
                    format!("https://github.com/o/r/pull/{number}\n").as_bytes(),
                )
                .unwrap();
        }
        // Re-detecting an existing identity must not write again either.
        for number in 1..=32 {
            assert!(
                !projector
                    .observe_committed(
                        terminal,
                        Some(session),
                        format!("https://github.com/o/r/pull/{number}\n").as_bytes(),
                    )
                    .unwrap()
            );
        }
        let store = projector.into_store();
        assert_eq!(store.loads.get(), 1, "hydration happens once per process");
        assert_eq!(store.saves.get(), 32, "one write per real change");
    }
    #[test]
    fn a_truncated_number_is_never_credited_to_the_wrong_pr() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        let session = SessionId::new();
        // The chunk ends mid-number. Crediting `pull/42` here would be a false
        // detection, so nothing is recorded until the token terminates.
        assert!(
            !projector
                .observe_committed(
                    terminal,
                    Some(session),
                    b"see https://github.com/o/r/pull/42"
                )
                .unwrap()
        );
        assert!(
            projector
                .observe_committed(terminal, Some(session), b"3\n")
                .unwrap()
        );
        let store = projector.into_store();
        let urls: Vec<String> = store.values.borrow()[&session]
            .entries
            .keys()
            .map(|identity| identity.as_url().to_owned())
            .collect();
        assert_eq!(urls, ["https://github.com/o/r/pull/423"]);
    }
    #[test]
    fn a_gap_discards_the_carry_instead_of_joining_across_dropped_bytes() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        let session = SessionId::new();
        projector
            .observe_committed(terminal, Some(session), b"https://github.com/o/r/pu")
            .unwrap();
        assert_eq!(projector.carried_terminals(), 1);
        projector.mark_gap(terminal);
        assert_eq!(projector.carried_terminals(), 0);
        // Joining across the gap would synthesize a PR that never appeared.
        assert!(
            !projector
                .observe_committed(terminal, Some(session), b"ll/42\n")
                .unwrap()
        );
        assert!(projector.into_store().values.borrow().is_empty());
    }
    #[test]
    fn exit_reclaims_the_carry_and_credits_an_unterminated_candidate() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        let session = SessionId::new();
        // The very last line of a run may never be followed by a terminator.
        assert!(
            !projector
                .observe_committed(
                    terminal,
                    Some(session),
                    b"opened https://github.com/o/r/pull/7"
                )
                .unwrap()
        );
        assert!(projector.release_terminal(terminal, Some(session)).unwrap());
        assert_eq!(projector.carried_terminals(), 0);
        // A root terminal has no inventory to flush into, and a second release is
        // a no-op rather than a duplicate.
        assert!(!projector.release_terminal(terminal, None).unwrap());
        assert!(!projector.release_terminal(terminal, Some(session)).unwrap());
        let store = projector.into_store();
        assert_eq!(store.values.borrow()[&session].entries.len(), 1);
        assert!(
            !store.values.borrow()[&session]
                .entries
                .values()
                .next()
                .unwrap()
                .auto_open
        );
    }
    #[test]
    fn the_carry_table_is_bounded_by_terminal_count() {
        let mut projector = OutputPrProjector::new(Store::default());
        let session = SessionId::new();
        for _ in 0..CARRY_TERMINALS_MAX + 8 {
            projector
                .observe_committed(TerminalId::new(), Some(session), b"unterminated")
                .unwrap();
        }
        assert_eq!(projector.carried_terminals(), CARRY_TERMINALS_MAX);
    }
    #[test]
    fn a_failed_read_is_retried_and_a_failed_write_forgets_the_cache() {
        let mut projector = OutputPrProjector::new(Store::default());
        projector.store.fail_load.set(true);
        assert!(
            projector
                .observe_committed(
                    TerminalId::new(),
                    Some(SessionId::new()),
                    b"https://github.com/o/r/pull/1\n",
                )
                .is_err()
        );
        projector.store.fail_load.set(false);
        projector.store.fail_save.set(true);
        let session = SessionId::new();
        assert!(
            projector
                .observe_committed(
                    TerminalId::new(),
                    Some(session),
                    b"https://github.com/o/r/pull/2\n",
                )
                .is_err()
        );
        // The write failed, so memory must not keep claiming the entry is durable.
        projector.store.fail_save.set(false);
        assert!(
            projector
                .observe_committed(
                    TerminalId::new(),
                    Some(session),
                    b"https://github.com/o/r/pull/2\n",
                )
                .unwrap()
        );
        let store = projector.into_store();
        assert_eq!(store.values.borrow()[&session].entries.len(), 1);
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

    #[derive(Clone, Copy)]
    struct PanickingRunner;

    impl GhProcessPort for PanickingRunner {
        type Error = ();

        fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, ()> {
            panic!("injected PR provider panic");
        }
    }

    #[derive(Clone, Copy)]
    struct SuccessfulRunner;

    impl GhProcessPort for SuccessfulRunner {
        type Error = ();

        fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, ()> {
            Ok(format!(
                r#"{{"title":"ready","state":"OPEN","headRefOid":"{HEAD_OID}"}}"#
            ))
        }
    }

    #[test]
    fn concurrent_refresh_contains_a_panicking_provider_worker() {
        let id = canonicalize("https://github.com/o/r/pull/17").unwrap();
        let worker = RefreshWorker::new(PanickingRunner, FakeClock::default(), 1, 10);
        let results = worker.fetch_many(vec![id.clone()]);
        assert_eq!(results, vec![(id, RefreshResult::Failed)]);

        let id = canonicalize("https://github.com/o/r/pull/18").unwrap();
        let worker = RefreshWorker::new(SuccessfulRunner, FakeClock::default(), 1, 10);
        let results = worker.fetch_many(vec![id.clone()]);
        assert!(matches!(
            results.as_slice(),
            [(actual, RefreshResult::Success(GhPrView { title: Some(title), .. }))]
                if actual == &id && title == "ready"
        ));
    }

    /// Feeds one URL as committed output. The newline terminates the candidate,
    /// which is what makes it eligible for detection in this chunk rather than
    /// being carried into the next one.
    fn discover(projector: &mut OutputPrProjector<Store>, session: SessionId, url: &str) {
        projector
            .observe_committed(
                TerminalId::new(),
                Some(session),
                format!("{url}\n").as_bytes(),
            )
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
        runner.results.borrow_mut().push_back(Ok(format!(
            r#"{{"title":"Done","state":"MERGED","headRefOid":"{HEAD_OID}"}}"#
        )));
        let calls = Rc::clone(&runner.calls);
        let mut worker = RefreshWorker::new(runner, FakeClock::default(), 2, 60_000);
        worker.rebuild(&mut projector).unwrap();
        let due = worker.claim_due(&mut projector).unwrap();
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
                    "title,state,headRefOid,isDraft,reviewDecision,statusCheckRollup"
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
    fn a_legacy_merged_entry_is_refreshed_once_to_backfill_its_head_oid() {
        let session = SessionId::new();
        let id = canonicalize("https://github.com/o/r/pull/19").unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, session, id.as_url());
        assert!(
            projector
                .sessions
                .get_mut(&session)
                .unwrap()
                .set_user_state(&id, PrState::Merged, false)
        );
        assert_eq!(projector.refresh_candidates().unwrap(), vec![id.clone()]);

        assert!(
            projector
                .publish_success(
                    &id,
                    &GhPrView {
                        title: Some("merged".into()),
                        state: PrState::Merged,
                        head_oid: HEAD_OID.into(),
                        draft: false,
                        checks: None,
                        review: None,
                    },
                )
                .unwrap()
        );
        assert!(projector.refresh_candidates().unwrap().is_empty());
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
            parse_gh_pr_view(&format!(
                r#"{{"title":"","state":"OPEN","headRefOid":"{HEAD_OID}"}}"#
            )),
            Some(GhPrView {
                title: None,
                state: PrState::Open,
                head_oid: HEAD_OID.into(),
                draft: false,
                checks: None,
                review: None,
            })
        );
        assert_eq!(
            parse_gh_pr_view(&format!(
                r#"{{"title":"x","state":"CLOSED","headRefOid":"{HEAD_OID}"}}"#
            )),
            Some(GhPrView {
                title: Some("x".into()),
                state: PrState::Closed,
                head_oid: HEAD_OID.into(),
                draft: false,
                checks: None,
                review: None,
            })
        );
        assert_eq!(
            parse_gh_pr_view(
                &format!(r#"{{"title":"x","state":"OPEN","headRefOid":"{HEAD_OID}","reviewDecision":"CHANGES_REQUESTED","statusCheckRollup":[]}}"#),
            )
            .unwrap()
            .review,
            Some(PrReviewDecision::ChangesRequested)
        );
        let required = parse_gh_pr_view(
            &format!(r#"{{"title":"x","state":"OPEN","headRefOid":"{HEAD_OID}","reviewDecision":"REVIEW_REQUIRED","statusCheckRollup":[{{"state":"EXPECTED"}}]}}"#),
        )
        .unwrap();
        assert_eq!(required.review, Some(PrReviewDecision::ReviewRequired));
        assert_eq!(required.checks, Some(PrChecksState::Pending));
        assert_eq!(
            parse_gh_pr_view(
                &format!(r#"{{"title":"x","state":"OPEN","headRefOid":"{HEAD_OID}","statusCheckRollup":[{{"conclusion":"FAILURE"}}]}}"#),
            )
            .unwrap()
            .checks,
            Some(PrChecksState::Failing)
        );
        for invalid in [
            "not json",
            "{}",
            "{\"title\":1,\"state\":\"OPEN\"}",
            "{\"title\":\"x\"}",
            "{\"title\":\"x\",\"state\":1}",
            "{\"title\":\"x\",\"state\":\"DRAFT\"}",
            r#"{"title":"x","state":"OPEN","reviewDecision":"UNKNOWN"}"#,
            r#"{"title":"x","state":"OPEN","statusCheckRollup":{}}"#,
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
            Ok(format!(
                r#"{{"title":"fresh","state":"OPEN","headRefOid":"{HEAD_OID}"}}"#
            )),
        ]);
        let clock = FakeClock::default();
        let mut worker = RefreshWorker::new(runner, clock.clone(), 1, 10_000);
        worker.rebuild(&mut projector).unwrap();
        let due = worker.claim_due(&mut projector).unwrap();
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
        assert!(worker.claim_due(&mut projector).unwrap().is_empty());
        clock.set(2_000);
        let due = worker.claim_due(&mut projector).unwrap();
        let result = worker.fetch(&due[0]);
        assert!(worker.complete(&mut projector, &id, result).unwrap());
        assert!(
            !projector
                .publish_success(
                    &id,
                    &GhPrView {
                        title: Some("fresh".into()),
                        state: PrState::Open,
                        head_oid: HEAD_OID.into(),
                        draft: false,
                        checks: None,
                        review: None,
                    },
                )
                .unwrap()
        );
        assert!(worker.claim_due(&mut projector).unwrap().is_empty());
        clock.set(12_000);
        assert_eq!(worker.claim_due(&mut projector).unwrap(), vec![id]);
    }

    #[test]
    fn closed_pull_requests_use_the_extended_refresh_interval() {
        let session = SessionId::new();
        let id = canonicalize("https://github.com/o/r/pull/15").unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, session, id.as_url());
        let clock = FakeClock::default();
        let mut worker = RefreshWorker::new(FakeRunner::default(), clock.clone(), 1, 10_000);
        worker.rebuild(&mut projector).unwrap();
        assert_eq!(worker.claim_due(&mut projector).unwrap(), vec![id.clone()]);
        assert!(
            worker
                .complete(
                    &mut projector,
                    &id,
                    RefreshResult::Success(GhPrView {
                        title: Some("closed".into()),
                        state: PrState::Closed,
                        head_oid: HEAD_OID.into(),
                        draft: false,
                        checks: None,
                        review: None,
                    }),
                )
                .unwrap()
        );
        clock.set(149_999);
        assert!(worker.claim_due(&mut projector).unwrap().is_empty());
        clock.set(150_000);
        assert_eq!(worker.claim_due(&mut projector).unwrap(), vec![id]);
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
        first.rebuild(&mut projector).unwrap();
        let selected = first.claim_due(&mut projector).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].as_url(), "https://github.com/o/r/pull/1");
        assert_eq!(selected[1].as_url(), "https://github.com/o/r/pull/2");
        assert!(first.claim_due(&mut projector).unwrap().len() <= 1);

        let mut restarted = RefreshWorker::new(FakeRunner::default(), clock, 2, 60_000);
        restarted.rebuild(&mut projector).unwrap();
        assert_eq!(restarted.claim_due(&mut projector).unwrap(), selected);
    }

    #[test]
    fn publish_errors_release_claims_into_backoff_and_keep_the_durable_snapshot() {
        let session = SessionId::new();
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/6")
            .unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, session, id.as_url());
        let runner = FakeRunner::default();
        runner.results.borrow_mut().push_back(Ok(format!(
            r#"{{"title":"remote","state":"OPEN","headRefOid":"{HEAD_OID}"}}"#
        )));
        let clock = FakeClock::default();
        let mut worker = RefreshWorker::new(runner, clock.clone(), 1, 10_000);
        worker.rebuild(&mut projector).unwrap();
        let due = worker.claim_due(&mut projector).unwrap();
        let result = worker.fetch(&due[0]);
        projector.store.fail_save.set(true);
        assert!(worker.complete(&mut projector, &id, result).is_err());
        clock.set(1_999);
        assert!(worker.claim_due(&mut projector).unwrap().is_empty());
        clock.set(2_000);
        assert_eq!(worker.claim_due(&mut projector).unwrap(), vec![id.clone()]);

        let mut failure_projector = OutputPrProjector::new(Store::default());
        discover(&mut failure_projector, session, id.as_url());
        failure_projector.store.fail_save.set(true);
        assert!(failure_projector.publish_failure(&id).is_err());
    }

    #[test]
    fn every_inventory_read_and_new_mutation_propagates_storage_errors() {
        let session = SessionId::new();
        let id = canonicalize("https://github.com/o/r/pull/16").unwrap();
        let failing_projector = || {
            let store = Store::default();
            store.fail_load.set(true);
            OutputPrProjector::new(store)
        };

        let mut projector = failing_projector();
        let mut worker = RefreshWorker::new(FakeRunner::default(), FakeClock::default(), 1, 10);
        assert!(worker.rebuild(&mut projector).is_err());
        let mut projector = failing_projector();
        assert!(worker.claim_due(&mut projector).is_err());
        let mut projector = failing_projector();
        assert!(projector.refresh_candidates().is_err());
        let mut projector = failing_projector();
        assert!(
            projector
                .publish_success(
                    &id,
                    &GhPrView {
                        title: None,
                        state: PrState::Open,
                        head_oid: HEAD_OID.into(),
                        draft: false,
                        checks: None,
                        review: None,
                    },
                )
                .is_err()
        );
        let mut projector = failing_projector();
        assert!(projector.publish_failure(&id).is_err());
        let mut projector = failing_projector();
        assert!(projector.snapshot(session).is_err());
        let mut projector = failing_projector();
        assert!(projector.snapshots(&[session]).is_err());
        let mut projector = failing_projector();
        assert!(projector.dismiss(session, id.as_url()).is_err());
        let mut projector = failing_projector();
        assert!(projector.retain_sessions(&BTreeSet::new()).is_err());

        let mut dismiss = OutputPrProjector::new(Store::default());
        discover(&mut dismiss, session, id.as_url());
        dismiss.store.fail_save.set(true);
        assert!(dismiss.dismiss(session, id.as_url()).is_err());

        let mut retain = OutputPrProjector::new(Store::default());
        discover(&mut retain, session, id.as_url());
        retain.store.fail_save.set(true);
        assert!(retain.retain_sessions(&BTreeSet::new()).is_err());
    }

    #[test]
    fn references_do_not_auto_open_but_standalone_urls_do_and_user_can_dismiss() {
        let session = SessionId::new();
        let terminal = TerminalId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        projector
            .observe_committed(
                terminal,
                Some(session),
                b"review https://github.com/o/r/pull/7 please\nhttps://github.com/o/r/pull/8\n",
            )
            .unwrap();
        let snapshot = projector.snapshot(session).unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        assert!(!snapshot.entries[0].auto_open);
        assert!(snapshot.entries[1].auto_open);
        let revision = snapshot.revision;
        assert!(
            projector
                .dismiss(session, "https://github.com/o/r/pull/8")
                .unwrap()
        );
        let dismissed = projector.snapshot(session).unwrap();
        assert!(dismissed.revision > revision);
        assert_eq!(dismissed.entries[1].state, PrState::Dismissed);
    }

    #[test]
    fn batch_snapshot_and_session_reconciliation_share_one_cached_document() {
        let kept = SessionId::new();
        let removed = SessionId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        discover(&mut projector, kept, "https://github.com/o/r/pull/1");
        discover(&mut projector, removed, "https://github.com/o/r/pull/2");
        assert!(!projector.dismiss(kept, "not-a-pr").unwrap());
        assert!(
            !projector
                .dismiss(SessionId::new(), "https://github.com/o/r/pull/9")
                .unwrap()
        );
        assert_eq!(projector.snapshots(&[kept, removed]).unwrap().len(), 2);
        assert!(projector.retain_sessions(&BTreeSet::from([kept])).unwrap());
        assert!(!projector.retain_sessions(&BTreeSet::from([kept])).unwrap());
        assert_eq!(projector.snapshot(removed).unwrap().entries, Vec::new());
        assert_eq!(projector.snapshots(&[kept]).unwrap()[0].entries.len(), 1);
    }

    #[test]
    fn parser_projects_checks_review_and_scheduler_drops_ineligible_work() {
        let view = parse_gh_pr_view(
            &format!(r#"{{"title":"ready","state":"OPEN","headRefOid":"{HEAD_OID}","isDraft":true,"reviewDecision":"APPROVED","statusCheckRollup":[{{"conclusion":"SUCCESS"}}]}}"#),
        )
        .unwrap();
        assert!(view.draft);
        assert_eq!(view.checks, Some(PrChecksState::Passing));
        assert_eq!(view.review, Some(PrReviewDecision::Approved));

        let keep = canonicalize("https://github.com/o/r/pull/1").unwrap();
        let drop = canonicalize("https://github.com/o/r/pull/2").unwrap();
        let mut scheduler = RefreshScheduler::new(2);
        scheduler.schedule(keep.clone(), 0, 0);
        scheduler.schedule(drop, 0, 0);
        scheduler.retain(&BTreeSet::from([keep.clone()]));
        assert_eq!(scheduler.claim_due(0), vec![keep]);
    }
}
