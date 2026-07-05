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
//! - **Session worktree note** — because Gemini has no system-prompt flag, the note
//!   that tells the agent it is already inside a usagi worktree (work in place, do
//!   not create another worktree or touch the parent repo) can't ride in out of
//!   band the way it does for Claude/Codex. It leads the opening prompt instead, so
//!   every Gemini launch — with or without a queued prompt — carries it.
//! - **Opening prompt** — a queued `session_prompt` follows the worktree note in
//!   that same `gemini -i <prompt>` (execute the prompt, then stay interactive), so
//!   the session opens already working on it.
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

/// Gemini's own model flag (`-m <model>`) when the wiring pins one, else empty.
/// The model name is escaped for the single-quoted shell context. Shared by the
/// interactive and headless launches.
fn model_flag_parts(wiring: &AgentWiring) -> Vec<String> {
    match wiring.model.as_deref() {
        Some(model) => vec!["-m".to_string(), shell_single_quote(model)],
        None => Vec::new(),
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
        wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        // The MCP/hooks wiring is intentionally not rendered: Gemini has no inline
        // flag for it. Only the model (when pinned), resume, and the opening prompt
        // are wired, via plain flags.
        let mut parts = vec!["gemini".to_string()];
        // An explicit model rides in as Gemini's `-m`; absent, Gemini uses its own
        // configured default.
        parts.extend(model_flag_parts(wiring));
        // `-r latest` continues the most recent conversation for this directory.
        if resume {
            parts.push("-r".to_string());
            parts.push("latest".to_string());
        }
        // The opening prompt always rides along as `-i=<prompt>` (execute it, then
        // stay interactive): Gemini has no system-prompt flag, so the session
        // worktree note leads it, with any queued prompt after a blank line. It is
        // escaped for the single-quoted shell context, and glued to the flag with
        // `=` so a prompt starting with `-` (e.g. `ai --help`) is read as the flag's
        // value rather than as the next option. `-r` and `-i` are independent flags,
        // so a resumed session still gets the note and can open on a queued prompt.
        let opening = super::session_opening_prompt(wiring.is_root, true, initial_prompt);
        parts.push(format!("-i={}", shell_single_quote(&opening)));
        parts.join(" ")
    }

    fn headless_command(&self, wiring: &AgentWiring, prompt: &str) -> String {
        // Gemini's headless mode is `gemini -p <prompt>` (run the prompt
        // non-interactively and exit). As with the interactive launch, the
        // MCP/hooks wiring is not rendered — Gemini exposes no inline flag for it,
        // so a headless Gemini run cannot drive usagi's MCP tools and works with
        // git and the filesystem alone; only the model (when pinned) is wired. No
        // interactive person is present, so `--yolo` auto-approves every tool call
        // (Gemini's bypass flag) to let it act without prompting. As in the
        // interactive launch, the session worktree note leads the prompt (there is
        // no system-prompt flag to carry it), so a headless run also stays confined
        // to its worktree; the combined text is escaped for the single-quoted shell
        // context.
        let mut parts = vec!["gemini".to_string(), "--yolo".to_string()];
        parts.extend(model_flag_parts(wiring));
        parts.push("-p".to_string());
        parts.push(shell_single_quote(&super::session_opening_prompt(
            wiring.is_root,
            true,
            Some(prompt),
        )));
        parts.join(" ")
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

    fn test_wiring() -> AgentWiring {
        let mut w = Settings::default().agent_wiring("usagi");
        w.is_root = false;
        w
    }

    fn session_opening_prompt(initial_prompt: Option<&str>) -> String {
        super::super::session_opening_prompt(false, true, initial_prompt)
    }

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
    fn launch_command_leads_with_the_worktree_note_and_ignores_wiring() {
        let agent = GeminiAgent::new();
        assert_eq!(agent.program(), "gemini");
        // With no queued prompt, the launch is not bare `gemini`: the session
        // worktree note still rides in as the opening prompt so Gemini knows it is
        // already in a worktree. The MCP/local-LLM wiring is ignored either way.
        let expected = format!("gemini -i={}", shell_single_quote(&session_opening_prompt(None)));
        assert_eq!(
            agent.launch_command(&test_wiring(), false, None),
            expected
        );
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        let mut w = settings.agent_wiring("usagi");
        w.is_root = false;
        assert_eq!(
            agent.launch_command(&w, false, None),
            expected
        );
    }

    #[test]
    fn launch_and_headless_render_the_model_flag_only_when_set() {
        let agent = GeminiAgent::new();
        // Default (no model): no `-m`, so Gemini uses its own default.
        let plain = agent.launch_command(&test_wiring(), false, None);
        assert!(!plain.contains("-m "), "{plain}");

        // With a model set, both the interactive and headless launches carry `-m`,
        // ahead of the note-led opening prompt.
        let mut w = test_wiring();
        w.model = Some("gemini-2.5-pro".to_string());
        let launch = agent.launch_command(&w, false, None);
        assert_eq!(
            launch,
            format!("gemini -m 'gemini-2.5-pro' -i={}", shell_single_quote(&session_opening_prompt(None)))
        );
        let headless = agent.headless_command(&w, "clean up");
        assert_eq!(
            headless,
            format!("gemini --yolo -m 'gemini-2.5-pro' -p {}", shell_single_quote(&session_opening_prompt(Some("clean up"))))
        );
    }

    #[test]
    fn launch_command_resumes_with_the_latest_flag() {
        // Resuming continues the latest conversation for the directory; the worktree
        // note still leads the opening prompt (it rides in every launch).
        let launch = GeminiAgent::new().launch_command(
            &test_wiring(),
            true,
            None,
        );
        assert_eq!(
            launch,
            format!("gemini -r latest -i={}", shell_single_quote(&session_opening_prompt(None)))
        );
    }

    #[test]
    fn launch_command_carries_an_opening_prompt() {
        // A queued prompt follows the worktree note in `-i=<prompt>`, single-quoted
        // for the shell and glued with `=` so a dash-leading prompt stays the flag's
        // value.
        let launch = GeminiAgent::new().launch_command(
            &test_wiring(),
            false,
            Some("fix issue #50"),
        );
        assert_eq!(
            launch,
            format!("gemini -i={}", shell_single_quote(&session_opening_prompt(Some("fix issue #50"))))
        );
        // A dash-leading prompt (`ai --help`) binds to `-i` instead of being
        // parsed as the next option.
        let dashed = GeminiAgent::new().launch_command(
            &test_wiring(),
            false,
            Some("--help"),
        );
        assert_eq!(
            dashed,
            format!("gemini -i={}", shell_single_quote(&session_opening_prompt(Some("--help"))))
        );
    }

    #[test]
    fn launch_command_combines_resume_and_prompt() {
        // `-r` and `-i` are independent, so a resumed session can open already
        // working on a queued prompt (after the worktree note).
        let launch = GeminiAgent::new().launch_command(
            &test_wiring(),
            true,
            Some("keep going"),
        );
        assert_eq!(
            launch,
            format!("gemini -r latest -i={}", shell_single_quote(&session_opening_prompt(Some("keep going"))))
        );
    }

    #[test]
    fn launch_command_escapes_single_quotes_in_a_prompt() {
        // Arbitrary user prompt text may contain single quotes, which would
        // otherwise break out of the shell argument; each is rendered as the POSIX
        // `'\''` idiom so Gemini receives the prompt verbatim. The note (quote-free)
        // still leads it.
        let launch = GeminiAgent::new().launch_command(
            &test_wiring(),
            false,
            Some("don't stop"),
        );
        assert_eq!(
            launch,
            format!("gemini -i={}", shell_single_quote(&session_opening_prompt(Some("don't stop"))))
        );
    }

    #[test]
    fn headless_command_runs_print_mode_with_auto_approval() {
        // The headless command runs Gemini non-interactively (`-p <prompt>`) with
        // `--yolo` auto-approving every tool call, so the background agent acts
        // without prompting. The worktree note leads the prompt (no system-prompt
        // flag carries it), so the run stays in its worktree. The wiring is not
        // rendered (Gemini has no inline flag for it), so no MCP config.
        let launch = GeminiAgent::new()
            .headless_command(&test_wiring(), "clean up");
        assert_eq!(
            launch,
            format!("gemini --yolo -p {}", shell_single_quote(&session_opening_prompt(Some("clean up"))))
        );
        assert!(!launch.contains("mcp"));
    }

    #[test]
    fn headless_command_escapes_single_quotes_in_the_prompt() {
        // Arbitrary prompt text may contain single quotes; each is rendered as the
        // POSIX `'\''` idiom so Gemini receives the prompt verbatim. The note
        // (quote-free) still leads it.
        let launch = GeminiAgent::new().headless_command(
            &test_wiring(),
            "don't delete 'main'",
        );
        assert_eq!(
            launch,
            format!("gemini --yolo -p {}", shell_single_quote(&session_opening_prompt(Some("don't delete 'main'"))))
        );
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
