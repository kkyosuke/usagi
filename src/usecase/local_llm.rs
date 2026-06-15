//! Provisioning for the optional local LLM the agent offloads work to.
//!
//! usagi delegates light tasks to a local model served through Ollama (see the
//! `usagi llm-mcp` server). This module decides whether the required materials —
//! the `ollama` runtime and the selected model — are present, and installs the
//! missing ones on request. It never runs on its own: the user opts in via the
//! config screen or `usagi doctor --fix`.
//!
//! All command execution goes through the shared [`CommandRunner`] abstraction
//! (defined alongside `doctor`'s remediation), so the logic here is exercised
//! with a fake runner and never shells out during tests.

use crate::usecase::doctor::CommandRunner;

/// The Ollama runtime binary usagi drives.
const OLLAMA: &str = "ollama";

/// How [`SetupStep::OllamaInstalled`] labels the install path: the official
/// one-line installer rather than a system package manager.
const INSTALLER: &str = "ollama.com/install.sh";

/// The official Ollama install command, run through a shell so the pipe works.
/// It supports both macOS and Linux and elevates with `sudo` itself where
/// needed (pre-authenticated by [`ensure`] when a password is supplied).
const INSTALL_SCRIPT: &str = "curl -fsSL https://ollama.com/install.sh | sh";

/// Whether the `ollama` runtime is installed and runnable.
pub fn ollama_installed(runner: &dyn CommandRunner) -> bool {
    runner.available(OLLAMA)
}

/// Whether `model` has already been pulled locally.
///
/// Probed with `ollama show <model>`, which exits zero only when the model is
/// present; its output is suppressed since this is a silent capability check.
pub fn model_present(runner: &dyn CommandRunner, model: &str) -> bool {
    runner.check(OLLAMA, &["show", model])
}

/// One thing [`ensure`] did (or found already done) while provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    /// The `ollama` runtime was already installed.
    OllamaAlreadyPresent,
    /// The `ollama` runtime was installed during this run.
    OllamaInstalled { manager: &'static str },
    /// The model was already pulled.
    ModelAlreadyPresent { model: String },
    /// The model was pulled during this run.
    ModelPulled { model: String },
}

/// Why [`ensure`] could not finish provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// `ollama` is missing and there is no automatic install path on this OS.
    OllamaUnavailable { manual: String },
    /// An `ollama` install was attempted but failed; `manual` says what to do
    /// by hand.
    OllamaInstallFailed {
        manager: &'static str,
        manual: String,
    },
    /// `ollama pull <model>` failed.
    ModelPullFailed { model: String },
}

/// Ensure the local LLM is ready to use for `model`: install `ollama` if it is
/// missing, then pull the model if it is not already present.
///
/// `sudo` carries the password used to pre-authenticate the privileged steps of
/// the runtime installer non-interactively: `Some(password)` (the TUI install
/// flow) caches sudo credentials up front so nothing prompts mid-install;
/// `None` (the `usagi doctor --fix` CLI) lets the installer prompt on the
/// terminal as usual.
///
/// Returns the ordered list of steps taken on success, or the first error that
/// stopped provisioning. Idempotent — re-running when everything is already in
/// place simply reports the "already present" steps.
pub fn ensure(
    os: &str,
    runner: &dyn CommandRunner,
    model: &str,
    sudo: Option<&str>,
) -> Result<Vec<SetupStep>, SetupError> {
    // The runtime must be installed before the model can be pulled, so these
    // run left-to-right; `?` short-circuits on the first failure.
    let ollama = install_ollama(os, runner, sudo)?;
    let model = pull_model(runner, model)?;
    Ok(vec![ollama, model])
}

