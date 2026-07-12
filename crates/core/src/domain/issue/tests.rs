use super::*;
use crate::domain::frontmatter::FrontmatterDoc;
use chrono::TimeZone;

fn sample() -> Issue {
    let ts = Utc.with_ymd_and_hms(2026, 6, 14, 1, 2, 3).unwrap();
    Issue {
        number: 7,
        title: "Add doctor command".to_string(),
        status: IssueStatus::InProgress,
        priority: IssuePriority::High,
        labels: vec!["cli".to_string(), "infra".to_string()],
        dependson: vec![1, 2],
        related: vec![3],
        parent: Some(4),
        milestone: Some("v1".to_string()),
        created_at: ts,
        updated_at: ts,
        body: "## Summary\n\nDo the thing.".to_string(),
    }
}

#[test]
fn status_round_trips_through_string() {
    for s in [
        IssueStatus::Todo,
        IssueStatus::InProgress,
        IssueStatus::Done,
    ] {
        assert_eq!(s.as_str().parse::<IssueStatus>().unwrap(), s);
        assert_eq!(s.to_string(), s.as_str());
    }
    assert!("nope".parse::<IssueStatus>().is_err());
}

#[test]
fn priority_round_trips_through_string() {
    for p in [
        IssuePriority::High,
        IssuePriority::Medium,
        IssuePriority::Low,
    ] {
        assert_eq!(p.as_str().parse::<IssuePriority>().unwrap(), p);
        assert_eq!(p.to_string(), p.as_str());
    }
    assert!("nope".parse::<IssuePriority>().is_err());
}

#[test]
fn enum_tokens_match_their_serde_representation() {
    // `as_str` and the serde `rename_all` derive each spell the on-disk token
    // independently; this locks them together so adding a variant that updates
    // one but not the other fails here instead of silently writing a file the
    // other half cannot read.
    for s in [
        IssueStatus::Todo,
        IssueStatus::InProgress,
        IssueStatus::Done,
    ] {
        assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
    }
    for p in [
        IssuePriority::High,
        IssuePriority::Medium,
        IssuePriority::Low,
    ] {
        assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
    }
}

#[test]
fn defaults_are_todo_and_medium() {
    assert_eq!(IssueStatus::default(), IssueStatus::Todo);
    assert_eq!(IssuePriority::default(), IssuePriority::Medium);
}

#[test]
fn slug_collapses_punctuation_and_lowercases() {
    let mut issue = sample();
    issue.title = "Fix:  the AWS-SSO   login!".to_string();
    assert_eq!(issue.slug(), "fix-the-aws-sso-login");
}

#[test]
fn slug_falls_back_when_title_has_no_alphanumerics() {
    let mut issue = sample();
    issue.title = "!!! ???".to_string();
    assert_eq!(issue.slug(), "issue");
}

#[test]
fn file_name_zero_pads_the_number() {
    let issue = sample();
    assert_eq!(issue.file_name(), "007-add-doctor-command.md");
}

#[test]
fn summary_mirrors_the_issue_without_body() {
    let issue = sample();
    let summary = issue.summary();
    assert_eq!(summary.number, 7);
    assert_eq!(summary.title, "Add doctor command");
    assert_eq!(summary.status, IssueStatus::InProgress);
    assert_eq!(summary.priority, IssuePriority::High);
    assert_eq!(summary.labels, vec!["cli", "infra"]);
    assert_eq!(summary.dependson, vec![1, 2]);
    assert_eq!(summary.related, vec![3]);
    assert_eq!(summary.parent, Some(4));
    assert_eq!(summary.milestone, Some("v1".to_string()));
    assert_eq!(summary.file, "007-add-doctor-command.md");
}

#[test]
fn markdown_round_trips() {
    let issue = sample();
    let text = issue.to_markdown();
    let parsed = Issue::from_markdown(&text).unwrap();
    assert_eq!(parsed, issue);
}

#[test]
fn markdown_renders_expected_shape() {
    let issue = sample();
    let text = issue.to_markdown();
    assert!(text.starts_with("---\nnumber: 7\ntitle: Add doctor command\n"));
    assert!(text.contains("status: in-progress\n"));
    assert!(text.contains("labels: [cli, infra]\n"));
    assert!(text.contains("dependson: [1, 2]\n"));
    assert!(text.contains("related: [3]\n"));
    assert!(text.contains("parent: 4\n"));
    assert!(text.contains("milestone: v1\n"));
    assert!(text.ends_with("## Summary\n\nDo the thing.\n"));
}

