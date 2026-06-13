use usagi::domain::project::{ProjectState, SessionStatus, Worktree};

#[test]
fn test_project_state_basic() {
    let state = ProjectState {
        initialized: true,
        worktrees: vec![Worktree {
            branch: "main".to_string(),
            directory: "main".to_string(),
            default: true,
            status: SessionStatus::Todo,
        }],
        current_worktree: Some("main".to_string()),
    };

    assert!(state.initialized);
    assert_eq!(state.worktrees.len(), 1);
    assert_eq!(state.current_worktree.as_deref(), Some("main"));
}

#[test]
fn test_project_state_serde_roundtrip() {
    let state = ProjectState::default();
    let json = serde_json::to_string(&state).unwrap();
    let restored: ProjectState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.initialized, state.initialized);
}
