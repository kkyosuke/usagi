//! Claude Code adapter.
//!
//! Builds Claude's full launch command: it wires usagi's MCP servers in via
//! `--mcp-config`, a session-scoped instruction via `--append-system-prompt`,
//! and lifecycle hooks via `--settings`, all rendered here in the infrastructure
//! layer (where `serde_json` is at hand to build and escape the JSON) from the
//! [`AgentWiring`] policy the domain hands over.
//!
//! It also answers whether a worktree has a Claude conversation to resume, by
//! looking for the transcript Claude Code keeps per project directory — so
//! `:agent` can continue (`claude --continue`) only when continuing would
//! actually find something.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::domain::agent::{Agent, AgentWiring};

/// System-prompt addendum injected into agents launched from a usagi session.
///
/// Every agent `:agent` starts already lives inside the session's dedicated
/// worktree, so the usual "create a worktree first" workflow step is redundant
/// here. We tell the agent up front to skip it and work in place. Kept free of
/// single quotes so it survives the single-quoted shell argument verbatim.
const SESSION_WORKTREE_PROMPT: &str = "あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。";

/// System-prompt addendum added when a local LLM MCP server is wired in.
///
/// It nudges the cloud agent to offload light, low-stakes work (summaries,
/// naming, boilerplate, simple transforms) to the `local_llm_ask` tool so the
/// cloud model's tokens are spent on the work that actually needs it. Kept free
/// of single quotes so it survives the single-quoted shell argument verbatim.
const LOCAL_LLM_PROMPT: &str = "トークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。";

/// One MCP server entry: the program to run and its arguments.
#[derive(Serialize)]
struct McpServer {
    command: String,
    args: Vec<String>,
}

/// The `"mcpServers"` map wired into Claude: always the unified `usagi` server
/// (`<usagi_bin> mcp`) so the agent can manage issues, memories and sessions;
/// plus the `usagi-llm` server (`<usagi_bin> llm-mcp --model <model>`) when the
/// local LLM is enabled, so the agent can offload light work to it. Field order
/// is the serialized key order, so `usagi` precedes `usagi-llm`.
#[derive(Serialize)]
struct McpServers {
    usagi: McpServer,
    #[serde(rename = "usagi-llm", skip_serializing_if = "Option::is_none")]
    usagi_llm: Option<McpServer>,
}

/// The `--mcp-config` payload: `{"mcpServers": …}`.
#[derive(Serialize)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: McpServers,
}

/// A single hook command: `{"type":"command","command":"…"}`.
#[derive(Serialize)]
struct HookCommand {
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
}

/// A hook entry wrapping its commands, as Claude's settings schema expects
/// (`{"hooks":[ … ]}`).
#[derive(Serialize)]
struct HookEntry {
    hooks: Vec<HookCommand>,
}

/// The lifecycle-hook map wiring Claude Code's events back into usagi, so the
/// agent reports its own ready / running / waiting state instead of usagi
/// guessing from the terminal bell. Each hook runs `<usagi_bin> agent-phase
/// <phase>`, which records the phase for the worktree the agent runs in (the hook
/// delivers its `cwd` on stdin); the home screen's session watcher reads it back
/// to mark the session.
///
/// The events: a freshly started or resumed session is idle (`SessionStart` →
/// `ready`); a submitted prompt starts a turn (`UserPromptSubmit` → `running`);
/// finishing a turn means the agent is done (`Stop` → `ended`); pausing mid-turn
/// for the user's input or permission means it waits (`Notification` →
/// `waiting`). A tool-permission prompt is also a wait, but `Notification` only
/// fires for it when the user is away; the dedicated `PermissionRequest` →
/// `waiting` hook catches it reliably (it fires right as the prompt appears, even
/// while the session is focused). The session ending is also done (`SessionEnd` →
/// `ended`).
#[derive(Serialize)]
struct Hooks {
    #[serde(rename = "UserPromptSubmit")]
    user_prompt_submit: Vec<HookEntry>,
    #[serde(rename = "Stop")]
    stop: Vec<HookEntry>,
    #[serde(rename = "Notification")]
    notification: Vec<HookEntry>,
    #[serde(rename = "PermissionRequest")]
    permission_request: Vec<HookEntry>,
    #[serde(rename = "SessionStart")]
    session_start: Vec<HookEntry>,
    #[serde(rename = "SessionEnd")]
    session_end: Vec<HookEntry>,
}

