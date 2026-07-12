use super::{
    AgentRuntimeId, AgentRuntimeRef, ClientId, CompletionFence, ConnectionId, DaemonGeneration,
    IdParseError, LegacyIdentityError, OperationId, ProtocolVersion, ProtocolVersionError,
    RequestId, ScopeError, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    migrate_identity,
};

fn terminal(session_id: Option<SessionId>) -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id,
        worktree_id: WorktreeId::new(),
    }
}

#[test]
fn resource_ids_round_trip_through_canonical_text_and_serde() {
    let ids = [
        WorkspaceId::new().as_str(),
        SessionId::new().as_str(),
        WorktreeId::new().as_str(),
        TerminalId::new().as_str(),
        AgentRuntimeId::new().as_str(),
        ClientId::new().as_str(),
        ConnectionId::new().as_str(),
        RequestId::new().as_str(),
        DaemonGeneration::new().as_str(),
    ];
    assert!(
        ids.iter()
            .all(|id| id.len() == 36 && id == &id.to_lowercase())
    );

    let id = WorkspaceId::new();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(serde_json::from_str::<WorkspaceId>(&json).unwrap(), id);
    assert_eq!(WorkspaceId::parse(&id.as_str()).unwrap(), id);
    assert_eq!(id.to_string(), id.as_str());
}

#[test]
fn ids_reject_invalid_and_noncanonical_input() {
    assert_eq!(
        WorkspaceId::parse("not-an-id"),
        Err(IdParseError::InvalidUuid)
    );
    assert_eq!(
        WorkspaceId::parse("550E8400-E29B-41D4-A716-446655440000"),
        Err(IdParseError::NonCanonical)
    );
    assert!(serde_json::from_str::<WorkspaceId>("\"not-an-id\"").is_err());
    assert!(IdParseError::InvalidUuid.to_string().contains("UUID"));
    assert!(IdParseError::NonCanonical.to_string().contains("canonical"));
}

#[test]
fn operation_id_is_uuid_v7_and_rejects_other_uuid_versions() {
    let operation = OperationId::new();
    assert_eq!(OperationId::parse(&operation.as_str()).unwrap(), operation);
    assert_eq!(
        serde_json::from_str::<OperationId>(&serde_json::to_string(&operation).unwrap()).unwrap(),
        operation
    );
    assert!(matches!(
        OperationId::parse(&WorkspaceId::new().as_str()),
        Err(IdParseError::WrongVersion {
            expected: 7,
            actual: Some(4)
        })
    ));
    assert!(
        IdParseError::WrongVersion {
            expected: 7,
            actual: Some(4),
        }
        .to_string()
        .contains("UUIDv7")
    );
    assert_eq!(operation.to_string(), operation.as_str());
}

#[test]
fn protocol_version_reserves_zero_generation() {
    assert_eq!(
        ProtocolVersion::new(0, 1),
        Err(ProtocolVersionError::ZeroGeneration)
    );
    let version = ProtocolVersion::new(2, 3).unwrap();
    assert_eq!(
        serde_json::from_str::<ProtocolVersion>(&serde_json::to_string(&version).unwrap()).unwrap(),
        version
    );
    assert_eq!(
        ProtocolVersionError::ZeroGeneration.to_string(),
        "protocol generation must be greater than zero"
    );
    assert!(serde_json::from_str::<ProtocolVersion>("{\"generation\":0,\"revision\":1}").is_err());
}

#[test]
fn terminal_fence_rejects_every_stale_scope_dimension() {
    let current = terminal(Some(SessionId::new()));
    let cases = [
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            ..current.clone()
        },
        TerminalRef {
            terminal_id: TerminalId::new(),
            ..current.clone()
        },
        TerminalRef {
            workspace_id: WorkspaceId::new(),
            ..current.clone()
        },
        TerminalRef {
            session_id: Some(SessionId::new()),
            ..current.clone()
        },
        TerminalRef {
            worktree_id: WorktreeId::new(),
            ..current.clone()
        },
    ];
    assert!(current.fences(&current));
    assert!(cases.iter().all(|candidate| !current.fences(candidate)));
}

#[test]
fn runtime_scope_requires_its_terminal_session_and_runtime_id() {
    let session = SessionId::new();
    let scoped_terminal = terminal(Some(session));
    let current = AgentRuntimeRef::new(AgentRuntimeId::new(), scoped_terminal, session).unwrap();
    let other =
        AgentRuntimeRef::new(AgentRuntimeId::new(), current.terminal.clone(), session).unwrap();
    assert!(!current.fences(&other));
    assert_eq!(
        AgentRuntimeRef::new(AgentRuntimeId::new(), terminal(None), session),
        Err(ScopeError::SessionDoesNotOwnTerminal)
    );
    assert!(
        ScopeError::SessionDoesNotOwnTerminal
            .to_string()
            .contains("must own")
    );
}

#[test]
fn completion_fence_rejects_every_late_worker_mismatch() {
    let current = CompletionFence {
        workspace_id: WorkspaceId::new(),
        session_id: SessionId::new(),
        operation_id: OperationId::new(),
        owner_daemon_generation: DaemonGeneration::new(),
        execution_attempt: 1,
        lifecycle_attempt: 1,
        expected_revision: 4,
    };
    let cases = [
        CompletionFence {
            workspace_id: WorkspaceId::new(),
            ..current.clone()
        },
        CompletionFence {
            session_id: SessionId::new(),
            ..current.clone()
        },
        CompletionFence {
            operation_id: OperationId::new(),
            ..current.clone()
        },
        CompletionFence {
            owner_daemon_generation: DaemonGeneration::new(),
            ..current.clone()
        },
        CompletionFence {
            execution_attempt: 2,
            ..current.clone()
        },
        CompletionFence {
            lifecycle_attempt: 2,
            ..current.clone()
        },
        CompletionFence {
            expected_revision: 5,
            ..current.clone()
        },
    ];
    assert!(current.fences(&current));
    assert!(cases.iter().all(|candidate| !current.fences(candidate)));
}

#[test]
fn legacy_identity_migration_fails_closed() {
    assert!(migrate_identity(Some(WorkspaceId::new()), false).is_ok());
    assert_eq!(
        migrate_identity::<WorkspaceId>(None, false),
        Err(LegacyIdentityError::Missing)
    );
    assert_eq!(
        migrate_identity(Some(WorkspaceId::new()), true),
        Err(LegacyIdentityError::Ambiguous)
    );
    assert!(
        LegacyIdentityError::Missing
            .to_string()
            .contains("no typed")
    );
    assert!(
        LegacyIdentityError::Ambiguous
            .to_string()
            .contains("ambiguous")
    );
}
