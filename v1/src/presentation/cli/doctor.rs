use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::presentation::theme::Palette;
use console::style;

use crate::domain::settings::LocalLlm;
use crate::infrastructure::storage::Storage;
use crate::usecase::doctor::{
    diagnose, fix_missing, installable_gaps, Check, CommandRunner, FixOutcome, Health, SystemRunner,
};
use crate::usecase::font::{self, FontError, FontStep};
use crate::usecase::local_llm::{self, SetupError, SetupStep};

/// Entry point for `usagi doctor`. Prints the diagnostics, then runs the install
/// pass for anything missing: `--fix` installs without asking, while plain
/// `usagi doctor` prompts for confirmation first. Binds the real terminal IO and
/// delegates the testable logic to [`doctor`].
pub fn run(fix: bool) -> anyhow::Result<()> {
    let storage = Storage::open_default()?;
    let os = std::env::consts::OS;
    let font_dirs = font::font_dirs(os, &dirs::home_dir().unwrap_or_default());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    doctor(
        &storage,
        fix,
        os,
        &font_dirs,
        &SystemRunner,
        stdin.lock(),
        &mut stdout,
    )
}

/// Gather the diagnostics and saved settings (the real IO), then hand off to
/// [`report_and_fix`]. Thin on purpose so the branching logic lives in a
/// function that takes its `checks` directly and is exercised with fakes;
/// `diagnose` runs its own [`SystemRunner`], so a test cannot steer the gap set
/// through the injected `runner` here.
fn doctor(
    storage: &Storage,
    fix: bool,
    os: &str,
    font_dirs: &[PathBuf],
    runner: &dyn CommandRunner,
    input: impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    let checks = diagnose(storage);
    // Fall back to defaults (local LLM off) if settings cannot be read.
    let local_llm = storage
        .load_settings()
        .map(|s| s.local_llm)
        .unwrap_or_default();
    report_and_fix(
        &checks, fix, os, &local_llm, font_dirs, runner, input, output,
    )
}

/// Print the diagnostics, then install missing tools/fonts. `--fix` installs
/// without asking; otherwise the user is prompted, and only when there is
/// something installable to do (see [`installable_gaps`]). Takes its `checks`
/// and `local_llm` directly so the prompt and remediation are unit-tested with a
/// fake runner, scripted `input`, and a captured `output` — no real terminal,
/// package manager, or font download.
#[allow(clippy::too_many_arguments)]
fn report_and_fix(
    checks: &[Check],
    fix: bool,
    os: &str,
    local_llm: &LocalLlm,
    font_dirs: &[PathBuf],
    runner: &dyn CommandRunner,
    mut input: impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    for line in render(checks) {
        writeln!(output, "{line}")?;
    }
    let gaps = installable_gaps(checks);
    // `--fix` installs unconditionally; plain `doctor` asks, but only when there
    // is an installable gap (no point prompting when everything is present).
    let install = if fix {
        true
    } else if gaps.is_empty() {
        false
    } else {
        confirm_install(&gaps, &mut input, output)?
    };
    if install {
        for line in fix_lines(checks, os, local_llm, font_dirs, runner) {
            writeln!(output, "{line}")?;
        }
    }
    Ok(())
}

