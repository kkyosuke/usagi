use super::fix::{detect_manager, fix_one, manual_hint};
use super::*;
use crate::domain::settings::AgentCli;

#[test]
fn health_labels_cover_every_variant() {
    assert_eq!(Health::Ok.label(), "ok");
    assert_eq!(Health::Warn.label(), "warn");
    assert_eq!(Health::Missing.label(), "missing");
}

#[test]
fn tool_check_reports_installed_and_missing_tools() {
    let git = tool_check("git", &SystemRunner);
    assert_eq!(git.name, "git");
    assert_eq!(git.health, Health::Ok);
    assert!(git.detail.is_none());

    let missing = tool_check("definitely-not-a-real-binary-xyz", &SystemRunner);
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
fn font_check_for_maps_presence_to_health() {
    assert_eq!(font_check_for(true).health, Health::Ok);

    let warn = font_check_for(false);
    assert_eq!(warn.name, "nerd font");
    assert_eq!(warn.health, Health::Warn);
    assert!(warn.detail.unwrap().contains("doctor --fix"));
}

#[test]
fn font_check_runs_in_the_current_environment() {
    assert_eq!(font_check().name, "nerd font");
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
fn agent_check_reports_presence_as_ok_or_warn() {
    // An installed agent is `ok` with its launch command as the detail; a missing
    // one is `warn` (optional — doctor still exits 0), keyed by its display name.
    let runner = FakeRunner::new(vec!["codex-fugu"], Ok(true));

    let present = agent_check(AgentCli::CodexFugu, &runner);
    assert_eq!(present.name, "sakana.ai");
    assert_eq!(present.health, Health::Ok);
    assert_eq!(present.detail.as_deref(), Some("codex-fugu"));

    let absent = agent_check(AgentCli::Claude, &runner);
    assert_eq!(absent.name, "Claude");
    assert_eq!(absent.health, Health::Warn);
    assert!(absent.detail.unwrap().contains("not found"));
}

#[test]
fn agent_checks_cover_every_agent_in_canonical_order() {
    let runner = FakeRunner::new(vec![], Ok(true));
    let names: Vec<_> = agent_checks(&runner).into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["Claude", "Codex", "sakana.ai", "Gemini"]);
}

#[test]
fn diagnose_covers_tools_agents_notifications_and_config() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let storage = Storage::new(dir.path().join("usagi"));
    // The local LLM is off by default, so its checks are not appended. The four
    // agent presence checks sit between the required tools and notifications.
    let names: Vec<_> = diagnose(&storage).into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "git",
            "bash",
            "Claude",
            "Codex",
            "sakana.ai",
            "Gemini",
            "notifications",
            "nerd font",
            "config"
        ]
    );
}

