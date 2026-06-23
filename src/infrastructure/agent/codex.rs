//! Codex CLI adapter.
//!
//! Wires usagi into Codex through its `-c key=value` config overrides (the same
//! dotted-path overrides Codex would otherwise read from `~/.codex/config.toml`):
//!
//! - **MCP servers** — the unified `usagi` server, plus the `usagi-llm` server
//!   when the local LLM is enabled (`mcp_servers.<name>.command` / `.args`).
//! - **Lifecycle hooks** — Codex's hook events (`SessionStart`, `UserPromptSubmit`,
//!   `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`) each run
//!   `<usagi_bin> agent-phase <phase>`, so the agent reports its own
//!   ready / running / waiting / ended state instead of usagi guessing from the
//!   terminal bell. Codex delivers the hook payload on stdin with the same `cwd`
//!   and `source` fields Claude Code uses, so `usagi agent-phase` records the
//!   phase for the right worktree with no Codex-specific handling. Because these
//!   are non-managed command hooks, the launch passes
//!   `--dangerously-bypass-hook-trust` so they run without an interactive trust
//!   prompt (usagi vets the hook command — it only ever runs usagi itself).
//!
//! A queued opening prompt rides along as Codex's positional `[PROMPT]` argument
//! so the session opens already working on it.
//!
//! Codex keeps its own session store that usagi does not mirror, so it reports no
//! resumable session and keeps no conversation to forget; launches are always
//! fresh.

use std::path::Path;

use crate::domain::agent::{Agent, AgentWiring};

/// Codex hook events wired back into usagi, paired with the phase each reports.
///
/// `SessionStart` → `ready` (idle start; the compaction guard in
/// [`crate::usecase::agent_phase`] keys off the payload's `source`, which Codex
/// sets to `startup` / `resume` / `clear` / `compact` exactly as Claude does).
/// `UserPromptSubmit` / `PreToolUse` / `PostToolUse` → `running` (a turn started,
/// and every mid-turn tool call re-asserts `running` so a session resumes out of
/// `waiting` once the user answers). `PermissionRequest` → `waiting` (paused for
/// the user). `Stop` → `ended` (the turn finished).
///
/// Events deliberately left unwired mirror Claude's: `SubagentStop` (the main
/// turn keeps going), `PreCompact` / `PostCompact` (handled by the `SessionStart`
/// guard, and a post-compaction tool call re-asserts `running` anyway).
const HOOK_PHASES: [(&str, &str); 6] = [
    ("SessionStart", "ready"),
    ("UserPromptSubmit", "running"),
    ("PreToolUse", "running"),
    ("PostToolUse", "running"),
    ("PermissionRequest", "waiting"),
    ("Stop", "ended"),
];

/// Wrap `text` as a single shell argument in single quotes, safe to drop into a
/// `sh -c` command line. A single quote cannot appear inside a single-quoted
/// string, so each one is rendered as `'\''` (close the quote, an escaped quote,
/// reopen) — the standard POSIX idiom. Everything else (newlines, `$`, spaces,
/// the `[`, `]`, `"` of a TOML value …) is literal inside single quotes, so Codex
/// receives the argument verbatim.
fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Render `text` as a TOML basic string (double-quoted), escaping the backslash
/// and double-quote that TOML treats specially. Used for the hook command, whose
/// embedded usagi binary path may carry backslashes on Windows; the surrounding
/// `-c` argument is single-quoted for the shell, so the double quotes here pass
/// through untouched.
fn toml_basic_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// One `-c <assignment>` config override, shell-quoted as a single argument so
/// the shell hands Codex the assignment verbatim.
fn dash_c(assignment: &str) -> String {
    format!("-c {}", shell_single_quote(assignment))
}

/// A `-c <key>=<value>` MCP override. Codex parses the value as TOML and falls
/// back to the raw string when that fails, so a command *path* is passed bare
/// (`…command=/opt/usagi`): a path is not valid TOML, so Codex keeps it as a
/// literal string — which sidesteps TOML escaping for awkward paths (spaces,
/// Windows backslashes). An args *array* is passed as TOML (`…args=["mcp"]`)
/// because it must parse as a list.
fn config_override(key: &str, value: &str) -> String {
    dash_c(&format!("{key}={value}"))
}

/// Render a Codex args array as a TOML inline array of basic strings, e.g.
/// `["llm-mcp","--model","qwen2.5-coder:7b"]`. The elements here come from fixed
/// usagi wiring (subcommand names and a model from the allowlist), none of which
/// contain a quote or backslash, so they need no escaping beyond the quotes.
fn toml_string_array(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("\"{item}\"")).collect();
    format!("[{}]", quoted.join(","))
}

