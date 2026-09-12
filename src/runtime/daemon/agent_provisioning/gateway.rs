//! Provider gateway environment.
//!
//! Some providers are not a CLI of their own: they are a shared CLI plus the
//! environment that makes it talk to *that* product. This module owns the one
//! place where that environment is assembled, so the launch and the readiness
//! probe cannot build it differently.

use std::collections::BTreeMap;
use std::path::Path;

use usagi_core::domain::agent::EnvironmentVariableName;
use usagi_core::domain::settings::DefaultModel;

/// The variables that make a shared CLI *be* this provider: its endpoint, its
/// model bindings, the directory it keeps state in, and its API key.
///
/// They come after the user's own bindings in [`launch_environment`] for the
/// same reason the MCP wiring does: a workspace must not be able to redirect a
/// managed launch by binding the same name. A provider that needs its own state
/// directory but has no resolved `$HOME` fails the launch instead of silently
/// falling back to the shared CLI's default home — that default is another
/// provider's state. A missing API key is *not* fatal here: the readiness probe
/// runs the same environment and refuses the launch with a recovery reason,
/// which is a better answer than a provisioning failure.
pub(in crate::runtime) fn provider_gateway_environment(
    agent: DefaultModel,
    home: Option<&Path>,
    user: &BTreeMap<String, String>,
) -> Result<Vec<(EnvironmentVariableName, String)>, ()> {
    let mut environment = Vec::new();
    for (name, value) in agent.gateway_environment() {
        environment.push((typed(name), (*value).to_owned()));
    }
    if let Some(name) = agent.state_directory_env() {
        let home = home.ok_or(())?;
        let directory = home.join(agent.state_directory());
        environment.push((typed(name), directory.to_str().ok_or(())?.to_owned()));
    }
    if let Some((source, target)) = agent.credential_binding()
        && let Some(value) = user.get(source)
    {
        environment.push((typed(target), value.clone()));
    }
    Ok(environment)
}

/// Every name here is a literal owned by the closed vocabulary, so an invalid
/// one is a programmer error in that table rather than a launch-time condition.
///
/// # Panics
///
/// Panics only if the vocabulary starts naming a variable that is not a valid
/// environment variable name.
fn typed(name: &str) -> EnvironmentVariableName {
    EnvironmentVariableName::new(name).expect("vocabulary environment name is valid")
}
