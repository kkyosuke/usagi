//! Public API contract tests for product-neutral agent adapters.

use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::{
    domain::{
        agent::{
            AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot,
            EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
            LaunchValidationError,
        },
        id::{SessionId, WorkspaceId, WorktreeId},
    },
    usecase::agent::{AgentProfileCatalog, validate_request, validate_snapshot},
};

#[derive(Clone)]
struct FakeAdapter {
    profile: AgentProfile,
}

impl AgentProfileCatalog for FakeAdapter {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        (profile_id == &self.profile.id).then(|| self.profile.clone())
    }
}

fn adapter(name: &str, revision: u32) -> FakeAdapter {
    FakeAdapter {
        profile: AgentProfile::new(
            AgentProfileId::new(name).unwrap(),
            name,
            revision,
            [
                AgentCapability::Resume,
                AgentCapability::InitialPrompt,
                AgentCapability::Headless,
            ],
            [LaunchMode::Interactive, LaunchMode::Headless],
        ),
    }
}

fn request(profile_id: AgentProfileId) -> LaunchRequest {
    LaunchRequest {
        profile_id,
        mode: LaunchMode::Headless,
        model: None,
        resume: true,
        initial_prompt: Some("continue safely".into()),
        scope: LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: SessionId::new(),
            worktree_id: WorktreeId::new(),
        },
        required_capabilities: BTreeSet::new(),
    }
}

fn plan(request: &LaunchRequest, revision: u32) -> LaunchPlan {
    LaunchPlan::new(
        request.profile_id.clone(),
        revision,
        "adapter-program",
        vec!["--resume".into()],
        [EnvironmentVariableName::new("TERM").unwrap()],
        PathBuf::from("/workspace"),
    )
    .unwrap()
}

#[test]
fn independent_adapter_catalogs_consume_the_same_request_contract() {
    let claude = adapter("fake-claude", 7);
    let codex = adapter("fake-codex", 4);
    assert_eq!(
        validate_request(&claude, &request(claude.profile.id.clone())).unwrap(),
        claude.profile
    );
    assert_eq!(
        validate_request(&codex, &request(codex.profile.id.clone())).unwrap(),
        codex.profile
    );
}

#[test]
fn unsupported_capability_and_unknown_profile_are_typed_rejections() {
    let catalog = adapter("fake-claude", 1);
    let mut unsupported = request(catalog.profile.id.clone());
    unsupported
        .required_capabilities
        .insert(AgentCapability::McpWiring);
    assert_eq!(
        validate_request(&catalog, &unsupported),
        Err(LaunchValidationError::UnsupportedCapability {
            capability: AgentCapability::McpWiring
        })
    );
    let unknown = request(AgentProfileId::new("unknown").unwrap());
    assert_eq!(
        validate_request(&catalog, &unknown),
        Err(LaunchValidationError::UnknownProfile {
            profile_id: AgentProfileId::new("unknown").unwrap()
        })
    );
}

#[test]
fn unsupported_mode_and_empty_prompt_are_typed_rejections() {
    let catalog = FakeAdapter {
        profile: AgentProfile::new(
            AgentProfileId::new("interactive-only").unwrap(),
            "interactive-only",
            1,
            [],
            [LaunchMode::Interactive],
        ),
    };
    let mut request = request(catalog.profile.id.clone());
    assert_eq!(
        validate_request(&catalog, &request),
        Err(LaunchValidationError::UnsupportedMode {
            mode: LaunchMode::Headless
        })
    );
    request.mode = LaunchMode::Interactive;
    request.resume = false;
    request.initial_prompt = Some(String::new());
    assert_eq!(
        validate_request(&catalog, &request),
        Err(LaunchValidationError::EmptyPrompt)
    );
}

#[test]
fn durable_snapshot_is_fail_closed_for_schema_and_revision_changes() {
    let catalog = adapter("fake-claude", 7);
    let request = request(catalog.profile.id.clone());
    let mut snapshot = DurableLaunchSnapshot::new(request.clone(), plan(&request, 7));
    assert_eq!(
        validate_snapshot(&catalog, &snapshot).unwrap(),
        catalog.profile
    );
    snapshot.schema_version = 2;
    assert_eq!(
        validate_snapshot(&catalog, &snapshot),
        Err(LaunchValidationError::SnapshotSchemaMismatch {
            expected: 1,
            actual: 2
        })
    );
    snapshot.schema_version = 1;
    snapshot.plan.profile_revision = 6;
    assert_eq!(
        validate_snapshot(&catalog, &snapshot),
        Err(LaunchValidationError::ProfileRevisionMismatch {
            expected: 6,
            actual: 7
        })
    );
    snapshot.plan.profile_revision = 7;
    snapshot.plan.profile_id = AgentProfileId::new("different-profile").unwrap();
    assert_eq!(
        validate_snapshot(&catalog, &snapshot),
        Err(LaunchValidationError::PlanProvenanceMismatch)
    );
    let mut invalid_request = request.clone();
    invalid_request.initial_prompt = Some(String::new());
    let invalid_snapshot =
        DurableLaunchSnapshot::new(invalid_request.clone(), plan(&invalid_request, 7));
    assert_eq!(
        validate_snapshot(&catalog, &invalid_snapshot),
        Err(LaunchValidationError::EmptyPrompt)
    );
}

#[test]
fn durable_serialization_contains_no_environment_values_or_secret_argument() {
    let catalog = adapter("fake-claude", 7);
    let request = request(catalog.profile.id.clone());
    let snapshot = DurableLaunchSnapshot::new(request.clone(), plan(&request, 7));
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(json.contains("TERM"));
    assert!(!json.contains("environment_value"));
    assert_eq!(
        LaunchPlan::new(
            request.profile_id,
            7,
            "adapter-program",
            vec!["token=secret".into()],
            [],
            PathBuf::from("/workspace")
        ),
        Err(LaunchValidationError::InvalidArgumentVector)
    );
}
