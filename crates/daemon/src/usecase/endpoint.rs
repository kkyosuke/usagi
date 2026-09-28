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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EndpointObservation {
    /// No probe was made. Carries no claim either way.
    #[default]
    NotObserved,
    /// The endpoint answered within the probe's budget.
    Answering,
    /// The endpoint did not answer within the probe's budget.
    Silent,
}

impl EndpointObservation {
    /// The observation a completed probe proves.
    ///
    /// `answered` is true for a completed handshake *and* for a typed refusal:
    /// a daemon that refuses a request has answered. Only a transport failure —
    /// no listener, a connection closed before any reply — proves silence.
    #[must_use]
    pub const fn probed(answered: bool) -> Self {
        if answered {
            Self::Answering
        } else {
            Self::Silent
        }
    }

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

    #[test]
    fn a_probe_that_got_an_answer_is_not_silent() {
        assert_eq!(
            EndpointObservation::probed(true),
            EndpointObservation::Answering
        );
        assert!(!EndpointObservation::probed(true).is_silent());
    }

    #[test]
    fn a_probe_that_got_no_answer_is_silent() {
        assert_eq!(
            EndpointObservation::probed(false),
            EndpointObservation::Silent
        );
        assert!(EndpointObservation::probed(false).is_silent());
    }

    /// An unprobed endpoint must never be read as a failed one: the default is
    /// what every verb that pays for no probe carries.
    #[test]
    fn an_unobserved_endpoint_proves_nothing() {
        assert_eq!(
            EndpointObservation::default(),
            EndpointObservation::NotObserved
        );
        assert!(!EndpointObservation::NotObserved.is_silent());
    }
}
