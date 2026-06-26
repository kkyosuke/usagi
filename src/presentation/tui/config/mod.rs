//! Configuration screen (画面 #4).
//!
//! Lists the user-configurable settings and lets each be cycled through its
//! choices with ←/→. Edits are held in memory and flagged as changed; they are
//! written to disk only when the user presses the Save button, which stays
//! disabled until there is something to save.

pub mod event;
mod provisioning;
pub mod state;
pub mod ui;

use std::path::PathBuf;

use anyhow::Result;
use console::Term;

use crate::domain::settings::{LocalSettings, Settings, LOCAL_LLM_MODELS};
use crate::infrastructure::git;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::io::term_reader::TermKeyReader;
use crate::usecase::doctor::SystemRunner;
use crate::usecase::{agent, local_llm, settings, workspace};

pub use event::Outcome;

use state::Config;

/// Runs the configuration screen for the application-wide **global** settings
/// (`~/.usagi/settings.json`). Used by the CLI and the welcome menu, neither of
/// which is tied to a particular workspace. Assumes the alternate screen is
/// already active.
pub fn run(term: &Term) -> Result<Outcome> {
    run_in(term, None)
}

/// Runs the configuration screen, choosing its scope from `repo_root`:
///
/// - `Some(root)` edits only that project's **local** overrides
///   (`<root>/.usagi/settings.json`). Used by the workspace home screen's
///   `config` command.
/// - `None` edits only the **global** settings. Used by [`run`].
///
/// The two scopes never share a screen: the global settings are loaded in the
/// local scope as well, but only to display the value each unset override falls
/// back to.
pub fn run_in(term: &Term, repo_root: Option<PathBuf>) -> Result<Outcome> {
    let storage = Storage::open_default()?;

    let (mut config, notice) = match load(&storage, repo_root.as_deref()) {
        Ok((settings, workspaces, local)) => {
            let config = match (repo_root.as_ref(), local) {
                (Some(root), Some(local)) => {
                    // Offer the repository's branches as Default Branch choices.
                    Config::workspace(settings, local, git::list_branches(root))
                }
                _ => Config::new(settings, workspaces),
            };
            (config, None)
        }
        Err(e) => (
            Config::new(Settings::default(), Vec::new()),
            Some(format!("Failed to load settings: {e}")),
        ),
    };

    // Probe which agent CLIs are installed so the Agent CLI selector only offers
    // agents the user can actually launch. Both scopes render that selector (the
    // global setting and the per-project override), so this runs unconditionally.
    config.set_available_agent_clis(agent::available_clis(&SystemRunner));

    // Probe whether the local LLM runtime and selected model are already
    // present, so the Local LLM row opens as an "Install" action or an on/off
    // toggle accordingly. Only the global scope renders that row
    // (`LocalField::ALL` has no Local LLM field), so skip the two `ollama`
    // subprocess probes when editing a workspace's local overrides.
    if repo_root.is_none() {
        let runner = SystemRunner;
        if local_llm::ollama_installed(&runner) {
            config.set_ollama_installed(true);
            config.set_installed_models(
                LOCAL_LLM_MODELS
                    .iter()
                    .filter(|model| local_llm::model_present(&runner, model))
                    .map(|model| model.to_string())
                    .collect(),
            );
        }
    }

    let mut reader = TermKeyReader::new(term.clone());
    // The scope decides what is written: a project context saves only the
    // project-local overrides, otherwise only the global settings.
    let mut save = |global: &Settings, local: Option<&LocalSettings>| -> Result<()> {
        match repo_root.as_ref() {
            Some(root) => {
                if let Some(local) = local {
                    // Always write the file, even when every override is cleared:
                    // an empty file simply means "defer to the global settings".
                    settings::save_local(root, local)?;
                }
            }
            None => settings::save(&storage, global)?,
        }
        Ok(())
    };
    // Provisioning runs on a background thread and returns immediately, so the
    // user can keep using usagi (and leave this screen) while it proceeds; the
    // global install task surfaces a loading rabbit on every screen until it
    // finishes. The Local LLM row installs just the `ollama` runtime (the sudo
    // password from the modal pre-authenticates its privileged steps); the model
    // picker pulls a chosen-but-unpulled model (unprivileged).
    let mut install_runtime =
        |password: &str| -> Result<()> { provisioning::start_install_runtime(password) };
    let mut pull_model = |model: &str| -> Result<()> { provisioning::start_pull_model(model) };
    event::event_loop(
        term,
        &mut reader,
        config,
        &mut save,
        &mut install_runtime,
        &mut pull_model,
        notice,
    )
}

/// Loads the global settings, the registered workspace names the
/// default-workspace field can cycle through, and (when a project context is
/// given) that project's local overrides.
fn load(
    storage: &Storage,
    repo_root: Option<&std::path::Path>,
) -> Result<(Settings, Vec<String>, Option<LocalSettings>)> {
    let current = settings::load(storage)?;
    let workspaces = workspace::list(storage)?
        .into_iter()
        .map(|w| w.name)
        .collect();
    let local = match repo_root {
        Some(root) => Some(settings::load_local(root)?),
        None => None,
    };
    Ok((current, workspaces, local))
}
