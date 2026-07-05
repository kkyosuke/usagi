//! Antigravity CLI (`agy`) adapter.
//!
//! Antigravity — the successor to Google's Gemini CLI — has no inline flag for
//! usagi's MCP servers, lifecycle hooks, or a system-prompt addendum (confirmed by
//! `agy --help`): MCP is configured only through `mcp_config.json` files and agent
//! instructions only through `AGENTS.md` / `GEMINI.md` context files, neither of
//! which usagi writes into the user's config or repository. So the [`AgentWiring`]
//! is not rendered into the launch command and `agy` sessions run without usagi's
//! MCP tools or hook-driven phase reporting (their state is inferred from the
//! terminal bell, like any hook-less agent).
//!
//! What `agy` *does* expose as plain CLI flags, usagi wires in:
//!
//! - **Session worktree note** — because `agy` has no system-prompt flag, the note
//!   that tells the agent it is already inside a usagi worktree (work in place, do
//!   not create another worktree or touch the parent repo) can't ride in out of
//!   band the way it does for Claude/Codex. It leads the opening prompt instead, so
//!   every `agy` launch — with or without a queued prompt — carries it.
//! - **Opening prompt** — a queued `session_prompt` follows the worktree note in
//!   that same `agy -i=<prompt>` (run the prompt, then stay interactive), so the
//!   session opens already working on it.
//! - **Resume** — when a worktree has a prior `agy` conversation, the launch
//!   resumes the most recent one with `agy -c` (`--continue`). usagi decides a
//!   worktree has one by scanning `agy`'s global input-history log
//!   (`~/.gemini/antigravity-cli/history.jsonl`), whose every line records the
//!   `workspace` it ran in — so a line naming the worktree means `agy` has
//!   conversed there. usagi gates `-c` on that signal, so it only continues a
//!   conversation belonging to the current worktree.
//! - **Forget** — on `session remove`, usagi drops the worktree's lines from that
//!   history log, so a session later recreated at the same path is no longer seen
//!   as resumable and starts fresh. (`agy`'s own conversation store is a set of
//!   SQLite databases usagi does not touch; clearing the history index is enough
//!   because usagi only resumes when the index still names the worktree.)

use std::path::{Path, PathBuf};

use super::util::{same_dir, shell_single_quote};
use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

/// Where Antigravity records its global input history:
/// `~/.gemini/antigravity-cli/history.jsonl`. Each line is a JSON object carrying
/// the `workspace` it ran in, so usagi can tell whether a worktree has a prior
/// conversation. `None` when the home directory can't be determined, so usagi
/// simply launches fresh rather than guessing.
fn antigravity_history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".gemini")
            .join("antigravity-cli")
            .join("history.jsonl")
    })
}

/// Where Antigravity looks for its MCP configuration:
/// `~/.gemini/antigravity-cli/mcp_config.json`.
fn antigravity_mcp_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".gemini")
            .join("antigravity-cli")
            .join("mcp_config.json")
    })
}

/// The `workspace` path recorded on a single `history.jsonl` line, or `None` when
/// the line is not JSON or has no string `workspace` field (so malformed lines are
/// ignored rather than aborting the scan).
fn workspace_of(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value
        .get("workspace")?
        .as_str()
        .map(|workspace| workspace.to_string())
}

/// Whether `history` records at least one `agy` run in the worktree at `dir` — a
/// conversation `agy -c` could continue there. A missing history file (no prior
/// run) reads as "nothing to resume", so usagi launches fresh.
fn has_resumable_session_in(history: &Path, dir: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(history) else {
        return false;
    };
    contents
        .lines()
        .filter_map(workspace_of)
        .any(|workspace| same_dir(Path::new(&workspace), dir))
}

/// Drop every `history` line recorded in the worktree at `dir` (best-effort), so a
/// session recreated at the same path later is no longer seen as resumable and
/// starts fresh. The mirror of [`has_resumable_session_in`]: what that finds, this
/// clears. Lines for other worktrees — and any line usagi cannot attribute to a
/// worktree (not JSON, or no `workspace`) — are preserved untouched.
fn forget_session_in(history: &Path, dir: &Path) {
    let Ok(contents) = std::fs::read_to_string(history) else {
        return;
    };
    let kept: Vec<&str> = contents
        .lines()
        .filter(|line| match workspace_of(line) {
            Some(workspace) => !same_dir(Path::new(&workspace), dir),
            None => true,
        })
        .collect();
    // Only rewrite when a line was actually removed, to avoid needless churn (and
    // to leave the file untouched when there was nothing to forget).
    if kept.len() == contents.lines().count() {
        return;
    }
    let mut rewritten = kept.join("\n");
    // A JSON-lines file ends each record (including the last) with a newline; keep
    // that shape unless everything was removed.
    if !rewritten.is_empty() {
        rewritten.push('\n');
    }
    let _ = std::fs::write(history, rewritten);
}

