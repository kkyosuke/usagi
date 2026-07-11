//! Agent launch capability checks: installed CLIs and explicit model availability.
//!
//! usagi can drive several agent CLIs ([`AgentCli`]), but a machine usually has
//! only the one (or few) the user installed. Probing the PATH for each tells the
//! config screen which agents to offer as selectable choices and feeds `doctor`'s
//! agent presence report. Explicit per-session models are also checked through
//! [`AgentModelProbe`] before orchestration mutates state or queues work. Both
//! external checks are ports, so callers can test them without shelling out.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::domain::agent::{AgentWiring, LaunchMode};
use crate::domain::agent_feature::{self, AgentFeature, Support};
use crate::domain::settings::AgentCli;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::usecase::doctor::CommandRunner;

/// The result of checking whether one agent CLI can use a requested model.
///
/// Availability is deliberately three-state. A probe failure is not equivalent
/// to a model being absent: callers need to distinguish an authoritative model
/// list that did not contain the requested name from a CLI whose models could not
/// be inspected at all. [`require_available_model`] rejects both non-available
/// states so orchestration never launches an agent on an unchecked model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAvailability {
    /// The requested model appeared in the CLI's authoritative model list.
    Available,
    /// The probe succeeded, but the requested model did not appear in its result.
    Unavailable {
        /// Models the probe did report, retained for an actionable error message.
        available_models: Vec<String>,
    },
    /// The CLI's model list could not be obtained or interpreted.
    Unverifiable {
        /// Human-readable reason the probe could not establish availability.
        reason: String,
    },
}

/// Port for checking an agent CLI's currently available models.
///
/// The production implementation invokes each supported CLI's model-list
/// command; orchestration tests inject a deterministic probe instead.
pub trait AgentModelProbe {
    /// Check whether `model` can be selected by `cli` right now.
    fn probe_model(&self, cli: AgentCli, model: &str) -> ModelAvailability;
}

/// The production model-availability probe used by session orchestration.
///
/// Codex and Antigravity expose non-interactive catalog commands, which this
/// probe runs with bounded output and time. CLIs without a safely queryable
/// catalog report [`ModelAvailability::Unverifiable`], so an explicit unchecked
/// model is never allowed through accidentally.
#[derive(Debug, Clone, Copy, Default)]
pub struct CliAgentModelProbe;

