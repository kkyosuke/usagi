//! Codex CLI adapter.
//!
//! Wires usagi into Codex through its `-c key=value` config overrides (the same
//! dotted-path overrides Codex would otherwise read from `~/.codex/config.toml`):
//!
//! - **MCP servers** — the unified `usagi` server, plus optional `usagi-llm`
//!   when the local-LLM setting is enabled
//!   (`mcp_servers.<name>.command` / `.args`). Each usagi-owned server is marked
//!   `default_tools_approval_mode = "approve"` so Codex does not ask before every
//!   MCP tool call; shell command approvals remain governed by the launch
//!   approval mode below.
//! - **System prompt** — the session note (already in a worktree; delegate light
//!   work to the local LLM when on) via `developer_instructions`, Codex's
//!   additive instruction override.
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
//! - **Approval mode** — interactive Codex launches pass
//!   `--sandbox workspace-write --ask-for-approval on-request`, so tool calls
//!   auto-run inside Codex's workspace sandbox and the model only escalates for
//!   approval when it needs to step outside that sandbox, instead of prompting on
//!   every command or edit. (Codex dropped the older `--full-auto` shorthand for
//!   this pair.) Headless runs use Codex's stronger
//!   `--dangerously-bypass-approvals-and-sandbox` because no user is present.
//!
//! When a worktree has a prior Codex conversation, the launch resumes it
//! (`codex resume --last`, which Codex filters to the current directory) so
//! reopening a session continues where it left off. usagi finds that prior
//! conversation by scanning Codex's rollout transcripts (`~/.codex/sessions`),
//! whose opening `session_meta` line records the `cwd` — the same mechanism backs
//! forgetting a session's history on removal.
//!
//! A queued opening prompt rides along as Codex's positional `[PROMPT]` argument
//! so the session opens already working on it. Resuming and an opening prompt do
//! not combine: `codex resume`'s positional prompt clashes with `--last`, so when
//! a prompt is queued the launch starts a fresh session working on it rather than
//! resuming.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::util::{same_dir, shell_single_quote};
use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

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

/// Codex MCP approval mode for usagi-owned MCP servers. `approve` skips the
/// per-tool MCP confirmation prompt while leaving shell command approval policy
/// untouched.
const USAGI_MCP_APPROVAL_MODE: &str = "approve";

/// Render `text` as a TOML basic string (double-quoted), escaping the backslash
/// and double-quote that TOML treats specially. Used for the hook command and the
/// system-prompt instruction, whose values may carry those characters; the
/// surrounding `-c` argument is single-quoted for the shell, so the double quotes
/// here pass through untouched.
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

