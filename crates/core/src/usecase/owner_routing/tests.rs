use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::json;

use super::*;
use crate::domain::id::{SessionId, TerminalId, WorkspaceId, WorktreeId};
use crate::domain::terminal_launch::TerminalKind;
use crate::usecase::client::{SessionAction, TerminalGeometry};

// ------------------------------------------------------------------ fixtures

fn generation(seed: u128) -> DaemonGeneration {
    DaemonGeneration::parse(&uuid::Uuid::from_u128(seed).hyphenated().to_string()).unwrap()
}

fn active(seed: u128) -> TrustedEndpoint {
    let generation = generation(seed);
    TrustedEndpoint {
        generation,
        role: GenerationRole::Active,
        endpoint: format!("generations/{generation}/sock"),
    }
}

fn draining(seed: u128) -> TrustedEndpoint {
    TrustedEndpoint {
        role: GenerationRole::Draining,
        ..active(seed)
    }
}

fn scope() -> TerminalLaunchScope {
    TerminalLaunchScope {
        workspace_id: WorkspaceId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        session_id: None,
        worktree_id: WorktreeId::parse("22222222-2222-4222-8222-222222222222").unwrap(),
    }
}

fn terminal(owner: DaemonGeneration, seed: u128) -> TerminalRef {
    let scope = scope();
    TerminalRef {
        daemon_generation: owner,
        terminal_id: TerminalId::parse(&uuid::Uuid::from_u128(seed).hyphenated().to_string())
            .unwrap(),
        workspace_id: scope.workspace_id,
        session_id: scope.session_id,
        worktree_id: scope.worktree_id,
    }
}

fn entry(terminal: TerminalRef, live: bool) -> TerminalInventoryEntry {
    TerminalInventoryEntry {
        terminal,
        kind: TerminalKind::Terminal,
        live,
    }
}

fn listed(
    generation: DaemonGeneration,
    entries: Vec<TerminalInventoryEntry>,
) -> GenerationInventory {
    GenerationInventory {
        generation,
        outcome: InventoryOutcome::Listed(entries),
    }
}

fn unreachable(generation: DaemonGeneration) -> GenerationInventory {
    GenerationInventory {
        generation,
        outcome: InventoryOutcome::Unreachable,
    }
}

fn two_generations() -> TrustedEndpoints {
    TrustedEndpoints::build(Some(generation(2)), vec![draining(1), active(2)]).unwrap()
}

// ------------------------------------------------------------ trusted set

#[test]
fn trusted_set_orders_active_first_then_draining_by_identity() {
    let endpoints = TrustedEndpoints::build(
        Some(generation(9)),
        vec![draining(3), active(9), draining(1)],
    )
    .unwrap();
    let order: Vec<_> = endpoints
        .all()
        .iter()
        .map(|entry| entry.generation)
        .collect();
    assert_eq!(order, vec![generation(9), generation(1), generation(3)]);
    assert_eq!(endpoints.active().unwrap().generation, generation(9));
    assert_eq!(
        endpoints.owner(generation(3)).unwrap().role,
        GenerationRole::Draining
    );
    assert!(endpoints.owner(generation(4)).is_none());
    assert!(!endpoints.is_empty());
}

#[test]
fn trusted_set_rejects_records_that_do_not_describe_one_authority() {
    for (current, entries) in [
        (Some(generation(1)), vec![active(1), draining(1)]),
        (Some(generation(1)), vec![active(1), active(2)]),
        (Some(generation(2)), vec![active(1)]),
        (None, vec![active(1)]),
        (Some(generation(1)), vec![draining(1)]),
    ] {
        assert!(matches!(
            TrustedEndpoints::build(current, entries),
            Err(DirectoryError::Corrupt(_))
        ));
    }
    // A set of draining-only generations with no `current` is consistent: the
    // active generation simply has not been published yet.
    let endpoints = TrustedEndpoints::build(None, vec![draining(1)]).unwrap();
    assert!(endpoints.active().is_none());
    assert!(TrustedEndpoints::default().is_empty());
}

#[test]
fn directory_errors_describe_themselves() {
    assert!(
        DirectoryError::Unreadable("permission denied".into())
            .to_string()
            .contains("permission denied")
    );
    assert!(
        DirectoryError::Corrupt("generation is listed twice")
            .to_string()
            .contains("listed twice")
    );
}

// ---------------------------------------------------------------- routing

