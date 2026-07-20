//! Shipping `usagi mcp` -> stdio -> autostarted daemon regression tests.

#![cfg(unix)]

mod support;

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use support::mcp::McpHarness;

#[test]
fn production_tools_list_fixes_the_47_tool_schema_contract() {
    let mut mcp = McpHarness::start();
    let tools = mcp.tools();
    assert_eq!(tools.len(), 47);
    let mut names = std::collections::HashSet::new();
    for tool in &tools {
        assert!(names.insert(tool["name"].as_str().unwrap()));
        assert!(!tool["description"].as_str().unwrap().is_empty());
        assert_eq!(tool["inputSchema"]["type"], "object");
        tool["inputSchema"]["properties"]
            .as_object()
            .expect("every production tool publishes object properties");
    }
}

#[test]
fn production_session_create_reaches_daemon_and_durable_lifecycle() {
    let mut mcp = McpHarness::start();
    let response = mcp.tool("session_create", &json!({"name":"mcp-e2e-session"}));
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("accepted operation")
    );
    assert!(
        mcp.workspace()
            .join(".usagi/sessions/mcp-e2e-session/.git")
            .exists()
    );
    let lifecycle = fs::read_to_string(mcp.data_dir().join("daemon/sessions.json")).unwrap();
    assert!(lifecycle.contains("mcp-e2e-session"));
}

#[test]
fn production_session_prompt_is_durable_and_status_observes_the_session() {
    let mut mcp = McpHarness::start();
    assert!(mcp.tool("session_create", &json!({"name":"prompt-target"}))["error"].is_null());
    let response = mcp.tool(
        "session_prompt",
        &json!({"name":"prompt-target","prompt":"continue the task","mode":"queue"}),
    );
    assert!(response.get("error").is_none(), "{response}");
    let delivered = tool_text(&response);
    assert_eq!(delivered["delivered_to"], "queue");
    assert_eq!(delivered["queued"], true);

    let dispatch = fs::read_to_string(mcp.data_dir().join("daemon/dispatch.json")).unwrap();
    assert!(dispatch.contains("continue the task"));
    let status = tool_text(&mcp.tool("session_status", &json!({})));
    let target = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "prompt-target")
        .unwrap();
    assert_eq!(target["agent_phase"], "none");
    assert_eq!(target["worktrees"][0]["dirty"], false);
    assert!(target["worktrees"][0]["merged"].is_boolean());
}

#[test]
fn production_delegate_brief_creates_a_session_and_queues_the_wrapped_prompt() {
    let mut mcp = McpHarness::start();
    let response = mcp.tool(
        "session_delegate_brief",
        &json!({"name":"brief-target","brief":"investigate flaky startup"}),
    );
    assert!(response.get("error").is_none(), "{response}");
    let delegated = tool_text(&response);
    assert_eq!(delegated["name"], "brief-target");
    assert_eq!(delegated["delivered_to"], "queue");
    assert!(
        mcp.workspace()
            .join(".usagi/sessions/brief-target/.git")
            .exists()
    );
    let dispatch = fs::read_to_string(mcp.data_dir().join("daemon/dispatch.json")).unwrap();
    assert!(dispatch.contains("investigate flaky startup"));
}

#[test]
fn production_issue_writes_are_refused_at_the_workspace_root() {
    let mut mcp = McpHarness::start();
    let read = mcp.tool("issue_get", &json!({"number":1}));
    assert_eq!(read["result"]["content"][0]["text"], "null");
    let write = mcp.tool("issue_create", &json!({"title":"refused"}));
    assert_eq!(write["error"]["code"], -32603);
    assert!(
        write["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace root")
    );
    assert!(!mcp.workspace().join(".usagi/issues").exists());
}

#[test]
fn production_store_tools_round_trip_through_stdio_and_durable_files() {
    let mut mcp = McpHarness::start_in_session("store-e2e");
    let created = tool_text(&mcp.tool(
        "issue_create",
        &json!({
            "title":"MCP durable issue",
            "priority":"high",
            "labels":["mcp"],
            "body":"round trip"
        }),
    ));
    let fetched = tool_text(&mcp.tool("issue_get", &json!({"number":1})));
    let found = tool_text(&mcp.tool(
        "issue_search",
        &json!({"query":"durable","label":"mcp","ready":true}),
    ));
    let saved = tool_text(&mcp.tool(
        "memory_save",
        &json!({
            "name":"MCP Fact",
            "title":"Durable fact",
            "type":"project",
            "body":"remember me"
        }),
    ));
    let memory = tool_text(&mcp.tool("memory_get", &json!({"name":"mcp-fact"})));

    assert_eq!(created["number"], 1);
    assert_eq!(fetched["title"], "MCP durable issue");
    assert_eq!(found[0]["ready"], true);
    assert_eq!(saved["name"], "mcp-fact");
    assert_eq!(memory["body"], "remember me");
    assert!(
        mcp.cwd()
            .join(".usagi/issues/001-mcp-durable-issue.md")
            .is_file()
    );
    assert!(mcp.cwd().join(".usagi/memory/mcp-fact.md").is_file());
}

