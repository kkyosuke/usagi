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
        Action, AppInfo, ClientPolicy, DaemonClient, EntryScreen, ExitCode, LauncherPolicyInputs,
        RunOutcome, TuiRequest, Write, claude_sandbox, daemon, execute_self_update, exit_code,
        guard_workspace, tui, write_client_error, write_daemon_outcome,
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
                    Ok(mut client) => usagi_cli::mcp::serve_with_client(
                        stdin.lock(),
                        out,
                        info.version,
                        &mut client,
                    )
                    .map(|()| ExitCode::SUCCESS),
                    Err(error) => {
                        writeln!(err, "daemon unavailable: {error}")?;
                        Ok(ExitCode::FAILURE)
                    }
                }
            }
            (Action::CaptureCodexSession, RunOutcome::CaptureCodexSession) => {
                let stdin = std::io::stdin();
                let mut input = stdin.lock();
                let credential = std::env::var("USAGI_MCP_CALLER_CREDENTIAL").ok();
                let request = match usagi_cli::cli::hooks::codex_session_capture::request_from_hook(
                    &mut input, credential,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        writeln!(err, "Codex session capture failed: {error}")?;
                        return Ok(ExitCode::FAILURE);
                    }
                };
                match daemon::policy_client(ClientPolicy::cli()) {
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
                let credential = std::env::var("USAGI_MCP_CALLER_CREDENTIAL").ok();
                let request = match usagi_cli::cli::hooks::agent_phase::request_from_hook(
                    &mut input, &phase, credential,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        writeln!(err, "agent phase report failed: {error}")?;
                        return Ok(ExitCode::FAILURE);
                    }
                };
                match daemon::policy_client(ClientPolicy::cli()) {
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
    execute_self_update_with(request, out, err, |script, select_version| {
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

fn execute_self_update_with<F>(
    request: &InstallerRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
    launch: F,
) -> std::io::Result<ExitCode>
where
    F: FnOnce(&[u8], bool) -> std::io::Result<std::process::Output>,
{
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
    if let Err(reason) = validate_launcher_policy_inputs(
        policy.protected_root.as_deref(),
        policy.backend.as_deref(),
        policy.tmpdir.as_deref(),
        policy.home.as_deref(),
        &policy.writable_roots,
    ) {
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

struct LauncherPolicyInputs {
    protected_root: Option<PathBuf>,
    backend: Option<PathBuf>,
    tmpdir: Option<PathBuf>,
    home: Option<PathBuf>,
    writable_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherPolicyError {
    Backend,
    ProtectedRoot,
    WritableRoot,
}

fn validate_launcher_policy_inputs(
    protected_root: Option<&Path>,
    backend: Option<&Path>,
    tmpdir: Option<&Path>,
    home: Option<&Path>,
    writable_roots: &[PathBuf],
) -> Result<(), LauncherPolicyError> {
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
    for root in [tmpdir, home].into_iter().flatten() {
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

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;

    use usagi_cli::cli::{DaemonCommand, InstallerRequest, RunOutcome, TuiRequest};
    use usagi_core::infrastructure::ipc::{build_identity, build_rollover_trigger};
    use usagi_core::usecase::claude_sandbox::SandboxMode;
    use usagi_core::usecase::client::{ClientError, DaemonReply, DaemonRequest};

    use super::{
        Action, LauncherPolicyError, execute_self_update_with, exit_code,
        validate_launcher_policy_inputs, write_client_error, write_daemon_outcome,
    };

    struct BrokenWriter;

    #[test]
    fn launcher_policy_rejects_root_and_symlink_inputs() {
        let root = tempfile::tempdir().unwrap();
        let protected = root.path().canonicalize().unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                None,
                Some(std::path::Path::new("/")),
                None,
                &[],
            ),
            Err(LauncherPolicyError::WritableRoot)
        );
        assert_eq!(
            validate_launcher_policy_inputs(Some(std::path::Path::new("/")), None, None, None, &[],),
            Err(LauncherPolicyError::ProtectedRoot)
        );
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                None,
                Some(&protected.join("missing-writable-root")),
                None,
                &[],
            ),
            Err(LauncherPolicyError::WritableRoot)
        );
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                Some(&protected.join("missing-sandbox-backend")),
                None,
                None,
                &[],
            ),
            Err(LauncherPolicyError::Backend)
        );
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                Some(std::path::Path::new("Cargo.toml")),
                None,
                None,
                &[],
            ),
            Err(LauncherPolicyError::Backend)
        );
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                Some(std::path::Path::new("/usr")),
                None,
                None,
                &[],
            ),
            Err(LauncherPolicyError::Backend)
        );

        let backend = tempfile::NamedTempFile::new().unwrap();
        let backend_path = backend.path().canonicalize().unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(Some(&protected), Some(&backend_path), None, None, &[],),
            Err(LauncherPolicyError::Backend)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backend_path, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                Some(&backend_path),
                Some(&protected),
                Some(&protected),
                std::slice::from_ref(&protected),
            ),
            Ok(())
        );

        #[cfg(unix)]
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                None,
                Some(std::path::Path::new("/usr")),
                None,
                &[],
            ),
            Err(LauncherPolicyError::WritableRoot)
        );
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
            validate_launcher_policy_inputs(Some(&protected), None, Some(&alias), None, &[]),
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
            validate_launcher_policy_inputs(
                Some(&protected),
                None,
                Some(&parent_alias.join("directory")),
                None,
                &[],
            ),
            Err(LauncherPolicyError::WritableRoot)
        );
        let executable = real_parent.join("executable");
        std::fs::write(&executable, "fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            validate_launcher_policy_inputs(
                Some(&protected),
                Some(&parent_alias.join("executable")),
                None,
                None,
                &[],
            ),
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
            let mut launches = 0;
            let mut out = Vec::new();
            let mut err = Vec::new();
            let status = execute_self_update_with(
                &request,
                &mut out,
                &mut err,
                |_, _| -> io::Result<std::process::Output> {
                    launches += 1;
                    unreachable!("identity failure must precede process launch")
                },
            )
            .unwrap();

            assert_eq!(status, std::process::ExitCode::FAILURE);
            assert_eq!(launches, 0);
            assert!(out.is_empty());
            assert_eq!(
                String::from_utf8(err).unwrap(),
                "self-update refused: embedded installer identity is invalid\n"
            );
            assert_eq!(std::fs::read(&installed).unwrap(), before);
            assert_eq!(mode(&installed), before_mode);
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
