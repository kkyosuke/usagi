use super::Workspace;

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
