use anyhow::Result;
use console::Term;

/// RAII guard that activates the terminal alternate screen and restores it on drop.
pub struct AlternateScreenGuard {
    term: Term,
}

impl AlternateScreenGuard {
    pub fn new(term: Term) -> Result<Self> {
        term.write_str("\x1b[?1049h")?;
        term.hide_cursor()?;
        Ok(Self { term })
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.term.write_str("\x1b[?1049l");
        let _ = self.term.show_cursor();
        let _ = self.term.write_line("USAGI run away ( ^-^)ノ");
    }
}
