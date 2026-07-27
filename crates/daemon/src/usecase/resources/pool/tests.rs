use std::sync::Arc;

use usagi_core::domain::id::{DaemonGeneration, OperationId};

use crate::usecase::resources::allocator::{ResourceAllocator, ResourceKind};
use crate::usecase::resources::fixture::{FileFault, MemoryFile, SharedBytes, policy, terminal};
use crate::usecase::resources::pool::{ForeignOccupancy, SharedPool, foreign_occupancy};

/// One claim of `kind`, owned by `owner`, committed into `bytes`.
fn claim(bytes: &SharedBytes, owner: DaemonGeneration, kind: ResourceKind) {
    let allocator = ResourceAllocator::new(MemoryFile::new(bytes), policy(4, 4));
    let resource = terminal(owner);
    let operation = OperationId::new();
    allocator
        .update(|document| {
            document.reserve(&operation, "digest", kind, owner, &resource, policy(4, 4))?;
            Ok(())
        })
        .expect("a healthy allocator accepts a fresh claim");
}

#[test]
fn a_process_without_a_shared_pool_adds_nothing_to_its_own_count() {
    assert_eq!(foreign_occupancy(None, 8), 0);
}

#[test]
fn a_shared_pool_counts_other_generations_and_excludes_the_owner_and_other_kinds() {
    let bytes = SharedBytes::default();
    let mine = DaemonGeneration::new();
    let theirs = DaemonGeneration::new();
    claim(&bytes, mine, ResourceKind::Terminal);
    claim(&bytes, theirs, ResourceKind::Terminal);
    claim(&bytes, theirs, ResourceKind::Terminal);
    claim(&bytes, theirs, ResourceKind::Agent);

    let pool = SharedPool::new(
        Arc::new(ResourceAllocator::new(
            MemoryFile::new(&bytes),
            policy(4, 4),
        )),
        mine,
        ResourceKind::Terminal,
    );
    assert_eq!(pool.occupied(), Some(2));
    assert_eq!(foreign_occupancy(Some(&pool), 4), 2);
    assert!(format!("{pool:?}").contains("terminal"));
}

#[test]
fn an_unreadable_pool_is_treated_as_full_rather_than_as_empty() {
    let bytes = SharedBytes::default();
    let pool = SharedPool::new(
        Arc::new(ResourceAllocator::new(
            MemoryFile::faulty(&bytes, FileFault::ReadFails),
            policy(4, 4),
        )),
        DaemonGeneration::new(),
        ResourceKind::Agent,
    );
    assert_eq!(pool.occupied(), None);
    assert_eq!(
        foreign_occupancy(Some(&pool), 3),
        3,
        "a pool that cannot be read must refuse, never admit"
    );
}
