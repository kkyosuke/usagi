use crate::infrastructure::storage::Storage;
use crate::presentation::tui;
use crate::usecase::settings;

/// Entry point for `usagi hop`: shows the interactive welcome screen.
pub fn run() -> anyhow::Result<()> {
    if notifications_enabled() {
        notify_hop();
    }
    tui::app::run()
}

/// Whether desktop notifications are enabled in the user's settings.
///
/// Defaults to enabled when settings cannot be loaded, so a missing or
/// unreadable config never silently suppresses notifications.
fn notifications_enabled() -> bool {
    Storage::open_default()
        .and_then(|storage| settings::load(&storage))
        .map(|settings| settings.notifications_enabled)
        .unwrap_or(true)
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
