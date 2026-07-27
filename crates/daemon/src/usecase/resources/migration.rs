//! Adopting the legacy single-writer stores, and refusing a rollover that cannot
//! be done seamlessly.
//!
//! `agents.json` and `terminals.json` were written by one process that could
//! assume it was the only one. Their records therefore carry two things a shard
//! needs and they cannot always prove: a complete owner generation, and a child
//! identity that can be re-observed. A fixed `start_identity` string — which is
//! what production wrote — proves nothing at all.
//!
//! Adoption is per record and never partial in effect: a record either becomes a
//! live shard resource, or it becomes
//! [`ResourceState::OwnershipUnknown`](crate::usecase::resources::shard::ResourceState::OwnershipUnknown)
//! and is reported. An unknown record is never spawned, killed, or released, and
//! never counted as live.
//!
//! The rollover itself is gated separately: a predecessor that does not advertise
//! both capabilities cannot participate in a planned handoff at all, because its
//! writes would still be whole-snapshot replacements. That is refused explicitly
//! rather than dressed up as a seamless continuation.

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use crate::usecase::resources::allocator::ResourceKind;
use crate::usecase::resources::identity::ChildIdentity;
use crate::usecase::resources::shard::{ResourceState, ShardDocument, ShardResource};

/// Advertised by a build whose children carry OS-observed start identity.
pub const REAL_CHILD_IDENTITY_CAPABILITY: &str = "daemon.child-identity.v1";

/// Advertised by a build that writes owner-generation shards and the global
/// allocator instead of whole-snapshot stores.
pub const SHARDED_STORE_CAPABILITY: &str = "daemon.owner-shard.v1";

/// One legacy runtime record, read from `agents.json` or `terminals.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyRuntimeRecord {
    pub resource: TerminalRef,
    pub kind: ResourceKind,
    /// The producer operation, when the legacy record kept one.
    pub operation: Option<OperationId>,
    pub digest: Option<String>,
    /// The recorded child, which may be an unverifiable fixed token.
    pub process: Option<ChildIdentity>,
    /// Whether the legacy store considered the record unterminated.
    pub live: bool,
}

/// Why a legacy record cannot be adopted as live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptionRefusal {
    /// The record names a different generation than the shard being built.
    ForeignGeneration,
    /// The record has no producer operation, so a replay could not be keyed.
    NoOperation,
    /// The record's child identity is missing or not OS-observable.
    UnverifiableIdentity,
    /// Two legacy records claim the same resource id, so neither can be trusted.
    Duplicate,
}

/// One record that could not be adopted as live, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRecord {
    pub resource: TerminalRef,
    pub refusal: AdoptionRefusal,
}

/// The result of adopting a legacy store into one owner shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionReport {
    pub shard: ShardDocument,
    /// Records kept as `OwnershipUnknown`. They hold no capacity and are never
    /// acted on.
    pub unknown: Vec<UnknownRecord>,
}

impl AdoptionReport {
    /// How many records were adopted as live resources.
    #[must_use]
    pub fn adopted(&self) -> usize {
        self.shard.live_resources()
    }
}

/// Adopt legacy records into `owner`'s shard.
///
/// Records that belong to another generation are still adopted as
/// `OwnershipUnknown` when they name this shard's generation; a record naming a
/// *different* generation is reported and left out entirely, because this process
/// is not its writer.
#[must_use]
pub fn adopt_legacy(owner: DaemonGeneration, records: &[LegacyRuntimeRecord]) -> AdoptionReport {
    let mut shard = ShardDocument::empty(owner);
    let mut unknown = Vec::new();
    for record in records {
        let rejected = if record.resource.daemon_generation != owner {
            Some(AdoptionRefusal::ForeignGeneration)
        } else if shard.resource(&record.resource).is_some() {
            Some(AdoptionRefusal::Duplicate)
        } else {
            None
        };
        if let Some(refusal) = rejected {
            unknown.push(UnknownRecord {
                resource: record.resource.clone(),
                refusal,
            });
            continue;
        }
        // A terminated legacy record holds nothing and proves nothing, so it is
        // adopted as unknown rather than given a fabricated exit status.
        let refusal = classify(record);
        let state = match refusal {
            None if record.live => ResourceState::Running,
            _ => ResourceState::OwnershipUnknown,
        };
        let process = record.process.clone();
        if let Some(refusal) = refusal {
            unknown.push(UnknownRecord {
                resource: record.resource.clone(),
                refusal,
            });
        }
        shard.resources.push(ShardResource {
            resource: record.resource.clone(),
            kind: record.kind,
            operation: record
                .operation
                .unwrap_or_else(legacy_operation_placeholder),
            digest: record.digest.clone().unwrap_or_default(),
            process,
            state,
            payload: None,
            revision: 1,
        });
    }
    AdoptionReport { shard, unknown }
}

/// Why a planned rollover was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverRefusal {
    /// The predecessor's children have no OS-observable identity.
    NoRealChildIdentity,
    /// The predecessor still writes whole-snapshot stores.
    NoShardedStore,
}

/// Whether the predecessor may take part in a planned rollover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverAdmission {
    /// Both capabilities are advertised: a seamless handoff is possible.
    Allowed,
    /// The predecessor cannot hand over safely, so the caller must refuse or
    /// perform an explicit cold transition (stop, then start).
    ColdTransitionRequired(RolloverRefusal),
}

/// Decide whether an old active generation may hand over while it stays alive.
#[must_use]
pub fn planned_rollover_admission(capabilities: &[String]) -> RolloverAdmission {
    for (capability, refusal) in [
        (
            REAL_CHILD_IDENTITY_CAPABILITY,
            RolloverRefusal::NoRealChildIdentity,
        ),
        (SHARDED_STORE_CAPABILITY, RolloverRefusal::NoShardedStore),
    ] {
        if !capabilities
            .iter()
            .any(|advertised| advertised == capability)
        {
            return RolloverAdmission::ColdTransitionRequired(refusal);
        }
    }
    RolloverAdmission::Allowed
}

fn classify(record: &LegacyRuntimeRecord) -> Option<AdoptionRefusal> {
    if record.operation.is_none() {
        return Some(AdoptionRefusal::NoOperation);
    }
    if !record
        .process
        .as_ref()
        .is_some_and(ChildIdentity::is_verifiable)
    {
        return Some(AdoptionRefusal::UnverifiableIdentity);
    }
    None
}

/// A stable, obviously synthetic operation id for a legacy record that never had
/// one. It exists so the shard stays well formed; it is never admitted, because
/// the record it belongs to is `OwnershipUnknown`.
fn legacy_operation_placeholder() -> OperationId {
    OperationId::parse("00000000-0000-7000-8000-000000000000")
        .expect("the canonical legacy placeholder is a valid UUIDv7")
}

#[cfg(test)]
mod tests;
