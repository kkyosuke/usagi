//! Reconnect discovery and explicit human cleanup regression contracts.
use super::tests::{item, small_limits};
use super::*;
use crate::infrastructure::ipc::DEFAULT_MAX_FRAME_BYTES;

fn answer(store: &UserDecisionStore, decision: &UserDecision) {
    store
        .resolve(
            decision.owner.workspace_id,
            decision.decision_id,
            UserDecisionAnswer::Option {
                option_id: "a".into(),
                comment: None,
            },
            Utc::now(),
        )
        .unwrap()
        .unwrap();
}

#[test]
fn lost_creation_reply_recovers_the_answer_without_a_key_or_known_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let mut request = item();
    request.idempotency_key = None;
    // The create reply never reaches the caller; only its authenticated owner
    // is retained across reconnect. The human still sees the committed question.
    store
        .create_with_default_expiry(request.clone())
        .unwrap()
        .unwrap();
    answer(&store, &store.pending_all().unwrap()[0]);
    let unrelated = item();
    store.create(unrelated.clone()).unwrap().unwrap();
    answer(&store, &unrelated);
    let pending = item();
    store.create(pending.clone()).unwrap().unwrap();
    drop(store);

    let store = UserDecisionStore::new(dir.path());
    let before = std::fs::read(store.path()).unwrap();
    let owners = [pending.owner.clone(), request.owner.clone()];
    let page = store
        .inbox_page(
            UserDecisionScope::Owners(&owners),
            None,
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
        .unwrap();
    assert_eq!(page.decisions.len(), 2);
    let recovered = page
        .decisions
        .iter()
        .find(|d| d.status == UserDecisionStatus::Resolved)
        .unwrap();
    assert_eq!(recovered.decision_id, request.decision_id);
    assert_eq!(
        recovered.answer,
        Some(UserDecisionAnswer::Option {
            option_id: "a".into(),
            comment: None
        })
    );
    assert_eq!(
        std::fs::read(store.path()).unwrap(),
        before,
        "discovery is read only"
    );
    assert_eq!(store.events().unwrap().len(), 2);
    assert!(
        store
            .pending_page(
                UserDecisionScope::Owners(&[request.owner.clone()]),
                None,
                DEFAULT_MAX_FRAME_BYTES
            )
            .unwrap()
            .unwrap()
            .decisions
            .is_empty()
    );
    assert!(
        store
            .inbox_page(
                UserDecisionScope::Owners(&[]),
                None,
                DEFAULT_MAX_FRAME_BYTES
            )
            .unwrap()
            .unwrap()
            .decisions
            .is_empty()
    );
    assert!(store.ack_event(recovered.decision_id).unwrap());
    let remaining = store
        .inbox_page(
            UserDecisionScope::Owners(&owners),
            None,
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
        .unwrap();
    assert_eq!(remaining.decisions, vec![pending]);
    assert_eq!(
        store
            .inbox_page(
                UserDecisionScope::Workspace(unrelated.owner.workspace_id),
                None,
                DEFAULT_MAX_FRAME_BYTES
            )
            .unwrap()
            .unwrap()
            .decisions
            .len(),
        1
    );
}

#[test]
fn discard_is_workspace_scoped_terminal_only_and_preserves_key_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let request = store.create(item()).unwrap().unwrap();
    let workspace = request.owner.workspace_id;
    for id in [request.decision_id, UserDecisionId::new()] {
        let before = std::fs::read(store.path()).unwrap();
        assert_eq!(
            store.discard_answer(workspace, id).unwrap(),
            Err(UserDecisionError::Terminal)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }
    answer(&store, &request);
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        store
            .discard_answer(WorkspaceId::new(), request.decision_id)
            .unwrap(),
        Err(UserDecisionError::Terminal)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    store
        .discard_answer(workspace, request.decision_id)
        .unwrap()
        .unwrap();
    let store = UserDecisionStore::new(dir.path());
    assert!(store.get(workspace, request.decision_id).unwrap().is_none());
    assert!(store.events().unwrap().is_empty());
    assert_eq!(
        store.create(request.clone()).unwrap(),
        Err(UserDecisionError::IdempotencyExpired)
    );
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        store
            .discard_answer(workspace, request.decision_id)
            .unwrap(),
        Err(UserDecisionError::Terminal)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);

    let acked = store.create(item()).unwrap().unwrap();
    answer(&store, &acked);
    store.ack_event(acked.decision_id).unwrap();
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        store
            .discard_answer(acked.owner.workspace_id, acked.decision_id)
            .unwrap(),
        Err(UserDecisionError::Terminal)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
}

