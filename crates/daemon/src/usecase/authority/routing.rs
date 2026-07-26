//! Whether a rollover may leave a draining generation behind at all.
//!
//! A planned restart is only safe when *every* participant can still reach the
//! old generation afterwards. The old daemon keeps its PTYs, and a client that
//! addresses "the current endpoint" would either fail to find them or — worse —
//! deliver an old terminal's input to a same-scope terminal on the new daemon.
//! So the handoff asks one question before it begins, and refuses with zero
//! effect when the answer is no:
//!
//! | participant | requirement |
//! |---|---|
//! | every admitted client connection | advertises [`OWNER_GENERATION_ROUTING_CAPABILITY`] |
//! | the successor daemon | advertises it too, so the client's routed requests are understood on the new side |
//! | the durable registry | is the schema this build writes, at exactly the revision the rollover was planned against |
//!
//! A refusal here happens *before* [`super::rollover::execute_rollover`] writes
//! anything: the registry, the current locator, the admission barrier, and every
//! PTY are exactly as they were. That is what makes it safe for `main` to carry
//! this authority while the shipping `daemon restart` that would drive it is
//! still #507's to enable.
//!
//! The client half of the contract is
//! [`usagi_core::usecase::owner_routing`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use usagi_core::domain::id::ConnectionId;
use usagi_core::infrastructure::ipc::{
    ClientHello, OWNER_GENERATION_ROUTING_CAPABILITY, ServerHello,
    supports_owner_generation_routing,
};

use crate::usecase::authority::registry::{REGISTRY_SCHEMA, RegistryDocument};

/// Why a rollover must not start. Every variant is effect zero: the old active
/// generation stays active, `current` stays published, and no PTY is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloverRefusal {
    /// At least one admitted client cannot address a draining generation.
    /// Rolling over would strand exactly that client's terminals.
    ClientRoutingUnsupported { connections: usize },
    /// The successor does not advertise owner-generation routing, so the
    /// requests a routing client sends it would not be understood.
    SuccessorRoutingUnsupported,
    /// The durable registry is not the schema this build writes.
    RegistrySchemaUnsupported,
    /// The registry moved since the rollover was planned. Re-planning against
    /// the current revision is the only safe continuation.
    RegistryRevisionMismatch { planned: u64, observed: u64 },
}

impl fmt::Display for RolloverRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientRoutingUnsupported { connections } => write!(
                f,
                "{connections} connected client(s) lack {OWNER_GENERATION_ROUTING_CAPABILITY}"
            ),
            Self::SuccessorRoutingUnsupported => write!(
                f,
                "the successor generation does not advertise {OWNER_GENERATION_ROUTING_CAPABILITY}"
            ),
            Self::RegistrySchemaUnsupported => {
                f.write_str("generation registry schema is not supported")
            }
            Self::RegistryRevisionMismatch { planned, observed } => write!(
                f,
                "generation registry moved from revision {planned} to {observed}"
            ),
        }
    }
}

impl std::error::Error for RolloverRefusal {}

/// What one admitted connection advertised.
///
/// Only the routing answer is retained: this ledger decides whether a rollover
/// may start, and nothing else about a client is its business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParticipantRouting {
    pub supports_owner_routing: bool,
}

impl ParticipantRouting {
    /// Read one client's hello.
    #[must_use]
    pub fn of(hello: &ClientHello) -> Self {
        Self {
            supports_owner_routing: supports_owner_generation_routing(&hello.capabilities),
        }
    }
}

/// The routing capability of every connection this generation currently holds.
///
/// It is keyed by connection rather than by client incarnation on purpose: a
/// client that reconnects with a newer build must be able to change its answer,
/// and a client that has gone away must stop blocking a rollover.
#[derive(Default)]
pub struct RoutingLedger {
    participants: Mutex<BTreeMap<ConnectionId, ParticipantRouting>>,
}

impl RoutingLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what an admitted connection advertised.
    pub fn admit(&self, connection: ConnectionId, hello: &ClientHello) {
        self.lock()
            .insert(connection, ParticipantRouting::of(hello));
    }

    /// Forget a connection that has gone away.
    pub fn disconnect(&self, connection: &ConnectionId) {
        self.lock().remove(connection);
    }

    /// How many admitted connections there are.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.lock().len()
    }

    /// How many admitted connections cannot address a draining generation.
    #[must_use]
    pub fn unsupported(&self) -> usize {
        self.lock()
            .values()
            .filter(|participant| !participant.supports_owner_routing)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ConnectionId, ParticipantRouting>> {
        self.participants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Decide whether a rollover may begin.
///
/// `planned_revision` is the registry revision the caller planned against, so a
/// concurrent writer that moved the registry is caught here rather than by a
/// compare-and-swap in the middle of the handoff. `successor` is the standby's
/// own `ServerHello`, already proved to be the expected artifact by
/// [`super::standby`]; this only asks whether it speaks the routing contract.
///
/// # Errors
/// Returns the [`RolloverRefusal`] that keeps the current authority in place.
pub fn admit_rollover(
    ledger: &RoutingLedger,
    document: &RegistryDocument,
    planned_revision: u64,
    successor: &ServerHello,
) -> Result<(), RolloverRefusal> {
    if document.schema != REGISTRY_SCHEMA {
        return Err(RolloverRefusal::RegistrySchemaUnsupported);
    }
    if document.revision != planned_revision {
        return Err(RolloverRefusal::RegistryRevisionMismatch {
            planned: planned_revision,
            observed: document.revision,
        });
    }
    if !supports_owner_generation_routing(&successor.capabilities) {
        return Err(RolloverRefusal::SuccessorRoutingUnsupported);
    }
    let connections = ledger.unsupported();
    if connections > 0 {
        return Err(RolloverRefusal::ClientRoutingUnsupported { connections });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
