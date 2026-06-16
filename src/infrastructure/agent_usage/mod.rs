//! Reading agent context-window usage from each CLI's on-disk transcript.
//!
//! The home screen's session watcher polls a [`UsageReader`] for every live
//! session to drive the sidebar's aggregate usage gauge. The reader is chosen
//! per the configured agent CLI via [`reader_for`], so a new agent only needs
//! its own [`UsageReader`] implementation wired in here — the call sites never
//! change.
//!
//! The Claude reader's transcript discovery and file reads are I/O (excluded
//! from coverage, like the rest of the embedded-terminal plumbing); the pieces
//! that turn bytes into a [`AgentUsage`] — [`parse_claude_transcript`] and
//! [`encode_project_dir`] — are pure and tested here.

mod claude;
mod gemini;

use std::path::Path;

pub use claude::ClaudeUsageReader;
pub use gemini::GeminiUsageReader;

use crate::domain::agent_usage::{context_window_for, AgentUsage, UsageReader};
use crate::domain::settings::AgentCli;

/// The usage reader for the configured agent CLI.
pub fn reader_for(cli: AgentCli) -> Box<dyn UsageReader> {
    match cli {
        AgentCli::Claude => Box::new(ClaudeUsageReader::new()),
        AgentCli::Gemini => Box::new(GeminiUsageReader::new()),
    }
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
    use std::path::PathBuf;

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

    #[test]
    fn reader_for_gemini_reports_no_usage_yet() {
        // Gemini has no transcript usagi reads, so its reader returns None for any
        // worktree — the dispatch still hands back a working reader.
        let reader = reader_for(AgentCli::Gemini);
        assert_eq!(reader.read(&PathBuf::from("/anywhere")), None);
    }

    #[test]
    fn reader_for_claude_returns_a_reader() {
        // A worktree with no transcript reads as None rather than panicking.
        let reader = reader_for(AgentCli::Claude);
        assert_eq!(
            reader.read(&PathBuf::from("/nonexistent/usagi/worktree/path")),
            None
        );
    }
}
