//! Agent adapters: one per CLI, each converting usagi's [`Agent`] interface into
//! that CLI's own invocation and on-disk artifacts.
//!
//! [`agent_for`] is the single place that maps the configured [`AgentCli`] to its
//! adapter, so adding a new agent is one adapter module plus one arm here. The
//! adapters' I/O (reading a transcript) lives in their own files and is excluded
//! from coverage; the pure pieces — the Claude launch-command rendering and
//! transcript parsing — live here and are tested directly.

mod claude;
mod gemini;

use std::path::Path;
use std::sync::Arc;

pub use claude::ClaudeAgent;
pub use gemini::GeminiAgent;

use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::agent_usage::{context_window_for, AgentUsage};
use crate::domain::settings::AgentCli;

/// The agent adapter for the configured CLI, shared (via `Arc`) between the
/// render loop and the background session watcher.
pub fn agent_for(cli: AgentCli) -> Arc<dyn Agent> {
    match cli {
        AgentCli::Claude => Arc::new(ClaudeAgent::new()),
        AgentCli::Gemini => Arc::new(GeminiAgent::new()),
    }
}

/// Render usagi's [`AgentWiring`] into Claude Code's launch command.
///
/// Claude accepts the MCP servers inline via `--mcp-config` and the
/// session-scoped instruction via `--append-system-prompt`; both are
/// single-quoted so the shell passes them through verbatim (no value contains a
/// single quote). The MCP config is built by string formatting — the values come
/// from usagi's own fixed wiring and the local-LLM model allowlist, so nothing
/// needs JSON escaping.
pub(crate) fn claude_launch_command(wiring: &AgentWiring) -> String {
    let servers = wiring
        .mcp_servers
        .iter()
        .map(|server| {
            let args = server
                .args
                .iter()
                .map(|arg| format!("\"{arg}\""))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "\"{}\":{{\"command\":\"{}\",\"args\":[{}]}}",
                server.name, server.command, args
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let mcp_config = format!("{{\"mcpServers\":{{{servers}}}}}");
    format!(
        "claude --mcp-config '{mcp_config}' \
         --append-system-prompt '{}'",
        wiring.system_prompt
    )
}

/// Parse the latest context-window usage from a Claude Code transcript.
///
/// A transcript is JSONL — one JSON object per line. Each assistant turn carries
/// a `message.usage` block; the **last** such block reflects how full the
/// context window currently is, so we scan from the end and take the first line
/// that reports a non-zero `usage`. Occupancy is the request's `input_tokens`
/// plus its cache-read and cache-creation tokens (everything fed to the model on
/// that turn). Lines without a usage block (user messages, tool results, …) and
/// malformed lines are skipped; an empty or usage-less transcript yields `None`.
pub(crate) fn parse_claude_transcript(contents: &str) -> Option<AgentUsage> {
    for line in contents.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("message") else {
            continue;
        };
        let Some(usage) = message.get("usage") else {
            continue;
        };
        let token = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let used = token("input_tokens")
            + token("cache_read_input_tokens")
            + token("cache_creation_input_tokens");
        if used == 0 {
            continue;
        }
        let model = message
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return Some(AgentUsage::new(used, context_window_for(model)));
    }
    None
}

/// Encode a worktree path the way Claude Code names its per-project directory
/// under `~/.claude/projects`: every byte that is not an ASCII letter or digit
/// becomes `-` (so `/a/.b_c` → `-a--b-c`).
pub(crate) fn encode_project_dir(worktree: &Path) -> String {
    worktree
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_usage::{DEFAULT_CONTEXT_WINDOW, EXTENDED_CONTEXT_WINDOW};
    use crate::domain::settings::Settings;
    use std::path::PathBuf;

    #[test]
    fn claude_launch_command_matches_the_expected_invocation() {
        // The default wiring (issue server only) renders the exact command line
        // `:agent` sends to the shell.
        let wiring = Settings::default().agent_wiring();
        assert_eq!(
            claude_launch_command(&wiring),
            "claude --mcp-config '{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}' \
             --append-system-prompt 'あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。'"
        );
    }

    #[test]
    fn claude_launch_command_appends_the_local_llm_server_when_wired() {
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        settings.local_llm.model = "qwen2.5-coder:7b".to_string();
        let launch = claude_launch_command(&settings.agent_wiring());
        assert!(launch.contains("\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}"));
        assert!(launch.contains(
            "\"usagi-llm\":{\"command\":\"usagi\",\"args\":[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]}"
        ));
        assert!(launch.contains("local_llm_ask"));
    }

    #[test]
    fn agent_for_gemini_reports_no_usage_yet() {
        // Gemini has no transcript usagi reads, so its adapter returns None for
        // any worktree — the dispatch still hands back a working agent.
        let agent = agent_for(AgentCli::Gemini);
        assert_eq!(agent.program(), "gemini");
        assert_eq!(agent.usage(&PathBuf::from("/anywhere")), None);
    }

    #[test]
    fn agent_for_claude_returns_an_agent() {
        // A worktree with no transcript reads as None rather than panicking.
        let agent = agent_for(AgentCli::Claude);
        assert_eq!(agent.program(), "claude");
        assert_eq!(
            agent.usage(&PathBuf::from("/nonexistent/usagi/worktree/path")),
            None
        );
    }

    #[test]
    fn encode_project_dir_replaces_non_alphanumerics_with_dashes() {
        assert_eq!(
            encode_project_dir(&PathBuf::from("/Users/k/git/repo/.usagi/sessions/x")),
            "-Users-k-git-repo--usagi-sessions-x"
        );
    }

    #[test]
    fn parse_takes_the_last_usage_and_sums_input_plus_cache() {
        let transcript = concat!(
            r#"{"type":"user","message":{"role":"user"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":100}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":2,"cache_read_input_tokens":41783,"cache_creation_input_tokens":21845,"output_tokens":1264}}}"#,
            "\n",
        );
        let usage = parse_claude_transcript(transcript).expect("a usage line is present");
        // The last line wins: 2 + 41783 + 21845.
        assert_eq!(usage.used_tokens, 63_630);
        assert_eq!(usage.limit_tokens, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn parse_reads_the_model_for_the_context_window() {
        let transcript =
            r#"{"message":{"model":"claude-opus-4-8[1m]","usage":{"input_tokens":1000}}}"#;
        let usage = parse_claude_transcript(transcript).expect("a usage line is present");
        assert_eq!(usage.used_tokens, 1000);
        assert_eq!(usage.limit_tokens, EXTENDED_CONTEXT_WINDOW);
    }

    #[test]
    fn parse_skips_blank_garbage_and_zero_usage_lines() {
        // The scan runs newest-first, so the skippable lines sit *after* the real
        // usage line here: walking back from the end it steps over a blank line, a
        // zero-usage block, a message with no usage, and a malformed line before
        // it reaches the line that actually reports usage.
        let transcript = concat!(
            r#"{"message":{"usage":{"input_tokens":7}}}"#,
            "\n",
            "not json at all\n",
            r#"{"message":{"role":"user"}}"#, // no usage block
            "\n",
            r#"{"message":{"usage":{"input_tokens":0,"cache_read_input_tokens":0}}}"#, // zero
            "\n",
            "   \n", // blank once trimmed
        );
        let usage = parse_claude_transcript(transcript).expect("the non-zero usage line is found");
        assert_eq!(usage.used_tokens, 7);
    }

    #[test]
    fn parse_returns_none_without_any_usage() {
        assert_eq!(parse_claude_transcript(""), None);
        assert_eq!(parse_claude_transcript("garbage\n{}\n"), None);
    }
}