/// The `--settings` payload: `{"hooks": …}`. Passed via `--settings`, which
/// *merges* with the user's own settings rather than replacing them.
#[derive(Serialize)]
struct HookSettings {
    hooks: Hooks,
}

/// The `--mcp-config` JSON for Claude Code. `usagi_bin` is the resolved usagi
/// binary path (so the wiring resolves even when usagi is run from a build and
/// not on `$PATH`); `serde_json` escapes it, so a Windows path with backslashes
/// stays valid JSON. The model name comes from a fixed allowlist
/// (`LOCAL_LLM_MODELS`).
fn mcp_config_json(local_llm_model: Option<&str>, usagi_bin: &str) -> String {
    let config = McpConfig {
        mcp_servers: McpServers {
            usagi: McpServer {
                command: usagi_bin.to_string(),
                args: vec!["mcp".to_string()],
            },
            usagi_llm: local_llm_model.map(|model| McpServer {
                command: usagi_bin.to_string(),
                args: vec![
                    "llm-mcp".to_string(),
                    "--model".to_string(),
                    model.to_string(),
                ],
            }),
        },
    };
    serde_json::to_string(&config).expect("MCP config serializes to JSON")
}

/// The `--settings` JSON wiring Claude Code's lifecycle hooks back into usagi
/// (see [`Hooks`]). `usagi_bin` is the resolved usagi binary path; `serde_json`
/// escapes it, and the JSON contains only double quotes so it survives the
/// single-quoted shell argument.
fn claude_hooks_settings(usagi_bin: &str) -> String {
    let phase = |phase: &str| {
        vec![HookEntry {
            hooks: vec![HookCommand {
                kind: "command",
                command: format!("{usagi_bin} agent-phase {phase}"),
            }],
        }]
    };
    let settings = HookSettings {
        hooks: Hooks {
            user_prompt_submit: phase("running"),
            stop: phase("ended"),
            notification: phase("waiting"),
            permission_request: phase("waiting"),
            session_start: phase("ready"),
            session_end: phase("ended"),
        },
    };
    serde_json::to_string(&settings).expect("hook settings serialize to JSON")
}

/// Wrap `text` as a single shell argument in single quotes, safe to drop into a
/// `sh -c` command line. A single quote cannot appear inside a single-quoted
/// string, so each one is rendered as `'\''` (close the quote, an escaped quote,
/// reopen) — the standard POSIX idiom. Everything else (newlines, `$`, spaces …)
/// is literal inside single quotes, so the agent receives the prompt verbatim.
fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The Claude Code adapter.
#[derive(Default)]
pub struct ClaudeAgent;

impl ClaudeAgent {
    /// A Claude adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Agent for ClaudeAgent {
    fn program(&self) -> &'static str {
        "claude"
    }

    fn launch_command(
        &self,
        wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        let local_llm_model = wiring.local_llm_model.as_deref();
        let mcp_config = mcp_config_json(local_llm_model, &wiring.usagi_bin);
        // The system prompt tells the agent it is already inside a usagi worktree,
        // so it skips creating one, and — when the local LLM is on — to delegate
        // light tasks to it.
        let system_prompt = match local_llm_model {
            Some(_) => format!("{SESSION_WORKTREE_PROMPT}{LOCAL_LLM_PROMPT}"),
            None => SESSION_WORKTREE_PROMPT.to_string(),
        };
        let hooks = claude_hooks_settings(&wiring.usagi_bin);
        // The wiring arguments are single-quoted so the shell passes them through
        // verbatim (none of these values contains a single quote). `--continue`
        // resumes the most recent conversation in the worktree; placed right after
        // the program name so it reads like a plain `claude -c` with usagi's wiring
        // appended.
        let resume_flag = if resume { "--continue " } else { "" };
        // A queued prompt rides along as Claude's positional query, so the agent
        // opens interactively already working on it. Unlike the wiring above it is
        // arbitrary user text, so it is escaped for the single-quoted shell context
        // (see [`shell_single_quote`]). Placed last so it is the trailing argument.
        let prompt_arg = match initial_prompt {
            Some(prompt) => format!(" {}", shell_single_quote(prompt)),
            None => String::new(),
        };
        format!(
            "claude {resume_flag}--mcp-config '{mcp_config}' \
             --append-system-prompt '{system_prompt}' \
             --settings '{hooks}'{prompt_arg}"
        )
    }

    fn has_resumable_session(&self, dir: &Path) -> bool {
        claude_projects_root().is_some_and(|root| has_resumable_session_in(&root, dir))
    }

    fn forget_session(&self, dir: &Path) {
        if let Some(root) = claude_projects_root() {
            forget_session_in(&root, dir);
        }
    }
}

