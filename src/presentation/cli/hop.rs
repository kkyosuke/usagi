use crate::presentation::tui;

/// Entry point for `usagi hop`: shows the interactive welcome screen.
pub fn run() -> anyhow::Result<()> {
    tui::welcome::run()
}