#[test]
fn control_operations_route_to_the_active_generation() {
    let control = DaemonRequest::Session {
        action: SessionAction::Create,
        operation_id: "operation".into(),
        payload: json!({}),
    };
    assert_eq!(
        route_daemon_request(&control).unwrap(),
        RouteTarget::ActiveControl
    );

    let launch = terminal_request(&TerminalRequest::Launch {
        intent: crate::usecase::client::TerminalLaunchIntent {
            request: crate::domain::terminal_launch::TerminalLaunchRequest {
                scope: scope(),
                profile_id: crate::domain::terminal_launch::TerminalProfileId::new("shell")
                    .unwrap(),
            },
            geometry: TerminalGeometry { cols: 80, rows: 24 },
            launch_operation: None,
        },
    });
    assert_eq!(
        route_daemon_request(&launch).unwrap(),
        RouteTarget::ActiveControl
    );
}

#[test]
fn every_reference_addressed_request_routes_to_its_owner_generation() {
    let owner = generation(1);
    let target = terminal(owner, 7);
    let requests = [
        TerminalRequest::Attach {
            terminal: target.clone(),
        },
        TerminalRequest::Resume {
            terminal: target.clone(),
            after_offset: 10,
        },
        TerminalRequest::Resync {
            terminal: target.clone(),
        },
        TerminalRequest::Input {
            terminal: target.clone(),
            subscription: 1,
            input_seq: 2,
            input_operation: None,
            bytes: vec![b'x'],
        },
        TerminalRequest::InputOutcome {
            terminal: target.clone(),
            input_operation: crate::domain::id::OperationId::new(),
        },
        TerminalRequest::Resize {
            terminal: target.clone(),
            geometry: TerminalGeometry { cols: 10, rows: 5 },
        },
        TerminalRequest::Detach {
            terminal: target.clone(),
            subscription: 1,
        },
        TerminalRequest::Observe {
            terminal: target.clone(),
            expected_revision: 3,
        },
        TerminalRequest::Dismiss {
            terminal: target.clone(),
            expected_revision: 3,
        },
    ];
    for request in requests {
        assert_eq!(
            route_terminal_request(&request),
            RouteTarget::Owner(owner),
            "{request:?}"
        );
        assert_eq!(
            route_daemon_request(&terminal_request(&request)).unwrap(),
            RouteTarget::Owner(owner)
        );
    }
}

#[test]
fn scope_queries_route_to_every_generation() {
    for request in [
        TerminalRequest::Inventory { scope: scope() },
        TerminalRequest::CompletedInventory { scope: scope() },
    ] {
        assert_eq!(
            route_daemon_request(&terminal_request(&request)).unwrap(),
            RouteTarget::EveryGeneration
        );
    }
}

#[test]
fn a_payload_that_contradicts_its_action_is_refused_instead_of_routed() {
    let mismatched = DaemonRequest::Terminal {
        action: TerminalAction::Input,
        payload: serde_json::to_value(TerminalRequest::Attach {
            terminal: terminal(generation(1), 7),
        })
        .unwrap(),
    };
    assert_eq!(
        route_daemon_request(&mismatched),
        Err(RoutingError::Unroutable)
    );

    let undecodable = DaemonRequest::Terminal {
        action: TerminalAction::Attach,
        payload: json!({"operation": "attach", "terminal": "not-a-ref"}),
    };
    assert_eq!(
        route_daemon_request(&undecodable),
        Err(RoutingError::Unroutable)
    );
}

#[test]
fn resolution_never_falls_back_to_the_active_endpoint() {
    let endpoints = two_generations();
    assert_eq!(
        resolve_route(&RouteTarget::ActiveControl, &endpoints).unwrap(),
        RouteResolution::Single(active(2))
    );
    assert_eq!(
        resolve_route(&RouteTarget::Owner(generation(1)), &endpoints).unwrap(),
        RouteResolution::Single(draining(1))
    );
    assert_eq!(
        resolve_route(&RouteTarget::Owner(generation(7)), &endpoints),
        Err(RoutingError::UnknownGeneration(generation(7)))
    );
    assert_eq!(
        resolve_route(&RouteTarget::EveryGeneration, &endpoints).unwrap(),
        RouteResolution::FanOut(vec![active(2), draining(1)])
    );

    let empty = TrustedEndpoints::default();
    for target in [RouteTarget::ActiveControl, RouteTarget::EveryGeneration] {
        assert_eq!(
            resolve_route(&target, &empty),
            Err(RoutingError::NoActiveGeneration)
        );
    }
    let draining_only = TrustedEndpoints::build(None, vec![draining(1)]).unwrap();
    assert_eq!(
        resolve_route(&RouteTarget::ActiveControl, &draining_only),
        Err(RoutingError::NoActiveGeneration)
    );

    assert_eq!(
        RouteResolution::Single(active(2)).into_endpoints(),
        vec![active(2)]
    );
    assert_eq!(
        RouteResolution::FanOut(vec![active(2), draining(1)]).into_endpoints(),
        vec![active(2), draining(1)]
    );
}

