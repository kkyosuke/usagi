//! 完全な process argv を CLI parser に渡し、typed outcome を TUI / daemon / MCP
//! adapter へ接続する composition adapter。

use std::io::Write;
use std::process::ExitCode;

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::usecase::client::{ClientError, ClientPolicy, DaemonClient, DaemonReply};
use usagi_tui::usecase::application::EntryScreen;

use super::{daemon, tui};

#[coverage(off)]
#[allow(clippy::too_many_lines)] // typed outcome を各実行面へ接続する 1 か所の dispatch。分割せず一望する。
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
            match daemon::client(ClientPolicy::mcp()) {
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
            match daemon::client(ClientPolicy::cli()) {
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
        RunOutcome::GuardWorkspace => guard_workspace(out),
        RunOutcome::ClaudeSandbox {
            mode,
            writable_roots,
            command,
        } => claude_sandbox(&mode, writable_roots, &command, err),
        RunOutcome::DaemonRequest(request) => match daemon::client(ClientPolicy::cli()) {
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

// fail-closed な OS sandbox 内で Claude を起動する合成の縁。実 cwd / home の解決・macOS の
// 継承 sandbox 判定・実プロセス起動を束ね、純粋な組み立て（run_in / platform_command）へ委ねる。
// sandbox を用意できなければ Claude を無保護で起動せず FAILURE を返す。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=canonical_roots_resolve_symlinks_dedupe_and_reject_missing_paths
fn claude_sandbox(
    mode: &str,
    mut writable_roots: Vec<std::path::PathBuf>,
    command: &[std::ffi::OsString],
    err: &mut dyn Write,
) -> std::io::Result<ExitCode> {
    use usagi_cli::cli::sandbox;

    let result = (|| -> Result<(), String> {
        // mode 文字列を検証する（session / root 以外は fail-closed で拒否）。cwd の扱いはどちらも同じ。
        sandbox::Mode::parse(mode)?;
        let cwd =
            std::env::current_dir().map_err(|e| format!("failed to resolve sandbox cwd: {e}"))?;
        // Claude は credential / 会話を、hook / MCP は usagi の state をここに書く。通常の一時領域も
        // socket / lock / 一時ファイルのため writable にする。
        let home =
            dirs::home_dir().ok_or_else(|| "cannot resolve the home directory".to_string())?;
        let claude_state = home.join(".claude");
        std::fs::create_dir_all(&claude_state)
            .map_err(|e| format!("failed to prepare Claude's sandbox state directory: {e}"))?;
        writable_roots.push(claude_state);
        writable_roots.push(home.join(".usagi"));
        writable_roots.extend(sandbox::writable_temp_roots());
        #[cfg(target_os = "macos")]
        writable_roots.extend(sandbox::macos_keychain_roots(&home)?);
        // cwd（session mode では session worktree、root mode では project root）は意図した作業領域。
        writable_roots.push(cwd);

        // usagi 自身が既に macOS sandbox 内で動いている場合、子は親の policy を継承するため、
        // 二重に profile を張れない。その場合だけ Claude を直接起動して親の境界を保つ。
        #[cfg(target_os = "macos")]
        if macos_inherits_sandbox()? {
            return run_inside_inherited_sandbox(command);
        }

        let writable_roots = sandbox::canonical_roots(writable_roots)?;
        let plan = sandbox::platform_command(&writable_roots, command)?;
        let status = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .status()
            .map_err(|e| format!("failed to start OS sandbox {}: {e}", plan.program.display()))?;
        if !status.success() {
            return Err(format!("sandboxed Claude exited with {status}"));
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) => {
            writeln!(err, "Claude sandbox refused: {error}")?;
            Ok(ExitCode::FAILURE)
        }
    }
}

/// 現在のプロセスが既に macOS Seatbelt profile を持ち、nested な `sandbox-exec` を拒否するか。
#[cfg(target_os = "macos")]
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=nested_sandbox_probe_accepts_only_the_known_seatbelt_rejection
fn macos_inherits_sandbox() -> Result<bool, String> {
    let output = std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .map_err(|e| format!("failed to probe the macOS Claude sandbox: {e}"))?;
    Ok(!output.status.success()
        && usagi_cli::cli::sandbox::is_nested_sandbox_rejection(&output.stderr))
}

/// 親 profile を継承する子プロセスとして、二重 profile 無しで Claude を起動する。
#[cfg(target_os = "macos")]
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=macos_profile_allows_only_canonical_write_roots
fn run_inside_inherited_sandbox(command: &[std::ffi::OsString]) -> Result<(), String> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| "Claude sandbox refused an empty command".to_string())?;
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("failed to start Claude inside the inherited macOS sandbox: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Claude inside the inherited macOS sandbox exited with {status}"
        ));
    }
    Ok(())
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
