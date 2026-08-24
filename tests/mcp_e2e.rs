//! Shipping `usagi mcp` -> stdio -> autostarted daemon regression tests.

#![cfg(unix)]

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use support::mcp::{FixtureArgv, McpHarness};
use usagi_core::domain::{
    agent::{AgentProfileId, CallerRef, ModelSelector},
    id::{AgentId, OperationId, UserDecisionId, WorkspaceId},
    role::RoleId,
    settings::DEFAULT_LOCAL_LLM_MODEL,
    user_decision::UserDecision,
};
use usagi_core::infrastructure::store::{
    DerivedState, issue::IssueStore, memory::MemoryStore, user_decision::UserDecisionStore,
};
use usagi_core::usecase::client::{
    DaemonClient, DaemonReply, DaemonRequest, DispatchAgentIntent, DispatchIntent,
    TuiUserDecisionAction,
};

#[test]
fn production_tools_list_fixes_the_49_tool_schema_contract() {
    let mut mcp = McpHarness::start();
    let tools = mcp.tools();
    assert_eq!(tools.len(), 49);
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
fn production_settings_do_not_pass_disabled_tool_families_to_mcp() {
    let mut mcp = McpHarness::start_with_tool_availability(false, false);
    let tools = mcp.tools();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(names.len(), 38);
    assert!(names.iter().all(|name| !name.starts_with("issue_")));
    assert!(names.iter().all(|name| !name.starts_with("memory_")));
    assert!(!names.contains(&"session_delegate_issue"));
    assert!(names.contains(&"session_list"));

    for name in ["issue_search", "memory_search", "session_delegate_issue"] {
        let response = mcp.tool(name, &json!({}));
        assert_eq!(response["error"]["code"], -32601);
    }
}

/// The registry and the Agent prompt must come from the same settings, or an
/// Agent is told about a family its own MCP server refused to register. Issue
/// off with Memory on makes that observable: the prompt has to lose exactly the
/// issue line and keep the rest.
#[test]
fn production_disabled_family_leaves_both_the_registry_and_the_agent_prompt() {
    let mut mcp = McpHarness::start_with_tool_availability(false, true);
    let tools = mcp.tools();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.iter().all(|name| !name.starts_with("issue_")));
    assert!(!names.contains(&"session_delegate_issue"));
    assert!(names.iter().any(|name| name.starts_with("memory_")));

    drop(mcp.launch_caller());
    let capture = wait_for_fixture_argv(&mcp, "codex", None, "<tools>");
    let prompt = shipping_system_prompt(&capture);
    assert!(
        prompt.contains("- session:"),
        "the session line is not conditional: {prompt}"
    );
    assert!(
        prompt.contains("- memory:"),
        "an enabled family lost its line: {prompt}"
    );
    assert!(
        !prompt.contains("- issue:"),
        "a disabled family stayed in the prompt: {prompt}"
    );
    // A disabled family is not described as missing either.
    assert!(!prompt.contains("issue"), "{prompt}");
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
    let lifecycle =
        fs::read_to_string(support::daemon::lifecycle_state_path(&mcp.data_dir())).unwrap();
    assert!(lifecycle.contains("mcp-e2e-session"));
}

#[test]
fn production_session_roles_default_project_and_conflict_without_instruction_leakage() {
    let mut mcp = McpHarness::start();
    fs::write(
        mcp.workspace().join(".usagi/roles.toml"),
        r#"version = 1
[defaults]
session = "coder"
[roles.coder]
summary = "Implement changes"
scopes = ["session"]
instructions = "ROLE_SECRET_CODER_INSTRUCTION"
[roles.reviewer]
summary = "Review changes"
scopes = ["session"]
instructions = "ROLE_SECRET_REVIEWER_INSTRUCTION"
"#,
    )
    .unwrap();

    let defaulted = mcp.tool("session_create", &json!({"name":"role-default"}));
    assert!(defaulted.get("error").is_none(), "{defaulted}");
    let explicit = mcp.tool(
        "session_create",
        &json!({"name":"role-review", "role":"reviewer"}),
    );
    assert!(explicit.get("error").is_none(), "{explicit}");
    let idempotent = mcp.tool(
        "session_create",
        &json!({"name":"role-review", "role":"reviewer"}),
    );
    assert!(idempotent.get("error").is_none(), "{idempotent}");
    let conflict = mcp.tool(
        "session_create",
        &json!({"name":"role-review", "role":"coder"}),
    );
    assert!(conflict.get("error").is_some(), "{conflict}");

    let list = tool_text(&mcp.tool("session_list", &json!({})));
    assert!(!list.to_string().contains("ROLE_SECRET_"));
    let defaulted = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "role-default")
        .unwrap();
    assert_eq!(defaulted["role_id"], "coder");
    assert_eq!(defaulted["role_summary"], "Implement changes");
    let review = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "role-review")
        .unwrap();
    assert_eq!(review["role_id"], "reviewer");

    let lifecycle =
        fs::read_to_string(support::daemon::lifecycle_state_path(&mcp.data_dir())).unwrap();
    assert!(lifecycle.contains("\"role_id\": \"reviewer\""));
    assert!(!lifecycle.contains("ROLE_SECRET_"));

    fs::write(mcp.workspace().join(".usagi/roles.toml"), "version = 99\n").unwrap();
    let rejected = mcp.tool("session_create", &json!({"name":"role-invalid"}));
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(
        !mcp.workspace()
            .join(".usagi/sessions/role-invalid")
            .exists()
    );
    let still_visible = tool_text(&mcp.tool("session_list", &json!({})));
    assert_eq!(still_visible["sessions"].as_array().unwrap().len(), 2);
    assert!(
        still_visible["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["role_summary"].is_null())
    );
}

