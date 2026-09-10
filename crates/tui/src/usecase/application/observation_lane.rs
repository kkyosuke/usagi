//! Single-flight cadence policy for background observations.
//!
//! The presentation shell decides what to observe and owns worker execution.
//! This application policy only decides when one active lane may start again.

use std::time::Duration;

/// Admission state for one bounded background observation lane.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ObservationLane {
    in_flight: bool,
    next_due: Option<Duration>,
    interval: Duration,
    backoff: Duration,
}

impl ObservationLane {
    /// Creates a lane that is due immediately.
    pub(crate) const fn new(interval: Duration, backoff: Duration) -> Self {
        Self {
            in_flight: false,
            next_due: Some(Duration::ZERO),
            interval,
            backoff,
        }
    }

    /// Admits at most one observation when the owner is active and due.
    ///
    /// An inactive lane is re-armed for its next activation without cancelling
    /// an observation that is already in flight.
    pub(crate) fn begin_if_due(&mut self, active: bool, now: Duration) -> bool {
        if !active {
            if !self.in_flight {
                self.next_due = Some(Duration::ZERO);
            }
            return false;
        }
        if self.in_flight || self.next_due.is_none_or(|due| now < due) {
            return false;
        }
        self.in_flight = true;
        self.next_due = None;
        true
    }

    /// Completes the current observation and schedules its next attempt.
    pub(crate) fn complete(&mut self, now: Duration, observed: bool) {
        self.in_flight = false;
        self.next_due = Some(
            now + if observed {
                self.interval
            } else {
                self.backoff
            },
        );
    }

    /// Makes an idle lane due immediately while preserving single-flight.
    pub(crate) fn refresh_now(&mut self) {
        if !self.in_flight {
            self.next_due = Some(Duration::ZERO);
        }
    }
}
