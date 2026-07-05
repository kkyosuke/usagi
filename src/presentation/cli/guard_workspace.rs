//! The hidden `usagi guard-workspace` subcommand.
//!
//! It is never run by a person: usagi wires it into Claude Code as a
//! `PreToolUse` hook (see [`crate::domain::settings::AgentCli::launch_command`]),
//! so every tool call is checked before it runs. The hook delivers its JSON
//! payload on stdin — the agent's `cwd`, the `tool_name`, and the tool input —
//! and usagi denies the call when it would touch the wrong tree.
//!
//! The same hook enforces one of two modes, chosen at runtime from `cwd`:
//!
//! - **Session mode** (cwd is inside `.usagi/sessions/<name>/`): a session's
//!   agent may edit anything inside its worktree, so a file-touching tool is
//!   denied only when its `tool_input.file_path` escapes the worktree. A usagi
//!   session worktree lives *inside* the main repository, so the repo root and
//!   sibling worktrees sit just above it on disk; without this an agent could
//!   edit `<repo>/src/...` or `cd` up and quietly work on the wrong tree.
//! - **Root mode** (cwd is the workspace root, *not* under `.usagi/sessions/`):
//!   the coordinator must not mutate the repository at all. Here the worktree
//!   confinement cannot help — cwd *is* the repo root, so nothing is "outside"
//!   it — so instead every file-writing tool (`Edit` / `Write` / `MultiEdit` /
//!   `NotebookEdit`) is denied regardless of path, and `Bash` calls are denied
//!   when they invoke a repository-mutating git subcommand (read-only git like
//!   `status` / `log` / `diff` still runs).
//!
//! The [`crate::usecase::session_system_prompt`] tells the agent to stay put;
//! this hook enforces it for Claude.
//!
//! A denial is reported the way Claude Code's `PreToolUse` contract expects: a
//! `hookSpecificOutput` object on stdout with `permissionDecision: "deny"` (and
//! exit 0). An allowed call prints nothing, so the tool proceeds through Claude's
//! usual permission flow. The mode / path / git logic lives in
//! [`crate::usecase::workspace_guard`], the JSON parsing in
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

/// The reason to deny this tool call, or `None` to let it proceed. The mode is
/// chosen from the payload's `cwd`: a payload with no `cwd` to anchor against,
/// or one that is unparseable, is nothing to guard and allowed. Split from
/// [`run`] so the decision is tested without the stdin / stdout IO.
fn deny_reason(raw: &str) -> Option<String> {
    let worktree = agent_state_store::worktree_from_hook_json(raw)?;
    if workspace_guard::is_session_worktree(&worktree) {
        session_deny_reason(raw, &worktree)
    } else {
        root_deny_reason(raw)
    }
}

/// Session mode: deny only when the tool's target (`tool_input.file_path`)
/// escapes the worktree. A tool with no file path (e.g. `Bash`, `Grep`) is
/// nothing to confine, so it is allowed.
fn session_deny_reason(raw: &str, worktree: &std::path::Path) -> Option<String> {
    let target = agent_state_store::tool_path_from_hook_json(raw)?;
    workspace_guard::escapes_worktree(worktree, &target).then(|| {
        format!(
            "{} はセッション worktree {} の外です。作業はこの worktree 配下だけで完結させ、\
             親のメインリポジトリのファイルには触れないでください。",
            target.display(),
            worktree.display()
        )
    })
}

/// Root mode: the coordinator must not mutate the repository. Deny every
/// file-writing tool regardless of path, and any `Bash` call that runs a
/// repository-mutating git subcommand (read-only git is allowed). All other
/// tools proceed.
fn root_deny_reason(raw: &str) -> Option<String> {
    let tool_name = agent_state_store::tool_name_from_hook_json(raw)?;
    if workspace_guard::is_write_tool(&tool_name) {
        return Some(format!(
            "ワークスペースルート（コーディネータ）ではファイル書き込みツール（{tool_name}）を実行できません。\
             root 行はリポジトリを変更しません。編集はセッションの worktree に委譲してください。"
        ));
    }
    let command = agent_state_store::bash_command_from_hook_json(raw)?;
    workspace_guard::command_mutates_repo(&command).then(|| {
        format!(
            "ワークスペースルート（コーディネータ）ではリポジトリを変更する git を実行できません（{command}）。\
             root 行はリポジトリを変更しません。読み取り（status / log / diff）は可能です。変更はセッションの worktree に委譲してください。"
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
            // A session cwd but no file path (e.g. a Bash call).
            r#"{"cwd":"/repo/.usagi/sessions/work","tool_input":{"command":"ls"}}"#,
            // Not JSON at all.
            "garbage",
        ] {
            assert_eq!(deny_reason(payload), None);
        }
    }

    #[test]
    fn root_mode_denies_a_write_tool_at_any_path() {
        // cwd is the workspace root (not under .usagi/sessions/), so even a
        // write inside the repo is denied — the coordinator must not mutate it.
        let payload =
            r#"{"cwd":"/repo","tool_name":"Write","tool_input":{"file_path":"/repo/src/main.rs"}}"#;
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("Write"));
    }

    #[test]
    fn root_mode_denies_a_mutating_git_command() {
        let payload =
            r#"{"cwd":"/repo","tool_name":"Bash","tool_input":{"command":"git commit -m x"}}"#;
        assert!(deny_reason(payload).unwrap().contains("git commit -m x"));
    }

    #[test]
    fn root_mode_allows_read_only_git_and_other_tools() {
        // Read-only git passes.
        assert_eq!(
            deny_reason(
                r#"{"cwd":"/repo","tool_name":"Bash","tool_input":{"command":"git status"}}"#
            ),
            None
        );
        // A non-writing tool (Read) passes regardless of path.
        assert_eq!(
            deny_reason(
                r#"{"cwd":"/repo","tool_name":"Read","tool_input":{"file_path":"/repo/src/main.rs"}}"#
            ),
            None
        );
    }
}
