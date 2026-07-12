use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::domain::{
    agent::{
        AgentCapability, AgentProfileId, EnvironmentVariableName, LaunchMode, LaunchRequest,
        LaunchScope, LaunchValidationError, ModelSelector,
    },
    id::{
        AgentRuntimeId, AgentRuntimeRef, CompletionFence, DaemonGeneration, OperationId, SessionId,
        TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    },
};

use super::{
    CodexAdapter, CodexMaterial, CodexProvision, CodexProvisionFailure, CodexProvisioner,
    LaunchResolver,
};
use crate::usecase::{
    generation::ProcessIdentity,
    runtime::{PtySpawner, RuntimeCoordinator, RuntimeStore, RuntimeStoreSnapshot},
    terminal::Geometry,
};

#[derive(Debug)]
struct FakeProvisioner {
    result: Result<CodexProvision, CodexProvisionFailure>,
    calls: Vec<(LaunchScope, Vec<CodexMaterial>)>,
}

impl FakeProvisioner {
    fn ready() -> Self {
        Self {
            result: Ok(CodexProvision {
                working_directory: PathBuf::from("/worktree"),
                environment_allowlist: [EnvironmentVariableName::new("USAGI_RUNTIME").unwrap()]
                    .into_iter()
                    .collect(),
            }),
            calls: Vec::new(),
        }
    }
}

impl CodexProvisioner for FakeProvisioner {
    fn provision(
        &mut self,
        scope: &LaunchScope,
        material: &[CodexMaterial],
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        self.calls.push((scope.clone(), material.to_vec()));
        self.result.clone()
    }
}

fn request(mode: LaunchMode) -> LaunchRequest {
    LaunchRequest {
        profile_id: AgentProfileId::new("codex").unwrap(),
        mode,
        model: Some(ModelSelector::new("gpt-5-codex").unwrap()),
        resume: false,
        initial_prompt: Some("fix the test".into()),
        scope: LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: SessionId::new(),
            worktree_id: WorktreeId::new(),
        },
        required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
    }
}

#[test]
fn renders_public_interactive_argv_and_materializes_all_codex_artifacts_in_scope() {
    let provisioner = FakeProvisioner::ready();
    let mut adapter = CodexAdapter::new(provisioner);
    let request = request(LaunchMode::Interactive);

    let snapshot = adapter.resolve(&request).unwrap();

    assert_eq!(snapshot.plan.program, "codex");
    assert_eq!(
        snapshot.plan.argv,
        [
            "--dangerously-bypass-hook-trust",
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "on-request",
            "-m",
            "gpt-5-codex",
            "--",
            "fix the test",
        ]
    );
    assert_eq!(snapshot.plan.working_directory, PathBuf::from("/worktree"));
    assert_eq!(
        adapter.provisioner.calls,
        vec![(
            request.scope,
            vec![
                CodexMaterial::Config,
                CodexMaterial::Mcp,
                CodexMaterial::Hooks
            ]
        )]
    );
}

#[test]
fn renders_resume_only_without_an_initial_prompt() {
    let mut request = request(LaunchMode::Interactive);
    request.resume = true;
    request.initial_prompt = None;
    let mut adapter = CodexAdapter::new(FakeProvisioner::ready());

    let snapshot = adapter.resolve(&request).unwrap();

    assert_eq!(
        snapshot.plan.argv,
        [
            "resume",
            "--last",
            "--dangerously-bypass-hook-trust",
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "on-request",
            "-m",
            "gpt-5-codex",
        ]
    );
}

#[test]
fn headless_requires_a_prompt_and_does_not_accept_resume() {
    let mut adapter = CodexAdapter::new(FakeProvisioner::ready());
    let mut missing_prompt = request(LaunchMode::Headless);
    missing_prompt.initial_prompt = None;
    assert_eq!(
        adapter.resolve(&missing_prompt),
        Err(LaunchValidationError::EmptyPrompt)
    );

    let mut resume = request(LaunchMode::Headless);
    resume.resume = true;
    assert_eq!(
        adapter.resolve(&resume),
        Err(LaunchValidationError::UnsupportedCapability {
            capability: AgentCapability::Resume
        })
    );
}

#[test]
fn renders_headless_exec_and_exposes_the_static_profile() {
    let mut adapter = CodexAdapter::new(FakeProvisioner::ready());
    let snapshot = adapter.resolve(&request(LaunchMode::Headless)).unwrap();

    assert_eq!(adapter.profile().id.as_str(), "codex");
    assert_eq!(
        snapshot.plan.argv,
        [
            "exec",
            "--dangerously-bypass-approvals-and-sandbox",
            "-m",
            "gpt-5-codex",
            "--",
            "fix the test",
        ]
    );
}

