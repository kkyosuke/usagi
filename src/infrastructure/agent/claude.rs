//! Claude Code adapter.
//!
//! Builds Claude's launch command, delegating to the pure
//! [`AgentCli::launch_command`], which wires in usagi's MCP servers, system
//! prompt, and lifecycle hooks. That rendering is pure and tested in the domain
//! settings builder.
//!
//! It also answers whether a worktree has a Claude conversation to resume, by
//! looking for the transcript Claude Code keeps per project directory — so
//! `:agent` can continue (`claude --continue`) only when continuing would
//! actually find something.

use std::path::{Path, PathBuf};

use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

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

    fn launch_command(&self, wiring: &AgentWiring, resume: bool) -> String {
        AgentCli::Claude.launch_command(
            wiring.local_llm_model.as_deref(),
            &wiring.usagi_bin,
            resume,
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