#[test]
fn a_scope_query_is_refused_by_the_single_endpoint_path() {
    // `request` answers from one endpoint, so a query every generation must
    // answer belongs to `inventory` and is refused rather than half-answered.
    let mut harness = router_harness(vec![Ok(two_generations())]);
    let error = harness
        .router
        .request(terminal_request(&TerminalRequest::Inventory {
            scope: scope(),
        }))
        .unwrap_err();
    let ClientError::Protocol(error) = error else {
        panic!("a misrouted scope query is a typed refusal");
    };
    assert_eq!(error.code, ErrorCode::StaleTarget);
    assert!(harness.recorder.borrow().sent.is_empty());
}

#[test]
fn routing_refusals_are_typed_and_effect_free() {
    let stale = RoutingError::UnknownGeneration(generation(4)).to_client_error();
    let ClientError::Protocol(error) = stale else {
        panic!("routing refusal is a protocol error");
    };
    assert_eq!(error.code, ErrorCode::StaleTarget);
    assert_eq!(error.side_effect, SideEffect::None);
    assert_eq!(error.error_id, "owner-generation-routing");
    assert!(error.current_daemon_generation.is_none());
    assert_eq!(
        error.details.unwrap()["owner_generation"],
        serde_json::json!(generation(4).as_str())
    );

    for (routing, code) in [
        (RoutingError::Unroutable, ErrorCode::StaleTarget),
        (RoutingError::NoActiveGeneration, ErrorCode::Unavailable),
        (
            RoutingError::Directory(DirectoryError::Unreadable("io".into())),
            ErrorCode::Unavailable,
        ),
    ] {
        let ClientError::Protocol(error) = routing.to_client_error() else {
            panic!("routing refusal is a protocol error");
        };
        assert_eq!(error.code, code);
        assert_eq!(error.side_effect, SideEffect::None);
        assert!(!routing.to_string().is_empty());
    }
    assert_eq!(
        RoutingError::from(DirectoryError::Corrupt("x")),
        RoutingError::Directory(DirectoryError::Corrupt("x"))
    );
}

// -------------------------------------------------------------- merging

#[test]
fn merged_inventory_is_deduplicated_deterministic_and_generation_fenced() {
    let old = generation(1);
    let new = generation(2);
    let mine = terminal(old, 10);
    let theirs = terminal(new, 11);
    let out_of_scope = TerminalRef {
        session_id: Some(SessionId::parse("33333333-3333-4333-8333-333333333333").unwrap()),
        ..terminal(new, 12)
    };
    let parts = vec![
        listed(
            new,
            vec![
                entry(theirs.clone(), true),
                // The new generation may not introduce the old owner's terminal.
                entry(mine.clone(), true),
                entry(out_of_scope, true),
            ],
        ),
        listed(
            old,
            vec![entry(mine.clone(), true), entry(mine.clone(), true)],
        ),
    ];
    let merged = merge_inventory(&parts, &scope());
    let refs: Vec<_> = merged
        .entries()
        .iter()
        .map(|entry| entry.terminal.clone())
        .collect();
    let mut expected = vec![mine.clone(), theirs.clone()];
    expected.sort();
    assert_eq!(refs, expected);
    assert_eq!(merged.answered().len(), 2);
    assert!(!merged.is_partial());
    assert!(merged.answered_any());

    // Reversing the answers cannot change the projection.
    let reversed = merge_inventory(&parts.iter().rev().cloned().collect::<Vec<_>>(), &scope());
    assert_eq!(merged, reversed);
}

#[test]
fn an_unreachable_generation_is_recorded_as_partial_not_as_absence() {
    let old = generation(1);
    let new = generation(2);
    let merged = merge_inventory(
        &[
            listed(new, vec![entry(terminal(new, 11), true)]),
            unreachable(old),
        ],
        &scope(),
    );
    assert!(merged.is_partial());
    assert_eq!(
        merged.unreachable().iter().copied().collect::<Vec<_>>(),
        vec![old]
    );
    assert!(merged.answered_any());

    let nothing = merge_inventory(&[unreachable(old), unreachable(new)], &scope());
    assert!(!nothing.answered_any());
    assert!(nothing.entries().is_empty());
    assert_eq!(MergedInventory::default().entries(), &[]);
}

