//! Adopting the legacy single-generation state into the registry.
//!
//! The pre-registry world stored a daemon owner in `daemon.json` and its
//! endpoint in `current.json`, with no generation table between them. Adopting
//! that state means asserting "this process owns this endpoint" — which is
//! exactly the claim that must never be guessed.
//!
//! Adoption therefore requires two independent proofs: the record names an
//! exact live OS process ([`DaemonProcessObservation::Exact`]), and the locator
//! is readable and names an endpoint. Anything else — a legacy record without
//! process identity, a reused PID, an unobservable platform, a missing or
//! unreadable locator — refuses, and the caller starts a fresh generation
//! instead of inheriting one.

use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::infrastructure::ipc::BuildIdentity;

use crate::usecase::authority::handoff::LocatorObservation;
use crate::usecase::authority::registry::{GenerationEntry, REGISTRY_SCHEMA, RegistryDocument};
use crate::usecase::generation::{GenerationRole, ProcessIdentity};

/// Why legacy state was not adopted. Every variant leaves the caller with an
/// empty registry, never a guessed owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRefusal {
    /// No lifecycle record exists, so there is nothing to adopt.
    NoRecord,
    /// The record predates process identity, so its owner is unknown.
    NoProcessIdentity,
    /// The recorded process is proven gone.
    StaleOwner,
    /// PID reuse or an unobservable platform: ownership cannot be established.
    UnverifiedOwner,
    /// No endpoint is published, so no generation can be named.
    MissingLocator,
    /// The locator exists but cannot be trusted.
    UnreadableLocator,
}

/// Adopt a proven legacy owner as the single active generation.
///
/// The adopted entry carries an unknown `expected_build`: the legacy owner
/// never declared one. That is deliberate — it is enough to be handed *off*
/// from, and never enough to be handed *to*, because a standby must present a
/// known artifact before it can take authority.
///
/// # Errors
/// Returns the [`MigrationRefusal`] that made adoption unprovable.
pub fn migrate_legacy(
    record: Option<&DaemonRecord>,
    observation: DaemonProcessObservation,
    locator: &LocatorObservation,
    process_group: u32,
) -> Result<RegistryDocument, MigrationRefusal> {
    let record = record.ok_or(MigrationRefusal::NoRecord)?;
    let identity = record
        .process_start_identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
        .ok_or(MigrationRefusal::NoProcessIdentity)?;
    match observation {
        DaemonProcessObservation::Exact => {}
        DaemonProcessObservation::Gone => return Err(MigrationRefusal::StaleOwner),
        DaemonProcessObservation::IdentityMismatch | DaemonProcessObservation::Unknown => {
            return Err(MigrationRefusal::UnverifiedOwner);
        }
    }
    let published = match locator {
        LocatorObservation::Published(published) => published,
        LocatorObservation::Absent => return Err(MigrationRefusal::MissingLocator),
        LocatorObservation::Unreadable => return Err(MigrationRefusal::UnreadableLocator),
    };
    Ok(RegistryDocument {
        schema: REGISTRY_SCHEMA.to_owned(),
        revision: 1,
        current: Some(published.generation),
        generations: vec![GenerationEntry {
            generation: published.generation,
            role: GenerationRole::Active,
            endpoint: published.endpoint.clone(),
            process: ProcessIdentity {
                pid: record.pid,
                start_identity: identity.to_owned(),
                process_group,
            },
            expected_build: BuildIdentity::default(),
            verified_build: None,
            revision: 1,
        }],
        handoff: None,
        completed_operation: None,
    })
}

#[cfg(test)]
mod tests;
