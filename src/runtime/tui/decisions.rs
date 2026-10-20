//! Assemble a complete observation before replacing the controller's inventory.

use std::collections::BTreeSet;

use usagi_core::domain::user_decision::UserDecision;
use usagi_core::infrastructure::ipc::{
    DaemonReply, DaemonRequest, TuiUserDecisionAction, UserDecisionCursor, UserDecisionPage,
};

// The durable store permits at most 256 pending decisions. Every continuation
// must advance by at least one, so this also bounds a faulty peer's work.
const MAX_PAGES: usize = 256;

pub(super) fn pending_snapshot(
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
        let page: UserDecisionPage = serde_json::from_value(value)
            .map_err(|_| "daemon returned an invalid decision page".to_owned())?;
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
        let actual = pending_snapshot(&mut |request| {
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
            pending_snapshot(&mut |_| Ok(page(Vec::new(), None)))
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
            assert!(pending_snapshot(&mut |_| replies.next().unwrap()).is_err());
        }
    }

    #[test]
    fn bounds_a_peer_that_never_finishes_the_inventory() {
        let mut calls = 0;
        let result = pending_snapshot(&mut |_| {
            calls += 1;
            Ok(page(vec![decision()], Some(cursor("a", calls))))
        });
        assert!(result.unwrap_err().contains("page limit"));
        assert_eq!(calls, MAX_PAGES);
    }
}