/// The Codex config overrides for one usagi-owned MCP server. Besides transport
/// wiring, mark the server's tools as pre-approved so `agent codex-fugu` does not
/// stop for a confirmation before every `issue_*` / `memory_*` / `session_*`
/// call.
fn mcp_server_overrides(name: &str, bin: &str, args: &[&str]) -> Vec<String> {
    vec![
        config_override(&format!("mcp_servers.{name}.command"), bin),
        config_override(
            &format!("mcp_servers.{name}.args"),
            &toml_string_array(args),
        ),
        config_override(
            &format!("mcp_servers.{name}.default_tools_approval_mode"),
            &toml_basic_string(USAGI_MCP_APPROVAL_MODE),
        ),
    ]
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

/// The `-c` config overrides shared by every Codex launch (fresh or resumed): the
/// usagi MCP server(s), the system-prompt instruction, and the lifecycle hooks.
/// Codex's own model flag (`-m <model>`) when the wiring pins one, else empty.
/// The model name is escaped for the single-quoted shell context. Shared by the
/// interactive and headless launches.
fn model_flag_parts(wiring: &AgentWiring) -> Vec<String> {
    match wiring.model.as_deref() {
        Some(model) => vec!["-m".to_string(), shell_single_quote(model)],
        None => Vec::new(),
    }
}

fn wiring_overrides(wiring: &AgentWiring) -> Vec<String> {
    let bin = &wiring.usagi_bin;
    let local_llm_model = wiring.local_llm_model.as_deref();
    // The unified usagi MCP server is always wired in (issues, memories,
    // sessions); the optional local-LLM server joins it when enabled.
    let mut overrides = mcp_server_overrides("usagi", bin, &["mcp"]);
    if let Some(model) = local_llm_model {
        overrides.extend(mcp_server_overrides(
            "usagi-llm",
            bin,
            &["llm-mcp", "--model", model],
        ));
    }
    // The system prompt rides along as Codex's additive `developer_instructions`.
    let system_prompt = super::session_system_prompt(local_llm_model);
    overrides.push(config_override(
        "developer_instructions",
        &toml_basic_string(&system_prompt),
    ));
    // Lifecycle hooks report the agent's phase back to usagi.
    for (event, phase) in HOOK_PHASES {
        overrides.push(hook_override(bin, event, phase));
    }
    overrides
}

/// Where Codex stores each session's rollout transcript:
/// `~/<home_subdir>/sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl` (`home_subdir` is
/// `.codex` for Codex, `.codex-fugu` for the codex-fugu variant). `None` when the
/// home directory can't be determined, so usagi simply launches fresh rather than
/// guessing.
fn codex_sessions_root(home_subdir: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(home_subdir).join("sessions"))
}

/// The working directory a Codex rollout transcript was recorded in, read from
/// its opening `session_meta` line (`{"type":"session_meta","payload":{"cwd":…}}`).
/// `None` when the file is unreadable, its first line is not that JSON, or it
/// carries no `cwd`. Only the first line is read, so this stays cheap on large
/// transcripts.
fn rollout_cwd(file: &Path) -> Option<PathBuf> {
    use std::io::BufRead;

    #[derive(Deserialize)]
    struct Meta {
        payload: MetaPayload,
    }
    #[derive(Deserialize)]
    struct MetaPayload {
        cwd: Option<PathBuf>,
    }

    let opened = std::fs::File::open(file).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(opened).read_line(&mut first).ok()?;
    serde_json::from_str::<Meta>(&first).ok()?.payload.cwd
}

/// Collect every `*.jsonl` rollout transcript under `root`, descending the
/// date-partitioned directory tree (`<YYYY>/<MM>/<DD>`). A missing or unreadable
/// directory contributes nothing.
fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

/// Whether `root` holds a Codex rollout transcript recorded in the worktree at
/// `dir` — i.e. a conversation `codex resume --last` could continue there. A
/// missing root (no prior run) reads as "nothing to resume", so usagi falls back
/// to a fresh launch.
fn has_resumable_session_in(root: &Path, dir: &Path) -> bool {
    let mut files = Vec::new();
    collect_rollouts(root, &mut files);
    files
        .iter()
        .any(|file| rollout_cwd(file).is_some_and(|cwd| same_dir(&cwd, dir)))
}

/// Delete every Codex rollout transcript under `root` recorded in the worktree at
/// `dir` (best-effort), so a session recreated at the same path later starts
/// fresh instead of resuming the old conversation. The mirror of
/// [`has_resumable_session_in`]: what that finds, this clears.
fn forget_session_in(root: &Path, dir: &Path) {
    let mut files = Vec::new();
    collect_rollouts(root, &mut files);
    for file in files {
        if rollout_cwd(&file).is_some_and(|cwd| same_dir(&cwd, dir)) {
            let _ = std::fs::remove_file(&file);
        }
    }
}