#[test]
fn rejects_unknown_profiles_and_unsupported_product_capabilities_before_provisioning() {
    let mut unknown = request(LaunchMode::Interactive);
    unknown.profile_id = AgentProfileId::new("other").unwrap();
    let mut adapter = CodexAdapter::new(FakeProvisioner::ready());
    assert_eq!(
        adapter.resolve(&unknown),
        Err(LaunchValidationError::UnknownProfile {
            profile_id: AgentProfileId::new("other").unwrap()
        })
    );
    assert!(adapter.provisioner.calls.is_empty());

    let mut unsupported = request(LaunchMode::Interactive);
    unsupported
        .required_capabilities
        .insert(AgentCapability::PhaseReporting);
    assert_eq!(
        adapter.resolve(&unsupported),
        Err(LaunchValidationError::UnsupportedCapability {
            capability: AgentCapability::PhaseReporting
        })
    );
    assert!(adapter.provisioner.calls.is_empty());
}

#[test]
fn typed_pre_spawn_provision_failures_do_not_create_a_snapshot() {
    for (failure, expected) in [
        (
            CodexProvisionFailure::ExecutableUnavailable,
            LaunchValidationError::InvalidProgram,
        ),
        (
            CodexProvisionFailure::MaterializationFailed,
            LaunchValidationError::InvalidWorkingDirectory,
        ),
    ] {
        let mut provisioner = FakeProvisioner::ready();
        provisioner.result = Err(failure);
        let mut adapter = CodexAdapter::new(provisioner);
        assert_eq!(
            adapter.resolve(&request(LaunchMode::Interactive)),
            Err(expected)
        );
    }
}

#[test]
fn durable_snapshot_contains_no_provisioned_values_and_fails_closed_on_revision_drift() {
    let mut adapter = CodexAdapter::new(FakeProvisioner::ready());
    let snapshot = adapter.resolve(&request(LaunchMode::Interactive)).unwrap();
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains("secret"));
    assert!(!serialized.contains("credential"));
    assert!(adapter.validate_snapshot(&snapshot).is_ok());

    let newer = CodexAdapter::with_revision(FakeProvisioner::ready(), 2);
    assert_eq!(
        newer.validate_snapshot(&snapshot),
        Err(LaunchValidationError::ProfileRevisionMismatch {
            expected: 1,
            actual: 2
        })
    );
}

#[test]
fn provisioned_environment_is_an_allowlist_not_an_environment_value_map() {
    let provision = CodexProvision {
        working_directory: PathBuf::from("/worktree"),
        environment_allowlist: BTreeSet::new(),
    };
    assert!(provision.environment_allowlist.is_empty());
}

#[derive(Default)]
struct Store(Vec<RuntimeStoreSnapshot>);

impl RuntimeStore for Store {
    type Error = ();

    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), Self::Error> {
        self.0.push(snapshot);
        Ok(())
    }
}

struct FakeSpawner {
    calls: usize,
}

impl PtySpawner for FakeSpawner {
    fn spawn(
        &mut self,
        _: &usagi_core::domain::agent::DurableLaunchSnapshot,
        _: &TerminalRef,
    ) -> Result<ProcessIdentity, crate::usecase::runtime::SpawnFailure> {
        self.calls += 1;
        Ok(ProcessIdentity {
            pid: 42,
            start_identity: "fake-start".into(),
            process_group: 42,
        })
    }
}

#[test]
fn runtime_reservation_uses_the_codex_resolver_before_pty_spawn_and_exits_normally() {
    let request = request(LaunchMode::Interactive);
    let generation = DaemonGeneration::new();
    let terminal = TerminalRef {
        daemon_generation: generation,
        terminal_id: TerminalId::new(),
        workspace_id: request.scope.workspace_id,
        session_id: Some(request.scope.session_id),
        worktree_id: request.scope.worktree_id,
    };
    let runtime =
        AgentRuntimeRef::new(AgentRuntimeId::new(), terminal, request.scope.session_id).unwrap();
    let fence = CompletionFence {
        workspace_id: request.scope.workspace_id,
        session_id: request.scope.session_id,
        operation_id: OperationId::new(),
        owner_daemon_generation: generation,
        execution_attempt: 1,
        lifecycle_attempt: 1,
        expected_revision: 1,
    };
    let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
    let mut resolver = CodexAdapter::new(FakeProvisioner::ready());
    let mut store = Store::default();
    let mut spawner = FakeSpawner { calls: 0 };

    coordinator
        .launch(
            &request,
            runtime.clone(),
            fence,
            Geometry { cols: 80, rows: 24 },
            &mut resolver,
            &mut store,
            &mut spawner,
        )
        .unwrap();

    assert_eq!(spawner.calls, 1);
    assert_eq!(store.0.len(), 2);
    assert_eq!(store.0[0].records[0].launch.plan.program, "codex");
    assert!(
        store.0[0].records[0]
            .launch
            .plan
            .argv
            .contains(&"fix the test".into())
    );
    coordinator.exit(&runtime, 0, &mut store).unwrap();
    assert_eq!(coordinator.occupied_slots(), 0);
}
