//! The `usagi daemon serve --standby` usecase: run this process as a standby.
//!
//! A standby is the second daemon process in one data directory. It exists so a
//! planned replacement has something to hand authority *to*, and everything
//! about its lifecycle follows from one rule: **it owns nothing**.
//!
//! | the active `serve` ([`super::serve`]) | a standby |
//! |---|---|
//! | holds the workspace fence and the instance lock for its whole lifetime | holds neither: it spawns nothing and writes no worktree |
//! | registers `daemon.json` as the data directory's owner | writes no lifecycle record |
//! | binds an endpoint and publishes `current.json` | binds a *private* endpoint and publishes nothing |
//! | starts PTY, supervisor, PR, teardown and custody workers | starts no worker, no tick, no spawn |
//! | reconciles the durable runtime state on the way in | hydrates it read-only |
//!
//! So the state machine is short:
//!
//! 1. **prepare** — arrange shutdown delivery before anything is bound;
//! 2. **preflight** — prove a live *registered* active generation owns this data
//!    directory ([`super::authority::standby::admissible_active`]). This is
//!    first because its refusals are the ordinary ones — no daemon running, an
//!    older build holding the directory without registering — and none of them
//!    should have created a socket inside a tree this process does not own;
//! 3. **bind** — bind the private endpoint ([`StandbyEndpoint::bind`]) and start
//!    answering a readiness handshake on it. Nothing published changes, so no
//!    client can discover this process;
//! 4. **admit** — re-prove the same thing (the active could have died while this
//!    process was binding), register this generation as its standby, and record
//!    the artifact its own hello advertised
//!    ([`super::authority::standby::prepare_standby`]);
//! 5. **run** — block until asked to shut down;
//! 6. **stand down** — release the registry entry, then retire the endpoint.
//!
//! The retirement order is the *reverse* of the active daemon's, and for the
//! same reason the active's is what it is: whatever names an endpoint must go
//! before the endpoint does. For the active that is `current.json` (retired
//! before its registry entry); for a standby nothing is published at all, so its
//! registry entry is the only thing that names its socket — and a retained
//! standby entry naming a socket nobody accepts on is exactly what a rollover
//! would trust.
//!
//! Every seam is injected, so this whole ordering is proved with fakes; the
//! synthesis root binds the real private endpoint, the real registry, and the
//! real readiness probe.

use std::io::{self, Write};

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::ShutdownSignal;

/// The private endpoint half of a standby process.
///
/// It is one port rather than the active daemon's `DaemonReady` because a
/// standby has strictly fewer verbs: there is no stale recovery to run (the
/// directory's endpoints belong to the active generation, not to this process)
/// and no locator to publish.
pub trait StandbyEndpoint {
    /// Hydrate the durable runtime state read-only, bind this generation's
    /// private endpoint, and start answering the readiness handshake.
    ///
    /// # Errors
    ///
    /// Returns the hydrate, bind, or accept-loop start failure. Nothing durable
    /// has been written when this fails.
    fn bind(&self) -> io::Result<()>;

    /// Stop admitting, join the accept loop, and unlink this generation's
    /// socket. Idempotent, so a cleanup path may call it without knowing
    /// whether [`bind`](Self::bind) got that far.
    ///
    /// # Errors
    ///
    /// Returns the join or unlink failure.
    fn retire(&self) -> io::Result<()>;
}

/// This process's participation in the durable registry as a standby.
///
/// Both verbs are idempotent, so a cleanup path may release an entry it is not
/// sure was ever registered.
pub trait StandbyAuthority {
    /// Prove a live registered active generation owns this data directory,
    /// reading only. Nothing is written, bound, or created.
    ///
    /// # Errors
    ///
    /// Returns the start refusal or the registry read failure.
    fn preflight(&self) -> io::Result<()>;

    /// Re-prove [`preflight`](Self::preflight), then register this generation as
    /// a verified standby.
    ///
    /// The proof is repeated because binding takes time and the active generation
    /// can die inside it. Repeating it immediately before the compare-and-swap is
    /// what keeps the window to the width of one registry write instead of the
    /// width of a socket bind.
    ///
    /// # Errors
    ///
    /// Returns the start refusal, the registry failure, or the readiness
    /// refusal. The active generation and the current locator are unchanged in
    /// every case.
    fn admit(&self) -> io::Result<()>;

    /// Give up this generation's registry entry.
    ///
    /// # Errors
    ///
    /// Returns the registry failure. A generation that was never registered is
    /// not a failure.
    fn release(&self) -> io::Result<()>;
}

/// Run this process as a standby generation under process id `pid`, writing
/// progress lines to `out`.
///
/// # Errors
///
/// Returns the shutdown preparation / wait error, the endpoint bind / retire
/// error, the authority admit / release error, or an `out` write error.
pub fn serve_standby(
    out: &mut dyn Write,
    endpoint: &dyn StandbyEndpoint,
    authority: &dyn StandbyAuthority,
    shutdown: &dyn ShutdownSignal,
    pid: u32,
    info: &AppInfo,
) -> io::Result<()> {
    let describe = info.describe();

    // Signal delivery is arranged before the endpoint exists, so a stop that
    // arrives during admission still unwinds through the ordinary path below
    // rather than leaving a socket and a registry entry behind.
    shutdown.prepare()?;

    // Refused here, nothing exists to clean up: no socket in the active
    // generation's directory, no registry write, no lifecycle record touched.
    authority.preflight()?;

    endpoint.bind()?;

    // The endpoint answers now, so readiness can prove this artifact against
    // the very socket a handoff would name.
    if let Err(error) = authority.admit() {
        stand_down(endpoint, authority);
        return Err(error);
    }
    if let Err(error) = writeln!(out, "{describe}: daemon standing by (pid {pid})") {
        stand_down(endpoint, authority);
        return Err(error);
    }
    if let Err(error) = shutdown.wait() {
        stand_down(endpoint, authority);
        return Err(error);
    }

    // Nothing may name this endpoint once it stops answering, so the registry
    // entry goes first and a failure to give it up keeps the endpoint bound.
    authority.release()?;
    endpoint.retire()?;
    writeln!(out, "{describe}: daemon standby stopped (pid {pid})")
}

/// Best-effort stand-down that preserves a primary error.
///
/// The order is the successful path's order: the entry that names the endpoint
/// is released before the endpoint is unlinked, so a second failure cannot leave
/// a retained standby whose socket is already gone.
fn stand_down(endpoint: &dyn StandbyEndpoint, authority: &dyn StandbyAuthority) {
    if authority.release().is_ok() {
        let _ = endpoint.retire();
    }
}

#[cfg(test)]
mod tests;
