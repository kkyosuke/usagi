//! Scoped decision pages sized against their actual negotiated response envelope.

use serde::Deserialize;
use serde_json::{Value, json};
use usagi_core::{
    domain::id::WorkspaceId,
    infrastructure::{
        ipc::{
            DEFAULT_MAX_FRAME_BYTES, ErrorCode, ProtocolError, RequestId, ResponseOutcome,
            ServerHello, UserDecisionCursor, UserDecisionPage,
        },
        store::user_decision::{UserDecisionPageError, UserDecisionScope, UserDecisionStore},
    },
};

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    cursor: Option<UserDecisionCursor>,
}

pub(super) fn response(
    store: &UserDecisionStore,
    hello: &ServerHello,
    request_id: &RequestId,
    workspace: WorkspaceId,
    scope: UserDecisionScope<'_>,
    payload: Value,
) -> Result<Value, ProtocolError> {
    let payload: Payload = serde_json::from_value(payload).map_err(|_| {
        ProtocolError::new(ErrorCode::InvalidArgument, "invalid decision page cursor")
    })?;
    let empty = UserDecisionPage {
        deferred: Vec::new(),
        decisions: Vec::new(),
        next_cursor: None,
    };
    let empty_reply = super::envelope(
        hello,
        request_id.clone(),
        ResponseOutcome::Ok,
        page_body(workspace, &empty),
    );
    // Both the workspace wrapper and envelope are constant for this response.
    // Measuring them also accounts for escaped request IDs and future fields.
    let overhead = serde_json::to_vec(&empty_reply)
        .expect("a response envelope contains only JSON values")
        .len()
        - serde_json::to_vec(&empty)
            .expect("a decision page contains only JSON values")
            .len();
    let budget = (hello.limits.max_frame_bytes as usize)
        .min(DEFAULT_MAX_FRAME_BYTES)
        .saturating_sub(overhead);
    let page = store
        .pending_page(scope, payload.cursor.as_ref(), budget)
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "decision inventory is unavailable")
        })?
        .map_err(page_error)?;
    Ok(page_body(workspace, &page))
}

fn page_body(workspace: WorkspaceId, page: &UserDecisionPage) -> Value {
    let mut value = json!(page);
    value["workspace"] = json!(workspace);
    value
}

