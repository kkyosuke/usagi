//! End-to-end check that the daemon's session aggregation reads the *real*
//! stores: a session written to a workspace's `state.json` and an agent phase
//! recorded for its worktree flow through [`usagi::usecase::daemon::gather`] into
//! the snapshot the daemon persists. The pure aggregation is unit-tested with
//! fakes in the usecase; this exercises the actual `WorkspaceStore` +
//! `agent_state_store` reads the composition-root adapters wire up.

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};

use usagi::domain::agent_phase::AgentPhase;
use usagi::domain::daemon::{SessionActivity, SessionSnapshot};
use usagi::domain::workspace_state::{
    BranchStatus, SessionAgent, SessionRecord, WorkspaceState, WorktreeState,
};
use usagi::infrastructure::{agent_state_store, workspace_store::WorkspaceStore};
use usagi::usecase::daemon::gather;

#[test]
fn gather_reads_sessions_and_phases_from_the_real_stores() {
    // agent_state_store resolves its data dir from $USAGI_HOME; point it at a
    // temp dir. This is the only test in this binary, so the env write is safe.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("USAGI_HOME", home.path());

    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().to_path_buf();
    let worktree = root.join(".usagi").join("sessions").join("work");
    std::fs::create_dir_all(&worktree).unwrap();

    // One session in the workspace, its agent running in `worktree`.
    let mut state = WorkspaceState::new();
    state.sessions = vec![SessionRecord {
        name: "work".to_string(),
        display_name: None,
        note: None,
        todos: Vec::new(),
        decisions: Vec::new(),
        label_id: None,
        agent: SessionAgent::default(),
        origin: Default::default(),
        started_from: None,
        root: worktree.clone(),
        worktrees: vec![WorktreeState {
            branch: Some("usagi/work".to_string()),
            path: worktree.clone(),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::default(),
            diff: None,
            ahead_behind: None,
            pr: vec![],
            updated_at: Utc.timestamp_opt(0, 0).unwrap(),
        }],
        created_at: Utc.timestamp_opt(0, 0).unwrap(),
        last_active: None,
    }];
    WorkspaceStore::new(&root).save(&state).unwrap();

    // The agent's hook recorded a Waiting phase for that worktree.
    agent_state_store::write(&worktree, AgentPhase::Waiting).unwrap();

    let snapshots = gather(
        &|| vec![root.clone()],
        &|r: &Path| {
            WorkspaceStore::new(r)
                .load()
                .unwrap()
                .map(|s| {
                    s.sessions
                        .into_iter()
                        .map(|session| {
                            (
                                session.name,
                                session.worktrees.into_iter().map(|w| w.path).collect(),
                            )
                        })
                        .collect::<Vec<(String, Vec<PathBuf>)>>()
                })
                .unwrap_or_default()
        },
        &agent_state_store::read,
    );

    assert_eq!(
        snapshots,
        vec![SessionSnapshot {
            workspace: root,
            name: "work".to_string(),
            worktree: Some(worktree),
            activity: Some(SessionActivity::Waiting),
        }]
    );

    std::env::remove_var("USAGI_HOME");
}
