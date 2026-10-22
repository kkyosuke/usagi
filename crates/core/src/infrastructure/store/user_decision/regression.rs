use super::tests::{fixed_now, item};
use super::*;
use crate::domain::user_decision::{UserDecisionPolicy as Policy, UserDecisionSelectionMode};
use crate::infrastructure::ipc::{DEFAULT_MAX_FRAME_BYTES, UserDecisionCursor, write_json_frame};

fn max_answer() -> UserDecisionAnswer {
    UserDecisionAnswer::Freeform {
        text: "\u{1}".repeat(Policy::FREEFORM_ANSWER_MAX_BYTES),
    }
}

#[test]
fn admission_reserves_every_accepted_worst_case_answer_and_rejects_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let mut template = item();
    template.idempotency_key = None;
    template.allow_freeform = true;
    template.prompt = "p".repeat(Policy::PROMPT_MAX_BYTES);
    let mut accepted = Vec::new();
    loop {
        let mut next = template.clone();
        next.decision_id = UserDecisionId::new();
        let before = std::fs::read(store.path()).ok();
        let result = store.create(next.clone()).unwrap();
        if result == Err(UserDecisionError::CapacityReached) {
            assert_eq!(std::fs::read(store.path()).ok(), before);
            break;
        }
        accepted.push(result.unwrap());
    }
    assert!(accepted.len() > 1 && accepted.len() < PENDING_LIMIT);
    for decision in &accepted {
        let reopened = UserDecisionStore::new(dir.path());
        reopened
            .resolve(
                decision.owner.workspace_id,
                decision.decision_id,
                max_answer(),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert!(std::fs::metadata(store.path()).unwrap().len() <= MAX_SERIALIZED_BYTES as u64);
    }
    assert_eq!(store.events().unwrap().len(), accepted.len());
    assert!(store.pending_all().unwrap().is_empty());
}

fn rich(bytes: usize) -> UserDecision {
    let mut decision = item();
    decision.idempotency_key = None;
    decision.allow_freeform = true;
    decision.created_at = fixed_now();
    decision.expires_at = Some(fixed_now() + chrono::Duration::hours(24));
    let mut remaining = bytes;
    decision.prompt = "p".repeat(remaining.min(Policy::PROMPT_MAX_BYTES));
    remaining -= decision.prompt.len();
    decision.options = (0..32)
        .map(|index| {
            let len = remaining.min(Policy::OPTION_DESCRIPTION_MAX_BYTES);
            remaining -= len;
            crate::domain::user_decision::UserDecisionOption {
                id: format!("option-{index}"),
                label: "Option".into(),
                description: Some("d".repeat(len)),
                pros: vec!["p".repeat(512); 4],
                cons: vec!["c".repeat(512); 4],
            }
        })
        .collect();
    assert_eq!(remaining, 0);
    decision
}

#[test]
fn legacy_full_store_recovers_answers_cancellation_and_expiry_across_restart() {
    const PAYLOAD: usize = Policy::PROMPT_MAX_BYTES + 32 * Policy::OPTION_DESCRIPTION_MAX_BYTES;
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let first = rich(1);
    let mut state = State {
        decisions: vec![first.clone()],
        ..State::default()
    };
    let mut available = MAX_SERIALIZED_BYTES - serialized_len(&state).unwrap();
    let min = serde_json::to_vec_pretty(&State {
        decisions: vec![first.clone(), rich(1)],
        ..State::default()
    })
    .unwrap()
    .len()
        + 1
        - serialized_len(&state).unwrap();
    let max = min + PAYLOAD - 1;
    let count = available.div_ceil(max);
    assert!(available >= min * count);
    for remaining in (1..=count).rev() {
        let bytes = 1 + (available - min * remaining).min(PAYLOAD - 1);
        state.decisions.push(rich(bytes));
        available -= min + bytes - 1;
    }
    assert_eq!(available, 0);
    assert!(state.decisions.len() <= MIN_TERMINAL_RETENTION);
    json_file::write_atomic(dir.path(), &store.path(), &state).unwrap();
    assert_eq!(
        std::fs::metadata(store.path()).unwrap().len(),
        MAX_SERIALIZED_BYTES as u64
    );
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        store.create(item()).unwrap(),
        Err(UserDecisionError::CapacityReached)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    store
        .resolve(
            first.owner.workspace_id,
            first.decision_id,
            max_answer(),
            fixed_now(),
        )
        .unwrap()
        .unwrap();
    let reopened = UserDecisionStore::new(dir.path());
    assert!(reopened.load().unwrap().recovery_bytes > 0);
    let second = &state.decisions[1];
    reopened
        .terminal(
            second.owner.workspace_id,
            second.decision_id,
            UserDecisionStatus::Cancelled,
            fixed_now(),
        )
        .unwrap()
        .unwrap();
    let remaining = reopened
        .expire_due(fixed_now() + chrono::Duration::hours(25))
        .unwrap();
    assert_eq!(remaining.len(), state.decisions.len() - 2);
    assert_eq!(
        UserDecisionStore::new(dir.path()).events().unwrap().len(),
        1
    );
    assert!(reopened.pending_all().unwrap().is_empty());
    assert_eq!(
        reopened
            .get(first.owner.workspace_id, first.decision_id)
            .unwrap()
            .unwrap()
            .answer,
        Some(max_answer())
    );
    assert!(reopened.ack_event(first.decision_id).unwrap());
    let recovered = UserDecisionStore::new(dir.path());
    assert_eq!(recovered.load().unwrap().recovery_bytes, 0);
    recovered.create(item()).unwrap().unwrap();
    assert!(std::fs::metadata(store.path()).unwrap().len() <= MAX_SERIALIZED_BYTES as u64);
}

