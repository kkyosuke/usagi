//! OS-verifiable identity of a spawned child.
//!
//! A PID is not an identity: the kernel reuses it, and a reused PID pointing at
//! somebody else's process is exactly the case a planned rollover must never
//! mistake for its own child. So a child's identity is only ever *observed*, and
//! the observation records where it came from.
//!
//! [`ChildIdentity`] therefore cannot be spelled by hand from usecase code: it is
//! constructed either by [`record_child`] — which reads the platform through the
//! [`ChildProcessProbe`] seam — or by [`ChildIdentity::unverifiable`], which
//! marks a legacy/fixed token as *not* authority. Everything that acts on a child
//! (final commit, capacity release, signal) requires
//! [`ChildIdentity::is_verifiable`] first, and an observation that is not
//! [`ChildObservation::Exact`] never becomes proof of ownership.

use std::io;

use serde::{Deserialize, Serialize};

use crate::usecase::generation::ProcessIdentity;
use crate::usecase::resources::ResourceError;

/// The only identity source that counts as authority: a token read from the OS
/// process table for that exact PID.
pub const IDENTITY_SOURCE_OS: &str = "os";

/// The source recorded for a legacy or fixed token. It is stored so a migration
/// can see *why* a record is unusable instead of guessing.
pub const IDENTITY_SOURCE_UNVERIFIED: &str = "unverified";

/// A spawned child's durable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildIdentity {
    pub pid: u32,
    /// Process-group identity, so group signalling is fenceable too.
    pub process_group: u32,
    /// Where `start_identity` came from. Only [`IDENTITY_SOURCE_OS`] is
    /// authority; anything else is recorded state that must fail closed.
    pub source: String,
    /// The opaque OS process-start token, compared for exact equality only.
    pub start_identity: String,
}

impl ChildIdentity {
    /// Mark a token that cannot be trusted (a legacy fixed string, a wall clock,
    /// a PID-derived value) as explicitly unverifiable.
    #[must_use]
    pub fn unverifiable(pid: u32, token: impl Into<String>) -> Self {
        Self {
            pid,
            process_group: pid,
            source: IDENTITY_SOURCE_UNVERIFIED.to_owned(),
            start_identity: token.into(),
        }
    }

    /// Whether this identity may be used as spawn/exit/signal authority.
    #[must_use]
    pub fn is_verifiable(&self) -> bool {
        self.source == IDENTITY_SOURCE_OS && !self.start_identity.is_empty()
    }

    /// The same identity in the daemon-lifecycle vocabulary, so the child and
    /// the daemon owner are fenced by one comparison rule. The lifecycle signal
    /// contract itself is unchanged: this only reuses its shape.
    ///
    /// # Errors
    /// Returns [`ResourceError::IdentityUnverifiable`] when the identity is not
    /// OS-observed, so an unverifiable token can never reach a signal path.
    pub fn to_process_identity(&self) -> Result<ProcessIdentity, ResourceError> {
        if !self.is_verifiable() {
            return Err(ResourceError::IdentityUnverifiable);
        }
        Ok(ProcessIdentity {
            pid: self.pid,
            start_identity: self.start_identity.clone(),
            process_group: self.process_group,
        })
    }
}

/// The platform seam a child's identity is observed through.
///
/// The real adapter reads the OS process table
/// ([`crate::infrastructure::child_identity`]); tests inject a fake that can also
/// report reuse, absence, permission failures, and malformed answers.
pub trait ChildProcessProbe {
    /// Read the process-start token for `pid`.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::NotFound`] when the process is gone, and any
    /// other error when the platform cannot answer safely.
    fn start_identity(&self, pid: u32) -> io::Result<String>;

    /// Read the process-group id for `pid`.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::NotFound`] when the process is gone, and any
    /// other error when the platform cannot answer safely.
    fn process_group(&self, pid: u32) -> io::Result<u32>;
}

/// Why a child's identity could not be recorded at spawn time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityRefusal {
    /// The child was already gone when its identity was read.
    Gone,
    /// The platform could not be read (permission, unsupported).
    Unobservable,
    /// The platform answered with something unusable as an identity.
    Malformed,
}

/// What an OS observation says about a recorded child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildObservation {
    /// The exact recorded process is still alive.
    Exact,
    /// The process is proven gone. Safe to finalize, never to signal.
    Gone,
    /// The PID is alive but is a *different* process. Also proves the recorded
    /// child is gone, and is kept distinct so no code path signals this PID.
    Reused,
    /// Nothing can be proved: unverifiable record, or an unreadable platform.
    Unknown,
}

impl ChildObservation {
    /// Whether the recorded child is definitely no longer running. This is the
    /// only condition under which a final may be committed without the child.
    #[must_use]
    pub fn is_definitely_gone(self) -> bool {
        matches!(self, Self::Gone | Self::Reused)
    }
}

/// Observe a freshly spawned child's identity.
///
/// # Errors
/// Returns the [`IdentityRefusal`] that keeps the caller from recording an
/// identity it cannot later verify.
pub fn record_child(
    probe: &dyn ChildProcessProbe,
    pid: u32,
) -> Result<ChildIdentity, IdentityRefusal> {
    let start_identity = probe.start_identity(pid).map_err(|error| refusal(&error))?;
    if start_identity.is_empty() {
        return Err(IdentityRefusal::Malformed);
    }
    let process_group = probe.process_group(pid).map_err(|error| refusal(&error))?;
    Ok(ChildIdentity {
        pid,
        process_group,
        source: IDENTITY_SOURCE_OS.to_owned(),
        start_identity,
    })
}

/// Compare a recorded child against the live OS process table.
#[must_use]
pub fn observe_child(probe: &dyn ChildProcessProbe, recorded: &ChildIdentity) -> ChildObservation {
    if !recorded.is_verifiable() {
        return ChildObservation::Unknown;
    }
    let actual = match probe.start_identity(recorded.pid) {
        Ok(actual) => actual,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ChildObservation::Gone,
        Err(_) => return ChildObservation::Unknown,
    };
    if actual != recorded.start_identity {
        return ChildObservation::Reused;
    }
    match probe.process_group(recorded.pid) {
        Ok(group) if group == recorded.process_group => ChildObservation::Exact,
        Err(error) if error.kind() == io::ErrorKind::NotFound => ChildObservation::Gone,
        // Either the start token matched while the group did not — so the record
        // does not describe this process as a whole — or the platform could not
        // answer. Neither is authority over any part of it.
        Ok(_) | Err(_) => ChildObservation::Unknown,
    }
}

/// What a platform failure means for a child's identity. It is deliberately not
/// generic over the value being read: one mapping, one compiled copy.
fn refusal(error: &io::Error) -> IdentityRefusal {
    match error.kind() {
        io::ErrorKind::NotFound => IdentityRefusal::Gone,
        io::ErrorKind::InvalidData => IdentityRefusal::Malformed,
        _ => IdentityRefusal::Unobservable,
    }
}

#[cfg(test)]
mod tests;