#[test]
fn empty_labels_and_deps_round_trip() {
    let mut issue = sample();
    issue.labels.clear();
    issue.dependson.clear();
    issue.related.clear();
    let text = issue.to_markdown();
    assert!(text.contains("labels: []\n"));
    assert!(text.contains("dependson: []\n"));
    assert!(text.contains("related: []\n"));
    assert_eq!(Issue::from_markdown(&text).unwrap(), issue);
}

#[test]
fn absent_parent_and_milestone_are_omitted_and_round_trip() {
    let mut issue = sample();
    issue.parent = None;
    issue.milestone = None;
    let text = issue.to_markdown();
    assert!(!text.contains("parent:"));
    assert!(!text.contains("milestone:"));
    assert_eq!(Issue::from_markdown(&text).unwrap(), issue);
}

#[test]
fn blank_parent_and_milestone_values_parse_as_none() {
    let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             parent: \nmilestone: \ncreated_at: 2026-06-14T00:00:00Z\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
    let issue = Issue::from_markdown(text).unwrap();
    assert_eq!(issue.parent, None);
    assert_eq!(issue.milestone, None);
}

#[test]
fn parse_rejects_a_non_numeric_parent() {
    let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             parent: nope\ncreated_at: 2026-06-14T00:00:00Z\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("invalid parent")
    );
}

#[test]
fn parse_tolerates_blank_lines_unknown_keys_and_crlf() {
    let text = "---\r\n\
            number: 12\r\n\
            \r\n\
            title: Weird: but valid\r\n\
            status: done\r\n\
            priority: low\r\n\
            labels: [a]\r\n\
            dependson: []\r\n\
            future_field: ignored\r\n\
            created_at: 2026-06-14T00:00:00Z\r\n\
            updated_at: 2026-06-14T00:00:00Z\r\n\
            ---\r\n\
            \r\n\
            Body here.\r\n";
    let issue = Issue::from_markdown(text).unwrap();
    assert_eq!(issue.number, 12);
    // The title keeps everything after the first colon.
    assert_eq!(issue.title, "Weird: but valid");
    assert_eq!(issue.status, IssueStatus::Done);
    assert_eq!(issue.labels, vec!["a"]);
    assert!(issue.body.starts_with("Body here."));
}

#[test]
fn parse_accepts_closing_fence_without_trailing_newline() {
    let text = "---\n\
            number: 1\n\
            title: T\n\
            status: todo\n\
            priority: medium\n\
            created_at: 2026-06-14T00:00:00Z\n\
            updated_at: 2026-06-14T00:00:00Z\n\
            ---";
    let issue = Issue::from_markdown(text).unwrap();
    assert_eq!(issue.number, 1);
    assert_eq!(issue.body, "");
    // Missing labels/dependson default to empty.
    assert!(issue.labels.is_empty());
    assert!(issue.dependson.is_empty());
}

#[test]
fn parse_rejects_missing_opening_fence() {
    let err = Issue::from_markdown("number: 1\n").unwrap_err();
    assert!(err.to_string().contains("opening"));
}

#[test]
fn parse_rejects_missing_closing_fence() {
    let text = "---\nnumber: 1\ntitle: T\n";
    let err = Issue::from_markdown(text).unwrap_err();
    assert!(err.to_string().contains("closing"));
}

#[test]
fn parse_rejects_line_without_colon() {
    let text = "---\nnonsense\n---\n";
    let err = Issue::from_markdown(text).unwrap_err();
    assert!(err.to_string().contains("invalid frontmatter line"));
}

#[test]
fn parse_rejects_bad_scalar_values() {
    // Non-numeric issue number.
    let bad_number = "---\nnumber: zzz\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(bad_number)
            .unwrap_err()
            .to_string()
            .contains("invalid number")
    );

    // Non-numeric dependency entry.
    let bad_dep = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             dependson: [x]\ncreated_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(bad_dep)
            .unwrap_err()
            .to_string()
            .contains("invalid issue number")
    );

    // A bad `related` entry reports the same field-agnostic error (it used to be
    // mislabelled "dependson" because both fields share the same parser).
    let bad_related = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             related: [y]\ncreated_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(bad_related)
            .unwrap_err()
            .to_string()
            .contains("invalid issue number")
    );

    // Unparseable timestamp.
    let bad_date = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: not-a-date\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(bad_date)
            .unwrap_err()
            .to_string()
            .contains("invalid timestamp")
    );

    // Invalid status/priority tokens.
    let bad_status = "---\nnumber: 1\ntitle: T\nstatus: nope\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(Issue::from_markdown(bad_status).is_err());
}