/// `session_remove` returns an acceptance and the daemon's own teardown worker
/// finishes the worktree removal afterwards. The wiring is what matters here: a
/// removal that only completed while the MCP request was still open would hold
/// the caller past its deadline for a session with a large `target/`.
#[test]
fn production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down() {
    let mut mcp = McpHarness::start();
    assert!(mcp.tool("session_create", &json!({"name":"teardown-target"}))["error"].is_null());
    let session_root = mcp.workspace().join(".usagi/sessions/teardown-target");
    assert!(session_root.join(".git").exists());

    let accepted = mcp.tool("session_remove", &json!({"name":"teardown-target"}));
    assert!(accepted.get("error").is_none(), "{accepted}");
    assert!(
        accepted["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("accepted operation")
    );

    // The daemon-owned worker finishes the teardown without another request:
    // the row leaves the durable snapshot and the worktree leaves the disk.
    wait_until(|| {
        !tool_text(&mcp.tool("session_list", &json!({})))["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["name"] == "teardown-target")
    });
    assert!(!session_root.exists());
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
    let workspace_dispatch =
        fs::read_to_string(mcp.data_dir().join("daemon/dispatch-workspaces.json")).unwrap();
    assert!(!dispatch.contains("continue the task"));
    assert!(workspace_dispatch.contains("continue the task"));
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
fn production_delegate_brief_immediately_dispatches_an_isolated_triage_worker() {
    let mut mcp = McpHarness::start();
    let caller_credential = mcp.launch_caller();
    mcp.restart_with_credential(&caller_credential);
    write_session_role_catalog(
        &mcp,
        "reviewer",
        "Review delegated work",
        "DELEGATE_ROLE_SECRET",
    );
    mcp.replace_fixture_agent(
        "codex",
        r#"#!/bin/sh
if [ "$1" = login ] && [ "$2" = status ]; then exit 0; fi
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"brief-worker","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agent_complete","arguments":{"summary":"brief triaged"}}}' \
  | "$USAGI_E2E_USAGI" mcp >> "$USAGI_MCP_FIXTURE_LOG"
"#,
    );
    let response = mcp.tool(
        "session_delegate_brief",
        &json!({
            "name":"brief-target",
            "role":"reviewer",
            "brief":"investigate flaky startup",
            "agent":{"runtime":"codex","model":"fixture-codex"}
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let delegated = tool_text(&response);
    assert_eq!(delegated["name"], "brief-target");
    assert!(delegated["run_id"].is_string());
    assert!(delegated["terminal"].is_object());
    assert!(
        mcp.workspace()
            .join(".usagi/sessions/brief-target/.git")
            .exists()
    );
    let dispatch = fs::read_to_string(mcp.data_dir().join("daemon/dispatch.json")).unwrap();
    assert!(dispatch.contains("investigate flaky startup"));
    assert!(!dispatch.contains("DELEGATE_ROLE_SECRET"));
    let argv = wait_for_fixture_argv(&mcp, "codex", None, "DELEGATE_ROLE_SECRET");
    assert_shipping_role_argv(&argv, "reviewer", "DELEGATE_ROLE_SECRET", None, false);
    let sessions = tool_text(&mcp.tool("session_list", &json!({})));
    let delegated_session = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "brief-target")
        .unwrap();
    assert_eq!(delegated_session["role_id"], "reviewer");
    wait_until(|| {
        let inbox = mcp.tool("agent_inbox", &json!({}));
        inbox.get("error").is_none()
            && tool_text(&inbox)["messages"]
                .as_array()
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["run_id"] == delegated["run_id"]
                            && message["kind"] == "completed"
                            && message["summary"] == "brief triaged"
                    })
                })
    });
}

/// Runtime/model policy has one authority across MCP schema publication,
/// delegate preflight, and dispatch after a managed worktree exists. A stale
/// MCP snapshot is still revalidated before create, while an uncommitted,
/// machine-local root allowlist governs both existing and newly delegated
/// sessions whose worktrees do not contain that policy.
#[test]
fn production_dispatch_uses_the_trusted_root_before_and_after_session_creation() {
    let mut mcp = McpHarness::start();
    let caller_credential = mcp.launch_caller();
    let lifecycle_path = support::daemon::lifecycle_state_path(&mcp.data_dir());

    // The MCP snapshot still advertises `fixture-codex`, but the workspace root
    // no longer allows it. The daemon decides that without any side effect.
    mcp.restart_with_credential(&caller_credential);
    fs::write(
        mcp.workspace().join(".usagi/config.toml"),
        "[agents.codex]\nmodels = [\"another-model\"]\n[agents.claude]\nmodels = [\"fixture-claude\"]\n",
    )
    .unwrap();
    let preflight = mcp.tool(
        "session_delegate_brief",
        &json!({
            "name":"preflight-target",
            "brief":"must never reach a worktree",
            "agent":{"runtime":"codex","model":"fixture-codex"}
        }),
    );
    assert_eq!(preflight["error"]["code"], -32603);
    assert!(
        !mcp.workspace()
            .join(".usagi/sessions/preflight-target")
            .exists()
    );
    // Nothing was journaled either: this refusal is not a rollback, the create
    // never ran.
    assert!(
        !fs::read_to_string(&lifecycle_path)
            .unwrap()
            .contains("preflight-target")
    );
    assert!(!branches(mcp.workspace()).contains(&"usagi/preflight-target".to_owned()));

    // The workspace root now allows a model the committed configuration does
    // not. Both MCP schema validation and daemon dispatch must use that root
    // authority, while the managed session remains the worker's launch cwd.
    fs::write(
        mcp.workspace().join(".usagi/config.toml"),
        "[agents.codex]\nmodels = [\"fixture-codex\", \"uncommitted-model\"]\n[agents.claude]\nmodels = [\"fixture-claude\"]\n",
    )
    .unwrap();
    mcp.restart_with_credential(&caller_credential);

    let created = mcp.tool("session_create", &json!({"name":"root-policy-target"}));
    assert!(created.get("error").is_none(), "{created}");
    let dispatched = mcp.tool(
        "session_dispatch",
        &json!({
            "session":{"name":"root-policy-target"},
            "agent":{"runtime":"codex","model":"uncommitted-model"},
            "prompt":"use trusted root policy"
        }),
    );
    assert!(dispatched.get("error").is_none(), "{dispatched}");
    let admission = tool_text(&dispatched);
    assert!(admission["run_id"].is_string());
    assert!(admission["terminal"].is_object());

    let delegated = mcp.tool(
        "session_delegate_brief",
        &json!({
            "name":"root-policy-delegated",
            "brief":"use trusted root policy after create",
            "agent":{"runtime":"codex","model":"uncommitted-model"}
        }),
    );
    assert!(delegated.get("error").is_none(), "{delegated}");
    let delegated = tool_text(&delegated);
    assert_eq!(delegated["name"], "root-policy-delegated");
    assert!(delegated["run_id"].is_string());
    assert!(
        mcp.workspace()
            .join(".usagi/sessions/root-policy-delegated/.git")
            .exists()
    );
}

