//! `doctor --fix` remediation: package managers, install commands, and the
//! logic that maps a missing check to an install attempt or manual guidance.

use super::*;

/// A package manager `doctor --fix` knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// Homebrew (macOS).
    Brew,
    /// Debian/Ubuntu `apt-get`.
    Apt,
    /// Fedora/RHEL `dnf`.
    Dnf,
    /// Arch `pacman`.
    Pacman,
}

impl Manager {
    /// The manager's own binary, used to detect whether it is installed.
    pub(super) fn binary(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::Apt => "apt-get",
            Manager::Dnf => "dnf",
            Manager::Pacman => "pacman",
        }
    }

    /// The package managers to try for `os`, in priority order.
    pub(super) fn candidates(os: &str) -> &'static [Manager] {
        match os {
            "macos" => &[Manager::Brew],
            "linux" => &[Manager::Apt, Manager::Dnf, Manager::Pacman],
            // Unknown / unsupported platforms have no auto-install path.
            _ => &[],
        }
    }

    /// The command that installs `tool` through this manager. System managers
    /// are prefixed with `sudo`, since installing a package needs root.
    pub(super) fn install(self, tool: &str) -> InstallCommand {
        match self {
            Manager::Brew => InstallCommand::new("brew", &["install", tool]),
            Manager::Apt => InstallCommand::new("sudo", &["apt-get", "install", "-y", tool]),
            Manager::Dnf => InstallCommand::new("sudo", &["dnf", "install", "-y", tool]),
            Manager::Pacman => InstallCommand::new("sudo", &["pacman", "-S", "--noconfirm", tool]),
        }
    }
}

/// A concrete command line to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl InstallCommand {
    pub(super) fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// The outcome of attempting to fix one missing tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// The tool was installed successfully via `manager`.
    Installed { tool: String, manager: &'static str },
    /// An install was attempted via `manager` but failed; `manual` says what
    /// to do by hand.
    Failed {
        tool: String,
        manager: &'static str,
        manual: String,
    },
    /// No package manager was available; only `manual` steps are offered.
    Manual { tool: String, manual: String },
}

/// Attempt to fix every missing tool in `checks`, returning one outcome per
/// `Missing` check in check order. `Ok`/`Warn` checks are skipped.
pub fn fix_missing(checks: &[Check], os: &str, runner: &dyn CommandRunner) -> Vec<FixOutcome> {
    checks
        .iter()
        .filter(|check| check.health == Health::Missing)
        .map(|check| fix_one(check.name, os, runner))
        .collect()
}

/// Try to install a single `tool`, falling back to manual instructions.
pub(super) fn fix_one(tool: &str, os: &str, runner: &dyn CommandRunner) -> FixOutcome {
    match detect_manager(os, runner) {
        Some(manager) => {
            let command = manager.install(tool);
            let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
            match runner.run(&command.program, &args) {
                Ok(true) => FixOutcome::Installed {
                    tool: tool.to_string(),
                    manager: manager.binary(),
                },
                // A non-zero exit or a spawn error both mean "couldn't install".
                Ok(false) | Err(_) => FixOutcome::Failed {
                    tool: tool.to_string(),
                    manager: manager.binary(),
                    manual: manual_hint(tool),
                },
            }
        }
        None => FixOutcome::Manual {
            tool: tool.to_string(),
            manual: manual_hint(tool),
        },
    }
}

/// The first available package manager for `os`, if any.
pub(super) fn detect_manager(os: &str, runner: &dyn CommandRunner) -> Option<Manager> {
    Manager::candidates(os)
        .iter()
        .copied()
        .find(|manager| runner.available(manager.binary()))
}

/// Human-readable manual install guidance for `tool`.
pub(super) fn manual_hint(tool: &str) -> String {
    let source = match tool {
        "git" => "https://git-scm.com/downloads",
        "bash" => "https://www.gnu.org/software/bash/",
        _ => "your platform's package manager",
    };
    format!("install `{tool}` manually ({source})")
}
