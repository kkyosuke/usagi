use super::*;
use serde_json::json;

/// Parse a handler reply back into JSON for assertions.
fn reply(server: &MemoryMcpServer, request: Value) -> Value {
    let line = serde_json::to_string(&request).unwrap();
    let response = server.handle_line(&line).expect("expected a reply");
    serde_json::from_str(&response).unwrap()
}

fn save(repo: &Path, name: &str, title: &str) -> String {
    call_tool(
        repo,
        "memory_save",
        json!({ "name": name, "title": title, "type": "project", "body": "b" }),
    )
    .unwrap()
}

#[test]
fn server_advertises_memory_identity_and_routes_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let server = MemoryMcpServer::new(tmp.path());

    let init = reply(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "usagi-memory");

    let tools = reply(
        &server,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 4);

    let saved = reply(
        &server,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_save","arguments":{"name":"fact","title":"Fact"}}}),
    );
    assert_eq!(saved["result"]["isError"], false);
}

#[test]
fn tool_names_and_schemas_cover_the_four_tools() {
    assert_eq!(tool_names().len(), 4);
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
fn save_upserts_patching_only_provided_fields_on_an_existing_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    // Create with a body and the default type (project via the `save` helper).
    call_tool(
        repo,
        "memory_save",
        json!({ "name": "fact", "title": "Old", "type": "project", "body": "keep me" }),
    )
    .unwrap();

    // Saving again with the same name patches only the fields passed: the title
    // and type change, while the untouched body is preserved (not reset).
    let updated = call_tool(
        repo,
        "memory_save",
        json!({ "name": "fact", "title": "New", "type": "feedback" }),
    )
    .unwrap();
    assert!(updated.contains("\"title\": \"New\""));
    assert!(updated.contains("\"type\": \"feedback\""));
    assert!(
        updated.contains("keep me"),
        "body should be preserved: {updated}"
    );
}

#[test]
fn save_requires_a_title_only_when_creating() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    // Creating a brand-new memory without a title is rejected.
    let err = call_tool(repo, "memory_save", json!({ "name": "fresh" })).unwrap_err();
    assert!(err.contains("`title` is required"), "{err}");

    // Once it exists, a title-less save is a valid no-field patch.
    call_tool(
        repo,
        "memory_save",
        json!({ "name": "fresh", "title": "T" }),
    )
    .unwrap();
    let touched = call_tool(repo, "memory_save", json!({ "name": "fresh" })).unwrap();
    assert!(touched.contains("\"title\": \"T\""));
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
        ("memory_delete", json!({ "name": "n" })),
    ] {
        assert!(
            call_tool(repo, name, args).is_err(),
            "{name} should surface the store error"
        );
    }
}
