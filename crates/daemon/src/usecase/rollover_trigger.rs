//! The active daemon's IPC-triggered authority handoff.
//!
//! The caller supplies only a durable operation id. The active process reads
//! the registry itself, re-probes the registered successor, verifies the hello,
//! and closes its own process-local admission barrier before committing the
//! handoff.

use std::fmt;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{OperationId, ServerHello};

use super::authority::admission::AdmissionGate;
use super::authority::registry::{GenerationRegistry, RegistryFailure};
use super::authority::rollover::{
    CurrentLocator, HandoffFailure, RolloverPlan, execute_gated_rollover,
};
use super::authority::routing::RoutingLedger;
use super::authority::standby::{ReadinessRefusal, StandbyProbe, verify_readiness};
use super::generation::GenerationRole;

/// A typed refusal before the handoff is allowed to write anything.
#[derive(Debug)]
pub enum RolloverTriggerFailure {
    Registry(RegistryFailure),
    NoActiveGeneration,
    ActiveGenerationMismatch,
    NoVerifiedStandby,
    MultipleVerifiedStandbys,
    Probe(std::io::Error),
    Readiness(ReadinessRefusal),
    Handoff(HandoffFailure),
}

impl fmt::Display for RolloverTriggerFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(f, "{error}"),
            Self::NoActiveGeneration => f.write_str("no registered active generation is alive"),
            Self::ActiveGenerationMismatch => {
                f.write_str("this daemon is not the registry's active generation")
            }
            Self::NoVerifiedStandby => f.write_str("no verified standby generation is registered"),
            Self::MultipleVerifiedStandbys => {
                f.write_str("more than one verified standby generation is registered")
            }
            Self::Probe(error) => write!(f, "standby endpoint probe failed: {error}"),
            Self::Readiness(error) => write!(f, "{error}"),
            Self::Handoff(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RolloverTriggerFailure {}

impl From<RegistryFailure> for RolloverTriggerFailure {
    fn from(error: RegistryFailure) -> Self {
        Self::Registry(error)
    }
}

impl From<HandoffFailure> for RolloverTriggerFailure {
    fn from(error: HandoffFailure) -> Self {
        Self::Handoff(error)
    }
}

/// Verify the registered successor and execute the handoff from this process.
///
/// # Errors
/// Returns a typed, effect-zero preflight refusal, or the handoff failure. The
/// handoff implementation restores the old gate for every pre-commit failure;
/// post-commit failures remain durable for recovery to roll forward.
pub fn execute(
    registry: &GenerationRegistry,
    locator: &dyn CurrentLocator,
    gate: &AdmissionGate,
    ledger: &RoutingLedger,
    probe: &dyn StandbyProbe,
    operation: &OperationId,
) -> Result<super::authority::handoff::RolloverOutcome, RolloverTriggerFailure> {
    let snapshot = registry.load()?;
    let document = snapshot.document();
    let active = document
        .active()
        .ok_or(RolloverTriggerFailure::NoActiveGeneration)?;
    if active.generation != gate.generation() || active.role != GenerationRole::Active {
        return Err(RolloverTriggerFailure::ActiveGenerationMismatch);
    }
    let mut candidates = document
        .generations
        .iter()
        .filter(|entry| entry.role == GenerationRole::Standby && entry.is_build_verified());
    let successor = candidates
        .next()
        .ok_or(RolloverTriggerFailure::NoVerifiedStandby)?;
    if candidates.next().is_some() {
        return Err(RolloverTriggerFailure::MultipleVerifiedStandbys);
    }
    let hello: ServerHello = probe
        .hello(&successor.endpoint)
        .map_err(RolloverTriggerFailure::Probe)?;
    verify_readiness(successor.generation, &successor.expected_build, &hello)
        .map_err(RolloverTriggerFailure::Readiness)?;
    let plan = RolloverPlan {
        ledger,
        successor: &hello,
        planned_revision: document.revision,
    };
    execute_gated_rollover(
        registry,
        locator,
        Some(gate),
        &plan,
        operation,
        Some(active.generation),
        successor.generation,
    )
    .map_err(Into::into)
}

/// Extract a canonical generation from the successor hello for tests/adapters.
#[must_use]
pub fn successor_generation(hello: &ServerHello) -> Option<DaemonGeneration> {
    DaemonGeneration::parse(&hello.daemon_generation.0).ok()
}

#[cfg(test)]
mod tests {
    use usagi_core::domain::id::{ConnectionId, DaemonGeneration};
    use usagi_core::infrastructure::ipc::{ClientHello, ClientId};

    use super::*;
    use crate::usecase::authority::admission::AdmissionGate;
    use crate::usecase::authority::fixture::{
        MemoryLocator, ProbeReply, RecordingProbe, build, hello, operation, process, registry,
    };
    use crate::usecase::authority::handoff::PublishedLocator;
    use crate::usecase::authority::registry::GenerationEntry;
    use crate::usecase::authority::routing::RolloverRefusal;
    use crate::usecase::authority::routing::RoutingLedger;
    use crate::usecase::authority::standby::StandbyProbe;
    use crate::usecase::generation::GenerationRole;

    struct World {
        registry: GenerationRegistry,
        file: std::sync::Arc<crate::usecase::authority::fixture::MemoryRegistryFile>,
        locator: MemoryLocator,
        gate: AdmissionGate,
        ledger: RoutingLedger,
        next: DaemonGeneration,
    }

    fn world() -> World {
        let (registry, file) = registry(2);
        let old = DaemonGeneration::new();
        let next = DaemonGeneration::new();
        registry
            .update(|document| {
                document.generations.push(GenerationEntry {
                    generation: old,
                    role: GenerationRole::Active,
                    endpoint: "generations/old/sock".into(),
                    process: process(1),
                    expected_build: build("old"),
                    verified_build: Some(build("old")),
                    revision: 1,
                });
                document.current = Some(old);
                Ok(())
            })
            .unwrap();
        registry
            .update(|document| {
                document.register_standby(
                    2,
                    next,
                    "generations/next/sock",
                    process(2),
                    build("next"),
                )
            })
            .unwrap();
        registry
            .update(|document| document.verify_standby_build(next, &build("next")))
            .unwrap();
        World {
            registry,
            file,
            locator: MemoryLocator::naming(PublishedLocator {
                generation: old,
                endpoint: "generations/old/sock".into(),
            }),
            gate: AdmissionGate::new(old, GenerationRole::Active),
            ledger: RoutingLedger::new(),
            next,
        }
    }

    #[test]
    fn readiness_failure_is_effect_zero() {
        let world = world();
        let before = world.file.contents();
        let writes = world.file.writes();
        let probe = RecordingProbe::new(ProbeReply::Failure("not ready"));
        assert!(matches!(
            execute(
                &world.registry,
                &world.locator,
                &world.gate,
                &world.ledger,
                &probe,
                &operation("readiness"),
            ),
            Err(RolloverTriggerFailure::Probe(_))
        ));
        assert_eq!(world.file.contents(), before);
        assert_eq!(world.file.writes(), writes);
        assert_eq!(world.gate.role(), GenerationRole::Active);
    }

    #[test]
    fn unsupported_client_routing_is_effect_zero() {
        let world = world();
        let before = world.file.contents();
        let writes = world.file.writes();
        world.ledger.admit(
            ConnectionId::new(),
            &ClientHello {
                client_id: ClientId("legacy".into()),
                connection_nonce: "nonce".into(),
                expected_daemon_generation: None,
                supported_protocols: Vec::new(),
                capabilities: Vec::new(),
                required_capabilities: Vec::new(),
                build: build("old"),
                workspace: None,
            },
        );
        let probe = RecordingProbe::new(ProbeReply::Hello(Box::new(hello(
            world.next,
            &build("next"),
        ))));
        assert!(matches!(
            execute(
                &world.registry,
                &world.locator,
                &world.gate,
                &world.ledger,
                &probe,
                &operation("routing"),
            ),
            Err(RolloverTriggerFailure::Handoff(HandoffFailure::Routing(_)))
        ));
        assert_eq!(world.file.contents(), before);
        assert_eq!(world.file.writes(), writes);
        assert_eq!(world.gate.role(), GenerationRole::Active);
    }

    struct MovingProbe {
        file: std::sync::Arc<crate::usecase::authority::fixture::MemoryRegistryFile>,
        hello: ServerHello,
    }

    impl StandbyProbe for MovingProbe {
        fn hello(&self, _endpoint: &str) -> std::io::Result<ServerHello> {
            let bytes = self.file.contents().unwrap();
            let mut document: crate::usecase::authority::registry::RegistryDocument =
                serde_json::from_str(&bytes).unwrap();
            document.revision += 1;
            self.file
                .set_contents(Some(&serde_json::to_string(&document).unwrap()));
            Ok(self.hello.clone())
        }
    }

    #[test]
    fn a_registry_revision_move_is_effect_zero() {
        let world = world();
        let writes = world.file.writes();
        let probe = MovingProbe {
            file: std::sync::Arc::clone(&world.file),
            hello: hello(world.next, &build("next")),
        };
        assert!(matches!(
            execute(
                &world.registry,
                &world.locator,
                &world.gate,
                &world.ledger,
                &probe,
                &operation("revision"),
            ),
            Err(RolloverTriggerFailure::Handoff(HandoffFailure::Routing(
                RolloverRefusal::RegistryRevisionMismatch { .. }
            )))
        ));
        assert_eq!(world.file.writes(), writes);
        assert_eq!(world.gate.role(), GenerationRole::Active);
    }

    #[test]
    fn verified_successor_is_handed_authority_by_the_old_gate() {
        let world = world();
        let probe = RecordingProbe::new(ProbeReply::Hello(Box::new(hello(
            world.next,
            &build("next"),
        ))));
        execute(
            &world.registry,
            &world.locator,
            &world.gate,
            &world.ledger,
            &probe,
            &operation("success"),
        )
        .unwrap();
        assert_eq!(world.gate.role(), GenerationRole::Draining);
        assert_eq!(
            world.registry.load().unwrap().document().current,
            Some(world.next)
        );
    }
}