#[test]
fn presence_collects_a_tab_only_on_an_authoritative_answer_or_a_verified_retirement() {
    let old = generation(1);
    let new = generation(2);
    let endpoints = two_generations();
    let live = terminal(old, 10);
    let exited = terminal(old, 11);
    let vanished = terminal(old, 12);

    let answered = merge_inventory(
        &[
            listed(
                old,
                vec![entry(live.clone(), true), entry(exited.clone(), false)],
            ),
            listed(new, vec![]),
        ],
        &scope(),
    );
    assert_eq!(
        presence_of(&live, &answered, &endpoints),
        OwnerPresence::Live
    );
    assert_eq!(
        presence_of(&exited, &answered, &endpoints),
        OwnerPresence::Gone
    );
    assert_eq!(
        presence_of(&vanished, &answered, &endpoints),
        OwnerPresence::Gone
    );

    // The same tab, with its owner silent, is kept.
    let partial = merge_inventory(&[unreachable(old), listed(new, vec![])], &scope());
    for tracked in [&live, &exited, &vanished] {
        assert_eq!(
            presence_of(tracked, &partial, &endpoints),
            OwnerPresence::Reconnecting
        );
    }

    // Retirement removes the generation from the trusted set, which is the
    // authority that finally collects the tab.
    let retired = TrustedEndpoints::build(Some(new), vec![active(2)]).unwrap();
    assert_eq!(presence_of(&live, &partial, &retired), OwnerPresence::Gone);
}

// ------------------------------------------------------- per-generation links

/// A connected session for the link tests. They are about which link is held,
/// reused, or dropped — never about the traffic over it — so this reuses the
/// router's own fake rather than introducing a second one.
fn connected(endpoint: &TrustedEndpoint) -> Box<dyn DaemonSession> {
    Box::new(FakeSession {
        generation: endpoint.generation,
        recorder: Rc::new(RefCell::new(Recorder::default())),
        replies: Rc::new(RefCell::new(BTreeMap::new())),
    })
}

#[test]
fn links_are_kept_per_generation_across_an_active_locator_change() {
    let mut links = GenerationLinks::new();
    assert!(links.is_empty());
    let old = draining(1);
    let new = active(2);
    let mut connects = 0;
    for endpoint in [&old, &new, &old] {
        links
            .session(endpoint, &mut |target| {
                connects += 1;
                Ok(connected(target))
            })
            .unwrap();
    }
    assert_eq!(connects, 2, "an existing link is reused");
    assert_eq!(links.len(), 2);
    assert!(links.is_connected(generation(1)));

    // Publishing a different `current` keeps both generations addressable, so
    // the draining link survives the handoff.
    let after_handoff =
        TrustedEndpoints::build(Some(generation(2)), vec![draining(1), active(2)]).unwrap();
    links.retain_trusted(&after_handoff);
    assert_eq!(links.len(), 2);

    // Retiring the old generation is what collects its link.
    let collected = TrustedEndpoints::build(Some(generation(2)), vec![active(2)]).unwrap();
    links.retain_trusted(&collected);
    assert_eq!(links.len(), 1);
    assert!(!links.is_connected(generation(1)));
}

#[test]
fn a_transport_failure_drops_only_that_generation_socket_and_keeps_its_cursor() {
    let mut links = GenerationLinks::new();
    let old = draining(1);
    let tracked = terminal(generation(1), 10);
    links
        .session(&old, &mut |target| Ok(connected(target)))
        .unwrap();
    assert!(links.advance_cursor(&tracked, 100));
    assert!(
        !links.advance_cursor(&tracked, 40),
        "a late frame cannot rewind"
    );
    assert!(links.advance_cursor(&tracked, 140));

    links.invalidate(generation(1));
    links.invalidate(generation(9));
    assert!(!links.is_connected(generation(1)));
    assert_eq!(
        links.cursor(&tracked),
        Some(140),
        "the reconnect resumes rather than replays"
    );

    let mut reconnects = 0;
    links
        .session(&old, &mut |target| {
            reconnects += 1;
            Ok(connected(target))
        })
        .unwrap();
    assert_eq!(reconnects, 1);
    assert_eq!(links.cursor(&tracked), Some(140));

    links.forget(&tracked);
    assert_eq!(links.cursor(&tracked), None);
    links.forget(&terminal(generation(9), 1));
    assert_eq!(links.cursor(&terminal(generation(9), 1)), None);
    assert!(!links.advance_cursor(&terminal(generation(9), 1), 1));
}