pub(super) fn page_error(error: UserDecisionPageError) -> ProtocolError {
    let (code, message) = match error {
        UserDecisionPageError::RevisionConflict => (
            ErrorCode::RevisionConflict,
            "decision inventory changed; restart pagination",
        ),
        UserDecisionPageError::InvalidCursor => {
            (ErrorCode::InvalidArgument, "invalid decision page cursor")
        }
        UserDecisionPageError::PageTooSmall => (
            ErrorCode::ResourceExhausted,
            "a decision does not fit the negotiated frame limit",
        ),
    };
    ProtocolError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::{
        domain::{
            agent::CallerRef,
            id::{AgentId, OperationId, UserDecisionId},
            user_decision::{
                UserDecision, UserDecisionOwner, UserDecisionSelectionMode, UserDecisionStatus,
            },
        },
        infrastructure::ipc::{
            BuildIdentity, ConnectionId, DaemonGeneration, GenerationRole, ProtocolLimits,
            ProtocolVersion,
        },
    };

    fn hello() -> ServerHello {
        ServerHello {
            connection_nonce: "test".into(),
            connection_id: ConnectionId("connection".into()),
            daemon_generation: DaemonGeneration("generation".into()),
            generation_role: GenerationRole::Active,
            protocol: ProtocolVersion {
                generation: 1,
                revision: 0,
            },
            capabilities: vec![],
            build: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
                artifact: "test".into(),
            },
            limits: ProtocolLimits::default(),
            daemon_process: None,
        }
    }

    fn decision() -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: WorkspaceId::new(),
                session_id: None,
                caller: CallerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "Question".into(),
            prompt: "Choose".into(),
            options: vec![],
            allow_freeform: true,
            allow_comment: false,
            require_confirmation: false,
            recommendation: None,
            selection_limits: None,
            selection_mode: UserDecisionSelectionMode::Single,
            context: vec![],
            expires_at: None,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: chrono::Utc::now(),
            resolved_at: None,
        }
    }

    fn response_size(hello: &ServerHello, request: &RequestId, body: Value) -> usize {
        serde_json::to_vec(&super::super::envelope(
            hello,
            request.clone(),
            ResponseOutcome::Ok,
            body,
        ))
        .unwrap()
        .len()
    }

    #[test]
    fn decision_pages_fit_the_exact_envelope_limit() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let first = decision();
        let mut second = decision();
        second.prompt = "\"\\\n".repeat(2048);
        for item in [&first, &second] {
            store.create(item.clone()).unwrap().unwrap();
        }
        let mut hello = hello();
        let request = RequestId("request\"\\\n".repeat(5));
        let page = store
            .pending_page(UserDecisionScope::All, None, 2048)
            .unwrap()
            .unwrap();
        assert_eq!(page.decisions, vec![first.clone()]);
        let body = page_body(first.owner.workspace_id, &page);
        let exact = response_size(&hello, &request, body.clone());
        hello.limits.max_frame_bytes = u32::try_from(exact).unwrap();
        let actual = response(
            &store,
            &hello,
            &request,
            first.owner.workspace_id,
            UserDecisionScope::All,
            json!({}),
        )
        .unwrap();
        assert_eq!(actual, body);
        assert_eq!(response_size(&hello, &request, actual), exact);
        hello.limits.max_frame_bytes -= 1;
        let deferred = response(
            &store,
            &hello,
            &request,
            first.owner.workspace_id,
            UserDecisionScope::All,
            json!({}),
        )
        .unwrap();
        assert!(response_size(&hello, &request, deferred.clone()) < exact);
        assert_eq!(
            deferred["deferred"],
            json!([usagi_core::infrastructure::store::user_decision::wire::reference(&first)])
        );
        hello.limits.max_frame_bytes = u32::try_from(DEFAULT_MAX_FRAME_BYTES).unwrap();
        let last = response(
            &store,
            &hello,
            &request,
            first.owner.workspace_id,
            UserDecisionScope::All,
            json!({"cursor":page.next_cursor}),
        )
        .unwrap();
        assert!(response_size(&hello, &request, last.clone()) <= DEFAULT_MAX_FRAME_BYTES);
        let last: UserDecisionPage = serde_json::from_value(last).unwrap();
        assert_eq!(last.decisions, vec![second]);
        assert!(last.next_cursor.is_none());
    }

    #[test]
    fn decision_pages_keep_empty_inventory_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let mut hello = hello();
        let request = RequestId("empty".into());
        let workspace = WorkspaceId::new();
        let empty = response(
            &store,
            &hello,
            &request,
            workspace,
            UserDecisionScope::All,
            json!({}),
        )
        .unwrap();
        assert_eq!(
            empty,
            json!({"workspace":workspace,"decisions":[],"next_cursor":null})
        );
        let exact = response_size(&hello, &request, empty.clone());
        hello.limits.max_frame_bytes = u32::try_from(exact).unwrap();
        assert_eq!(
            response(
                &store,
                &hello,
                &request,
                workspace,
                UserDecisionScope::All,
                json!({})
            )
            .unwrap(),
            empty
        );
        for limit in [0, u32::try_from(exact - 1).unwrap()] {
            hello.limits.max_frame_bytes = limit;
            assert_eq!(
                response(
                    &store,
                    &hello,
                    &request,
                    workspace,
                    UserDecisionScope::All,
                    json!({})
                )
                .unwrap_err()
                .code,
                ErrorCode::ResourceExhausted
            );
        }
    }

    #[test]
    fn decision_pages_reject_invalid_stale_and_unavailable_observations() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let first = decision();
        let mut second = decision();
        second.prompt = "p".repeat(4096);
        for item in [&first, &second] {
            store.create(item.clone()).unwrap().unwrap();
        }
        let hello = hello();
        let request = RequestId("invalid".into());
        let workspace = first.owner.workspace_id;
        let cursor = store
            .pending_page(UserDecisionScope::All, None, 2048)
            .unwrap()
            .unwrap()
            .next_cursor
            .unwrap();
        for payload in [
            json!({"cursor":"bad"}),
            json!({"cursor":{"revision":cursor.revision,"offset":-1}}),
            json!({"cursor":{"revision":cursor.revision,"offset":0}}),
            json!({"cursor":{"revision":cursor.revision,"offset":2}}),
        ] {
            assert_eq!(
                response(
                    &store,
                    &hello,
                    &request,
                    workspace,
                    UserDecisionScope::All,
                    payload
                )
                .unwrap_err()
                .code,
                ErrorCode::InvalidArgument
            );
        }
        store.create(decision()).unwrap().unwrap();
        assert_eq!(
            response(
                &store,
                &hello,
                &request,
                workspace,
                UserDecisionScope::All,
                json!({"cursor":cursor})
            )
            .unwrap_err()
            .code,
            ErrorCode::RevisionConflict
        );
        std::fs::write(store.path(), "{").unwrap();
        assert_eq!(
            response(
                &store,
                &hello,
                &request,
                workspace,
                UserDecisionScope::All,
                json!({})
            )
            .unwrap_err()
            .code,
            ErrorCode::Unavailable
        );
    }
}
