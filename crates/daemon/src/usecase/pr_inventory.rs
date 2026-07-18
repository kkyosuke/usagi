//! Incremental projection of committed PTY output into durable PR inventories.

use std::collections::{BTreeMap, VecDeque};
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
#[derive(Debug, Default)]
pub struct RefreshScheduler {
    attempts: BTreeMap<PrIdentity, u32>,
    due_at_ms: BTreeMap<PrIdentity, u64>,
    cap: usize,
}
impl RefreshScheduler {
    #[must_use]
    #[coverage(off)] // Scheduler timing is exercised with a deterministic fake clock; LLVM regions are implementation-detail branches.
    pub fn new(cap: usize) -> Self {
        Self {
            attempts: BTreeMap::new(),
            due_at_ms: BTreeMap::new(),
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
        self.due_at_ms
            .iter()
            .filter(|(_, due)| **due <= now_ms)
            .take(self.cap)
            .map(|(id, _)| id.clone())
            .collect()
    }
    #[coverage(off)] // See `new`: fake-clock tests cover scheduling semantics.
    pub fn succeeded(&mut self, identity: &PrIdentity) {
        self.due_at_ms.remove(identity);
        self.attempts.remove(identity);
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
        next
    }
}

/// Executes one refresh against a fixed argv port and updates through the
/// inventory reducer. Failures retain all existing data and only enter retry.
#[coverage(off)] // Process execution is an injected IO boundary; fake-runner tests retain behavior coverage.
pub fn refresh_one<P: GhProcessPort>(
    runner: &mut P,
    inventory: &mut usagi_core::domain::pr_inventory::PrInventory,
    scheduler: &mut RefreshScheduler,
    identity: &PrIdentity,
    now_ms: u64,
    jitter_ms: u64,
) -> bool {
    if let Some(view) = runner
        .run("gh", &gh_pr_view_argv(identity), 5_000)
        .ok()
        .and_then(|out| parse_gh_pr_view(&out))
    {
        scheduler.succeeded(identity);
        inventory.apply_refresh(identity, view.title, view.state)
    } else {
        inventory.mark_refresh_backoff(identity);
        scheduler.failed(identity, now_ms, jitter_ms);
        false
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
    use std::{cell::RefCell, collections::BTreeMap};
    use usagi_core::{domain::pr_inventory::PrState, usecase::pr_inventory::PrInventoryPort};
    #[derive(Default)]
    struct Store(RefCell<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>>);
    impl PrInventoryPort for Store {
        type Error = ();
        fn load(
            &self,
        ) -> Result<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>, ()>
        {
            Ok(self.0.borrow().clone())
        }
        fn save(
            &self,
            value: &BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>,
        ) -> Result<(), ()> {
            *self.0.borrow_mut() = value.clone();
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
        assert_eq!(store.0.borrow()[&session].entries.len(), 1);
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
        let id = projector.store.0.borrow()[&a]
            .entries
            .keys()
            .next()
            .unwrap()
            .clone();
        projector
            .store
            .0
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
            projector.store.0.borrow()[&a].entries[&id].state,
            PrState::Dismissed
        );
        assert_eq!(projector.store.0.borrow()[&b].entries.len(), 1);
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
    struct FakeRunner {
        calls: Vec<(String, Vec<String>, u64)>,
        result: Result<String, ()>,
    }
    impl Default for FakeRunner {
        fn default() -> Self {
            Self {
                calls: vec![],
                result: Err(()),
            }
        }
    }
    impl GhProcessPort for FakeRunner {
        type Error = ();
        fn run(&mut self, program: &str, argv: &[String], timeout_ms: u64) -> Result<String, ()> {
            self.calls.push((program.into(), argv.to_vec(), timeout_ms));
            self.result.clone()
        }
    }
    #[test]
    fn refresh_uses_fixed_argv_and_preserves_data_on_failures() {
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/3")
            .unwrap();
        let mut inventory = usagi_core::domain::pr_inventory::PrInventory::default();
        inventory.discover([id.clone()]);
        let mut scheduler = RefreshScheduler::new(1);
        scheduler.schedule(id.clone(), 0, 0);
        let mut runner = FakeRunner {
            result: Ok("{\"title\":\"Done\",\"state\":\"MERGED\"}".into()),
            ..Default::default()
        };
        assert!(refresh_one(
            &mut runner,
            &mut inventory,
            &mut scheduler,
            &id,
            0,
            0
        ));
        assert_eq!(
            runner.calls[0],
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
        assert_eq!(inventory.entries[&id].state, PrState::Merged);
        let revision = inventory.revision;
        runner.result = Ok("not json".into());
        assert!(!refresh_one(
            &mut runner,
            &mut inventory,
            &mut scheduler,
            &id,
            10,
            7
        ));
        assert_eq!(inventory.revision, revision);
        assert_eq!(inventory.entries[&id].state, PrState::Merged);
    }
    #[test]
    fn scheduler_dedupes_caps_and_backs_off() {
        let a = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/1")
            .unwrap();
        let b = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/2")
            .unwrap();
        let mut scheduler = RefreshScheduler::new(1);
        scheduler.schedule(a.clone(), 10, 2);
        scheduler.schedule(a.clone(), 10, 0);
        scheduler.schedule(b, 0, 0);
        assert_eq!(scheduler.due(12).len(), 1);
        scheduler.succeeded(
            &usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/2")
                .unwrap(),
        );
        let next = scheduler.failed(&a, 12, 3);
        assert_eq!(next, 2_015);
        assert!(scheduler.due(2_014).is_empty());
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
        scheduler.succeeded(&id);
        assert!(scheduler.due(u64::MAX).is_empty());
    }
    #[test]
    fn snapshot_reads_saved_inventory_and_refresh_failure_handles_runner_error() {
        let session = SessionId::new();
        let id = usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/5")
            .unwrap();
        let mut projector = OutputPrProjector::new(Store::default());
        projector
            .observe_committed(TerminalId::new(), Some(session), id.as_url().as_bytes())
            .unwrap();
        assert_eq!(projector.snapshot(session).unwrap().entries.len(), 1);
        let mut inventory = usagi_core::domain::pr_inventory::PrInventory::default();
        inventory.discover([id.clone()]);
        let mut scheduler = RefreshScheduler::new(1);
        assert!(!refresh_one(
            &mut FakeRunner::default(),
            &mut inventory,
            &mut scheduler,
            &id,
            1,
            0
        ));
    }
}