#[test]
fn abandoned_answers_can_be_reviewed_and_discarded_to_recover_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let limits = small_limits(64 * 1024, 8);
    let store = UserDecisionStore::with_limits(dir.path(), limits);
    let mut admitted = Vec::new();
    for bytes in [6000, 1] {
        loop {
            let mut request = item();
            request.idempotency_key = None;
            request.prompt = "p".repeat(bytes);
            match store.create(request).unwrap() {
                Ok(decision) => {
                    answer(&store, &decision);
                    admitted.push(decision);
                    assert!(admitted.len() < 128);
                }
                Err(error) => {
                    assert_eq!(error, UserDecisionError::CapacityReached);
                    break;
                }
            }
        }
    }
    assert!(store.pending_all().unwrap().is_empty());
    assert!(
        store
            .expire_due(Utc::now() + chrono::Duration::days(30))
            .unwrap()
            .is_empty()
    );
    let mut fresh = item();
    fresh.idempotency_key = None;
    assert_eq!(
        store.create(fresh.clone()).unwrap(),
        Err(UserDecisionError::CapacityReached)
    );
    let first = &admitted[0];
    assert_eq!(
        store
            .inbox_page(UserDecisionScope::All, None, DEFAULT_MAX_FRAME_BYTES)
            .unwrap()
            .unwrap()
            .decisions
            .len(),
        admitted.len()
    );
    store
        .discard_answer(first.owner.workspace_id, first.decision_id)
        .unwrap()
        .unwrap();
    drop(store);
    let reopened = UserDecisionStore::with_limits(dir.path(), limits);
    assert_eq!(reopened.events().unwrap().len(), admitted.len() - 1);
    assert!(
        reopened
            .get(first.owner.workspace_id, first.decision_id)
            .unwrap()
            .is_none()
    );
    reopened.create(fresh).unwrap().unwrap();
}

#[test]
fn inbox_cursors_detect_ack_and_discard_but_ignore_foreign_owners() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let first = store.create(item()).unwrap().unwrap();
    answer(&store, &first);
    let mut second = first.clone();
    second.decision_id = UserDecisionId::new();
    second.idempotency_key = None;
    second.prompt = "p".repeat(4096);
    store.create(second.clone()).unwrap().unwrap();
    answer(&store, &second);
    let owners = [first.owner.clone()];
    let scope = UserDecisionScope::Owners(&owners);
    let cursor = store
        .inbox_page(scope, None, 2048)
        .unwrap()
        .unwrap()
        .next_cursor
        .unwrap();
    let foreign = store.create(item()).unwrap().unwrap();
    answer(&store, &foreign);
    store
        .discard_answer(foreign.owner.workspace_id, foreign.decision_id)
        .unwrap()
        .unwrap();
    assert!(
        store
            .inbox_page(scope, Some(&cursor), DEFAULT_MAX_FRAME_BYTES)
            .unwrap()
            .is_ok()
    );
    store.ack_event(first.decision_id).unwrap();
    assert_eq!(
        store
            .inbox_page(scope, Some(&cursor), DEFAULT_MAX_FRAME_BYTES)
            .unwrap(),
        Err(UserDecisionPageError::RevisionConflict)
    );
    store
        .discard_answer(second.owner.workspace_id, second.decision_id)
        .unwrap()
        .unwrap();
    assert!(
        store
            .inbox_page(scope, None, DEFAULT_MAX_FRAME_BYTES)
            .unwrap()
            .unwrap()
            .decisions
            .is_empty()
    );
}
