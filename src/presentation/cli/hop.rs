#[cfg(not(test))]
use crate::presentation::tui::app::run as app_run;

#[cfg(test)]
use tests::mock_app_run as app_run;

/// Entry point for `usagi hop`: shows the interactive welcome screen.
pub fn run() -> anyhow::Result<()> {
    app_run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::cell::RefCell;

    thread_local! {
        static MOCK_RESULT: RefCell<Option<Result<(), &'static str>>> = const { RefCell::new(None) };
    }

    pub(super) fn mock_app_run() -> anyhow::Result<()> {
        MOCK_RESULT.with(|res| {
            match res.borrow_mut().take() {
                Some(Ok(())) => Ok(()),
                Some(Err(e)) => bail!(e),
                None => Ok(()), // default
            }
        })
    }

    #[test]
    fn test_run_success() {
        MOCK_RESULT.with(|res| *res.borrow_mut() = Some(Ok(())));
        let result = run();
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_error() {
        MOCK_RESULT.with(|res| *res.borrow_mut() = Some(Err("TUI error")));
        let result = run();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "TUI error");
    }
}
