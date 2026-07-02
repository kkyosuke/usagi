//! Gemini CLI adapter.
//!
//! Gemini has no inline flag for usagi's MCP servers, lifecycle hooks, or a
//! system-prompt addendum — those are only configurable through Gemini's
//! `settings.json` files, which usagi does not write into the user's config or
//! repository. So the [`AgentWiring`] is not rendered into the launch command and
//! Gemini sessions run without usagi's MCP tools or hook-driven phase reporting
//! (their state is inferred from the terminal bell, like any hook-less agent).
//!
//! What Gemini *does* expose as plain CLI flags, usagi wires in:
//!
//! - **Opening prompt** — a queued `session_prompt` rides along as
//!   `gemini -i <prompt>` (execute the prompt, then stay interactive), so the
//!   session opens already working on it.
//! - **Resume** — when a worktree has a prior Gemini conversation, the launch
//!   resumes the latest one with `gemini -r latest` (Gemini scopes "latest" to the
//!   current directory). usagi finds that prior conversation by scanning Gemini's
//!   chat store (`~/.gemini/tmp/<project>/chats`), keyed to the worktree by the
//!   sibling `.project_root` marker — the same mechanism backs forgetting a
//!   session's history on removal.

use std::path::{Path, PathBuf};

use super::util::{same_dir, shell_single_quote};
use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

/// Where Gemini stores each project's chat history:
/// `~/.gemini/tmp/<project>/`. `None` when the home directory can't be
/// determined, so usagi simply launches fresh rather than guessing.
fn gemini_projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini").join("tmp"))
}

/// The Gemini project directories under `root` whose `.project_root` marker names
/// the worktree at `dir`. Gemini names each project directory by a short label and
/// records the absolute project path in a `.project_root` file beside the chats,
/// so matching that marker maps a worktree to its chat store without replicating
/// Gemini's label-derivation.
fn project_dirs_for(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut matched = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return matched;
    };
    for entry in entries.flatten() {
        let project = entry.path();
        let Ok(recorded) = std::fs::read_to_string(project.join(".project_root")) else {
            continue;
        };
        if same_dir(Path::new(recorded.trim()), dir) {
            matched.push(project);
        }
    }
    matched
}

/// Whether a Gemini project directory holds at least one chat transcript
/// (`chats/*.json`) — i.e. a conversation `gemini -r latest` could resume.
fn has_chat_transcript(project: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(project.join("chats")) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
}

/// Whether `root` holds a Gemini chat transcript recorded in the worktree at
/// `dir` — a conversation `gemini -r latest` could continue there. A missing root
/// (no prior run) reads as "nothing to resume", so usagi launches fresh.
fn has_resumable_session_in(root: &Path, dir: &Path) -> bool {
    project_dirs_for(root, dir)
        .iter()
        .any(|project| has_chat_transcript(project))
}

