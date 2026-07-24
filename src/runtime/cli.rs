//! 完全な process argv を CLI parser に渡し、typed outcome を TUI / daemon / MCP
//! adapter へ接続する composition adapter。

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::usecase::claude_sandbox::{
    self, Platform, SandboxMode, SandboxPlan, SandboxRequest,
};
use usagi_core::usecase::client::{ClientError, ClientPolicy, DaemonClient, DaemonReply};
use usagi_tui::usecase::application::EntryScreen;

use super::{daemon, tui};

// 各 `RunOutcome` を実行面へ接続するだけの routing match。arm が増えて 100 行を超えるが、
// 分割しても routing の一覧性が下がるだけなので too_many_lines を許容する。
#[allow(clippy::too_many_lines)]
#[coverage(off)]
pub(crate) fn dispatch(
    args: Vec<std::ffi::OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    info: &AppInfo,
) -> std::io::Result<ExitCode> {
    match usagi_cli::cli::run(args, info.version, out, err)? {
        RunOutcome::Exit(code) => Ok(exit_code(code)),
        RunOutcome::LaunchTui(request) => {
            let entry = match request {
                TuiRequest::Welcome => EntryScreen::Welcome,
                TuiRequest::Workspace { path } => {
                    let path =
                        tui::resolve_workspace_path(&path.unwrap_or(std::env::current_dir()?))?;
                    EntryScreen::Workspace { path }
                }
                TuiRequest::Config => EntryScreen::Config,
                TuiRequest::Doctor => EntryScreen::Doctor,
            };
            tui::launch(out, info, &entry).map(|()| ExitCode::SUCCESS)
        }
        RunOutcome::LaunchDaemon(command) => {
            daemon::run(out, command, info).map(|()| ExitCode::SUCCESS)
        }
        RunOutcome::RequestDaemonReplacement => {
            match daemon::request_replacement(ClientPolicy::cli()) {
                Ok(trigger) => {
                    writeln!(
                        out,
                        "daemon replacement requested (operation {})",
                        trigger.operation_id.0
                    )?;
                    Ok(ExitCode::SUCCESS)
                }
                Err(error) => {
                    write_client_error(err, "daemon replacement refused", &error)?;
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        RunOutcome::LaunchMcp => {
            let stdin = std::io::stdin();
            match daemon::policy_client(ClientPolicy::mcp()) {
                Ok(mut client) => {
                    usagi_cli::mcp::serve_with_client(stdin.lock(), out, info.version, &mut client)
                        .map(|()| ExitCode::SUCCESS)
                }
                Err(error) => {
                    writeln!(err, "daemon unavailable: {error}")?;
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        RunOutcome::CaptureCodexSession => {
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
        RunOutcome::ReportAgentPhase { phase } => {
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
        RunOutcome::GuardWorkspace => guard_workspace(out),
        RunOutcome::ClaudeSandbox {
            mode,
            writable_roots,
            command,
        } => claude_sandbox(mode, writable_roots, command, err),
        RunOutcome::DaemonRequest(request) => match daemon::policy_client(ClientPolicy::cli()) {
            Ok(mut client) => write_daemon_outcome(client.request(request), out, err),
            Err(error) => {
                write_client_error(err, "daemon unavailable", &error)?;
                Ok(ExitCode::FAILURE)
            }
        },
        RunOutcome::SelfUpdate { command } => {
            let result = std::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .output()?;
            out.write_all(&result.stdout)?;
            err.write_all(&result.stderr)?;
            if result.status.success() {
                writeln!(out, "usagi was updated; restart it to use the new binary.")?;
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(exit_code(result.status.code().unwrap_or(1)))
            }
        }
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

// Claude を OS sandbox の中で fail-closed 起動する合成の縁。実 platform / backend / 環境の解決と
// exec を束ね、純粋な起動計画は `usagi_core::usecase::claude_sandbox` に委ねる。backend 不在・未対応
// platform では無保護フォールバックせず、拒否理由を stderr へ書いて失敗終了する。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=macos_wraps_claude_with_a_write_confining_profile
fn claude_sandbox(
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
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
    let request = SandboxRequest {
        platform,
        mode,
        backend: resolve_sandbox_backend(platform),
        launch_roots: writable_roots,
        tmpdir: std::env::var_os("TMPDIR").map(PathBuf::from),
        home: std::env::var_os("HOME").map(PathBuf::from),
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

// 対象 platform の sandbox backend を探索する。macOS は既定パスの `sandbox-exec`、Linux は
// PATH 上の `bwrap`。見つからなければ `None`（呼び出し側が fail-closed で拒否する）。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=a_missing_backend_is_rejected_on_each_supported_platform
fn resolve_sandbox_backend(platform: Platform) -> Option<PathBuf> {
    match platform {
        Platform::MacOs => {
            let path = PathBuf::from("/usr/bin/sandbox-exec");
            path.exists().then_some(path)
        }
        Platform::Linux => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join("bwrap"))
                .find(|candidate| candidate.is_file())
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
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    #![coverage(off)]

    use std::io::{self, Write};

    use usagi_core::infrastructure::ipc::{build_identity, build_rollover_trigger};
    use usagi_core::usecase::client::{ClientError, DaemonReply};

    use super::{write_client_error, write_daemon_outcome};

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken output"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn accepted_reply_propagates_output_failure() {
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
}