#[test]
fn recovery_headroom_includes_new_metadata_at_the_legacy_global_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let mut state = State::default();
    for _ in 0..GLOBAL_PENDING_LIMIT {
        let mut decision = item();
        decision.allow_freeform = true;
        decision.prompt = "p".into();
        state.decisions.push(decision);
    }
    let mut legacy = serde_json::json!(state);
    legacy.as_object_mut().unwrap().remove("recovery_bytes");
    let mut remaining =
        MAX_SERIALIZED_BYTES - serde_json::to_vec_pretty(&legacy).unwrap().len() - 1;
    for decision in legacy["decisions"].as_array_mut().unwrap() {
        let growth = remaining.min(Policy::PROMPT_MAX_BYTES - 1);
        decision["prompt"] = serde_json::json!("p".repeat(growth + 1));
        remaining -= growth;
    }
    assert_eq!(remaining, 0);
    json_file::write_atomic(dir.path(), &store.path(), &legacy).unwrap();
    assert_eq!(
        std::fs::metadata(store.path()).unwrap().len(),
        MAX_SERIALIZED_BYTES as u64
    );
    let recovered = store.load().unwrap();
    let hard_limit = capacity::recovery_limit(store.limits);
    assert!(recovered.recovery_bytes <= hard_limit - MAX_SERIALIZED_BYTES);
    assert!(capacity::reserved_len(&recovered).unwrap() <= hard_limit);
    assert_eq!(serde_json::json!(recovered.decisions), legacy["decisions"]);
    let first = &recovered.decisions[0];
    assert!(!store.ack_event(first.decision_id).unwrap());
    let reopened = UserDecisionStore::new(dir.path()).load().unwrap();
    assert_eq!(reopened.decisions, recovered.decisions);
    assert!(reopened.recovery_bytes <= hard_limit - MAX_SERIALIZED_BYTES);
}

