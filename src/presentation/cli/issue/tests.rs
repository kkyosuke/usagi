use super::render::render_list;
use super::*;

fn create(repo: &Path, title: &str, deps: Vec<u32>) {
    execute(
        repo,
        IssueCommand::Create {
            title: title.to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: deps,
            related: vec![],
            parent: None,
            milestone: None,
            body: String::new(),
            json: false,
        },
    )
    .unwrap();
}

#[test]
fn create_reports_the_new_number_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    let lines = execute(
        repo,
        IssueCommand::Create {
            title: "First task".to_string(),
            priority: IssuePriority::High,
            labels: vec!["cli".to_string()],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            body: "details".to_string(),
            json: false,
        },
    )
    .unwrap();

    assert_eq!(lines, vec!["created #1: First task"]);
    assert_eq!(
        issue::get(repo, 1).unwrap().unwrap().priority,
        IssuePriority::High
    );
}

#[test]
fn create_with_json_emits_the_issue() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = execute(
        tmp.path(),
        IssueCommand::Create {
            title: "T".to_string(),
            priority: IssuePriority::Low,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            body: String::new(),
            json: true,
        },
    )
    .unwrap();
    let json = lines.join("\n");
    assert!(json.contains("\"number\": 1"));
    assert!(json.contains("\"priority\": \"low\""));
}

#[test]
fn list_marks_ready_blocked_and_done() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "base", vec![]);
    create(repo, "blocked", vec![1]);

    let lines = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: None,
            ready: false,
            json: false,
        },
    )
    .unwrap();

    assert!(lines[0].contains("#1"));
    assert!(lines[0].contains("ready"));
    assert!(lines[1].contains("#2"));
    assert!(lines[1].contains("blocked"));
    assert!(lines[1].contains("(blocked by 1)"));
}

#[test]
fn list_reports_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = execute(
        tmp.path(),
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: None,
            ready: false,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(lines, vec!["No issues found."]);
}

#[test]
fn list_ready_only_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "base", vec![]);
    create(repo, "blocked", vec![1]);

    // ready filter keeps only #1.
    let ready = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: None,
            ready: true,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(ready.len(), 1);
    assert!(ready[0].contains("#1"));

    // JSON output carries the readiness annotation.
    let json = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: None,
            ready: false,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(json.contains("\"ready\": true"));
    assert!(json.contains("\"unmet_deps\""));
}

#[test]
fn done_issue_is_marked_done_in_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "task", vec![]);
    execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: None,
            status: Some(IssueStatus::Done),
            priority: None,
            labels: None,
            dependson: None,
            related: None,
            parent: None,
            clear_parent: false,
            milestone: None,
            clear_milestone: false,
            body: None,
            json: false,
        },
    )
    .unwrap();

    let lines = render_list(&issue::list(repo, &IssueFilter::default()).unwrap());
    assert!(lines[0].contains("done"));
}

