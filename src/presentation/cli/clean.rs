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
//! then spawning the process) is IO; the testable pieces — the cleanup prompt
//! ([`clean_prompt`]) and the agent-CLI resolution ([`resolve_agent_cli`]) — are
//! pure functions with unit tests below. The spawn itself ([`spawn_detached`]) is
//! a thin IO wrapper.

use std::env;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::domain::settings::AgentCli;
use crate::infrastructure::repo_paths::STATE_DIR;

/// The log file (under the workspace's `.usagi/`) the background agent's output
/// is appended to.
const CLEAN_LOG: &str = "clean.log";

/// Entry point for `usagi clean`. Resolves the workspace and the agent to run,
/// builds the cleanup prompt, and spawns the agent headlessly in the background,
/// returning immediately. `dry_run` makes the agent report without deleting;
/// `agent` overrides the configured default CLI for this run.
pub fn run(dry_run: bool, agent: Option<String>) -> Result<()> {
    let cwd = env::current_dir()?;
    let root = crate::usecase::session::workspace_root(&cwd);

    // The wired-in MCP servers invoke usagi back, so they are pointed at this
    // process's own executable path rather than the bare name `usagi`, so they
    // resolve even when usagi is run straight from a build and not on `$PATH`.
    let usagi_bin = env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "usagi".to_string());

    let storage = crate::infrastructure::storage::Storage::open_default()?;
    let settings = crate::usecase::settings::effective(&storage, &root)?;

    let agent_cli = resolve_agent_cli(settings.agent_cli, agent.as_deref())?;
    let adapter = crate::infrastructure::agent::agent_for(agent_cli);
    let wiring = settings.agent_wiring(&usagi_bin);

    let prompt = clean_prompt(dry_run);
    let command = adapter.headless_command(&wiring, &prompt);

    let log_path = root.join(STATE_DIR).join(CLEAN_LOG);
    spawn_detached(&command, &root, &log_path)?;

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

/// Resolve which agent CLI to run: the `--agent <name>` override when given
/// (accepting the same names as the 在席 prompt — launch command or display
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

/// Spawn `command` via `sh -c` detached in the background, with `cwd` as its
/// working directory and its stdout/stderr appended to `log_path`. Returns once
/// the child is spawned — usagi does not wait for it. A thin IO wrapper.
fn spawn_detached(command: &str, cwd: &Path, log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("opening log file {}", log_path.display()))?;

    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // Detach from usagi's process group so the agent keeps running after usagi
    // exits (Unix only; on other platforms the child simply outlives the parent).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    builder
        .spawn()
        .with_context(|| format!("spawning background agent: {command}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            AgentCli::CodexFugu
        );
    }

    #[test]
    fn resolve_agent_cli_errors_on_an_unknown_override() {
        // A typo is surfaced rather than silently falling back.
        let err = resolve_agent_cli(AgentCli::Claude, Some("nope")).unwrap_err();
        assert!(err.to_string().contains("unknown agent CLI: nope"));
    }

    #[test]
    fn spawn_detached_runs_the_command_with_cwd_and_appends_to_the_log() {
        // The wrapper runs `sh -c <command>` with the given cwd and appends
        // stdout/stderr to the log file, creating its parent directory. Use a
        // command that writes a marker into cwd and exits, then wait briefly.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let log = cwd.join(".usagi").join("clean.log");
        spawn_detached("printf done > marker; printf log-line 1>&2", cwd, &log).unwrap();

        // Poll for the detached child to finish (it is not waited on).
        let marker = cwd.join("marker");
        for _ in 0..100 {
            if marker.exists()
                && std::fs::read_to_string(&log)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done");
        assert!(std::fs::read_to_string(&log).unwrap().contains("log-line"));
    }

    #[test]
    fn spawn_detached_errors_when_the_log_path_is_unusable() {
        // A log path whose parent cannot be created (a file stands where a
        // directory is needed) surfaces an error rather than spawning.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        // `blocker` is a file, so `blocker/.usagi/clean.log`'s parent cannot be made.
        let log = blocker.join(".usagi").join("clean.log");
        assert!(spawn_detached("true", dir.path(), &log).is_err());
    }
}
