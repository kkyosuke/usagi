use super::*;
use chrono::{TimeZone, Utc};

/// Build a listed issue from the fields the visualization helpers read,
/// without touching disk.
fn listed(
    number: u32,
    status: IssueStatus,
    dependson: Vec<u32>,
    unmet_deps: Vec<u32>,
    parent: Option<u32>,
    milestone: Option<&str>,
) -> ListedIssue {
    let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
    ListedIssue {
        summary: IssueSummary {
            number,
            title: format!("issue {number}"),
            status,
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson,
            related: vec![],
            parent,
            milestone: milestone.map(str::to_string),
            file: format!("{number:03}-issue.md"),
            created_at: ts,
            updated_at: ts,
        },
        unmet_deps,
    }
}

#[test]
fn stats_tally_status_readiness_and_progress() {
    let items = vec![
        listed(1, IssueStatus::Done, vec![], vec![], None, None),
        listed(2, IssueStatus::InProgress, vec![], vec![], None, None),
        listed(3, IssueStatus::Todo, vec![], vec![], None, None),
        listed(4, IssueStatus::Todo, vec![3], vec![3], None, None),
    ];
    let stats = IssueStats::from_listed(&items);
    assert_eq!(stats.total, 4);
    assert_eq!(stats.done, 1);
    assert_eq!(stats.in_progress, 1);
    assert_eq!(stats.todo, 2);
    // Ready = not done with all deps met: #2 (in-progress) and #3 (todo).
    // #4 is blocked by #3; #1 is done.
    assert_eq!(stats.ready, 2);
    assert_eq!(stats.completion_percent(), 25);
    assert_eq!(stats.progress_bar(8), "[##------]");
}

#[test]
fn empty_stats_have_zero_completion_and_empty_bar() {
    let stats = IssueStats::from_listed(&[]);
    assert_eq!(stats.completion_percent(), 0);
    assert_eq!(stats.progress_bar(4), "[----]");
}

#[test]
fn stats_progress_math_is_overflow_safe_for_huge_counts() {
    // Counts near `usize::MAX` must not overflow the internal `done * 100` /
    // `done * width` before the divide. The visible output is unchanged from the
    // ordinary case: half done is 50% and fills half the bar.
    let stats = IssueStats {
        total: usize::MAX - 1,
        done: (usize::MAX - 1) / 2,
        todo: (usize::MAX - 1) / 2,
        in_progress: 0,
        ready: 0,
    };
    assert_eq!(stats.completion_percent(), 50);
    assert_eq!(stats.progress_bar(8), "[####----]");
}

#[test]
fn group_by_round_trips_through_string() {
    for g in [
        GroupBy::Status,
        GroupBy::Priority,
        GroupBy::Milestone,
        GroupBy::Parent,
    ] {
        assert_eq!(g.to_string().parse::<GroupBy>().unwrap(), g);
    }
    assert!("nope".parse::<GroupBy>().is_err());
}

#[test]
fn group_orders_status_and_keeps_lifecycle_order() {
    let items = vec![
        listed(1, IssueStatus::Done, vec![], vec![], None, None),
        listed(2, IssueStatus::Todo, vec![], vec![], None, None),
        listed(3, IssueStatus::InProgress, vec![], vec![], None, None),
    ];
    let groups = group(items, GroupBy::Status);
    let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["todo", "in-progress", "done"]);
}

#[test]
fn group_by_priority_orders_and_merges_same_bucket() {
    let mut high = listed(1, IssueStatus::Todo, vec![], vec![], None, None);
    high.summary.priority = IssuePriority::High;
    let mut low = listed(2, IssueStatus::Todo, vec![], vec![], None, None);
    low.summary.priority = IssuePriority::Low;
    // Two mediums land in the same bucket, exercising the merge path.
    let med_a = listed(3, IssueStatus::Todo, vec![], vec![], None, None);
    let med_b = listed(4, IssueStatus::Todo, vec![], vec![], None, None);

    let groups = group(vec![high, low, med_a, med_b], GroupBy::Priority);
    let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["high", "medium", "low"]);
    let medium = groups.iter().find(|(l, _)| l == "medium").unwrap();
    assert_eq!(medium.1.len(), 2);
}

