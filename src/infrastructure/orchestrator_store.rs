//! Workspace-local durable plan, claim, and event storage.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::domain::orchestrator::{Claim, Claims, Event, Plan, Stamped};
use crate::infrastructure::{json_file, store_lock::StoreLock};

const FORMAT: &str = "usagi-orchestrator";
const VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct OrchestratorStore {
    root: PathBuf,
}

impl OrchestratorStore {
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace.join(".usagi/orchestrators"),
        }
    }

    fn plan_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    fn state_path(&self, id: &str) -> PathBuf {
        self.plan_dir(id).join("state.json")
    }
    fn claims_path(&self) -> PathBuf {
        self.root.join("claims.json")
    }

    pub fn load_plan(&self, id: &str) -> Result<Option<Stamped<Plan>>> {
        json_file::read(&self.state_path(id))
    }

    /// Lock-protected compare-and-swap. `expected_revision=None` creates a plan
    /// only when absent; otherwise the current revision must match exactly.
    pub fn save_plan(
        &self,
        plan: &Plan,
        expected_revision: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<Stamped<Plan>> {
        let dir = self.plan_dir(&plan.id);
        let _lock = StoreLock::acquire(&dir)?;
        let current: Option<Stamped<Plan>> = json_file::read(&self.state_path(&plan.id))?;
        let actual = current.as_ref().map(|v| v.revision);
        if actual != expected_revision {
            bail!(
                "orchestrator plan CAS conflict: expected {expected_revision:?}, found {actual:?}"
            );
        }
        let stamped = Stamped {
            format: FORMAT.into(),
            version: VERSION,
            revision: actual.map_or(0, |v| v + 1),
            written_at: now,
            value: plan.clone(),
        };
        json_file::write_atomic(&dir, &self.state_path(&plan.id), &stamped)?;
        Ok(stamped)
    }

    pub fn load_claims(&self) -> Result<Stamped<Claims>> {
        Ok(
            json_file::read(&self.claims_path())?.unwrap_or_else(|| Stamped {
                format: FORMAT.into(),
                version: VERSION,
                revision: 0,
                written_at: DateTime::<Utc>::UNIX_EPOCH,
                value: Claims::default(),
            }),
        )
    }

    /// Atomically claims `(workspace, issue)`. An expired claim still blocks
    /// takeover until the caller proves it freshly re-observed session and PR.
    pub fn claim(
        &self,
        claim: Claim,
        now: DateTime<Utc>,
        session_and_pr_absent: bool,
    ) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.root)?;
        let mut claims = self.load_claims()?;
        if let Some(current) = claims.value.by_issue.get(&claim.issue) {
            let same = current.plan == claim.plan && current.owner == claim.owner;
            let expired = current.lease.expires_at <= now;
            if !same && (!expired || !session_and_pr_absent) {
                return Ok(false);
            }
        }
        claims.value.by_issue.insert(claim.issue, claim);
        claims.revision += 1;
        claims.written_at = now;
        json_file::write_atomic(&self.root, &self.claims_path(), &claims)?;
        Ok(true)
    }

    pub fn release_claim(
        &self,
        issue: u64,
        plan: &str,
        owner: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.root)?;
        let mut claims = self.load_claims()?;
        let owned = claims
            .value
            .by_issue
            .get(&issue)
            .is_some_and(|c| c.plan == plan && c.owner == owner);
        if !owned {
            return Ok(false);
        }
        claims.value.by_issue.remove(&issue);
        claims.revision += 1;
        claims.written_at = now;
        json_file::write_atomic(&self.root, &self.claims_path(), &claims)?;
        Ok(true)
    }

    /// Append an event by deterministic id. The plan lock plus existence check
    /// is the deduplication point; the shared JSON writer keeps creation atomic.
    pub fn append_event(&self, event: &Event) -> Result<bool> {
        let plan_dir = self.plan_dir(&event.plan);
        let _lock = StoreLock::acquire(&plan_dir)?;
        let dir = plan_dir.join("events");
        fs::create_dir_all(&dir).context(format!("failed to create {}", dir.display()))?;
        let path = dir.join(format!("{}.json", event.id));
        if path.exists() {
            return Ok(false);
        }
        json_file::write_atomic(&dir, &path, event)?;
        Ok(true)
    }

    pub fn load_events(&self, plan: &str) -> Result<Vec<Event>> {
        let dir = self.plan_dir(plan).join("events");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e).context(format!("failed to read {}", dir.display())),
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                json_file::read(&path)?.context(format!("event disappeared: {}", path.display()))
            })
            .collect()
    }

    /// Acknowledge an event after the plan revision containing its effect has
    /// been durably saved. Missing events are already acknowledged.
    pub fn acknowledge_event(&self, plan: &str, event_id: &str) -> Result<bool> {
        let plan_dir = self.plan_dir(plan);
        let _lock = StoreLock::acquire(&plan_dir)?;
        let path = plan_dir.join("events").join(format!("{event_id}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context(format!("failed to acknowledge {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::Duration;

    use super::*;
    use crate::domain::orchestrator::{EventKind, Lease};

    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn claim(owner: &str) -> Claim {
        Claim {
            issue: 183,
            plan: owner.into(),
            owner: owner.into(),
            generation: 1,
            lease: Lease {
                owner: owner.into(),
                expires_at: now() + Duration::minutes(5),
            },
        }
    }

    #[test]
    fn two_owners_cannot_claim_the_same_workspace_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(OrchestratorStore::new(tmp.path()));
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|owner| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.claim(claim(owner), now(), false).unwrap()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn expired_claim_needs_fresh_absence_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        store.claim(claim("a"), now(), false).unwrap();
        let later = now() + Duration::minutes(6);
        assert!(!store.claim(claim("b"), later, false).unwrap());
        assert!(store.claim(claim("b"), later, true).unwrap());
    }

    #[test]
    fn plan_cas_rejects_a_stale_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let plan = Plan {
            id: "p".into(),
            owner: "a".into(),
            max_parallel: 1,
            nodes: BTreeMap::new(),
        };
        let saved = store.save_plan(&plan, None, now()).unwrap();
        assert_eq!(saved.revision, 0);
        assert_eq!(store.load_plan("p").unwrap(), Some(saved));
        assert!(store.save_plan(&plan, None, now()).is_err());
        assert_eq!(store.save_plan(&plan, Some(0), now()).unwrap().revision, 1);
    }

    #[test]
    fn repeated_event_is_stored_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let kind = EventKind::Succeeded;
        let event = Event {
            id: Event::deterministic_id("p", 1, 2, &kind, 0),
            plan: "p".into(),
            issue: 1,
            generation: 2,
            kind,
            terminal_revision: 0,
            observed_at: now(),
        };
        assert!(store.append_event(&event).unwrap());
        assert!(!store.append_event(&event).unwrap());
        assert_eq!(store.load_events("p").unwrap(), vec![event]);
    }

    #[test]
    fn only_owner_can_release_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        store.claim(claim("a"), now(), false).unwrap();
        assert!(!store.release_claim(183, "b", "b", now()).unwrap());
        assert!(store.release_claim(183, "a", "a", now()).unwrap());
    }

    #[test]
    fn missing_event_directory_is_an_empty_queue() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(OrchestratorStore::new(tmp.path())
            .load_events("missing")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn event_io_errors_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let kind = EventKind::Failed;
        let event = Event {
            id: "missing/child".into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            kind,
            terminal_revision: 0,
            observed_at: now(),
        };
        assert!(store.append_event(&event).is_err());

        let events = tmp.path().join(".usagi/orchestrators/q/events");
        fs::create_dir_all(events.parent().unwrap()).unwrap();
        fs::write(&events, "not a directory").unwrap();
        assert!(store.load_events("q").is_err());

        let ack_dir = tmp
            .path()
            .join(".usagi/orchestrators/ack/events/directory.json");
        fs::create_dir_all(&ack_dir).unwrap();
        assert!(store.acknowledge_event("ack", "directory").is_err());
    }

    #[test]
    fn event_is_retained_until_acknowledged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let event = Event {
            id: "p-1-1-succeeded-0".into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            kind: EventKind::Succeeded,
            terminal_revision: 0,
            observed_at: now(),
        };
        store.append_event(&event).unwrap();
        assert!(store.acknowledge_event("p", &event.id).unwrap());
        assert!(!store.acknowledge_event("p", &event.id).unwrap());
        assert!(store.load_events("p").unwrap().is_empty());
    }
}
