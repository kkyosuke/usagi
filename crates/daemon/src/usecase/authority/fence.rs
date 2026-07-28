//! Classifying one IPC request for the admission fence.
//!
//! [`super::admission::classify`] answers "may this *kind* of work happen on
//! this role", and it deliberately refuses to guess the kind: a request type a
//! build cannot name is exactly the one whose effects it cannot bound. This
//! module is the other half — the one mapping the wire vocabulary onto that
//! kind — and it exists so **both** serving roles read it from one place. Before
//! it, the active accept loop had no fence at all and the standby carried a
//! private copy of the mapping in the composition root.
//!
//! Two inputs decide the answer, and the second is the role's honest statement
//! about itself rather than something inferred from the request:
//!
//! | input | source |
//! |---|---|
//! | what the request asks for | the `kind` and, for terminals, the `operation` discriminants |
//! | whether this generation can own the runtime a request names | [`OwnedRuntime`], fixed by the role at bind time |
//!
//! An unrecognized `kind` is classified [`RequestClass::Control`], which is the
//! only fail-closed direction available: `Control` is refused by every role
//! except `active`, so a request this build cannot name can never be admitted by
//! a draining, standby, or retired generation.
//!
//! ## Terminal ownership is the runtime's answer, not this module's
//!
//! A terminal request that carries a [`TerminalRef`](usagi_core::domain::id::TerminalRef)
//! is classified [`ResourceOwner::SelfGeneration`] for a generation that owns
//! runtime state, even though the ref names a generation of its own. That is not
//! a shortcut, it is where the two fences divide:
//!
//! * **this fence** decides whether the *role* may perform terminal IO at all,
//!   and issues the [`LeaseClass::OwnerTerminal`](super::admission::LeaseClass)
//!   lease a handoff barrier waits on. That is the whole reason it runs before
//!   dispatch: a re-check after an effect cannot un-write a PTY.
//! * **the terminal runtime** decides whether the exact ref is one of its
//!   records. `TerminalRef` equality is exact and includes the owner generation
//!   ([`TerminalRef::fences`](usagi_core::domain::id::TerminalRef::fences)), so a
//!   ref belonging to another generation resolves to no record — it can never be
//!   answered by a same-scope terminal on this one.
//!
//! Deciding staleness here instead would mean re-deciding it *without* the
//! records, and would convert every stale ref a client still holds into a
//! different typed error than the one the runtime already gives it. A generation
//! that owns nothing has no such records to consult, so it states
//! [`OwnedRuntime::Nothing`] and every named runtime is correctly another
//! generation's.

use serde_json::Value;

use super::admission::{RequestClass, ResourceOwner};

/// Whether this generation can own the runtime a request names.
///
/// It is the role's own statement, fixed when the process binds its endpoint,
/// and never derived from the request: a standby that guessed "this ref might be
/// mine" would be guessing about records it has never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedRuntime {
    /// This generation owns the runtime state of its data directory, so a
    /// request that names a runtime is resolved against its own records.
    Own,
    /// This generation owns nothing at all, so every named runtime belongs to
    /// another generation. A standby's stance ([`super::standby`]).
    Nothing,
}

impl OwnedRuntime {
    /// How a request that names a runtime resolves under this stance.
    const fn named(self) -> ResourceOwner {
        match self {
            Self::Own => ResourceOwner::SelfGeneration,
            Self::Nothing => ResourceOwner::OtherGeneration,
        }
    }
}

/// Classify `body` for a generation whose ownership stance is `owned`.
///
/// The mapping is exhaustive over the wire vocabulary
/// ([`usagi_core::usecase::client::DaemonRequest`]) rather than over the types,
/// because the fence runs before the body is deserialized into a request: a
/// payload that fails to parse must still be classified, and classifying it as
/// `Control` is what keeps it refused everywhere it could do harm.
///
/// | `kind` | class | names a runtime |
/// |---|---|---|
/// | `terminal` with `action` `launch` | [`RequestClass::Spawn`] | no |
/// | `terminal` with `action` `inventory` / `completed_inventory` | [`RequestClass::Inventory`] | no |
/// | `terminal` with `action` `input_outcome` | [`RequestClass::Read`] | yes |
/// | `terminal`, every other action | [`RequestClass::TerminalIo`] | yes |
/// | `agent` / `resume_agent` | [`RequestClass::Spawn`] | no |
/// | `agent_inventory` | [`RequestClass::Inventory`] | no |
/// | `metrics` / `pr` | [`RequestClass::Read`] | no |
/// | anything else | [`RequestClass::Control`] | no |
///
/// `input_outcome` is a [`RequestClass::Read`] that names a runtime because it
/// reads one generation's own durable input ledger and writes nothing. Naming
/// the runtime is what refuses it on a generation that holds no such ledger,
/// instead of answering "unknown operation" — which a client is entitled to read
/// as "the write never happened".
#[must_use]
pub fn classify_request(body: &Value, owned: OwnedRuntime) -> (RequestClass, ResourceOwner) {
    match body.get("kind").and_then(Value::as_str) {
        Some("rollover") => (RequestClass::Rollover, ResourceOwner::Unscoped),
        Some("terminal") => terminal_class(body, owned),
        // Creating a daemon-owned runtime is the effect a control barrier exists
        // to have already stopped, so it is named apart from the rest of the
        // control plane even though both take the same lease.
        Some("agent" | "resume_agent") => (RequestClass::Spawn, ResourceOwner::Unscoped),
        Some("agent_inventory") => (RequestClass::Inventory, ResourceOwner::Unscoped),
        Some("metrics" | "pr") => (RequestClass::Read, ResourceOwner::Unscoped),
        _ => (RequestClass::Control, ResourceOwner::Unscoped),
    }
}

/// Classify the terminal surface, whose `action` separates the scope-addressed
/// requests from the ref-addressed ones.
///
/// `action` is read rather than the payload's `operation` tag because `action` is
/// the field the terminal owner dispatches on: a fence that classified one
/// discriminant while the effect obeyed the other could be talked out of the
/// lease it was supposed to take.
fn terminal_class(body: &Value, owned: OwnedRuntime) -> (RequestClass, ResourceOwner) {
    match body.get("action").and_then(Value::as_str) {
        Some("launch") => (RequestClass::Spawn, ResourceOwner::Unscoped),
        Some("inventory" | "completed_inventory") => {
            (RequestClass::Inventory, ResourceOwner::Unscoped)
        }
        Some("input_outcome") => (RequestClass::Read, owned.named()),
        // Every remaining terminal action is addressed by an exact ref, and an
        // action this build cannot name is treated as IO on a named runtime
        // rather than as a scope query: that is the direction in which a
        // generation which owns nothing refuses it.
        _ => (RequestClass::TerminalIo, owned.named()),
    }
}

#[cfg(test)]
mod tests;