/// The Codex CLI adapter, shared by Codex and the codex-fugu variant.
///
/// Both speak the same invocation surface (the `-c` overrides, lifecycle hooks,
/// and `resume --last` built above); they differ only in the program launched and
/// the home subdirectory their rollout transcripts live under. The constructors
/// fix those two values: [`new`](Self::new) for `codex` / `~/.codex`,
/// [`fugu`](Self::fugu) for `codex-fugu` / `~/.codex-fugu`.
pub struct CodexAgent {
    /// The program name launched (`codex` or `codex-fugu`).
    program: &'static str,
    /// The home subdirectory holding rollout transcripts (`.codex` / `.codex-fugu`).
    home_subdir: &'static str,
}

impl CodexAgent {
    /// The Codex adapter (`codex`, transcripts under `~/.codex`).
    pub fn new() -> Self {
        Self {
            // Program name sourced from the domain SSoT (`AgentCli::command`).
            program: AgentCli::Codex.command(),
            home_subdir: ".codex",
        }
    }

    /// The codex-fugu adapter (`codex-fugu`, transcripts under `~/.codex-fugu`).
    pub fn fugu() -> Self {
        Self {
            program: AgentCli::CodexFugu.command(),
            home_subdir: ".codex-fugu",
        }
    }
}

impl Default for CodexAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CodexAgent {
    fn program(&self) -> &'static str {
        self.program
    }

    fn launch_command(
        &self,
        wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String {
        let overrides = wiring_overrides(wiring);
        // Resume only when there is no queued prompt to deliver: `codex resume`'s
        // positional `[PROMPT]` clashes with `--last` (the lone positional binds
        // to `[SESSION_ID]`), so a queued prompt instead starts a fresh session
        // already working on it. The hooks are non-managed command hooks, so
        // Codex would otherwise prompt to trust each one; usagi vets them (they
        // only run usagi itself), so `--dangerously-bypass-hook-trust` lets them
        // run unattended on both paths. `--sandbox workspace-write` keeps the
        // interactive session confined to the worktree while
        // `--ask-for-approval on-request` avoids per-command prompts unless the
        // model needs to escalate beyond that sandbox. This is the modern Codex
        // spelling of the removed `--full-auto` shorthand.
        let resuming = resume && initial_prompt.is_none();
        let mut parts = if resuming {
            vec![
                self.program.to_string(),
                "resume".to_string(),
                "--last".to_string(),
                "--dangerously-bypass-hook-trust".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "on-request".to_string(),
            ]
        } else {
            vec![
                self.program.to_string(),
                "--dangerously-bypass-hook-trust".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "on-request".to_string(),
            ]
        };
        // An explicit model rides in as Codex's `-m`; absent, Codex uses its own
        // configured default.
        parts.extend(model_flag_parts(wiring));
        parts.extend(overrides);
        // A queued prompt rides along as Codex's trailing positional query (only
        // on the fresh path, where it is unambiguous). It is arbitrary user text,
        // so it is escaped for the single-quoted shell context and placed behind a
        // `--` end-of-options marker: a prompt starting with `-` (e.g. `ai --help`)
        // must bind to the positional, not be parsed as a flag.
        if !resuming {
            if let Some(prompt) = initial_prompt {
                parts.push("--".to_string());
                parts.push(shell_single_quote(prompt));
            }
        }
        parts.join(" ")
    }

    fn headless_command(&self, wiring: &AgentWiring, prompt: &str) -> String {
        // Codex's headless mode is `codex exec <prompt>` (run the prompt
        // non-interactively and exit). The usagi MCP server is wired in via the
        // same `-c mcp_servers.usagi.*` overrides as the interactive launch, so
        // the agent can drive usagi (session_list / session_remove …) while it
        // works. No interactive person is present, so the run bypasses approvals
        // and the filesystem sandbox: `--dangerously-bypass-approvals-and-sandbox`
        // is Codex's single flag for full non-interactive autonomy (it lets the
        // agent delete worktrees and run git without prompting). The MCP wiring is
        // reused but lifecycle hooks are dropped — a headless run reports no phase.
        let bin = &wiring.usagi_bin;
        let mut parts = vec![
            self.program.to_string(),
            "exec".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
        ];
        parts.extend(model_flag_parts(wiring));
        parts.extend(mcp_server_overrides("usagi", bin, &["mcp"]));
        if let Some(model) = wiring.local_llm_model.as_deref() {
            parts.extend(mcp_server_overrides(
                "usagi-llm",
                bin,
                &["llm-mcp", "--model", model],
            ));
        }
        parts.push(shell_single_quote(prompt));
        parts.join(" ")
    }

    fn has_resumable_session(&self, dir: &Path) -> bool {
        codex_sessions_root(self.home_subdir)
            .is_some_and(|root| has_resumable_session_in(&root, dir))
    }

    fn forget_session(&self, dir: &Path) {
        if let Some(root) = codex_sessions_root(self.home_subdir) {
            forget_session_in(&root, dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;
    use std::fs;

    /// An [`AgentWiring`] for the tests: the bare name `usagi` stands in for the
    /// resolved binary path the caller passes, with the local LLM off unless a
    /// model is given.
    fn wiring(usagi_bin: &str, local_llm_model: Option<&str>) -> AgentWiring {
        AgentWiring {
            usagi_bin: usagi_bin.to_string(),
            local_llm_model: local_llm_model.map(str::to_string),
            model: None,
        }
    }

    #[test]
    fn launch_and_headless_render_the_model_flag_only_when_set() {
        // Default (no model): no `-m` flag, so Codex uses its own default.
        let plain = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains(" -m "), "{plain}");

        // With a model set, both the interactive and headless launches carry `-m`.
        let mut w = wiring("usagi", None);
        w.model = Some("gpt-5-codex".to_string());
        let launch = CodexAgent::new().launch_command(&w, false, None);
        assert!(launch.contains("-m 'gpt-5-codex'"), "{launch}");
        let headless = CodexAgent::new().headless_command(&w, "clean up");
        assert!(headless.contains("-m 'gpt-5-codex'"), "{headless}");
    }

    /// Write a rollout transcript whose opening `session_meta` line records `cwd`,
    /// under `root/<sub>/`, mirroring Codex's date-partitioned layout.
    fn write_rollout(root: &Path, sub: &str, name: &str, cwd: &str) -> PathBuf {
        let dir = root.join(sub);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        let line = format!(
            r#"{{"timestamp":"t","type":"session_meta","payload":{{"session_id":"x","cwd":"{cwd}","cli_version":"0.142.0"}}}}"#
        );
        fs::write(&file, format!("{line}\n{{\"type\":\"event_msg\"}}\n")).unwrap();
        file
    }

    #[test]
    fn launch_command_wires_in_the_usagi_mcp_server() {
        // With the local LLM off the unified usagi server is wired in via Codex's
        // `-c` config overrides — the command path bare (literal-string fallback)
        // and the args as a TOML array.
        let launch = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("-c 'mcp_servers.usagi.args=[\"mcp\"]'"));
        assert!(launch.contains("-c 'mcp_servers.usagi.default_tools_approval_mode=\"approve\"'"));
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
        assert!(
            launch.contains("-c 'mcp_servers.usagi-llm.default_tools_approval_mode=\"approve\"'")
        );
    }

    #[test]
    fn launch_command_injects_the_system_prompt_via_developer_instructions() {
        // The session note rides along as Codex's additive `developer_instructions`
        // override (the worktree note alone with the local LLM off).
        let launch = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(launch.contains(
            "-c 'developer_instructions=\"あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。なお、この worktree は親のメインリポジトリの内側に置かれていますが、作業はこのディレクトリ配下だけで完結させ、親ディレクトリ（メインリポジトリ本体）のファイルは読み書きせず、そこへ cd もしないでください。\"'"
        ));
        // With the local LLM on, the delegation nudge is appended to the note.
        let with_llm = CodexAgent::new().launch_command(
            &wiring("usagi", Some("qwen2.5-coder:7b")),
            false,
            None,
        );
        assert!(with_llm.contains("developer_instructions=\"あなたは usagi"));
        assert!(with_llm.contains("local_llm_ask"));
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
        // opens already working on it. It is the trailing, single-quoted argument
        // behind a `--` end-of-options marker (so a `-`-leading prompt cannot be
        // parsed as a flag); the wiring before it is unchanged.
        let launch =
            CodexAgent::new().launch_command(&wiring("usagi", None), false, Some("fix issue #50"));
        assert!(launch.ends_with(" -- 'fix issue #50'"));
        // With no prompt the trailing query is absent: the command is exactly the
        // prompt-carrying one with its ` -- '…'` suffix stripped.
        let plain = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(!plain.contains("fix issue #50"));
        assert_eq!(launch, format!("{plain} -- 'fix issue #50'"));
        // A dash-leading prompt (`ai --help`) stays behind the separator as the
        // positional query instead of aborting the launch as an unknown flag.
        let dashed =
            CodexAgent::new().launch_command(&wiring("usagi", None), false, Some("--help"));
        assert!(dashed.ends_with(" -- '--help'"));
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
    fn launch_command_resumes_the_previous_conversation() {
        // With a resumable conversation and no queued prompt, the launch uses
        // `codex resume --last` (Codex filters it to the worktree) and still
        // carries the full wiring and the trust bypass.
        let launch = CodexAgent::new().launch_command(&wiring("usagi", None), true, None);
        assert!(launch.starts_with("codex resume --last --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("usagi agent-phase ready"));
        assert!(launch.contains("developer_instructions="));
    }

    #[test]
    fn launch_command_uses_modern_approval_flags_for_attended_sessions() {
        // Both the fresh and resumed interactive launches carry Codex's modern
        // spelling of the old `--full-auto` shorthand: run inside the workspace
        // sandbox, and ask only when the model requests escalation.
        let fresh = CodexAgent::new().launch_command(&wiring("usagi", None), false, None);
        assert!(fresh.contains(" --sandbox workspace-write --ask-for-approval on-request "));
        assert!(fresh.starts_with(
            "codex --dangerously-bypass-hook-trust --sandbox workspace-write --ask-for-approval on-request "
        ));
        let resumed = CodexAgent::new().launch_command(&wiring("usagi", None), true, None);
        assert!(resumed.starts_with(
            "codex resume --last --dangerously-bypass-hook-trust --sandbox workspace-write --ask-for-approval on-request "
        ));
        assert!(!fresh.contains("--full-auto"));
        assert!(!resumed.contains("--full-auto"));
    }

    #[test]
    fn launch_command_starts_fresh_with_a_prompt_even_when_resumable() {
        // Resuming and an opening prompt do not combine (the resume positional
        // clashes with `--last`), so a queued prompt starts a fresh session
        // working on it — identical to the non-resume launch with that prompt.
        let resumed_with_prompt =
            CodexAgent::new().launch_command(&wiring("usagi", None), true, Some("do the thing"));
        let fresh_with_prompt =
            CodexAgent::new().launch_command(&wiring("usagi", None), false, Some("do the thing"));
        assert_eq!(resumed_with_prompt, fresh_with_prompt);
        assert!(!resumed_with_prompt.contains("resume --last"));
        assert!(resumed_with_prompt.ends_with(" 'do the thing'"));
    }

    #[test]
    fn headless_command_runs_exec_with_the_usagi_mcp_server() {
        // The headless command runs Codex non-interactively (`codex exec`) with
        // the full-autonomy bypass and the usagi MCP server wired in via `-c`, so
        // the background agent can drive usagi unattended. No interactive lifecycle
        // hooks are rendered (a headless run reports no phase).
        let launch = CodexAgent::new().headless_command(&wiring("usagi", None), "clean up");
        assert!(launch.starts_with(
            "codex exec --dangerously-bypass-approvals-and-sandbox -c 'mcp_servers.usagi.command=usagi'"
        ));
        assert!(launch.contains("-c 'mcp_servers.usagi.args=[\"mcp\"]'"));
        assert!(launch.contains("-c 'mcp_servers.usagi.default_tools_approval_mode=\"approve\"'"));
        assert!(launch.ends_with(" 'clean up'"));
        // The local-LLM server is absent when no model is given, and no hooks ride along.
        assert!(!launch.contains("usagi-llm"));
        assert!(!launch.contains("hooks."));
    }

    #[test]
    fn headless_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the usagi server in the
        // headless `-c` overrides too.
        let launch = CodexAgent::new()
            .headless_command(&wiring("usagi", Some("qwen2.5-coder:7b")), "clean up");
        assert!(launch.contains("-c 'mcp_servers.usagi-llm.command=usagi'"));
        assert!(launch.contains(
            "-c 'mcp_servers.usagi-llm.args=[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]'"
        ));
        assert!(
            launch.contains("-c 'mcp_servers.usagi-llm.default_tools_approval_mode=\"approve\"'")
        );
    }

    #[test]
    fn headless_command_uses_the_fugu_program_and_escapes_the_prompt() {
        // codex-fugu reuses the adapter, so its headless command launches
        // `codex-fugu exec`. The prompt is arbitrary text, escaped for the
        // single-quoted shell context.
        let launch = CodexAgent::fugu().headless_command(&wiring("usagi", None), "don't stop");
        assert!(launch.starts_with("codex-fugu exec --dangerously-bypass-approvals-and-sandbox "));
        assert!(launch.ends_with(r" 'don'\''t stop'"));
    }

    #[test]
    fn toml_basic_string_escapes_backslash_and_quote() {
        assert_eq!(toml_basic_string("plain"), "\"plain\"");
        assert_eq!(toml_basic_string(r"a\b"), r#""a\\b""#);
        assert_eq!(toml_basic_string(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn has_resumable_session_in_finds_a_transcript_for_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();

        // No transcripts yet → nothing to resume.
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // A transcript recorded in another directory does not match.
        write_rollout(
            root.path(),
            "2026/06/23",
            "rollout-other.jsonl",
            "/some/other/dir",
        );
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // A transcript recorded in the worktree means there is a conversation to
        // continue, even nested in the date-partitioned layout.
        write_rollout(
            root.path(),
            "2026/06/23",
            "rollout-wt.jsonl",
            &worktree.to_string_lossy(),
        );
        assert!(has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn has_resumable_session_in_is_false_for_a_missing_root() {
        // A sessions root that does not exist (no agent ever run) reads as nothing
        // to resume — the directory walk yields no files rather than erroring.
        assert!(!has_resumable_session_in(
            Path::new("/nonexistent/codex/sessions"),
            Path::new("/some/worktree")
        ));
        // Forgetting against a missing root is likewise a harmless no-op.
        forget_session_in(
            Path::new("/nonexistent/codex/sessions"),
            Path::new("/some/worktree"),
        );
    }

    #[test]
    fn has_resumable_session_matches_a_canonically_equal_cwd() {
        // The recorded cwd may differ from the worktree path only by a resolvable
        // component (e.g. a trailing `/.`); canonicalization still matches it.
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(worktree.join("sub")).unwrap();
        // The recorded cwd is raw-different (a `sub/..` round-trip) but resolves to
        // the worktree, so canonicalization in `same_dir` still matches it.
        let recorded = worktree.join("sub").join("..");
        write_rollout(
            root.path(),
            "2026/06/23",
            "rollout-dotted.jsonl",
            &recorded.to_string_lossy(),
        );
        assert!(has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn has_resumable_session_ignores_files_without_a_session_meta_cwd() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let dir = root.path().join("2026/06/23");
        fs::create_dir_all(&dir).unwrap();
        // A non-jsonl file and a jsonl without a parseable session_meta cwd are
        // both ignored.
        fs::write(dir.join("notes.txt"), worktree.to_string_lossy().as_bytes()).unwrap();
        fs::write(dir.join("rollout-bad.jsonl"), "not json\n").unwrap();
        assert!(!has_resumable_session_in(root.path(), &worktree));
    }

    #[test]
    fn forget_session_in_deletes_only_the_worktrees_transcripts() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let mine = write_rollout(
            root.path(),
            "2026/06/23",
            "rollout-wt.jsonl",
            &worktree.to_string_lossy(),
        );
        let other = write_rollout(
            root.path(),
            "2026/06/23",
            "rollout-other.jsonl",
            "/some/other/dir",
        );

        forget_session_in(root.path(), &worktree);

        // The worktree's transcript is gone; another directory's is untouched.
        assert!(!mine.exists());
        assert!(other.exists());
        assert!(!has_resumable_session_in(root.path(), &worktree));

        // Forgetting again, with nothing left to match, is a harmless no-op.
        forget_session_in(root.path(), &worktree);
    }

    #[test]
    fn has_resumable_session_resolves_against_the_real_home() {
        // Exercises the home-directory wrapper end to end: a worktree that has
        // never run an agent has no transcript, so it is not resumable.
        let agent = CodexAgent::new();
        assert!(!agent.has_resumable_session(Path::new("/nonexistent/usagi/worktree")));
        // Forgetting such a worktree is a no-op.
        agent.forget_session(Path::new("/nonexistent/usagi/worktree"));
    }

    #[test]
    fn default_agent_matches_new() {
        // The Settings-driven wiring path uses the default constructor; it behaves
        // the same as `new`.
        let launch = CodexAgent::default().launch_command(
            &Settings::default().agent_wiring("usagi"),
            false,
            None,
        );
        assert!(launch.starts_with("codex --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
    }

    #[test]
    fn fugu_launches_the_codex_fugu_program_with_the_same_wiring() {
        // The codex-fugu adapter is Codex with a different program name: the launch
        // starts with `codex-fugu` but carries the identical `-c` wiring and hooks.
        let agent = CodexAgent::fugu();
        assert_eq!(agent.program(), "codex-fugu");
        let launch = agent.launch_command(&wiring("usagi", None), false, None);
        assert!(launch.starts_with("codex-fugu --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("usagi agent-phase ready"));
        // Resuming uses `codex-fugu resume --last`, mirroring Codex.
        let resumed = agent.launch_command(&wiring("usagi", None), true, None);
        assert!(resumed.starts_with("codex-fugu resume --last --dangerously-bypass-hook-trust "));
    }

    #[test]
    fn fugu_resumes_from_its_own_sessions_root() {
        // codex-fugu's rollout store is `~/.codex-fugu/sessions`, distinct from
        // Codex's `~/.codex/sessions`. A transcript under one root resumes only the
        // matching adapter.
        let home = tempfile::tempdir().unwrap();
        let worktree = home.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let fugu_root = home.path().join(".codex-fugu").join("sessions");
        write_rollout(
            &fugu_root,
            "2026/06/24",
            "rollout-wt.jsonl",
            &worktree.to_string_lossy(),
        );

        // The fugu root sees the transcript; the codex root (empty) does not.
        assert!(has_resumable_session_in(&fugu_root, &worktree));
        let codex_root = home.path().join(".codex").join("sessions");
        assert!(!has_resumable_session_in(&codex_root, &worktree));

        // codex_sessions_root threads the subdirectory through to the right store.
        assert!(codex_sessions_root(".codex-fugu")
            .unwrap()
            .ends_with(".codex-fugu/sessions"));
        assert!(codex_sessions_root(".codex")
            .unwrap()
            .ends_with(".codex/sessions"));
    }
}
