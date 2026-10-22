//! Assemble a complete observation before replacing the controller's inventory.

use std::collections::BTreeSet;

use usagi_core::domain::user_decision::UserDecision;
use usagi_core::infrastructure::ipc::{
    DaemonReply, DaemonRequest, TuiUserDecisionAction, UserDecisionCursor, UserDecisionPage,
};

// Bound work for faulty peers. The byte-bounded durable inbox (including its
// legacy recovery reserve) fits comfortably within 256 normal wire pages.
const MAX_PAGES: usize = 256;
use usagi_core::infrastructure::store::user_decision::wire::{
    DecisionChunk, DecisionReference, MAX_RECOVERY_RECORD_BYTES, revision,
};

fn read_deferred(
    request: &mut dyn FnMut(DaemonRequest) -> Result<DaemonReply, String>,
    reference: &DecisionReference,
) -> Result<UserDecision, String> {
    let invalid = || "daemon returned an invalid decision fragment".to_owned();
    let mut text = String::new();
    for _ in 0..128 {
        let DaemonReply::Ok(value) = request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::Get,
            payload: serde_json::json!({"workspace": reference.workspace, "decision_id": reference.decision_id,
                "cursor": {"revision": reference.revision, "offset": text.len()}}),
        })?
        else {
            return Err(invalid());
        };
        let chunk: DecisionChunk = serde_json::from_value(value).map_err(|_| invalid())?;
        if chunk.decision_id != reference.decision_id
            || chunk.revision != reference.revision
            || chunk.offset != text.len()
            || chunk.data.is_empty()
            || text.len() + chunk.data.len() > MAX_RECOVERY_RECORD_BYTES
        {
            return Err(invalid());
        }
        text.push_str(&chunk.data);
        if let Some(next) = chunk.next_cursor {
            if next.revision != reference.revision || next.offset != text.len() {
                return Err(invalid());
            }
        } else {
            let decision: UserDecision = serde_json::from_str(&text).map_err(|_| invalid())?;
            if revision(&text) != reference.revision
                || decision.decision_id != reference.decision_id
                || decision.owner.workspace_id != reference.workspace
            {
                return Err(invalid());
            }
            return Ok(decision);
        }
    }
    Err("daemon exceeded the decision fragment limit".to_owned())
}

