//! `usagi clean`: hand stale-session cleanup off to a background AI agent.
//!
//! Rather than usagi guessing which session worktrees are safe to delete, it
//! launches the configured agent CLI **headlessly and detached** on a cleanup
//! prompt: the agent inspects `.usagi/sessions/<name>/`, decides which worktrees
//! are merged or abandoned, and removes them itself via usagi's MCP session
//! tools and git. `usagi clean` returns the moment the agent is spawned; the
//! agent's output is appended to `<workspace>/.usagi/clean.log` so the run can be
//! followed after the fact.
//!
//! With `--dry-run` the prompt instructs the agent to report its findings without
//! deleting anything. `--agent <name>` overrides the configured default CLI for
//! this run.
//!
//! The orchestration here (resolving the workspace, settings and binary path,
//! building the prompt and command) is pure once the one genuine side effect —
//! spawning the detached process — is injected: [`run`] takes a `spawn` function,
//! so the whole flow is unit-tested with a recording stub while `main` wires in
//! the real detached spawn.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::domain::settings::AgentCli;
use crate::infrastructure::repo_paths::STATE_DIR;

/// The log file (under the workspace's `.usagi/`) the background agent's output
/// is appended to.
const CLEAN_LOG: &str = "clean.log";

/// Entry point for `usagi clean`. Resolves the workspace and the agent to run,
/// builds the cleanup prompt, and spawns the agent headlessly in the background
/// via `spawn`, returning immediately. `dry_run` makes the agent report without
/// deleting; `agent` overrides the configured default CLI for this run.
///
/// `spawn` runs `<command>` detached with the given working directory, appending
/// its output to the given log path (the production `spawn_detached` in `main`);
/// it is a parameter so the resolution and command-building above are exercised
/// in tests without launching a real process.
pub fn run(
    dry_run: bool,
    agent: Option<String>,
    spawn: impl Fn(&str, &Path, &Path) -> Result<()>,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let root = crate::usecase::session::workspace_root(&cwd);

    // The wired-in MCP servers invoke usagi back, so they are pointed at this
    // process's own executable path rather than the bare name `usagi`, so they
    // resolve even when usagi is run straight from a build and not on `$PATH`.
    let usagi_bin = usagi_bin_path(env::current_exe().ok());

    let storage = crate::infrastructure::storage::Storage::open_default()?;
    let settings = crate::usecase::settings::effective(&storage, &root)?;

    let agent_cli = resolve_agent_cli(settings.agent_cli, agent.as_deref())?;
    let adapter = crate::infrastructure::agent::agent_for(agent_cli);
    let wiring = settings.agent_wiring(&usagi_bin);

    let prompt = clean_prompt(dry_run);
    let _ = adapter.provision(&wiring);
    let command = adapter.headless_command(&wiring, &prompt);

    let log_path = root.join(STATE_DIR).join(CLEAN_LOG);
    spawn(&command, &root, &log_path)?;

    println!(
        "{} ({}) をバックグラウンドで起動しました。",
        agent_cli.display_name(),
        agent_cli.command()
    );
    if dry_run {
        println!("--dry-run: 削除はせず、対象を報告します。");
    }
    println!("ログ: {}", log_path.display());
    Ok(())
}

/// Resolve the usagi binary to wire into the MCP servers: this process's own
/// executable path, falling back to the bare name `usagi` when it cannot be
/// determined or is not valid UTF-8. The current executable is a parameter so
/// both the resolved and fallback paths are unit-tested.
fn usagi_bin_path(current_exe: Option<PathBuf>) -> String {
    current_exe
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "usagi".to_string())
}

/// Resolve which agent CLI to run: the `--agent <name>` override when given
/// (accepting the same names as the 集中 prompt — launch command or display
/// name), else the configured default. An unrecognised override is an error
/// rather than a silent fallback, so a typo is surfaced.
fn resolve_agent_cli(default: AgentCli, override_name: Option<&str>) -> Result<AgentCli> {
    match override_name {
        Some(name) => {
            AgentCli::from_name(name).with_context(|| format!("unknown agent CLI: {name}"))
        }
        None => Ok(default),
    }
}