/// The shipping tool contract does not advertise an existing-agent selector for a
/// tool that creates the session it dispatches into (#611).
#[test]
fn production_delegate_brief_publishes_and_accepts_only_a_new_agent_selector() {
    let mut mcp = McpHarness::start();
    let tools = mcp.tools();
    let branches = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()["inputSchema"]["properties"]["agent"]["oneOf"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert!(
        branches("session_delegate_brief")
            .iter()
            .all(|branch| branch["required"] == json!(["runtime", "model"]))
    );
    assert_eq!(branches("session_dispatch")[0]["required"], json!(["id"]));

    let refused = mcp.tool(
        "session_delegate_brief",
        &json!({"brief":"triage","name":"id-selector","agent":{"id":"00000000-0000-4000-8000-000000000000"}}),
    );
    assert_eq!(refused["error"]["code"], -32602);
    assert!(!mcp.workspace().join(".usagi/sessions/id-selector").exists());
}

/// The local branches of one repository, without `refs/heads/`.
fn branches(repo: &std::path::Path) -> Vec<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn production_delegate_brief_rejects_an_unknown_caller_without_creating_a_session() {
    let mut mcp = McpHarness::start();
    let response = mcp.tool(
        "session_delegate_brief",
        &json!({
            "name":"unowned-brief",
            "brief":"must not create a worktree",
            "agent":{"runtime":"codex","model":"fixture-codex"}
        }),
    );
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        !mcp.workspace()
            .join(".usagi/sessions/unowned-brief")
            .exists()
    );
}

