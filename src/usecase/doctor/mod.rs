//! Health checks behind `usagi doctor`.
//!
//! Originally `doctor` only verified that the external `git`/`bash` binaries
//! were installed. usagi has since grown desktop notifications (`usagi hop`)
//! and file-based config/workspace storage, so `doctor` now also reports on
//! those subsystems.

use crate::domain::settings::AgentCli;
use crate::infrastructure::storage::Storage;
use crate::usecase::font;
use crate::usecase::local_llm;

mod fix;
mod runner;

pub use fix::{fix_missing, FixOutcome, InstallCommand, Manager};
pub use runner::{CommandRunner, SystemRunner};

/// Names of the checks `usagi doctor` can install, in display order: missing
/// required tools, a missing Nerd Font, and (when enabled) the local LLM
/// runtime/model. This drives the interactive install prompt.
///
/// Excluded — they have no automatic install:
/// - `config` is created on first run, not installed (a `missing` here means
///   the settings are unreadable, which `--fix` cannot repair).
/// - The optional agent CLIs and desktop notifications report `warn`, not
///   `missing`, so they never match.
///
/// The Nerd Font is the one `warn` that *is* installable (the TUI falls back to
/// words without it, so its absence is a `warn`), so it is matched by name
/// rather than by health.
pub fn installable_gaps(checks: &[Check]) -> Vec<&'static str> {
    checks
        .iter()
        .filter(|check| is_installable_gap(check))
        .map(|check| check.name)
        .collect()
}

/// Whether a single `check` is an installable gap (see [`installable_gaps`]).
fn is_installable_gap(check: &Check) -> bool {
    match check.name {
        // Created on first run, never auto-installed.
        CONFIG_CHECK => false,
        // A missing Nerd Font is a `warn`, but the font flow can install it.
        NERD_FONT_CHECK => check.health == Health::Warn,
        // Required tools and the local-LLM runtime/model: `missing` is
        // installable. Optional agent CLIs / notifications report `warn`, so
        // they never match here.
        _ => check.health == Health::Missing,
    }
}

/// Health of a single diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Working as expected.
    Ok,
    /// Usable, but degraded or unconfigured (`doctor` still exits successfully).
    Warn,
    /// A required dependency is missing.
    Missing,
}

impl Health {
    /// Short label shown in the `doctor` output.
    pub fn label(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Warn => "warn",
            Health::Missing => "missing",
        }
    }
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub health: Health,
    /// Optional human-readable context (a path, or why something is degraded).
    pub detail: Option<String>,
}

impl Check {
    fn ok(name: &'static str) -> Self {
        Self {
            name,
            health: Health::Ok,
            detail: None,
        }
    }

    fn ok_with(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Ok,
            detail: Some(detail.into()),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Warn,
            detail: Some(detail.into()),
        }
    }

    fn missing(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Missing,
            detail: Some(detail.into()),
        }
    }
}

/// External command-line tools usagi shells out to.
const REQUIRED_TOOLS: &[&str] = &["git", "bash"];

/// Name of the Nerd Font presence check.
pub(super) const NERD_FONT_CHECK: &str = "nerd font";

/// Name of the config/workspace storage check.
pub(super) const CONFIG_CHECK: &str = "config";

/// Name of the `ollama` runtime check.
pub(super) const OLLAMA_CHECK: &str = "ollama";
/// Name of the local LLM model presence check.
pub(super) const LOCAL_LLM_MODEL_CHECK: &str = "local-llm model";

/// Diagnostic checks whose remedy is the dedicated [`local_llm::ensure`] flow
/// (driven from the CLI right after the generic fix pass), not the generic
/// package-manager install path. `doctor --fix` must skip these so it never
/// shells out `brew install "local-llm model"` and the like.
const LOCAL_LLM_CHECKS: &[&str] = &[OLLAMA_CHECK, LOCAL_LLM_MODEL_CHECK];

/// Whether `name` is a local-LLM check handled by [`local_llm::ensure`] rather
/// than the generic package-manager remediation.
pub(super) fn is_local_llm_check(name: &str) -> bool {
    LOCAL_LLM_CHECKS.contains(&name)
}

/// Run every diagnostic and return the checks in display order.
///
/// The local LLM checks are appended only when it is enabled in the saved
/// settings — they are irrelevant (and `ollama` need not be installed) when the
/// feature is off, which is the default.
pub fn diagnose(storage: &Storage) -> Vec<Check> {
    let runner = SystemRunner;
    let mut checks: Vec<Check> = REQUIRED_TOOLS
        .iter()
        .map(|&name| tool_check(name, &runner))
        .collect();
    checks.extend(agent_checks(&runner));
    checks.push(notification_check());
    checks.push(font_check());
    checks.push(config_check(storage));
    if let Ok(settings) = storage.load_settings() {
        checks.extend(local_llm_checks(
            settings.local_llm.enabled,
            &settings.local_llm.model,
            &runner,
        ));
    }
    checks
}

