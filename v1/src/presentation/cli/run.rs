use anyhow::Result;
use console::Term;

#[cfg(not(test))]
use crate::presentation::tui::gallery::run as gallery_run;
#[cfg(test)]
use tests::mock_gallery_run as gallery_run;

/// Entry point for `usagi run <N>`: play one of the usagi animations full-screen.
pub fn run(n: u8) -> Result<()> {
    gallery_run(&Term::stdout(), n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::cell::RefCell;

    thread_local! {
        /// The number `run` forwarded, and the result the mock returns.
        static MOCK: RefCell<(Option<u8>, Result<(), &'static str>)> =
            const { RefCell::new((None, Ok(()))) };
    }

    pub(super) fn mock_gallery_run(_term: &Term, n: u8) -> Result<()> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = Some(n);
            match m.1 {
                Ok(()) => Ok(()),
                Err(e) => bail!(e),
            }
        })
    }

    #[test]
    fn forwards_the_number_to_the_gallery_and_returns_ok() {
        MOCK.with(|m| *m.borrow_mut() = (None, Ok(())));
        assert!(run(3).is_ok());
        // The chosen animation number is passed straight through.
        MOCK.with(|m| assert_eq!(m.borrow().0, Some(3)));
    }

    #[test]
    fn propagates_a_gallery_error() {
        MOCK.with(|m| *m.borrow_mut() = (None, Err("boom")));
        let err = run(1).unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }
}