#[test]
fn parse_rejects_missing_required_fields() {
    // Missing title.
    let text = "---\nnumber: 1\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("title")
    );

    // Missing number.
    let text = "---\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("number")
    );

    // Missing created_at.
    let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("created_at")
    );

    // Missing updated_at.
    let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\n---\n";
    assert!(
        Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("updated_at")
    );
}

#[test]
fn summary_serializes_to_json() {
    let summary = sample().summary();
    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("\"status\":\"in-progress\""));
    let back: IssueSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(back, summary);
}

#[test]
fn labels_with_special_characters_round_trip_losslessly() {
    let mut issue = sample();
    // Each label exercises a structural character of the `[a, b, c]` encoding:
    // a comma (delimiter), brackets, a backslash, and boundary spaces.
    issue.labels = vec![
        "a, b".to_string(),
        "[bracketed]".to_string(),
        "back\\slash".to_string(),
        "  spaced  ".to_string(),
        "plain".to_string(),
    ];
    let text = issue.to_markdown();
    let parsed = Issue::from_markdown(&text).unwrap();
    assert_eq!(parsed.labels, issue.labels);
}

#[test]
fn title_and_milestone_preserve_boundary_spaces_on_round_trip() {
    // Regression: free-text scalars used to be `.trim()`-ed on parse, so a title
    // or milestone with leading/trailing spaces silently lost them on reload —
    // unlike the list fields, which escape boundary spaces. Only the single
    // `key: ` delimiter space is stripped now.
    let mut issue = sample();
    issue.title = "  spaced title  ".to_string();
    issue.milestone = Some("  v2  ".to_string());
    let parsed = Issue::from_markdown(&issue.to_markdown()).unwrap();
    assert_eq!(parsed.title, "  spaced title  ");
    assert_eq!(parsed.milestone, Some("  v2  ".to_string()));
}

#[test]
fn label_with_a_comma_is_one_value_not_two() {
    // Regression: `a, b` used to split into `["a", "b"]` on reload.
    let mut issue = sample();
    issue.labels = vec!["a, b".to_string()];
    let parsed = Issue::from_markdown(&issue.to_markdown()).unwrap();
    assert_eq!(parsed.labels, vec!["a, b".to_string()]);
}

#[test]
fn simple_labels_render_unescaped_and_still_parse() {
    // Plain values carry no escapes, so the on-disk shape stays readable and
    // hand-written / legacy files keep parsing.
    let mut issue = sample();
    issue.labels = vec!["cli".to_string(), "infra".to_string()];
    let text = issue.to_markdown();
    assert!(text.contains("labels: [cli, infra]\n"));
    assert_eq!(
        Issue::from_markdown(&text).unwrap().labels,
        vec!["cli".to_string(), "infra".to_string()]
    );
}

#[test]
fn empty_label_list_round_trips() {
    let mut issue = sample();
    issue.labels.clear();
    let text = issue.to_markdown();
    assert!(text.contains("labels: []\n"));
    assert!(Issue::from_markdown(&text).unwrap().labels.is_empty());
}

#[test]
fn parse_keeps_a_stray_backslash_and_decodes_an_escaped_comma() {
    // Hand-authored frontmatter: `c:\path` carries a backslash before a
    // non-escapable char (kept verbatim), and `a\, b` is one comma-bearing item.
    let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             labels: [c:\\path, a\\, b]\ncreated_at: 2026-06-14T00:00:00Z\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
    let issue = Issue::from_markdown(text).unwrap();
    assert_eq!(
        issue.labels,
        vec!["c:\\path".to_string(), "a, b".to_string()]
    );
}

#[test]
fn to_markdown_neutralises_newlines_so_values_cannot_inject_frontmatter() {
    let mut issue = sample();
    // Newline-bearing values that, written verbatim, would each forge a second
    // frontmatter line (a status / parent override and a split label).
    issue.title = "Fix\nstatus: done".to_string();
    issue.milestone = Some("v1\nparent: 9".to_string());
    issue.labels = vec!["a\nb".to_string()];

    let md = issue.to_markdown();
    assert!(md.contains("title: Fix status: done"));
    assert!(md.contains("milestone: v1 parent: 9"));

    // Reloads cleanly and the forged fields never took effect.
    let parsed = Issue::from_markdown(&md).unwrap();
    assert_eq!(parsed.status, issue.status); // not overwritten to `done`
    assert_eq!(parsed.parent, issue.parent); // not overwritten to 9
    assert_eq!(parsed.title, "Fix status: done");
    assert_eq!(parsed.milestone.as_deref(), Some("v1 parent: 9"));
    assert_eq!(parsed.labels, vec!["a b".to_string()]);
}
