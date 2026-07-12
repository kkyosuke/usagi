use chrono::{DateTime, Duration, Utc};

use super::{Recent, UniteOverview};
use crate::domain::workspace::{Workspace, WorkspaceOverview};

fn base() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn overview(
    name: &str,
    minutes_ago: i64,
    sessions: usize,
    open: usize,
    pr: usize,
) -> WorkspaceOverview {
    let mut workspace = Workspace::new(name, format!("/tmp/{name}"));
    workspace.updated_at = base() - Duration::minutes(minutes_ago);
    WorkspaceOverview::new(workspace, sessions, open, pr)
}

#[test]
fn unite_aggregates_counts_and_takes_the_latest_time() {
    let unite = UniteOverview::new(vec![
        overview("alpha", 30, 2, 4, 1),
        overview("beta", 10, 1, 0, 2),
    ]);
    assert_eq!(unite.members().len(), 2);
    assert_eq!(unite.primary_name(), "alpha");
    assert_eq!(unite.extra_count(), 1);
    // Counts are summed across the members.
    assert_eq!(unite.session_count(), 3);
    assert_eq!(unite.open_issue_count(), 4);
    assert_eq!(unite.pr_count(), 3);
    // The group's time is the most recent member's (beta, 10min ago).
    assert_eq!(unite.updated_at(), Some(base() - Duration::minutes(10)));
    // Exercise the derived Clone / PartialEq / Debug.
    assert_eq!(unite.clone(), unite);
    assert!(format!("{unite:?}").contains("alpha"));
}

#[test]
fn empty_unite_reports_neutral_values() {
    let unite = UniteOverview::new(Vec::new());
    assert_eq!(unite.primary_name(), "");
    assert_eq!(unite.extra_count(), 0);
    assert_eq!(unite.updated_at(), None);
    assert_eq!(unite.session_count(), 0);
    assert_eq!(unite.open_issue_count(), 0);
    assert_eq!(unite.pr_count(), 0);
}

#[test]
fn recent_updated_at_dispatches_by_variant() {
    let workspace = Recent::Workspace(overview("solo", 5, 1, 1, 1));
    assert_eq!(workspace.updated_at(), Some(base() - Duration::minutes(5)));

    let unite = Recent::Unite(UniteOverview::new(vec![
        overview("alpha", 30, 1, 1, 1),
        overview("beta", 8, 1, 1, 1),
    ]));
    assert_eq!(unite.updated_at(), Some(base() - Duration::minutes(8)));

    // Exercise the derived Clone / PartialEq / Debug on the enum.
    assert_eq!(workspace.clone(), workspace);
    assert!(format!("{unite:?}").contains("Unite"));
}
