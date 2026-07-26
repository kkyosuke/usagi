//! Legacy records become shard state only when they can be proved.

use usagi_core::domain::id::{DaemonGeneration, OperationId};

use super::{
    AdoptionRefusal, LegacyRuntimeRecord, REAL_CHILD_IDENTITY_CAPABILITY, RolloverAdmission,
    RolloverRefusal, SHARDED_STORE_CAPABILITY, adopt_legacy, planned_rollover_admission,
};
use crate::usecase::resources::CasDocument;
use crate::usecase::resources::allocator::ResourceKind;
use crate::usecase::resources::fixture::{terminal, verified};
use crate::usecase::resources::identity::ChildIdentity;
use crate::usecase::resources::shard::ResourceState;

fn record(owner: DaemonGeneration, live: bool) -> LegacyRuntimeRecord {
    LegacyRuntimeRecord {
        resource: terminal(owner),
        kind: ResourceKind::Terminal,
        operation: Some(OperationId::new()),
        digest: Some("digest".to_owned()),
        process: Some(verified(101, "os:101")),
        live,
    }
}

#[test]
fn a_provable_live_record_becomes_a_running_shard_resource() {
    let owner = DaemonGeneration::new();
    let adoptable = record(owner, true);
    let report = adopt_legacy(owner, std::slice::from_ref(&adoptable));
    assert!(report.unknown.is_empty());
    assert_eq!(report.adopted(), 1);
    assert_eq!(
        report.shard.resource(&adoptable.resource).unwrap().state,
        ResourceState::Running
    );
    report.shard.validate().unwrap();
}

#[test]
fn an_unprovable_record_is_kept_unknown_and_never_counted_as_live() {
    let owner = DaemonGeneration::new();
    let fixed_identity = LegacyRuntimeRecord {
        process: Some(ChildIdentity::unverifiable(102, "start")),
        ..record(owner, true)
    };
    let no_identity = LegacyRuntimeRecord {
        process: None,
        ..record(owner, true)
    };
    let no_operation = LegacyRuntimeRecord {
        operation: None,
        digest: None,
        ..record(owner, true)
    };
    let terminated = record(owner, false);
    let records = vec![
        fixed_identity.clone(),
        no_identity.clone(),
        no_operation.clone(),
        terminated.clone(),
    ];

    let report = adopt_legacy(owner, &records);
    assert_eq!(report.adopted(), 0, "nothing unprovable is live");
    assert_eq!(report.shard.resources.len(), 4);
    for record in [&fixed_identity, &no_identity, &no_operation, &terminated] {
        assert_eq!(
            report.shard.resource(&record.resource).unwrap().state,
            ResourceState::OwnershipUnknown
        );
    }
    let refusals: Vec<AdoptionRefusal> = report
        .unknown
        .iter()
        .map(|unknown| unknown.refusal)
        .collect();
    assert_eq!(
        refusals,
        vec![
            AdoptionRefusal::UnverifiableIdentity,
            AdoptionRefusal::UnverifiableIdentity,
            AdoptionRefusal::NoOperation,
        ],
        "a terminated record is simply not live; it is not a refusal"
    );
    report.shard.validate().unwrap();
}

#[test]
fn foreign_and_duplicated_records_are_left_out_of_the_shard_entirely() {
    let owner = DaemonGeneration::new();
    let foreign = record(DaemonGeneration::new(), true);
    let mine = record(owner, true);
    let duplicate = LegacyRuntimeRecord {
        resource: mine.resource.clone(),
        ..record(owner, true)
    };

    let report = adopt_legacy(owner, &[foreign.clone(), mine.clone(), duplicate]);
    assert_eq!(report.shard.resources.len(), 1);
    assert_eq!(report.adopted(), 1);
    assert_eq!(
        report
            .unknown
            .iter()
            .map(|unknown| unknown.refusal)
            .collect::<Vec<_>>(),
        vec![
            AdoptionRefusal::ForeignGeneration,
            AdoptionRefusal::Duplicate
        ]
    );
    assert_eq!(report.unknown[0].resource, foreign.resource);
    report.shard.validate().unwrap();
}

#[test]
fn a_predecessor_without_both_capabilities_cannot_hand_over_while_it_lives() {
    let both = vec![
        REAL_CHILD_IDENTITY_CAPABILITY.to_owned(),
        SHARDED_STORE_CAPABILITY.to_owned(),
    ];
    assert_eq!(
        planned_rollover_admission(&both),
        RolloverAdmission::Allowed
    );
    assert_eq!(
        planned_rollover_admission(&[SHARDED_STORE_CAPABILITY.to_owned()]),
        RolloverAdmission::ColdTransitionRequired(RolloverRefusal::NoRealChildIdentity)
    );
    assert_eq!(
        planned_rollover_admission(&[REAL_CHILD_IDENTITY_CAPABILITY.to_owned()]),
        RolloverAdmission::ColdTransitionRequired(RolloverRefusal::NoShardedStore)
    );
    assert_eq!(
        planned_rollover_admission(&[]),
        RolloverAdmission::ColdTransitionRequired(RolloverRefusal::NoRealChildIdentity)
    );
}