/// Where Claude Code stores each project's conversation transcripts:
/// `~/.claude/projects`. `None` when the home directory can't be determined, so
/// usagi simply launches fresh rather than guessing.
fn claude_projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude").join("projects"))
}

/// The transcript directory name Claude Code derives from a working directory:
/// every non-alphanumeric character of the absolute path is replaced with `-`
/// (e.g. `/Users/a/proj.x/.usagi` → `-Users-a-proj-x--usagi`). Mirroring that
/// scheme lets usagi find the worktree's transcripts to decide whether a resume
/// is possible.
fn claude_project_dir_name(worktree: &Path) -> String {
    worktree
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether `projects_root` holds a non-empty transcript directory for the
/// worktree at `dir` — i.e. at least one `*.jsonl` transcript Claude could
/// resume with `--continue`. A missing directory (no prior run, or Claude's
/// path scheme changed) reads as "nothing to resume", so `:agent` falls back to
/// a fresh launch.
fn has_resumable_session_in(projects_root: &Path, dir: &Path) -> bool {
    let project_dir = projects_root.join(claude_project_dir_name(dir));
    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
}

/// Delete Claude Code's transcript directory for the worktree at `dir` under
/// `projects_root` (best-effort). A missing directory — nothing ever run there —
/// is a no-op, so removing a session that never launched Claude is harmless.
fn forget_session_in(projects_root: &Path, dir: &Path) {
    let project_dir = projects_root.join(claude_project_dir_name(dir));
    let _ = std::fs::remove_dir_all(project_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// An [`AgentWiring`] for the tests: the bare name `usagi` stands in for the
    /// resolved binary path the caller passes, with the local LLM off unless a
    /// model is given.
    fn wiring(usagi_bin: &str, local_llm_model: Option<&str>) -> AgentWiring {
        AgentWiring {
            usagi_bin: usagi_bin.to_string(),
            local_llm_model: local_llm_model.map(str::to_string),
        }
    }

    #[test]
    fn launch_command_wires_in_the_usagi_mcp_servers() {
        // With the local LLM off (`None`), the unified usagi server is wired in
        // and the system prompt is just the worktree note.
        let launch = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        // The program is `claude`, with usagi's MCP server passed inline via
        // `--mcp-config` and a session-scoped instruction passed via
        // `--append-system-prompt` (both single-quoted so the shell keeps them).
        assert_eq!(
            launch,
            "claude --mcp-config '{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}' \
             --append-system-prompt 'あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。' \
             --settings '{\"hooks\":{\"UserPromptSubmit\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]}],\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}],\"Notification\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}],\"PermissionRequest\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}],\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ready\"}]}],\"SessionEnd\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}]}}'"
        );
    }

    #[test]
    fn launch_command_adds_continue_only_when_resuming() {
        // Resuming inserts `--continue` right after the program name so Claude
        // picks up the worktree's previous conversation; the rest of the wiring
        // is unchanged.
        let resumed = ClaudeAgent::new().launch_command(&wiring("usagi", None), true, None);
        assert!(resumed.starts_with("claude --continue --mcp-config '"));
        // Without resuming the flag is absent and the command starts plainly.
        let fresh = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(fresh.starts_with("claude --mcp-config '"));
        assert!(!fresh.contains("--continue"));
    }

    #[test]
    fn launch_command_appends_an_initial_prompt_as_the_trailing_query() {
        // A queued prompt rides along as Claude's positional query so the agent
        // opens already working on it. It is the trailing, single-quoted argument;
        // the wiring before it is unchanged.
        let launch =
            ClaudeAgent::new().launch_command(&wiring("usagi", None), false, Some("fix issue #50"));
        assert!(launch.ends_with(" 'fix issue #50'"));
        // The wiring is still present and the program still starts plainly.
        assert!(launch.starts_with("claude --mcp-config '"));
        assert!(launch.contains("--append-system-prompt '"));
        // With no prompt the trailing query is absent: the command is exactly the
        // prompt-carrying one with its ` '…'` suffix stripped.
        let plain = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains("fix issue #50"));
        assert_eq!(launch, format!("{plain} 'fix issue #50'"));
    }

    #[test]
    fn launch_command_escapes_single_quotes_in_an_initial_prompt() {
        // Arbitrary user prompt text may contain single quotes, which would
        // otherwise break out of the shell argument. Each is rendered as the POSIX
        // `'\''` idiom so the agent receives the prompt verbatim.
        let launch = ClaudeAgent::new().launch_command(
            &wiring("usagi", None),
            false,
            Some("don't break 'this'"),
        );
        assert!(launch.ends_with(r" 'don'\''t break '\''this'\'''"));
    }

    #[test]
    fn launch_command_carries_a_prompt_alongside_continue() {
        // Resuming and an opening prompt compose: `--continue` stays right after
        // the program name and the prompt is still the trailing query.
        let launch =
            ClaudeAgent::new().launch_command(&wiring("usagi", None), true, Some("keep going"));
        assert!(launch.starts_with("claude --continue --mcp-config '"));
        assert!(launch.ends_with(" 'keep going'"));
    }

    #[test]
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // An embedded single quote closes, escapes, and reopens the quoting.
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
        // Other shell metacharacters are literal inside single quotes.
        assert_eq!(shell_single_quote("$x `y` \"z\""), "'$x `y` \"z\"'");
    }

    #[test]
    fn launch_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the issue server in the
        // MCP config and the delegation prompt is appended after the worktree note.
        let launch = ClaudeAgent::new().launch_command(
            &wiring("usagi", Some("qwen2.5-coder:7b")),
            false,
            None,
        );
        assert!(launch.contains(
            "\"usagi-llm\":{\"command\":\"usagi\",\"args\":[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]}"
        ));
        // The issue server is still present alongside it.
        assert!(launch.contains("\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}"));
        // The delegation instruction is appended to the worktree note.
        assert!(launch.contains("local_llm_ask"));
    }

    #[test]
    fn launch_command_wires_in_lifecycle_hooks() {
        // The phase-reporting hooks ride along via --settings whether or not the
        // local LLM is enabled, so usagi always learns the agent's state.
        for model in [None, Some("qwen2.5-coder:7b")] {
            let launch = ClaudeAgent::new().launch_command(&wiring("usagi", model), false, None);
            assert!(launch.contains("--settings '{\"hooks\":"));
            assert!(launch.contains("usagi agent-phase ready"));
            assert!(launch.contains("usagi agent-phase running"));
            assert!(launch.contains("usagi agent-phase waiting"));
            assert!(launch.contains("usagi agent-phase ended"));
            // A tool-permission prompt waits too, caught reliably (even while
            // focused) by the dedicated PermissionRequest hook, not just the
            // away-only Notification.
            assert!(launch.contains("\"PermissionRequest\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}]"));
        }
    }

    #[test]
    fn launch_command_embeds_the_given_binary_path_in_hooks_and_mcp() {
        // The caller passes the resolved usagi binary path (e.g. from
        // `current_exe()`); both the MCP servers and every lifecycle hook must
        // invoke that exact path, not the bare name `usagi`, so the wiring works
        // when usagi is run from a build that is not on `$PATH`.
        let launch = ClaudeAgent::new().launch_command(
            &wiring("/opt/usagi/bin/usagi", Some("qwen2.5-coder:7b")),
            false,
            None,
        );
        // MCP servers point at the resolved binary.
        assert!(launch.contains(r#""usagi":{"command":"/opt/usagi/bin/usagi","args":["mcp"]}"#));
        assert!(launch.contains(
            r#""usagi-llm":{"command":"/opt/usagi/bin/usagi","args":["llm-mcp","--model","qwen2.5-coder:7b"]}"#
        ));
        // Every lifecycle hook invokes that same binary.
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase ready"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase running"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase waiting"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase ended"));
        // The bare name no longer appears as a standalone command.
        assert!(!launch.contains(r#""command":"usagi""#));
    }

    #[test]
    fn launch_command_json_escapes_a_windows_binary_path() {
        // A Windows path carries backslashes; serde_json doubles them so the
        // `--mcp-config` / `--settings` JSON stays valid.
        let launch =
            ClaudeAgent::new().launch_command(&wiring(r"C:\usagi\usagi.exe", None), false, None);
        assert!(launch.contains(r#""command":"C:\\usagi\\usagi.exe","args":["mcp"]"#));
        assert!(launch.contains(r"C:\\usagi\\usagi.exe agent-phase running"));
    }

    #[test]
    fn project_dir_name_replaces_non_alphanumerics_with_dashes() {
        // Both the path separators and the dot of `.usagi` collapse to `-`,
        // matching Claude Code's own transcript directory naming.
        assert_eq!(
            claude_project_dir_name(Path::new("/Users/a/proj.x/.usagi")),
            "-Users-a-proj-x--usagi"
        );
        // Digits and casing are preserved.
        assert_eq!(
            claude_project_dir_name(Path::new("/repo/KKyosuke2")),
            "-repo-KKyosuke2"
        );
    }

    #[test]
    fn has_resumable_session_in_is_true_only_with_a_jsonl_transcript() {
        let root = tempfile::tempdir().unwrap();
        let worktree = Path::new("/some/worktree");
        let project_dir = root.path().join(claude_project_dir_name(worktree));

        // No transcript directory yet → nothing to resume.
        assert!(!has_resumable_session_in(root.path(), worktree));

        // An empty transcript directory still has nothing to resume.
        fs::create_dir_all(&project_dir).unwrap();
        assert!(!has_resumable_session_in(root.path(), worktree));

        // A non-transcript file is ignored.
        fs::write(project_dir.join("notes.txt"), "x").unwrap();
        assert!(!has_resumable_session_in(root.path(), worktree));

        // A `.jsonl` transcript means Claude has a conversation to continue.
        fs::write(project_dir.join("session.jsonl"), "{}").unwrap();
        assert!(has_resumable_session_in(root.path(), worktree));
    }

    #[test]
    fn has_resumable_session_resolves_against_the_real_home() {
        // Exercises the home-directory wrapper end to end: a worktree that has
        // never run an agent has no transcript, so it is not resumable.
        let agent = ClaudeAgent::new();
        assert!(!agent.has_resumable_session(Path::new("/nonexistent/usagi/worktree")));
    }

    #[test]
    fn forget_session_in_deletes_the_whole_transcript_directory() {
        let root = tempfile::tempdir().unwrap();
        let worktree = Path::new("/some/worktree");
        let project_dir = root.path().join(claude_project_dir_name(worktree));
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("session.jsonl"), "{}").unwrap();
        assert!(has_resumable_session_in(root.path(), worktree));

        // Forgetting drops the transcripts, so nothing is resumable afterwards.
        forget_session_in(root.path(), worktree);
        assert!(!project_dir.exists());
        assert!(!has_resumable_session_in(root.path(), worktree));

        // Forgetting again, with the directory already gone, is a harmless no-op.
        forget_session_in(root.path(), worktree);
    }

    #[test]
    fn forget_session_resolves_against_the_real_home() {
        // Exercises the home-directory wrapper end to end: forgetting a worktree
        // that never ran an agent is a no-op (its transcript dir does not exist).
        let agent = ClaudeAgent::new();
        agent.forget_session(Path::new("/nonexistent/usagi/worktree"));
    }
}