/// A `-c` override wiring one lifecycle hook: `event` fires
/// `<usagi_bin> agent-phase <phase>` via a single matcher-less command handler,
/// e.g. `hooks.Stop=[{hooks=[{type="command",command="usagi agent-phase ended"}]}]`.
/// The matcher is omitted so the hook matches every occurrence of the event.
fn hook_override(usagi_bin: &str, event: &str, phase: &str) -> String {
    let command = toml_basic_string(&format!("{usagi_bin} agent-phase {phase}"));
    config_override(
        &format!("hooks.{event}"),
        &format!("[{{hooks=[{{type=\"command\",command={command}}}]}}]"),
    )
}

/// The Codex CLI adapter.
#[derive(Default)]
pub struct CodexAgent;

impl CodexAgent {
    /// A Codex adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Agent for CodexAgent {
    fn program(&self) -> &'static str {
        "codex"
    }

    fn launch_command(
        &self,
        wiring: &AgentWiring,
        _resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        let bin = &wiring.usagi_bin;
        // The hooks are non-managed command hooks, so Codex would otherwise prompt
        // to trust each one; usagi vets them (they only run usagi itself), so the
        // bypass flag lets them run unattended.
        let mut parts = vec![
            "codex".to_string(),
            "--dangerously-bypass-hook-trust".to_string(),
        ];
        // The unified usagi MCP server is always wired in (issues, memories,
        // sessions); the local-LLM server joins it when enabled.
        parts.push(config_override("mcp_servers.usagi.command", bin));
        parts.push(config_override(
            "mcp_servers.usagi.args",
            &toml_string_array(&["mcp"]),
        ));
        if let Some(model) = wiring.local_llm_model.as_deref() {
            parts.push(config_override("mcp_servers.usagi-llm.command", bin));
            parts.push(config_override(
                "mcp_servers.usagi-llm.args",
                &toml_string_array(&["llm-mcp", "--model", model]),
            ));
        }
        // Lifecycle hooks report the agent's phase back to usagi.
        for (event, phase) in HOOK_PHASES {
            parts.push(hook_override(bin, event, phase));
        }
        // A queued prompt rides along as Codex's positional query, so the agent
        // opens already working on it. It is arbitrary user text, so it is escaped
        // for the single-quoted shell context. Placed last as the trailing
        // argument. Codex has no usagi-driven resume, so `resume` is ignored.
        if let Some(prompt) = initial_prompt {
            parts.push(shell_single_quote(prompt));
        }
        parts.join(" ")
    }

    fn has_resumable_session(&self, _dir: &Path) -> bool {
        // Codex keeps its own session store that usagi does not mirror, so usagi
        // never drives a resume — launches are always fresh.
        false
    }

    fn forget_session(&self, _dir: &Path) {
        // usagi keeps no Codex conversation store, so there is nothing to clear.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;

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
    fn launch_command_wires_in_the_usagi_mcp_server() {
        // With the local LLM off the unified usagi server is wired in via Codex's
        // `-c` config overrides — the command path bare (literal-string fallback)
        // and the args as a TOML array.
        let launch = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("-c 'mcp_servers.usagi.args=[\"mcp\"]'"));
        // The local-LLM server is absent when no model is given.
        assert!(!launch.contains("usagi-llm"));
    }

    #[test]
    fn launch_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the usagi server in the
        // overrides, carrying the `llm-mcp --model <model>` args as a TOML array.
        let launch = CodexAgent::new().launch_command(
            &wiring("usagi", Some("qwen2.5-coder:7b")),
            false,
            None,
        );
        assert!(launch.contains("-c 'mcp_servers.usagi-llm.command=usagi'"));
        assert!(launch.contains(
            "-c 'mcp_servers.usagi-llm.args=[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]'"
        ));
    }

    #[test]
    fn launch_command_wires_in_lifecycle_hooks() {
        // Each lifecycle hook rides along as a `-c hooks.<Event>` override running
        // `usagi agent-phase <phase>`, whether or not the local LLM is enabled, so
        // usagi always learns the agent's state.
        for model in [None, Some("qwen2.5-coder:7b")] {
            let launch = CodexAgent::new().launch_command(&wiring("usagi", model), false, None);
            // The trust bypass lets the non-managed command hooks run unattended.
            assert!(launch.contains("codex --dangerously-bypass-hook-trust "));
            // SessionStart → ready; a turn and every mid-turn tool call → running.
            assert!(launch.contains(
                "-c 'hooks.SessionStart=[{hooks=[{type=\"command\",command=\"usagi agent-phase ready\"}]}]'"
            ));
            assert!(launch.contains(
                "-c 'hooks.UserPromptSubmit=[{hooks=[{type=\"command\",command=\"usagi agent-phase running\"}]}]'"
            ));
            assert!(launch.contains(
                "-c 'hooks.PreToolUse=[{hooks=[{type=\"command\",command=\"usagi agent-phase running\"}]}]'"
            ));
            assert!(launch.contains(
                "-c 'hooks.PostToolUse=[{hooks=[{type=\"command\",command=\"usagi agent-phase running\"}]}]'"
            ));
            // A permission prompt waits; the turn finishing ends the session.
            assert!(launch.contains(
                "-c 'hooks.PermissionRequest=[{hooks=[{type=\"command\",command=\"usagi agent-phase waiting\"}]}]'"
            ));
            assert!(launch.contains(
                "-c 'hooks.Stop=[{hooks=[{type=\"command\",command=\"usagi agent-phase ended\"}]}]'"
            ));
        }
    }

    #[test]
    fn launch_command_embeds_the_given_binary_path() {
        // The caller passes the resolved usagi binary path (e.g. from
        // `current_exe()`); both the MCP overrides and every hook command must
        // invoke that exact path, not the bare name, so the wiring works when
        // usagi is run from a build not on `$PATH`.
        let launch = CodexAgent::new().launch_command(
            &wiring("/opt/usagi/bin/usagi", Some("qwen2.5-coder:7b")),
            false,
            None,
        );
        assert!(launch.contains("-c 'mcp_servers.usagi.command=/opt/usagi/bin/usagi'"));
        assert!(launch.contains("-c 'mcp_servers.usagi-llm.command=/opt/usagi/bin/usagi'"));
        // Every hook invokes that same binary.
        assert!(launch.contains("command=\"/opt/usagi/bin/usagi agent-phase ready\""));
        assert!(launch.contains("command=\"/opt/usagi/bin/usagi agent-phase ended\""));
        // The bare name no longer appears as a standalone override value.
        assert!(!launch.contains("command=usagi'"));
    }

    #[test]
    fn launch_command_toml_escapes_a_windows_binary_path_in_hooks() {
        // A Windows path carries backslashes. In the MCP command override it rides
        // bare (literal-string fallback, no escaping); inside a hook command it is
        // a TOML basic string, so each backslash is doubled to stay valid TOML.
        let launch =
            CodexAgent::new().launch_command(&wiring(r"C:\usagi\usagi.exe", None), false, None);
        // MCP command: bare, backslashes intact.
        assert!(launch.contains(r"-c 'mcp_servers.usagi.command=C:\usagi\usagi.exe'"));
        // Hook command: TOML basic string with doubled backslashes.
        assert!(launch.contains(r#"command="C:\\usagi\\usagi.exe agent-phase ready""#));
    }

    #[test]
    fn launch_command_appends_an_initial_prompt_as_the_trailing_query() {
        // A queued prompt rides along as Codex's positional query so the agent
        // opens already working on it. It is the trailing, single-quoted argument;
        // the wiring before it is unchanged.
        let launch =
            CodexAgent::new().launch_command(&wiring("usagi", None), false, Some("fix issue #50"));
        assert!(launch.ends_with(" 'fix issue #50'"));
        // With no prompt the trailing query is absent: the command is exactly the
        // prompt-carrying one with its ` '…'` suffix stripped.
        let plain = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains("fix issue #50"));
        assert_eq!(launch, format!("{plain} 'fix issue #50'"));
    }

    #[test]
    fn launch_command_escapes_single_quotes_in_an_initial_prompt() {
        // Arbitrary user prompt text may contain single quotes, which would
        // otherwise break out of the shell argument. Each is rendered as the POSIX
        // `'\''` idiom so the agent receives the prompt verbatim.
        let launch = CodexAgent::new().launch_command(
            &wiring("usagi", None),
            false,
            Some("don't break 'this'"),
        );
        assert!(launch.ends_with(r" 'don'\''t break '\''this'\'''"));
    }

    #[test]
    fn launch_command_ignores_resume() {
        // Codex has no usagi-driven resume, so requesting one launches identically
        // to a fresh start, and it never reports a resumable session.
        let fresh = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        let resumed = CodexAgent::new().launch_command(&wiring("usagi", None), true, None);
        assert_eq!(fresh, resumed);
        assert!(!CodexAgent::new().has_resumable_session(Path::new("/any/worktree")));
        // Forgetting a session is a no-op for Codex (no conversation store).
        CodexAgent::new().forget_session(Path::new("/any/worktree"));
    }

    #[test]
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // An embedded single quote closes, escapes, and reopens the quoting.
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn toml_basic_string_escapes_backslash_and_quote() {
        assert_eq!(toml_basic_string("plain"), "\"plain\"");
        assert_eq!(toml_basic_string(r"a\b"), r#""a\\b""#);
        assert_eq!(toml_basic_string(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn default_agent_matches_new() {
        // The Settings-driven wiring path uses the default constructor; it behaves
        // the same as `new`.
        let launch =
            CodexAgent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("codex --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
    }
}