#[test]
fn group_by_milestone_and_parent_put_none_last() {
    let items = vec![
        listed(1, IssueStatus::Todo, vec![], vec![], None, Some("v2")),
        listed(2, IssueStatus::Todo, vec![], vec![], None, Some("v1")),
        listed(3, IssueStatus::Todo, vec![], vec![], None, None),
    ];
    let groups = group(items, GroupBy::Milestone);
    let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["v1", "v2", "(no milestone)"]);

    let items = vec![
        listed(1, IssueStatus::Todo, vec![], vec![], Some(10), None),
        listed(2, IssueStatus::Todo, vec![], vec![], Some(2), None),
        listed(3, IssueStatus::Todo, vec![], vec![], None, None),
    ];
    let groups = group(items, GroupBy::Parent);
    let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
    // #2 sorts before #10 numerically, "(no parent)" last.
    assert_eq!(labels, vec!["#2", "#10", "(no parent)"]);
}

#[test]
fn dependency_tree_nests_dependents_under_dependencies() {
    // #1 root; #2 and #3 depend on #1; #4 depends on #2.
    let items = vec![
        listed(1, IssueStatus::Done, vec![], vec![], None, None),
        listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
        listed(3, IssueStatus::Todo, vec![1], vec![], None, None),
        listed(4, IssueStatus::Todo, vec![2], vec![], None, None),
    ];
    let lines = dependency_tree(&items);
    assert_eq!(lines[0], "#1 issue 1 [done]");
    assert!(lines[1].contains("├─ #2 issue 2 [todo]"));
    assert!(lines.iter().any(|l| l.contains("└─ #4 issue 4 [todo]")));
    assert!(lines.iter().any(|l| l.contains("└─ #3 issue 3 [todo]")));
}

#[test]
fn dependency_tree_marks_repeats_and_handles_cycles_and_missing() {
    // A diamond: #4 depends on both #2 and #3, which both depend on #1.
    let diamond = vec![
        listed(1, IssueStatus::Todo, vec![], vec![], None, None),
        listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
        listed(3, IssueStatus::Todo, vec![1], vec![], None, None),
        listed(4, IssueStatus::Todo, vec![2, 3], vec![], None, None),
    ];
    let lines = dependency_tree(&diamond);
    // #4 appears under both #2 and #3; one of them carries the ↑ repeat mark.
    assert!(lines.iter().filter(|l| l.contains("#4 issue 4")).count() >= 2);
    assert!(lines.iter().any(|l| l.contains('↑')));

    // A pure cycle (#1↔#2) still terminates and shows both nodes.
    let cycle = vec![
        listed(1, IssueStatus::Todo, vec![2], vec![], None, None),
        listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
    ];
    let lines = dependency_tree(&cycle);
    assert!(lines.iter().any(|l| l.contains("#1 issue 1")));
    assert!(lines.iter().any(|l| l.contains("#2 issue 2")));

    // A dependency on a non-existent issue is shown as missing.
    let orphan = vec![listed(1, IssueStatus::Todo, vec![99], vec![99], None, None)];
    let lines = dependency_tree(&orphan);
    assert!(lines.iter().any(|l| l.contains("#99 (missing)")));
}

#[test]
fn dependency_tree_caps_a_pathologically_deep_chain_without_overflowing() {
    // A single linear chain #1 ← #2 ← … ← #N far deeper than MAX_DEPTH (256):
    // #2 depends on #1, #3 on #2, and so on. Without a depth cap this recurses
    // N deep and overflows the stack; the cap truncates it with a marker instead.
    let n: u32 = 5_000;
    let mut items = vec![listed(1, IssueStatus::Todo, vec![], vec![], None, None)];
    items.extend((2..=n).map(|k| listed(k, IssueStatus::Todo, vec![k - 1], vec![], None, None)));

    // Must not panic / overflow the stack, and must surface the truncation.
    let lines = dependency_tree(&items);
    assert!(lines.iter().any(|l| l.contains("depth limit reached")));
    // Nothing is silently dropped: truncation re-roots the remaining chain, so
    // both ends still appear somewhere in the output.
    assert!(lines.iter().any(|l| l.contains("#1 issue 1 ")));
    assert!(lines.iter().any(|l| l.contains("#5000 issue 5000 ")));
}