fn tool_text(response: &serde_json::Value) -> serde_json::Value {
    assert!(response.get("error").is_none(), "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn production_agent_fixture_is_injected_without_cli_credentials() {
    let mut mcp = McpHarness::start();
    assert!(mcp.fixture_bin().join("codex").exists());
    assert!(mcp.fixture_bin().join("claude").exists());
    assert!(!mcp.fixture_log().exists());
    mcp.replace_fixture_agent(
        "codex",
        "#!/bin/sh\nprintf 'custom-worker\\n' >> \"$USAGI_MCP_FIXTURE_LOG\"\n",
    );
    assert!(
        fs::read_to_string(mcp.fixture_bin().join("codex"))
            .unwrap()
            .contains("custom-worker")
    );
    let tools = mcp.tools();
    let dispatch = tools
        .iter()
        .find(|tool| tool["name"] == "session_dispatch")
        .unwrap();
    let branches = dispatch["inputSchema"]["properties"]["agent"]["oneOf"]
        .as_array()
        .unwrap();
    assert!(branches.iter().any(|branch| {
        branch["properties"]["runtime"]["const"] == "codex"
            && branch["properties"]["model"]["enum"] == json!(["fixture-codex"])
    }));
    assert!(branches.iter().any(|branch| {
        branch["properties"]["runtime"]["const"] == "claude"
            && branch["properties"]["model"]["enum"] == json!(["fixture-claude"])
    }));
    let response = mcp.tool("agent_list", &json!({}));
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("caller provenance is unknown")
    );
    assert!(!mcp.fixture_log().exists());
}

#[test]
fn production_dispatch_worker_complete_reaches_the_caller_inbox() {
    let mut mcp = McpHarness::start();
    let caller_credential = mcp.launch_caller();
    mcp.restart_with_credential(&caller_credential);
    mcp.replace_fixture_agent(
        "codex",
        r#"#!/bin/sh
if [ "$1" = login ] && [ "$2" = status ]; then exit 0; fi
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"fixture-worker","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agent_complete","arguments":{"summary":"fixture completed","result":{"commits":["abc123"],"changed_files":["fixture.rs"],"verification":"fixture green"}}}}' \
  | "$USAGI_E2E_USAGI" mcp >> "$USAGI_MCP_FIXTURE_LOG"
"#,
    );

    let dispatched = mcp.tool(
        "session_dispatch",
        &json!({
            "session":{"name":"mcp-worker"},
            "agent":{"runtime":"codex","model":"fixture-codex"},
            "prompt":"complete through MCP"
        }),
    );
    assert!(dispatched.get("error").is_none(), "{dispatched}");
    let admission: serde_json::Value =
        serde_json::from_str(dispatched["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(admission["run_id"].is_string());
    assert!(admission["terminal"].is_object());

    let deadline = Instant::now() + Duration::from_secs(10);
    let message = loop {
        let inbox = mcp.tool("agent_inbox", &json!({"unread_only":true}));
        assert!(inbox.get("error").is_none(), "{inbox}");
        let body: serde_json::Value =
            serde_json::from_str(inbox["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if let Some(message) = body["messages"].as_array().and_then(|items| items.first()) {
            break message.clone();
        }
        assert!(
            Instant::now() < deadline,
            "completion did not reach caller inbox"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(message["run_id"], admission["run_id"]);
    assert_eq!(message["kind"], "completed");
    assert_eq!(message["summary"], "fixture completed");
    assert_eq!(message["result"]["commits"], json!(["abc123"]));

    for (tool, arguments) in [
        ("session_get", json!({"name":"mcp-worker"})),
        ("agent_list", json!({"session":"mcp-worker"})),
        ("agent_get", json!({"agent_id":admission["agent_id"]})),
    ] {
        let observed = mcp.tool(tool, &arguments);
        assert!(observed.get("error").is_none(), "{tool}: {observed}");
    }

    mcp.replace_fixture_agent(
        "codex",
        r#"#!/bin/sh
if [ "$1" = login ] && [ "$2" = status ]; then exit 0; fi
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"fixture-worker","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agent_fail","arguments":{"summary":"fixture failed","error":"expected fixture error"}}}' \
  | "$USAGI_E2E_USAGI" mcp >> "$USAGI_MCP_FIXTURE_LOG"
"#,
    );
    let failed = mcp.tool(
        "session_dispatch",
        &json!({
            "session":{"name":"mcp-failing-worker"},
            "agent":{"runtime":"codex","model":"fixture-codex"},
            "prompt":"fail through MCP"
        }),
    );
    assert!(failed.get("error").is_none(), "{failed}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inbox = mcp.tool("agent_inbox", &json!({}));
        let body: serde_json::Value =
            serde_json::from_str(inbox["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["kind"] == "failed"
                    && message["summary"] == "fixture failed: expected fixture error"
            })
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failure did not reach caller inbox"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(mcp.data_dir().join("daemon/dispatch.json").exists());
}
