//! CLI 面の outcome を TUI / daemon adapter へ接続する composition adapter。

use std::io::Write;

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::usecase::client::{ClientPolicy, DaemonClient, DaemonReply};
use usagi_tui::usecase::application::EntryScreen;

use super::{daemon, tui};

#[coverage(off)]
pub(crate) fn dispatch(
    args: Vec<std::ffi::OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    info: &AppInfo,
) -> std::io::Result<()> {
    match usagi_cli::cli::run(args, info.version, out, err)? {
        RunOutcome::Exit(code) => std::process::exit(code),
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
            tui::launch(out, info, &entry)
        }
        RunOutcome::DaemonRequest(request) => match daemon::client(ClientPolicy::cli()) {
            Ok(mut client) => match client.request(request) {
                Ok(DaemonReply::Accepted {
                    operation_id,
                    revision,
                    ..
                }) => writeln!(
                    out,
                    "accepted operation {operation_id} (revision {revision})"
                ),
                Ok(DaemonReply::Ok(value)) => writeln!(out, "{value}"),
                Err(error) => {
                    writeln!(err, "daemon request failed: {error}")?;
                    Ok(())
                }
            },
            Err(error) => {
                writeln!(err, "daemon unavailable: {error}")?;
                Ok(())
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
                writeln!(out, "usagi was updated; restart it to use the new binary.")
            } else {
                std::process::exit(result.status.code().unwrap_or(1));
            }
        }
    }
}
