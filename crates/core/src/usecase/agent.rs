//! Pure profile lookup and launch-boundary validation.

use crate::domain::agent::{
    AgentProfile, AgentProfileId, DurableLaunchSnapshot, LaunchRequest, LaunchValidationError,
};

/// Code-defined profile lookup seam. Adapters register static descriptors here;
/// this catalog is not durable daemon state and performs no executable probing.
pub trait AgentProfileCatalog {
    /// Returns the descriptor for a stable profile ID, if this adapter owns it.
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile>;
}

/// Validates a request against static profile metadata without rendering or IO.
///
/// # Errors
///
/// Returns a typed rejection when the profile is unknown or does not satisfy
/// the requested mode/capabilities.
pub fn validate_request(
    catalog: &impl AgentProfileCatalog,
    request: &LaunchRequest,
) -> Result<AgentProfile, LaunchValidationError> {
    let profile =
        catalog
            .find(&request.profile_id)
            .ok_or_else(|| LaunchValidationError::UnknownProfile {
                profile_id: request.profile_id.clone(),
            })?;
    if !profile.allowed_modes.contains(&request.mode) {
        return Err(LaunchValidationError::UnsupportedMode { mode: request.mode });
    }
    if request.initial_prompt.as_deref().is_some_and(str::is_empty) {
        return Err(LaunchValidationError::EmptyPrompt);
    }
    for capability in request.required_capabilities() {
        if !profile.capabilities.contains(&capability) {
            return Err(LaunchValidationError::UnsupportedCapability { capability });
        }
    }
    Ok(profile)
}

/// Restores a durable snapshot only when it still exactly matches the static
/// descriptor and immutable request provenance. It intentionally never falls
/// back to re-resolving a newer profile revision.
///
/// # Errors
///
/// Returns a typed, fail-closed rejection for schema, profile revision,
/// request, or plan provenance mismatches.
pub fn validate_snapshot(
    catalog: &impl AgentProfileCatalog,
    snapshot: &DurableLaunchSnapshot,
) -> Result<AgentProfile, LaunchValidationError> {
    if snapshot.schema_version != DurableLaunchSnapshot::SCHEMA_VERSION {
        return Err(LaunchValidationError::SnapshotSchemaMismatch {
            expected: DurableLaunchSnapshot::SCHEMA_VERSION,
            actual: snapshot.schema_version,
        });
    }
    let profile = validate_request(catalog, &snapshot.request)?;
    if profile.revision != snapshot.plan.profile_revision {
        return Err(LaunchValidationError::ProfileRevisionMismatch {
            expected: snapshot.plan.profile_revision,
            actual: profile.revision,
        });
    }
    if snapshot.plan.profile_id != snapshot.request.profile_id {
        return Err(LaunchValidationError::PlanProvenanceMismatch);
    }
    Ok(profile)
}