#[test]
fn a_republished_endpoint_for_the_same_generation_is_not_reused() {
    let mut links = GenerationLinks::new();
    let first = draining(1);
    let moved = TrustedEndpoint {
        endpoint: "generations/other/sock".into(),
        ..first.clone()
    };
    let mut connects = 0;
    for endpoint in [&first, &moved] {
        links
            .session(endpoint, &mut |target| {
                connects += 1;
                Ok(connected(target))
            })
            .unwrap();
    }
    assert_eq!(connects, 2);
    assert_eq!(GenerationLinks::default().len(), 0);

    let mut failing = GenerationLinks::new();
    assert!(
        failing
            .session(&first, &mut |_| Err(ClientError::Unavailable(
                "refused".into()
            )))
            .is_err()
    );
    assert!(!failing.is_connected(generation(1)));
}

// ------------------------------------------------------------------- router

#[derive(Clone, Default)]
struct Recorder {
    connects: Vec<DaemonGeneration>,
    sent: Vec<(DaemonGeneration, DaemonRequest)>,
}

type Shared = Rc<RefCell<Recorder>>;

/// The scripted answers each generation gives, in order.
type Scripted = Rc<RefCell<BTreeMap<DaemonGeneration, Vec<Result<DaemonReply, ClientError>>>>>;

/// A session bound to one generation, scripted per generation.
struct FakeSession {
    generation: DaemonGeneration,
    recorder: Shared,
    replies: Scripted,
}

impl DaemonSession for FakeSession {
    fn exchange(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        self.recorder
            .borrow_mut()
            .sent
            .push((self.generation, request));
        let mut replies = self.replies.borrow_mut();
        let queue = replies.entry(self.generation).or_default();
        if queue.is_empty() {
            return Ok(DaemonReply::Ok(json!({"terminals": []})));
        }
        queue.remove(0)
    }

    fn rearm(&mut self, _budget_ms: u64) {}
}

struct FakeTransport {
    recorder: Shared,
    replies: Scripted,
    refuse: Option<DaemonGeneration>,
}

impl GenerationTransport for FakeTransport {
    fn connect(
        &mut self,
        endpoint: &TrustedEndpoint,
    ) -> Result<Box<dyn DaemonSession>, ClientError> {
        if self.refuse == Some(endpoint.generation) {
            return Err(ClientError::Unavailable("endpoint refused".into()));
        }
        self.recorder
            .borrow_mut()
            .connects
            .push(endpoint.generation);
        Ok(Box::new(FakeSession {
            generation: endpoint.generation,
            recorder: Rc::clone(&self.recorder),
            replies: Rc::clone(&self.replies),
        }))
    }
}

struct FakeDirectory {
    snapshots: RefCell<Vec<Result<TrustedEndpoints, DirectoryError>>>,
    reads: RefCell<usize>,
}

impl FakeDirectory {
    fn of(snapshots: Vec<Result<TrustedEndpoints, DirectoryError>>) -> Self {
        Self {
            snapshots: RefCell::new(snapshots),
            reads: RefCell::new(0),
        }
    }
}

impl GenerationDirectory for FakeDirectory {
    fn snapshot(&self) -> Result<TrustedEndpoints, DirectoryError> {
        *self.reads.borrow_mut() += 1;
        let mut snapshots = self.snapshots.borrow_mut();
        if snapshots.len() > 1 {
            snapshots.remove(0)
        } else {
            snapshots[0].clone()
        }
    }
}

struct Harness {
    router: OwnerRouter,
    recorder: Shared,
    replies: Scripted,
}

fn router_harness(snapshots: Vec<Result<TrustedEndpoints, DirectoryError>>) -> Harness {
    let recorder: Shared = Rc::new(RefCell::new(Recorder::default()));
    let replies = Rc::new(RefCell::new(BTreeMap::new()));
    let transport = FakeTransport {
        recorder: Rc::clone(&recorder),
        replies: Rc::clone(&replies),
        refuse: None,
    };
    Harness {
        router: OwnerRouter::new(FakeDirectory::of(snapshots), transport),
        recorder,
        replies,
    }
}

fn attach(owner: DaemonGeneration) -> DaemonRequest {
    terminal_request(&TerminalRequest::Attach {
        terminal: terminal(owner, 10),
    })
}

fn control() -> DaemonRequest {
    DaemonRequest::Session {
        action: SessionAction::Create,
        operation_id: "operation".into(),
        payload: json!({}),
    }
}

#[test]
fn the_router_sends_old_terminal_work_to_its_owner_and_new_control_to_the_active() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    harness.router.request(attach(generation(1))).unwrap();
    harness.router.request(control()).unwrap();
    harness.router.request(attach(generation(1))).unwrap();

    let recorder = harness.recorder.borrow();
    assert_eq!(recorder.connects, vec![generation(1), generation(2)]);
    let targets: Vec<_> = recorder.sent.iter().map(|(owner, _)| *owner).collect();
    assert_eq!(
        targets,
        vec![generation(1), generation(2), generation(1)],
        "no request lands on the wrong generation, and the owner link is reused"
    );
    assert_eq!(harness.router.links().len(), 2);
}