#[test]
fn to_prompt_includes_metadata_and_body() {
    let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
    let issue = Issue {
        number: 7,
        title: "Add doctor".to_string(),
        status: IssueStatus::Todo,
        priority: IssuePriority::High,
        labels: vec!["cli".to_string(), "ux".to_string()],
        dependson: vec![3, 4],
        related: vec![5],
        parent: Some(2),
        milestone: Some("v1".to_string()),
        created_at: ts,
        updated_at: ts,
        body: "  Diagnose the environment.  ".to_string(),
    };

    let prompt = to_prompt(&issue);
    assert!(prompt.contains("issue #7「Add doctor」"));
    assert!(prompt.contains("- status: todo"));
    assert!(prompt.contains("- priority: high"));
    assert!(prompt.contains("- labels: cli, ux"));
    assert!(prompt.contains("- dependson: 3, 4"));
    assert!(prompt.contains("- related: 5"));
    assert!(prompt.contains("- parent: 2"));
    assert!(prompt.contains("- milestone: v1"));
    // The body is trimmed of surrounding whitespace.
    assert!(prompt.contains("\nDiagnose the environment.\n"));
    // The prompt stays repository-agnostic: it instructs the agent to follow
    // the repo's own conventions rather than hardcoding usagi/Rust specifics.
    assert!(prompt.contains("リポジトリの規約"));
    assert!(!prompt.contains("cargo"));
    assert!(!prompt.contains(".agents/workflow.md"));
}

#[test]
fn to_prompt_renders_placeholders_for_empty_fields() {
    let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
    let issue = Issue {
        number: 1,
        title: "Bare".to_string(),
        status: IssueStatus::InProgress,
        priority: IssuePriority::Medium,
        labels: vec![],
        dependson: vec![],
        related: vec![],
        parent: None,
        milestone: None,
        created_at: ts,
        updated_at: ts,
        body: "   ".to_string(),
    };

    let prompt = to_prompt(&issue);
    assert!(prompt.contains("- labels: なし"));
    assert!(prompt.contains("- dependson: なし"));
    assert!(prompt.contains("- related: なし"));
    assert!(prompt.contains("- parent: なし"));
    assert!(prompt.contains("- milestone: なし"));
    assert!(prompt.contains("（本文なし）"));
}

fn new_issue(title: &str) -> NewIssue {
    NewIssue {
        title: title.to_string(),
        priority: IssuePriority::Medium,
        labels: vec![],
        dependson: vec![],
        related: vec![],
        parent: None,
        milestone: None,
        body: String::new(),
    }
}

#[test]
fn create_assigns_increasing_numbers_and_defaults_to_todo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    let first = create(repo, new_issue("First")).unwrap();
    let second = create(repo, new_issue("Second")).unwrap();

    assert_eq!(first.number, 1);
    assert_eq!(second.number, 2);
    assert_eq!(first.status, IssueStatus::Todo);
    assert_eq!(get(repo, 1).unwrap().unwrap().title, "First");
    assert!(get(repo, 99).unwrap().is_none());
}

