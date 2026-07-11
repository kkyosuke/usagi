use anyhow::Result;

/// Entry point for `usagi hop`: shows the interactive welcome screen.
///
/// The interactive TUI itself is injected as `app` so this entry point is
/// unit-testable without driving a real terminal: production passes
/// [`crate::presentation::tui::app::run`], tests pass a stub. Generic over the
/// runner, so the lib build that backs the integration tests never instantiates
/// the (terminal-bound) real path here.
pub fn run(app: impl FnOnce() -> Result<()>) -> Result<()> {
    app()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn run_returns_ok_when_the_tui_exits_cleanly() {
        assert!(run(|| Ok(())).is_ok());
    }

    #[test]
    fn run_propagates_a_tui_error() {
        let result = run(|| bail!("TUI error"));
        assert_eq!(result.unwrap_err().to_string(), "TUI error");
    }
}
