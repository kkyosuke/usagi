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
//! PTY are exactly as they were. The shipping `daemon restart` consults this
//! authority before it starts a rollover.
//!
//! The client half of the contract is
//! [`usagi_core::usecase::owner_routing`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Condvar, Mutex};

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
    /// Another rollover already froze connection admission for its commit.
    RoutingAdmissionBusy,
    /// MCP caller credentials are process-local and cannot move to the
    /// successor. This includes credentials whose child has not connected yet.
    McpAuthorityRetained { credentials: usize },
    /// The active process could not inspect its process-local MCP authority.
    McpAuthorityUnavailable,
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
            Self::RoutingAdmissionBusy => {
                f.write_str("routing admission is frozen by another rollover")
            }
            Self::McpAuthorityRetained { credentials } => write!(
                f,
                "{credentials} daemon-provisioned MCP caller credential(s) remain on the active generation"
            ),
            Self::McpAuthorityUnavailable => {
                f.write_str("daemon-provisioned MCP caller authority is unavailable")
            }
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
    state: Mutex<RoutingState>,
    unfrozen: Condvar,
}

#[derive(Default)]
struct RoutingState {
    participants: BTreeMap<ConnectionId, ParticipantRouting>,
    frozen: bool,
}

/// Exclusive admission freeze held across a rollover's authority commit.
///
/// A connection whose hello was negotiated concurrently waits in
/// [`RoutingLedger::admit`] before handshake success is written. By then either
/// the old generation is active again, or its connection fence can refuse a
/// routing-incapable peer without exposing a stale successful handshake.
pub struct RoutingFreeze<'a> {
    ledger: &'a RoutingLedger,
}

impl Drop for RoutingFreeze<'_> {
    fn drop(&mut self) {
        let mut state = self.ledger.lock();
        state.frozen = false;
        self.ledger.unfrozen.notify_all();
    }
}

impl RoutingLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what an admitted connection advertised.
    pub fn admit(&self, connection: ConnectionId, hello: &ClientHello) {
        let mut state = self.lock();
        while state.frozen {
            state = self
                .unfrozen
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state
            .participants
            .insert(connection, ParticipantRouting::of(hello));
    }

    /// Forget a connection that has gone away.
    pub fn disconnect(&self, connection: &ConnectionId) {
        self.lock().participants.remove(connection);
    }

    /// How many admitted connections there are.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.lock().participants.len()
    }

    /// How many admitted connections cannot address a draining generation.
    #[must_use]
    pub fn unsupported(&self) -> usize {
        self.lock()
            .participants
            .values()
            .filter(|participant| !participant.supports_owner_routing)
            .count()
    }

    /// Freeze connection admission after atomically proving that every already
    /// admitted participant supports owner-generation routing.
    fn freeze_supported(&self) -> Result<RoutingFreeze<'_>, RolloverRefusal> {
        let mut state = self.lock();
        if state.frozen {
            return Err(RolloverRefusal::RoutingAdmissionBusy);
        }
        let connections = state
            .participants
            .values()
            .filter(|participant| !participant.supports_owner_routing)
            .count();
        if connections > 0 {
            return Err(RolloverRefusal::ClientRoutingUnsupported { connections });
        }
        state.frozen = true;
        Ok(RoutingFreeze { ledger: self })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RoutingState> {
        self.state
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
pub fn admit_rollover<'a>(
    ledger: &'a RoutingLedger,
    document: &RegistryDocument,
    planned_revision: u64,
    successor: &ServerHello,
) -> Result<RoutingFreeze<'a>, RolloverRefusal> {
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
    ledger.freeze_supported()
}

#[cfg(test)]
mod tests;
