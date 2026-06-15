//! Health checks behind `usagi doctor`.
//!
//! Originally `doctor` only verified that the external `git`/`bash` binaries
//! were installed. usagi has since grown desktop notifications (`usagi hop`)
//! and file-based config/workspace storage, so `doctor` now also reports on
//! those subsystems.

use crate::infrastructure::storage::Storage;
use crate::usecase::local_llm;

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

// --- `doctor --fix`: remediate missing dependencies -----------------------

/// Runs external commands on behalf of `doctor --fix`.
///
/// Abstracted behind a trait so the remediation logic can be tested without
/// shelling out to a real package manager. Production code uses
/// [`SystemRunner`]; tests inject a fake.
pub trait CommandRunner {
    /// Whether `program` is available on the PATH (checked via `--version`,
    /// output suppressed).
    fn available(&self, program: &str) -> bool;

    /// Run an install command (`program args...`), returning whether it
    /// exited successfully. Its output is shown to the user.
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool>;

    /// Run `program args...`, feeding `input` to its standard input, returning
    /// whether it exited successfully. Used to hand a command a secret it must
    /// not appear on the process's argument list — notably the sudo password
    /// piped to `sudo -S`. The default delegates to [`run`](Self::run)
    /// (ignoring the input), which is all a test fake needs; the real runner
    /// overrides it to actually pipe the bytes.
    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> std::io::Result<bool> {
        let _ = input;
        self.run(program, args)
    }

    /// Run `program args...` quietly (stdout/stderr suppressed), returning
    /// whether it exited successfully. Used for capability probes — e.g.
    /// "is this Ollama model already pulled?" — where the command's own output
    /// should not reach the user.
    fn check(&self, program: &str, args: &[&str]) -> bool;
}

