//! The platform adapter that observes a spawned child's identity.
//!
//! It answers exactly two questions about a PID — its process-start token and its
//! process group — and it reads both from the OS process table. Nothing here
//! derives an identity from a wall clock or from the PID itself, because a value
//! like that cannot tell a reused PID from the original child.
//!
//! An absent process is reported as [`io::ErrorKind::NotFound`], which the
//! usecase layer turns into "gone"; every other failure stays an error, so an
//! unreadable platform becomes "unknown" rather than a guess.

use std::io;

use crate::usecase::resources::identity::ChildProcessProbe;

/// Reads child identity from the local OS process table.
pub struct UnixChildProbe;

impl ChildProcessProbe for UnixChildProbe {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn start_identity(&self, pid: u32) -> io::Result<String> {
        process_start_identity(pid)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn process_group(&self, pid: u32) -> io::Result<u32> {
        let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
        // SAFETY: `getpgid` only reads the process table for `pid`.
        let group = unsafe { libc::getpgid(pid) };
        if group < 0 {
            return Err(io::Error::last_os_error());
        }
        u32::try_from(group).map_err(|_| io::Error::other("process group out of range"))
    }
}

/// Reads the kernel's process start time for `pid` on Linux.
#[cfg(target_os = "linux")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
fn process_start_identity(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat"))?;
    let start_time = stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?;
    start_time
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(format!("linux:{start_time}"))
}

/// Reads the kernel's process start timestamp for `pid` on macOS.
#[cfg(target_os = "macos")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
fn process_start_identity(pid: u32) -> io::Result<String> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
    // SAFETY: `info` is initialized and the buffer pointer/length describe the
    // exact `proc_bsdinfo` allocation for the duration of `proc_pidinfo`.
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_arg = libc::c_int::try_from(size)
        .map_err(|_| io::Error::other("proc_bsdinfo size out of range"))?;
    // SAFETY: see the initialized buffer argument above.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size_arg,
        )
    };
    if read <= 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ESRCH) {
            io::Error::from(io::ErrorKind::NotFound)
        } else {
            error
        });
    }
    if usize::try_from(read).unwrap_or(0) < size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short proc_bsdinfo read",
        ));
    }
    Ok(format!(
        "macos:{}.{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

/// Platforms without a readable process table cannot fence a child, so they
/// refuse rather than inventing a token.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
fn process_start_identity(_pid: u32) -> io::Result<String> {
    Err(io::Error::other(
        "process start identity is unavailable on this platform",
    ))
}
