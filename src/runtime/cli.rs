//! 完全な process argv を CLI parser に渡し、typed outcome を TUI / daemon / MCP
//! adapter へ接続する composition adapter。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use usagi_cli::cli::{InstallerRequest, RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::usecase::claude_sandbox::{
    self, Platform, SandboxMode, SandboxPlan, SandboxRequest,
};
use usagi_core::usecase::client::{ClientError, ClientPolicy, DaemonClient, DaemonReply};
use usagi_tui::usecase::application::EntryScreen;

use super::{daemon, tui};

// 各 `RunOutcome` を実行面へ接続するだけの routing match。arm が増えて 100 行を超えるが、
// 分割しても routing の一覧性が下がるだけなので too_many_lines を許容する。
#[coverage(off)] // Parsed action to production stdio composition.
pub(crate) fn dispatch(
    args: Vec<std::ffi::OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    info: &AppInfo,
) -> std::io::Result<ExitCode> {
    let outcome = usagi_cli::cli::run(args, info.version, out, err)?;
    let action = Action::from(&outcome);
    action_io::execute_action(action, outcome, out, err, info)
}

enum Action {
    Exit,
    LaunchWelcome,
    LaunchWorkspace,
    LaunchConfig,
    LaunchDoctor,
    LaunchDaemon,
    RequestDaemonReplacement,
    LaunchMcp,
    CaptureCodexSession,
    ReportAgentPhase,
    GuardWorkspace,
    ClaudeSandbox,
    DaemonRequest,
    SelfUpdate,
}

impl From<&RunOutcome> for Action {
    fn from(outcome: &RunOutcome) -> Self {
        match outcome {
            RunOutcome::Exit(_) => Self::Exit,
            RunOutcome::LaunchTui(TuiRequest::Welcome) => Self::LaunchWelcome,
            RunOutcome::LaunchTui(TuiRequest::Workspace { .. }) => Self::LaunchWorkspace,
            RunOutcome::LaunchTui(TuiRequest::Config) => Self::LaunchConfig,
            RunOutcome::LaunchTui(TuiRequest::Doctor) => Self::LaunchDoctor,
            RunOutcome::LaunchDaemon(_) => Self::LaunchDaemon,
            RunOutcome::RequestDaemonReplacement { .. } => Self::RequestDaemonReplacement,
            RunOutcome::LaunchMcp => Self::LaunchMcp,
            RunOutcome::CaptureCodexSession => Self::CaptureCodexSession,
            RunOutcome::ReportAgentPhase { .. } => Self::ReportAgentPhase,
            RunOutcome::GuardWorkspace => Self::GuardWorkspace,
            RunOutcome::ClaudeSandbox { .. } => Self::ClaudeSandbox,
            RunOutcome::DaemonRequest(_) => Self::DaemonRequest,
            RunOutcome::SelfUpdate(_) => Self::SelfUpdate,
        }
    }
}

// Each action arm binds a fully classified route to production stdin, process,
// daemon, or terminal IO. The classification above remains coverage-visible.
mod action_io {
    #![coverage(off)]

    use super::{
        Action, AppInfo, ClientPolicy, DaemonClient, DaemonReply, EntryScreen, ExitCode,
        LauncherPolicyInputs, RunOutcome, TuiRequest, Write, claude_sandbox, daemon,
        execute_self_update, exit_code, guard_workspace, tui, write_client_error,
        write_daemon_outcome,
    };