/// Ask whether to install the missing `gaps`, returning the user's choice. Only
/// `y`/`yes` (case-insensitive) accepts; every other answer — including EOF or a
/// non-interactive stdin — declines, so the command never blocks in a script or
/// CI.
fn confirm_install(
    gaps: &[&str],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<bool> {
    let items = gaps.join(", ");
    writeln!(output, "未インストールの項目があります: {items}")?;
    write!(output, "インストールしますか? [y/N]: ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The lines printed by `usagi doctor --fix`: the standard tool remediation,
/// the Nerd Font download, then local LLM provisioning when it is enabled. Pure
/// (the side effects are confined to `runner`/the filesystem under `font_dirs`)
/// so every branch is unit-testable.
fn fix_lines(
    checks: &[Check],
    os: &str,
    local_llm: &LocalLlm,
    font_dirs: &[PathBuf],
    runner: &dyn CommandRunner,
) -> Vec<String> {
    let mut lines = render_fixes(&fix_missing(checks, os, runner));
    // Downloading a Nerd Font is not a package-manager install, so it has its
    // own flow (like the local LLM) rather than going through `fix_missing`.
    // Idempotent: an already-installed font is reported, not re-downloaded.
    lines.extend(render_font_fix(&font::ensure(os, runner, font_dirs)));
    if local_llm.enabled {
        // The CLI runs on a real terminal, so the installer can prompt for
        // sudo itself; no pre-supplied password (that is the TUI flow). Output
        // stays loud (`quiet = false`) so the user watches install progress.
        let result = local_llm::ensure(os, runner, &local_llm.model, None, false);
        lines.extend(render_local_llm_fix(&result));
    }
    lines
}

/// Formats Nerd Font provisioning ([`font::ensure`]) into printable lines.
fn render_font_fix(result: &Result<FontStep, FontError>) -> Vec<String> {
    let line = match result {
        Ok(FontStep::AlreadyPresent) => "a Nerd Font is already installed".to_string(),
        Ok(FontStep::Installed { font, dir }) => {
            format!("installed the {font} Nerd Font into {dir}")
        }
        Err(FontError::Unsupported { manual }) => {
            format!("could not install a Nerd Font automatically; {manual}")
        }
        Err(FontError::ToolMissing { tool, manual }) => {
            format!("`{tool}` is required to install a Nerd Font; {manual}")
        }
        Err(FontError::DirCreateFailed { dir, manual }) => {
            format!("could not create the font directory {dir}; {manual}")
        }
        Err(FontError::DownloadFailed { manual }) => {
            format!("could not download the Nerd Font; {manual}")
        }
        Err(FontError::ExtractFailed { manual }) => {
            format!("could not extract the Nerd Font; {manual}")
        }
    };
    vec![line]
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
    let name_width = checks
        .iter()
        .map(|check| check.name.chars().count())
        .max()
        .unwrap_or(0);
    let status_width = [Health::Ok, Health::Warn, Health::Missing]
        .iter()
        .map(|health| health.label().chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = vec![style("usagi doctor").bold().to_string()];
    lines.extend(checks.iter().map(|check| {
        let name = format!("{:<name_width$}", check.name);
        let status = format!("{:<status_width$}", check.health.label());
        let status = health_label(check.health, status);
        let row = match &check.detail {
            Some(detail) => format!(
                "  {}  {}  {}  {}",
                health_icon(check.health),
                status,
                name,
                style(detail).dim()
            ),
            None => format!("  {}  {}  {}", health_icon(check.health), status, name),
        };
        row.trim_end().to_string()
    }));
    lines.push(String::new());
    lines.push(render_summary(checks));
    lines
}

/// Single-character status marker shown at the start of each diagnostic row:
/// a green `✓` (ok), a yellow `!` (warn), or a red `✗` (missing). The glyph
/// (not just its color) distinguishes the three states, so the output still
/// reads on a no-color terminal.
fn health_icon(health: Health) -> String {
    match health {
        Health::Ok => style("✓").success().to_string(),
        Health::Warn => style("!").warning().to_string(),
        Health::Missing => style("✗").danger().to_string(),
    }
}

/// Colored health label printed next to [`health_icon`].
fn health_label(health: Health, label: String) -> String {
    match health {
        Health::Ok => style(label).success().to_string(),
        Health::Warn => style(label).warning().to_string(),
        Health::Missing => style(label).danger().to_string(),
    }
}

/// Summary footer tallying the diagnostic results by health.
fn render_summary(checks: &[Check]) -> String {
    let ok = checks
        .iter()
        .filter(|check| check.health == Health::Ok)
        .count();
    let warn = checks
        .iter()
        .filter(|check| check.health == Health::Warn)
        .count();
    let missing = checks
        .iter()
        .filter(|check| check.health == Health::Missing)
        .count();

    format!(
        "summary: {} ok, {} warn, {} missing",
        style(ok).success(),
        style(warn).warning(),
        style(missing).danger()
    )
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

    /// A `git` check that is `missing`, so [`installable_gaps`] reports a gap and
    /// the prompt path is exercised deterministically (without depending on what
    /// the host actually has installed).
    fn missing_git() -> Vec<Check> {
        vec![Check {
            name: "git",
            health: Health::Missing,
            detail: Some("`git` was not found on your PATH".into()),
        }]
    }

    /// Run [`report_and_fix`] against `input`, returning the captured output as a
    /// string. Uses a pre-installed font so the font flow never downloads.
    fn run_report(checks: &[Check], fix: bool, input: &str) -> String {
        let runner = FakeRunner {
            available: vec![],
            check: false,
        };
        let (_guard, font_dirs) = font_dirs_with_font();
        let mut out = Vec::new();
        report_and_fix(
            checks,
            fix,
            "macos",
            &LocalLlm::default(),
            &font_dirs,
            &runner,
            input.as_bytes(),
            &mut out,
        )
        .expect("report_and_fix should not fail writing to a Vec");
        String::from_utf8(out).expect("output is valid UTF-8")
    }

    #[test]
    fn report_and_fix_diagnoses_without_installing_when_nothing_is_missing() {
        // All checks healthy and no `--fix`: only the diagnostics are printed,
        // with no prompt and no install pass.
        let checks = vec![Check {
            name: "git",
            health: Health::Ok,
            detail: None,
        }];
        let out = run_report(&checks, false, "");
        let out = console::strip_ansi_codes(&out);
        assert!(out.contains("✓  ok       git"));
        assert!(out.contains("summary: 1 ok, 0 warn, 0 missing"));
        assert!(!out.contains("インストールしますか"));
        assert!(!out.contains("Nerd Font"));
    }

    #[test]
    fn report_and_fix_installs_without_prompting_under_fix() {
        // `--fix` skips the prompt and runs the remediation even with a gap; the
        // (pre-installed) font reports the no-op line.
        let out = run_report(&missing_git(), true, "");
        assert!(!out.contains("インストールしますか"));
        assert!(out.contains("a Nerd Font is already installed"));
    }

    #[test]
    fn report_and_fix_installs_when_the_user_confirms() {
        // A gap + `y`: the prompt is shown and the install pass runs.
        let out = run_report(&missing_git(), false, "y\n");
        assert!(out.contains("未インストールの項目があります: git"));
        assert!(out.contains("インストールしますか? [y/N]:"));
        assert!(out.contains("a Nerd Font is already installed"));
    }

    #[test]
    fn report_and_fix_skips_install_when_the_user_declines() {
        // A gap + `n`: the prompt is shown but nothing is installed.
        let out = run_report(&missing_git(), false, "n\n");
        assert!(out.contains("インストールしますか? [y/N]:"));
        assert!(!out.contains("Nerd Font"));
    }

    #[test]
    fn confirm_install_accepts_y_and_yes_case_insensitively() {
        for answer in ["y\n", "Y\n", "yes\n", "  YES  \n"] {
            let mut out = Vec::new();
            let accepted = confirm_install(&["git"], &mut answer.as_bytes(), &mut out)
                .expect("writing to a Vec never fails");
            assert!(accepted, "{answer:?} should accept");
        }
    }

    #[test]
    fn confirm_install_declines_on_other_answers_and_eof() {
        // A blank/`n` answer and an empty (EOF / non-interactive) stream both
        // decline, so the command never blocks in a script or CI.
        for answer in ["n\n", "\n", "nope\n", ""] {
            let mut out = Vec::new();
            let accepted = confirm_install(&["git"], &mut answer.as_bytes(), &mut out)
                .expect("writing to a Vec never fails");
            assert!(!accepted, "{answer:?} should decline");
        }
    }

    #[test]
    fn render_aligns_rows_and_appends_summary() {
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
        let lines = render(&checks)
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "usagi doctor",
                "  ✓  ok       git",
                "  !  warn     notifications  no D-Bus session bus",
                "",
                "summary: 1 ok, 1 warn, 0 missing",
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

    /// A temp directory pre-populated with a Nerd Font, so `font::ensure`
    /// reports it already present (the install path has its own tests). Returns
    /// the guard (kept alive by the caller) and the dirs list to pass in.
    fn font_dirs_with_font() -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        std::fs::write(dir.path().join("JetBrainsMonoNerdFont-Regular.ttf"), b"x").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        (dir, dirs)
    }

    #[test]
    fn fix_lines_omits_local_llm_when_disabled() {
        // All checks healthy + local LLM off: the standard success line followed
        // by the (idempotent) Nerd Font report.
        let runner = FakeRunner {
            available: vec![],
            check: false,
        };
        let (_guard, dirs) = font_dirs_with_font();
        let lines = fix_lines(&[], "macos", &LocalLlm::default(), &dirs, &runner);
        assert_eq!(
            lines,
            vec![
                "All required tools are installed 🎉",
                "a Nerd Font is already installed",
            ]
        );
    }

    #[test]
    fn fix_lines_installs_a_nerd_font_when_missing() {
        // No font present and the download tools available: the font is fetched
        // and its install line is appended after the tools report.
        let runner = FakeRunner {
            available: vec!["curl", "unzip"],
            check: false,
        };
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let dirs = vec![dir.path().to_path_buf()];
        let lines = fix_lines(&[], "macos", &LocalLlm::default(), &dirs, &runner);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "All required tools are installed 🎉");
        assert!(lines[1].starts_with("installed the JetBrainsMono Nerd Font into"));
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
        let (_guard, dirs) = font_dirs_with_font();
        let lines = fix_lines(&[], "macos", &local_llm, &dirs, &runner);
        assert_eq!(
            lines,
            vec![
                "All required tools are installed 🎉",
                "a Nerd Font is already installed",
                "ollama is already installed",
                "ollama server is already running",
                "local LLM model `qwen2.5-coder:7b` is already pulled",
            ]
        );
    }

    #[test]
    fn fix_lines_installs_ollama_and_pulls_when_missing() {
        // ollama absent and the model not pulled: the official installer and
        // the pull both run (exercising the runner's `run`).
        let runner = FakeRunner {
            available: vec![],
            check: false,
        };
        let local_llm = LocalLlm {
            enabled: true,
            model: "qwen2.5:7b".to_string(),
        };
        let (_guard, dirs) = font_dirs_with_font();
        let lines = fix_lines(&[], "macos", &local_llm, &dirs, &runner);
        assert_eq!(
            lines,
            vec![
                "All required tools are installed 🎉",
                "a Nerd Font is already installed",
                "installed `ollama` via ollama.com/install.sh",
                "ollama server is already running",
                "pulled local LLM model `qwen2.5:7b`",
            ]
        );
    }

    #[test]
    fn render_font_fix_describes_each_step_and_error() {
        assert_eq!(
            render_font_fix(&Ok(FontStep::AlreadyPresent)),
            vec!["a Nerd Font is already installed"]
        );
        assert_eq!(
            render_font_fix(&Ok(FontStep::Installed {
                font: "JetBrainsMono",
                dir: "/fonts".to_string(),
            })),
            vec!["installed the JetBrainsMono Nerd Font into /fonts"]
        );
        assert_eq!(
            render_font_fix(&Err(FontError::Unsupported {
                manual: "M".to_string(),
            })),
            vec!["could not install a Nerd Font automatically; M"]
        );
        assert_eq!(
            render_font_fix(&Err(FontError::ToolMissing {
                tool: "curl",
                manual: "M".to_string(),
            })),
            vec!["`curl` is required to install a Nerd Font; M"]
        );
        assert_eq!(
            render_font_fix(&Err(FontError::DirCreateFailed {
                dir: "/fonts".to_string(),
                manual: "M".to_string(),
            })),
            vec!["could not create the font directory /fonts; M"]
        );
        assert_eq!(
            render_font_fix(&Err(FontError::DownloadFailed {
                manual: "M".to_string(),
            })),
            vec!["could not download the Nerd Font; M"]
        );
        assert_eq!(
            render_font_fix(&Err(FontError::ExtractFailed {
                manual: "M".to_string(),
            })),
            vec!["could not extract the Nerd Font; M"]
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
