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

use std::ffi::CStr;

/// The first `getpwuid_r` scratch buffer. NSS and Open Directory entries are far
/// smaller than this in practice, so the first call normally succeeds.
const INITIAL_BUFFER_BYTES: usize = 1024;

/// The largest scratch buffer the resolver will grow to before reporting
/// failure. A passwd entry that does not fit here is not one this daemon can
/// use, and an unbounded retry loop would be a memory amplifier driven by the
/// platform's directory service.
const MAXIMUM_BUFFER_BYTES: usize = 64 * 1024;

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
        if code == libc::ERANGE && capacity < MAXIMUM_BUFFER_BYTES {
            capacity *= 2;
            continue;
        }
        if code != 0 || found.is_null() || entry.pw_name.is_null() {
            return None;
        }
        // SAFETY: a found entry's `pw_name` is a NUL-terminated string inside
        // `buffer`, which outlives this borrow.
        let name = unsafe { CStr::from_ptr(entry.pw_name) };
        return name.to_str().ok().map(str::to_owned);
    }
}
