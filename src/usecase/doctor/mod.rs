//! Health checks behind `usagi doctor`.
//!
//! Originally `doctor` only verified that the external `git`/`bash` binaries
//! were installed. usagi has since grown desktop notifications (`usagi hop`)
//! and file-based config/workspace storage, so `doctor` now also reports on
//! those subsystems.

use crate::infrastructure::storage::Storage;
use crate::usecase::local_llm;

mod fix;
mod runner;

pub use fix::{fix_missing, FixOutcome, InstallCommand, Manager};
pub use runner::{CommandRunner, SystemRunner};

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

/// Run every diagnostic and return the checks in display order.
///
/// The local LLM checks are appended only when it is enabled in the saved
/// settings — they are irrelevant (and `ollama` need not be installed) when the
/// feature is off, which is the default.
pub fn diagnose(storage: &Storage) -> Vec<Check> {
    let mut checks: Vec<Check> = REQUIRED_TOOLS
        .iter()
        .map(|&name| tool_check(name))
        .collect();
    checks.push(notification_check());
    checks.push(config_check(storage));
    if let Ok(settings) = storage.load_settings() {
        checks.extend(local_llm_checks(
            settings.local_llm.enabled,
            &settings.local_llm.model,
            &SystemRunner,
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
        Check::ok("ollama")
    } else {
        Check::missing(
            "ollama",
            "`ollama` runtime not installed; run `usagi doctor --fix`",
        )
    };
    let model_check = if local_llm::model_present(runner, model) {
        Check::ok_with("local-llm model", model.to_string())
    } else {
        Check::missing(
            "local-llm model",
            format!("model `{model}` not pulled; run `usagi doctor --fix`"),
        )
    };
    vec![ollama, model_check]
}

/// Check that an external tool is installed and runnable.
fn tool_check(name: &'static str) -> Check {
    if which(name) {
        Check::ok(name)
    } else {
        Check::missing(name, format!("`{name}` was not found on your PATH"))
    }
}

fn which(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Check that usagi's config/workspace storage is readable.
fn config_check(storage: &Storage) -> Check {
    let dir = storage.dir().display().to_string();
    match storage.load_settings() {
        Ok(_) => Check::ok_with("config", dir),
        Err(_) => Check::missing("config", format!("could not read settings under {dir}")),
    }
}

#[cfg(test)]
mod tests;