#[test]
fn production_supervisor_tools_observe_one_durable_aggregate() {
    let mut mcp = McpHarness::start();
    let caller_credential = mcp.launch_caller();
    mcp.restart_with_credential(&caller_credential);
    let started = mcp.tool(
        "supervisor_start",
        &json!({
            "root_task": "coordinate the production fixture",
            "initial_task_dag": [{
                "task_id": "inspect",
                "dependencies": ["root"],
                "instruction": "inspect without exposing this body",
                "required_artifact_contract": "none"
            }],
            "idempotency_key": "production-supervisor-e2e"
        }),
    );
    assert!(started.get("error").is_none(), "{started}");
    let started: serde_json::Value =
        serde_json::from_str(started["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let run_id = started["supervisor_run_id"].as_str().unwrap();
    assert_eq!(started["state"], "running");
    assert_eq!(started["tasks"].as_array().unwrap().len(), 2);
    assert!(!started.to_string().contains("inspect without exposing"));

    let fetched = mcp.tool("supervisor_get", &json!({"supervisor_run_id": run_id}));
    let fetched: serde_json::Value =
        serde_json::from_str(fetched["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(fetched["supervisor_run_id"], run_id);
    assert_eq!(fetched["state_revision"], started["state_revision"]);

    let listed = mcp.tool("supervisor_list", &json!({"limit": 10}));
    let listed: serde_json::Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(listed["runs"].as_array().unwrap().len(), 1);
    assert_eq!(listed["runs"][0]["supervisor_run_id"], run_id);

    let events = mcp.tool(
        "supervisor_events",
        &json!({"supervisor_run_id": run_id, "after_sequence": 0, "limit": 10}),
    );
    let events: serde_json::Value =
        serde_json::from_str(events["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 3);
    assert_eq!(events["next_sequence"], 4);

    let durable_dir = mcp.data_dir().join("daemon/supervisor-runs");
    assert!(fs::read_dir(durable_dir).unwrap().count() >= 2);
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
    let memory_store = MemoryStore::new(mcp.cwd());
    let memory_dirty = memory_store.dir().join(".derived-dirty");
    fs::write(&memory_dirty, "rebuild from markdown\n").unwrap();
    let repaired = memory_store.read("mcp-fact").unwrap().unwrap();
    let memory = tool_text(&mcp.tool("memory_get", &json!({"name":"mcp-fact"})));

    assert_eq!(created["number"], 1);
    assert_eq!(fetched["title"], "MCP durable issue");
    assert_eq!(found[0]["ready"], true);
    assert_eq!(saved["name"], "mcp-fact");
    assert_eq!(repaired.body, "remember me");
    assert!(!memory_dirty.exists());
    let missing = memory_store.remove_with_outcome("missing-memory").unwrap();
    assert!(!missing.value);
    assert_eq!(missing.derived, DerivedState::Fresh);
    assert_eq!(memory["body"], "remember me");
    assert!(
        mcp.cwd()
            .join(".usagi/issues/001-mcp-durable-issue.md")
            .is_file()
    );
    assert!(mcp.cwd().join(".usagi/memory/mcp-fact.md").is_file());

    fs::remove_file(memory_store.index_path()).unwrap();
    fs::create_dir(memory_store.index_path()).unwrap();
    fs::write(&memory_dirty, "rebuild from markdown\n").unwrap();
    let fallback = memory_store.read("mcp-fact").unwrap().unwrap();
    assert_eq!(fallback.body, "remember me");
    assert!(memory_dirty.is_file());
}

#[test]
fn production_issue_create_commits_source_when_derived_refresh_fails() {
    let mut mcp = McpHarness::start_in_session("derived-refresh-failure");
    let issues = mcp.cwd().join(".usagi/issues");
    fs::create_dir_all(issues.join("index.json")).unwrap();

    let created = tool_text(&mcp.tool("issue_create", &json!({"title":"Committed without index"})));

    assert_eq!(created["number"], 1);
    assert!(issues.join("001-committed-without-index.md").is_file());
    assert_eq!(
        fs::read_to_string(issues.join(".derived-dirty")).unwrap(),
        "rebuild from markdown\n"
    );
    let fetched = tool_text(&mcp.tool("issue_get", &json!({"number":1})));
    assert_eq!(fetched["title"], "Committed without index");
    assert!(issues.join("index.json").is_dir());
    assert!(issues.join(".derived-dirty").is_file());

    // The RPC returns the committed Issue, while the store outcome retains the
    // explicit derived-state contract for direct callers.
    let store = IssueStore::new(mcp.cwd());
    let persisted = store.read(1).unwrap().unwrap();
    let outcome = store.write(&persisted).unwrap();
    assert_eq!(outcome.derived, DerivedState::RebuildNeeded);

    let unreadable_claim = issues.join("002-unreadable.md");
    fs::create_dir(&unreadable_claim).unwrap();
    let dirty_before = fs::read(issues.join(".derived-dirty")).unwrap();
    let error = store.read(2).unwrap_err();
    assert!(error.to_string().contains("failed to read"));
    assert!(unreadable_claim.is_dir());
    assert_eq!(
        fs::read(issues.join(".derived-dirty")).unwrap(),
        dirty_before
    );

    fs::remove_dir(&unreadable_claim).unwrap();
    fs::remove_dir(issues.join("index.json")).unwrap();
    let repaired = store.read(1).unwrap().unwrap();
    assert_eq!(repaired.title, "Committed without index");
    assert!(issues.join("index.json").is_file());
    assert!(!issues.join(".derived-dirty").exists());
    let missing = store.remove_with_outcome(999).unwrap();
    assert!(!missing.value);
    assert_eq!(missing.derived, DerivedState::Fresh);
}

#[test]
fn production_issue_create_uses_the_v1_git_common_sequence_authority() {
    let mut mcp = McpHarness::start_in_session("v1-sequence-compat");
    let authority = mcp.workspace().join(".git/usagi/issue-numbers");
    let reservations = authority.join("reservations");
    fs::create_dir_all(&reservations).unwrap();
    fs::write(
        authority.join("sequence.json"),
        "{\n  \"version\": 1,\n  \"last_reserved\": 515\n}\n",
    )
    .unwrap();
    fs::write(reservations.join("0000000515.reserved"), "515\n").unwrap();

    let created = tool_text(&mcp.tool("issue_create", &json!({"title":"After v1"})));

    assert_eq!(created["number"], 516);
    assert!(mcp.cwd().join(".usagi/issues/516-after-v1.md").is_file());
    assert_eq!(
        fs::read_to_string(reservations.join("0000000516.reserved")).unwrap(),
        "516\n"
    );
    assert!(
        fs::read_to_string(authority.join("sequence.json"))
            .unwrap()
            .contains("\"last_reserved\": 516")
    );
}

#[test]
fn production_issue_create_from_nested_linked_worktree_uses_git_common_authority() {
    let mut mcp = McpHarness::start_in_nested_session("nested-v1-sequence-compat");
    let authority = mcp.workspace().join(".git/usagi/issue-numbers");
    fs::create_dir_all(&authority).unwrap();
    fs::write(
        authority.join("sequence.json"),
        "{\n  \"version\": 1,\n  \"last_reserved\": 515\n}\n",
    )
    .unwrap();
    let common_legacy = mcp.workspace().join(".git/usagi-issue-sequence/next");
    fs::create_dir_all(common_legacy.parent().unwrap()).unwrap();
    fs::write(&common_legacy, "migrated-to-usagi-issue-numbers:515\n").unwrap();
    fs::write(authority.join("legacy-v2-migrated"), "515\n").unwrap();

    let created = mcp.tool(
        "issue_create",
        &json!({"title":"Nested allocator authority","body":"body"}),
    );
    assert!(created.get("error").is_none(), "{created}");
    let body: serde_json::Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["number"], 516);
    assert!(authority.join("reservations/0000000516.reserved").is_file());
    assert_eq!(
        fs::read_to_string(common_legacy).unwrap(),
        "migrated-to-usagi-issue-numbers:516\n"
    );
    assert_eq!(
        fs::read_to_string(mcp.cwd().join(".usagi/issues/usagi-issue-sequence/next")).unwrap(),
        "migrated-to-usagi-issue-numbers:516\n"
    );
    assert!(!mcp.cwd().join(".usagi/issue-numbers").exists());
}

#[test]
fn production_issue_point_tools_reject_duplicate_numbers_without_changing_siblings() {
    let mut mcp = McpHarness::start_in_session("duplicate-issue-e2e");
    let created = tool_text(&mcp.tool("issue_create", &json!({"title":"First"})));
    let number = created["number"].as_u64().unwrap();
    let issues_dir = mcp.cwd().join(".usagi/issues");
    let first = issues_dir.join(format!("{number:03}-first.md"));
    let second = issues_dir.join(format!("{number:03}-second.md"));
    let first_source = fs::read(&first).unwrap();
    fs::write(&second, &first_source).unwrap();

    let listed = tool_text(&mcp.tool("issue_search", &json!({})));
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let mut listed_files: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["file"].as_str().unwrap().to_owned())
        .collect();
    listed_files.sort();
    assert_eq!(
        listed_files,
        [
            format!("{number:03}-first.md"),
            format!("{number:03}-second.md")
        ]
    );
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|issue| issue["ambiguous"] == true && issue["ready"] == false)
    );
    let matching = tool_text(&mcp.tool("issue_search", &json!({"query":"First"})));
    let mut matching_files: Vec<_> = matching
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["file"].as_str().unwrap().to_owned())
        .collect();
    matching_files.sort();
    assert_eq!(
        matching_files,
        [
            format!("{number:03}-first.md"),
            format!("{number:03}-second.md")
        ]
    );
    let source_before = [fs::read(&first).unwrap(), fs::read(&second).unwrap()];
    let index = issues_dir.join("index.json");
    let index_before = fs::read(&index).unwrap();
    let dirty = issues_dir.join(".derived-dirty");
    fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
    let dirty_before = fs::read(&dirty).unwrap();

    for response in [
        mcp.tool("issue_create", &json!({"title":"First"})),
        mcp.tool("issue_get", &json!({"number":number})),
        mcp.tool("issue_to_prompt", &json!({"number":number})),
        mcp.tool(
            "issue_update",
            &json!({"number":number,"title":"Replacement"}),
        ),
        mcp.tool("issue_delete", &json!({"number":number})),
    ] {
        assert_eq!(response["error"]["code"], -32603);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains(&format!("issue #{number} is ambiguous")));
        assert!(message.contains(first.to_str().unwrap()));
        assert!(message.contains(second.to_str().unwrap()));
    }

    assert_eq!(fs::read(&first).unwrap(), source_before[0]);
    assert_eq!(fs::read(&second).unwrap(), source_before[1]);
    assert_eq!(fs::read(index).unwrap(), index_before);
    assert_eq!(fs::read(dirty).unwrap(), dirty_before);
    assert!(
        !issues_dir
            .join(format!("{number:03}-replacement.md"))
            .exists()
    );
}

