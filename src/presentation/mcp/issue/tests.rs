use super::*;
use crate::presentation::mcp::PROTOCOL_VERSION;

/// Parse a handler reply back into JSON for assertions.
fn reply(server: &McpServer, request: Value) -> Value {
    let line = serde_json::to_string(&request).unwrap();
    let response = server.handle_line(&line).expect("expected a reply");
    serde_json::from_str(&response).unwrap()
}

/// Call a tool and return the parsed tool-result object.
fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
    reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )["result"]
            .clone()
}

/// The text payload of a tool result, parsed back into JSON.
fn tool_json(result: &Value) -> Value {
    let text = result["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn initialize_advertises_tools_capability() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    );
    assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert!(res["result"]["capabilities"]["tools"].is_object());
    assert_eq!(res["result"]["serverInfo"]["name"], "usagi-issue");
}

#[test]
fn ping_returns_empty_result() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(&server, json!({"jsonrpc":"2.0","id":7,"method":"ping"}));
    assert_eq!(res["id"], 7);
    assert_eq!(res["result"], json!({}));
}

#[test]
fn tools_list_returns_issue_tools() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(
        &server,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let tools = res["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"issue_create"));
    assert!(names.contains(&"issue_to_prompt"));
    assert!(names.contains(&"issue_search"));
    assert!(names.contains(&"issue_delete"));
    // The separate list tool was folded into search (query optional).
    assert!(!names.contains(&"issue_list"));
}

#[test]
fn issue_to_prompt_renders_prompt_and_errors_when_missing() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    call(
        &server,
        "issue_create",
        json!({ "title": "Add doctor", "body": "Diagnose the env." }),
    );

    let result = call(&server, "issue_to_prompt", json!({ "number": 1 }));
    assert_eq!(result["isError"], false);
    let payload = tool_json(&result);
    assert_eq!(payload["number"], 1);
    assert_eq!(payload["title"], "Add doctor");
    let prompt = payload["prompt"].as_str().unwrap();
    assert!(prompt.contains("issue #1"));
    assert!(prompt.contains("Diagnose the env."));

    let missing = call(&server, "issue_to_prompt", json!({ "number": 99 }));
    assert_eq!(missing["isError"], true);
}

#[test]
fn notifications_get_no_reply() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let line = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
    assert!(server.handle_line(&line).is_none());
}

#[test]
fn parse_error_is_reported() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res: Value = serde_json::from_str(&server.handle_line("{ not json").unwrap()).unwrap();
    assert_eq!(res["error"]["code"], -32700);
    assert_eq!(res["id"], Value::Null);
}

#[test]
fn missing_method_is_an_invalid_request() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(&server, json!({"jsonrpc":"2.0","id":1,"foo":"bar"}));
    assert_eq!(res["error"]["code"], -32600);
}

#[test]
fn unknown_method_is_not_found() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"frobnicate"}),
    );
    assert_eq!(res["error"]["code"], -32601);
}

#[test]
fn tool_call_without_a_name_is_invalid_params() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let res = reply(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}),
    );
    assert_eq!(res["error"]["code"], -32602);
}

#[test]
fn unknown_tool_is_reported_as_tool_error() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    let result = call(&server, "issue_nonexistent", json!({}));
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));
}

#[test]
fn create_get_list_update_delete_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let server = McpServer::new(tmp.path());

    // create #1 (base) and #2 (depends on #1)
    let created = call(
        &server,
        "issue_create",
        json!({"title":"base","priority":"high","labels":["cli"]}),
    );
    assert_eq!(created["isError"], false);
    assert_eq!(tool_json(&created)["number"], 1);

    call(
        &server,
        "issue_create",
        json!({"title":"blocked","dependson":[1]}),
    );

    // get #1
    let got = call(&server, "issue_get", json!({"number":1}));
    assert_eq!(tool_json(&got)["title"], "base");
    // get missing -> null
    let missing = call(&server, "issue_get", json!({"number":99}));
    assert_eq!(tool_json(&missing), Value::Null);

    // search with no query lists all: #1 ready, #2 blocked by #1
    let listed = tool_json(&call(&server, "issue_search", json!({})));
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["ready"], true);
    assert_eq!(arr[1]["ready"], false);
    assert_eq!(arr[1]["unmet_deps"], json!([1]));

    // ready-only filter (no query) keeps just #1
    let ready = tool_json(&call(&server, "issue_search", json!({"ready":true})));
    assert_eq!(ready.as_array().unwrap().len(), 1);

    // search by title
    let found = tool_json(&call(&server, "issue_search", json!({"query":"blocked"})));
    assert_eq!(found.as_array().unwrap().len(), 1);

    // update #1 -> done, then #2 becomes ready
    let updated = call(&server, "issue_update", json!({"number":1,"status":"done"}));
    assert_eq!(tool_json(&updated)["status"], "done");
    let listed = tool_json(&call(&server, "issue_search", json!({})));
    assert_eq!(listed.as_array().unwrap()[1]["ready"], true);

    // update missing -> tool error
    let bad = call(
        &server,
        "issue_update",
        json!({"number":99,"status":"done"}),
    );
    assert_eq!(bad["isError"], true);

    // delete #1
    let deleted = call(&server, "issue_delete", json!({"number":1}));
    assert_eq!(tool_json(&deleted), json!({"number":1,"deleted":true}));
    let again = call(&server, "issue_delete", json!({"number":1}));
    assert_eq!(tool_json(&again)["deleted"], false);
}