#[test]
fn recent_answer_survives_reads_failed_delivery_and_explicit_ack_after_later_questions() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let first = item();
    store.create(first.clone()).unwrap().unwrap();
    for index in 0..TERMINAL_RETENTION {
        let mut later = item();
        later.idempotency_key = None;
        store.create(later.clone()).unwrap().unwrap();
        store
            .terminal(
                later.owner.workspace_id,
                later.decision_id,
                UserDecisionStatus::Cancelled,
                fixed_now() + chrono::Duration::seconds(i64::try_from(index).unwrap()),
            )
            .unwrap()
            .unwrap();
    }
    let expected = store
        .resolve(
            first.owner.workspace_id,
            first.decision_id,
            UserDecisionAnswer::Option {
                option_id: "a".into(),
                comment: None,
            },
            Utc::now(),
        )
        .unwrap()
        .unwrap();
    let before = std::fs::read(store.path()).unwrap();
    let reply = store
        .get(first.owner.workspace_id, first.decision_id)
        .unwrap()
        .unwrap();
    // The writer fails; get has not acknowledged or compacted the delivery.
    let mut buffer = [0_u8; 1];
    let mut closed = std::io::Cursor::new(&mut buffer[..]);
    assert!(write_json_frame(&mut closed, &reply, DEFAULT_MAX_FRAME_BYTES).is_err());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(
        UserDecisionStore::new(dir.path())
            .get(first.owner.workspace_id, first.decision_id)
            .unwrap(),
        Some(expected.clone())
    );
    assert!(store.ack_event(first.decision_id).unwrap());
    assert!(!store.ack_event(first.decision_id).unwrap());
    assert_eq!(
        store
            .get(first.owner.workspace_id, first.decision_id)
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn admission_bounds_escaped_record_and_legacy_pages_preserve_all_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    let mut large = rich(Policy::PROMPT_MAX_BYTES + 32 * Policy::OPTION_DESCRIPTION_MAX_BYTES);
    for option in &mut large.options {
        option.description = Some("\u{1}".repeat(2048));
        option.pros = vec!["\\".repeat(512); 4];
        option.cons = option.pros.clone();
    }
    large.context = vec![
        crate::domain::user_decision::UserDecisionContext::Table {
            title: "Data".into(),
            columns: vec!["\\".repeat(512); 6],
            rows: vec![vec!["\\".repeat(512); 6]; 16],
        };
        4
    ];
    large.validate_request().unwrap();
    assert!(wire::record_bytes(&large) > DEFAULT_MAX_FRAME_BYTES);
    assert_eq!(
        store.create(large.clone()).unwrap(),
        Err(UserDecisionError::InvalidRequest)
    );
    assert!(!store.path().exists());
    let small = item();
    let mut another = large.clone();
    another.decision_id = UserDecisionId::new();
    let state = State {
        decisions: vec![large.clone(), small.clone(), another.clone()],
        ..State::default()
    };
    json_file::write_atomic(dir.path(), &store.path(), &state).unwrap();
    let page = store
        .pending_page(UserDecisionScope::All, None, DEFAULT_MAX_FRAME_BYTES)
        .unwrap()
        .unwrap();
    assert_eq!(page.decisions, vec![small]);
    assert_eq!(
        page.deferred,
        vec![wire::reference(&large), wire::reference(&another)]
    );
    assert!(page.next_cursor.is_none());
    let mut cursor = None;
    let mut text = String::new();
    loop {
        let value = wire::response(&large, cursor.as_ref(), 128 * 1024).unwrap();
        assert!(serde_json::to_vec(&value).unwrap().len() <= 128 * 1024);
        let chunk: wire::DecisionChunk = serde_json::from_value(value).unwrap();
        assert_eq!(chunk.offset, text.len());
        text.push_str(&chunk.data);
        cursor = chunk.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(serde_json::from_str::<UserDecision>(&text).unwrap(), large);
    let reference = wire::reference(&large);
    for (cursor, error) in [
        (
            UserDecisionCursor {
                revision: "stale".into(),
                offset: 0,
            },
            UserDecisionPageError::RevisionConflict,
        ),
        (
            UserDecisionCursor {
                revision: reference.revision,
                offset: text.len(),
            },
            UserDecisionPageError::InvalidCursor,
        ),
    ] {
        assert_eq!(wire::response(&large, Some(&cursor), 4096), Err(error));
    }
    assert_eq!(
        wire::response(&large, None, 0),
        Err(UserDecisionPageError::PageTooSmall)
    );
    assert_eq!(
        wire::response(&large, None, text.len()).unwrap(),
        serde_json::json!(large)
    );
}

#[test]
fn reservation_covers_escaped_multiselect_and_comment() {
    let mut decision = rich(1);
    decision.allow_comment = true;
    decision.allow_freeform = false;
    decision.selection_mode = UserDecisionSelectionMode::Multiple;
    for (index, option) in decision.options.iter_mut().enumerate() {
        option.id = format!("{index:02}{}", "\u{1}".repeat(126));
    }
    let answer = UserDecisionAnswer::Options {
        option_ids: decision
            .options
            .iter()
            .map(|option| option.id.clone())
            .collect(),
        comment: Some("\\".repeat(Policy::COMMENT_MAX_BYTES)),
    };
    decision.validate_answer(&answer, fixed_now()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = UserDecisionStore::new(dir.path());
    store.create(decision.clone()).unwrap().unwrap();
    let reserved = capacity::reserved_len(&store.load().unwrap()).unwrap();
    store
        .resolve(
            decision.owner.workspace_id,
            decision.decision_id,
            answer,
            fixed_now(),
        )
        .unwrap()
        .unwrap();
    assert!(serialized_len(&store.load().unwrap()).unwrap() <= reserved);
}

#[test]
fn malformed_recovery_capacity_and_transaction_overflow_fail_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let limits = super::tests::small_limits(2048, 8);
    let store = UserDecisionStore::with_limits(dir.path(), limits);
    let mut state = State {
        decisions: vec![item()],
        recovery_bytes: 1,
        ..State::default()
    };
    json_file::write_atomic(dir.path(), &store.path(), &state).unwrap();
    assert!(
        store
            .load()
            .unwrap_err()
            .to_string()
            .contains("reserved capacity")
    );
    state.recovery_bytes = usize::MAX;
    json_file::write_atomic(dir.path(), &store.path(), &state).unwrap();
    assert!(store.load().unwrap_err().to_string().contains("hard limit"));
    state.recovery_bytes = 0;
    state.decisions[0].prompt = "p".repeat(4096);
    json_file::write_atomic(dir.path(), &store.path(), &state).unwrap();
    assert!(store.load().unwrap_err().to_string().contains("hard limit"));
    std::fs::remove_file(store.path()).unwrap();
    assert!(
        store
            .mutate(|state| {
                state.decisions.push(item());
            })
            .unwrap_err()
            .to_string()
            .contains("capacity")
    );
    assert_eq!(
        store
            .mutate_decision(|state| {
                state.decisions.push(item());
                Ok(())
            })
            .unwrap(),
        Err(UserDecisionError::CapacityReached)
    );
    assert!(!store.path().exists());
}

#[test]
fn chunks_respect_utf8_boundaries_and_reject_offsets_inside_characters() {
    let mut decision = item();
    decision.title = "日本語".into();
    let text = serde_json::to_string(&decision).unwrap();
    let reference = wire::reference(&decision);
    let cursor = UserDecisionCursor {
        revision: reference.revision.clone(),
        offset: 0,
    };
    let overhead = serde_json::to_vec(&wire::DecisionChunk {
        decision_id: decision.decision_id,
        revision: reference.revision.clone(),
        offset: 0,
        data: String::new(),
        next_cursor: Some(UserDecisionCursor {
            revision: reference.revision,
            offset: text.len(),
        }),
    })
    .unwrap()
    .len();
    let japanese = text.find('日').unwrap();
    let response = wire::response(&decision, Some(&cursor), overhead + (japanese + 1) * 6).unwrap();
    let chunk: wire::DecisionChunk = serde_json::from_value(response).unwrap();
    assert_eq!(chunk.data.len(), japanese);
    assert_eq!(
        wire::response(
            &decision,
            Some(&UserDecisionCursor {
                offset: japanese + 1,
                ..cursor
            }),
            4096
        ),
        Err(UserDecisionPageError::InvalidCursor)
    );
}

#[test]
fn completed_history_cannot_permanently_block_new_questions() {
    let dir = tempfile::tempdir().unwrap();
    let mut decision = rich(Policy::PROMPT_MAX_BYTES + 32 * Policy::OPTION_DESCRIPTION_MAX_BYTES);
    decision.idempotency_key = Some("first".into());
    let budget = capacity::reserved_len(&State {
        decisions: vec![decision.clone()],
        ..State::default()
    })
    .unwrap();
    let limits = StoreLimits {
        max_serialized_bytes: budget,
        ..StoreLimits::default()
    };
    let store = UserDecisionStore::with_limits(dir.path(), limits);
    store.create(decision.clone()).unwrap().unwrap();
    store
        .resolve(
            decision.owner.workspace_id,
            decision.decision_id,
            max_answer(),
            fixed_now(),
        )
        .unwrap()
        .unwrap();
    let mut next = decision.clone();
    next.decision_id = UserDecisionId::new();
    next.idempotency_key = Some("later".into());
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        store.create(next.clone()).unwrap(),
        Err(UserDecisionError::CapacityReached)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(store.events().unwrap().len(), 1);
    store.ack_event(decision.decision_id).unwrap();
    assert!(store.events().unwrap().is_empty());
    assert!(store.pending_all().unwrap().is_empty());
    let admitted = store.create(next.clone()).unwrap();
    assert_eq!(admitted.unwrap().decision_id, next.decision_id);
    assert_eq!(
        store.create(decision).unwrap(),
        Err(UserDecisionError::IdempotencyExpired)
    );
}