#[test]
fn production_delegate_issue_preserves_ambiguity_without_session_or_queue_side_effects() {
    let mut mcp = McpHarness::start();
    let issues_dir = mcp.workspace().join(".usagi/issues");
    fs::create_dir_all(&issues_dir).unwrap();
    let first = issues_dir.join("001-first.md");
    let second = issues_dir.join("001-second.md");
    let source = "---\nnumber: 1\ntitle: First\nstatus: todo\npriority: high\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-07-22T00:00:00+00:00\nupdated_at: 2026-07-22T00:00:00+00:00\n---\n\nbody\n";
    fs::write(&first, source).unwrap();
    fs::write(&second, source).unwrap();

    let sessions_path = support::daemon::lifecycle_state_path(&mcp.data_dir());
    let dispatch_path = mcp.data_dir().join("daemon/dispatch.json");
    let sessions_before = fs::read(&sessions_path).unwrap();
    let dispatch_before = fs::read(&dispatch_path).ok();
    let name = "ambiguous-must-not-exist";

    let response = mcp.tool("session_delegate_issue", &json!({"number":1,"name":name}));

    assert_eq!(response["error"]["code"], -32603);
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("issue #1 is ambiguous"));
    assert!(message.contains(first.to_str().unwrap()));
    assert!(message.contains(second.to_str().unwrap()));
    assert!(!mcp.workspace().join(".usagi/sessions").join(name).exists());
    assert_eq!(fs::read(sessions_path).unwrap(), sessions_before);
    assert_eq!(fs::read(dispatch_path).ok(), dispatch_before);

    let branch = Command::new("git")
        .args(["-C", mcp.workspace().to_str().unwrap(), "branch", "--list"])
        .arg(format!("usagi/{name}"))
        .output()
        .unwrap();
    assert!(branch.status.success());
    assert!(branch.stdout.is_empty());
}

#[test]
fn production_delegate_issue_assigns_the_selected_role_without_persisting_instructions() {
    let mut mcp = McpHarness::start();
    write_session_role_catalog(&mcp, "coder", "Implement an issue", "ISSUE_ROLE_SECRET");
    fs::create_dir_all(mcp.workspace().join(".usagi/issues")).unwrap();
    fs::write(
        mcp.workspace().join(".usagi/issues/001-delegated-role.md"),
        "---\nnumber: 1\ntitle: Delegated role\nstatus: todo\npriority: high\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-08-01T00:00:00+00:00\nupdated_at: 2026-08-01T00:00:00+00:00\n---\n\nImplement it.\n",
    )
    .unwrap();

    let response = mcp.tool(
        "session_delegate_issue",
        &json!({"number":1, "name":"role-issue", "role":"coder"}),
    );
    assert!(response.get("error").is_none(), "{response}");
    let sessions = tool_text(&mcp.tool("session_list", &json!({})));
    let delegated = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "role-issue")
        .unwrap();
    assert_eq!(delegated["role_id"], "coder");
    assert_eq!(delegated["role_summary"], "Implement an issue");
    let dispatch = fs::read_to_string(mcp.data_dir().join("daemon/dispatch.json")).unwrap();
    assert!(!dispatch.contains("ISSUE_ROLE_SECRET"));
}

