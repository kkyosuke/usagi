//! Health checks behind `usagi doctor`.
//!
//! Originally `doctor` only verified that the external `git`/`bash` binaries
//! were installed. usagi has since grown desktop notifications (`usagi hop`)
//! and file-based config/workspace storage, so `doctor` now also reports on
//! those subsystems.

use crate::infrastructure::storage::Storage;

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
pub fn diagnose(storage: &Storage) -> Vec<Check> {
    let mut checks: Vec<Check> = REQUIRED_TOOLS
        .iter()
        .map(|&name| tool_check(name))
        .collect();
    checks.push(notification_check());
    checks.push(config_check(storage));
    checks
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
mod tests {
    use super::*;

    #[test]
    fn health_labels_cover_every_variant() {
        assert_eq!(Health::Ok.label(), "ok");
        assert_eq!(Health::Warn.label(), "warn");
        assert_eq!(Health::Missing.label(), "missing");
    }

    #[test]
    fn tool_check_reports_installed_and_missing_tools() {
        let git = tool_check("git");
        assert_eq!(git.name, "git");
        assert_eq!(git.health, Health::Ok);
        assert!(git.detail.is_none());

        let missing = tool_check("definitely-not-a-real-binary-xyz");
        assert_eq!(missing.health, Health::Missing);
        assert!(missing.detail.unwrap().contains("PATH"));
    }

    #[test]
    fn notifications_supported_per_platform() {
        assert!(notifications_supported("macos", false));
        assert!(notifications_supported("windows", false));
        assert!(notifications_supported("linux", true));
        assert!(!notifications_supported("linux", false));
    }

    #[test]
    fn notification_check_for_maps_support_to_health() {
        assert_eq!(notification_check_for("macos", false).health, Health::Ok);

        let warn = notification_check_for("linux", false);
        assert_eq!(warn.health, Health::Warn);
        assert!(warn.detail.unwrap().contains("D-Bus"));
    }

    #[test]
    fn notification_check_runs_in_the_current_environment() {
        assert_eq!(notification_check().name, "notifications");
    }

    #[test]
    fn config_check_is_ok_when_settings_load() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        let check = config_check(&storage);
        assert_eq!(check.health, Health::Ok);
        assert_eq!(check.detail.unwrap(), storage.dir().display().to_string());
    }

    #[test]
    fn config_check_is_missing_when_settings_cannot_be_read() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        // A directory where `settings.json` is expected makes the read fail.
        std::fs::create_dir_all(storage.dir().join("settings.json")).unwrap();
        let check = config_check(&storage);
        assert_eq!(check.health, Health::Missing);
        assert!(check.detail.unwrap().contains("could not read settings"));
    }

    #[test]
    fn diagnose_covers_tools_notifications_and_config() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        let names: Vec<_> = diagnose(&storage).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["git", "bash", "notifications", "config"]);
    }
}
