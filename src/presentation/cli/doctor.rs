use crate::domain::settings::LocalLlm;
use crate::infrastructure::storage::Storage;
use crate::usecase::doctor::{
    diagnose, fix_missing, Check, CommandRunner, FixOutcome, SystemRunner,
};
use crate::usecase::local_llm::{self, SetupError, SetupStep};

/// Entry point for `usagi doctor`. With `fix`, attempts to install missing
/// tools (or prints manual steps); otherwise just prints the diagnostics.
pub fn run(fix: bool) -> anyhow::Result<()> {
    let storage = Storage::open_default()?;
    let checks = diagnose(&storage);
    let lines = if fix {
        // Fall back to defaults (local LLM off) if settings cannot be read.
        let local_llm = storage
            .load_settings()
            .map(|s| s.local_llm)
            .unwrap_or_default();
        fix_lines(&checks, std::env::consts::OS, &local_llm, &SystemRunner)
    } else {
        render(&checks)
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// The lines printed by `usagi doctor --fix`: the standard tool remediation,
/// followed by local LLM provisioning when it is enabled. Pure (the side
/// effects are confined to `runner`) so every branch is unit-testable.
fn fix_lines(
    checks: &[Check],
    os: &str,
    local_llm: &LocalLlm,
    runner: &dyn CommandRunner,
) -> Vec<String> {
    let mut lines = render_fixes(&fix_missing(checks, os, runner));
    if local_llm.enabled {
        let result = local_llm::ensure(os, runner, &local_llm.model);
        lines.extend(render_local_llm_fix(&result));
    }
    lines
}

/// Formats local LLM provisioning ([`local_llm::ensure`]) into printable lines.
fn render_local_llm_fix(result: &Result<Vec<SetupStep>, SetupError>) -> Vec<String> {
    match result {
        Ok(steps) => steps.iter().map(render_setup_step).collect(),
        Err(error) => vec![render_setup_error(error)],
    }
}

fn render_setup_step(step: &SetupStep) -> String {
    match step {
        SetupStep::OllamaAlreadyPresent => "ollama is already installed".to_string(),
        SetupStep::OllamaInstalled { manager } => format!("installed `ollama` via {manager}"),
        SetupStep::ServerAlreadyRunning => "ollama server is already running".to_string(),
        SetupStep::ServerStarted => "started the ollama server".to_string(),
        SetupStep::ModelAlreadyPresent { model } => {
            format!("local LLM model `{model}` is already pulled")
        }
        SetupStep::ModelPulled { model } => format!("pulled local LLM model `{model}`"),
    }
}

fn render_setup_error(error: &SetupError) -> String {
    match error {
        SetupError::OllamaUnavailable { manual } => {
            format!("could not install `ollama` automatically; {manual}")
        }
        SetupError::OllamaInstallFailed { manager, manual } => {
            format!("could not install `ollama` via {manager}; {manual}")
        }
        SetupError::ServerStartFailed => local_llm::server_start_failed_message(),
        SetupError::ModelPullFailed { model } => {
            format!("could not pull local LLM model `{model}`")
        }
    }
}

/// Formats the `--fix` outcomes into the lines printed by `usagi doctor --fix`.
fn render_fixes(outcomes: &[FixOutcome]) -> Vec<String> {
    if outcomes.is_empty() {
        return vec!["All required tools are installed 🎉".to_string()];
    }
    outcomes
        .iter()
        .map(|outcome| match outcome {
            FixOutcome::Installed { tool, manager } => {
                format!("installed `{tool}` via {manager}")
            }
            FixOutcome::Failed {
                tool,
                manager,
                manual,
            } => format!("could not install `{tool}` via {manager}; {manual}"),
            FixOutcome::Manual { tool: _, manual } => {
                format!("no package manager found; {manual}")
            }
        })
        .collect()
}

/// Formats the diagnostics into the lines printed by `usagi doctor`.
fn render(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .map(|check| {
            let status = format!("{:<14} {}", check.name, check.health.label());
            match &check.detail {
                Some(detail) => format!("{status}  ({detail})"),
                None => status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::doctor::Health;

    #[test]
    fn render_fixes_reports_nothing_to_do_when_no_outcomes() {
        let lines = render_fixes(&[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("All required tools are installed"));
    }

    #[test]
    fn render_fixes_describes_each_outcome_variant() {
        let outcomes = vec![
            FixOutcome::Installed {
                tool: "git".to_string(),
                manager: "brew",
            },
            FixOutcome::Failed {
                tool: "bash".to_string(),
                manager: "apt-get",
                manual: "install `bash` manually".to_string(),
            },
            FixOutcome::Manual {
                tool: "node".to_string(),
                manual: "install `node` manually".to_string(),
            },
        ];
        let lines = render_fixes(&outcomes);
        assert_eq!(
            lines,
            vec![
                "installed `git` via brew",
                "could not install `bash` via apt-get; install `bash` manually",
                "no package manager found; install `node` manually",
            ]
        );
    }

    #[test]
    fn run_with_fix_succeeds() {
        // In the test environment the required tools are present, so `--fix`
        // has nothing to install and simply reports success.
        assert!(run(true).is_ok());
    }

    #[test]
    fn render_aligns_status_and_appends_detail() {
        let checks = vec![
            Check {
                name: "git",
                health: Health::Ok,
                detail: None,
            },
            Check {
                name: "notifications",
                health: Health::Warn,
                detail: Some("no D-Bus session bus".into()),
            },
        ];
        let lines = render(&checks);
        assert_eq!(
            lines,
            vec![
                "git            ok",
                "notifications  warn  (no D-Bus session bus)",
            ]
        );
    }

    #[test]
    fn run_succeeds() {
        assert!(run(false).is_ok());
    }

    // --- local LLM provisioning -------------------------------------------

    /// A [`CommandRunner`] whose probe/availability are configurable, used to
    /// drive `fix_lines` without touching a real `ollama`/package manager.
    struct FakeRunner {
        available: Vec<&'static str>,
        check: bool,
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.available.contains(&program)
        }
        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }
        fn check(&self, _program: &str, args: &[&str]) -> bool {
            // The server is treated as already running (the start path is
            // covered in `usecase::local_llm`); `self.check` answers only the
            // model-presence probe.
            if args.first() == Some(&"ps") {
                return true;
            }
            self.check
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fake_runner_spawn_is_inert() {
        // The fake reports the server as already running, so its `spawn` is
        // never hit by `fix_lines`; the start path is covered in
        // `usecase::local_llm`. Assert the no-op directly.
        let runner = FakeRunner {
            available: vec![],
            check: false,
        };
        assert!(runner.spawn("ollama", &["serve"]).is_ok());
    }

    #[test]
    fn fix_lines_omits_local_llm_when_disabled() {
        // All checks healthy + local LLM off: only the standard success line.
        let runner = FakeRunner {
            available: vec![],
            check: false,
        };
        let lines = fix_lines(&[], "macos", &LocalLlm::default(), &runner);
        assert_eq!(lines, vec!["All required tools are installed 🎉"]);
    }

    #[test]
    fn fix_lines_appends_local_llm_provisioning_when_enabled() {
        // ollama + model already present: provisioning reports the no-op steps
        // after the standard tools line.
        let runner = FakeRunner {
            available: vec!["ollama"],
            check: true,
        };
        let local_llm = LocalLlm {
            enabled: true,
            model: "qwen2.5-coder:7b".to_string(),
        };
        let lines = fix_lines(&[], "macos", &local_llm, &runner);
        assert_eq!(
            lines,
            vec![
                "All required tools are installed 🎉",
                "ollama is already installed",
                "ollama server is already running",
                "local LLM model `qwen2.5-coder:7b` is already pulled",
            ]
        );
    }

    #[test]
    fn fix_lines_installs_ollama_and_pulls_when_missing() {
        // ollama absent but brew present, and the model is not pulled: both the
        // install and the pull run (exercising the runner's `run`).
        let runner = FakeRunner {
            available: vec!["brew"],
            check: false,
        };
        let local_llm = LocalLlm {
            enabled: true,
            model: "qwen2.5:7b".to_string(),
        };
        let lines = fix_lines(&[], "macos", &local_llm, &runner);
        assert_eq!(
            lines,
            vec![
                "All required tools are installed 🎉",
                "installed `ollama` via brew",
                "ollama server is already running",
                "pulled local LLM model `qwen2.5:7b`",
            ]
        );
    }

    #[test]
    fn render_local_llm_fix_describes_each_step() {
        let steps = vec![
            SetupStep::OllamaInstalled { manager: "brew" },
            SetupStep::OllamaAlreadyPresent,
            SetupStep::ServerStarted,
            SetupStep::ServerAlreadyRunning,
            SetupStep::ModelPulled {
                model: "qwen2.5:7b".to_string(),
            },
            SetupStep::ModelAlreadyPresent {
                model: "qwen2.5:7b".to_string(),
            },
        ];
        let lines = render_local_llm_fix(&Ok(steps));
        assert_eq!(
            lines,
            vec![
                "installed `ollama` via brew",
                "ollama is already installed",
                "started the ollama server",
                "ollama server is already running",
                "pulled local LLM model `qwen2.5:7b`",
                "local LLM model `qwen2.5:7b` is already pulled",
            ]
        );
    }

    #[test]
    fn render_local_llm_fix_describes_each_error() {
        assert_eq!(
            render_local_llm_fix(&Err(SetupError::OllamaUnavailable {
                manual: "install Ollama from https://ollama.com/download".to_string(),
            })),
            vec!["could not install `ollama` automatically; install Ollama from https://ollama.com/download"]
        );
        assert_eq!(
            render_local_llm_fix(&Err(SetupError::OllamaInstallFailed {
                manager: "brew",
                manual: "x".to_string(),
            })),
            vec!["could not install `ollama` via brew; x"]
        );
        assert_eq!(
            render_local_llm_fix(&Err(SetupError::ServerStartFailed)),
            vec!["could not start the ollama server; try running `ollama serve`"]
        );
        assert_eq!(
            render_local_llm_fix(&Err(SetupError::ModelPullFailed {
                model: "qwen2.5:7b".to_string(),
            })),
            vec!["could not pull local LLM model `qwen2.5:7b`"]
        );
    }
}