fn tool_text(response: &serde_json::Value) -> serde_json::Value {
    assert!(response.get("error").is_none(), "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn write_session_role_catalog(mcp: &McpHarness, id: &str, summary: &str, instructions: &str) {
    fs::write(
        mcp.workspace().join(".usagi/roles.toml"),
        format!(
            "version = 1\n[roles.{id}]\nsummary = {summary:?}\nscopes = [\"session\"]\ninstructions = {instructions:?}\n"
        ),
    )
    .unwrap();
}

fn wait_for_fixture_argv(
    mcp: &McpHarness,
    runtime: &str,
    user_prompt: Option<&str>,
    instruction_marker: &str,
) -> FixtureArgv {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(capture) = mcp.fixture_argv().into_iter().find(|capture| {
            capture.runtime == runtime
                && capture
                    .arguments
                    .iter()
                    .any(|argument| argument.contains(instruction_marker))
                && user_prompt.is_none_or(|prompt| {
                    capture.arguments.iter().any(|argument| argument == prompt)
                })
        }) {
            return capture;
        }
        assert!(
            Instant::now() < deadline,
            "shipping {runtime} argv was not captured before timeout"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The single ephemeral system-prompt value a shipping launch passed, read
/// through each product's own grammar.
fn shipping_system_prompt(capture: &FixtureArgv) -> String {
    if capture.runtime == "claude" {
        let positions = capture
            .arguments
            .iter()
            .enumerate()
            .filter_map(|(index, argument)| (argument == "--append-system-prompt").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(
            positions.len(),
            1,
            "Claude system prompt flag count changed"
        );
        capture
            .arguments
            .get(positions[0] + 1)
            .expect("Claude system prompt value is missing")
            .clone()
    } else {
        assert!(
            matches!(capture.runtime.as_str(), "codex" | "codex-fugu"),
            "unexpected fixture runtime"
        );
        let assignments = capture
            .arguments
            .iter()
            .filter(|argument| argument.starts_with("developer_instructions="))
            .collect::<Vec<_>>();
        assert_eq!(
            assignments.len(),
            1,
            "Codex-compatible developer instruction count changed"
        );
        let parsed: toml::Value = toml::from_str(assignments[0])
            .expect("shipping developer_instructions must remain valid TOML");
        parsed["developer_instructions"]
            .as_str()
            .expect("developer_instructions must remain a TOML string")
            .to_owned()
    }
}

fn assert_shipping_role_argv(
    capture: &FixtureArgv,
    role_id: &str,
    instructions: &str,
    user_prompt: Option<&str>,
    local_llm: bool,
) {
    let role = RoleId::new(role_id).unwrap();
    let expected = usagi_core::domain::agent::prompt::launch_system_prompt(
        usagi_core::domain::agent::prompt::PromptScope::Session,
        Some(shipping_tool_families(local_llm)),
        Some((&role, instructions)),
    );
    let system_prompt = shipping_system_prompt(capture);
    assert!(
        system_prompt == expected,
        "shipping system prompt composition or ordering changed"
    );
    assert_eq!(system_prompt.matches(instructions).count(), 1);
    assert_eq!(
        system_prompt
            .matches(&format!("<role id=\"{role_id}\">"))
            .count(),
        1
    );
    assert_eq!(
        capture
            .arguments
            .iter()
            .filter(|argument| argument.contains(instructions))
            .count(),
        1,
        "role instruction escaped its single ephemeral system argument"
    );
    if let Some(user_prompt) = user_prompt {
        assert_eq!(
            capture
                .arguments
                .iter()
                .filter(|argument| argument.as_str() == user_prompt)
                .count(),
            1,
            "initial user prompt argv changed"
        );
        assert!(
            !user_prompt.contains(instructions),
            "role instruction entered the initial user prompt"
        );
    }
}

/// The families a shipping launch gets: this harness leaves Issue and Memory at
/// their default (enabled) in both settings layers, so only the delegation server
/// varies with the local-LLM setting.
fn shipping_tool_families(
    local_llm: bool,
) -> usagi_core::domain::agent::mcp_tools::McpToolFamilies {
    usagi_core::domain::agent::mcp_tools::McpToolFamilies {
        issue: true,
        memory: true,
        local_llm,
    }
}

fn assert_shipping_legacy_argv(capture: &FixtureArgv) {
    let expected = usagi_core::domain::agent::prompt::launch_system_prompt(
        usagi_core::domain::agent::prompt::PromptScope::Session,
        Some(shipping_tool_families(false)),
        None,
    );
    let assignments = capture
        .arguments
        .iter()
        .filter(|argument| argument.starts_with("developer_instructions="))
        .collect::<Vec<_>>();
    assert_eq!(assignments.len(), 1);
    let parsed: toml::Value = toml::from_str(assignments[0]).unwrap();
    assert!(
        parsed["developer_instructions"].as_str() == Some(expected.as_str()),
        "role-free shipping argv no longer uses the legacy session prompt"
    );
}

fn assert_secret_absent_from_durable_data(mcp: &McpHarness, secrets: &[&str]) {
    fn visit(path: &std::path::Path, fixture_argv: &std::path::Path, secrets: &[&str]) {
        if path == fixture_argv {
            return;
        }
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), fixture_argv, secrets);
            }
            return;
        }
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        for secret in secrets {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "role instruction entered durable daemon data"
            );
        }
    }

    visit(
        &mcp.data_dir(),
        &mcp.data_dir().join("fixture-argv"),
        secrets,
    );
}

#[test]
fn production_role_prompt_contract_reaches_every_shipping_agent_argv() {
    const ROLE_SECRET: &str = "SHIPPING_ROLE_SECRET";
    const UPDATED_SECRET: &str = "SHIPPING_UPDATED_ROLE_SECRET";

    let mut mcp = McpHarness::start_with_all_agents();
    let caller_credential = mcp.launch_caller();
    let legacy = mcp
        .fixture_argv()
        .into_iter()
        .find(|capture| capture.runtime == "codex")
        .expect("legacy caller argv was captured");
    assert_shipping_legacy_argv(&legacy);
    mcp.restart_with_credential(&caller_credential);
    mcp.enable_local_llm();
    write_session_role_catalog(
        &mcp,
        "shipping-reviewer",
        "Review shipping argv",
        ROLE_SECRET,
    );

    let cases = [
        (
            "sakana-ai",
            "fixture-sakana",
            "codex-fugu",
            "check Sakana argv",
        ),
        ("claude", "fixture-claude", "claude", "check Claude argv"),
        ("codex", "fixture-codex", "codex", "check Codex argv"),
    ];
    let mut responses = Vec::new();
    let mut captures = Vec::new();
    for (index, (runtime, model, executable, prompt)) in cases.into_iter().enumerate() {
        let response = mcp.tool(
            "session_dispatch",
            &json!({
                "session":{"name":format!("shipping-role-{index}"), "role":"shipping-reviewer"},
                "agent":{"runtime":runtime,"model":model},
                "prompt":prompt
            }),
        );
        assert!(
            response.get("error").is_none(),
            "shipping {runtime} dispatch failed with code {:?}: {:?}",
            response["error"]["code"].as_i64(),
            response["error"]["message"].as_str()
        );
        assert!(!response.to_string().contains(ROLE_SECRET));
        responses.push(response);
        let capture = wait_for_fixture_argv(&mcp, executable, Some(prompt), ROLE_SECRET);
        assert_shipping_role_argv(
            &capture,
            "shipping-reviewer",
            ROLE_SECRET,
            Some(prompt),
            true,
        );
        captures.push(capture);
    }

    for capture in &captures {
        assert!(
            capture
                .arguments
                .iter()
                .any(|argument| argument.contains(DEFAULT_LOCAL_LLM_MODEL)),
            "local-LLM MCP configuration was not provisioned"
        );
    }

    // Editing the catalog cannot rewrite the already-running process argv.
    write_session_role_catalog(
        &mcp,
        "shipping-reviewer",
        "Review updated shipping argv",
        UPDATED_SECRET,
    );
    assert!(captures.iter().all(|capture| {
        capture
            .arguments
            .iter()
            .any(|argument| argument.contains(ROLE_SECRET))
            && capture
                .arguments
                .iter()
                .all(|argument| !argument.contains(UPDATED_SECRET))
    }));

    for response in responses {
        assert!(!response.to_string().contains(ROLE_SECRET));
    }
    assert_secret_absent_from_durable_data(&mcp, &[ROLE_SECRET, UPDATED_SECRET]);
}

