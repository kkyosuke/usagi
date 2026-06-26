//! The hidden `usagi guard-workspace` subcommand.
//!
//! It is never run by a person: usagi wires it into Claude Code as a
//! `PreToolUse` hook (see [`crate::domain::settings::AgentCli::launch_command`]),
//! so every file-touching tool call is checked before it runs. The hook delivers
//! its JSON payload on stdin — the agent's worktree (`cwd`) and the tool's target
//! (`tool_input.file_path`) — and usagi denies the call when the target escapes
//! the session worktree.
//!
//! Why this exists: a usagi session worktree lives *inside* the main repository
//! (`<repo>/.usagi/sessions/<name>/`), so the repo root and its sibling worktrees
//! sit just above it. Without a guard an agent can read or edit `<repo>/src/...`
//! or `cd` up into the repo and quietly work on the wrong tree. The
//! [`crate::usecase::session_system_prompt`] tells the agent to stay put; this
//! hook enforces it for Claude.
//!
//! A denial is reported the way Claude Code's `PreToolUse` contract expects: a
//! `hookSpecificOutput` object on stdout with `permissionDecision: "deny"` (and
//! exit 0). An allowed call prints nothing, so the tool proceeds through Claude's
//! usual permission flow. The path / JSON logic lives in
//! [`crate::usecase::workspace_guard`] and
//! [`crate::infrastructure::agent_state_store`]; this is a thin stdin → stdout
//! shim.

use std::io::{Read, Write};

use anyhow::Result;
use serde::Serialize;

use crate::infrastructure::agent_state_store;
use crate::usecase::workspace_guard;

/// Claude Code's `PreToolUse` deny payload: `{"hookSpecificOutput": …}`.
#[derive(Serialize)]
struct GuardOutput {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput,
}

/// The decision Claude reads back: deny this tool call, with a reason it shows
/// to the agent.
#[derive(Serialize)]
struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    #[serde(rename = "permissionDecision")]
    permission_decision: &'static str,
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: String,
}

/// Entry point for `usagi guard-workspace`. Reads the `PreToolUse` payload from
/// stdin, and when the tool's target escapes the agent's worktree, writes the
/// deny decision to stdout. Reading and writing are injected so the whole
/// decision is unit-tested without the process's real stdin / stdout.
pub fn run(mut input: impl Read, mut output: impl Write) -> Result<()> {
    let mut raw = String::new();
    let _ = input.read_to_string(&mut raw);
    if let Some(reason) = deny_reason(&raw) {
        let payload = GuardOutput {
            hook_specific_output: HookSpecificOutput {
                hook_event_name: "PreToolUse",
                permission_decision: "deny",
                permission_decision_reason: reason,
            },
        };
        write!(output, "{}", serde_json::to_string(&payload)?)?;
    }
    Ok(())
}

/// The reason to deny this tool call, or `None` to let it proceed. A call is
/// denied only when the payload carries both a worktree (`cwd`) and a tool target
/// (`tool_input.file_path`) and that target escapes the worktree — a missing
/// field, an unparseable payload, or a tool with no file path (e.g. `Bash`) is
/// nothing to guard, so it is allowed. Split from [`run`] so the decision is
/// tested without the stdin / stdout IO.
fn deny_reason(raw: &str) -> Option<String> {
    let worktree = agent_state_store::worktree_from_hook_json(raw)?;
    let target = agent_state_store::tool_path_from_hook_json(raw)?;
    workspace_guard::escapes_worktree(&worktree, &target).then(|| {
        format!(
            "{} はセッション worktree {} の外です。作業はこの worktree 配下だけで完結させ、\
             親のメインリポジトリのファイルには触れないでください。",
            target.display(),
            worktree.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn denies_a_tool_targeting_the_parent_repo() {
        let payload = r#"{"cwd":"/repo/.usagi/sessions/work","tool_name":"Edit","tool_input":{"file_path":"/repo/src/main.rs"}}"#;
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("\"hookEventName\":\"PreToolUse\""));
        // The reason names the offending path so the agent learns what to avoid.
        assert!(written.contains("/repo/src/main.rs"));
    }

    #[test]
    fn allows_a_tool_inside_the_worktree() {
        let payload = r#"{"cwd":"/repo/.usagi/sessions/work","tool_name":"Edit","tool_input":{"file_path":"/repo/.usagi/sessions/work/src/main.rs"}}"#;
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        // An allowed call writes nothing, so the tool runs through Claude's
        // normal permission flow.
        assert!(out.is_empty());
    }

    #[test]
    fn allows_when_the_payload_is_missing_fields_or_unparseable() {
        // No cwd to anchor against.
        for payload in [
            r#"{"tool_input":{"file_path":"/repo/src/main.rs"}}"#,
            // A cwd but no file path (e.g. a Bash call).
            r#"{"cwd":"/repo/.usagi/sessions/work","tool_input":{"command":"ls"}}"#,
            // Not JSON at all.
            "garbage",
        ] {
            assert_eq!(deny_reason(payload), None);
        }
    }
}
