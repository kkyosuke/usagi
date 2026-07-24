//! The hidden `usagi guard-workspace` subcommand.
//!
//! It is never run by a person: usagi wires it into Claude Code as a
//! `PreToolUse` hook (see [`crate::domain::settings::AgentCli::launch_command`]),
//! so every tool call is checked before it runs. The hook delivers its JSON
//! payload on stdin — the agent's `cwd`, the `tool_name`, and the tool input —
//! and usagi denies malformed, unknown, or unsafe calls. This hook is defense in
//! depth; the OS sandbox installed by `claude-sandbox` is the hard boundary.
//!
//! The same hook enforces one of two modes, chosen at runtime from `cwd`:
//!
//! - **Session mode** (cwd is inside `.usagi/sessions/<name>/`): file-writing
//!   tool paths are resolved through existing symlinks and denied when they
//!   escape the worktree. Shell and subagent calls are validated structurally,
//!   then delegated to the mandatory inherited OS sandbox.
//! - **Root mode** (cwd is the workspace root, *not* under `.usagi/sessions/`):
//!   the coordinator must not mutate the repository at all. Here the worktree
//!   confinement cannot help — cwd *is* the repo root, so nothing is "outside"
//!   it — so instead every file-writing tool (`Edit` / `Write` / `MultiEdit` /
//!   `NotebookEdit`) is denied regardless of path, and `Bash` calls are denied
//!   unless a shell command is in a deliberately small read-only allowlist.
//!
//! The [`crate::usecase::session_system_prompt`] tells the agent to stay put;
//! this hook enforces it for Claude.
//!
//! A denial is reported the way Claude Code's `PreToolUse` contract expects: a
//! `hookSpecificOutput` object on stdout with `permissionDecision: "deny"` (and
//! exit 0). An allowed call prints nothing, so the tool proceeds through Claude's
//! usual permission flow. The mode / path / git logic lives in
//! [`crate::usecase::workspace_guard`]; this is a thin stdin → stdout shim.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

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
    if let Err(error) = input.read_to_string(&mut raw) {
        write_denial(
            &mut output,
            format!("guard payload could not be read: {error}"),
        )?;
        return Ok(());
    }
    if let Some(reason) = deny_reason(&raw) {
        write_denial(&mut output, reason)?;
    }
    Ok(())
}

fn write_denial(mut output: impl Write, reason: String) -> Result<()> {
    let payload = GuardOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PreToolUse",
            permission_decision: "deny",
            permission_decision_reason: reason,
        },
    };
    write!(output, "{}", serde_json::to_string(&payload)?)?;
    Ok(())
}

/// The reason to deny this tool call, or `None` to let it proceed. The mode is
/// chosen from the payload's canonical `cwd`. Any malformed or incomplete
/// payload is denied: a hook parser failure must never become permission.
fn deny_reason(raw: &str) -> Option<String> {
    let payload: serde_json::Value = match serde_json::from_str(raw) {
        Ok(payload) => payload,
        Err(error) => return Some(format!("malformed PreToolUse payload: {error}")),
    };
    let cwd = match payload.get("cwd").and_then(serde_json::Value::as_str) {
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        _ => return Some("PreToolUse payload has no absolute cwd".to_string()),
    };
    let cwd = match std::fs::canonicalize(&cwd) {
        Ok(cwd) => cwd,
        Err(error) => return Some(format!("PreToolUse cwd cannot be canonicalized: {error}")),
    };
    let tool_name = match payload.get("tool_name").and_then(serde_json::Value::as_str) {
        Some(name) if !name.is_empty() => name,
        _ => return Some("PreToolUse payload has no tool_name".to_string()),
    };
    let input = match payload
        .get("tool_input")
        .and_then(serde_json::Value::as_object)
    {
        Some(input) => input,
        None => return Some("PreToolUse payload has no object tool_input".to_string()),
    };

    if let Some(worktree) = session_worktree_root(&cwd) {
        session_deny_reason(tool_name, input, &cwd, &worktree)
    } else {
        root_deny_reason(tool_name, input)
    }
}

fn session_worktree_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|candidate| {
        let sessions = candidate.parent()?;
        let usagi = sessions.parent()?;
        (sessions.file_name()? == "sessions" && usagi.file_name()? == ".usagi")
            .then(|| candidate.to_path_buf())
    })
}

/// Session mode: canonicalize file-write targets and reject escapes. Known
/// non-writing tools pass; shell and subagent effects remain confined by the
/// inherited OS sandbox.
fn session_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
    worktree: &Path,
) -> Option<String> {
    if workspace_guard::is_write_tool(tool_name) {
        let target = match input.get("file_path").and_then(serde_json::Value::as_str) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => return Some(format!("{tool_name} payload has no file_path")),
        };
        return match workspace_guard::path_escapes_root(worktree, cwd, &target) {
            Ok(false) => None,
            Ok(true) => Some(format!(
                "{} はセッション worktree {} の外です。",
                target.display(),
                worktree.display()
            )),
            Err(error) => Some(format!("tool path cannot be safely resolved: {error}")),
        };
    }
    match tool_name {
        // Shell commands and subagents inherit the mandatory OS sandbox. The
        // hook validates their shape but deliberately does not claim to parse
        // shell semantics as a security boundary.
        "Bash" => match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) if !command.trim().is_empty() => None,
            _ => Some("Bash payload has no command".to_string()),
        },
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "Task" | "Skill" | "TodoWrite"
        | "AskUserQuestion" => None,
        name if name.starts_with("mcp__") => None,
        _ => Some(format!("unknown tool is denied fail-closed: {tool_name}")),
    }
}

