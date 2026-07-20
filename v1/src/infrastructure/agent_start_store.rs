//! Durable queued-agent start requests shared by the daemon and TUI fallback.
//!
//! A prompt file is a delivery channel; this store is the launch transaction.
//! Requests retain the agent launch pair chosen when the prompt was published
//! and advance under one store lock, so two consumers cannot both spawn it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::domain::workspace_state::SessionAgent;
use crate::infrastructure::store_lock::StoreLock;
use crate::infrastructure::worktree_keyed_store::{
    dir, file_name, key, read_ours, write_stamped, WorktreeStamped,
};

const SUBDIR: &str = "agent-start-requests";
pub const LEASE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum StartState {
    Queued,
    Claimed { owner: String, lease_until: u64 },
    Spawned { terminal: u64 },
    InputAcknowledged { terminal: u64 },
    Running { terminal: u64 },
    Dead { attempts: u32, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    pub worktree: PathBuf,
    pub id: u64,
    pub generation: u64,
    pub prompt: String,
    #[serde(default)]
    pub reuse_live_agent: bool,
    pub agent: SessionAgent,
    pub attempts: u32,
    pub state: StartState,
}

impl WorktreeStamped for StartRequest {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

fn secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn path_for(base: &Path, worktree: &Path) -> (PathBuf, PathBuf) {
    let key = key(worktree);
    (key.clone(), base.join(file_name(&key)))
}

fn read_at(base: &Path, worktree: &Path) -> Option<StartRequest> {
    let (key, path) = path_for(base, worktree);
    read_ours(&path, &key)
}

fn write_at(base: &Path, request: &StartRequest) -> Result<()> {
    let (_, path) = path_for(base, &request.worktree);
    write_stamped(base, &path, request)
}

fn authoritative_agent(worktree: &Path) -> SessionAgent {
    let workspace = crate::usecase::session::workspace_root(worktree);
    let Ok(sessions) = crate::usecase::session::list(&workspace) else {
        return SessionAgent::default();
    };
    for session in sessions {
        if session.root == worktree {
            return session.agent;
        }
        for candidate in &session.worktrees {
            if candidate.path == worktree {
                return session.agent;
            }
        }
    }
    SessionAgent::default()
}

/// Publish or replace the launch transaction after the authoritative session
/// state has been saved. The monotonically increasing generation prevents a
/// late consumer from substituting a newer launch pair into an older request.
pub fn publish(worktree: &Path, prompt: &str, reuse_live_agent: bool) -> Result<StartRequest> {
    let base = dir(SUBDIR)?;
    publish_in(
        &base,
        worktree,
        prompt,
        reuse_live_agent,
        authoritative_agent(worktree),
    )
}

fn publish_in(
    base: &Path,
    worktree: &Path,
    prompt: &str,
    reuse_live_agent: bool,
    agent: SessionAgent,
) -> Result<StartRequest> {
    let _lock = StoreLock::acquire(base)?;
    let previous = read_at(base, worktree);
    let generation = previous
        .as_ref()
        .map_or(1, |r| r.generation.saturating_add(1));
    let now = secs(SystemTime::now());
    let request = StartRequest {
        worktree: key(worktree),
        id: now.rotate_left(17) ^ generation ^ stable_id(worktree),
        generation,
        prompt: prompt.to_string(),
        reuse_live_agent,
        agent,
        attempts: 0,
        state: StartState::Queued,
    };
    write_at(base, &request)?;
    Ok(request)
}

fn stable_id(path: &Path) -> u64 {
    u64::from_str_radix(&file_name(&key(path)), 16).unwrap_or(0)
}

pub fn claim(worktree: &Path, owner: &str, now: SystemTime) -> Result<Option<StartRequest>> {
    let base = dir(SUBDIR)?;
    claim_in(&base, worktree, owner, now)
}

fn claim_in(
    base: &Path,
    worktree: &Path,
    owner: &str,
    now: SystemTime,
) -> Result<Option<StartRequest>> {
    let _lock = StoreLock::acquire(base)?;
    let Some(mut request) = read_at(base, worktree) else {
        return Ok(None);
    };
    let available = matches!(request.state, StartState::Queued)
        || matches!(request.state, StartState::Claimed { lease_until, .. } if lease_until <= secs(now));
    if !available {
        return Ok(None);
    }
    request.attempts = request.attempts.saturating_add(1);
    request.state = StartState::Claimed {
        owner: owner.to_string(),
        lease_until: secs(now + LEASE),
    };
    write_at(base, &request)?;
    Ok(Some(request))
}

/// Compare-and-set a claimed request. Both id and owner must still match.
pub fn advance(worktree: &Path, id: u64, owner: &str, next: StartState) -> Result<StartRequest> {
    let base = dir(SUBDIR)?;
    advance_in(&base, worktree, id, owner, next)
}

/// Return a failed claim to the queue, or dead-letter it after the bounded
/// attempt count. The owner comparison prevents a timed-out worker from
/// overwriting a newer claimant's result.
pub fn fail(worktree: &Path, id: u64, owner: &str, error: &str) -> Result<StartRequest> {
    let Some(request) = read(worktree) else {
        return Err(anyhow!("start request disappeared"));
    };
    let next = if request.attempts
        >= crate::infrastructure::agent_prompt_store::MAX_PROMPT_RETRY_ATTEMPTS
    {
        StartState::Dead {
            attempts: request.attempts,
            error: error.to_string(),
        }
    } else {
        StartState::Queued
    };
    advance(worktree, id, owner, next)
}

fn advance_in(
    base: &Path,
    worktree: &Path,
    id: u64,
    owner: &str,
    next: StartState,
) -> Result<StartRequest> {
    let _lock = StoreLock::acquire(base)?;
    let Some(mut request) = read_at(base, worktree) else {
        return Err(anyhow!("start request disappeared"));
    };
    if request.id != id
        || !matches!(&request.state, StartState::Claimed { owner: held, .. } if held == owner)
    {
        return Err(anyhow!("start request claim changed"));
    }
    request.state = next;
    write_at(base, &request)?;
    Ok(request)
}

pub fn read(worktree: &Path) -> Option<StartRequest> {
    let base = dir(SUBDIR).ok()?;
    read_at(&base, worktree)
}

pub fn clear(worktree: &Path, id: u64) -> Result<bool> {
    let base = dir(SUBDIR)?;
    let _lock = StoreLock::acquire(&base)?;
    let Some(request) = read_at(&base, worktree) else {
        return Ok(false);
    };
    if request.id != id {
        return Ok(false);
    }
    let (_, path) = path_for(&base, worktree);
    fs::remove_file(path)?;
    Ok(true)
}

/// Discard any launch transaction for a session being removed.
pub fn clear_any(worktree: &Path) {
    let _ = try_clear_any(worktree);
}

pub fn try_clear_any(worktree: &Path) -> Result<()> {
    if let Some(request) = read(worktree) {
        clear(worktree, request.id)?;
    }
    Ok(())
}

pub fn queued_worktrees_in(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            serde_json::from_slice::<serde_json::Value>(&fs::read(entry.path()).ok()?)
                .ok()?
                .get("worktree")?
                .as_str()
                .map(PathBuf::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::AgentCli;
    use crate::domain::workspace_state::{
        BranchStatus, SessionRecord, WorkspaceState, WorktreeState,
    };
    use crate::infrastructure::workspace_store::WorkspaceStore;
    use chrono::Utc;

    fn agent(cli: AgentCli, model: &str) -> SessionAgent {
        SessionAgent {
            cli: Some(cli),
            model: Some(model.into()),
        }
    }

    #[test]
    fn publish_pins_pair_and_increments_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        fs::create_dir(&wt).unwrap();
        let a = publish_in(tmp.path(), &wt, "a", false, agent(AgentCli::Claude, "one")).unwrap();
        let b = publish_in(tmp.path(), &wt, "b", true, agent(AgentCli::Codex, "two")).unwrap();
        assert_eq!(a.generation, 1);
        assert_eq!(b.generation, 2);
        assert_eq!(a.agent.model.as_deref(), Some("one"));
        assert_eq!(b.agent.model.as_deref(), Some("two"));
        assert!(b.reuse_live_agent);
    }

    #[test]
    fn one_consumer_claims_and_expired_lease_is_recovered() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        fs::create_dir(&wt).unwrap();
        publish_in(tmp.path(), &wt, "go", false, SessionAgent::default()).unwrap();
        let epoch = UNIX_EPOCH + Duration::from_secs(100);
        assert!(claim_in(tmp.path(), &wt, "daemon", epoch)
            .unwrap()
            .is_some());
        assert!(claim_in(
            tmp.path(),
            &wt,
            "tui",
            epoch + LEASE - Duration::from_secs(1)
        )
        .unwrap()
        .is_none());
        assert!(claim_in(tmp.path(), &wt, "tui", epoch + LEASE)
            .unwrap()
            .is_some());
    }

    #[test]
    fn stale_owner_cannot_commit_after_takeover() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        fs::create_dir(&wt).unwrap();
        let request = publish_in(tmp.path(), &wt, "go", false, SessionAgent::default()).unwrap();
        let epoch = UNIX_EPOCH + Duration::from_secs(100);
        claim_in(tmp.path(), &wt, "daemon", epoch).unwrap();
        claim_in(tmp.path(), &wt, "tui", epoch + LEASE).unwrap();
        assert!(advance_in(
            tmp.path(),
            &wt,
            request.id,
            "daemon",
            StartState::Spawned { terminal: 1 }
        )
        .is_err());
        assert!(advance_in(
            tmp.path(),
            &wt,
            request.id,
            "tui",
            StartState::Spawned { terminal: 1 }
        )
        .is_ok());
    }

    #[test]
    fn listing_returns_stamped_worktrees_and_ignores_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        fs::create_dir(&wt).unwrap();
        publish_in(tmp.path(), &wt, "go", false, SessionAgent::default()).unwrap();
        fs::write(tmp.path().join("junk"), "not json").unwrap();
        assert_eq!(
            queued_worktrees_in(tmp.path()),
            vec![wt.canonicalize().unwrap()]
        );
        assert!(queued_worktrees_in(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn public_api_covers_retry_dead_letter_and_cleanup() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        let wt = home.path().join("wt");
        fs::create_dir(&wt).unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        assert!(claim(&wt, "none", SystemTime::now()).unwrap().is_none());
        let mut request = publish(&wt, "go", false).unwrap();
        assert_eq!(read(&wt).unwrap().id, request.id);
        assert_eq!(
            queued_worktrees_in(&home.path().join(SUBDIR)),
            vec![wt.canonicalize().unwrap()]
        );
        assert!(!clear(&wt, request.id.wrapping_add(1)).unwrap());
        for attempt in 1..=crate::infrastructure::agent_prompt_store::MAX_PROMPT_RETRY_ATTEMPTS {
            request = claim(&wt, "daemon", SystemTime::now()).unwrap().unwrap();
            let failed = fail(&wt, request.id, "daemon", "boom").unwrap();
            assert_eq!(failed.attempts, attempt);
        }
        assert!(matches!(read(&wt).unwrap().state, StartState::Dead { .. }));
        assert!(claim(&wt, "daemon", SystemTime::now()).unwrap().is_none());
        clear_any(&wt);
        assert!(read(&wt).is_none());
        assert!(!clear(&wt, request.id).unwrap());

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn public_listing_degrades_to_empty_when_directory_is_missing() {
        assert!(queued_worktrees_in(Path::new("/definitely/missing/start-requests")).is_empty());
    }

    #[test]
    fn every_persisted_state_round_trips() {
        let states = [
            StartState::Queued,
            StartState::Claimed {
                owner: "a".into(),
                lease_until: 1,
            },
            StartState::Spawned { terminal: 2 },
            StartState::InputAcknowledged { terminal: 3 },
            StartState::Running { terminal: 4 },
            StartState::Dead {
                attempts: 5,
                error: "x".into(),
            },
        ];
        for state in states {
            let encoded = serde_json::to_vec(&state).unwrap();
            assert_eq!(
                serde_json::from_slice::<StartState>(&encoded).unwrap(),
                state
            );
        }
    }

    fn session(root: PathBuf, worktrees: Vec<WorktreeState>, agent: SessionAgent) -> SessionRecord {
        SessionRecord {
            name: "work".into(),
            display_name: None,
            note: None,
            label_id: None,
            agent,
            origin: Default::default(),
            started_from: None,
            root,
            worktrees,
            todos: Vec::new(),
            decisions: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
        }
    }

    #[test]
    fn authoritative_agent_covers_root_worktree_and_unreadable_state() {
        let workspace = tempfile::tempdir().unwrap();
        let session_root = workspace.path().join(".usagi/sessions/work");
        fs::create_dir_all(&session_root).unwrap();
        let pair = agent(AgentCli::Codex, "root-model");
        let store = WorkspaceStore::new(workspace.path());
        store
            .save(&WorkspaceState {
                sessions: vec![session(session_root.clone(), Vec::new(), pair.clone())],
                ..WorkspaceState::default()
            })
            .unwrap();
        assert_eq!(authoritative_agent(&session_root), pair);

        let secondary = session_root.join("repo");
        fs::create_dir(&secondary).unwrap();
        let pair = agent(AgentCli::Claude, "secondary-model");
        store
            .save(&WorkspaceState {
                sessions: vec![session(
                    session_root.clone(),
                    vec![
                        WorktreeState {
                            branch: Some("other".into()),
                            path: session_root.join("other"),
                            head: "def5678".into(),
                            primary: false,
                            upstream: None,
                            status: BranchStatus::New,
                            diff: None,
                            ahead_behind: None,
                            pr: Vec::new(),
                            updated_at: Utc::now(),
                        },
                        WorktreeState {
                            branch: Some("work".into()),
                            path: secondary.clone(),
                            head: "abc1234".into(),
                            primary: false,
                            upstream: None,
                            status: BranchStatus::New,
                            diff: None,
                            ahead_behind: None,
                            pr: Vec::new(),
                            updated_at: Utc::now(),
                        },
                    ],
                    pair.clone(),
                )],
                ..WorkspaceState::default()
            })
            .unwrap();
        assert_eq!(authoritative_agent(&secondary), pair);

        fs::write(store.state_path(), "not json").unwrap();
        assert_eq!(authoritative_agent(&session_root), SessionAgent::default());
    }

    #[test]
    fn missing_request_errors_on_failure_and_advance() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("missing");
        assert!(advance_in(tmp.path(), &wt, 1, "daemon", StartState::Queued).is_err());

        let _guard = crate::test_support::process_env_guard();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, tmp.path());
        assert!(fail(&wt, 1, "daemon", "boom").is_err());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }
}