#[test]
fn create_numbers_issues_across_the_whole_workspace() {
    // A workspace root with two session worktrees mirrored under
    // `.usagi/sessions/`. Issues live in each worktree's own store, but
    // numbering is shared so two branches never reuse a number and collide on
    // merge.
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    let session_a = workspace.join(".usagi").join("sessions").join("a");
    let session_b = workspace.join(".usagi").join("sessions").join("b");

    // A loose file under the sessions dir must be skipped by the scan.
    std::fs::create_dir_all(workspace.join(".usagi").join("sessions")).unwrap();
    std::fs::write(
        workspace.join(".usagi").join("sessions").join("note.txt"),
        "x",
    )
    .unwrap();

    let w1 = create(workspace, new_issue("workspace one")).unwrap();
    let a2 = create(&session_a, new_issue("session a")).unwrap();
    let b3 = create(&session_b, new_issue("session b")).unwrap();
    let w4 = create(workspace, new_issue("workspace two")).unwrap();

    // Numbers increase across every worktree and are never reused.
    assert_eq!((w1.number, a2.number, b3.number, w4.number), (1, 2, 3, 4));

    // Each issue is stored only in the worktree it was created in.
    assert!(get(&session_a, 2).unwrap().is_some());
    assert!(get(workspace, 2).unwrap().is_none());
    assert!(get(&session_b, 3).unwrap().is_some());
    assert!(get(workspace, 3).unwrap().is_none());
    assert_eq!(get(workspace, 1).unwrap().unwrap().title, "workspace one");
    assert_eq!(get(workspace, 4).unwrap().unwrap().title, "workspace two");
}

#[test]
fn update_applies_only_set_fields_and_touches_updated_at() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let created = create(repo, new_issue("Title")).unwrap();

    let updated = update(
        repo,
        1,
        IssueChanges {
            status: Some(IssueStatus::Done),
            body: Some("done now".to_string()),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(updated.status, IssueStatus::Done);
    assert_eq!(updated.body, "done now");
    // Untouched fields are preserved.
    assert_eq!(updated.title, "Title");
    assert_eq!(updated.priority, created.priority);
    assert!(updated.updated_at >= created.updated_at);
}

#[test]
fn update_can_change_title_priority_labels_and_deps() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, new_issue("Old")).unwrap();

    let updated = update(
        repo,
        1,
        IssueChanges {
            title: Some("New".to_string()),
            priority: Some(IssuePriority::High),
            labels: Some(vec!["cli".to_string()]),
            dependson: Some(vec![2]),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(updated.title, "New");
    assert_eq!(updated.priority, IssuePriority::High);
    assert_eq!(updated.labels, vec!["cli"]);
    assert_eq!(updated.dependson, vec![2]);
}

#[test]
fn update_returns_none_for_a_missing_issue() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(update(tmp.path(), 1, IssueChanges::default())
        .unwrap()
        .is_none());
}

#[test]
fn delete_reports_whether_the_issue_existed() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, new_issue("Doomed")).unwrap();

    assert!(delete(repo, 1).unwrap());
    assert!(!delete(repo, 1).unwrap());
}

#[test]
fn changes_is_empty_detects_no_op_updates() {
    assert!(IssueChanges::default().is_empty());
    assert!(!IssueChanges {
        status: Some(IssueStatus::Done),
        ..Default::default()
    }
    .is_empty());
    // The relation fields also count as changes.
    assert!(!IssueChanges {
        parent: Some(Some(1)),
        ..Default::default()
    }
    .is_empty());
    assert!(!IssueChanges {
        milestone: Some(None),
        ..Default::default()
    }
    .is_empty());
    assert!(!IssueChanges {
        related: Some(vec![2]),
        ..Default::default()
    }
    .is_empty());
}

#[test]
fn create_persists_relations_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let created = create(
        repo,
        NewIssue {
            related: vec![3],
            parent: Some(2),
            milestone: Some("v1".to_string()),
            ..new_issue("child")
        },
    )
    .unwrap();
    assert_eq!(created.related, vec![3]);
    assert_eq!(created.parent, Some(2));
    assert_eq!(created.milestone, Some("v1".to_string()));
}

