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
