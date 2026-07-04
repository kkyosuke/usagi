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

use crate::domain::settings::{AgentCli, LocalSettings, Settings, LOCAL_LLM_MODELS};
use crate::infrastructure::git;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::io::loading::run_with_loading;
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

    // Everything the screen needs from external subprocesses — the installed
    // agent CLIs, the repository's branches, and the local LLM presence — is
    // probed on a worker thread while the loading rabbit animates, so opening the
    // screen no longer freezes on those `--version` / `git` / `ollama` spawns
    // (see [`probe`]). Fast work (well under the loading grace period) shows
    // nothing; only a slow probe surfaces the rabbit. A panicked probe thread
    // falls back to empty results rather than crashing the screen.
    let probe_root = repo_root.clone();
    let probes = run_with_loading(term, "設定を読み込み中…", move || {
        probe(probe_root.as_deref())
    })
    .unwrap_or_default();

    let (mut config, notice) = match load(&storage, repo_root.as_deref()) {
        Ok((settings, workspaces, local)) => {
            let config = match (repo_root.as_ref(), local) {
                (Some(_root), Some(local)) => {
                    // Offer the repository's branches as Default Branch choices.
                    Config::workspace(settings, local, probes.branches)
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

    // The Agent CLI selector only offers agents the user can actually launch.
    // Both scopes render it (the global setting and the per-project override), so
    // this is set unconditionally.
    config.set_available_agent_clis(probes.clis);

    // The Local LLM row opens as an "Install" action or an on/off toggle
    // depending on whether the runtime and selected model are present. Only the
    // global scope renders that row, so [`probe`] leaves these empty for a
    // workspace scope and the row stays hidden.
    if probes.ollama_installed {
        config.set_ollama_installed(true);
        config.set_installed_models(probes.installed_models);
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

/// The results of the config screen's external-subprocess probes, gathered off
/// the UI thread by [`probe`].
#[derive(Default)]
struct Probes {
    /// Agent CLIs found on the PATH, in [`AgentCli::ALL`] order.
    clis: Vec<AgentCli>,
    /// The repository's branches (Default Branch choices); empty in the global
    /// scope, which has no repository.
    branches: Vec<String>,
    /// Whether the `ollama` runtime is installed; always `false` in a workspace
    /// scope, which does not render the Local LLM row.
    ollama_installed: bool,
    /// Which [`LOCAL_LLM_MODELS`] are already pulled locally.
    installed_models: Vec<String>,
}

/// Run the three independent, subprocess-backed probes the config screen needs
/// concurrently, so the wait is the slowest single probe rather than their sum.
///
/// - the installed agent CLIs (`<cmd> --version` per [`AgentCli::ALL`]),
/// - the repository's branches (`git`), only meaningful in a workspace scope,
/// - the local LLM presence (`ollama` runtime + each [`LOCAL_LLM_MODELS`]),
///   only rendered in the global scope (`repo_root` is `None`).
///
/// [`SystemRunner`] is a zero-sized unit struct, so each scoped thread simply
/// constructs its own. A panicked thread degrades to that probe's default
/// (empty / `false`) rather than aborting the launch.
fn probe(repo_root: Option<&std::path::Path>) -> Probes {
    std::thread::scope(|scope| {
        let clis = scope.spawn(|| agent::available_clis(&SystemRunner));
        let branches = scope.spawn(move || match repo_root {
            Some(root) => git::list_branches(root),
            None => Vec::new(),
        });
        let llm = scope.spawn(move || {
            // Only the global scope renders the Local LLM row, so skip the two
            // `ollama` probes entirely when editing a workspace's overrides.
            if repo_root.is_some() {
                return (false, Vec::new());
            }
            let runner = SystemRunner;
            if !local_llm::ollama_installed(&runner) {
                return (false, Vec::new());
            }
            let models = LOCAL_LLM_MODELS
                .iter()
                .filter(|model| local_llm::model_present(&runner, model))
                .map(|model| model.to_string())
                .collect();
            (true, models)
        });

        let clis = clis.join().unwrap_or_default();
        let branches = branches.join().unwrap_or_default();
        let (ollama_installed, installed_models) = llm.join().unwrap_or((false, Vec::new()));
        Probes {
            clis,
            branches,
            ollama_installed,
            installed_models,
        }
    })
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