    #[allow(clippy::too_many_lines)]
    pub(super) fn execute_action(
        action: Action,
        outcome: RunOutcome,
        out: &mut dyn Write,
        err: &mut dyn Write,
        info: &AppInfo,
    ) -> std::io::Result<ExitCode> {
        match (action, outcome) {
            (Action::Exit, RunOutcome::Exit(code)) => Ok(exit_code(code)),
            (Action::LaunchWelcome, RunOutcome::LaunchTui(TuiRequest::Welcome)) => {
                tui::launch(out, info, &EntryScreen::Welcome).map(|()| ExitCode::SUCCESS)
            }
            (Action::LaunchWorkspace, RunOutcome::LaunchTui(TuiRequest::Workspace { path })) => {
                let path = tui::resolve_workspace_path(&path.unwrap_or(std::env::current_dir()?))?;
                tui::launch(out, info, &EntryScreen::Workspace { path }).map(|()| ExitCode::SUCCESS)
            }
            (Action::LaunchConfig, RunOutcome::LaunchTui(TuiRequest::Config)) => {
                tui::launch(out, info, &EntryScreen::Config).map(|()| ExitCode::SUCCESS)
            }
            (Action::LaunchDoctor, RunOutcome::LaunchTui(TuiRequest::Doctor)) => {
                tui::launch(out, info, &EntryScreen::Doctor).map(|()| ExitCode::SUCCESS)
            }
            (Action::LaunchDaemon, RunOutcome::LaunchDaemon(command)) => {
                daemon::run(out, command, info, None).map(|()| ExitCode::SUCCESS)
            }
            (Action::RequestDaemonReplacement, RunOutcome::RequestDaemonReplacement { force }) => {
                match daemon::replace_running_daemon(out, ClientPolicy::cli(), force, info)? {
                    Ok(()) => Ok(ExitCode::SUCCESS),
                    Err(error) => {
                        write_client_error(err, "daemon replacement refused", &error)?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::LaunchMcp, RunOutcome::LaunchMcp) => {
                let stdin = std::io::stdin();
                match daemon::policy_client(ClientPolicy::mcp()) {
                    Ok(mut client) => {
                        let credential = match client
                            .request(usagi_core::usecase::client::DaemonRequest::McpChildClaim)
                        {
                            Ok(DaemonReply::Ok(body)) => body
                                .get("credential")
                                .and_then(serde_json::Value::as_str)
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned),
                            _ => None,
                        };
                        if let Some(credential) = credential {
                            usagi_cli::mcp::serve_with_client_and_caller(
                                stdin.lock(),
                                out,
                                info.version,
                                &mut client,
                                &credential,
                            )
                        } else {
                            // Manual MCP remains useful for unprivileged store and
                            // observation tools; caller-scoped mutation stays absent.
                            usagi_cli::mcp::serve_with_client(
                                stdin.lock(),
                                out,
                                info.version,
                                &mut client,
                            )
                        }
                        .map(|()| ExitCode::SUCCESS)
                    }
                    Err(error) => {
                        writeln!(err, "daemon unavailable: {error}")?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::CaptureCodexSession, RunOutcome::CaptureCodexSession) => {
                let stdin = std::io::stdin();
                let mut input = stdin.lock();
                let request = match usagi_cli::cli::hooks::codex_session_capture::request_from_hook(
                    &mut input, None,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        writeln!(err, "Codex session capture failed: {error}")?;
                        return Ok(ExitCode::FAILURE);
                    }
                };
                // A lifecycle hook reports to the daemon that launched this agent, so
                // that daemon is running by construction. Attach to it instead of
                // taking the cold-start path: the agent runs inside a sandbox whose
                // writable roots exclude the data home, so `bootstrap.lock` is
                // `PermissionDenied` there and every hook would fall through to the
                // broker and report `daemon transport is unavailable`. A per-tool-call
                // hook must not start a daemon, and must not pay bootstrap latency.
                match daemon::attached_client(ClientPolicy::cli()) {
                    Ok(mut client) => match client.request(request) {
                        Ok(_) => Ok(ExitCode::SUCCESS),
                        Err(error) => {
                            write_client_error(err, "Codex session capture failed", &error)?;
                            Ok(ExitCode::FAILURE)
                        }
                    },
                    Err(error) => {
                        write_client_error(err, "Codex session capture failed", &error)?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::ReportAgentPhase, RunOutcome::ReportAgentPhase { phase }) => {
                let stdin = std::io::stdin();
                let mut input = stdin.lock();
                let request = match usagi_cli::cli::hooks::agent_phase::request_from_hook(
                    &mut input, &phase, None,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        writeln!(err, "agent phase report failed: {error}")?;
                        return Ok(ExitCode::FAILURE);
                    }
                };
                // A lifecycle hook reports to the daemon that launched this agent, so
                // that daemon is running by construction. Attach to it instead of
                // taking the cold-start path: the agent runs inside a sandbox whose
                // writable roots exclude the data home, so `bootstrap.lock` is
                // `PermissionDenied` there and every hook would fall through to the
                // broker and report `daemon transport is unavailable`. A per-tool-call
                // hook must not start a daemon, and must not pay bootstrap latency.
                match daemon::attached_client(ClientPolicy::cli()) {
                    Ok(mut client) => match client.request(request) {
                        Ok(_) => Ok(ExitCode::SUCCESS),
                        Err(error) => {
                            write_client_error(err, "agent phase report failed", &error)?;
                            Ok(ExitCode::FAILURE)
                        }
                    },
                    Err(error) => {
                        write_client_error(err, "agent phase report failed", &error)?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::GuardWorkspace, RunOutcome::GuardWorkspace) => guard_workspace(out),
            (
                Action::ClaudeSandbox,
                RunOutcome::ClaudeSandbox {
                    mode,
                    protected_root,
                    backend,
                    tmpdir,
                    home,
                    cache_dir,
                    writable_roots,
                    command,
                },
            ) => claude_sandbox(
                mode,
                LauncherPolicyInputs {
                    protected_root,
                    backend,
                    tmpdir,
                    home,
                    cache_dir,
                    writable_roots,
                },
                command,
                err,
            ),
            (Action::DaemonRequest, RunOutcome::DaemonRequest(request)) => {
                match daemon::policy_client(ClientPolicy::cli()) {
                    Ok(mut client) => write_daemon_outcome(client.request(request), out, err),
                    Err(error) => {
                        write_client_error(err, "daemon unavailable", &error)?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::SelfUpdate, RunOutcome::SelfUpdate(request)) => {
                execute_self_update(&request, out, err)
            }
            _ => unreachable!("action classification and outcome diverged"),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=installer_identity_failure_has_zero_process_effects
fn execute_self_update(
    request: &InstallerRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    execute_self_update_with(request, out, err, &mut |script, select_version| {
        use std::process::{Command, Stdio};

        let mut command = Command::new("bash");
        command
            .arg("-s")
            .arg("--")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if select_version {
            command.arg("--select-version");
        }
        let mut child = command.spawn()?;
        let write_result = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("installer stdin is unavailable"))?
            .write_all(script);
        if let Err(error) = write_result {
            let _ = child.wait();
            return Err(error);
        }
        child.wait_with_output()
    })
}

type InstallerLauncher<'a> = dyn FnMut(&[u8], bool) -> std::io::Result<std::process::Output> + 'a;

fn execute_self_update_with(
    request: &InstallerRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
    launch: &mut InstallerLauncher<'_>,
) -> std::io::Result<ExitCode> {
    let Some(script) = request.verified_script() else {
        writeln!(
            err,
            "self-update refused: embedded installer identity is invalid"
        )?;
        return Ok(ExitCode::FAILURE);
    };
    let result = launch(script, request.select_version())?;
    out.write_all(&result.stdout)?;
    err.write_all(&result.stderr)?;
    if result.status.success() {
        writeln!(out, "usagi was updated; restart it to use the new binary.")?;
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(exit_code(result.status.code().unwrap_or(1)))
    }
}

// Claude `PreToolUse` フックの実 stdin を束ね、純粋な判定 usecase に委ねる合成の縁。
// deny は終了コードではなく stdout の JSON payload で伝えるため、常に成功終了する。
#[coverage(off)] // coverage: reason=composition owner=root-cli expires=2027-01-31 tests=denies_a_tool_targeting_the_parent_repo
fn guard_workspace(out: &mut dyn Write) -> std::io::Result<ExitCode> {
    let stdin = std::io::stdin();
    usagi_cli::cli::hooks::guard_workspace::evaluate(&mut stdin.lock(), out)?;
    Ok(ExitCode::SUCCESS)
}

// Claude を OS sandbox の中で fail-closed 起動する合成の縁。実 platform / daemon-issued policy の再検証と
// exec を束ね、純粋な起動計画は `usagi_core::usecase::claude_sandbox` に委ねる。backend 不在・未対応
// platform では無保護フォールバックせず、拒否理由を stderr へ書いて失敗終了する。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=macos_wraps_claude_with_a_write_confining_profile
fn claude_sandbox(
    mode: SandboxMode,
    policy: LauncherPolicyInputs,
    command: Vec<String>,
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    let platform = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Unsupported
    };
    if let Err(reason) = validate_launcher_policy_inputs(&policy) {
        writeln!(err, "claude-sandbox: {reason:?}")?;
        return Ok(ExitCode::FAILURE);
    }
    let request = SandboxRequest {
        platform,
        mode,
        protected_root: policy.protected_root,
        backend: policy.backend,
        launch_roots: policy.writable_roots,
        tmpdir: policy.tmpdir,
        home: policy.home,
        cache_dir: policy.cache_dir,
        // E2E テスト専用 seam。release ビルドでは `cfg!(debug_assertions)` が false になるため、
        // 配布バイナリはこの環境変数を見ても拘束を外さない。
        passthrough: claude_sandbox::passthrough_requested(
            cfg!(debug_assertions),
            std::env::var(claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE)
                .ok()
                .as_deref(),
        ),
        command,
    };
    match claude_sandbox::plan(&request) {
        SandboxPlan::Launch { program, argv } => exec_sandbox(&program, &argv, err),
        SandboxPlan::Reject { reason } => {
            writeln!(err, "claude-sandbox: {reason}")?;
            Ok(ExitCode::FAILURE)
        }
    }
}

/// launcher が exec 直前に検証する policy path 一式。同じ `Option<PathBuf>` が並ぶため、
/// 位置引数ではなく名前付きで渡す（順序を取り違えても型では気づけない）。
#[derive(Default)]
struct LauncherPolicyInputs {
    protected_root: Option<PathBuf>,
    backend: Option<PathBuf>,
    tmpdir: Option<PathBuf>,
    home: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    writable_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherPolicyError {
    Backend,
    ProtectedRoot,
    WritableRoot,
}

fn validate_launcher_policy_inputs(
    policy: &LauncherPolicyInputs,
) -> Result<(), LauncherPolicyError> {
    let LauncherPolicyInputs {
        protected_root,
        backend,
        tmpdir,
        home,
        cache_dir,
        writable_roots,
    } = policy;
    let (protected_root, backend, tmpdir, home, cache_dir) = (
        protected_root.as_deref(),
        backend.as_deref(),
        tmpdir.as_deref(),
        home.as_deref(),
        cache_dir.as_deref(),
    );
    if let Some(backend) = backend {
        if !backend.is_absolute() {
            return Err(LauncherPolicyError::Backend);
        }
        let metadata =
            std::fs::symlink_metadata(backend).map_err(|_| LauncherPolicyError::Backend)?;
        if !metadata.file_type().is_file() {
            return Err(LauncherPolicyError::Backend);
        }
        if backend.canonicalize().ok().as_deref() != Some(backend) {
            return Err(LauncherPolicyError::Backend);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(LauncherPolicyError::Backend);
            }
        }
    }
    for &protected_root in protected_root.as_slice() {
        validate_launcher_directory(protected_root, LauncherPolicyError::ProtectedRoot)?;
    }
    for root in writable_roots {
        validate_launcher_directory(root, LauncherPolicyError::WritableRoot)?;
    }
    for root in [tmpdir, home, cache_dir].into_iter().flatten() {
        validate_launcher_directory(root, LauncherPolicyError::WritableRoot)?;
    }
    Ok(())
}

fn validate_launcher_directory(
    path: &Path,
    error: LauncherPolicyError,
) -> Result<(), LauncherPolicyError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(error);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| error)?;
    if !metadata.file_type().is_dir() {
        return Err(error);
    }
    if path.canonicalize().ok().as_deref() != Some(path) {
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(error);
        }
    }
    Ok(())
}

// daemon bootstrap の trusted environment で sandbox backend を一度だけ探索する。macOS は既定パスの
// `sandbox-exec`、Linux は PATH 上の `bwrap` を canonical absolute path にする。Agent child は再探索しない。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=a_missing_backend_is_rejected_on_each_supported_platform
pub(crate) fn resolve_sandbox_backend(platform: Platform) -> Option<PathBuf> {
    match platform {
        Platform::MacOs => {
            let path = PathBuf::from("/usr/bin/sandbox-exec");
            path.canonicalize().ok()
        }
        Platform::Linux => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join("bwrap"))
                .find(|candidate| candidate.is_file())
                .and_then(|candidate| candidate.canonicalize().ok())
        }),
        Platform::Unsupported => None,
    }
}

// backend を現在のプロセスに置き換えて起動する。exec は成功時に戻らないため、戻った場合は
// 失敗であり、理由を stderr に書いて失敗終了する。unix 以外では plan() が既に拒否している。
#[cfg(unix)]
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=macos_wraps_claude_with_a_write_confining_profile
fn exec_sandbox(
    program: &std::path::Path,
    argv: &[String],
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(program).args(argv).exec();
    writeln!(
        err,
        "claude-sandbox: {} を exec できません: {error}",
        program.display()
    )?;
    Ok(ExitCode::FAILURE)
}

#[cfg(not(unix))]
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=unsupported_platform_never_launches_unprotected
fn exec_sandbox(
    _program: &std::path::Path,
    _argv: &[String],
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    writeln!(err, "claude-sandbox: OS sandbox は unix でのみ利用できます")?;
    Ok(ExitCode::FAILURE)
}

#[coverage(off)] // coverage: reason=composition owner=root-cli expires=2027-01-31 tests=cli_daemon_reply_contract_maps_stdout_stderr_and_exit_code
fn write_daemon_outcome(
    outcome: Result<DaemonReply, ClientError>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    match outcome {
        Ok(DaemonReply::Accepted {
            operation_id,
            revision,
            ..
        }) => {
            let message = format!("accepted operation {operation_id} (revision {revision})");
            writeln!(out, "{message}")?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(DaemonReply::Ok(value)) => {
            writeln!(out, "{value}")?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            write_client_error(err, "daemon request failed", &error)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=root-cli expires=2027-01-31 tests=cli_daemon_reply_contract_maps_stdout_stderr_and_exit_code
fn write_client_error(
    err: &mut dyn Write,
    context: &str,
    error: &ClientError,
) -> std::io::Result<()> {
    match error {
        ClientError::Protocol(error) => {
            let code = serde_json::to_value(error.code)
                .expect("error code serializes")
                .as_str()
                .expect("error code serializes as a string")
                .to_owned();
            writeln!(
                err,
                "{context} [{code}; error_id={}]: {}",
                error.error_id, error.message
            )
        }
        ClientError::Unavailable(_) => writeln!(
            err,
            "{context} [unavailable]: daemon transport is unavailable"
        ),
        ClientError::RolloverRequired(trigger) => writeln!(
            err,
            "{context} [busy; operation_id={}]: daemon build rollover is required; the current daemon remains running",
            trigger.operation_id.0
        ),
        ClientError::BuildIdentityUnavailable => writeln!(
            err,
            "{context} [unavailable]: exact daemon build identity is unavailable; the current daemon remains running"
        ),
        ClientError::Lifecycle(message) => {
            writeln!(err, "{context} [unavailable]: {message}")
        }
        ClientError::BootstrapContended => writeln!(
            err,
            "{context} [busy]: another usagi process is establishing the daemon connection; try again"
        ),
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Render a failure that reached the process boundary, as the message the
/// caller wrote rather than as the Rust value carrying it.
///
/// Returning `io::Result` from `main` makes Rust print the error with `Debug`,
/// so a carefully written `io::Error::other("…")` reaches the terminal spelled
/// `Error: Custom { kind: Other, error: "…" }`. Every failing CLI path shares
/// that boundary, so the rendering belongs here rather than at each call site.
pub(crate) fn write_process_failure(err: &mut dyn Write, error: &std::io::Error) {
    // The message is all the operator can act on, and every failure this
    // boundary sees already carries one. Writing to a closed stderr cannot be
    // reported anywhere, so it is deliberately ignored.
    let _ = writeln!(err, "error: {error}");
}

/// Turn what [`dispatch`] returned into the status the process exits with,
/// reporting a failure on the way out.
///
/// Keeping this separate from `main` is what makes both arms testable: `main`
/// then holds only the real argv and stdio it binds.
pub(crate) fn process_outcome(result: std::io::Result<ExitCode>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            write_process_failure(err, &error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{self, Write};
    use std::path::PathBuf;

    use usagi_cli::cli::{DaemonCommand, InstallerRequest, RunOutcome, TuiRequest};
    use usagi_core::infrastructure::ipc::{build_identity, build_rollover_trigger};
    use usagi_core::usecase::claude_sandbox::SandboxMode;
    use usagi_core::usecase::client::{ClientError, DaemonReply, DaemonRequest};

    use super::{
        Action, ExitCode, LauncherPolicyError, LauncherPolicyInputs, execute_self_update_with,
        exit_code, process_outcome, validate_launcher_policy_inputs, write_client_error,
        write_daemon_outcome,
    };

    struct BrokenWriter;

    #[test]
    fn launcher_policy_rejects_root_and_symlink_inputs() {
        let root = tempfile::tempdir().unwrap();
        let protected = root.path().canonicalize().unwrap();
        let inputs = |tmpdir: Option<&std::path::Path>, cache_dir: Option<&std::path::Path>| {
            LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                tmpdir: tmpdir.map(std::path::Path::to_path_buf),
                cache_dir: cache_dir.map(std::path::Path::to_path_buf),
                ..LauncherPolicyInputs::default()
            }
        };
        assert_eq!(
            validate_launcher_policy_inputs(&inputs(Some(std::path::Path::new("/")), None)),
            Err(LauncherPolicyError::WritableRoot)
        );
        assert_eq!(
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(PathBuf::from("/")),
                ..LauncherPolicyInputs::default()
            }),
            Err(LauncherPolicyError::ProtectedRoot)
        );
        assert_eq!(
            validate_launcher_policy_inputs(&inputs(
                Some(&protected.join("missing-writable-root")),
                None
            )),
            Err(LauncherPolicyError::WritableRoot)
        );
        // 存在しない cache root も他の policy path と同じく拒否する。
        assert_eq!(
            validate_launcher_policy_inputs(&inputs(None, Some(&protected.join("missing-cache")))),
            Err(LauncherPolicyError::WritableRoot)
        );

        let backend = tempfile::NamedTempFile::new().unwrap();
        let backend_path = backend.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backend_path, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        // backend・tmpdir・home・cache root・起動固有 root がすべて所有された canonical
        // directory なら受け入れる。
        assert_eq!(
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                backend: Some(backend_path),
                tmpdir: Some(protected.clone()),
                home: Some(protected.clone()),
                cache_dir: Some(protected.clone()),
                writable_roots: vec![protected.clone()],
            }),
            Ok(())
        );

        #[cfg(unix)]
        assert_eq!(
            validate_launcher_policy_inputs(&inputs(Some(std::path::Path::new("/usr")), None)),
            Err(LauncherPolicyError::WritableRoot)
        );
    }

    #[test]
    fn launcher_policy_rejects_unusable_sandbox_backends() {
        let root = tempfile::tempdir().unwrap();
        let protected = root.path().canonicalize().unwrap();
        let backend = tempfile::NamedTempFile::new().unwrap();
        let backend_path = backend.path().canonicalize().unwrap();
        let refused = |backend: PathBuf| {
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                backend: Some(backend),
                ..LauncherPolicyInputs::default()
            })
        };
        // 存在しない / 相対 / directory / 実行 bit の無い file はいずれも拒否する。
        assert_eq!(
            refused(protected.join("missing-sandbox-backend")),
            Err(LauncherPolicyError::Backend)
        );
        assert_eq!(
            refused(PathBuf::from("Cargo.toml")),
            Err(LauncherPolicyError::Backend)
        );
        assert_eq!(
            refused(PathBuf::from("/usr")),
            Err(LauncherPolicyError::Backend)
        );
        assert_eq!(refused(backend_path), Err(LauncherPolicyError::Backend));
    }

    #[cfg(unix)]
    #[test]
    fn launcher_policy_rejects_direct_and_parent_symlink_aliases() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let protected_dir = tempfile::tempdir().unwrap();
        let protected = protected_dir.path().canonicalize().unwrap();
        let alias = protected.with_extension("alias");
        symlink(&protected, &alias).unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                tmpdir: Some(alias.clone()),
                ..LauncherPolicyInputs::default()
            }),
            Err(LauncherPolicyError::WritableRoot)
        );
        std::fs::remove_file(alias).unwrap();

        let real_parent_dir = tempfile::tempdir().unwrap();
        let real_parent = real_parent_dir.path().canonicalize().unwrap();
        let parent_alias = real_parent.with_extension("parent-alias");
        symlink(&real_parent, &parent_alias).unwrap();
        let directory = real_parent.join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                tmpdir: Some(parent_alias.join("directory")),
                ..LauncherPolicyInputs::default()
            }),
            Err(LauncherPolicyError::WritableRoot)
        );
        let executable = real_parent.join("executable");
        std::fs::write(&executable, "fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(&LauncherPolicyInputs {
                protected_root: Some(protected.clone()),
                backend: Some(parent_alias.join("executable")),
                ..LauncherPolicyInputs::default()
            }),
            Err(LauncherPolicyError::Backend)
        );
        std::fs::remove_file(parent_alias).unwrap();
    }

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken output"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailOnSecondWrite {
        writes: usize,
    }

    impl Write for FailOnSecondWrite {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 2 {
                Err(io::Error::other("second write failed"))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn process_exit_codes_are_bounded_to_the_platform_representation() {
        assert_eq!(exit_code(0), std::process::ExitCode::SUCCESS);
        assert_eq!(exit_code(7), std::process::ExitCode::from(7));
        assert_eq!(exit_code(-1), std::process::ExitCode::from(1));
        assert_eq!(exit_code(256), std::process::ExitCode::from(1));
    }

    #[test]
    fn every_parsed_outcome_maps_to_one_typed_runtime_action() {
        let assert_route = |outcome: RunOutcome, expected: Action| {
            assert_eq!(
                std::mem::discriminant(&Action::from(&outcome)),
                std::mem::discriminant(&expected)
            );
        };
        assert_route(RunOutcome::Exit(7), Action::Exit);
        assert_route(
            RunOutcome::LaunchTui(TuiRequest::Welcome),
            Action::LaunchWelcome,
        );
        assert_route(
            RunOutcome::LaunchTui(TuiRequest::Workspace {
                path: Some(PathBuf::from("workspace")),
            }),
            Action::LaunchWorkspace,
        );
        assert_route(
            RunOutcome::LaunchTui(TuiRequest::Config),
            Action::LaunchConfig,
        );
        assert_route(
            RunOutcome::LaunchTui(TuiRequest::Doctor),
            Action::LaunchDoctor,
        );
        assert_route(
            RunOutcome::LaunchDaemon(DaemonCommand::Start),
            Action::LaunchDaemon,
        );
        assert_route(
            RunOutcome::RequestDaemonReplacement { force: true },
            Action::RequestDaemonReplacement,
        );
        assert_route(RunOutcome::LaunchMcp, Action::LaunchMcp);
        assert_route(RunOutcome::CaptureCodexSession, Action::CaptureCodexSession);
        assert_route(
            RunOutcome::ReportAgentPhase {
                phase: "working".into(),
            },
            Action::ReportAgentPhase,
        );
        assert_route(RunOutcome::GuardWorkspace, Action::GuardWorkspace);
        assert_route(
            RunOutcome::ClaudeSandbox {
                mode: SandboxMode::Session,
                protected_root: None,
                backend: None,
                tmpdir: None,
                home: None,
                cache_dir: None,
                writable_roots: vec![PathBuf::from("worktree")],
                command: vec!["claude".into()],
            },
            Action::ClaudeSandbox,
        );
        assert_route(
            RunOutcome::DaemonRequest(DaemonRequest::Rollover {
                operation_id: "operation".into(),
            }),
            Action::DaemonRequest,
        );
        assert_route(
            RunOutcome::SelfUpdate(InstallerRequest::new(b"", [0; 32], false)),
            Action::SelfUpdate,
        );
    }

    #[test]
    fn installer_identity_failure_has_zero_process_effects() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("usagi");
        std::fs::write(&installed, b"old binary bytes").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o751)).unwrap();
        }
        let before = std::fs::read(&installed).unwrap();
        let before_mode = mode(&installed);

        for request in [
            InstallerRequest::new(b"complete installer", [0; 32], false),
            InstallerRequest::new(b"#!/bin/bash\ntrunc", [1; 32], true),
        ] {
            let launches = Cell::new(0);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut launch = |_: &[u8], _: bool| {
                launches.set(launches.get() + 1);
                Ok(process_output(0, b"", b""))
            };
            launch(&[], false).unwrap();
            launches.set(0);
            let status =
                execute_self_update_with(&request, &mut out, &mut err, &mut launch).unwrap();

            assert_eq!(status, std::process::ExitCode::FAILURE);
            assert_eq!(launches.get(), 0);
            assert!(out.is_empty());
            assert_eq!(
                String::from_utf8(err).unwrap(),
                "self-update refused: embedded installer identity is invalid\n"
            );
            assert_eq!(std::fs::read(&installed).unwrap(), before);
            assert_eq!(mode(&installed), before_mode);
        }
    }

    #[test]
    fn verified_installer_maps_process_output_and_io_failures() {
        let request = InstallerRequest::new(
            b"",
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ],
            true,
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let status =
            execute_self_update_with(&request, &mut out, &mut err, &mut |script, selected| {
                assert!(script.is_empty());
                assert!(selected);
                Ok(process_output(0, b"installed\n", b"warning\n"))
            })
            .unwrap();
        assert_eq!(status, std::process::ExitCode::SUCCESS);
        assert_eq!(
            out,
            b"installed\nusagi was updated; restart it to use the new binary.\n"
        );
        assert_eq!(err, b"warning\n");

        let status =
            execute_self_update_with(&request, &mut Vec::new(), &mut Vec::new(), &mut |_, _| {
                Ok(process_output(7, b"", b"failed\n"))
            })
            .unwrap();
        assert_eq!(status, std::process::ExitCode::from(7));

        let launch_error =
            execute_self_update_with(&request, &mut Vec::new(), &mut Vec::new(), &mut |_, _| {
                Err(io::Error::other("launch failed"))
            })
            .unwrap_err();
        assert_eq!(launch_error.to_string(), "launch failed");

        let output_error =
            execute_self_update_with(&request, &mut BrokenWriter, &mut Vec::new(), &mut |_, _| {
                Ok(process_output(0, b"output", b""))
            })
            .unwrap_err();
        assert_eq!(output_error.kind(), io::ErrorKind::Other);

        let stderr_error =
            execute_self_update_with(&request, &mut Vec::new(), &mut BrokenWriter, &mut |_, _| {
                Ok(process_output(0, b"", b"warning"))
            })
            .unwrap_err();
        assert_eq!(stderr_error.kind(), io::ErrorKind::Other);

        let mut completion_writer = FailOnSecondWrite::default();
        completion_writer.flush().unwrap();
        let completion_error = execute_self_update_with(
            &request,
            &mut completion_writer,
            &mut Vec::new(),
            &mut |_, _| Ok(process_output(0, b"output", b"")),
        )
        .unwrap_err();
        assert_eq!(completion_error.to_string(), "second write failed");

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let status = execute_self_update_with(
                &request,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut |_, _| {
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(9),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                },
            )
            .unwrap();
            assert_eq!(status, std::process::ExitCode::FAILURE);
        }

        let invalid = InstallerRequest::new(b"invalid", [0; 32], false);
        let launches = Cell::new(0);
        let mut launch = |_: &[u8], _: bool| {
            launches.set(launches.get() + 1);
            Ok(process_output(0, b"", b""))
        };
        launch(&[], false).unwrap();
        launches.set(0);
        let identity_error =
            execute_self_update_with(&invalid, &mut Vec::new(), &mut BrokenWriter, &mut launch)
                .unwrap_err();
        assert_eq!(identity_error.kind(), io::ErrorKind::Other);
        assert_eq!(launches.get(), 0);
    }

    fn process_output(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> std::process::Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit_code << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit_code as u32)
        };
        std::process::Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn mode(path: &std::path::Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode()
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }

    #[test]
    fn accepted_reply_propagates_output_failure() {
        BrokenWriter.flush().unwrap();
        let result = write_daemon_outcome(
            Ok(DaemonReply::Accepted {
                operation_id: "operation".into(),
                revision: 1,
                body: serde_json::json!(null),
            }),
            &mut BrokenWriter,
            &mut Vec::new(),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
    }

    #[test]
    fn ok_and_error_replies_render_stdout_and_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = write_daemon_outcome(
            Ok(DaemonReply::Ok(serde_json::json!({"result": "done"}))),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(status, std::process::ExitCode::SUCCESS);
        assert_eq!(String::from_utf8(out).unwrap(), "{\"result\":\"done\"}\n");
        assert!(err.is_empty());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = write_daemon_outcome(
            Err(ClientError::Unavailable("offline".into())),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(status, std::process::ExitCode::FAILURE);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "daemon request failed [unavailable]: daemon transport is unavailable\n"
        );
    }

    #[test]
    fn build_identity_failures_render_typed_effect_free_messages() {
        let running = build_identity("1", "a", "test", "debug", &"a".repeat(64));
        let expected = build_identity("1", "b", "test", "debug", &"b".repeat(64));
        let trigger = build_rollover_trigger(&running, &expected, "local", false).unwrap();
        let mut rollover = Vec::new();
        write_client_error(
            &mut rollover,
            "replacement",
            &ClientError::RolloverRequired(trigger.clone()),
        )
        .unwrap();
        let rendered = String::from_utf8(rollover).unwrap();
        assert!(rendered.contains("[busy; operation_id="));
        assert!(rendered.contains(&trigger.operation_id.0));
        assert!(rendered.contains("current daemon remains running"));

        let mut unknown = Vec::new();
        write_client_error(
            &mut unknown,
            "replacement",
            &ClientError::BuildIdentityUnavailable,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(unknown).unwrap(),
            "replacement [unavailable]: exact daemon build identity is unavailable; the current daemon remains running\n"
        );
    }

    /// The message the failing path wrote is what reaches the terminal. The
    /// `io::Error` carrying it — its `Debug` spelling, its `ErrorKind`, the
    /// escaping `Debug` adds — must not appear.
    #[test]
    fn a_process_failure_renders_as_its_message_and_not_as_a_rust_value() {
        let mut err = Vec::new();
        let code = process_outcome(
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "refusing to stop the daemon: the daemon still owns 1 Agent runtime(s)",
            )),
            &mut err,
        );
        let rendered = String::from_utf8(err).unwrap();
        assert_eq!(
            rendered,
            "error: refusing to stop the daemon: the daemon still owns 1 Agent runtime(s)\n"
        );
        assert!(!rendered.contains("Custom"), "{rendered}");
        assert!(!rendered.contains("WouldBlock"), "{rendered}");
        assert!(!rendered.contains('\\'), "{rendered}");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
    }

    /// A successful dispatch keeps the exit code it chose and writes nothing to
    /// stderr, so a command that reports a refusal itself is not reported twice.
    #[test]
    fn a_successful_outcome_keeps_its_exit_code_and_stays_quiet() {
        let mut err = Vec::new();
        let code = process_outcome(Ok(exit_code(3)), &mut err);
        assert!(err.is_empty());
        assert_eq!(format!("{code:?}"), format!("{:?}", exit_code(3)));
    }

    /// Bootstrap contention renders as busy-and-retryable, not as an absent
    /// daemon: the daemon may be running and healthy while another process is
    /// simply mid-connect.
    #[test]
    fn bootstrap_contention_renders_as_busy_rather_than_unavailable() {
        let mut contended = Vec::new();
        write_client_error(
            &mut contended,
            "session list",
            &ClientError::BootstrapContended,
        )
        .unwrap();
        let rendered = String::from_utf8(contended).unwrap();
        assert_eq!(
            rendered,
            "session list [busy]: another usagi process is establishing the daemon connection; try again\n"
        );
        assert!(!rendered.contains("unavailable"));
    }
}