#[test]
fn the_router_refuses_a_payload_that_cannot_name_its_owner_without_connecting() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    let mismatched = DaemonRequest::Terminal {
        action: TerminalAction::Input,
        payload: serde_json::to_value(TerminalRequest::Attach {
            terminal: terminal(generation(1), 10),
        })
        .unwrap(),
    };
    let ClientError::Protocol(error) = harness.router.request(mismatched).unwrap_err() else {
        panic!("an unroutable payload is a typed refusal");
    };
    assert_eq!(error.code, ErrorCode::StaleTarget);
    // The refusal precedes the directory entirely: nothing was connected, and
    // nothing was sent under an action the payload does not describe.
    assert!(harness.recorder.borrow().connects.is_empty());
    assert!(harness.recorder.borrow().sent.is_empty());
}

#[test]
fn an_unknown_owner_is_refused_after_one_refresh_and_never_rerouted() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    let error = harness.router.request(attach(generation(7))).unwrap_err();
    let ClientError::Protocol(error) = error else {
        panic!("an unaddressable owner is a typed refusal");
    };
    assert_eq!(error.code, ErrorCode::StaleTarget);
    assert!(harness.recorder.borrow().sent.is_empty());

    // A snapshot that predates the handoff is refreshed exactly once, and the
    // owner published by the refresh is then reachable.
    let mut late = router_harness(vec![
        Ok(TrustedEndpoints::build(Some(generation(2)), vec![active(2)]).unwrap()),
        Ok(two_generations()),
    ]);
    late.router.request(attach(generation(1))).unwrap();
    assert_eq!(late.recorder.borrow().connects, vec![generation(1)]);
}

#[test]
fn an_unreadable_directory_refuses_without_unaddressing_a_live_owner() {
    let mut harness = router_harness(vec![
        Ok(two_generations()),
        Err(DirectoryError::Unreadable("io".into())),
    ]);
    harness.router.request(attach(generation(1))).unwrap();
    // The refresh triggered by an unknown owner fails; the previous snapshot is
    // kept, so the owner that *is* known stays addressable.
    let error = harness.router.request(attach(generation(7))).unwrap_err();
    assert!(matches!(error, ClientError::Protocol(_)));
    harness.router.request(attach(generation(1))).unwrap();
    assert_eq!(harness.router.endpoints().all().len(), 2);

    let mut cold = router_harness(vec![Err(DirectoryError::Corrupt("duplicate"))]);
    assert!(cold.router.request(control()).is_err());
    assert!(cold.router.refresh().is_err());
}

#[test]
fn a_transport_failure_drops_only_that_generation_connection() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    harness.router.request(attach(generation(1))).unwrap();
    harness.router.request(control()).unwrap();
    harness.replies.borrow_mut().insert(
        generation(1),
        vec![Err(ClientError::Unavailable("socket died".into()))],
    );
    let input = terminal_request(&TerminalRequest::Input {
        terminal: terminal(generation(1), 10),
        subscription: 1,
        input_seq: 1,
        input_operation: None,
        bytes: b"ls\n".to_vec(),
    });
    let before = harness.recorder.borrow().sent.len();
    assert!(harness.router.request(input).is_err());
    assert_eq!(
        harness.recorder.borrow().sent.len(),
        before + 1,
        "a lost input is never written a second time by this layer"
    );
    assert!(!harness.router.links().is_connected(generation(1)));
    assert!(
        harness.router.links().is_connected(generation(2)),
        "the active connection is untouched by a draining failure"
    );

    // A definitive protocol answer keeps the connection.
    harness.replies.borrow_mut().insert(
        generation(2),
        vec![Err(ClientError::Protocol(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "no",
        )))],
    );
    assert!(harness.router.request(control()).is_err());
    assert!(harness.router.links().is_connected(generation(2)));
}