const MODEL_PROBE_LIMITS: ModelProbeLimits = ModelProbeLimits {
    command_timeout: Duration::from_secs(20),
    poll_interval: Duration::from_millis(50),
    cleanup_timeout: Duration::from_secs(1),
    reader_timeout: Duration::from_secs(1),
    stdout_cap: 4 * 1024 * 1024,
    stderr_cap: 8 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelProbeLimits {
    command_timeout: Duration,
    poll_interval: Duration,
    /// Maximum nonblocking reap budget after attempting to kill a timed-out
    /// direct child. A failed kill must not turn into an unbounded `wait()`.
    cleanup_timeout: Duration,
    /// Maximum wait for pipe readers after the direct child has exited.
    /// Descendants can inherit its pipe descriptors, so joining reader threads
    /// is not safe even after that direct child has been reaped.
    reader_timeout: Duration,
    stdout_cap: usize,
    stderr_cap: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogCommandOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stdout_truncated: bool,
}

trait CatalogCommandRunner {
    fn run(
        &self,
        command: &str,
        args: &[&str],
        limits: ModelProbeLimits,
    ) -> Result<CatalogCommandOutput, String>;
}

struct SystemCatalogCommandRunner;

type ReaderResult = std::io::Result<(Vec<u8>, bool)>;

impl CatalogCommandRunner for SystemCatalogCommandRunner {
    fn run(
        &self,
        command: &str,
        args: &[&str],
        limits: ModelProbeLimits,
    ) -> Result<CatalogCommandOutput, String> {
        let invocation = invocation(command, args);
        let mut command_builder = Command::new(command);
        command_builder
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Put the catalog command and any descendants it spawns in their own
        // process group. A timeout can then terminate the whole tree instead of
        // leaving a grandchild holding our pipe descriptors and drain threads.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command_builder.process_group(0);
        }
        let mut child = command_builder
            .spawn()
            .map_err(|error| format!("failed to start {invocation}: {error}"))?;

        // The handles are guaranteed by the `Stdio::piped` configuration above.
        // Move each into an independent drain before waiting, otherwise either
        // full pipe could block the child and deadlock the probe.
        let mut stdout = child
            .stdout
            .take()
            .expect("piped model-catalog stdout must be present");
        let mut stderr = child
            .stderr
            .take()
            .expect("piped model-catalog stderr must be present");
        let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
        let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = stdout_tx.send(read_capped(&mut stdout, limits.stdout_cap));
        });
        std::thread::spawn(move || {
            let _ = stderr_tx.send(read_capped(&mut stderr, limits.stderr_cap));
        });

        let Some(status) = wait_for_catalog_child(
            &mut child,
            limits.command_timeout,
            limits.poll_interval,
            limits.cleanup_timeout,
        ) else {
            // Deliberately do not join the readers here. A grandchild may have
            // inherited stdout/stderr and can keep them open after the catalog
            // child is killed, so joining would turn the timeout into an
            // unbounded wait. The bounded nonblocking cleanup above normally
            // reaps the direct child; a detached fallback reaper owns it if kill
            // failed or exit was not observable before that cleanup deadline.
            // Neither that reaper nor the bounded-memory pipe drains are joined
            // on the request thread.
            drop(std::thread::spawn(move || {
                // Retry the group-wide termination in case the bounded cleanup
                // observed a transient failure, then kill the direct child as a
                // final fallback so the blocking reap below can eventually end.
                let _ = child.kill_catalog();
                let _ = child.kill();
                let _ = child.wait();
            }));
            return Err(format!(
                "{invocation} did not finish within {:?} or could not be polled; termination was attempted",
                limits.command_timeout
            ));
        };

        // Even a normally exited child can leave pipe descriptors in a spawned
        // descendant. Receive both reader results against one deadline rather
        // than joining either thread indefinitely.
        let reader_deadline = Instant::now() + limits.reader_timeout;
        let (stdout, stdout_truncated) =
            match receive_reader(&stdout_rx, reader_deadline, "stdout", &invocation) {
                Ok(output) => output,
                Err(error) => {
                    let _ = child.kill_catalog();
                    return Err(error);
                }
            };
        // Stderr is drained to keep the subprocess from blocking, but it is not
        // propagated across the MCP boundary: CLI diagnostics can contain local
        // paths, account identifiers, or authentication details.
        if let Err(error) = receive_reader(&stderr_rx, reader_deadline, "stderr", &invocation) {
            let _ = child.kill_catalog();
            return Err(error);
        }

        Ok(CatalogCommandOutput {
            success: status.success,
            status: status.display,
            stdout,
            stdout_truncated,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogProcessStatus {
    success: bool,
    display: String,
}

trait WaitableCatalogChild {
    fn try_wait_catalog(&mut self) -> std::io::Result<Option<CatalogProcessStatus>>;
    fn kill_catalog(&mut self) -> std::io::Result<()>;
}

impl WaitableCatalogChild for Child {
    fn try_wait_catalog(&mut self) -> std::io::Result<Option<CatalogProcessStatus>> {
        self.try_wait().map(|status| {
            status.map(|status| CatalogProcessStatus {
                success: status.success(),
                display: status.to_string(),
            })
        })
    }

    fn kill_catalog(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            ignore_missing_process_group(signal_process_group(self.id() as libc::pid_t))
        }
        #[cfg(not(unix))]
        {
            self.kill()
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pid: libc::pid_t) -> std::io::Result<()> {
    // SAFETY: every caller passes the positive pid of a subprocess that was
    // started as a process-group leader. `killpg` neither dereferences pointers
    // nor borrows Rust-managed memory.
    if unsafe { libc::killpg(pid, libc::SIGKILL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn ignore_missing_process_group(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        // The group disappearing between `try_wait` and `killpg` is the same
        // terminal condition as a successful kill.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other,
    }
}

fn wait_for_catalog_child(
    child: &mut dyn WaitableCatalogChild,
    timeout: Duration,
    poll: Duration,
    cleanup_timeout: Duration,
) -> Option<CatalogProcessStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait_catalog() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(poll),
            _ => {
                let _ = child.kill_catalog();
                // Reap only through nonblocking polls. `kill` can fail (or take
                // time to become observable), and calling blocking `wait` after
                // that would defeat the request timeout. `try_wait` reaps once
                // the child has exited; otherwise this cleanup gives up at its
                // own hard deadline and the request still returns.
                let cleanup_deadline = Instant::now() + cleanup_timeout;
                loop {
                    match child.try_wait_catalog() {
                        Ok(Some(_)) => return None,
                        Ok(None) if Instant::now() < cleanup_deadline => std::thread::sleep(poll),
                        _ => return None,
                    }
                }
            }
        }
    }
}

fn read_capped(reader: &mut dyn Read, cap: usize) -> ReaderResult {
    let mut output = Vec::new();
    reader.take(cap as u64 + 1).read_to_end(&mut output)?;
    let truncated = output.len() > cap;
    if truncated {
        output.truncate(cap);
        // Keep draining after the retained prefix so the child never blocks on
        // a full pipe. This copy remains best-effort because truncation is
        // already authoritative and its only purpose is to unblock the child.
        let _ = std::io::copy(reader, &mut std::io::sink());
    }
    Ok((output, truncated))
}

fn receive_reader(
    receiver: &Receiver<ReaderResult>,
    deadline: Instant,
    stream: &str,
    invocation: &str,
) -> Result<(Vec<u8>, bool), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(format!(
            "failed to read {stream} from {invocation}: {error}"
        )),
        Err(RecvTimeoutError::Timeout) => Err(format!(
            "the {stream} reader for {invocation} did not finish before its deadline"
        )),
        Err(RecvTimeoutError::Disconnected) => Err(format!(
            "the {stream} reader for {invocation} stopped without returning a result"
        )),
    }
}

fn invocation(command: &str, args: &[&str]) -> String {
    if args.is_empty() {
        format!("`{command}`")
    } else {
        format!("`{command} {}`", args.join(" "))
    }
}

