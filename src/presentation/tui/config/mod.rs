//! Configuration screen (画面 #4).
//!
//! Lists the user-configurable settings and lets each be cycled through its
//! choices with ←/→. Edits are held in memory and flagged as changed; they are
//! written to disk only when the user presses the Save button, which stays
//! disabled until there is something to save.

pub mod event;
pub mod state;
pub mod ui;

use std::path::PathBuf;

use anyhow::Result;
use console::Term;

use crate::domain::settings::{LocalSettings, Settings};
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::term_reader::TermKeyReader;
use crate::usecase::doctor::SystemRunner;
use crate::usecase::{local_llm, settings, workspace};

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
                (Some(_), Some(local)) => Config::workspace(settings, local),
                _ => Config::new(settings, workspaces),
            };
            (config, None)
        }
        Err(e) => (
            Config::new(Settings::default(), Vec::new()),
            Some(format!("Failed to load settings: {e}")),
        ),
    };

    // Probe whether the local LLM runtime and selected model are already
    // present, so the Local LLM row opens as an "Install" action or an on/off
    // toggle accordingly.
    let runner = SystemRunner;
    let model = config.local_llm_model().to_string();
    config.set_local_llm_installed(
        local_llm::ollama_installed(&runner) && local_llm::model_present(&runner, &model),
    );

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
    // Installing the local LLM provisions the runtime + model on demand (this
    // blocks the screen while `ollama pull` runs, then the row becomes a toggle).
    let mut install = |model: &str| -> Result<()> {
        local_llm::ensure(std::env::consts::OS, &SystemRunner, model)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(install_error_message(&e)))
    };
    event::event_loop(term, &mut reader, config, &mut save, &mut install, notice)
}

/// A short human message for a local LLM provisioning failure.
fn install_error_message(error: &local_llm::SetupError) -> String {
    match error {
        local_llm::SetupError::OllamaUnavailable { manual }
        | local_llm::SetupError::OllamaInstallFailed { manual, .. } => manual.clone(),
        local_llm::SetupError::ServerStartFailed => local_llm::server_start_failed_message(),
        local_llm::SetupError::ModelPullFailed { model } => {
            format!("could not pull `{model}`")
        }
    }
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
