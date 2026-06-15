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
/// Returns the ordered list of steps taken on success, or the first error that
/// stopped provisioning. Idempotent — re-running when everything is already in
/// place simply reports the "already present" steps.
pub fn ensure(
    os: &str,
    runner: &dyn CommandRunner,
    model: &str,
) -> Result<Vec<SetupStep>, SetupError> {
    // The runtime must be installed before the model can be pulled, so these
    // run left-to-right; `?` short-circuits on the first failure.
    let ollama = install_ollama(os, runner)?;
    let model = pull_model(runner, model)?;
    Ok(vec![ollama, model])
}

/// Install the `ollama` runtime if it is not already present.
fn install_ollama(os: &str, runner: &dyn CommandRunner) -> Result<SetupStep, SetupError> {
    if runner.available(OLLAMA) {
        return Ok(SetupStep::OllamaAlreadyPresent);
    }
    // Homebrew (macOS) is the only package manager that ships Ollama directly;
    // elsewhere we point at the official installer rather than guess.
    if os == "macos" && runner.available("brew") {
        match runner.run("brew", &["install", "ollama"]) {
            Ok(true) => Ok(SetupStep::OllamaInstalled { manager: "brew" }),
            Ok(false) | Err(_) => Err(SetupError::OllamaInstallFailed {
                manager: "brew",
                manual: ollama_manual(),
            }),
        }
    } else {
        Err(SetupError::OllamaUnavailable {
            manual: ollama_manual(),
        })
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
        ran: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn new(available: Vec<&'static str>, run: std::io::Result<bool>) -> Self {
            Self {
                available,
                present_models: Vec::new(),
                run,
                ran: RefCell::new(Vec::new()),
            }
        }

        fn with_present_models(mut self, models: Vec<&'static str>) -> Self {
            self.present_models = models;
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
        let steps = ensure("macos", &runner, "qwen2.5-coder:7b").unwrap();
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
    fn ensure_installs_ollama_and_pulls_the_model_when_missing() {
        // ollama absent but brew present; the model is not yet pulled.
        let runner = FakeRunner::new(vec!["brew"], Ok(true));
        let steps = ensure("macos", &runner, "qwen2.5:7b").unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaInstalled { manager: "brew" },
                SetupStep::ModelPulled {
                    model: "qwen2.5:7b".to_string()
                }
            ]
        );
        assert_eq!(
            *runner.ran.borrow(),
            vec!["brew install ollama", "ollama pull qwen2.5:7b"]
        );
    }

    #[test]
    fn ensure_reports_when_ollama_cannot_be_auto_installed() {
        // No brew on macOS -> no auto-install path.
        let no_brew = FakeRunner::new(vec![], Ok(true));
        assert_eq!(
            ensure("macos", &no_brew, "qwen2.5:7b"),
            Err(SetupError::OllamaUnavailable {
                manual: ollama_manual()
            })
        );

        // Linux has no package-manager path for ollama either.
        let linux = FakeRunner::new(vec!["apt-get"], Ok(true));
        assert_eq!(
            ensure("linux", &linux, "qwen2.5:7b"),
            Err(SetupError::OllamaUnavailable {
                manual: ollama_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_ollama_install() {
        // brew present but the install command exits non-zero.
        let runner = FakeRunner::new(vec!["brew"], Ok(false));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b"),
            Err(SetupError::OllamaInstallFailed {
                manager: "brew",
                manual: ollama_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_model_pull() {
        // ollama present, model missing, and the pull fails (spawn error here).
        let runner = FakeRunner::new(vec!["ollama"], Err(std::io::Error::other("boom")));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b"),
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
