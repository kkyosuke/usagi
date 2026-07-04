use super::*;
use serde_json::json;

fn save(repo: &Path, name: &str, title: &str) -> String {
    call_tool(
        repo,
        "memory_save",
        json!({ "name": name, "title": title, "type": "project", "body": "b" }),
    )
    .unwrap()
}

#[test]
fn tool_names_and_schemas_cover_the_five_tools() {
    assert_eq!(tool_names().len(), 5);
    let schemas = tool_schemas();
    let names: Vec<&str> = schemas
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    for name in tool_names() {
        assert!(names.contains(name), "{name} missing from schemas");
    }
}

#[test]
fn save_get_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let saved = save(repo, "My Fact", "A title");
    assert!(saved.contains("\"name\": \"my-fact\""));

    let got = call_tool(repo, "memory_get", json!({ "name": "my-fact" })).unwrap();
    assert!(got.contains("\"title\": \"A title\""));
}

#[test]
fn get_returns_null_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let got = call_tool(tmp.path(), "memory_get", json!({ "name": "nope" })).unwrap();
    assert_eq!(got, "null");
}

#[test]
fn search_lists_all_without_a_query_and_filters_with_one() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "deploy", "Deploy steps");

    // Omitting `query` lists every memory (optionally filtered by type) — the
    // behaviour a separate `memory_list` tool used to provide.
    let listed = call_tool(repo, "memory_search", json!({ "type": "project" })).unwrap();
    assert!(listed.contains("\"file\": \"deploy.md\""));

    // A `query` narrows to a full-text match.
    let found = call_tool(repo, "memory_search", json!({ "query": "deploy" })).unwrap();
    assert!(found.contains("\"name\": \"deploy\""));

    // A query that matches nothing yields an empty list.
    let none = call_tool(repo, "memory_search", json!({ "query": "zzz" })).unwrap();
    assert_eq!(none, "[]");
}

#[test]
fn update_changes_fields_and_reports_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "fact", "Old");

    let updated = call_tool(
        repo,
        "memory_update",
        json!({ "name": "fact", "title": "New", "type": "feedback" }),
    )
    .unwrap();
    assert!(updated.contains("\"title\": \"New\""));
    assert!(updated.contains("\"type\": \"feedback\""));

    let err = call_tool(repo, "memory_update", json!({ "name": "nope" })).unwrap_err();
    assert!(err.contains("no memory 'nope'"));
}

#[test]
fn delete_reports_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "fact", "Title");

    let deleted = call_tool(repo, "memory_delete", json!({ "name": "fact" })).unwrap();
    assert!(deleted.contains("\"deleted\": true"));

    let again = call_tool(repo, "memory_delete", json!({ "name": "fact" })).unwrap();
    assert!(again.contains("\"deleted\": false"));
}

#[test]
fn invalid_arguments_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    // memory_get requires a string `name`.
    let err = call_tool(tmp.path(), "memory_get", json!({ "name": 5 })).unwrap_err();
    assert!(err.contains("invalid arguments"));
}

#[test]
fn unknown_tool_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let err = call_tool(tmp.path(), "memory_bogus", json!({})).unwrap_err();
    assert!(err.contains("unknown tool"));
}

#[test]
fn store_errors_surface_as_tool_errors() {
    let tmp = tempfile::tempdir().unwrap();
    // A file where the memory directory should be makes every store operation
    // fail, exercising each tool's error path.
    std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
    std::fs::write(tmp.path().join(".usagi/memory"), "x").unwrap();
    let repo = tmp.path();

    for (name, args) in [
        ("memory_save", json!({ "name": "n", "title": "t" })),
        ("memory_get", json!({ "name": "n" })),
        ("memory_search", json!({})),
        ("memory_search", json!({ "query": "q" })),
        ("memory_update", json!({ "name": "n", "title": "t" })),
        ("memory_delete", json!({ "name": "n" })),
    ] {
        assert!(
            call_tool(repo, name, args).is_err(),
            "{name} should surface the store error"
        );
    }
}
