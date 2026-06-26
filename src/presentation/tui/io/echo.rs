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
    /// The terminal fd whose `ECHO` flag was cleared, restored on drop.
    fd: libc::c_int,
    /// The flags captured before we cleared `ECHO`, restored on drop. `None`
    /// when there was nothing to change (no TTY), so drop leaves it alone.
    saved: Option<libc::termios>,
}

#[cfg(unix)]
impl EchoGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::for_fd(libc::STDIN_FILENO)
    }

    /// Build a guard that clears (and restores) `ECHO` on `fd`. [`new`](Self::new)
    /// uses stdin; the fd is a parameter so the guard can be exercised against a
    /// pseudo-terminal in tests rather than the process's real stdin.
    fn for_fd(fd: libc::c_int) -> Self {
        Self {
            fd,
            saved: disable_echo(fd),
        }
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(original) = self.saved.take() {
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &original);
            }
        }
    }
}

/// Clear the `ECHO` line-discipline flag on `fd`, returning the prior `termios`
/// for restoration. Only `ECHO` is touched, so canonical mode and output
/// post-processing keep working. Returns `None` if `fd` is not a terminal (or the
/// flags could not be applied).
#[cfg(unix)]
fn disable_echo(fd: libc::c_int) -> Option<libc::termios> {
    use std::mem::MaybeUninit;

    unsafe {
        let mut current = MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(fd, current.as_mut_ptr()) != 0 {
            return None;
        }
        let original = current.assume_init();
        let mut modified = original;
        modified.c_lflag &= !libc::ECHO;
        (libc::tcsetattr(fd, libc::TCSANOW, &modified) == 0).then_some(original)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;

    /// Open a pseudo-terminal and return its (master, slave) fds. The slave is a
    /// real TTY, so `termios` reads/writes against it behave like the live stdin
    /// the guard manages in production.
    fn open_pty() -> (libc::c_int, libc::c_int) {
        let mut master = MaybeUninit::<libc::c_int>::uninit();
        let mut slave = MaybeUninit::<libc::c_int>::uninit();
        let rc = unsafe {
            libc::openpty(
                master.as_mut_ptr(),
                slave.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        unsafe { (master.assume_init(), slave.assume_init()) }
    }

    fn echo_enabled(fd: libc::c_int) -> bool {
        unsafe {
            let mut t = MaybeUninit::<libc::termios>::uninit();
            assert_eq!(libc::tcgetattr(fd, t.as_mut_ptr()), 0);
            (t.assume_init().c_lflag & libc::ECHO) != 0
        }
    }

    #[test]
    fn guard_clears_echo_on_a_tty_and_restores_it_on_drop() {
        let (master, slave) = open_pty();
        assert!(echo_enabled(slave), "a fresh pty starts with echo on");
        {
            let _guard = EchoGuard::for_fd(slave);
            assert!(
                !echo_enabled(slave),
                "the guard clears echo for its lifetime"
            );
        }
        // Dropping the guard restores the original (echoing) flags.
        assert!(echo_enabled(slave), "drop restores the prior echo setting");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    #[test]
    fn guard_is_a_no_op_when_the_fd_is_not_a_terminal() {
        // A non-tty fd (-1) cannot have its termios read, so nothing is saved and
        // drop has nothing to restore.
        let guard = EchoGuard::for_fd(-1);
        assert!(guard.saved.is_none());
        drop(guard);
    }

    #[test]
    fn new_targets_stdin_without_panicking() {
        // `new` wires the guard to the real stdin fd; whether stdin is a tty
        // depends on the environment, so this just exercises the constructor.
        let _guard = EchoGuard::new();
    }
}
