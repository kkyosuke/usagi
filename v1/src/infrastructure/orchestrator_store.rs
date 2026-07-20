//! Workspace-local durable plan, claim, and event storage.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::orchestrator::{Claim, Claims, Event, Plan, Stamped};
use crate::infrastructure::{json_file, store_lock::StoreLock};

const FORMAT: &str = "usagi-orchestrator";
const PLAN_VERSION: u32 = 1;
const CLAIMS_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedEvent {
    pub event: Event,
    pub reason: String,
    pub rejected_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct OrchestratorStore {
    root: PathBuf,
    workspace: String,
}

impl OrchestratorStore {
    pub fn new(workspace: &Path) -> Self {
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        Self {
            root: workspace.join(".usagi/orchestrators"),
            workspace: workspace.to_string_lossy().into_owned(),
        }
    }

    pub fn workspace_key(&self) -> &str {
        &self.workspace
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
        let plan = json_file::read(&self.state_path(id))?;
        if let Some(stamped) = &plan {
            validate_envelope(stamped, PLAN_VERSION, "plan")?;
        }
        Ok(plan)
    }

    pub fn plan_ids(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context(format!("failed to read {}", self.root.display())),
        };
        let mut ids = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .and_then(|_| entry.file_name().into_string().ok())
            })
            .filter(|id| self.state_path(id).exists())
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
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
            version: PLAN_VERSION,
            revision: actual.map_or(0, |v| v + 1),
            written_at: now,
            value: plan.clone(),
        };
        json_file::write_atomic(&dir, &self.state_path(&plan.id), &stamped)?;
        Ok(stamped)
    }

    pub fn load_claims(&self) -> Result<Stamped<Claims>> {
        let claims: Option<Stamped<Claims>> = json_file::read(&self.claims_path())?;
        if let Some(stamped) = &claims {
            validate_envelope(stamped, CLAIMS_VERSION, "claims")?;
            if stamped
                .value
                .by_issue
                .values()
                .any(|claim| claim.workspace != self.workspace)
            {
                bail!(
                    "orchestrator claims workspace does not match {}",
                    self.workspace
                );
            }
        }
        Ok(claims.unwrap_or_else(|| Stamped {
            format: FORMAT.into(),
            version: CLAIMS_VERSION,
            revision: 0,
            written_at: DateTime::<Utc>::UNIX_EPOCH,
            value: Claims::default(),
        }))
    }

    /// Atomically claims `(workspace, issue)`. An expired claim still blocks
    /// takeover until the caller proves it freshly re-observed session and PR.
    pub fn claim(
        &self,
        claim: Claim,
        now: DateTime<Utc>,
        absent_observation: Option<&Claim>,
    ) -> Result<ClaimOutcome> {
        if claim.workspace != self.workspace {
            bail!("claim workspace does not match {}", self.workspace);
        }
        let _lock = StoreLock::acquire(&self.root)?;
        let mut claims = self.load_claims()?;
        if let Some(current) = claims.value.by_issue.get(&claim.issue) {
            let same = current.plan == claim.plan
                && current.owner == claim.owner
                && current.generation == claim.generation;
            let expired = current.lease.expires_at <= now;
            let observed_absent = absent_observation.is_some_and(|seen| seen == current);
            if !same && (!expired || !observed_absent) {
                return Ok(ClaimOutcome::Busy(current.clone()));
            }
        }
        claims.value.by_issue.insert(claim.issue, claim);
        claims.revision += 1;
        claims.written_at = now;
        json_file::write_atomic(&self.root, &self.claims_path(), &claims)?;
        Ok(ClaimOutcome::Acquired)
    }

    pub fn release_claim(
        &self,
        issue: u64,
        plan: &str,
        owner: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.root)?;
        let mut claims = self.load_claims()?;
        let owned = claims
            .value
            .by_issue
            .get(&issue)
            .is_some_and(|c| c.plan == plan && c.owner == owner && c.generation == generation);
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

    /// Move a stale/unknown event into an append-only rejection ledger. The
    /// ledger write precedes pending-file removal under the plan lock, so a
    /// crash may leave a harmless duplicate but can never silently lose it.
    pub fn reject_event(
        &self,
        plan: &str,
        event_id: &str,
        reason: &str,
        rejected_at: DateTime<Utc>,
    ) -> Result<bool> {
        let plan_dir = self.plan_dir(plan);
        let _lock = StoreLock::acquire(&plan_dir)?;
        let pending = plan_dir.join("events").join(format!("{event_id}.json"));
        let Some(event): Option<Event> = json_file::read(&pending)? else {
            return Ok(false);
        };
        let dir = plan_dir.join("rejected-events");
        let path = dir.join(format!("{event_id}.json"));
        if !path.exists() {
            json_file::write_atomic(
                &dir,
                &path,
                &RejectedEvent {
                    event,
                    reason: reason.to_string(),
                    rejected_at,
                },
            )?;
        }
        match fs::remove_file(&pending) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context(format!("failed to reject {}", pending.display())),
        }
    }

    pub fn load_rejected_events(&self, plan: &str) -> Result<Vec<RejectedEvent>> {
        let dir = self.plan_dir(plan).join("rejected-events");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context(format!("failed to read {}", dir.display())),
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                json_file::read(&path)?
                    .context(format!("rejected event disappeared: {}", path.display()))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Acquired,
    Busy(Claim),
}

