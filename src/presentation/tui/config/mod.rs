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

use crate::domain::settings::{LocalSettings, Settings, LOCAL_LLM_MODELS};
use crate::infrastructure::git;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::screen::FramePainter;
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

    // Probe whether the `ollama` runtime is installed and which offered models
    // are already pulled, so the Local LLM row opens as an "Install" action or
    // an on/off toggle, and the model picker shows the right install markers.
    // Only the global scope renders those rows (`LocalField::ALL` has no Local
    // LLM field), so skip the `ollama` subprocess probes when editing a
    // workspace's local overrides; the per-model probe also only runs once the
    // runtime is present (each is an `ollama show`).
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
    // Installing the `ollama` runtime and pulling a model both run on a
    // background thread so the screen can animate a spinner while they proceed.
    // The runtime install takes the sudo password from the modal to
    // pre-authenticate its privileged steps; `ollama pull` is unprivileged.
    let mut install_runtime =
        |password: &str| -> Result<()> { run_install_with_spinner(term, password) };
    let mut pull_model = |model: &str| -> Result<()> { run_pull_with_spinner(term, model) };
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

/// Installs the `ollama` runtime on a background thread, animating the install
/// spinner on the main thread until it finishes. The sudo password is forwarded
/// to [`local_llm::ensure_runtime`] so the installer can elevate unattended.
fn run_install_with_spinner(term: &Term, password: &str) -> Result<()> {
    let password_owned = password.to_string();
    run_with_spinner(term, "ollama", move || {
        local_llm::ensure_runtime(std::env::consts::OS, &SystemRunner, Some(&password_owned))
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(install_error_message(&e)))
    })
}

/// Pulls `model` into the installed runtime on a background thread, animating
/// the spinner until the pull finishes. Backs the model picker's "install on
/// select" path; no sudo is needed for [`local_llm::ensure_model`].
fn run_pull_with_spinner(term: &Term, model: &str) -> Result<()> {
    let model_owned = model.to_string();
    run_with_spinner(term, model, move || {
        local_llm::ensure_model(&SystemRunner, &model_owned)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(install_error_message(&e)))
    })
}

/// Runs `work` on a background thread, animating the install spinner labelled
/// with `subject` on the main thread until it finishes. Shared by the runtime
/// install and the model pull so both show the same progress modal.
fn run_with_spinner(
    term: &Term,
    subject: &str,
    work: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        // The receiver is only dropped once we have the result, so this send
        // cannot fail in practice; ignore the error if it somehow does.
        let _ = tx.send(work());
    });

    let mut painter = FramePainter::new();
    let mut tick = 0usize;
    let result = loop {
        let (height, width) = term.size();
        let frame = ui::installing_frame(height as usize, width as usize, subject, tick);
        let _ = painter.paint(term, frame);
        // Poll for completion on a short cadence so the spinner keeps moving.
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => tick = tick.wrapping_add(1),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("install worker stopped unexpectedly"))
            }
        }
    };
    let _ = worker.join();
    result
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