#[test]
fn production_hook_capture_works_without_an_inherited_credential() {
    let mut mcp = McpHarness::start_in_production();
    let caller_credential = mcp.launch_caller();
    assert!(caller_credential.is_empty());
    let log = fs::read_to_string(mcp.fixture_log()).unwrap();
    assert!(log.lines().any(|line| line == "credential:unset"));
    assert!(!log.contains("USAGI_MCP_CALLER_CREDENTIAL="));
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
    let response = mcp.tool("user_decision_list", &json!({}));
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("decision caller provenance is unknown")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One process-spanning round trip keeps every last-mile assertion visible.
fn production_user_decision_round_trip_reaches_the_original_caller() {
    let mcp = McpHarness::start();
    let executable = env!("CARGO_BIN_EXE_usagi");
    mcp.replace_fixture_agent(
        "codex",
        &format!(
            r#"#!/bin/sh
if [ "$1 $2" = "login status" ]; then exit 0; fi
credential_excluded=false
approval_disabled=false
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-c" ] && [ "$2" = 'mcp_servers.usagi.env_vars = ["USAGI_HOME", "USAGI_RUNTIME_MODE", "USAGI_WORKSPACE_ROOT"]' ]; then
    credential_excluded=true
  fi
  if [ "$1" = "-c" ] && [ "$2" = 'mcp_servers.usagi.default_tools_approval_mode = "approve"' ]; then
    approval_disabled=true
  fi
  shift
done
if [ "$credential_excluded" != true ] || [ "$approval_disabled" != true ]; then
  printf 'unsafe Codex MCP environment or non-interactive approval configuration\n' >> "$USAGI_MCP_FIXTURE_LOG"
  exit 1
fi
{{
  printf '%s\n' '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","clientInfo":{{"name":"decision-agent","version":"1"}}}}}}'
  printf '%s\n' '{{"jsonrpc":"2.0","method":"notifications/initialized"}}'
  printf '%s\n' '{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"user_decision_request","arguments":{{"title":"Deploy?","prompt":"Choose","options":[{{"id":"yes","label":"Yes"}}]}}}}}}'
}} | env -i PATH="$PATH" USAGI_HOME="$USAGI_HOME" USAGI_RUNTIME_MODE="$USAGI_RUNTIME_MODE" USAGI_WORKSPACE_ROOT="$USAGI_WORKSPACE_ROOT" "{executable}" mcp >> "$USAGI_MCP_FIXTURE_LOG"
"#,
        ),
    );

    let lifecycle: serde_json::Value = serde_json::from_slice(
        &fs::read(support::daemon::lifecycle_state_path(&mcp.data_dir())).unwrap(),
    )
    .unwrap();
    let workspace: WorkspaceId =
        serde_json::from_value(lifecycle["state"]["workspace_id"].clone()).unwrap();
    let mut client = mcp.daemon_client();
    let reply = client
        .request(DaemonRequest::Dispatch {
            operation_id: OperationId::new().to_string(),
            intent: DispatchIntent {
                workspace,
                session_name: "decision-e2e".into(),
                caller: CallerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
                agent: DispatchAgentIntent::New {
                    runtime: AgentProfileId::new("codex").unwrap(),
                    model: ModelSelector::new("fixture-codex").unwrap(),
                },
                prompt: "request a human decision".into(),
            },
        })
        .unwrap();
    assert!(matches!(reply, DaemonReply::Accepted { .. }));

    let decision_path = mcp.data_dir().join("daemon/user-decisions.json");
    wait_until(|| {
        fs::read(&decision_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|state| !state["decisions"][0].is_null())
    });
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(&decision_path).unwrap()).unwrap();
    let decision: UserDecision = serde_json::from_value(state["decisions"][0].clone()).unwrap();
    let decision_id: UserDecisionId = decision.decision_id;

    let listed = client
        .request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::List,
            payload: json!({}),
        })
        .unwrap();
    assert!(
        matches!(listed, DaemonReply::Ok(ref body) if body["decisions"][0]["decision_id"] == json!(decision_id))
    );
    let resolved = client
        .request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::Resolve,
            payload: json!({"decision_id": decision_id, "answer": {"kind":"option", "option_id":"yes"}}),
        })
        .unwrap();
    assert!(matches!(resolved, DaemonReply::Ok(ref body) if body["status"] == "resolved"));

    let fetched = client
        .request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::Get,
            payload: json!({"decision_id": decision_id}),
        })
        .unwrap();
    assert!(matches!(fetched, DaemonReply::Ok(ref body) if body["answer"]["option_id"] == "yes"));

    let mut cancellable = decision.clone();
    cancellable.decision_id = UserDecisionId::new();
    cancellable.title = "Cancel me".into();
    cancellable.idempotency_key = None;
    let store = UserDecisionStore::new(mcp.data_dir().join("daemon"));
    store.create(cancellable.clone()).unwrap().unwrap();
    let cancelled = client
        .request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::Cancel,
            payload: json!({"decision_id": cancellable.decision_id}),
        })
        .unwrap();
    assert!(matches!(cancelled, DaemonReply::Ok(ref body) if body["status"] == "cancelled"));

    wait_until(|| {
        fs::read_to_string(mcp.fixture_log()).is_ok_and(|output| {
            output.contains("\\\"status\\\":\\\"resolved\\\"")
                && output.contains("\\\"option_id\\\":\\\"yes\\\"")
        })
    });
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(decision_path).unwrap()).unwrap();
    assert!(durable["events"].as_array().unwrap().is_empty());
}

