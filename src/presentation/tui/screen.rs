use std::io;

use anyhow::Result;
use console::{Key, Term};

/// Source of key presses driving an interactive screen.
///
/// Abstracting the read lets event loops be exercised without a real terminal:
/// tests supply a scripted sequence of keys.
pub trait KeyReader {
    fn read_key(&mut self) -> io::Result<Key>;
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_writes_farewell_when_not_dismissed() {
        let guard = AlternateScreenGuard::new(Term::stdout()).unwrap();
        // Dropping without dismissing takes the farewell branch.
        drop(guard);
    }

    #[test]
    fn dismiss_suppresses_farewell() {
        let mut guard = AlternateScreenGuard::new(Term::stdout()).unwrap();
        guard.dismiss();
        // Dropping after dismiss skips the farewell branch.
        drop(guard);
    }
}