/// Delete every Gemini chat transcript under `root` recorded in the worktree at
/// `dir` (best-effort), so a session recreated at the same path later starts
/// fresh instead of resuming the old conversation. The mirror of
/// [`has_resumable_session_in`]: what that finds, this clears. Only the `chats`
/// transcripts are removed; the project marker and other state are left intact.
fn forget_session_in(root: &Path, dir: &Path) {
    for project in project_dirs_for(root, dir) {
        let chats = project.join("chats");
        let Ok(entries) = std::fs::read_dir(&chats) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// The Gemini CLI adapter.
#[derive(Default)]
pub struct GeminiAgent;

impl GeminiAgent {
    /// A Gemini adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Agent for GeminiAgent {
    fn program(&self) -> &'static str {
        // The program name lives once in the domain (`AgentCli::command`); the
        // adapter reads it from there rather than re-spelling the literal.
        AgentCli::Gemini.command()
    }

    fn launch_command(
        &self,
        _wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        // The wiring (MCP servers, hooks, system prompt) is intentionally not
        // rendered: Gemini has no inline flag for it. Only resume and the opening
        // prompt are wired, via plain flags.
        let mut parts = vec!["gemini".to_string()];
        // `-r latest` continues the most recent conversation for this directory.
        if resume {
            parts.push("-r".to_string());
            parts.push("latest".to_string());
        }
        // A queued prompt rides along as `-i=<prompt>` (execute it, then stay
        // interactive). It is arbitrary user text, so it is escaped for the
        // single-quoted shell context, and glued to the flag with `=` so a prompt
        // starting with `-` (e.g. `ai --help`) is read as the flag's value rather
        // than as the next option. `-r` and `-i` are independent flags, so a
        // resumed session can still open already working on a queued prompt.
        if let Some(prompt) = initial_prompt {
            parts.push(format!("-i={}", shell_single_quote(prompt)));
        }
        parts.join(" ")
    }

    fn headless_command(&self, _wiring: &AgentWiring, prompt: &str) -> String {
        // Gemini's headless mode is `gemini -p <prompt>` (run the prompt
        // non-interactively and exit). As with the interactive launch, the wiring
        // (MCP servers / hooks) is not rendered — Gemini exposes no inline flag
        // for it, so a headless Gemini run cannot drive usagi's MCP tools and
        // works with git and the filesystem alone. No interactive person is
        // present, so `--yolo` auto-approves every tool call (Gemini's bypass flag)
        // to let it act without prompting. The prompt is arbitrary text, so it is
        // escaped for the single-quoted shell context.
        let prompt = shell_single_quote(prompt);
        format!("gemini --yolo -p {prompt}")
    }

    fn has_resumable_session(&self, dir: &Path) -> bool {
        gemini_projects_root().is_some_and(|root| has_resumable_session_in(&root, dir))
    }

    fn forget_session(&self, dir: &Path) {
        if let Some(root) = gemini_projects_root() {
            forget_session_in(&root, dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;
    use std::fs;

    /// Create a Gemini project directory under `root` whose `.project_root` marker
    /// names `project_root`, optionally seeding a `chats/<name>.json` transcript.
    fn write_project(root: &Path, label: &str, project_root: &str, chat: Option<&str>) -> PathBuf {
        let project = root.join(label);
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".project_root"), project_root).unwrap();
        if let Some(name) = chat {
            let chats = project.join("chats");
            fs::create_dir_all(&chats).unwrap();
            fs::write(chats.join(name), "{}").unwrap();
        }
        project
    }

    #[test]
    fn launch_command_is_plain_without_resume_or_prompt() {
        let agent = GeminiAgent::new();
        assert_eq!(agent.program(), "gemini");
        // The wiring is ignored — plain `gemini` whether or not the local LLM is on.
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None),
            "gemini"
        );
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        assert_eq!(
            agent.launch_command(&settings.agent_wiring("usagi"), false, None),
            "gemini"
        );
    }

    #[test]
    fn launch_command_resumes_with_the_latest_flag() {
        // Resuming continues the latest conversation for the directory.
        let launch = GeminiAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            true,
            None,
        );
        assert_eq!(launch, "gemini -r latest");
    }

    #[test]
    fn launch_command_carries_an_opening_prompt() {
        // A queued prompt rides along as `-i=<prompt>`, single-quoted for the
        // shell and glued with `=` so a dash-leading prompt stays the flag's value.
        let launch = GeminiAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("fix issue #50"),
        );
        assert_eq!(launch, "gemini -i='fix issue #50'");
        // A dash-leading prompt (`ai --help`) binds to `-i` instead of being
        // parsed as the next option.
        let dashed = GeminiAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("--help"),
        );
        assert_eq!(dashed, "gemini -i='--help'");
    }

    #[test]
    fn launch_command_combines_resume_and_prompt() {
        // `-r` and `-i` are independent, so a resumed session can open already
        // working on a queued prompt.
        let launch = GeminiAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            true,
            Some("keep going"),
        );
        assert_eq!(launch, "gemini -r latest -i='keep going'");
    }

    #[test]
    fn launch_command_escapes_single_quotes_in_a_prompt() {
        // Arbitrary user prompt text may contain single quotes, which would
        // otherwise break out of the shell argument; each is rendered as the POSIX
        // `'\''` idiom so Gemini receives the prompt verbatim.
        let launch = GeminiAgent::new().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            Some("don't stop"),
        );
        assert_eq!(launch, r"gemini -i='don'\''t stop'");
    }

    #[test]
    fn headless_command_runs_print_mode_with_auto_approval() {
        // The headless command runs Gemini non-interactively (`-p <prompt>`) with
        // `--yolo` auto-approving every tool call, so the background agent acts
        // without prompting. The wiring is not rendered (Gemini has no inline flag
        // for it), so the line carries no MCP config.
        let launch = GeminiAgent::new()
            .headless_command(&Settings::default().agent_wiring("usagi"), "clean up");
        assert_eq!(launch, "gemini --yolo -p 'clean up'");
        assert!(!launch.contains("mcp"));
    }

    #[test]
    fn headless_command_escapes_single_quotes_in_the_prompt() {
        // Arbitrary prompt text may contain single quotes; each is rendered as the
        // POSIX `'\''` idiom so Gemini receives the prompt verbatim.
        let launch = GeminiAgent::new().headless_command(
            &Settings::default().agent_wiring("usagi"),
            "don't delete 'main'",
        );
        assert_eq!(launch, r"gemini --yolo -p 'don'\''t delete '\''main'\'''");
    }

    #[test]
    fn has_resumable_session_in_finds_a_transcript_for_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();

        // No project directories yet → nothing to resume.
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // A project for another directory does not match.
        write_project(root.path(), "other", "/some/other/dir", Some("s.json"));
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // A project for the worktree but with no chat transcript yet is not
        // resumable.
        let mine = write_project(root.path(), "wt", &worktree.to_string_lossy(), None);
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // Once it has a chat transcript, the worktree has a conversation to resume.
        let chats = mine.join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join("session-1.json"), "{}").unwrap();
        assert!(has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn has_resumable_session_in_ignores_projects_without_a_marker() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        // A project directory with chats but no `.project_root` marker cannot be
        // attributed to the worktree, so it is ignored.
        let project = root.path().join("nomarker");
        fs::create_dir_all(project.join("chats")).unwrap();
        fs::write(project.join("chats").join("s.json"), "{}").unwrap();
        assert!(!has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn forget_session_in_deletes_only_the_worktrees_transcripts() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let mine = write_project(
            root.path(),
            "wt",
            &worktree.to_string_lossy(),
            Some("s.json"),
        );
        let other = write_project(root.path(), "other", "/some/other/dir", Some("s.json"));

        forget_session_in(root.path(), &worktree);

        // The worktree's transcript is gone; another directory's is untouched.
        assert!(!mine.join("chats").join("s.json").exists());
        assert!(other.join("chats").join("s.json").exists());
        assert!(!has_resumable_session_in(root.path(), &worktree));
        // The project marker itself is left intact (only chats are cleared).
        assert!(mine.join(".project_root").exists());

        // Forgetting again, with nothing left to match, is a harmless no-op.
        forget_session_in(root.path(), &worktree);
    }

    #[test]
    fn forget_session_in_on_a_marked_project_without_chats_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        write_project(root.path(), "wt", &worktree.to_string_lossy(), None);
        // No chats directory to walk — forgetting is a harmless no-op.
        forget_session_in(root.path(), &worktree);
        assert!(!has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn has_resumable_session_in_is_false_for_a_missing_root() {
        // A projects root that does not exist (no agent ever run) reads as nothing
        // to resume, and forgetting against it is a no-op.
        let missing = Path::new("/nonexistent/gemini/tmp");
        assert!(!has_resumable_session_in(
            missing,
            Path::new("/some/worktree")
        ));
        forget_session_in(missing, Path::new("/some/worktree"));
    }

    #[test]
    fn resume_and_forget_resolve_against_the_real_home() {
        // Exercises the home-directory wrappers end to end: a worktree that has
        // never run Gemini has no transcript, so it is not resumable, and
        // forgetting it is a no-op.
        let agent = GeminiAgent::new();
        assert!(!agent.has_resumable_session(Path::new("/nonexistent/usagi/worktree")));
        agent.forget_session(Path::new("/nonexistent/usagi/worktree"));
    }
}
