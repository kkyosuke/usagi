//! Whether the recorded daemon still answers on its IPC endpoint.
//!
//! Every other lifecycle observation in this crate proves a *process*: the
//! recorded pid still carries the exact process-start identity `daemon.json`
//! recorded. That proof says nothing about whether the daemon is still
//! *serving*. A daemon whose accept loop is gone — a background worker took the
//! process down but the process has not finished exiting, a listener retired
//! without the record being reclaimed — keeps its pid, keeps `daemon.lock`, and
//! answers nothing.
//!
//! Read as "running", that state makes `status` report a daemon nobody can
//! reach, and it makes a planned replacement pick the one transition that needs
//! the endpoint it cannot reach. Naming the observation separately is what lets
//! each verb say something true about it.
//!
//! The observation is deliberately tri-state. "Not observed" is not "silent":
//! a verb that never speaks to a running daemon (`serve` above all, which would
//! be probing the endpoint it is about to publish) must pay nothing for an
//! answer it cannot use, and must not have its absence read as a failure.

/// What a bounded probe of the recorded owner's endpoint proved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointObservation {
    /// No probe was made. Carries no claim either way.
    NotObserved,
    /// The endpoint answered. A framed refusal counts: a daemon that refuses a
    /// request has answered. Only a transport failure — no listener, a
    /// connection closed before any reply — fails to count.
    Answering,
    /// Every attempt the probe made failed at the transport.
    Silent,
}

impl EndpointObservation {
    /// Whether this observation proves the recorded owner cannot be reached.
    ///
    /// Only [`Self::Silent`] does. An unprobed endpoint proves nothing, so every
    /// decision that reads this keeps the behaviour it had before the probe
    /// existed.
    #[must_use]
    pub const fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }
}

#[cfg(test)]
mod tests {
    use super::EndpointObservation;

    /// Silence is the only observation that proves anything against the daemon,
    /// and an unprobed endpoint must never be mistaken for a failed one.
    #[test]
    fn only_a_probe_that_got_no_answer_proves_the_owner_is_unreachable() {
        assert!(EndpointObservation::Silent.is_silent());
        assert!(!EndpointObservation::Answering.is_silent());
        assert!(!EndpointObservation::NotObserved.is_silent());
    }
}