/// Install the `ollama` runtime if it is not already present, using the
/// official installer. `sudo` pre-authenticates the privileged steps when set.
fn install_ollama(
    os: &str,
    runner: &dyn CommandRunner,
    sudo: Option<&str>,
) -> Result<SetupStep, SetupError> {
    if runner.available(OLLAMA) {
        return Ok(SetupStep::OllamaAlreadyPresent);
    }
    // The official installer only supports macOS and Linux; elsewhere there is
    // no automatic path, so point at the manual download instead of guessing.
    if os != "macos" && os != "linux" {
        return Err(SetupError::OllamaUnavailable {
            manual: ollama_manual(),
        });
    }
    let install_failed = || SetupError::OllamaInstallFailed {
        manager: INSTALLER,
        manual: ollama_manual(),
    };
    // Caching sudo credentials first (reading the password from stdin) lets the
    // installer's privileged steps run unattended. A failure here means the
    // password was wrong or sudo is unavailable, so stop before installing.
    if let Some(password) = sudo {
        if !runner
            .run_with_input("sudo", &["-S", "-v"], password)
            .unwrap_or(false)
        {
            return Err(install_failed());
        }
    }
    match runner.run("sh", &["-c", INSTALL_SCRIPT]) {
        Ok(true) => Ok(SetupStep::OllamaInstalled { manager: INSTALLER }),
        Ok(false) | Err(_) => Err(install_failed()),
    }
}

/// Pull `model` if it is not already present.
fn pull_model(runner: &dyn CommandRunner, model: &str) -> Result<SetupStep, SetupError> {
    if model_present(runner, model) {
        return Ok(SetupStep::ModelAlreadyPresent {
            model: model.to_string(),
        });
    }
    match runner.run(OLLAMA, &["pull", model]) {
        Ok(true) => Ok(SetupStep::ModelPulled {
            model: model.to_string(),
        }),
        Ok(false) | Err(_) => Err(SetupError::ModelPullFailed {
            model: model.to_string(),
        }),
    }
}

