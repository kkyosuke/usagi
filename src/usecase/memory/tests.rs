use super::*;
use std::path::Path;

fn new(name: &str, title: &str, kind: MemoryType) -> NewMemory {
    NewMemory {
        name: name.to_string(),
        title: title.to_string(),
        kind,
        related: vec![],
        body: format!("Body for {title}."),
    }
}

fn save_one(repo: &Path, name: &str, title: &str, kind: MemoryType) -> Memory {
    save(repo, new(name, title, kind)).unwrap()
}

#[test]
fn save_creates_then_get_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let saved = save_one(
        repo,
        "User Prefers Tabs",
        "ユーザーはタブを好む",
        MemoryType::User,
    );

    // The name is slugified on save.
    assert_eq!(saved.name, "user-prefers-tabs");
    let got = get(repo, "user-prefers-tabs").unwrap().unwrap();
    assert_eq!(got, saved);
    // Lookups slugify their argument too.
    assert!(get(repo, "User Prefers Tabs").unwrap().is_some());
    assert!(get(repo, "missing").unwrap().is_none());
}

#[test]
fn save_upserts_in_place_preserving_created_at() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let first = save_one(repo, "fact", "First", MemoryType::Project);

    let mut again = new("fact", "Second", MemoryType::Reference);
    again.body = "changed".to_string();
    let second = save(repo, again).unwrap();

    // Same name → one memory, created_at preserved, other fields replaced.
    assert_eq!(list(repo, &MemoryFilter::default()).unwrap().len(), 1);
    assert_eq!(second.created_at, first.created_at);
    assert!(second.updated_at >= first.updated_at);
    assert_eq!(second.title, "Second");
    assert_eq!(second.kind, MemoryType::Reference);
}

#[test]
fn list_filters_by_type_and_orders_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save_one(repo, "a", "Alpha", MemoryType::User);
    save_one(repo, "b", "Beta", MemoryType::Project);
    save_one(repo, "c", "Gamma", MemoryType::Project);

    let all = list(repo, &MemoryFilter::default()).unwrap();
    assert_eq!(all.len(), 3);
    // Newest first: c was saved last.
    assert_eq!(all[0].name, "c");

    let projects = list(
        repo,
        &MemoryFilter {
            kind: Some(MemoryType::Project),
        },
    )
    .unwrap();
    assert_eq!(projects.len(), 2);
    assert!(projects.iter().all(|s| s.kind == MemoryType::Project));
}

#[test]
fn search_matches_name_title_and_body_then_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(
        repo,
        NewMemory {
            name: "deploy".to_string(),
            title: "Deploy steps".to_string(),
            kind: MemoryType::Project,
            related: vec![],
            body: "Run the magic script.".to_string(),
        },
    )
    .unwrap();
    save_one(repo, "other", "Other", MemoryType::User);

    // Body match.
    assert_eq!(
        search(repo, "magic", &MemoryFilter::default())
            .unwrap()
            .len(),
        1
    );
    // Title match, case-insensitive.
    assert_eq!(
        search(repo, "DEPLOY", &MemoryFilter::default())
            .unwrap()
            .len(),
        1
    );
    // Empty query matches everything.
    assert_eq!(search(repo, "", &MemoryFilter::default()).unwrap().len(), 2);
    // Filter narrows by type.
    assert_eq!(
        search(
            repo,
            "",
            &MemoryFilter {
                kind: Some(MemoryType::User)
            }
        )
        .unwrap()
        .len(),
        1
    );
    // No match.
    assert!(search(repo, "nonexistent", &MemoryFilter::default())
        .unwrap()
        .is_empty());
}

#[test]
fn update_applies_only_given_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save_one(repo, "fact", "Title", MemoryType::Project);

    let updated = update(
        repo,
        "fact",
        MemoryChanges {
            title: Some("New title".to_string()),
            kind: Some(MemoryType::Feedback),
            related: Some(vec!["other".to_string()]),
            body: Some("New body".to_string()),
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(updated.title, "New title");
    assert_eq!(updated.kind, MemoryType::Feedback);
    assert_eq!(updated.related, vec!["other".to_string()]);
    assert_eq!(updated.body, "New body");
}

#[test]
fn update_returns_none_for_a_missing_memory() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(update(tmp.path(), "missing", MemoryChanges::default())
        .unwrap()
        .is_none());
}

#[test]
fn delete_reports_whether_it_existed() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save_one(repo, "doomed", "Doomed", MemoryType::Project);

    assert!(delete(repo, "Doomed").unwrap());
    assert!(get(repo, "doomed").unwrap().is_none());
    assert!(!delete(repo, "doomed").unwrap());
}

#[test]
fn changes_is_empty_detects_no_op() {
    assert!(MemoryChanges::default().is_empty());
    assert!(!MemoryChanges {
        title: Some("x".to_string()),
        ..Default::default()
    }
    .is_empty());
}