#[test]
fn update_sets_and_clears_parent_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            parent: Some(2),
            milestone: Some("v1".to_string()),
            ..new_issue("task")
        },
    )
    .unwrap();

    // Setting replaces; related is replaced wholesale like dependson.
    let set = update(
        repo,
        1,
        IssueChanges {
            parent: Some(Some(5)),
            milestone: Some(Some("v2".to_string())),
            related: Some(vec![9]),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(set.parent, Some(5));
    assert_eq!(set.milestone, Some("v2".to_string()));
    assert_eq!(set.related, vec![9]);

    // An outer Some(None) clears the optional field.
    let cleared = update(
        repo,
        1,
        IssueChanges {
            parent: Some(None),
            milestone: Some(None),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(cleared.parent, None);
    assert_eq!(cleared.milestone, None);
    // An outer None leaves the field untouched.
    assert_eq!(cleared.related, vec![9]);
}

#[test]
fn list_filters_by_parent_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(repo, new_issue("epic")).unwrap();
    create(
        repo,
        NewIssue {
            parent: Some(1),
            milestone: Some("v1".to_string()),
            ..new_issue("child a")
        },
    )
    .unwrap();
    create(
        repo,
        NewIssue {
            parent: Some(1),
            milestone: Some("v2".to_string()),
            ..new_issue("child b")
        },
    )
    .unwrap();

    let by_parent = list(
        repo,
        &IssueFilter {
            parent: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_parent.len(), 2);

    let by_milestone = list(
        repo,
        &IssueFilter {
            milestone: Some("v1".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_milestone.len(), 1);
    assert_eq!(by_milestone[0].summary.number, 2);
}

#[test]
fn list_annotates_readiness_from_dependencies() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    // #1 todo, #2 depends on #1, #3 depends on #1.
    create(repo, new_issue("base")).unwrap();
    create(
        repo,
        NewIssue {
            dependson: vec![1],
            ..new_issue("blocked")
        },
    )
    .unwrap();

    let listed = list(repo, &IssueFilter::default()).unwrap();
    let blocked = listed.iter().find(|l| l.summary.number == 2).unwrap();
    // #1 is not done yet, so #2 is blocked.
    assert_eq!(blocked.unmet_deps, vec![1]);
    assert!(!blocked.is_ready());
    // #1 has no deps, so it is ready.
    let base = listed.iter().find(|l| l.summary.number == 1).unwrap();
    assert!(base.is_ready());

    // Mark #1 done: #2 becomes ready.
    update(
        repo,
        1,
        IssueChanges {
            status: Some(IssueStatus::Done),
            ..Default::default()
        },
    )
    .unwrap();
    let listed = list(repo, &IssueFilter::default()).unwrap();
    let blocked = listed.iter().find(|l| l.summary.number == 2).unwrap();
    assert!(blocked.unmet_deps.is_empty());
    assert!(blocked.is_ready());
    // A done issue is never "ready".
    let base = listed.iter().find(|l| l.summary.number == 1).unwrap();
    assert!(!base.is_ready());
}

#[test]
fn nonexistent_dependency_counts_as_unmet() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            dependson: vec![999],
            ..new_issue("orphan dep")
        },
    )
    .unwrap();

    let listed = list(repo, &IssueFilter::default()).unwrap();
    assert_eq!(listed[0].unmet_deps, vec![999]);
    assert!(!listed[0].is_ready());
}

#[test]
fn list_filters_by_status_priority_label_and_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            priority: IssuePriority::High,
            labels: vec!["cli".to_string()],
            ..new_issue("a")
        },
    )
    .unwrap();
    create(
        repo,
        NewIssue {
            dependson: vec![1],
            ..new_issue("b")
        },
    )
    .unwrap();

    // Priority filter.
    let high = list(
        repo,
        &IssueFilter {
            priority: Some(IssuePriority::High),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(high.len(), 1);
    assert_eq!(high[0].summary.number, 1);

    // Label filter.
    let cli = list(
        repo,
        &IssueFilter {
            label: Some("cli".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cli.len(), 1);

    // Status filter.
    let todos = list(
        repo,
        &IssueFilter {
            status: Some(IssueStatus::Todo),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(todos.len(), 2);

    // Ready-only: #2 is blocked by #1, so only #1 is ready.
    let ready = list(
        repo,
        &IssueFilter {
            ready_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].summary.number, 1);
}

#[test]
fn search_matches_title_and_body_case_insensitively() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            body: "Investigate the LOGIN flow".to_string(),
            ..new_issue("Auth bug")
        },
    )
    .unwrap();
    create(repo, new_issue("Unrelated")).unwrap();

    // Matches body text regardless of case.
    let by_body = search(repo, "login", &IssueFilter::default()).unwrap();
    assert_eq!(by_body.len(), 1);
    assert_eq!(by_body[0].summary.number, 1);

    // Matches title.
    let by_title = search(repo, "auth", &IssueFilter::default()).unwrap();
    assert_eq!(by_title.len(), 1);

    // No match.
    assert!(search(repo, "zzzzz", &IssueFilter::default())
        .unwrap()
        .is_empty());

    // An empty query matches every issue.
    let all = search(repo, "", &IssueFilter::default()).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn search_handles_non_ascii_text() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            body: "ログイン画面のバグ".to_string(),
            ..new_issue("認証エラー")
        },
    )
    .unwrap();
    create(repo, new_issue("無関係")).unwrap();

    // A multi-byte (Japanese) query matches both title and body.
    assert_eq!(
        search(repo, "認証", &IssueFilter::default()).unwrap().len(),
        1
    );
    assert_eq!(
        search(repo, "ログイン", &IssueFilter::default())
            .unwrap()
            .len(),
        1
    );
    // A multi-byte needle does not match across character boundaries, so an
    // unrelated query finds nothing rather than a false positive.
    assert!(search(repo, "認画", &IssueFilter::default())
        .unwrap()
        .is_empty());

    // Case folding works for non-ASCII letters (Greek), unlike ASCII-only
    // folding which left the upper/lower forms unmatched.
    create(
        repo,
        NewIssue {
            body: "ΔΕΛΤΑ".to_string(),
            ..new_issue("three")
        },
    )
    .unwrap();
    assert_eq!(
        search(repo, "δελτα", &IssueFilter::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn search_respects_filters_and_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    create(
        repo,
        NewIssue {
            body: "shared keyword".to_string(),
            priority: IssuePriority::High,
            ..new_issue("one")
        },
    )
    .unwrap();
    create(
        repo,
        NewIssue {
            body: "shared keyword".to_string(),
            dependson: vec![1],
            ..new_issue("two")
        },
    )
    .unwrap();

    let high = search(
        repo,
        "shared",
        &IssueFilter {
            priority: Some(IssuePriority::High),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(high.len(), 1);
    assert_eq!(high[0].summary.number, 1);

    let ready = search(
        repo,
        "shared",
        &IssueFilter {
            ready_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].summary.number, 1);
}

#[test]
fn concurrent_creates_keep_distinct_numbers_with_no_lost_write() {
    // Two threads create issues against the same store at the same time. Before
    // the cross-process lock, both could read the same `max_number`, pick the
    // same number, and the second writer's stale-sibling cleanup would delete
    // the first writer's freshly created file — a lost write. With the lock the
    // allocate→write sequence is serialised, so the two creates must land on
    // DISTINCT numbers and BOTH files survive.
    use std::sync::{Arc, Barrier};
    use std::thread;

    // Many rounds so a missing lock would almost certainly trip at least once.
    for _ in 0..16 {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        let start = Arc::new(Barrier::new(2));

        let handles: Vec<_> = ["A", "B"]
            .into_iter()
            .map(|title| {
                let repo = repo.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    create(&repo, new_issue(title)).unwrap()
                })
            })
            .collect();

        let mut numbers: Vec<u32> = handles
            .into_iter()
            .map(|h| h.join().unwrap().number)
            .collect();
        numbers.sort_unstable();

        // Distinct numbers were handed out (no reuse)...
        assert_eq!(numbers, vec![1, 2], "creates must get distinct numbers");
        // ...and both issues are backed by a file on disk (no lost write).
        let store = IssueStore::new(&repo);
        assert_eq!(store.scan().unwrap().len(), 2, "both issues must survive");
        assert!(store.read(1).unwrap().is_some());
        assert!(store.read(2).unwrap().is_some());
    }
}