fn probe_command_with(
    runner: &dyn CatalogCommandRunner,
    command: &str,
    args: &[&str],
    model: &str,
    parse: fn(&str) -> Result<Vec<String>, String>,
    limits: ModelProbeLimits,
) -> ModelAvailability {
    let invocation = invocation(command, args);
    let output = match runner.run(command, args, limits) {
        Ok(output) => output,
        Err(reason) => return ModelAvailability::Unverifiable { reason },
    };
    if output.stdout_truncated {
        return ModelAvailability::Unverifiable {
            reason: format!(
                "{invocation} stdout exceeded the {}-byte limit",
                limits.stdout_cap
            ),
        };
    }
    if !output.success {
        return ModelAvailability::Unverifiable {
            reason: format!(
                "{invocation} exited with {}; run the command directly for local diagnostics",
                output.status
            ),
        };
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            return ModelAvailability::Unverifiable {
                reason: format!("{invocation} stdout was not valid UTF-8: {error}"),
            };
        }
    };
    let available_models = match parse(&stdout) {
        Ok(models) => models,
        Err(error) => {
            return ModelAvailability::Unverifiable {
                reason: format!("could not parse {invocation} output: {error}"),
            };
        }
    };
    if available_models.iter().any(|candidate| candidate == model) {
        ModelAvailability::Available
    } else {
        ModelAvailability::Unavailable { available_models }
    }
}

fn probe_model_with(
    runner: &dyn CatalogCommandRunner,
    cli: AgentCli,
    model: &str,
    limits: ModelProbeLimits,
) -> ModelAvailability {
    match cli {
        AgentCli::Codex => probe_command_with(
            runner,
            "codex",
            &["debug", "models"],
            model,
            parse_codex_debug_models,
            limits,
        ),
        AgentCli::Antigravity => {
            probe_command_with(runner, "agy", &["models"], model, parse_model_lines, limits)
        }
        AgentCli::Gemini => ModelAvailability::Unverifiable {
            reason: "Gemini model-catalog probing is not implemented by usagi".to_string(),
        },
        AgentCli::Claude | AgentCli::SakanaAi => ModelAvailability::Unverifiable {
            reason: format!(
                "{} does not expose a model catalog that usagi can safely query",
                cli.display_name()
            ),
        },
    }
}

impl AgentModelProbe for CliAgentModelProbe {
    fn probe_model(&self, cli: AgentCli, model: &str) -> ModelAvailability {
        probe_model_with(&SystemCatalogCommandRunner, cli, model, MODEL_PROBE_LIMITS)
    }
}

/// Require a requested agent model to have been positively verified.
///
/// Both a confirmed absence and an inconclusive probe are errors. This
/// fail-closed policy matters for queued orchestration: once a pane has spawned,
/// a CLI rejecting its model happens inside the PTY and can no longer be returned
/// synchronously to the `session_prompt` caller.
pub fn require_available_model(
    probe: &dyn AgentModelProbe,
    cli: AgentCli,
    model: &str,
) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err(format!(
            "model must not be blank for {}",
            cli.display_name()
        ));
    }

    match probe.probe_model(cli, model) {
        ModelAvailability::Available => Ok(()),
        ModelAvailability::Unavailable { available_models } => {
            let detail = if available_models.is_empty() {
                "the probe returned no available models".to_string()
            } else {
                format!("available models: {}", available_models.join(", "))
            };
            Err(format!(
                "model {model:?} is not available for {} ({detail})",
                cli.display_name()
            ))
        }
        ModelAvailability::Unverifiable { reason } => Err(format!(
            "could not verify model {model:?} for {}: {reason}; clear the explicit model override to use the CLI default",
            cli.display_name()
        )),
    }
}

/// Parse the JSON object emitted by `codex debug models`.
///
/// Only entries whose `visibility` is `"list"` are selectable. Their `slug`
/// values are trimmed and de-duplicated in first-seen order. An empty selectable
/// set is an error: it cannot authoritatively validate a requested model.
pub fn parse_codex_debug_models(output: &str) -> Result<Vec<String>, String> {
    let document: serde_json::Value = serde_json::from_str(output.trim())
        .map_err(|error| format!("invalid `codex debug models` JSON: {error}"))?;
    let document = document
        .as_object()
        .ok_or_else(|| "`codex debug models` output is not a JSON object".to_string())?;
    let entries = document
        .get("models")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "`codex debug models` output has no models array".to_string())?;

    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let entry = entry
            .as_object()
            .ok_or_else(|| format!("`codex debug models` entry {} is not an object", index + 1))?;
        if entry.get("visibility").and_then(serde_json::Value::as_str) != Some("list") {
            continue;
        }
        let slug = entry
            .get("slug")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`codex debug models` entry {} has no string slug",
                    index + 1
                )
            })?
            .trim();
        if slug.is_empty() {
            return Err(format!(
                "`codex debug models` entry {} has a blank slug",
                index + 1
            ));
        }
        if seen.insert(slug.to_string()) {
            models.push(slug.to_string());
        }
    }
    if models.is_empty() {
        Err("`codex debug models` returned no listed models".to_string())
    } else {
        Ok(models)
    }
}

/// Parse the line-oriented output of `agy models`.
///
/// Antigravity prints one display model name per line, preceded by a fixed
/// progress message. Whitespace, blank lines, that progress line, and duplicate
/// names are removed while preserving first-seen order.
pub fn parse_model_lines(output: &str) -> Result<Vec<String>, String> {
    const PROGRESS: &str = "Fetching available models...";

    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for raw in output.lines() {
        let model = raw.trim();
        if model.is_empty() || model == PROGRESS {
            continue;
        }
        if seen.insert(model.to_string()) {
            models.push(model.to_string());
        }
    }
    if models.is_empty() {
        Err("agy models returned no available models".to_string())
    } else {
        Ok(models)
    }
}

/// The agent CLIs whose launch command is available on the PATH, in
/// [`AgentCli::ALL`] order. An empty result means none are installed.
pub fn available_clis(runner: &dyn CommandRunner) -> Vec<AgentCli> {
    AgentCli::ALL
        .into_iter()
        .filter(|cli| runner.available(cli.command()))
        .collect()
}