fn validate_envelope<T>(stamped: &Stamped<T>, version: u32, kind: &str) -> Result<()> {
    if stamped.format != FORMAT || stamped.version != version {
        bail!(
            "unsupported orchestrator {kind} envelope: format {:?}, version {}",
            stamped.format,
            stamped.version
        );
    }
    Ok(())
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
            workspace: String::new(),
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
                    let mut claim = claim(owner);
                    claim.workspace = store.workspace_key().into();
                    store.claim(claim, now(), None).unwrap()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| matches!(won, ClaimOutcome::Acquired))
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn expired_claim_needs_fresh_absence_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let mut first = claim("a");
        first.workspace = store.workspace_key().into();
        store.claim(first.clone(), now(), None).unwrap();
        let later = now() + Duration::minutes(6);
        let mut second = claim("b");
        second.workspace = store.workspace_key().into();
        assert!(matches!(
            store.claim(second.clone(), later, None).unwrap(),
            ClaimOutcome::Busy(_)
        ));
        assert_eq!(
            store.claim(second, later, Some(&first)).unwrap(),
            ClaimOutcome::Acquired
        );
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
            credential: Some("credential".into()),
            kind,
            terminal_revision: 0,
            observed_at: now(),
        };
        assert!(store.append_event(&event).unwrap());
        assert!(!store.append_event(&event).unwrap());
        assert_eq!(store.load_events("p").unwrap(), vec![event]);
    }

    #[test]
    fn plan_ids_lists_saved_plans_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        assert!(store.plan_ids().unwrap().is_empty());
        for id in ["b", "a"] {
            store
                .save_plan(
                    &Plan {
                        id: id.into(),
                        owner: "owner".into(),
                        max_parallel: 1,
                        nodes: BTreeMap::new(),
                    },
                    None,
                    now(),
                )
                .unwrap();
        }
        fs::create_dir_all(tmp.path().join(".usagi/orchestrators/empty")).unwrap();
        assert_eq!(store.plan_ids().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn plan_ids_reports_a_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/orchestrators"), "not a dir").unwrap();

        let error = OrchestratorStore::new(tmp.path()).plan_ids().unwrap_err();

        assert!(error.to_string().contains("failed to read"));
    }

    #[test]
    fn only_owner_can_release_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let mut first = claim("a");
        first.workspace = store.workspace_key().into();
        store.claim(first, now(), None).unwrap();
        assert!(!store.release_claim(183, "b", "b", 1, now()).unwrap());
        assert!(!store.release_claim(183, "a", "a", 2, now()).unwrap());
        assert!(store.release_claim(183, "a", "a", 1, now()).unwrap());
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
    fn claims_fail_closed_on_unknown_envelope_or_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        let claims_path = tmp.path().join(".usagi/orchestrators/claims.json");
        fs::create_dir_all(claims_path.parent().unwrap()).unwrap();
        fs::write(
            &claims_path,
            r#"{"format":"usagi-orchestrator","version":1,"revision":0,"written_at":"2026-01-01T00:00:00Z","value":{"by_issue":{}}}"#,
        )
        .unwrap();
        assert!(store
            .load_claims()
            .unwrap_err()
            .to_string()
            .contains("unsupported orchestrator claims envelope"));

        let stamped = Stamped {
            format: FORMAT.into(),
            version: CLAIMS_VERSION,
            revision: 0,
            written_at: now(),
            value: Claims {
                by_issue: [(183, claim("a"))].into(),
            },
        };
        json_file::write_atomic(claims_path.parent().unwrap(), &claims_path, &stamped).unwrap();
        assert!(store
            .load_claims()
            .unwrap_err()
            .to_string()
            .contains("workspace does not match"));
    }

    #[test]
    fn equal_issue_numbers_in_different_workspaces_have_independent_claims() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for (workspace, owner) in [(first.path(), "a"), (second.path(), "b")] {
            let store = OrchestratorStore::new(workspace);
            let mut candidate = claim(owner);
            candidate.workspace = store.workspace_key().into();
            assert_eq!(
                store.claim(candidate, now(), None).unwrap(),
                ClaimOutcome::Acquired
            );
        }
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
            credential: Some("credential".into()),
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
            credential: Some("credential".into()),
            kind: EventKind::Succeeded,
            terminal_revision: 0,
            observed_at: now(),
        };
        store.append_event(&event).unwrap();
        assert!(store.acknowledge_event("p", &event.id).unwrap());
        assert!(!store.acknowledge_event("p", &event.id).unwrap());
        assert!(store.load_events("p").unwrap().is_empty());
    }

    #[test]
    fn stale_event_moves_to_durable_rejection_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OrchestratorStore::new(tmp.path());
        assert!(!store.reject_event("p", "missing", "stale", now()).unwrap());
        assert!(store.load_rejected_events("p").unwrap().is_empty());
        let event = Event {
            id: "stale".into(),
            plan: "p".into(),
            issue: 1,
            generation: 1,
            credential: Some("old".into()),
            kind: EventKind::Succeeded,
            terminal_revision: 2,
            observed_at: now(),
        };
        store.append_event(&event).unwrap();
        assert!(store
            .reject_event("p", "stale", "stale_generation", now())
            .unwrap());
        assert!(!store
            .reject_event("p", "stale", "stale_generation", now())
            .unwrap());
        assert!(store.load_events("p").unwrap().is_empty());
        assert_eq!(
            store.load_rejected_events("p").unwrap(),
            vec![RejectedEvent {
                event,
                reason: "stale_generation".into(),
                rejected_at: now(),
            }]
        );
    }
}