pub(super) fn inbox_snapshot(
    request: &mut dyn FnMut(DaemonRequest) -> Result<DaemonReply, String>,
) -> Result<Vec<UserDecision>, String> {
    let mut cursor: Option<UserDecisionCursor> = None;
    let mut decisions = Vec::new();
    let mut seen = BTreeSet::new();
    for _ in 0..MAX_PAGES {
        let reply = request(DaemonRequest::UserDecision {
            action: TuiUserDecisionAction::ListAll,
            payload: serde_json::json!({ "cursor": cursor }),
        })?;
        let DaemonReply::Ok(value) = reply else {
            return Err("daemon did not return a decision page".to_owned());
        };
        let mut page: UserDecisionPage = serde_json::from_value(value)
            .map_err(|_| "daemon returned an invalid decision page".to_owned())?;
        for reference in &page.deferred {
            page.decisions.push(read_deferred(request, reference)?);
        }
        if page
            .decisions
            .iter()
            .any(|decision| !seen.insert(decision.decision_id))
        {
            return Err("daemon repeated a decision across pages".to_owned());
        }
        let empty = page.decisions.is_empty();
        decisions.extend(page.decisions);
        let Some(next) = page.next_cursor else {
            return Ok(decisions);
        };
        if empty
            || next.offset != decisions.len()
            || next.revision.is_empty()
            || cursor
                .as_ref()
                .is_some_and(|previous| previous.revision != next.revision)
        {
            return Err("daemon returned an invalid decision continuation".to_owned());
        }
        cursor = Some(next);
    }
    Err("daemon exceeded the decision page limit".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use usagi_core::domain::id::{AgentId, OperationId, UserDecisionId, WorkspaceId};

    fn decision() -> UserDecision {
        serde_json::from_value(json!({
            "decision_id": UserDecisionId::new(),
            "owner": {
                "workspace_id": WorkspaceId::new(), "session_id": null,
                "caller": { "session_id": null, "agent_id": AgentId::new() },
                "run_id": OperationId::new()
            },
            "title": "Choose", "prompt": "Which option?", "options": [],
            "allow_freeform": true, "expires_at": null, "idempotency_key": null,
            "status": "pending", "answer": null,
            "created_at": "2026-01-01T00:00:00Z", "resolved_at": null
        }))
        .unwrap()
    }

    fn cursor(revision: &str, offset: usize) -> UserDecisionCursor {
        UserDecisionCursor {
            revision: revision.to_owned(),
            offset,
        }
    }

    fn page(decisions: Vec<UserDecision>, next_cursor: Option<UserDecisionCursor>) -> DaemonReply {
        DaemonReply::Ok(
            serde_json::to_value(UserDecisionPage {
                deferred: Vec::new(),
                decisions,
                next_cursor,
            })
            .unwrap(),
        )
    }

    #[test]
    fn gathers_all_pages_before_publishing_and_passes_exact_cursors() {
        let expected = vec![decision(), decision(), decision()];
        let first = cursor("revision-a", 1);
        let second = cursor("revision-a", 2);
        let mut replies = [
            page(expected[..1].to_vec(), Some(first.clone())),
            page(expected[1..2].to_vec(), Some(second.clone())),
            page(expected[2..].to_vec(), None),
        ]
        .into_iter();
        let mut requests = Vec::new();
        let actual = inbox_snapshot(&mut |request| {
            requests.push(request);
            Ok(replies.next().unwrap())
        })
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(requests.len(), 3);
        for (request, expected) in requests.into_iter().zip([None, Some(first), Some(second)]) {
            assert_eq!(
                request,
                DaemonRequest::UserDecision {
                    action: TuiUserDecisionAction::ListAll,
                    payload: json!({ "cursor": expected }),
                }
            );
        }
    }

    #[test]
    fn empty_inventory_is_a_complete_snapshot() {
        assert!(
            inbox_snapshot(&mut |_| Ok(page(Vec::new(), None)))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn discards_partial_inventory_on_errors_or_invalid_continuations() {
        let first = decision();
        let second = decision();
        let failures = [
            Err("RevisionConflict".to_owned()),
            Ok(DaemonReply::Accepted {
                operation_id: "not-a-snapshot".to_owned(),
                revision: 1,
                body: json!({}),
            }),
            Ok(DaemonReply::Ok(json!({ "decisions": "invalid" }))),
            Ok(page(vec![first.clone()], None)),
            Ok(page(Vec::new(), Some(cursor("a", 1)))),
            Ok(page(vec![second.clone()], Some(cursor("a", 1)))),
            Ok(page(vec![second.clone()], Some(cursor("", 2)))),
            Ok(page(vec![second.clone()], Some(cursor("b", 2)))),
            Ok(page(vec![second.clone(), second], None)),
        ];
        for failure in failures {
            let mut replies =
                [Ok(page(vec![first.clone()], Some(cursor("a", 1)))), failure].into_iter();
            assert!(inbox_snapshot(&mut |_| replies.next().unwrap()).is_err());
        }
    }

    #[test]
    fn bounds_a_peer_that_never_finishes_the_inventory() {
        let mut calls = 0;
        let result = inbox_snapshot(&mut |_| {
            calls += 1;
            Ok(page(vec![decision()], Some(cursor("a", calls))))
        });
        assert!(result.unwrap_err().contains("page limit"));
        assert_eq!(calls, MAX_PAGES);
    }
    #[test]
    fn deferred_inventory_restores_full_records_before_publishing() {
        for resolved in [false, true] {
            let mut expected = decision();
            if resolved {
                expected.status = usagi_core::domain::user_decision::UserDecisionStatus::Resolved;
                expected.answer = Some(
                    usagi_core::domain::user_decision::UserDecisionAnswer::Freeform {
                        text: "Received answer".into(),
                    },
                );
            }
            let reference =
                usagi_core::infrastructure::store::user_decision::wire::reference(&expected);
            let mut reads = 0;
            let actual = inbox_snapshot(&mut |request| {
                let request = json!(request);
                assert_eq!(request["kind"], "user_decision");
                if request["action"] == "list_all" {
                    return Ok(DaemonReply::Ok(
                        json!({"decisions": [], "deferred": [reference], "next_cursor":null}),
                    ));
                }
                reads += 1;
                assert_eq!(request["action"], "get");
                let payload = &request["payload"];
                assert_eq!(payload["workspace"], json!(expected.owner.workspace_id));
                let cursor: UserDecisionCursor =
                    serde_json::from_value(payload["cursor"].clone()).unwrap();
                Ok(DaemonReply::Ok(
                    usagi_core::infrastructure::store::user_decision::wire::response(
                        &expected,
                        Some(&cursor),
                        2400,
                    )
                    .unwrap(),
                ))
            })
            .unwrap();
            assert_eq!(actual, vec![expected]);
            assert!(reads > 1);
        }
    }

    #[test]
    fn rejects_incomplete_changed_malformed_and_unbounded_fragments() {
        let expected = decision();
        let reference =
            usagi_core::infrastructure::store::user_decision::wire::reference(&expected);
        let text = serde_json::to_string(&expected).unwrap();
        let valid = DecisionChunk {
            decision_id: expected.decision_id,
            revision: reference.revision.clone(),
            offset: 0,
            data: text.clone(),
            next_cursor: None,
        };
        let failures = [
            Err("disconnected".into()),
            Ok(DaemonReply::Accepted {
                operation_id: "x".into(),
                revision: 1,
                body: json!({}),
            }),
            Ok(DaemonReply::Ok(json!({}))),
            Ok(DaemonReply::Ok(json!(DecisionChunk {
                offset: 1,
                ..valid.clone()
            }))),
            Ok(DaemonReply::Ok(json!(DecisionChunk {
                data: "x".repeat(MAX_RECOVERY_RECORD_BYTES + 1),
                ..valid.clone()
            }))),
            Ok(DaemonReply::Ok(json!(DecisionChunk {
                data: "{".into(),
                ..valid.clone()
            }))),
            Ok(DaemonReply::Ok(json!(DecisionChunk {
                data: format!("{text} "),
                ..valid.clone()
            }))),
            Ok(DaemonReply::Ok(json!(DecisionChunk {
                next_cursor: Some(cursor("wrong", text.len())),
                ..valid
            }))),
        ];
        for failure in failures {
            assert!(read_deferred(&mut |_| failure.clone(), &reference).is_err());
        }
        let mut calls = 0;
        assert!(
            read_deferred(
                &mut |_| {
                    let chunk = DecisionChunk {
                        decision_id: expected.decision_id,
                        revision: reference.revision.clone(),
                        offset: calls,
                        data: "x".into(),
                        next_cursor: Some(cursor(&reference.revision, calls + 1)),
                    };
                    calls += 1;
                    Ok(DaemonReply::Ok(json!(chunk)))
                },
                &reference
            )
            .unwrap_err()
            .contains("fragment limit")
        );
        assert_eq!(calls, 128);
    }
}
