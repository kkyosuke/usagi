use serde_json::json;

use super::{OwnedRuntime, classify_request};
use crate::usecase::authority::admission::{
    AdmissionRefusal, LeaseClass, RequestClass, ResourceOwner, classify,
};
use crate::usecase::generation::GenerationRole;

/// The wire body a client actually sends, built through the shared request type
/// so a rename of the serde tags fails these tests instead of silently
/// reclassifying every request as `Control`.
fn terminal_body(action: usagi_core::usecase::client::TerminalAction) -> serde_json::Value {
    serde_json::to_value(usagi_core::usecase::client::DaemonRequest::Terminal {
        action,
        payload: json!(null),
    })
    .unwrap()
}

#[test]
fn the_terminal_surface_separates_scope_queries_from_ref_addressed_io() {
    use usagi_core::usecase::client::TerminalAction as Action;
    for (action, expected) in [
        (
            Action::Launch,
            (RequestClass::Spawn, ResourceOwner::Unscoped),
        ),
        (
            Action::Inventory,
            (RequestClass::Inventory, ResourceOwner::Unscoped),
        ),
        (
            Action::CompletedInventory,
            (RequestClass::Inventory, ResourceOwner::Unscoped),
        ),
        (
            Action::InputOutcome,
            (RequestClass::Read, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Attach,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Resume,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Resync,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Input,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Resize,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Detach,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Observe,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
        (
            Action::Dismiss,
            (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
        ),
    ] {
        assert_eq!(
            classify_request(&terminal_body(action), OwnedRuntime::Own),
            expected,
            "{action:?}"
        );
    }
}

/// A generation that owns nothing resolves every *named* runtime to another
/// generation's, and leaves the scope-addressed queries alone: those are how a
/// standby proves it can serve at all.
#[test]
fn a_generation_that_owns_nothing_resolves_every_named_runtime_elsewhere() {
    use usagi_core::usecase::client::TerminalAction as Action;
    for (action, expected) in [
        (
            Action::Attach,
            (RequestClass::TerminalIo, ResourceOwner::OtherGeneration),
        ),
        (
            Action::InputOutcome,
            (RequestClass::Read, ResourceOwner::OtherGeneration),
        ),
        // Not "named": a scope query addresses no runtime, so the stance does
        // not change it.
        (
            Action::Inventory,
            (RequestClass::Inventory, ResourceOwner::Unscoped),
        ),
        (
            Action::Launch,
            (RequestClass::Spawn, ResourceOwner::Unscoped),
        ),
    ] {
        assert_eq!(
            classify_request(&terminal_body(action), OwnedRuntime::Nothing),
            expected,
            "{action:?}"
        );
    }
}

#[test]
fn the_non_terminal_surface_names_spawns_reads_and_inventories() {
    for (kind, expected) in [
        ("agent", (RequestClass::Spawn, ResourceOwner::Unscoped)),
        (
            "resume_agent",
            (RequestClass::Spawn, ResourceOwner::Unscoped),
        ),
        (
            "agent_inventory",
            (RequestClass::Inventory, ResourceOwner::Unscoped),
        ),
        ("metrics", (RequestClass::Read, ResourceOwner::Unscoped)),
        ("pr", (RequestClass::Read, ResourceOwner::Unscoped)),
        ("session", (RequestClass::Control, ResourceOwner::Unscoped)),
        ("dispatch", (RequestClass::Control, ResourceOwner::Unscoped)),
        (
            "dispatch_tool",
            (RequestClass::Control, ResourceOwner::Unscoped),
        ),
        (
            "supervisor_tool",
            (RequestClass::Control, ResourceOwner::Unscoped),
        ),
        (
            "user_decision",
            (RequestClass::Control, ResourceOwner::Unscoped),
        ),
        (
            "codex_session_capture",
            (RequestClass::Control, ResourceOwner::Unscoped),
        ),
        (
            "agent_phase_report",
            (RequestClass::Control, ResourceOwner::Unscoped),
        ),
    ] {
        for owned in [OwnedRuntime::Own, OwnedRuntime::Nothing] {
            assert_eq!(
                classify_request(&json!({"kind": kind}), owned),
                expected,
                "{kind} under {owned:?}"
            );
        }
    }
}

/// Fail closed on everything this build cannot name: an absent, non-string, or
/// unknown `kind`, and a body that is not even an object. `Control` is the only
/// class every non-active role refuses, so an unnameable request cannot be
/// admitted by a draining, standby, or retired generation.
#[test]
fn an_unnameable_request_is_control_so_every_non_active_role_refuses_it() {
    for body in [
        json!({}),
        json!({"kind": "future_verb"}),
        json!({"kind": 7}),
        json!({"kind": null}),
        json!("not an object"),
        json!(null),
    ] {
        let classified = classify_request(&body, OwnedRuntime::Own);
        assert_eq!(
            classified,
            (RequestClass::Control, ResourceOwner::Unscoped),
            "{body}"
        );
        for role in [
            GenerationRole::Draining,
            GenerationRole::Standby,
            GenerationRole::Retired,
        ] {
            assert!(
                classify(role, classified.0, classified.1).is_err(),
                "{body} was admitted by {role:?}"
            );
        }
    }
}

/// A terminal body whose `action` this build cannot name is IO on a named
/// runtime, not a scope query — so the generation that owns nothing refuses it
/// rather than answering it.
#[test]
fn an_unnameable_terminal_action_is_ref_addressed_io() {
    let body = json!({"kind": "terminal", "action": "teleport", "payload": null});
    assert_eq!(
        classify_request(&body, OwnedRuntime::Own),
        (RequestClass::TerminalIo, ResourceOwner::SelfGeneration)
    );
    assert_eq!(
        classify_request(&body, OwnedRuntime::Nothing),
        (RequestClass::TerminalIo, ResourceOwner::OtherGeneration)
    );
    assert_eq!(
        classify(
            GenerationRole::Standby,
            RequestClass::TerminalIo,
            ResourceOwner::OtherGeneration
        ),
        Err(AdmissionRefusal::NotOwner)
    );
}

/// The classification an active generation gets for the two barriers that matter:
/// control work takes the lease a handoff closes first, and IO on a terminal it
/// owns takes the one that outlives the handoff.
#[test]
fn an_active_generation_takes_the_control_lease_for_control_and_the_owner_lease_for_its_terminals()
{
    use usagi_core::usecase::client::TerminalAction as Action;
    let (class, owner) = classify_request(&json!({"kind": "session"}), OwnedRuntime::Own);
    assert_eq!(
        classify(GenerationRole::Active, class, owner),
        Ok(Some(LeaseClass::ActiveControl))
    );
    let (class, owner) = classify_request(&terminal_body(Action::Input), OwnedRuntime::Own);
    assert_eq!(
        classify(GenerationRole::Active, class, owner),
        Ok(Some(LeaseClass::OwnerTerminal))
    );
    // The same terminal IO on a draining generation keeps its owner lease, while
    // control is already refused. That pair is the whole point of the fence.
    assert_eq!(
        classify(GenerationRole::Draining, class, owner),
        Ok(Some(LeaseClass::OwnerTerminal))
    );
    let (class, owner) = classify_request(&json!({"kind": "session"}), OwnedRuntime::Own);
    assert_eq!(
        classify(GenerationRole::Draining, class, owner),
        Err(AdmissionRefusal::NotActive)
    );
}
