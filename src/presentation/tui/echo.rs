//! Suppressing terminal echo for the TUI's lifetime.
//!
//! `console` toggles raw mode per keypress and restores the original (cooked,
//! echoing) mode in between. With mouse reporting on (see [`super::screen`]), a
//! fast wheel spin floods stdin with report sequences, and the kernel echoes
//! those bytes to the screen in the gaps between reads — the stray
//! `^[[<64;…M` noise. Turning echo off for the whole TUI suppresses that while
//! leaving output post-processing (so `\n` keeps mapping to `\r\n` and the
//! screens render normally) untouched.
//!
//! This is platform terminal I/O (a thin `termios` wrapper, no-op off Unix), so
//! it is excluded from coverage.

/// RAII guard that disables terminal echo on creation and restores the previous
/// setting on drop. A no-op when the echo state cannot be read (e.g. stdin is
/// not a TTY) or on non-Unix platforms.
#[cfg(unix)]
pub struct EchoGuard {
    /// The flags captured before we cleared `ECHO`, restored on drop. `None`
    /// when there was nothing to change (no TTY), so drop leaves it alone.
    saved: Option<libc::termios>,
}

#[cfg(unix)]
impl EchoGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            saved: disable_echo(),
        }
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(original) = self.saved.take() {
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
            }
        }
    }
}

/// Clear the `ECHO` line-discipline flag on stdin, returning the prior `termios`
/// for restoration. Only `ECHO` is touched, so canonical mode and output
/// post-processing keep working. Returns `None` if stdin is not a terminal.
#[cfg(unix)]
fn disable_echo() -> Option<libc::termios> {
    use std::mem::MaybeUninit;

    let fd = libc::STDIN_FILENO;
    unsafe {
        let mut current = MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(fd, current.as_mut_ptr()) != 0 {
            return None;
        }
        let original = current.assume_init();
        let mut modified = original;
        modified.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(fd, libc::TCSANOW, &modified) != 0 {
            return None;
        }
        Some(original)
    }
}

/// On non-Unix platforms there is no `termios` echo flag to manage, so the guard
/// is an inert placeholder.
#[cfg(not(unix))]
pub struct EchoGuard;

#[cfg(not(unix))]
impl EchoGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self
    }
}