#[test]
fn diagnose_skips_local_llm_when_settings_cannot_be_read() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let storage = Storage::new(dir.path().join("usagi"));
    // A directory where `settings.json` is expected makes the load fail, so
    // diagnose cannot know whether the local LLM is on and skips its checks.
    std::fs::create_dir_all(storage.dir().join("settings.json")).unwrap();
    let names: Vec<_> = diagnose(&storage).into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "git",
            "bash",
            "Claude",
            "Codex",
            "sakana.ai",
            "Gemini",
            "notifications",
            "nerd font",
            "config"
        ]
    );
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

    fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
        // No background daemons are needed to exercise the remediation
        // logic, so the fake treats every spawn as a clean launch.
        Ok(())
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
fn fake_runner_spawn_is_inert() {
    // The remediation logic never starts a daemon, so the fake's `spawn`
    // (required by the trait, exercised for real in `local_llm`) is a
    // no-op; assert it stays one.
    let runner = FakeRunner::new(vec![], Ok(true));
    assert!(runner.spawn("ollama", &["serve"]).is_ok());
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
fn install_commands_use_non_interactive_sudo_for_system_managers() {
    assert_eq!(
        Manager::Brew.install("git"),
        InstallCommand::new("brew", &["install", "git"])
    );
    assert_eq!(
        Manager::Apt.install("git"),
        InstallCommand::new("sudo", &["-n", "apt-get", "install", "-y", "git"])
    );
    assert_eq!(
        Manager::Dnf.install("git"),
        InstallCommand::new("sudo", &["-n", "dnf", "install", "-y", "git"])
    );
    assert_eq!(
        Manager::Pacman.install("git"),
        InstallCommand::new("sudo", &["-n", "pacman", "-S", "--noconfirm", "git"])
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
fn fix_missing_skips_local_llm_checks() {
    // The `ollama` and model checks have a dedicated remedy (`local_llm::ensure`,
    // run separately by the CLI). They are not real package names, so routing
    // them through the package manager would spuriously fail; `fix_missing` must
    // skip them and only act on externally-installable tools.
    let checks = vec![
        missing("git"),
        missing(OLLAMA_CHECK),
        missing(LOCAL_LLM_MODEL_CHECK),
    ];
    // `brew` is available and every run succeeds; if the LLM checks leaked
    // through, we'd see install outcomes for them too.
    let runner = FakeRunner::new(vec!["brew"], Ok(true));
    let outcomes = fix_missing(&checks, "macos", &runner);
    assert_eq!(
        outcomes,
        vec![FixOutcome::Installed {
            tool: "git".to_string(),
            manager: "brew",
        }]
    );
}

#[test]
fn fix_missing_skips_the_config_check() {
    // A `missing` config means unreadable settings (config is created on first
    // run, not installed), so `brew install config` would be nonsense. Only the
    // genuinely installable `git` should produce an outcome.
    let checks = vec![missing("git"), missing(CONFIG_CHECK)];
    let runner = FakeRunner::new(vec!["brew"], Ok(true));
    let outcomes = fix_missing(&checks, "macos", &runner);
    assert_eq!(
        outcomes,
        vec![FixOutcome::Installed {
            tool: "git".to_string(),
            manager: "brew",
        }]
    );
}

#[test]
fn is_local_llm_check_matches_only_the_llm_checks() {
    assert!(is_local_llm_check(OLLAMA_CHECK));
    assert!(is_local_llm_check(LOCAL_LLM_MODEL_CHECK));
    assert!(!is_local_llm_check("git"));
    assert!(!is_local_llm_check("bash"));
}

#[test]
fn installable_gaps_lists_only_installable_missing_items() {
    // A missing required tool and a missing Nerd Font (a `warn`) are installable;
    // a healthy tool, an optional agent CLI (`warn`), an unreadable config
    // (`missing`), and a present font are all excluded.
    let checks = vec![
        missing("git"),
        Check::ok("bash"),
        Check::warn("sakana.ai", "`codex-fugu` not found (optional)"),
        Check::warn(NERD_FONT_CHECK, "no Nerd Font found"),
        missing(CONFIG_CHECK),
        missing(OLLAMA_CHECK),
    ];
    // Required tool, Nerd Font, and the local-LLM runtime are gaps; the rest are
    // not. Order follows the input (display) order.
    assert_eq!(
        installable_gaps(&checks),
        vec!["git", NERD_FONT_CHECK, OLLAMA_CHECK]
    );
}

#[test]
fn installable_gaps_treats_a_present_font_and_healthy_checks_as_no_gap() {
    let checks = vec![Check::ok("git"), Check::ok(NERD_FONT_CHECK)];
    assert!(installable_gaps(&checks).is_empty());
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

    // The quiet variant behaves the same on the exit code, only suppressing the
    // command's output (which a TUI install relies on to keep the screen clean).
    assert!(runner.run_quiet("git", &["--version"]).unwrap());
    assert!(runner
        .run_quiet("definitely-not-a-real-binary-xyz", &[])
        .is_err());

    // Feeding input on stdin: `cat` consumes it and exits cleanly, while a
    // missing binary still errors out before anything is piped. The quiet
    // variant pipes the input the same way, with output discarded.
    assert!(runner.run_with_input("cat", &[], "secret").unwrap());
    assert!(runner
        .run_with_input("definitely-not-a-real-binary-xyz", &[], "secret")
        .is_err());
    assert!(runner.run_with_input_quiet("cat", &[], "secret").unwrap());
    assert!(runner
        .run_with_input_quiet("definitely-not-a-real-binary-xyz", &[], "secret")
        .is_err());

    // A quiet probe returns true for a clean exit and false otherwise
    // (a non-zero exit or a missing binary).
    assert!(runner.check("git", &["--version"]));
    assert!(!runner.check("git", &["--no-such-flag-zzz"]));
    assert!(!runner.check("definitely-not-a-real-binary-xyz", &[]));

    // Spawning a real binary launches it without waiting; a missing
    // program surfaces the spawn error.
    assert!(runner.spawn("git", &["--version"]).is_ok());
    assert!(runner
        .spawn("definitely-not-a-real-binary-xyz", &[])
        .is_err());
}