#[test]
fn show_renders_markdown_or_json_or_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "Visible", vec![]);

    let md = execute(
        repo,
        IssueCommand::Show {
            number: 1,
            json: false,
        },
    )
    .unwrap();
    assert!(md.iter().any(|l| l.contains("title: Visible")));

    let json = execute(
        repo,
        IssueCommand::Show {
            number: 1,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(json.contains("\"body\""));

    let missing = execute(
        repo,
        IssueCommand::Show {
            number: 9,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no issue #9"]);
}

#[test]
fn update_changes_fields_or_reports_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "Old", vec![]);

    let lines = execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: Some("New".to_string()),
            status: None,
            priority: None,
            labels: Some(vec!["x".to_string()]),
            dependson: Some(vec![2]),
            related: None,
            parent: None,
            clear_parent: false,
            milestone: None,
            clear_milestone: false,
            body: Some("b".to_string()),
            json: false,
        },
    )
    .unwrap();
    assert_eq!(lines, vec!["updated #1: New"]);
    let stored = issue::get(repo, 1).unwrap().unwrap();
    assert_eq!(stored.labels, vec!["x"]);
    assert_eq!(stored.dependson, vec![2]);

    // JSON variant.
    let json = execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: None,
            status: Some(IssueStatus::InProgress),
            priority: None,
            labels: None,
            dependson: None,
            related: None,
            parent: None,
            clear_parent: false,
            milestone: None,
            clear_milestone: false,
            body: None,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(json.contains("\"status\": \"in-progress\""));

    // Missing issue.
    let missing = execute(
        repo,
        IssueCommand::Update {
            number: 9,
            title: None,
            status: None,
            priority: None,
            labels: None,
            dependson: None,
            related: None,
            parent: None,
            clear_parent: false,
            milestone: None,
            clear_milestone: false,
            body: None,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no issue #9"]);
}

#[test]
fn search_filters_and_supports_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "Login bug", vec![]);
    create(repo, "Unrelated", vec![]);

    let hits = execute(
        repo,
        IssueCommand::Search {
            query: "login".to_string(),
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            ready: false,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].contains("Login bug"));

    let json = execute(
        repo,
        IssueCommand::Search {
            query: "login".to_string(),
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            ready: false,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(json.contains("Login bug"));
}

#[test]
fn delete_requires_yes_and_reports_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "Doomed", vec![]);

    // Without --yes nothing is deleted.
    let refused = execute(
        repo,
        IssueCommand::Delete {
            number: 1,
            yes: false,
        },
    )
    .unwrap();
    assert_eq!(refused, vec!["pass --yes to delete #1"]);
    assert!(issue::get(repo, 1).unwrap().is_some());

    // With --yes it is deleted.
    let deleted = execute(
        repo,
        IssueCommand::Delete {
            number: 1,
            yes: true,
        },
    )
    .unwrap();
    assert_eq!(deleted, vec!["deleted #1"]);

    // Deleting a missing issue reports so.
    let missing = execute(
        repo,
        IssueCommand::Delete {
            number: 1,
            yes: true,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no issue #1"]);
}

#[test]
fn optional_change_maps_value_clear_and_absent() {
    assert_eq!(optional_change(Some(5), false), Some(Some(5)));
    assert_eq!(optional_change::<u32>(None, true), Some(None));
    assert_eq!(optional_change::<u32>(None, false), None);
    // A value wins over a stray clear flag (clap normally forbids both).
    assert_eq!(optional_change(Some(5), true), Some(Some(5)));
}

#[test]
fn create_accepts_relations_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    execute(
        repo,
        IssueCommand::Create {
            title: "child".to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![3],
            parent: Some(2),
            milestone: Some("v1".to_string()),
            body: String::new(),
            json: false,
        },
    )
    .unwrap();
    let stored = issue::get(repo, 1).unwrap().unwrap();
    assert_eq!(stored.related, vec![3]);
    assert_eq!(stored.parent, Some(2));
    assert_eq!(stored.milestone, Some("v1".to_string()));
}

#[test]
fn update_sets_then_clears_parent_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "task", vec![]);

    // Set parent + milestone, replace related.
    execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: None,
            status: None,
            priority: None,
            labels: None,
            dependson: None,
            related: Some(vec![9]),
            parent: Some(5),
            clear_parent: false,
            milestone: Some("v2".to_string()),
            clear_milestone: false,
            body: None,
            json: false,
        },
    )
    .unwrap();
    let after_set = issue::get(repo, 1).unwrap().unwrap();
    assert_eq!(after_set.parent, Some(5));
    assert_eq!(after_set.milestone, Some("v2".to_string()));
    assert_eq!(after_set.related, vec![9]);

    // Clear flags remove the optional fields; related left untouched.
    execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: None,
            status: None,
            priority: None,
            labels: None,
            dependson: None,
            related: None,
            parent: None,
            clear_parent: true,
            milestone: None,
            clear_milestone: true,
            body: None,
            json: false,
        },
    )
    .unwrap();
    let after_clear = issue::get(repo, 1).unwrap().unwrap();
    assert_eq!(after_clear.parent, None);
    assert_eq!(after_clear.milestone, None);
    assert_eq!(after_clear.related, vec![9]);
}

