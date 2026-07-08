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

use super::util::shell_single_quote;
use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

/// One MCP server entry: the program to run and its arguments.
#[derive(Serialize)]
struct McpServer {
    command: String,
    args: Vec<String>,
}

/// The `"mcpServers"` map wired into Claude: always the unified `usagi` server
/// (`<usagi_bin> mcp`) so the agent can manage issues, memories and sessions;
/// plus the optional `usagi-llm` server when its setting is enabled. Field order
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
/// `ready`); a submitted prompt starts a turn (`UserPromptSubmit` → `running`).
/// Once a turn is underway every tool call is a sign the agent is actively
/// working, so a tool about to run or having just run is also `running`
/// (`PreToolUse` / `PostToolUse` → `running`). This is what un-sticks a session
/// after a wait: when the user answers a question or grants a permission, the
/// agent resumes with no fresh `UserPromptSubmit`, but its next tool call fires
/// `PreToolUse`/`PostToolUse` and pulls the session back out of `waiting` into
/// `running`. These hooks only fire mid-turn, so they never invent a false
/// `running` for an idle session.
///
/// Finishing a turn means the agent is done (`Stop` → `ended`); pausing mid-turn
/// for the user's input or permission means it waits (`Notification` →
/// `waiting`). A tool-permission prompt is also a wait, but `Notification` only
/// fires for it when the user is away; the dedicated `PermissionRequest` →
/// `waiting` hook catches it reliably (it fires right as the prompt appears, even
/// while the session is focused). The session ending is also done (`SessionEnd` →
/// `ended`).
///
/// Hooks deliberately left unwired: `SubagentStop` (a finished subagent does not
/// end the main turn — the main agent keeps working, and its own `PostToolUse`
/// for the `Task` tool already holds it `running`); `PreCompact` / `PostCompact`
/// (compaction is handled by the `SessionStart` guard in
/// [`crate::usecase::agent_phase`], and a post-compaction tool call re-asserts
/// `running` anyway).
#[derive(Serialize)]
struct Hooks {
    #[serde(rename = "UserPromptSubmit")]
    user_prompt_submit: Vec<HookEntry>,
    #[serde(rename = "PreToolUse")]
    pre_tool_use: Vec<HookEntry>,
    #[serde(rename = "PostToolUse")]
    post_tool_use: Vec<HookEntry>,
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
    // Before any tool runs, also confine the agent to its session worktree: the
    // guard denies a Read / Edit / Write whose target escapes the worktree (the
    // repo root and sibling worktrees sit just above it on disk). It rides
    // alongside the phase reporter — Claude runs every `PreToolUse` hook, and a
    // deny from any one blocks the call.
    let mut pre_tool_use = phase("running");
    pre_tool_use.push(HookEntry {
        hooks: vec![HookCommand {
            kind: "command",
            command: format!("{usagi_bin} guard-workspace"),
        }],
    });
    let settings = HookSettings {
        hooks: Hooks {
            user_prompt_submit: phase("running"),
            pre_tool_use,
            post_tool_use: phase("running"),
            stop: phase("ended"),
            notification: phase("waiting"),
            permission_request: phase("waiting"),
            session_start: phase("ready"),
            session_end: phase("ended"),
        },
    };
    serde_json::to_string(&settings).expect("hook settings serialize to JSON")
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
        // The program name lives once in the domain (`AgentCli::command`); the
        // adapter reads it from there rather than re-spelling the literal.
        AgentCli::Claude.command()
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
        let system_prompt = super::session_system_prompt(wiring.is_root, false, local_llm_model);
        let hooks = claude_hooks_settings(&wiring.usagi_bin);
        // Every wiring value is escaped for the single-quoted `sh -c` context via
        // [`shell_single_quote`] rather than being wrapped in bare `'…'`. The JSON
        // blobs and system prompt are normally quote-free, but a single quote can
        // reach them through values they embed — a `usagi_bin` path containing an
        // apostrophe (e.g. `/Users/o'brien/bin/usagi`), or a `local_llm.model` from
        // a hand-edited `settings.json`. Bare quoting would let such a quote break
        // out of the shell argument, so we always escape. `--continue` resumes the
        // most recent conversation in the worktree; placed right after the program
        // name so it reads like a plain `claude -c` with usagi's wiring appended.
        let resume_flag = if resume { "--continue " } else { "" };
        // An explicit model rides in as Claude's `--model`; absent, Claude uses its
        // own configured default. Escaped like every other wiring value.
        let model_flag = match wiring.model.as_deref() {
            Some(model) => format!("--model {} ", shell_single_quote(model)),
            None => String::new(),
        };
        // A queued prompt rides along as Claude's positional query, so the agent
        // opens interactively already working on it. Placed last so it is the
        // trailing argument, behind a `--` end-of-options marker: the prompt is
        // arbitrary user text (MCP `session_prompt`), so one
        // starting with `-` (e.g. `--help`) must reach the CLI as the query,
        // not be parsed as a flag that aborts the launch.
        let prompt_arg = match initial_prompt {
            Some(prompt) => format!(" -- {}", shell_single_quote(prompt)),
            None => String::new(),
        };
        let mcp_config = shell_single_quote(&mcp_config);
        let system_prompt = shell_single_quote(&system_prompt);
        let hooks = shell_single_quote(&hooks);
        format!(
            "claude {resume_flag}{model_flag}--mcp-config {mcp_config} \
             --append-system-prompt {system_prompt} \
             --settings {hooks}{prompt_arg}"
        )
    }

    fn headless_command(&self, wiring: &AgentWiring, prompt: &str) -> String {
        // Claude's headless mode is `claude -p <prompt>` (print mode: run the
        // prompt and exit). usagi's MCP servers are wired in exactly as in the
        // interactive launch via `--mcp-config`, so the agent can drive usagi
        // (session_list / session_remove …) while it works. No interactive person
        // is present, so `--dangerously-skip-permissions` lets it act (delete
        // worktrees, run git) without approval prompts. Lifecycle hooks are
        // omitted: a headless run reports no phase to watch.
        let mcp_config = mcp_config_json(wiring.local_llm_model.as_deref(), &wiring.usagi_bin);
        let mcp_config = shell_single_quote(&mcp_config);
        let prompt = shell_single_quote(prompt);
        let model_flag = match wiring.model.as_deref() {
            Some(model) => format!("--model {} ", shell_single_quote(model)),
            None => String::new(),
        };
        format!(
            "claude -p {prompt} {model_flag}--dangerously-skip-permissions --mcp-config {mcp_config}"
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
            model: None,
            is_root: false,
            sandbox_writable_roots: Vec::new(),
        }
    }

    #[test]
    fn launch_command_renders_the_model_flag_only_when_a_model_is_set() {
        // Default (no model): Claude is launched without `--model`, on its own
        // configured default.
        let plain = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains("--model"), "{plain}");

        // With a model set on the wiring, it rides in as Claude's `--model`, and
        // headless launches carry it too.
        let mut w = wiring("usagi", None);
        w.model = Some("opus".to_string());
        let launch = ClaudeAgent::new().launch_command(&w, false, None);
        assert!(launch.contains("--model 'opus'"), "{launch}");
        let headless = ClaudeAgent::new().headless_command(&w, "clean up");
        assert!(headless.contains("--model 'opus'"), "{headless}");
    }

    #[test]
    fn launch_command_wires_in_the_usagi_mcp_servers() {
        // With the local LLM off (`None`), the unified usagi server is wired in
        // and the system prompt is just the worktree note.
        let launch = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        // The program is `claude`, with usagi's MCP server passed inline via
        // `--mcp-config` and a session-scoped instruction passed via
        // `--append-system-prompt` (both single-quoted so the shell keeps them).
        let tokens = shell_words::split(&launch).expect("launch line is well-formed shell");
        assert_eq!(tokens.len(), 7);
        assert_eq!(tokens[0], "claude");
        assert_eq!(tokens[1], "--mcp-config");
        assert_eq!(
            tokens[2],
            "{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}"
        );
        assert_eq!(tokens[3], "--append-system-prompt");
        assert_eq!(
            tokens[4],
            super::super::session_system_prompt(false, false, None)
        );
        assert_eq!(tokens[5], "--settings");
        assert_eq!(
            tokens[6],
            "{\"hooks\":{\"UserPromptSubmit\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]}],\"PreToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]},{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi guard-workspace\"}]}],\"PostToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]}],\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}],\"Notification\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}],\"PermissionRequest\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}],\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ready\"}]}],\"SessionEnd\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}]}}"
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
        // opens already working on it. It is the trailing, single-quoted argument
        // behind a `--` end-of-options marker (so a `-`-leading prompt cannot be
        // parsed as a flag); the wiring before it is unchanged.
        let launch =
            ClaudeAgent::new().launch_command(&wiring("usagi", None), false, Some("fix issue #50"));
        assert!(launch.ends_with(" -- 'fix issue #50'"));
        // The wiring is still present and the program still starts plainly.
        assert!(launch.starts_with("claude --mcp-config '"));
        assert!(launch.contains("--append-system-prompt '"));
        // With no prompt the trailing query is absent: the command is exactly the
        // prompt-carrying one with its ` -- '…'` suffix stripped.
        let plain = ClaudeAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains("fix issue #50"));
        assert_eq!(launch, format!("{plain} -- 'fix issue #50'"));
        // A dash-leading prompt (`--help`) stays behind the separator as the
        // positional query instead of aborting the launch as an unknown flag.
        let dashed =
            ClaudeAgent::new().launch_command(&wiring("usagi", None), false, Some("--help"));
        assert!(dashed.ends_with(" -- '--help'"));
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
            // Every tool call mid-turn re-asserts `running`, so a session resumes
            // out of `waiting` once the user answers a prompt or grants a
            // permission (no fresh UserPromptSubmit fires then). The PreToolUse
            // array carries a second entry — the worktree guard — right after it.
            assert!(launch.contains("\"PreToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]},{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi guard-workspace\"}]}]"));
            assert!(launch.contains("\"PostToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]}]"));
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
        // The worktree guard hook runs the same resolved binary too.
        assert!(launch.contains("/opt/usagi/bin/usagi guard-workspace"));
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
    fn launch_command_escapes_single_quotes_in_the_wiring() {
        // A single quote can reach the wiring JSON through a binary path whose
        // owning user has an apostrophe in their name (e.g. `/Users/o'brien/...`)
        // or through a hand-edited `local_llm.model`. The whole `--mcp-config` /
        // `--settings` / `--append-system-prompt` blobs must be escaped for the
        // single-quoted shell context — a bare `'…'` wrap would let the quote
        // break out and inject a command into the `sh -c` line.
        let launch = ClaudeAgent::new().launch_command(
            &wiring("/Users/o'brien/bin/usagi", Some("evil';touch /tmp/pwned;'")),
            false,
            None,
        );
        // Tokenizing the line the way `sh` would proves the quotes did not break
        // out: a successful injection would split into extra tokens (e.g. a
        // standalone `touch`/`/tmp/pwned`). Instead the malicious payload stays
        // sealed inside the `--mcp-config` argument as inert JSON text.
        let tokens = shell_words::split(&launch).expect("launch line is well-formed shell");
        assert_eq!(tokens[0], "claude");
        assert_eq!(tokens[1], "--mcp-config");
        let mcp: serde_json::Value =
            serde_json::from_str(&tokens[2]).expect("the --mcp-config token is intact JSON");
        assert_eq!(
            mcp["mcpServers"]["usagi-llm"]["args"][2],
            "evil';touch /tmp/pwned;'"
        );
        assert_eq!(
            mcp["mcpServers"]["usagi"]["command"],
            "/Users/o'brien/bin/usagi"
        );
        // No token is the bare injected command — it never escaped the JSON.
        assert!(!tokens.iter().any(|t| t == "touch" || t == "/tmp/pwned"));
    }

    #[test]
    fn headless_command_runs_print_mode_with_the_usagi_mcp_server() {
        // The headless command runs Claude in print mode (`-p <prompt>`) with the
        // permission bypass and usagi's MCP server wired in via `--mcp-config`, so
        // the background agent can drive usagi (session_list / session_remove …)
        // unattended.
        let launch = ClaudeAgent::new().headless_command(&wiring("usagi", None), "clean up");
        assert_eq!(
            launch,
            "claude -p 'clean up' --dangerously-skip-permissions \
             --mcp-config '{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}'"
        );
    }

    #[test]
    fn headless_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the usagi server in the
        // headless MCP config too.
        let launch = ClaudeAgent::new()
            .headless_command(&wiring("usagi", Some("qwen2.5-coder:7b")), "clean up");
        assert!(launch.contains(
            "\"usagi-llm\":{\"command\":\"usagi\",\"args\":[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]}"
        ));
        assert!(launch.contains("\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}"));
    }

    #[test]
    fn headless_command_escapes_single_quotes_in_the_prompt_and_wiring() {
        // The prompt is arbitrary text and the binary path can carry an apostrophe;
        // both are escaped so neither can break out of the single-quoted shell
        // argument and inject a command into the `sh -c` line.
        let launch = ClaudeAgent::new().headless_command(
            &wiring("/Users/o'brien/bin/usagi", None),
            "don't delete 'main'",
        );
        let tokens = shell_words::split(&launch).expect("headless line is well-formed shell");
        assert_eq!(tokens[0], "claude");
        assert_eq!(tokens[1], "-p");
        assert_eq!(tokens[2], "don't delete 'main'");
        assert_eq!(tokens[3], "--dangerously-skip-permissions");
        assert_eq!(tokens[4], "--mcp-config");
        let mcp: serde_json::Value =
            serde_json::from_str(&tokens[5]).expect("the --mcp-config token is intact JSON");
        assert_eq!(
            mcp["mcpServers"]["usagi"]["command"],
            "/Users/o'brien/bin/usagi"
        );
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
