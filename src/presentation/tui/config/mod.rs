//! Configuration screen (画面 #4).
//!
//! Lists the user-configurable settings and lets each be cycled through its
//! choices with ←/→. Edits are held in memory and flagged as changed; they are
//! written to disk only when the user presses the Save button, which stays
//! disabled until there is something to save.

pub mod event;
pub mod state;
pub mod ui;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use console::Term;

use crate::domain::settings::{LocalSettings, Settings};
use crate::infrastructure::git;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::term_reader::TermKeyReader;
use crate::usecase::doctor::SystemRunner;
use crate::usecase::{local_llm, settings, workspace};

pub use event::Outcome;

use state::Config;

/// Runs the configuration screen on the given terminal until the user goes back
/// or quits. Wires the real terminal and storage-backed settings to the
/// testable event loop in [`event`]. Assumes the alternate screen is already
/// active.
///
/// When invoked from inside a git repository, the screen also edits that
/// project's local overrides (`<repo>/.usagi/settings.json`); the global and
/// local settings are saved together when the user saves. The project context
/// is the repository containing the current directory.
pub fn run(term: &Term) -> Result<Outcome> {
    run_in(term, current_repo_root())
}

/// Runs the configuration screen with an explicit project context: `repo_root`
/// is the repository whose local overrides are edited alongside the global
/// settings, or `None` to edit only the global settings. Used by the workspace
/// screen's `config` command, which knows the workspace it was opened for.
pub fn run_in(term: &Term, repo_root: Option<PathBuf>) -> Result<Outcome> {
    let storage = Storage::open_default()?;

    let (mut config, notice) = match load(&storage, repo_root.as_deref()) {
        Ok((settings, workspaces, local)) => {
            let config = match (repo_root.as_ref(), local) {
                (Some(_), Some(local)) => Config::with_local(settings, workspaces, local),
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
    // Saving writes the global settings, plus the project-local overrides when
    // the screen has a project context.
    let mut save = |global: &Settings, local: Option<&LocalSettings>| -> Result<()> {
        settings::save(&storage, global)?;
        if let (Some(root), Some(local)) = (repo_root.as_ref(), local) {
            // Always write the file, even when every override is cleared: an
            // empty file simply means "defer entirely to the global settings".
            settings::save_local(root, local)?;
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
        local_llm::SetupError::ModelPullFailed { model } => {
            format!("could not pull `{model}`")
        }
    }
}

/// The primary worktree of the repository containing the current directory, or
/// `None` when usagi is not run from inside a git repository.
fn current_repo_root() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    if !git::is_repository(&cwd) {
        return None;
    }
    git::primary_worktree(&cwd).ok()
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