#[test]
fn scope_inventory_merges_every_generation_and_survives_a_silent_owner() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    let old = terminal(generation(1), 10);
    let new = terminal(generation(2), 11);
    harness.replies.borrow_mut().insert(
        generation(1),
        vec![Ok(DaemonReply::Ok(
            json!({"terminals": [entry(old.clone(), true)]}),
        ))],
    );
    harness.replies.borrow_mut().insert(
        generation(2),
        vec![Ok(DaemonReply::Accepted {
            operation_id: "operation".into(),
            revision: 1,
            body: json!({"terminals": [entry(new.clone(), true)]}),
        })],
    );
    let merged = harness.router.inventory(&scope()).unwrap();
    assert_eq!(merged.entries().len(), 2);
    assert!(!merged.is_partial());
    assert_eq!(
        presence_of(&old, &merged, harness.router.endpoints()),
        OwnerPresence::Live
    );

    // The draining owner goes silent: its tab is kept, the active answer is not
    // turned into "the old terminal is gone".
    harness.replies.borrow_mut().insert(
        generation(1),
        vec![Err(ClientError::Unavailable("timed out".into()))],
    );
    harness.replies.borrow_mut().insert(
        generation(2),
        vec![Ok(DaemonReply::Ok(
            json!({"terminals": [entry(new.clone(), true)]}),
        ))],
    );
    let partial = harness.router.inventory(&scope()).unwrap();
    assert!(partial.is_partial());
    assert_eq!(
        presence_of(&old, &partial, harness.router.endpoints()),
        OwnerPresence::Reconnecting
    );
    assert_eq!(
        presence_of(&new, &partial, harness.router.endpoints()),
        OwnerPresence::Live
    );
}

#[test]
fn an_undecodable_inventory_answer_is_uncertainty_not_an_empty_scope() {
    let mut harness = router_harness(vec![Ok(two_generations())]);
    for body in [
        json!({}),
        json!({"terminals": "no"}),
        json!({"terminals": [1]}),
    ] {
        harness
            .replies
            .borrow_mut()
            .insert(generation(1), vec![Ok(DaemonReply::Ok(body))]);
        let merged = harness.router.inventory(&scope()).unwrap();
        assert_eq!(
            merged.unreachable().iter().copied().collect::<Vec<_>>(),
            vec![generation(1)]
        );
    }
}

#[test]
fn inventory_refuses_when_no_generation_is_addressable() {
    let mut harness = router_harness(vec![Ok(TrustedEndpoints::default())]);
    assert!(harness.router.inventory(&scope()).is_err());
}

#[test]
fn a_refused_owner_connection_leaves_the_request_unsent_and_the_scope_partial() {
    let recorder: Shared = Rc::new(RefCell::new(Recorder::default()));
    let replies = Rc::new(RefCell::new(BTreeMap::new()));
    let transport = FakeTransport {
        recorder: Rc::clone(&recorder),
        replies: Rc::clone(&replies),
        refuse: Some(generation(1)),
    };
    let mut router = OwnerRouter::new(FakeDirectory::of(vec![Ok(two_generations())]), transport);
    assert!(router.request(attach(generation(1))).is_err());
    assert!(recorder.borrow().sent.is_empty());
    let merged = router.inventory(&scope()).unwrap();
    assert_eq!(
        merged.unreachable().iter().copied().collect::<Vec<_>>(),
        vec![generation(1)]
    );
    assert!(merged.answered().contains(&generation(2)));

    router.links_mut().invalidate(generation(2));
    assert!(!router.links().is_connected(generation(2)));
}

#[test]
fn a_retired_generation_is_collected_by_the_next_refresh() {
    let mut harness = router_harness(vec![
        Ok(two_generations()),
        Ok(TrustedEndpoints::build(Some(generation(2)), vec![active(2)]).unwrap()),
    ]);
    harness.router.request(attach(generation(1))).unwrap();
    assert_eq!(harness.router.links().len(), 1);
    harness.router.refresh().unwrap();
    assert_eq!(harness.router.links().len(), 0);
    assert!(harness.router.endpoints().owner(generation(1)).is_none());
}

// -------------------------------------------------------------- route cache

/// The three registry states a client actually has to distinguish, resolved
/// through the cache the shipping client holds.
///
/// The single-generation row is the one every current build produces: control
/// work and owner-addressed work land on the same active endpoint, which is what
/// makes this routing a no-op until a rollover can publish a second generation.
#[test]
fn route_cache_resolves_control_owner_and_fan_out_for_every_registry_state() {
    // active only.
    let one = TrustedEndpoints::build(Some(generation(2)), vec![active(2)]).unwrap();
    let mut cache = RouteCache::new(FakeDirectory::of(vec![Ok(one)]));
    assert_eq!(
        cache.resolve(&RouteTarget::ActiveControl).unwrap(),
        RouteResolution::Single(active(2))
    );
    assert_eq!(cache.owner(generation(2)).unwrap(), active(2));
    assert_eq!(cache.every_generation().unwrap(), vec![active(2)]);

    // active + draining: control stays on the active endpoint while the owner of
    // an old terminal resolves to the draining one, and a scope query asks both.
    let mut cache = RouteCache::new(FakeDirectory::of(vec![Ok(two_generations())]));
    assert_eq!(
        cache.resolve(&RouteTarget::ActiveControl).unwrap(),
        RouteResolution::Single(active(2))
    );
    assert_eq!(cache.owner(generation(1)).unwrap(), draining(1));
    assert_eq!(
        cache.every_generation().unwrap(),
        vec![active(2), draining(1)]
    );

    // unknown owner: fail closed, never the active endpoint.
    assert_eq!(
        cache.owner(generation(7)).unwrap_err(),
        RoutingError::UnknownGeneration(generation(7))
    );
}

