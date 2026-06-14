use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::echo::EchoGuard;

/// Source of key presses driving an interactive screen.
///
/// Abstracting the read lets event loops be exercised without a real terminal:
/// tests supply a scripted sequence of keys.
pub trait KeyReader {
    fn read_key(&mut self) -> io::Result<Key>;
}

/// Enter the alternate screen.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
/// Leave the alternate screen, restoring the prior contents.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";
/// Disable alternate scroll mode (DECSET 1007). Some terminals enable it by
/// default, which makes the mouse wheel emit arrow-key presses while the
/// alternate screen is active — those synthetic arrows would scroll the usagi
/// lists and leak into the embedded terminal's shell. Turning it off helps on
/// terminals that honour it, but many (e.g. Apple Terminal.app) ignore it
/// entirely, so it is only a first line of defence — see [`ENABLE_MOUSE`].
const DISABLE_ALT_SCROLL: &str = "\x1b[?1007l";
/// Re-enable alternate scroll mode, restoring the terminal's usual behaviour
/// once the TUI exits.
const ENABLE_ALT_SCROLL: &str = "\x1b[?1007h";
/// Enable mouse reporting: normal tracking (DECSET 1000) plus SGR extended
/// coordinates (DECSET 1006). With reporting on, the terminal hands wheel and
/// click events to us as escape sequences instead of scrolling its own
/// viewport or synthesising arrow keys — so the wheel can no longer move the
/// usagi lists or leak into the embedded shell. The reported events are then
/// dropped by the key reader (see `term_reader`). This is the robust stop that
/// works even where alternate scroll mode (1007) is ignored.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
/// Disable mouse reporting, restoring the terminal's normal wheel / selection
/// behaviour once the TUI exits. Reset in the reverse order of [`ENABLE_MOUSE`].
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1000l";

/// RAII guard that activates the terminal alternate screen and restores it on drop.
pub struct AlternateScreenGuard {
    term: Term,
    farewell: bool,
    /// Disables terminal echo while the TUI is up so the mouse-report flood that
    /// arrives once [`ENABLE_MOUSE`] is set is not echoed to the screen between
    /// `console`'s per-key raw reads. Dropped (echo restored) after this guard's
    /// own `drop` body runs.
    _echo: EchoGuard,
}

impl AlternateScreenGuard {
    pub fn new(term: Term) -> Result<Self> {
        let echo = EchoGuard::new();
        term.write_str(ENTER_ALT_SCREEN)?;
        term.write_str(DISABLE_ALT_SCROLL)?;
        term.write_str(ENABLE_MOUSE)?;
        term.hide_cursor()?;
        Ok(Self {
            term,
            farewell: true,
            _echo: echo,
        })
    }

    /// Suppresses the farewell message on drop (e.g. when exiting with an error).
    pub fn dismiss(&mut self) {
        self.farewell = false;
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.term.write_str(DISABLE_MOUSE);
        let _ = self.term.write_str(ENABLE_ALT_SCROLL);
        let _ = self.term.write_str(LEAVE_ALT_SCREEN);
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
