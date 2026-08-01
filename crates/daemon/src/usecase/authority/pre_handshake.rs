//! Daemon-wide admission for connections that have not completed their hello.
//!
//! This is intentionally separate from generation admission and from any policy
//! for established, long-lived connections. A permit accounts only for the
//! interval from `accept(2)` until the daemon has completely read and answered
//! the first frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The production-wide upper bound for concurrently incomplete handshakes.
pub const PRE_HANDSHAKE_CONNECTION_LIMIT: usize = 32;

struct State {
    in_flight: AtomicUsize,
    limit: usize,
}

/// A daemon-wide, non-blocking pre-handshake admission gate.
///
/// A full gate never queues an accepted socket and never asks the caller to
/// spawn a worker. The caller can therefore fail closed using only the accepted
/// descriptor it already owns.
#[derive(Clone)]
pub struct PreHandshakeAdmission {
    state: Arc<State>,
}

impl PreHandshakeAdmission {
    /// Build a gate with `limit` simultaneous incomplete handshakes.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(State {
                in_flight: AtomicUsize::new(0),
                limit,
            }),
        }
    }

    /// Try to reserve one incomplete handshake without waiting.
    ///
    /// `None` means the caller must close the just-accepted connection before
    /// allocating a worker or any request-scoped daemon state.
    #[must_use]
    pub fn try_admit(&self) -> Option<PreHandshakePermit> {
        let admitted = self
            .state
            .in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.state.limit).then_some(current + 1)
            })
            .is_ok();
        admitted.then(|| PreHandshakePermit {
            state: Arc::clone(&self.state),
        })
    }

    /// Number of currently incomplete handshakes.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }
}

/// One admitted but incomplete handshake.
///
/// Dropping it after either a successful hello or any failure returns capacity.
pub struct PreHandshakePermit {
    state: Arc<State>,
}

impl Drop for PreHandshakePermit {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_bounded_and_released_by_the_handshake_lifetime() {
        let admission = PreHandshakeAdmission::new(2);
        let first = admission.try_admit().expect("first handshake is admitted");
        let second = admission.try_admit().expect("second handshake is admitted");
        assert_eq!(admission.in_flight(), 2);
        assert!(admission.try_admit().is_none());

        drop(first);
        let replacement = admission
            .try_admit()
            .expect("a completed handshake returns capacity");
        assert_eq!(admission.in_flight(), 2);

        drop((second, replacement));
        assert_eq!(admission.in_flight(), 0);
    }

    #[test]
    fn a_zero_capacity_gate_always_fails_closed() {
        let admission = PreHandshakeAdmission::new(0);
        assert!(admission.try_admit().is_none());
        assert_eq!(admission.in_flight(), 0);
    }

    #[test]
    fn cloned_gates_share_one_daemon_wide_count() {
        let admission = PreHandshakeAdmission::new(1);
        let peer = admission.clone();
        let permit = peer.try_admit().expect("shared capacity is available");
        assert!(admission.try_admit().is_none());
        drop(permit);
        assert!(admission.try_admit().is_some());
    }
}
