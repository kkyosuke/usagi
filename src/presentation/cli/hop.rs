use crate::presentation::tui;

/// Entry point for `usagi hop`: shows the interactive welcome screen.
pub fn run() -> anyhow::Result<()> {
    notify_hop();
    tui::welcome::run()
}

/// Show a desktop notification when hopping in.
///
/// Best-effort: notification failures (e.g. headless/CI environments without a
/// notification daemon) are intentionally ignored so they never block `hop`.
fn notify_hop() {
    let _ = notify_rust::Notification::new()
        .summary("usagi")
        .body("🐇 hop しました！")
        .show();
}