/// The agent CLIs whose launch command is available on the PATH, and which
/// support usagi's MCP server integration.
pub fn mcp_capable_clis(runner: &dyn CommandRunner) -> Vec<AgentCli> {
    available_clis(runner)
        .into_iter()
        .filter(|cli| agent_feature::support(*cli, AgentFeature::Mcp) == Support::Yes)
        .collect()
}

/// Build the per-pane launch wiring from a workspace-wide base wiring.
///
/// The domain wiring is data-only, so the caller injects the git-common-dir
/// resolver. Interactive launches carry two kinds of extra writable roots:
///
/// - the workspace-local `.usagi` directory, so an agent launched from inside
///   `.usagi/sessions/<name>/` can still use MCP session orchestration tools
///   that mutate the parent workspace's `state.json` and sibling session
///   worktrees;
/// - the repository's shared git common directory, so sandboxed Codex sessions
///   can update `.git` without approval prompts.
///
/// A git resolver failure leaves the launch usable with the workspace metadata
/// root only; adapters still add their own fixed roots (for Codex, usagi's
/// global data directory).
pub fn wiring_for_launch(
    base: &AgentWiring,
    model: Option<String>,
    dir: &Path,
    mode: LaunchMode,
    resolve_git_common_dir: &dyn Fn(&Path) -> Option<PathBuf>,
) -> AgentWiring {
    let mut sandbox_writable_roots = base.sandbox_writable_roots.clone();
    if mode == LaunchMode::Interactive {
        sandbox_writable_roots.push(crate::usecase::session::workspace_root(dir).join(STATE_DIR));
        sandbox_writable_roots.extend(resolve_git_common_dir(dir));
    }
    AgentWiring {
        model,
        is_root: !crate::usecase::workspace_guard::is_session_worktree(dir),
        sandbox_writable_roots,
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::path::PathBuf;

    /// A runner that reports a fixed allowlist of programs as available.
    struct FakeRunner(Vec<&'static str>);

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.0.contains(&program)
        }
        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ExpectedModelProbe {
        cli: AgentCli,
        model: &'static str,
        availability: ModelAvailability,
    }

    impl AgentModelProbe for ExpectedModelProbe {
        fn probe_model(&self, cli: AgentCli, model: &str) -> ModelAvailability {
            assert_eq!(cli, self.cli);
            assert_eq!(model, self.model);
            self.availability.clone()
        }
    }

    #[derive(Clone)]
    struct FakeCatalogRunner {
        result: Result<CatalogCommandOutput, String>,
        calls: RefCell<Vec<(String, Vec<String>, ModelProbeLimits)>>,
    }

    impl FakeCatalogRunner {
        fn returning(result: Result<CatalogCommandOutput, String>) -> Self {
            Self {
                result,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl CatalogCommandRunner for FakeCatalogRunner {
        fn run(
            &self,
            command: &str,
            args: &[&str],
            limits: ModelProbeLimits,
        ) -> Result<CatalogCommandOutput, String> {
            self.calls.borrow_mut().push((
                command.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                limits,
            ));
            self.result.clone()
        }
    }

    fn test_limits() -> ModelProbeLimits {
        ModelProbeLimits {
            command_timeout: Duration::from_secs(1),
            poll_interval: Duration::ZERO,
            cleanup_timeout: Duration::ZERO,
            reader_timeout: Duration::from_millis(50),
            stdout_cap: 64,
            stderr_cap: 32,
        }
    }

    fn completed(stdout: impl Into<Vec<u8>>) -> CatalogCommandOutput {
        CatalogCommandOutput {
            success: true,
            status: "exit status: 0".to_string(),
            stdout: stdout.into(),
            stdout_truncated: false,
        }
    }

    struct FakeCatalogChild {
        polls: VecDeque<std::io::Result<Option<CatalogProcessStatus>>>,
        killed: bool,
        fail_kill: bool,
    }

    impl FakeCatalogChild {
        fn new(polls: Vec<std::io::Result<Option<CatalogProcessStatus>>>) -> Self {
            Self {
                polls: polls.into(),
                killed: false,
                fail_kill: false,
            }
        }
    }

    impl WaitableCatalogChild for FakeCatalogChild {
        fn try_wait_catalog(&mut self) -> std::io::Result<Option<CatalogProcessStatus>> {
            self.polls.pop_front().unwrap_or(Ok(None))
        }

        fn kill_catalog(&mut self) -> std::io::Result<()> {
            self.killed = true;
            if self.fail_kill {
                Err(std::io::Error::other("kill failed"))
            } else {
                Ok(())
            }
        }
    }

    fn successful_status() -> CatalogProcessStatus {
        CatalogProcessStatus {
            success: true,
            display: "exit status: 0".to_string(),
        }
    }

    #[test]
    fn catalog_invocation_formats_commands_with_and_without_arguments() {
        assert_eq!(
            invocation("codex", &["debug", "models"]),
            "`codex debug models`"
        );
        assert_eq!(invocation("codex", &[]), "`codex`");
    }

    #[test]
    fn capped_catalog_reader_returns_complete_and_truncated_outputs() {
        let mut complete = Cursor::new(b"model".to_vec());
        assert_eq!(
            read_capped(&mut complete, 5).unwrap(),
            (b"model".to_vec(), false)
        );

        let mut long = Cursor::new(b"models".to_vec());
        assert_eq!(
            read_capped(&mut long, 5).unwrap(),
            (b"model".to_vec(), true)
        );
        assert_eq!(
            long.position(),
            6,
            "the truncated remainder must be drained"
        );
    }

    #[test]
    fn capped_catalog_reader_propagates_an_io_error() {
        struct ErrorReader;
        impl Read for ErrorReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("read failed"))
            }
        }

        assert_eq!(
            read_capped(&mut ErrorReader, 5).unwrap_err().to_string(),
            "read failed"
        );
    }

    #[test]
    fn reader_result_receiver_reports_success_io_error_timeout_and_disconnect() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Ok((b"ok".to_vec(), false))).unwrap();
        assert_eq!(
            receive_reader(&rx, Instant::now(), "stdout", "`catalog`").unwrap(),
            (b"ok".to_vec(), false)
        );

        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Err(std::io::Error::other("broken pipe"))).unwrap();
        assert_eq!(
            receive_reader(&rx, Instant::now(), "stderr", "`catalog`").unwrap_err(),
            "failed to read stderr from `catalog`: broken pipe"
        );

        let (_tx, rx) = mpsc::sync_channel(1);
        assert_eq!(
            receive_reader(&rx, Instant::now(), "stdout", "`catalog`").unwrap_err(),
            "the stdout reader for `catalog` did not finish before its deadline"
        );

        let (tx, rx) = mpsc::sync_channel::<ReaderResult>(1);
        drop(tx);
        assert_eq!(
            receive_reader(
                &rx,
                Instant::now() + Duration::from_secs(1),
                "stderr",
                "`catalog`"
            )
            .unwrap_err(),
            "the stderr reader for `catalog` stopped without returning a result"
        );
    }

    #[test]
    fn catalog_child_wait_returns_a_completed_status_without_cleanup() {
        let mut child = FakeCatalogChild::new(vec![Ok(Some(successful_status()))]);

        assert_eq!(
            wait_for_catalog_child(
                &mut child,
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::ZERO,
            ),
            Some(successful_status())
        );
        assert!(!child.killed);
    }

    #[test]
    fn catalog_child_wait_repolls_until_the_child_completes() {
        let mut child = FakeCatalogChild::new(vec![Ok(None), Ok(Some(successful_status()))]);

        assert_eq!(
            wait_for_catalog_child(
                &mut child,
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::ZERO,
            ),
            Some(successful_status())
        );
        assert!(child.polls.is_empty());
    }

    #[test]
    fn catalog_child_wait_kills_and_nonblocking_reaps_on_timeout() {
        let mut timed_out =
            FakeCatalogChild::new(vec![Ok(None), Ok(None), Ok(Some(successful_status()))]);
        assert_eq!(
            wait_for_catalog_child(
                &mut timed_out,
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            None
        );
        assert!(timed_out.killed);
        assert!(timed_out.polls.is_empty());
    }

    #[test]
    fn catalog_child_wait_stays_bounded_when_poll_and_kill_fail() {
        let mut failed = FakeCatalogChild::new(vec![Err(std::io::Error::other("poll failed"))]);
        failed.fail_kill = true;
        assert_eq!(
            wait_for_catalog_child(
                &mut failed,
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::ZERO,
            ),
            None
        );
        assert!(failed.killed);
    }

    #[cfg(unix)]
    #[test]
    fn missing_process_groups_are_success_but_other_signal_errors_survive() {
        assert!(ignore_missing_process_group(Ok(())).is_ok());
        assert!(
            ignore_missing_process_group(Err(std::io::Error::from_raw_os_error(libc::ESRCH)))
                .is_ok()
        );
        assert_eq!(
            ignore_missing_process_group(Err(std::io::Error::from_raw_os_error(libc::EPERM)))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_catalog_runner_captures_status_and_bounded_output() {
        let limits = ModelProbeLimits {
            stdout_cap: 3,
            stderr_cap: 4,
            ..test_limits()
        };
        let output = SystemCatalogCommandRunner
            .run("sh", &["-c", "printf abcdef; printf warning >&2"], limits)
            .unwrap();

        assert!(output.success);
        assert_eq!(output.stdout, b"abc");
        assert!(output.stdout_truncated);

        let nonzero = SystemCatalogCommandRunner
            .run("sh", &["-c", "printf denied >&2; exit 7"], test_limits())
            .unwrap();
        assert!(!nonzero.success);
        assert!(nonzero.status.contains('7'), "{}", nonzero.status);
    }

    #[test]
    fn system_catalog_runner_reports_a_spawn_failure() {
        let error = SystemCatalogCommandRunner
            .run("usagi-command-that-must-not-exist", &[], test_limits())
            .unwrap_err();

        assert!(
            error.starts_with("failed to start `usagi-command-that-must-not-exist`:"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_catalog_runner_timeout_does_not_join_inherited_pipe_readers() {
        let limits = ModelProbeLimits {
            command_timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(5),
            cleanup_timeout: Duration::from_millis(100),
            reader_timeout: Duration::from_millis(10),
            ..test_limits()
        };
        let started = Instant::now();
        let error = SystemCatalogCommandRunner
            .run("sh", &["-c", "sleep 3 & wait"], limits)
            .unwrap_err();

        assert!(error.contains("did not finish within 200ms"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a timed-out reader was joined instead of detached"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_catalog_runner_bounds_readers_after_a_successful_child_exit() {
        let limits = ModelProbeLimits {
            reader_timeout: Duration::from_millis(10),
            ..test_limits()
        };
        let started = Instant::now();
        let error = SystemCatalogCommandRunner
            .run("sh", &["-c", "sleep 3 & exit 0"], limits)
            .unwrap_err();

        assert!(error.contains("reader"), "{error}");
        assert!(error.contains("did not finish"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a reader held by a descendant was joined instead of bounded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_catalog_runner_bounds_a_stderr_reader_held_by_a_descendant() {
        let limits = ModelProbeLimits {
            reader_timeout: Duration::from_millis(10),
            ..test_limits()
        };
        let error = SystemCatalogCommandRunner
            .run("sh", &["-c", "sleep 3 >/dev/null & exit 0"], limits)
            .unwrap_err();

        assert!(error.contains("stderr reader"), "{error}");
        assert!(error.contains("did not finish"), "{error}");
    }

    #[test]
    fn cli_model_probe_dispatches_codex_and_accepts_an_exact_match() {
        let output =
            completed(br#"{"models":[{"slug":"gpt-5-codex","visibility":"list"}]}"#.to_vec());
        let runner = FakeCatalogRunner::returning(Ok(output));
        let limits = test_limits();

        assert_eq!(
            probe_model_with(&runner, AgentCli::Codex, "gpt-5-codex", limits),
            ModelAvailability::Available
        );
        assert_eq!(
            *runner.calls.borrow(),
            vec![(
                "codex".to_string(),
                vec!["debug".to_string(), "models".to_string()],
                limits,
            )]
        );
    }

    #[test]
    fn cli_model_probe_dispatches_antigravity_and_reports_alternatives() {
        let runner = FakeCatalogRunner::returning(Ok(completed("Gemini Pro\nGemini Flash\n")));
        let limits = test_limits();

        assert_eq!(
            probe_model_with(&runner, AgentCli::Antigravity, "Missing", limits),
            ModelAvailability::Unavailable {
                available_models: vec!["Gemini Pro".to_string(), "Gemini Flash".to_string()]
            }
        );
        assert_eq!(
            *runner.calls.borrow(),
            vec![("agy".to_string(), vec!["models".to_string()], limits)]
        );
    }

    #[test]
    fn cli_model_probe_rejects_clis_without_a_queryable_catalog_without_running_a_command() {
        fn assert_unqueryable_cli(cli: AgentCli) {
            let runner = FakeCatalogRunner::returning(Err("must not run".to_string()));
            let result = probe_model_with(&runner, cli, "explicit", test_limits());

            assert_eq!(
                result,
                ModelAvailability::Unverifiable {
                    reason: format!(
                        "{} does not expose a model catalog that usagi can safely query",
                        cli.display_name()
                    )
                }
            );
            assert!(runner.calls.borrow().is_empty());
        }
        assert_unqueryable_cli(AgentCli::Claude);
        assert_unqueryable_cli(AgentCli::SakanaAi);

        let runner = FakeCatalogRunner::returning(Err("must not run".to_string()));
        assert_eq!(
            probe_model_with(&runner, AgentCli::Gemini, "explicit", test_limits()),
            ModelAvailability::Unverifiable {
                reason: "Gemini model-catalog probing is not implemented by usagi".to_string()
            }
        );
        assert!(runner.calls.borrow().is_empty());

        // Exercise the public production implementation while staying on the
        // deterministic no-subprocess branch.
        assert_eq!(
            CliAgentModelProbe.probe_model(AgentCli::Claude, "explicit"),
            ModelAvailability::Unverifiable {
                reason: "Claude does not expose a model catalog that usagi can safely query"
                    .to_string()
            }
        );
    }

    #[test]
    fn cli_model_probe_preserves_command_failures() {
        let runner = FakeCatalogRunner::returning(Err("catalog timed out".to_string()));

        assert_eq!(
            probe_model_with(&runner, AgentCli::Codex, "model", test_limits()),
            ModelAvailability::Unverifiable {
                reason: "catalog timed out".to_string()
            }
        );
    }

    #[test]
    fn cli_model_probe_rejects_truncated_stdout() {
        let mut output = completed("ignored");
        output.stdout_truncated = true;
        let runner = FakeCatalogRunner::returning(Ok(output));

        assert_eq!(
            probe_model_with(&runner, AgentCli::Antigravity, "ignored", test_limits()),
            ModelAvailability::Unverifiable {
                reason: "`agy models` stdout exceeded the 64-byte limit".to_string()
            }
        );
    }

    #[test]
    fn cli_model_probe_reports_nonzero_exit_without_exposing_stderr() {
        let mut output = completed(Vec::new());
        output.success = false;
        output.status = "exit status: 2".to_string();
        let runner = FakeCatalogRunner::returning(Ok(output));
        assert_eq!(
            probe_model_with(&runner, AgentCli::Codex, "model", test_limits()),
            ModelAvailability::Unverifiable {
                reason: "`codex debug models` exited with exit status: 2; run the command directly for local diagnostics"
                    .to_string()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_model_probe_does_not_include_raw_stderr_in_its_error() {
        let result = probe_command_with(
            &SystemCatalogCommandRunner,
            "sh",
            &["-c", "printf super >&2; printf secret >&2; exit 2"],
            "model",
            parse_model_lines,
            test_limits(),
        );
        let result = format!("{result:?}");
        assert!(result.contains("exit status: 2"), "{result}");
        assert!(!result.contains("supersecret"), "{result}");
    }

    #[test]
    fn cli_model_probe_rejects_invalid_utf8_and_unparseable_catalogs() {
        let runner = FakeCatalogRunner::returning(Ok(completed(vec![0xff])));
        let invalid = probe_model_with(&runner, AgentCli::Antigravity, "model", test_limits());
        let invalid = format!("{invalid:?}");
        assert!(
            invalid.contains("`agy models` stdout was not valid UTF-8:"),
            "{invalid}"
        );

        let runner = FakeCatalogRunner::returning(Ok(completed("not json")));
        let malformed = probe_model_with(&runner, AgentCli::Codex, "model", test_limits());
        let malformed = format!("{malformed:?}");
        assert!(
            malformed.contains("could not parse `codex debug models` output:"),
            "{malformed}"
        );
    }

    #[test]
    fn require_available_model_accepts_only_a_positive_probe_and_trims_the_name() {
        let probe = ExpectedModelProbe {
            cli: AgentCli::Codex,
            model: "gpt-5-codex",
            availability: ModelAvailability::Available,
        };

        assert_eq!(
            require_available_model(&probe, AgentCli::Codex, "  gpt-5-codex  "),
            Ok(())
        );
    }

    #[test]
    fn require_available_model_rejects_a_blank_name_without_probing() {
        let probe = ExpectedModelProbe {
            cli: AgentCli::Claude,
            model: "must not be probed",
            availability: ModelAvailability::Available,
        };
        let error = require_available_model(&probe, AgentCli::Claude, " \n\t ")
            .expect_err("blank model must fail closed");

        assert_eq!(error, "model must not be blank for Claude");
    }

    #[test]
    fn require_available_model_reports_a_confirmed_absence_and_the_alternatives() {
        let probe = ExpectedModelProbe {
            cli: AgentCli::Claude,
            model: "missing",
            availability: ModelAvailability::Unavailable {
                available_models: vec!["sonnet".to_string(), "opus".to_string()],
            },
        };

        let error = require_available_model(&probe, AgentCli::Claude, "missing")
            .expect_err("an unavailable model must fail closed");
        assert_eq!(
            error,
            "model \"missing\" is not available for Claude (available models: sonnet, opus)"
        );
    }

    #[test]
    fn require_available_model_handles_an_authoritative_empty_list() {
        let probe = ExpectedModelProbe {
            cli: AgentCli::Antigravity,
            model: "missing",
            availability: ModelAvailability::Unavailable {
                available_models: Vec::new(),
            },
        };

        let error = require_available_model(&probe, AgentCli::Antigravity, "missing")
            .expect_err("an empty available-model list must fail closed");
        assert_eq!(
            error,
            "model \"missing\" is not available for Antigravity (the probe returned no available models)"
        );
    }

    #[test]
    fn require_available_model_rejects_an_inconclusive_probe() {
        let probe = ExpectedModelProbe {
            cli: AgentCli::Gemini,
            model: "gemini-pro",
            availability: ModelAvailability::Unverifiable {
                reason: "not authenticated".to_string(),
            },
        };

        let error = require_available_model(&probe, AgentCli::Gemini, "gemini-pro")
            .expect_err("an unverifiable model must fail closed");
        assert_eq!(
            error,
            "could not verify model \"gemini-pro\" for Gemini: not authenticated; clear the explicit model override to use the CLI default"
        );
    }

    #[test]
    fn parse_codex_debug_models_returns_trimmed_listed_slugs_in_first_seen_order() {
        let output = r#"{
            "models": [
                {"slug": " gpt-5-codex ", "visibility": "list"},
                {"slug": "internal", "visibility": "hidden"},
                {"slug": "gpt-5-mini", "visibility": "list"},
                {"slug": "gpt-5-codex", "visibility": "list"},
                {"slug": "missing-visibility"}
            ]
        }"#;

        assert_eq!(
            parse_codex_debug_models(output).unwrap(),
            vec!["gpt-5-codex".to_string(), "gpt-5-mini".to_string()]
        );
    }

    #[test]
    fn parse_codex_debug_models_rejects_invalid_json() {
        let error = parse_codex_debug_models("not json")
            .expect_err("malformed debug output must fail closed");

        assert!(
            error.starts_with("invalid `codex debug models` JSON:"),
            "{error}"
        );
    }

    #[test]
    fn parse_codex_debug_models_requires_an_object() {
        let error = parse_codex_debug_models("[]")
            .expect_err("a non-object debug response must fail closed");

        assert_eq!(error, "`codex debug models` output is not a JSON object");
    }

    #[test]
    fn parse_codex_debug_models_requires_a_models_array() {
        let error = parse_codex_debug_models(r#"{"models":{}}"#)
            .expect_err("a malformed models field must fail closed");

        assert_eq!(error, "`codex debug models` output has no models array");
    }

    #[test]
    fn parse_codex_debug_models_requires_object_entries() {
        let error = parse_codex_debug_models(r#"{"models":["gpt-5"]}"#)
            .expect_err("a malformed model entry must fail closed");

        assert_eq!(error, "`codex debug models` entry 1 is not an object");
    }

    #[test]
    fn parse_codex_debug_models_requires_a_string_slug_for_listed_entries() {
        let error = parse_codex_debug_models(r#"{"models":[{"slug":5,"visibility":"list"}]}"#)
            .expect_err("a malformed listed slug must fail closed");

        assert_eq!(error, "`codex debug models` entry 1 has no string slug");
    }

    #[test]
    fn parse_codex_debug_models_rejects_a_blank_listed_slug() {
        let error = parse_codex_debug_models(r#"{"models":[{"slug":"  ","visibility":"list"}]}"#)
            .expect_err("a blank listed slug must fail closed");

        assert_eq!(error, "`codex debug models` entry 1 has a blank slug");
    }

    #[test]
    fn parse_codex_debug_models_rejects_an_empty_selectable_set() {
        let error =
            parse_codex_debug_models(r#"{"models":[{"slug":"internal","visibility":"hidden"}]}"#)
                .expect_err("a response without listed models must fail closed");

        assert_eq!(error, "`codex debug models` returned no listed models");
    }

    #[test]
    fn parse_model_lines_removes_progress_blanks_and_duplicates() {
        let output =
            "  Fetching available models...  \r\n\r\n  Gemini Pro  \nGemini Flash\nGemini Pro\n";

        assert_eq!(
            parse_model_lines(output).unwrap(),
            vec!["Gemini Pro".to_string(), "Gemini Flash".to_string()]
        );
    }

    #[test]
    fn parse_model_lines_errors_when_only_progress_and_blanks_are_present() {
        let error = parse_model_lines("\n Fetching available models... \n\t\n")
            .expect_err("an empty model list must fail closed");

        assert_eq!(error, "agy models returned no available models");
    }

    #[test]
    fn available_clis_filters_to_installed_in_canonical_order() {
        // Only `claude` and `codex-fugu` are on the PATH: the result keeps them in
        // ALL order and drops the rest.
        let runner = FakeRunner(vec!["claude", "codex-fugu"]);
        assert_eq!(
            available_clis(&runner),
            vec![AgentCli::Claude, AgentCli::SakanaAi]
        );
    }

    #[test]
    fn available_clis_is_empty_when_none_installed() {
        assert!(available_clis(&FakeRunner(vec![])).is_empty());
    }

    #[test]
    fn available_clis_returns_all_when_everything_installed() {
        let runner = FakeRunner(vec!["claude", "codex", "codex-fugu", "gemini", "agy"]);
        assert_eq!(available_clis(&runner), AgentCli::ALL.to_vec());
    }

    #[test]
    fn fake_runner_non_probe_methods_are_inert() {
        // `available_clis` only calls `available`; the fake's other `CommandRunner`
        // methods exist solely to satisfy the trait. Exercise them so the double
        // is fully covered (mirroring the doctor module's fake-runner tests).
        let runner = FakeRunner(vec![]);
        assert!(runner.run("x", &[]).unwrap());
        assert!(runner.check("x", &[]));
        assert!(runner.spawn("x", &[]).is_ok());
    }

    #[test]
    fn mcp_capable_clis_filters_to_installed_and_mcp_supported() {
        let runner = FakeRunner(vec!["claude", "codex", "gemini"]);
        assert_eq!(
            mcp_capable_clis(&runner),
            vec![AgentCli::Claude, AgentCli::Codex, AgentCli::Gemini]
        );
    }

    fn base_wiring() -> AgentWiring {
        AgentWiring {
            usagi_bin: "usagi".to_string(),
            local_llm_model: Some("qwen2.5-coder:7b".to_string()),
            model: None,
            is_root: true,
            sandbox_writable_roots: vec![PathBuf::from("/old/git")],
        }
    }

    #[test]
    fn launch_wiring_carries_model_root_flag_and_git_common_dir() {
        let dir = Path::new("/repo/.usagi/sessions/fix");
        let wiring = wiring_for_launch(
            &base_wiring(),
            Some("gpt-5-codex".to_string()),
            dir,
            LaunchMode::Interactive,
            &|_| Some(PathBuf::from("/repo/.git")),
        );

        assert_eq!(wiring.usagi_bin, "usagi");
        assert_eq!(wiring.local_llm_model.as_deref(), Some("qwen2.5-coder:7b"));
        assert_eq!(wiring.model.as_deref(), Some("gpt-5-codex"));
        assert!(!wiring.is_root);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![
                PathBuf::from("/old/git"),
                PathBuf::from("/repo/.usagi"),
                PathBuf::from("/repo/.git")
            ]
        );
    }

    #[test]
    fn launch_wiring_still_carries_workspace_state_dir_when_git_common_dir_is_unresolved() {
        let dir = Path::new("/repo/.usagi/sessions/fix");
        let wiring = wiring_for_launch(&base_wiring(), None, dir, LaunchMode::Interactive, &|_| {
            None
        });

        assert_eq!(wiring.model, None);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git"), PathBuf::from("/repo/.usagi")]
        );
    }

    #[test]
    fn launch_wiring_carries_workspace_state_dir_for_workspace_root() {
        let dir = Path::new("/repo");
        let wiring = wiring_for_launch(&base_wiring(), None, dir, LaunchMode::Interactive, &|_| {
            None
        });

        assert!(wiring.is_root);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git"), PathBuf::from("/repo/.usagi")]
        );
    }

    #[test]
    fn launch_wiring_marks_workspace_root_and_skips_headless_sandbox_roots() {
        use std::cell::Cell;

        let dir = Path::new("/repo");
        let calls = Cell::new(0);
        let resolve_git_common_dir = |path: &Path| {
            calls.set(calls.get() + 1);
            Some(path.join(".git"))
        };
        assert_eq!(
            resolve_git_common_dir(dir),
            Some(PathBuf::from("/repo/.git"))
        );
        let calls_before_launch = calls.get();

        let wiring = wiring_for_launch(
            &base_wiring(),
            None,
            dir,
            LaunchMode::Headless,
            &resolve_git_common_dir,
        );

        assert!(wiring.is_root);
        assert_eq!(calls.get(), calls_before_launch);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git")]
        );
    }
}