/// The production [`CommandRunner`], backed by [`std::process::Command`].
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn available(&self, program: &str) -> bool {
        which(program)
    }

    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        // Inherit stdio so the user sees the package manager's progress.
        std::process::Command::new(program)
            .args(args)
            .status()
            .map(|status| status.success())
    }

    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> std::io::Result<bool> {
        use std::io::Write as _;
        // Pipe the input (e.g. the sudo password) on stdin so it never reaches
        // the argument list; stdout/stderr stay inherited so progress shows.
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A trailing newline so the reading program (sudo) treats it as a
            // complete line. Dropping the handle afterwards closes the pipe, so
            // a reader waiting on EOF (e.g. `cat`) does not block `wait`. A write
            // failure (the child already exited) is ignored: `wait` then reports
            // the command's own non-zero exit.
            let _ = writeln!(stdin, "{input}");
        }
        child.wait().map(|status| status.success())
    }

    fn check(&self, program: &str, args: &[&str]) -> bool {
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

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
    fn binary(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::Apt => "apt-get",
            Manager::Dnf => "dnf",
            Manager::Pacman => "pacman",
        }
    }

    /// The package managers to try for `os`, in priority order.
    fn candidates(os: &str) -> &'static [Manager] {
        match os {
            "macos" => &[Manager::Brew],
            "linux" => &[Manager::Apt, Manager::Dnf, Manager::Pacman],
            // Unknown / unsupported platforms have no auto-install path.
            _ => &[],
        }
    }

    /// The command that installs `tool` through this manager. System managers
    /// are prefixed with `sudo`, since installing a package needs root.
    fn install(self, tool: &str) -> InstallCommand {
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
    fn new(program: &str, args: &[&str]) -> Self {
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
fn fix_one(tool: &str, os: &str, runner: &dyn CommandRunner) -> FixOutcome {
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
fn detect_manager(os: &str, runner: &dyn CommandRunner) -> Option<Manager> {
    Manager::candidates(os)
        .iter()
        .copied()
        .find(|manager| runner.available(manager.binary()))
}

/// Human-readable manual install guidance for `tool`.
fn manual_hint(tool: &str) -> String {
    let source = match tool {
        "git" => "https://git-scm.com/downloads",
        "bash" => "https://www.gnu.org/software/bash/",
        _ => "your platform's package manager",
    };
    format!("install `{tool}` manually ({source})")
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
        // The local LLM is off by default, so its checks are not appended.
        let names: Vec<_> = diagnose(&storage).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["git", "bash", "notifications", "config"]);
    }

    #[test]
    fn diagnose_skips_local_llm_when_settings_cannot_be_read() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        // A directory where `settings.json` is expected makes the load fail, so
        // diagnose cannot know whether the local LLM is on and skips its checks.
        std::fs::create_dir_all(storage.dir().join("settings.json")).unwrap();
        let names: Vec<_> = diagnose(&storage).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["git", "bash", "notifications", "config"]);
    }

    #[test]
    fn local_llm_checks_are_empty_when_disabled() {
        let runner = FakeRunner::new(vec![], Ok(true));
        assert!(local_llm_checks(false, "qwen2.5-coder:7b", &runner).is_empty());
    }

    #[test]
    fn local_llm_checks_report_ollama_and_model_presence() {
        // Both present: ollama available and the model probe (run -> Ok(true))
        // succeeds.
        let ready = FakeRunner::new(vec!["ollama"], Ok(true));
        let checks = local_llm_checks(true, "qwen2.5-coder:7b", &ready);
        assert_eq!(checks[0].name, "ollama");
        assert_eq!(checks[0].health, Health::Ok);
        assert_eq!(checks[1].name, "local-llm model");
        assert_eq!(checks[1].health, Health::Ok);
        assert_eq!(checks[1].detail.as_deref(), Some("qwen2.5-coder:7b"));

        // Neither present: ollama missing and the probe fails.
        let missing = FakeRunner::new(vec![], Ok(false));
        let checks = local_llm_checks(true, "qwen2.5-coder:7b", &missing);
        assert_eq!(checks[0].health, Health::Missing);
        assert!(checks[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("doctor --fix"));
        assert_eq!(checks[1].health, Health::Missing);
        assert!(checks[1].detail.as_deref().unwrap().contains("not pulled"));
    }

    // --- `doctor --fix` ----------------------------------------------------

    /// A configurable [`CommandRunner`] for testing remediation without
    /// touching a real package manager.
    struct FakeRunner {
        available: Vec<&'static str>,
        run: std::io::Result<bool>,
    }

    impl FakeRunner {
        fn new(available: Vec<&'static str>, run: std::io::Result<bool>) -> Self {
            Self { available, run }
        }
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.available.contains(&program)
        }

        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            match &self.run {
                Ok(ok) => Ok(*ok),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            // A probe succeeds when the configured run result is a clean exit.
            matches!(self.run, Ok(true))
        }
    }

    fn missing(name: &'static str) -> Check {
        Check::missing(name, "not found")
    }

    #[test]
    fn run_with_input_defaults_to_run_ignoring_the_input() {
        // The trait's default delegates to `run` and discards the piped input,
        // so a runner that does not override it mirrors its `run` result.
        let ok = FakeRunner::new(vec![], Ok(true));
        assert!(ok.run_with_input("sudo", &["-S", "-v"], "secret").unwrap());
        let fail = FakeRunner::new(vec![], Ok(false));
        assert!(!fail
            .run_with_input("sudo", &["-S", "-v"], "secret")
            .unwrap());
    }

    #[test]
    fn manager_binaries_and_candidates_per_os() {
        assert_eq!(Manager::Brew.binary(), "brew");
        assert_eq!(Manager::Apt.binary(), "apt-get");
        assert_eq!(Manager::Dnf.binary(), "dnf");
        assert_eq!(Manager::Pacman.binary(), "pacman");

        assert_eq!(Manager::candidates("macos"), &[Manager::Brew]);
        assert_eq!(
            Manager::candidates("linux"),
            &[Manager::Apt, Manager::Dnf, Manager::Pacman]
        );
        assert!(Manager::candidates("freebsd").is_empty());
    }

    #[test]
    fn install_commands_use_sudo_for_system_managers() {
        assert_eq!(
            Manager::Brew.install("git"),
            InstallCommand::new("brew", &["install", "git"])
        );
        assert_eq!(
            Manager::Apt.install("git"),
            InstallCommand::new("sudo", &["apt-get", "install", "-y", "git"])
        );
        assert_eq!(
            Manager::Dnf.install("git"),
            InstallCommand::new("sudo", &["dnf", "install", "-y", "git"])
        );
        assert_eq!(
            Manager::Pacman.install("git"),
            InstallCommand::new("sudo", &["pacman", "-S", "--noconfirm", "git"])
        );
    }

    #[test]
    fn detect_manager_picks_the_first_available_in_priority_order() {
        // dnf is available but apt is not: skip apt, pick dnf.
        let runner = FakeRunner::new(vec!["dnf"], Ok(true));
        assert_eq!(detect_manager("linux", &runner), Some(Manager::Dnf));

        // Nothing available -> no manager.
        let none = FakeRunner::new(vec![], Ok(true));
        assert_eq!(detect_manager("linux", &none), None);

        // OS with no candidates -> no manager even if a binary exists.
        let brew = FakeRunner::new(vec!["brew"], Ok(true));
        assert_eq!(detect_manager("freebsd", &brew), None);
    }

    #[test]
    fn fix_one_installs_when_a_manager_succeeds() {
        let runner = FakeRunner::new(vec!["brew"], Ok(true));
        assert_eq!(
            fix_one("git", "macos", &runner),
            FixOutcome::Installed {
                tool: "git".to_string(),
                manager: "brew",
            }
        );
    }

    #[test]
    fn fix_one_reports_failure_on_nonzero_exit_or_spawn_error() {
        // Non-zero exit.
        let failed = FakeRunner::new(vec!["brew"], Ok(false));
        assert!(matches!(
            fix_one("git", "macos", &failed),
            FixOutcome::Failed {
                manager: "brew",
                ..
            }
        ));

        // Spawn error.
        let errored = FakeRunner::new(vec!["brew"], Err(std::io::Error::other("boom")));
        assert_eq!(
            fix_one("bash", "macos", &errored),
            FixOutcome::Failed {
                tool: "bash".to_string(),
                manager: "brew",
                manual: manual_hint("bash"),
            }
        );
    }

    #[test]
    fn fix_one_falls_back_to_manual_without_a_manager() {
        let runner = FakeRunner::new(vec![], Ok(true));
        assert_eq!(
            fix_one("git", "linux", &runner),
            FixOutcome::Manual {
                tool: "git".to_string(),
                manual: manual_hint("git"),
            }
        );
    }

    #[test]
    fn fix_missing_only_acts_on_missing_checks() {
        let checks = vec![
            Check::ok("git"),
            missing("bash"),
            Check::warn("notifications", "degraded"),
        ];
        let runner = FakeRunner::new(vec!["brew"], Ok(true));
        let outcomes = fix_missing(&checks, "macos", &runner);
        assert_eq!(
            outcomes,
            vec![FixOutcome::Installed {
                tool: "bash".to_string(),
                manager: "brew",
            }]
        );
    }

    #[test]
    fn manual_hint_links_known_tools_and_falls_back_otherwise() {
        assert!(manual_hint("git").contains("git-scm.com"));
        assert!(manual_hint("bash").contains("gnu.org"));
        let other = manual_hint("node");
        assert!(other.contains("node"));
        assert!(other.contains("package manager"));
    }

    #[test]
    fn system_runner_detects_and_executes_real_commands() {
        let runner = SystemRunner;
        // `git` is available in the test environment; a bogus binary is not.
        assert!(runner.available("git"));
        assert!(!runner.available("definitely-not-a-real-binary-xyz"));

        // Running an installed tool succeeds; a missing program errors out.
        assert!(runner.run("git", &["--version"]).unwrap());
        assert!(runner.run("definitely-not-a-real-binary-xyz", &[]).is_err());

        // Feeding input on stdin: `cat` consumes it and exits cleanly, while a
        // missing binary still errors out before anything is piped.
        assert!(runner.run_with_input("cat", &[], "secret").unwrap());
        assert!(runner
            .run_with_input("definitely-not-a-real-binary-xyz", &[], "secret")
            .is_err());

        // A quiet probe returns true for a clean exit and false otherwise
        // (a non-zero exit or a missing binary).
        assert!(runner.check("git", &["--version"]));
        assert!(!runner.check("git", &["--no-such-flag-zzz"]));
        assert!(!runner.check("definitely-not-a-real-binary-xyz", &[]));
    }
}
