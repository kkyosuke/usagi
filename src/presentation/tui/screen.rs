use anyhow::Result;
use console::Term;

/// RAII guard that activates the terminal alternate screen and restores it on drop.
pub struct AlternateScreenGuard {
    term: Term,
    farewell: bool,
}

impl AlternateScreenGuard {
    pub fn new(term: Term) -> Result<Self> {
        term.write_str("\x1b[?1049h")?;
        term.hide_cursor()?;
        Ok(Self {
            term,
            farewell: true,
        })
    }

    /// Suppresses the farewell message on drop (e.g. when exiting with an error).
    pub fn dismiss(&mut self) {
        self.farewell = false;
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.term.write_str("\x1b[?1049l");
        let _ = self.term.show_cursor();
        if self.farewell {
            let _ = self.term.write_line("USAGI run away ( ^-^)ノ");
        }
    }
}