/// Root mode: the coordinator must not mutate the repository. Deny every
/// file-writing tool regardless of path and every shell command outside the
/// strict read-only allowlist.
fn root_deny_reason(
    tool_name: &str,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if workspace_guard::is_write_tool(tool_name) {
        return Some(format!(
            "ワークスペースルート（コーディネータ）ではファイル書き込みツール（{tool_name}）を実行できません。\
             root 行はリポジトリを変更しません。編集はセッションの worktree に委譲してください。"
        ));
    }
    if tool_name == "Bash" {
        let command = match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) if !command.trim().is_empty() => command,
            _ => return Some("Bash payload has no command".to_string()),
        };
        return workspace_guard::command_mutates_repo(command).then(|| {
            format!(
                "ワークスペースルートでは read-only allowlist 外の shell command を実行できません（{command}）。"
            )
        });
    }
    match tool_name {
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "Task" | "Skill" | "TodoWrite"
        | "AskUserQuestion" => None,
        name if name.starts_with("mcp__") => None,
        _ => Some(format!("unknown tool is denied fail-closed: {tool_name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let worktree = root.join(".usagi/sessions/work");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        (temp, root, worktree)
    }

    fn payload(cwd: &Path, tool_name: &str, input: serde_json::Value) -> String {
        serde_json::json!({"cwd": cwd, "tool_name": tool_name, "tool_input": input}).to_string()
    }

    #[test]
    fn denies_a_tool_targeting_the_parent_repo() {
        let (_temp, root, worktree) = layout();
        let target = root.join("src/main.rs");
        let payload = payload(&worktree, "Edit", serde_json::json!({"file_path": target}));
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("\"hookEventName\":\"PreToolUse\""));
        // The reason names the offending path so the agent learns what to avoid.
        assert!(written.contains("src/main.rs"));
    }

    #[test]
    fn allows_a_tool_inside_the_worktree() {
        let (_temp, _root, worktree) = layout();
        let payload = payload(
            &worktree,
            "Edit",
            serde_json::json!({"file_path": worktree.join("src/main.rs")}),
        );
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        // An allowed call writes nothing, so the tool runs through Claude's
        // normal permission flow.
        assert!(out.is_empty());
    }

    #[test]
    fn denies_when_the_payload_is_missing_fields_or_unparseable() {
        for payload in [
            r#"{"tool_input":{"file_path":"/repo/src/main.rs"}}"#,
            r#"{"cwd":"/repo/.usagi/sessions/work","tool_input":{"command":"ls"}}"#,
            "garbage",
        ] {
            assert!(deny_reason(payload).is_some());
        }
    }

    #[test]
    fn denies_unknown_tools_and_uncanonicalizable_cwd() {
        let (_temp, _root, worktree) = layout();
        let unknown = payload(&worktree, "FutureMutator", serde_json::json!({}));
        assert!(deny_reason(&unknown).unwrap().contains("unknown tool"));
        let missing = payload(
            &worktree.join("missing"),
            "Read",
            serde_json::json!({"file_path": "/etc/hosts"}),
        );
        assert!(deny_reason(&missing)
            .unwrap()
            .contains("cannot be canonicalized"));
    }

    #[test]
    fn root_mode_denies_a_write_tool_at_any_path() {
        // cwd is the workspace root (not under .usagi/sessions/), so even a
        // write inside the repo is denied — the coordinator must not mutate it.
        let (temp, _root, _worktree) = layout();
        let payload = payload(
            temp.path(),
            "Write",
            serde_json::json!({"file_path": temp.path().join("src/main.rs")}),
        );
        let mut out = Vec::new();
        run(Cursor::new(payload), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains("\"permissionDecision\":\"deny\""));
        assert!(written.contains("Write"));
    }

    #[test]
    fn root_mode_denies_a_mutating_git_command() {
        let (temp, _root, _worktree) = layout();
        let payload = payload(
            temp.path(),
            "Bash",
            serde_json::json!({"command": "git commit -m x"}),
        );
        assert!(deny_reason(&payload).unwrap().contains("git commit -m x"));
    }

    #[test]
    fn root_mode_allows_read_only_git_and_other_tools() {
        let (temp, _root, _worktree) = layout();
        // Read-only git passes.
        let git = payload(
            temp.path(),
            "Bash",
            serde_json::json!({"command": "git status"}),
        );
        assert_eq!(deny_reason(&git), None);
        // A non-writing tool (Read) passes regardless of path.
        let read = payload(
            temp.path(),
            "Read",
            serde_json::json!({"file_path": "/etc/hosts"}),
        );
        assert_eq!(deny_reason(&read), None);
    }

    #[test]
    fn root_mode_denies_adversarial_shell_commands() {
        let (temp, _root, _worktree) = layout();
        for command in [
            "sh -c 'git commit -m x'",
            "git status > /tmp/sentinel",
            "sed -i s/a/b/ file",
            "rm -f file",
            "env git commit -m x",
            "/usr/bin/git commit -m x",
            "command git status",
        ] {
            let payload = payload(temp.path(), "Bash", serde_json::json!({"command": command}));
            assert!(deny_reason(&payload).is_some(), "allowed {command}");
        }
    }

    #[test]
    fn session_shell_is_delegated_to_the_os_sandbox_but_malformed_is_denied() {
        let (_temp, _root, worktree) = layout();
        for command in ["sh -c 'echo x > /tmp/sentinel'", "rm -f /tmp/sentinel"] {
            let payload = payload(&worktree, "Bash", serde_json::json!({"command": command}));
            assert_eq!(deny_reason(&payload), None, "sandbox handles {command}");
        }
        let malformed = payload(&worktree, "Bash", serde_json::json!({}));
        assert!(deny_reason(&malformed).is_some());
    }
}
