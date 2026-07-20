//! CLI 面の outcome を TUI / daemon adapter へ接続する composition adapter。

use std::io::Write;
use std::process::ExitCode;

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::usecase::client::{ClientError, ClientPolicy, DaemonClient, DaemonReply};
use usagi_tui::usecase::application::EntryScreen;

use super::{daemon, tui};

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

    use usagi_core::usecase::client::DaemonReply;

    use super::write_daemon_outcome;

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
}