#[test]
fn list_filters_by_parent_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "epic", vec![]);
    execute(
        repo,
        IssueCommand::Create {
            title: "child".to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: Some(1),
            milestone: Some("v1".to_string()),
            body: String::new(),
            json: false,
        },
    )
    .unwrap();

    let by_parent = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: Some(1),
            milestone: None,
            group_by: None,
            ready: false,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(by_parent.len(), 1);
    assert!(by_parent[0].contains("child"));

    let by_milestone = execute(
        repo,
        IssueCommand::Search {
            query: String::new(),
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: Some("v1".to_string()),
            ready: false,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(by_milestone.len(), 1);
}

#[test]
fn graph_renders_a_tree_with_a_progress_footer() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "root", vec![]);
    create(repo, "child", vec![1]);

    let lines = execute(repo, IssueCommand::Graph).unwrap();
    assert!(lines[0].contains("#1 root"));
    assert!(lines.iter().any(|l| l.contains("└─ #2 child")));
    assert!(lines.iter().any(|l| l.contains("2 issues")));
    assert!(lines.iter().any(|l| l.contains("ready")));
}

#[test]
fn graph_reports_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = execute(tmp.path(), IssueCommand::Graph).unwrap();
    assert_eq!(lines, vec!["No issues found."]);
}

#[test]
fn list_grouped_by_status_emits_headers_and_footers() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "a", vec![]);
    create(repo, "b", vec![]);
    execute(
        repo,
        IssueCommand::Update {
            number: 1,
            title: None,
            status: Some(IssueStatus::Done),
            priority: None,
            labels: None,
            dependson: None,
            related: None,
            parent: None,
            clear_parent: false,
            milestone: None,
            clear_milestone: false,
            body: None,
            json: false,
        },
    )
    .unwrap();

    let lines = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: Some(GroupBy::Status),
            ready: false,
            json: false,
        },
    )
    .unwrap();
    let text = lines.join("\n");
    assert!(text.contains("== todo =="));
    assert!(text.contains("== done =="));
    // Overall footer reflects 1 of 2 done.
    assert!(text.contains("2 issues · 1 done (50%)"));
}

#[test]
fn list_grouped_reports_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = execute(
        tmp.path(),
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: Some(GroupBy::Priority),
            ready: false,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(lines, vec!["No issues found."]);
}

#[test]
fn grouping_is_ignored_for_json_output() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, "a", vec![]);
    // With --json the grouped rendering is bypassed in favor of the array.
    let json = execute(
        repo,
        IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            parent: None,
            milestone: None,
            group_by: Some(GroupBy::Status),
            ready: false,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(json.contains("\"number\": 1"));
    assert!(!json.contains("=="));
}

#[test]
fn execute_propagates_store_errors() {
    let tmp = tempfile::tempdir().unwrap();
    // A file where the `.usagi` directory should be makes the store fail,
    // and the error propagates out of `execute`.
    std::fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
    let result = execute(
        tmp.path(),
        IssueCommand::Create {
            title: "boom".to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            body: String::new(),
            json: false,
        },
    );
    assert!(result.is_err());
}

#[test]
fn run_executes_against_the_current_directory() {
    // `run` reads the current directory; point it at a throwaway repo.
    let _guard = crate::test_support::process_env_guard();
    let tmp = tempfile::tempdir().unwrap();
    let original = env::current_dir().unwrap();
    env::set_current_dir(tmp.path()).unwrap();
    let result = run(IssueCommand::List {
        status: None,
        priority: None,
        label: None,
        parent: None,
        milestone: None,
        group_by: None,
        ready: false,
        json: false,
    });
    env::set_current_dir(original).unwrap();
    assert!(result.is_ok());
}