/// Diagnostics for the optional local LLM, or an empty list when it is
/// disabled. Reports whether the `ollama` runtime and the selected model are
/// installed; the remedy for either is `usagi doctor --fix`.
fn local_llm_checks(enabled: bool, model: &str, runner: &dyn CommandRunner) -> Vec<Check> {
    if !enabled {
        return Vec::new();
    }
    let ollama = if local_llm::ollama_installed(runner) {
        Check::ok(OLLAMA_CHECK)
    } else {
        Check::missing(
            OLLAMA_CHECK,
            "`ollama` runtime not installed; run `usagi doctor --fix`",
        )
    };
    let model_check = if local_llm::model_present(runner, model) {
        Check::ok_with(LOCAL_LLM_MODEL_CHECK, model.to_string())
    } else {
        Check::missing(
            LOCAL_LLM_MODEL_CHECK,
            format!("model `{model}` not pulled; run `usagi doctor --fix`"),
        )
    };
    vec![ollama, model_check]
}

/// One presence check per agent CLI usagi can drive, in [`AgentCli::ALL`] order.
///
/// The agents are optional — usagi only launches the one configured in settings —
/// so a missing agent is a `warn`, not a `missing`: `doctor` still exits
/// successfully and `--fix` leaves it alone (they are not generic
/// package-manager installs). The check is named by the agent's display name
/// (e.g. `sakana.ai`) and reports the launch command it probed.
fn agent_checks(runner: &dyn CommandRunner) -> Vec<Check> {
    AgentCli::ALL
        .into_iter()
        .map(|cli| agent_check(cli, runner))
        .collect()
}

/// Whether a single agent CLI's launch command is installed on the PATH.
fn agent_check(cli: AgentCli, runner: &dyn CommandRunner) -> Check {
    let command = cli.command();
    if runner.available(command) {
        Check::ok_with(cli.display_name(), command)
    } else {
        Check::warn(
            cli.display_name(),
            format!("`{command}` not found on your PATH (optional)"),
        )
    }
}

/// Check that an external tool is installed and runnable, probing through the
/// [`CommandRunner`] abstraction so the check is testable without shelling out.
fn tool_check(name: &'static str, runner: &dyn CommandRunner) -> Check {
    if runner.available(name) {
        Check::ok(name)
    } else {
        Check::missing(name, format!("`{name}` was not found on your PATH"))
    }
}

/// Check whether desktop notifications (used by `usagi hop`) can be delivered.
fn notification_check() -> Check {
    notification_check_for(
        std::env::consts::OS,
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
    )
}

/// Pure core of [`notification_check`], split out so every branch is testable
/// without depending on the host OS or environment.
fn notification_check_for(os: &str, dbus_session: bool) -> Check {
    if notifications_supported(os, dbus_session) {
        Check::ok("notifications")
    } else {
        Check::warn(
            "notifications",
            "no D-Bus session bus; `usagi hop` notifications will be skipped",
        )
    }
}

/// Whether desktop notifications are likely to work on the given platform.
///
/// macOS and Windows ship a native notification centre; on Linux/BSD
/// `notify-rust` talks to a notification daemon over the session D-Bus, so a
/// missing session bus (e.g. headless or CI) is treated as unsupported.
fn notifications_supported(os: &str, dbus_session: bool) -> bool {
    match os {
        "macos" | "ios" | "windows" => true,
        _ => dbus_session,
    }
}

/// Check whether a Nerd Font (used by the TUI's git/issue glyphs) is installed.
///
/// The font is optional — the TUI falls back to colored words without it — so a
/// missing font is a `warn`, not a `missing`: `doctor` still exits successfully.
/// Its remedy is the dedicated [`font::ensure`] flow the CLI runs during `--fix`
/// (not the generic package-manager path), so it is reported like the local-LLM
/// checks rather than routed through [`fix_missing`].
fn font_check() -> Check {
    let home = dirs::home_dir().unwrap_or_default();
    let dirs = font::font_dirs(std::env::consts::OS, &home);
    font_check_for(font::nerd_font_installed(&dirs))
}

/// Pure core of [`font_check`], split out so both branches are testable without
/// depending on the host's installed fonts.
fn font_check_for(installed: bool) -> Check {
    if installed {
        Check::ok(NERD_FONT_CHECK)
    } else {
        Check::warn(
            NERD_FONT_CHECK,
            "no Nerd Font found; run `usagi doctor --fix` to install one",
        )
    }
}

/// Check that usagi's config/workspace storage is readable.
fn config_check(storage: &Storage) -> Check {
    let dir = storage.dir().display().to_string();
    match storage.load_settings() {
        Ok(_) => Check::ok_with(CONFIG_CHECK, dir),
        Err(_) => Check::missing(CONFIG_CHECK, format!("could not read settings under {dir}")),
    }
}

#[cfg(test)]
mod tests;