/// Manual install guidance for the `ollama` runtime.
pub fn ollama_manual() -> String {
    "install Ollama from https://ollama.com/download".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A configurable [`CommandRunner`] that records the commands it ran, so a
    /// test can assert both the outcome and that the right commands fired.
    struct FakeRunner {
        available: Vec<&'static str>,
        /// Models the fake reports as already pulled (matched on `show <model>`).
        present_models: Vec<&'static str>,
        /// Result returned by `run` (install / pull).
        run: std::io::Result<bool>,
        /// Result returned by the sudo pre-authentication (`run_with_input`).
        sudo: std::io::Result<bool>,
        ran: RefCell<Vec<String>>,
        /// The input piped to the last `run_with_input` call (the password).
        piped: RefCell<Option<String>>,
    }

    impl FakeRunner {
        fn new(available: Vec<&'static str>, run: std::io::Result<bool>) -> Self {
            Self {
                available,
                present_models: Vec::new(),
                run,
                sudo: Ok(true),
                ran: RefCell::new(Vec::new()),
                piped: RefCell::new(None),
            }
        }

        fn with_present_models(mut self, models: Vec<&'static str>) -> Self {
            self.present_models = models;
            self
        }

        fn with_sudo(mut self, sudo: std::io::Result<bool>) -> Self {
            self.sudo = sudo;
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.available.contains(&program)
        }

        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match &self.run {
                Ok(ok) => Ok(*ok),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn run_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &str,
        ) -> std::io::Result<bool> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            *self.piped.borrow_mut() = Some(input.to_string());
            match &self.sudo {
                Ok(ok) => Ok(*ok),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn check(&self, _program: &str, args: &[&str]) -> bool {
            // Mimics `ollama show <model>`: succeeds only for known models.
            args.last()
                .is_some_and(|model| self.present_models.contains(model))
        }
    }

    #[test]
    fn ollama_installed_reflects_availability() {
        assert!(ollama_installed(&FakeRunner::new(vec!["ollama"], Ok(true))));
        assert!(!ollama_installed(&FakeRunner::new(vec![], Ok(true))));
    }

    #[test]
    fn model_present_uses_the_show_probe() {
        let runner =
            FakeRunner::new(vec!["ollama"], Ok(true)).with_present_models(vec!["qwen2.5-coder:7b"]);
        assert!(model_present(&runner, "qwen2.5-coder:7b"));
        assert!(!model_present(&runner, "qwen2.5:7b"));
    }

    #[test]
    fn ensure_is_a_no_op_when_everything_is_present() {
        let runner =
            FakeRunner::new(vec!["ollama"], Ok(true)).with_present_models(vec!["qwen2.5-coder:7b"]);
        let steps = ensure("macos", &runner, "qwen2.5-coder:7b", None).unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaAlreadyPresent,
                SetupStep::ModelAlreadyPresent {
                    model: "qwen2.5-coder:7b".to_string()
                }
            ]
        );
        // Nothing was installed or pulled.
        assert!(runner.ran.borrow().is_empty());
    }

    #[test]
    fn ensure_installs_ollama_via_the_official_script_and_pulls_the_model() {
        // ollama absent and no sudo password supplied (the CLI path): the
        // installer runs directly, then the model is pulled.
        let runner = FakeRunner::new(vec![], Ok(true));
        let steps = ensure("linux", &runner, "qwen2.5:7b", None).unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaInstalled { manager: INSTALLER },
                SetupStep::ModelPulled {
                    model: "qwen2.5:7b".to_string()
                }
            ]
        );
        assert_eq!(
            *runner.ran.borrow(),
            vec![
                format!("sh -c {INSTALL_SCRIPT}"),
                "ollama pull qwen2.5:7b".to_string(),
            ]
        );
        // No sudo pre-authentication without a password.
        assert!(runner.piped.borrow().is_none());
    }

    #[test]
    fn ensure_preauthenticates_sudo_when_a_password_is_supplied() {
        // The TUI path hands over a password: sudo is validated first (reading
        // the password from stdin), then the installer and pull run.
        let runner = FakeRunner::new(vec![], Ok(true));
        let steps = ensure("macos", &runner, "qwen2.5:7b", Some("hunter2")).unwrap();
        assert_eq!(steps[0], SetupStep::OllamaInstalled { manager: INSTALLER });
        assert_eq!(
            *runner.ran.borrow(),
            vec![
                "sudo -S -v".to_string(),
                format!("sh -c {INSTALL_SCRIPT}"),
                "ollama pull qwen2.5:7b".to_string(),
            ]
        );
        assert_eq!(runner.piped.borrow().as_deref(), Some("hunter2"));
    }

    #[test]
    fn ensure_reports_when_the_os_has_no_installer() {
        // The official installer supports only macOS and Linux.
        let runner = FakeRunner::new(vec![], Ok(true));
        assert_eq!(
            ensure("windows", &runner, "qwen2.5:7b", None),
            Err(SetupError::OllamaUnavailable {
                manual: ollama_manual()
            })
        );
        // Nothing was attempted on an unsupported OS.
        assert!(runner.ran.borrow().is_empty());
    }

    #[test]
    fn ensure_stops_when_sudo_preauthentication_fails() {
        // A wrong password (sudo exits non-zero) aborts before the installer.
        let runner = FakeRunner::new(vec![], Ok(true)).with_sudo(Ok(false));
        assert_eq!(
            ensure("linux", &runner, "qwen2.5:7b", Some("wrong")),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
        // The installer never ran; only the failed sudo validation did.
        assert_eq!(*runner.ran.borrow(), vec!["sudo -S -v".to_string()]);
    }

    #[test]
    fn ensure_stops_when_sudo_validation_errors() {
        // sudo itself fails to spawn (an I/O error rather than a clean non-zero
        // exit): treated like a failed install, and the installer never runs.
        let runner =
            FakeRunner::new(vec![], Ok(true)).with_sudo(Err(std::io::Error::other("no sudo")));
        assert_eq!(
            ensure("linux", &runner, "qwen2.5:7b", Some("pw")),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
        assert_eq!(*runner.ran.borrow(), vec!["sudo -S -v".to_string()]);
    }

    #[test]
    fn ensure_reports_a_failed_ollama_install() {
        // The installer script exits non-zero.
        let runner = FakeRunner::new(vec![], Ok(false));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b", None),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_model_pull() {
        // ollama present, model missing, and the pull fails (spawn error here).
        let runner = FakeRunner::new(vec!["ollama"], Err(std::io::Error::other("boom")));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b", None),
            Err(SetupError::ModelPullFailed {
                model: "qwen2.5:7b".to_string()
            })
        );
    }

    #[test]
    fn ollama_manual_points_at_the_official_download() {
        assert!(ollama_manual().contains("ollama.com"));
    }
}
