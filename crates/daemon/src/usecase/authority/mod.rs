//! Cross-process daemon-generation authority.
//!
//! [`crate::usecase::generation`] fences one process: it decides which
//! generation may admit work *inside* a single daemon. This module is the
//! authority two daemon processes share while a planned restart keeps both
//! alive, and it is deliberately built from separable pieces so each one
//! answers exactly one question:
//!
//! | piece | question it answers |
//! |---|---|
//! | [`registry`] | which generations exist, what role does each hold, and who is `current`? |
//! | [`standby`] | is this candidate the exact build it was admitted for — proved without any side effect? |
//! | [`handoff`] | how do the registry and the current locator — two independent durable objects — reach one outcome across a crash? |
//! | [`admission`] | may *this* request, right now, produce an effect on *this* generation? |
//! | [`routing`] | may this rollover leave a draining generation behind — can every participant still address it? |
//! | [`workers`] | which client threads must be unblocked and joined before a generation is collected? |
//! | [`rollover`] | driving the above against the durable objects, with a named boundary at every write |
//! | [`migration`] | can the legacy single-generation state be adopted, or must it fail closed? |
//!
//! Every module here is pure: durability is an injected
//! [`registry::RegistryFile`] seam, the published endpoint is an injected
//! [`rollover::CurrentLocator`], and process liveness is an injected
//! observation — so the whole authority is exercised deterministically. The
//! real filesystem adapters live in
//! [`crate::infrastructure::generation_registry`].
//!
//! The contract this implements is documented in
//! [5. daemon](../../../../../document/05-daemon.md); the write order and its
//! crash boundaries are stated once, in [`handoff`].

#[cfg(test)]
mod fixture;

pub mod admission;
pub mod handoff;
pub mod migration;
pub mod registry;
pub mod rollover;
pub mod routing;
pub mod standby;
pub mod workers;
