//! Shipping `usagi mcp` -> stdio -> autostarted daemon regression tests.

#![cfg(unix)]

mod support;

use std::fs;

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

fn tool_text(response: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
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
fn production_store_call_is_an_explicit_error_until_store_wiring_exists() {
    let mut mcp = McpHarness::start();
    let response = mcp.tool("issue_get", &json!({"number":1}));
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not yet implemented")
    );
    assert!(!mcp.workspace().join(".usagi/issues").exists());
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
            .contains("not implemented")
    );
    assert!(!mcp.fixture_log().exists());
}
