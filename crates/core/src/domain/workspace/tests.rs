use super::{Workspace, WorkspaceOverview};

#[test]
fn new_stamps_equal_created_and_updated_times() {
    let ws = Workspace::new("app", "/home/user/app");
    assert_eq!(ws.name, "app");
    assert_eq!(ws.path.to_str(), Some("/home/user/app"));
    // Both timestamps are taken from a single `Utc::now()`, so they match.
    assert_eq!(ws.created_at, ws.updated_at);
    // Exercise the derived Clone / PartialEq / Debug.
    assert_eq!(ws.clone(), ws);
    assert!(format!("{ws:?}").contains("app"));
}

#[test]
fn workspace_round_trips_through_json() {
    let ws = Workspace::new("app", "/home/user/app");
    let json = serde_json::to_string(&ws).unwrap();
    let back: Workspace = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ws);
}

#[test]
fn overview_carries_the_workspace_and_its_counts() {
    let ws = Workspace::new("app", "/home/user/app");
    let overview = WorkspaceOverview::new(ws.clone(), 2, 4, 1);
    assert_eq!(overview.workspace, ws);
    assert_eq!(overview.session_count, 2);
    assert_eq!(overview.open_issue_count, 4);
    assert_eq!(overview.pr_count, 1);
    // Exercise the derived Clone / PartialEq / Debug.
    assert_eq!(overview.clone(), overview);
    assert!(format!("{overview:?}").contains("app"));
}