/// The Antigravity CLI adapter.
#[derive(Default)]
pub struct AntigravityAgent;

impl AntigravityAgent {
    /// An Antigravity adapter.
    pub fn new() -> Self {
        Self
    }
}

/// Antigravity's model flag (`--model <model>`) when the wiring pins one, else
/// empty. The model name is escaped for the single-quoted shell context. Shared by
/// the interactive and headless launches.
fn model_flag_parts(wiring: &AgentWiring) -> Vec<String> {
    match wiring.model.as_deref() {
        Some(model) => vec!["--model".to_string(), shell_single_quote(model)],
        None => Vec::new(),
    }
}

impl Agent for AntigravityAgent {
    fn program(&self) -> &'static str {
        // The program name lives once in the domain (`AgentCli::command`); the
        // adapter reads it from there rather than re-spelling the literal.
        AgentCli::Antigravity.command()
    }

    fn launch_command(
        &self,
        wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        // The MCP/hooks wiring is intentionally not rendered: `agy` has no inline
        // flag for it. Only the model (when pinned), resume, and the opening prompt
        // are wired, via plain flags.
        let mut parts = vec!["agy".to_string()];
        // An explicit model rides in as `--model`; absent, `agy` auto-selects.
        parts.extend(model_flag_parts(wiring));
        // `-c` (`--continue`) resumes the most recent conversation. usagi only
        // passes `resume` when the worktree already has one (see
        // `has_resumable_session`), so this continues that worktree's conversation.
        if resume {
            parts.push("-c".to_string());
        }
        // The opening prompt always rides along as `-i=<prompt>` (run it, then stay
        // interactive): `agy` has no system-prompt flag, so the session worktree
        // note leads it, with any queued prompt after a blank line. It is escaped
        // for the single-quoted shell context, and glued to the flag with `=` so a
        // prompt starting with `-` (e.g. `--help`) is read as the flag's value
        // rather than as the next option. `-c` and `-i` are independent, so a
        // resumed session still gets the note and can open on a queued prompt.
        let opening = super::session_opening_prompt(initial_prompt);
        parts.push(format!("-i={}", shell_single_quote(&opening)));
        parts.join(" ")
    }

    fn headless_command(&self, wiring: &AgentWiring, prompt: &str) -> String {
        // `agy`'s headless mode is `agy -p <prompt>` (run the prompt
        // non-interactively and print the response). As with the interactive
        // launch, the MCP/hooks wiring is not rendered — `agy` exposes no inline
        // flag for it, so a headless run works with git and the filesystem alone;
        // only the model (when pinned) is wired. No interactive person is present,
        // so `--dangerously-skip-permissions` auto-approves every tool request to
        // let it act without prompting. As in the interactive launch, the session
        // worktree note leads the prompt (there is no system-prompt flag to carry
        // it), so a headless run also stays confined to its worktree; the combined
        // text is escaped for the single-quoted shell context.
        let mut parts = vec![
            "agy".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        parts.extend(model_flag_parts(wiring));
        parts.push("-p".to_string());
        parts.push(shell_single_quote(&super::session_opening_prompt(Some(
            prompt,
        ))));
        parts.join(" ")
    }

    fn has_resumable_session(&self, dir: &Path) -> bool {
        antigravity_history_path().is_some_and(|history| has_resumable_session_in(&history, dir))
    }

    fn forget_session(&self, dir: &Path) {
        if let Some(history) = antigravity_history_path() {
            forget_session_in(&history, dir);
        }
    }

    fn provision(&self, wiring: &AgentWiring) -> Result<(), String> {
        if let Some(path) = antigravity_mcp_config_path() {
            super::util::update_mcp_config(&path, wiring)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;
    use std::fs;

    /// The session worktree note that leads every `agy` opening prompt (`-i` / `-p`)
    /// because `agy` has no system-prompt flag to carry it out of band.
    const NOTE: &str = super::super::SESSION_WORKTREE_PROMPT;

    /// A `history.jsonl` line recording `agy` running in `workspace`.
    fn history_line(workspace: &str) -> String {
        format!(r#"{{"display":"hi","timestamp":1,"workspace":"{workspace}"}}"#)
    }

    #[test]
    fn launch_command_leads_with_the_worktree_note_and_ignores_wiring() {
        let agent = AntigravityAgent::new();
        assert_eq!(agent.program(), "agy");
        // With no queued prompt, the launch is not bare `agy`: the session worktree
        // note still rides in as the opening prompt so `agy` knows it is already in a
        // worktree. The MCP/local-LLM wiring is ignored either way.
        let expected = format!("agy -i='{NOTE}'");
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None),
            expected
        );
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        assert_eq!(
            agent.launch_command(&settings.agent_wiring("usagi"), false, None),
            expected
        );
    }

    #[test]
    fn launch_and_headless_render_the_model_flag_only_when_set() {
        let agent = AntigravityAgent::new();
        // Default (no model): no `--model`, so `agy` auto-selects.
        let plain = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(!plain.contains("--model"), "{plain}");

        // With a model set, both the interactive and headless launches carry it,
        // ahead of the note-led opening prompt.
        let mut w = Settings::default().agent_wiring("usagi");
        w.model = Some("gemini-3-pro".to_string());
        let launch = agent.launch_command(&w, false, None);
        assert_eq!(launch, format!("agy --model 'gemini-3-pro' -i='{NOTE}'"));
        let headless = agent.headless_command(&w, "clean up");
        assert_eq!(
            headless,
            format!(
                "agy --dangerously-skip-permissions --model 'gemini-3-pro' -p '{NOTE}\n\nclean up'"
            )
        );
    }

    #[test]
    fn launch_command_resumes_with_the_continue_flag() {
        // Resuming continues the most recent conversation for the worktree; the
        // worktree note still leads the opening prompt (it rides in every launch).
        let launch = AntigravityAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            true,
            None,
        );
        assert_eq!(launch, format!("agy -c -i='{NOTE}'"));
    }

    #[test]
    fn launch_command_carries_an_opening_prompt() {
        // A queued prompt follows the worktree note in `-i=<prompt>`, single-quoted
        // for the shell and glued with `=` so a dash-leading prompt stays the flag's
        // value.
        let launch = AntigravityAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("fix issue #50"),
        );
        assert_eq!(launch, format!("agy -i='{NOTE}\n\nfix issue #50'"));
        // A dash-leading prompt (`--help`) binds to `-i` instead of being parsed as
        // the next option.
        let dashed = AntigravityAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("--help"),
        );
        assert_eq!(dashed, format!("agy -i='{NOTE}\n\n--help'"));
    }

    #[test]
    fn launch_command_combines_resume_and_prompt() {
        // `-c` and `-i` are independent, so a resumed session can open already
        // working on a queued prompt (after the worktree note).
        let launch = AntigravityAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            true,
            Some("keep going"),
        );
        assert_eq!(launch, format!("agy -c -i='{NOTE}\n\nkeep going'"));
    }

    #[test]
    fn launch_command_escapes_single_quotes_in_a_prompt() {
        // Arbitrary user prompt text may contain single quotes, which would
        // otherwise break out of the shell argument; each is rendered as the POSIX
        // `'\''` idiom so `agy` receives the prompt verbatim. The note (quote-free)
        // still leads it.
        let launch = AntigravityAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("don't stop"),
        );
        let escaped_prompt = r"don'\''t stop";
        assert_eq!(launch, format!("agy -i='{NOTE}\n\n{escaped_prompt}'"));
    }

    #[test]
    fn headless_command_runs_print_mode_with_auto_approval() {
        // The headless command runs `agy` non-interactively (`-p <prompt>`) with
        // `--dangerously-skip-permissions` auto-approving every tool request, so the
        // background agent acts without prompting. The worktree note leads the prompt
        // (no system-prompt flag carries it), so the run stays in its worktree. The
        // wiring is not rendered (`agy` has no inline flag for it), so no MCP config.
        let launch = AntigravityAgent::new()
            .headless_command(&Settings::default().agent_wiring("usagi"), "clean up");
        assert_eq!(
            launch,
            format!("agy --dangerously-skip-permissions -p '{NOTE}\n\nclean up'")
        );
        assert!(!launch.contains("mcp"));
    }

    #[test]
    fn headless_command_escapes_single_quotes_in_the_prompt() {
        // Arbitrary prompt text may contain single quotes; each is rendered as the
        // POSIX `'\''` idiom so `agy` receives the prompt verbatim. The note
        // (quote-free) still leads it.
        let launch = AntigravityAgent::new().headless_command(
            &Settings::default().agent_wiring("usagi"),
            "don't delete 'main'",
        );
        let escaped_prompt = r"don'\''t delete '\''main'\''";
        assert_eq!(
            launch,
            format!("agy --dangerously-skip-permissions -p '{NOTE}\n\n{escaped_prompt}'")
        );
    }

    #[test]
    fn workspace_of_reads_the_field_and_ignores_malformed_lines() {
        assert_eq!(workspace_of(&history_line("/wt")).as_deref(), Some("/wt"));
        // Not JSON, no `workspace`, or a non-string `workspace` → nothing.
        assert_eq!(workspace_of("not json"), None);
        assert_eq!(workspace_of(r#"{"display":"hi"}"#), None);
        assert_eq!(workspace_of(r#"{"workspace":42}"#), None);
    }

    #[test]
    fn has_resumable_session_in_finds_a_run_for_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let history = root.path().join("history.jsonl");

        // No history file yet → nothing to resume.
        assert!(!has_resumable_session_in(&history, &worktree));

        // A run in another directory does not match.
        fs::write(&history, format!("{}\n", history_line("/some/other/dir"))).unwrap();
        assert!(!has_resumable_session_in(&history, &worktree));

        // A run in the worktree makes it resumable; a malformed line is skipped.
        let contents = format!(
            "not json\n{}\n{}\n",
            history_line("/some/other/dir"),
            history_line(&worktree.to_string_lossy())
        );
        fs::write(&history, contents).unwrap();
        assert!(has_resumable_session_in(&history, &worktree));
    }

    #[test]
    fn forget_session_in_drops_only_the_worktrees_lines() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let history = root.path().join("history.jsonl");
        let contents = format!(
            "not json\n{}\n{}\n{}\n",
            history_line("/some/other/dir"),
            history_line(&worktree.to_string_lossy()),
            history_line(&worktree.to_string_lossy()),
        );
        fs::write(&history, &contents).unwrap();

        forget_session_in(&history, &worktree);

        // The worktree's runs are gone and it is no longer resumable...
        assert!(!has_resumable_session_in(&history, &worktree));
        // ...while the other directory's run and the unattributable line survive.
        let after = fs::read_to_string(&history).unwrap();
        assert!(after.contains("/some/other/dir"));
        assert!(after.starts_with("not json\n"));
        assert!(after.ends_with('\n'));

        // Forgetting again, with nothing left to match, leaves the file untouched.
        let before = fs::read_to_string(&history).unwrap();
        forget_session_in(&history, &worktree);
        assert_eq!(fs::read_to_string(&history).unwrap(), before);
    }

    #[test]
    fn forget_session_in_can_empty_the_history() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let history = root.path().join("history.jsonl");
        // A history holding only the worktree's own runs empties out entirely (no
        // trailing newline is added to an empty file).
        fs::write(
            &history,
            format!("{}\n", history_line(&worktree.to_string_lossy())),
        )
        .unwrap();
        forget_session_in(&history, &worktree);
        assert_eq!(fs::read_to_string(&history).unwrap(), "");
    }

    #[test]
    fn has_resumable_session_in_is_false_for_a_missing_history() {
        // A history file that does not exist (no agent ever run) reads as nothing to
        // resume, and forgetting against it is a no-op.
        let missing = Path::new("/nonexistent/antigravity/history.jsonl");
        assert!(!has_resumable_session_in(
            missing,
            Path::new("/some/worktree")
        ));
        forget_session_in(missing, Path::new("/some/worktree"));
    }

    #[test]
    fn resume_and_forget_resolve_against_the_real_home() {
        // Exercises the home-directory wrappers end to end: a worktree that has
        // never run `agy` has no history line, so it is not resumable, and forgetting
        // it is a no-op.
        let agent = AntigravityAgent::new();
        assert!(!agent.has_resumable_session(Path::new("/nonexistent/usagi/worktree")));
        agent.forget_session(Path::new("/nonexistent/usagi/worktree"));
    }
}
