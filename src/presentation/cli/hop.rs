use crate::presentation::tui;

/// Entry point for `usagi hop`: shows the interactive startup screen.
pub fn run() -> anyhow::Result<()> {
    tui::home::run()
}