/// A retired owner is refused exactly like one that never existed: it is absent
/// from the trusted set, and absence is the verified retirement.
#[test]
fn route_cache_refuses_a_retired_owner_instead_of_the_active_endpoint() {
    let retired = TrustedEndpoints::build(Some(generation(2)), vec![active(2)]).unwrap();
    let mut cache = RouteCache::new(FakeDirectory::of(vec![Ok(two_generations()), Ok(retired)]));
    assert_eq!(cache.owner(generation(1)).unwrap(), draining(1));
    cache.invalidate();
    let error = cache.owner(generation(1)).unwrap_err();
    assert_eq!(error, RoutingError::UnknownGeneration(generation(1)));
    // The refusal is a stale target, not an unavailable transport: the reference
    // names something that no longer exists.
    assert_eq!(
        error.to_client_error().code(),
        crate::infrastructure::ipc::ErrorCode::StaleTarget
    );
    assert_eq!(cache.owner(generation(2)).unwrap(), active(2));
}

/// The registry is a file. Resolving from the cached snapshot must not read it,
/// or every IPC request would carry a directory traversal (#555).
#[test]
fn route_cache_reads_the_directory_once_until_it_has_a_reason_to_read_again() {
    let directory = FakeDirectory::of(vec![Ok(two_generations())]);
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut cache = RouteCache::new(CountingDirectory {
        inner: directory,
        reads: std::sync::Arc::clone(&reads),
    });
    assert!(!cache.is_loaded());
    for _ in 0..8 {
        cache.owner(generation(1)).unwrap();
        cache.owner(generation(2)).unwrap();
        cache.every_generation().unwrap();
        cache.resolve(&RouteTarget::ActiveControl).unwrap();
    }
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "resolution must not re-read the records"
    );
    assert!(cache.is_loaded());

    // An owner the snapshot cannot resolve is the one reason to look again: the
    // snapshot may predate the handoff that published it. One extra read, then
    // the refusal stands.
    assert!(cache.owner(generation(7)).is_err());
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);

    // Explicit invalidation is the caller reporting evidence the cache cannot
    // see for itself — a lane that stopped answering.
    cache.invalidate();
    cache.owner(generation(1)).unwrap();
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 3);
}

/// An unreadable directory keeps the previous snapshot and refuses, rather than
/// unaddressing a live owner or falling back to the endpoint used last.
#[test]
fn route_cache_keeps_the_last_snapshot_when_the_directory_becomes_unreadable() {
    let mut cache = RouteCache::new(FakeDirectory::of(vec![
        Ok(two_generations()),
        Err(DirectoryError::Unreadable("gone".into())),
    ]));
    assert_eq!(cache.owner(generation(1)).unwrap(), draining(1));
    // The unresolvable owner triggers the refresh, which now fails.
    assert_eq!(
        cache.owner(generation(7)).unwrap_err(),
        RoutingError::Directory(DirectoryError::Unreadable("gone".into()))
    );
    assert_eq!(cache.endpoints(), &two_generations());
}

/// Nothing published at all is `NoActiveGeneration`, and it stays effect zero:
/// an unavailable daemon is not a licence to route anywhere.
#[test]
fn route_cache_refuses_every_target_when_nothing_is_published() {
    let mut cache = RouteCache::new(FakeDirectory::of(vec![Ok(TrustedEndpoints::default())]));
    assert_eq!(
        cache.resolve(&RouteTarget::ActiveControl).unwrap_err(),
        RoutingError::NoActiveGeneration
    );
    assert_eq!(
        cache.every_generation().unwrap_err(),
        RoutingError::NoActiveGeneration
    );
    assert_eq!(
        cache.owner(generation(2)).unwrap_err(),
        RoutingError::UnknownGeneration(generation(2))
    );
}

struct CountingDirectory {
    inner: FakeDirectory,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl GenerationDirectory for CountingDirectory {
    fn snapshot(&self) -> Result<TrustedEndpoints, DirectoryError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.snapshot()
    }
}
