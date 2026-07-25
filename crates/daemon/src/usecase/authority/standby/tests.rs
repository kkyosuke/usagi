use std::error::Error as _;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::DaemonGeneration as WireGeneration;

use super::*;
use crate::usecase::authority::fixture::{
    MemoryLocator, ProbeReply, RecordingProbe, build, hello, process, registry, unknown_build,
};
use crate::usecase::authority::registry::RegistryError;
use crate::usecase::generation::GenerationRole;

const ENDPOINT: &str = "generations/standby/sock";

#[test]
fn readiness_admits_only_the_exact_admitted_artifact_from_the_registered_peer() {
    let generation = DaemonGeneration::new();
    let expected = build("next");
    verify_readiness(generation, &expected, &hello(generation, &expected)).unwrap();

    let mut foreign = hello(generation, &expected);
    foreign.daemon_generation = WireGeneration(DaemonGeneration::new().as_str());
    assert_eq!(
        verify_readiness(generation, &expected, &foreign),
        Err(ReadinessRefusal::GenerationMismatch)
    );

    // Same version and target, different source tree.
    assert_eq!(
        verify_readiness(generation, &expected, &hello(generation, &build("other"))),
        Err(ReadinessRefusal::IdentityMismatch)
    );
    assert_eq!(
        verify_readiness(generation, &expected, &hello(generation, &unknown_build())),
        Err(ReadinessRefusal::IdentityUnknown)
    );
    assert_eq!(
        verify_readiness(generation, &unknown_build(), &hello(generation, &expected)),
        Err(ReadinessRefusal::IdentityUnknown)
    );

    for missing in [BUILD_ARTIFACT_CAPABILITY, GENERATION_HANDOFF_CAPABILITY] {
        let mut older = hello(generation, &expected);
        older
            .capabilities
            .retain(|advertised| advertised != missing);
        assert_eq!(
            verify_readiness(generation, &expected, &older),
            Err(ReadinessRefusal::UnsupportedCapability)
        );
    }
}

#[test]
fn preparing_a_standby_verifies_it_without_touching_the_current_locator() {
    let (store, file) = registry(2);
    let generation = DaemonGeneration::new();
    let expected = build("next");
    let probe = RecordingProbe::new(ProbeReply::Hello(Box::new(hello(generation, &expected))));
    let locator = MemoryLocator::default();

    let snapshot = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &expected,
    )
    .unwrap();

    let entry = snapshot.document().entry(generation).unwrap();
    assert_eq!(entry.role, GenerationRole::Standby);
    assert!(entry.is_build_verified());
    assert_eq!(snapshot.document().current, None);
    assert_eq!(probe.calls(), vec![ENDPOINT.to_owned()]);
    // Exactly two registry writes — admit and verify — and nothing else.
    assert_eq!(file.writes(), 2);
    assert!(locator.publishes().is_empty());
    assert_eq!(locator.retires(), 0);

    // Re-running readiness is idempotent: no probe, no further write.
    let repeated = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &expected,
    )
    .unwrap();
    assert!(
        repeated
            .document()
            .entry(generation)
            .unwrap()
            .is_build_verified()
    );
    assert_eq!(probe.calls().len(), 1);
    assert_eq!(file.writes(), 2);
}

#[test]
fn a_mismatched_or_unreachable_standby_keeps_the_old_authority() {
    let active = DaemonGeneration::new();
    let generation = DaemonGeneration::new();
    let expected = build("next");

    let cases: [(ProbeReply, &str); 3] = [
        (
            ProbeReply::Hello(Box::new(hello(generation, &build("other")))),
            "does not match",
        ),
        (
            ProbeReply::Hello(Box::new(hello(generation, &unknown_build()))),
            "unknown",
        ),
        (ProbeReply::Failure("connection refused"), "probe failed"),
    ];

    for (reply, message) in cases {
        let (store, _) = registry(2);
        store
            .update(|document| {
                document
                    .generations
                    .push(crate::usecase::authority::registry::GenerationEntry {
                        generation: active,
                        role: GenerationRole::Active,
                        endpoint: "generations/active/sock".into(),
                        process: process(11),
                        expected_build: build("old"),
                        verified_build: None,
                        revision: 1,
                    });
                document.current = Some(active);
                Ok(())
            })
            .unwrap();
        let probe = RecordingProbe::new(reply);
        let locator = MemoryLocator::default();

        let failure = prepare_standby(
            &store,
            &probe,
            generation,
            ENDPOINT,
            &process(22),
            &expected,
        )
        .unwrap_err();
        assert!(failure.to_string().contains(message), "{failure}");
        assert!(failure.source().is_none());

        let document = store.load().unwrap();
        // The candidate stays a standby and the active generation is untouched.
        assert_eq!(
            document.document().role(generation),
            Some(GenerationRole::Standby)
        );
        assert_eq!(document.document().current, Some(active));
        assert!(locator.publishes().is_empty());
    }
}

#[test]
fn an_unknown_expected_artifact_is_refused_before_a_slot_is_taken() {
    let (store, file) = registry(2);
    let generation = DaemonGeneration::new();
    let probe = RecordingProbe::new(ProbeReply::Failure("never reached"));

    let failure = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &unknown_build(),
    )
    .unwrap_err();
    assert!(matches!(&failure, StandbyFailure::Registry(registry)
            if registry.refusal() == Some(RegistryError::BuildIdentityUnknown)));
    assert!(probe.calls().is_empty());
    assert_eq!(file.writes(), 0);
    assert_eq!(store.load().unwrap().document().retained(), 0);
}

#[test]
fn every_readiness_refusal_reads_as_a_safety_outcome() {
    for refusal in [
        ReadinessRefusal::GenerationMismatch,
        ReadinessRefusal::IdentityUnknown,
        ReadinessRefusal::IdentityMismatch,
        ReadinessRefusal::UnsupportedCapability,
    ] {
        assert!(!refusal.to_string().is_empty());
        assert!(refusal.source().is_none());
        let failure: StandbyFailure = refusal.into();
        assert_eq!(failure.to_string(), refusal.to_string());
    }
    let registry: StandbyFailure = RegistryError::StaleRevision.into();
    assert_eq!(
        registry.to_string(),
        RegistryError::StaleRevision.to_string()
    );
}
