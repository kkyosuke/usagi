use crate::presentation::tui;

/// Entry point for `usagi hop`: shows the interactive launch screen.
pub fn run() -> anyhow::Result<()> {
    tui::launch::run()
}
