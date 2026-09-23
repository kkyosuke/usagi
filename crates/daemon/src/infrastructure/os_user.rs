//! The platform adapter that answers who this process runs as.
//!
//! The name comes from the kernel's own view of the process — its effective UID
//! resolved through the passwd database — and never from an inherited `USER`,
//! because an inherited value is only a string the launching environment chose.
//! A child that authenticates against an OS keychain is indexed by the user it
//! actually runs as, so the daemon has to answer the question the same way.
//!
//! Resolution is a single library call, not a subprocess: the daemon asks once
//! at startup and every PTY launch afterwards reuses that answer.
//!
//! The two decisions the lookup makes — how far to grow the scratch buffer, and
//! what one `getpwuid_r` return means — are pure functions here
//! ([`grown`], [`Attempt::of`]) with their own tests. Only the syscall itself is
//! excluded from coverage, so a wrong retry bound or an inverted null check
//! cannot hide behind the platform call.

use std::ffi::CStr;

/// The first `getpwuid_r` scratch buffer. NSS and Open Directory entries are far
/// smaller than this in practice, so the first call normally succeeds.
const INITIAL_BUFFER_BYTES: usize = 1024;

/// The largest scratch buffer the resolver will grow to before reporting
/// failure. A passwd entry that does not fit here is not one this daemon can
/// use, and an unbounded retry loop would be a memory amplifier driven by the
/// platform's directory service.
const MAXIMUM_BUFFER_BYTES: usize = 64 * 1024;

/// What one `getpwuid_r` return means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// The entry was written and its name can be read.
    Resolved,
    /// The scratch buffer was too small; retry with a larger one.
    Grow,
    /// No entry for this UID, or a platform that cannot answer.
    Unavailable,
}

impl Attempt {
    /// Classifies a completed call. Both glibc and Darwin return the errno as
    /// the result value (never `-1`), and report "no such user" as success with
    /// a null result pointer, so a zero return alone does not mean resolved.
    fn of(code: libc::c_int, found: bool, named: bool) -> Self {
        if code == libc::ERANGE {
            Self::Grow
        } else if code == 0 && found && named {
            Self::Resolved
        } else {
            Self::Unavailable
        }
    }
}

/// The next scratch capacity to try, or `None` once the bound is reached.
fn grown(capacity: usize) -> Option<usize> {
    (capacity < MAXIMUM_BUFFER_BYTES).then(|| capacity.saturating_mul(2))
}

/// The owned name of a resolved entry, or `None` for one this daemon cannot
/// carry in an environment value. A passwd name is bytes to the platform, but an
/// environment value here is a `String`, so a non-UTF-8 name is treated as no
/// answer and falls back like any other unresolvable one.
fn owned_name(name: &CStr) -> Option<String> {
    name.to_str().ok().map(str::to_owned)
}

/// The OS user name for this process's effective UID, or `None` when the
/// platform cannot answer.
///
/// `None` is a real outcome, not an error to log at the boundary: a process
/// whose UID has no passwd entry (a container without a matching user) still has
/// to be able to open a PTY. The caller decides the fallback.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=terminal_user_environment
#[must_use]
pub fn effective_user_name() -> Option<String> {
    // SAFETY: `geteuid` reads this process's own credentials and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let mut capacity = INITIAL_BUFFER_BYTES;
    loop {
        // SAFETY: `passwd` is a plain C struct of pointers and integers, and
        // `getpwuid_r` overwrites every field it reports as found.
        let mut entry = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut buffer = vec![0 as libc::c_char; capacity];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `entry`, `buffer` and `found` are live for the call, and the
        // buffer pointer and length describe the same allocation.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        // Both pointers are read before the outcome is known, which POSIX leaves
        // unspecified for a failed call. That is only a null comparison, never a
        // dereference, and `entry` is zeroed before every call, so the bytes are
        // initialized whatever the platform did or did not write. Do not turn
        // either read into a dereference.
        match Attempt::of(code, !found.is_null(), !entry.pw_name.is_null()) {
            Attempt::Grow => capacity = grown(capacity)?,
            Attempt::Unavailable => return None,
            // SAFETY: a resolved entry's `pw_name` is a NUL-terminated string
            // inside `buffer`, which outlives this borrow.
            Attempt::Resolved => return owned_name(unsafe { CStr::from_ptr(entry.pw_name) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Attempt, INITIAL_BUFFER_BYTES, MAXIMUM_BUFFER_BYTES, grown, owned_name};
    use std::ffi::CString;

    #[test]
    fn a_name_the_environment_cannot_carry_is_no_answer() {
        assert_eq!(
            owned_name(&CString::new("kyosuke").unwrap()).as_deref(),
            Some("kyosuke")
        );
        // A passwd name is bytes to the platform; this one is not UTF-8.
        assert_eq!(owned_name(&CString::new([0xff, 0xfe]).unwrap()), None);
    }

    #[test]
    fn a_too_small_buffer_grows_up_to_the_bound_and_then_gives_up() {
        let mut capacity = INITIAL_BUFFER_BYTES;
        let mut attempts = 0;
        while let Some(larger) = grown(capacity) {
            assert!(larger > capacity, "a retry must enlarge the buffer");
            capacity = larger;
            attempts += 1;
            assert!(attempts < 64, "the retry loop must be bounded");
        }
        assert!(capacity >= MAXIMUM_BUFFER_BYTES);
        assert_eq!(grown(capacity), None);
    }

    #[test]
    fn only_a_written_and_named_entry_counts_as_resolved() {
        assert_eq!(Attempt::of(0, true, true), Attempt::Resolved);
        // A UID with no passwd entry: success, but nothing was written.
        assert_eq!(Attempt::of(0, false, true), Attempt::Unavailable);
        assert_eq!(Attempt::of(0, true, false), Attempt::Unavailable);
        assert_eq!(Attempt::of(libc::ERANGE, false, false), Attempt::Grow);
        for code in [libc::ENOENT, libc::EPERM, libc::EIO] {
            assert_eq!(Attempt::of(code, true, true), Attempt::Unavailable);
        }
    }
}
