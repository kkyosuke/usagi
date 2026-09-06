use std::fs;

use serde_json::json;
use usagi_core::domain::id::{DaemonGeneration, OperationId};
use usagi_core::usecase::client::SessionAction;
use usagi_daemon::infrastructure::session_worktree::{SystemGit, SystemSessionWorktreeIo};
use usagi_daemon::usecase::session_runtime::{SessionRuntime, SessionRuntimeError};

#[test]
fn invalid_base_ref_is_rejected_before_a_worktree_effect() {
    let fixture = tempfile::tempdir().expect("temp dir");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let mut runtime = SessionRuntime::open(
        workspace.clone(),
        &fixture.path().join("data/daemon"),
        DaemonGeneration::new(),
        SystemGit,
        SystemSessionWorktreeIo,
    )
    .expect("session runtime");
    let operation_id = OperationId::new().as_str();

    assert_eq!(
        runtime.handle(
            SessionAction::Create,
            &operation_id,
            &json!({"name":"invalid-base-ref", "base_ref":"main"}),
        ),
        Err(SessionRuntimeError::InvalidRequest)
    );
    assert!(!workspace.join(".usagi/sessions/invalid-base-ref").exists());
}