/// The Agent child is handed the **mode-neutral** data home plus the runtime
/// mode, and re-applying that mode has to land it back on the very directory
/// the daemon is using.
///
/// This is pinned in `production` because it is the only mode whose selected
/// directory *is* the base: a caller that recovers the base by stripping one
/// component from the selected directory stays correct under `local` / `dev`
/// and silently hands production the directory *above* the data home (#608).
#[test]
fn production_agent_children_reapply_the_runtime_mode_onto_the_daemon_data_home() {
    let mut mcp = McpHarness::start_in_production();
    // production の selected directory は `$USAGI_HOME` そのもの。
    assert_eq!(mcp.data_dir(), mcp.home());

    mcp.replace_fixture_agent(
        "codex",
        r#"#!/bin/sh
if [ "$1" = login ] && [ "$2" = status ]; then exit 0; fi
printf 'data-home:%s\n' "$USAGI_HOME" >> "$USAGI_MCP_FIXTURE_LOG"
printf 'runtime-mode:%s\n' "$USAGI_RUNTIME_MODE" >> "$USAGI_MCP_FIXTURE_LOG"
printf 'credential:%s\n' "${USAGI_MCP_CALLER_CREDENTIAL-unset}" >> "$USAGI_MCP_FIXTURE_LOG"
printf 'fixture-ready\n' >> "$USAGI_MCP_FIXTURE_LOG"
while IFS= read -r line; do printf 'fixture-input:%s\n' "$line"; done
"#,
    );
    mcp.launch_caller_without_mcp();

    let log = fs::read_to_string(mcp.fixture_log()).unwrap();
    let logged = |prefix: &str| {
        log.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("the Agent child logged no {prefix} line:\n{log}"))
            .to_owned()
    };
    let child_home = PathBuf::from(logged("data-home:"));
    let child_mode = logged("runtime-mode:");
    assert_eq!(logged("credential:"), "unset");

    assert_eq!(child_mode, "production");
    // The child re-applies the announced mode to the announced home. This test
    // binary's artifact default is local, so dropping the announced
    // `production` spelling would resolve a different directory rather than
    // pass by accident.
    let resolved = usagi_core::infrastructure::paths::DataHome::new(
        &child_home,
        usagi_core::infrastructure::paths::RuntimeMode::from_env_value(
            Some(&child_mode),
            usagi_core::infrastructure::paths::DEFAULT_RUNTIME_MODE,
        ),
    )
    .selected();
    assert_eq!(resolved, mcp.data_dir());
    // And the announced home is never the directory above the data home.
    assert_ne!(
        child_home.as_path(),
        mcp.data_dir().parent().expect("the data home has a parent")
    );
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "condition was not met before timeout"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One process-spanning dispatch keeps completion and argv assertions together.
fn production_dispatch_worker_complete_reaches_the_caller_inbox() {
    let mut mcp = McpHarness::start();
    let caller_credential = mcp.launch_caller();
    mcp.restart_with_credential(&caller_credential);
    write_session_role_catalog(&mcp, "coder", "Dispatch coder", "DISPATCH_ROLE_SECRET");
    mcp.replace_fixture_agent(
        "codex",
        r#"#!/bin/sh
if [ "$1" = login ] && [ "$2" = status ]; then exit 0; fi
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"fixture-worker","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agent_complete","arguments":{"summary":"fixture completed","result":{"commits":["abc123"],"changed_files":["fixture.rs"],"verification":"fixture green"}}}}' \
  | "$USAGI_E2E_USAGI" mcp >> "$USAGI_MCP_FIXTURE_LOG"
"#,
    );

    let dispatched = mcp.tool(
        "session_dispatch",
        &json!({
            "session":{"name":"mcp-worker", "role":"coder"},
            "agent":{"runtime":"codex","model":"fixture-codex"},
            "prompt":"complete through MCP"
        }),
    );
    assert!(dispatched.get("error").is_none(), "{dispatched}");
    let admission: serde_json::Value =
        serde_json::from_str(dispatched["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(admission["run_id"].is_string());
    assert!(admission["terminal"].is_object());
    let argv = wait_for_fixture_argv(
        &mcp,
        "codex",
        Some("complete through MCP"),
        "DISPATCH_ROLE_SECRET",
    );
    assert_shipping_role_argv(
        &argv,
        "coder",
        "DISPATCH_ROLE_SECRET",
        Some("complete through MCP"),
        false,
    );

    let session = tool_text(&mcp.tool("session_get", &json!({"name":"mcp-worker"})));
    assert_eq!(session["role_id"], "coder");
    assert_eq!(session["role_summary"], "Dispatch coder");

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
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"fixture-worker","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
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
    let dispatch = fs::read_to_string(mcp.data_dir().join("daemon/dispatch.json")).unwrap();
    assert!(!dispatch.contains("DISPATCH_ROLE_SECRET"));
}