/// The cleanup task handed to the background agent. Scopes it firmly to stale
/// session worktrees under `.usagi/sessions/<name>/` and forbids touching the
/// repository proper, `main/`, or worktrees with uncommitted work. With
/// `dry_run` the agent reports its findings instead of deleting.
fn clean_prompt(dry_run: bool) -> String {
    let action = if dry_run {
        "削除は一切せず、削除すべきと判断した対象とその理由を一覧で報告してください（dry-run）。"
    } else {
        "不要と判断したセッションを usagi の MCP セッションツール（session_list で一覧、session_remove で削除）や git を用いて安全に削除してください。"
    };
    format!(
        "あなたは usagi の放置セッション（worktree）を整理する自律エージェントです。\n\
         対象は `.usagi/sessions/<name>/` 配下のセッション worktree のみです。\n\
         手順:\n\
         1. session_list で現在のセッションを把握し、各セッションのブランチがマージ済みか、長期間放置されていないかを git で確認する。\n\
         2. マージ済み、または明らかに放置されて不要なセッションを特定する。\n\
         3. {action}\n\
         厳守事項:\n\
         - リポジトリ本体・`main/`・ワークスペースのルートには一切触れない。\n\
         - 未コミットの変更（uncommitted changes）が残っている worktree は削除しない。\n\
         - 判断に迷うものは削除せず、理由とともに残す。\n\
         - 作業はセッションの追加・整理に限定し、それ以外の変更は行わない。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        /// The command the recording stub last received, and the result it returns.
        static SPAWN: RefCell<(Option<String>, Result<(), &'static str>)> =
            const { RefCell::new((None, Ok(()))) };
    }

    /// A `spawn` stub that records the command instead of launching a process.
    fn recording_spawn(command: &str, _cwd: &Path, _log: &Path) -> Result<()> {
        SPAWN.with(|s| {
            let mut s = s.borrow_mut();
            s.0 = Some(command.to_string());
            match s.1 {
                Ok(()) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        })
    }

    fn last_command() -> Option<String> {
        SPAWN.with(|s| s.borrow().0.clone())
    }

    #[test]
    fn run_spawns_the_cleanup_agent_in_the_background() {
        SPAWN.with(|s| *s.borrow_mut() = (None, Ok(())));
        // With no `--agent` override and the default (non-dry-run) mode the agent
        // is launched headlessly on the deletion prompt.
        run(false, None, recording_spawn).unwrap();
        assert!(last_command().is_some());
    }

    #[test]
    fn run_honors_a_dry_run_and_an_agent_override() {
        SPAWN.with(|s| *s.borrow_mut() = (None, Ok(())));
        // `--agent gemini` overrides the default and `--dry-run` is reported.
        run(true, Some("gemini".to_string()), recording_spawn).unwrap();
        assert!(last_command().is_some());
    }

    #[test]
    fn run_errors_on_an_unknown_agent_override() {
        SPAWN.with(|s| *s.borrow_mut() = (None, Ok(())));
        // A typo'd `--agent` is surfaced before anything is spawned.
        let err = run(false, Some("nope".to_string()), recording_spawn).unwrap_err();
        assert!(err.to_string().contains("unknown agent CLI: nope"));
    }

    #[test]
    fn run_propagates_a_spawn_failure() {
        SPAWN.with(|s| *s.borrow_mut() = (None, Err("spawn failed")));
        assert_eq!(
            run(false, None, recording_spawn).unwrap_err().to_string(),
            "spawn failed"
        );
    }

    #[test]
    fn usagi_bin_path_uses_the_executable_path_when_available() {
        assert_eq!(
            usagi_bin_path(Some(PathBuf::from("/opt/bin/usagi"))),
            "/opt/bin/usagi"
        );
    }

    #[test]
    fn usagi_bin_path_falls_back_to_the_bare_name() {
        // No executable path (or a non-UTF-8 one) falls back to the bare `usagi`.
        assert_eq!(usagi_bin_path(None), "usagi");
    }

    #[test]
    fn clean_prompt_scopes_to_session_worktrees_and_forbids_the_repo() {
        let prompt = clean_prompt(false);
        // Scoped to session worktrees and the usagi session tools.
        assert!(prompt.contains(".usagi/sessions/<name>/"));
        assert!(prompt.contains("session_list"));
        assert!(prompt.contains("session_remove"));
        // The repo proper, main/, and uncommitted worktrees are off-limits.
        assert!(prompt.contains("`main/`"));
        assert!(prompt.contains("未コミット"));
        // In the default (non-dry-run) mode it actually deletes.
        assert!(prompt.contains("削除してください"));
        assert!(!prompt.contains("dry-run"));
    }

    #[test]
    fn clean_prompt_reports_without_deleting_in_dry_run() {
        let prompt = clean_prompt(true);
        // Dry-run reports rather than deletes.
        assert!(prompt.contains("dry-run"));
        assert!(prompt.contains("削除は一切せず"));
        // The scope and guardrails are still present.
        assert!(prompt.contains(".usagi/sessions/<name>/"));
        assert!(prompt.contains("`main/`"));
    }

    #[test]
    fn resolve_agent_cli_defaults_when_no_override() {
        // With no `--agent`, the configured default is used as-is.
        assert_eq!(
            resolve_agent_cli(AgentCli::Codex, None).unwrap(),
            AgentCli::Codex
        );
    }

    #[test]
    fn resolve_agent_cli_honors_a_recognised_override() {
        // An override by launch command or display name resolves to its variant,
        // overriding the default.
        assert_eq!(
            resolve_agent_cli(AgentCli::Claude, Some("gemini")).unwrap(),
            AgentCli::Gemini
        );
        assert_eq!(
            resolve_agent_cli(AgentCli::Claude, Some("sakana.ai")).unwrap(),
            AgentCli::SakanaAi
        );
    }

    #[test]
    fn resolve_agent_cli_errors_on_an_unknown_override() {
        // A typo is surfaced rather than silently falling back.
        let err = resolve_agent_cli(AgentCli::Claude, Some("nope")).unwrap_err();
        assert!(err.to_string().contains("unknown agent CLI: nope"));
    }
}