#[test]
fn create_stores_relations_and_update_clears_them_with_null() {
    let tmp = tempfile::tempdir().unwrap();
    let server = McpServer::new(tmp.path());

    let created = tool_json(&call(
        &server,
        "issue_create",
        json!({"title":"child","related":[3],"parent":2,"milestone":"v1"}),
    ));
    assert_eq!(created["related"], json!([3]));
    assert_eq!(created["parent"], 2);
    assert_eq!(created["milestone"], "v1");

    // An explicit null clears parent/milestone; an absent field is untouched.
    let cleared = tool_json(&call(
        &server,
        "issue_update",
        json!({"number":1,"parent":null,"milestone":null}),
    ));
    assert_eq!(cleared["parent"], Value::Null);
    assert_eq!(cleared["milestone"], Value::Null);
    // related was not mentioned, so it is left as-is.
    assert_eq!(cleared["related"], json!([3]));

    // Setting a value replaces it.
    let set = tool_json(&call(
        &server,
        "issue_update",
        json!({"number":1,"parent":5,"related":[9]}),
    ));
    assert_eq!(set["parent"], 5);
    assert_eq!(set["related"], json!([9]));
}

#[test]
fn list_and_search_filter_by_parent_and_milestone() {
    let tmp = tempfile::tempdir().unwrap();
    let server = McpServer::new(tmp.path());
    call(&server, "issue_create", json!({"title":"epic"}));
    call(
        &server,
        "issue_create",
        json!({"title":"child a","parent":1,"milestone":"v1","body":"shared"}),
    );
    call(
        &server,
        "issue_create",
        json!({"title":"child b","parent":1,"milestone":"v2","body":"shared"}),
    );

    let by_parent = tool_json(&call(&server, "issue_search", json!({"parent":1})));
    assert_eq!(by_parent.as_array().unwrap().len(), 2);

    let by_milestone = tool_json(&call(&server, "issue_search", json!({"milestone":"v1"})));
    let arr = by_milestone.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["number"], 2);

    let searched = tool_json(&call(
        &server,
        "issue_search",
        json!({"query":"shared","milestone":"v2"}),
    ));
    let arr = searched.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["number"], 3);
}

#[test]
fn tool_call_without_arguments_defaults_to_empty() {
    let server = McpServer::new(tempfile::tempdir().unwrap().path());
    // No `arguments` field: issue_search takes none required, so it lists all.
    let res = reply(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"issue_search"}}),
    );
    assert_eq!(res["result"]["isError"], false);
    assert_eq!(tool_json(&res["result"]), json!([]));
}

#[test]
fn invalid_arguments_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let server = McpServer::new(tmp.path());
    // issue_create requires a title.
    let result = call(&server, "issue_create", json!({"priority":"high"}));
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("invalid arguments"));
}

#[test]
fn usecase_errors_surface_for_every_tool() {
    // A file where the `.usagi` directory should be makes the store fail,
    // exercising each tool's error path.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
    let server = McpServer::new(tmp.path());

    for (name, args) in [
        ("issue_create", json!({"title":"x"})),
        ("issue_get", json!({"number":1})),
        ("issue_to_prompt", json!({"number":1})),
        ("issue_search", json!({})),
        ("issue_search", json!({"query":"x"})),
        ("issue_update", json!({"number":1,"status":"done"})),
        ("issue_delete", json!({"number":1})),
    ] {
        let result = call(&server, name, args);
        assert_eq!(result["isError"], true, "{name} should error");
    }
}
