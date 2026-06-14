use crate::infrastructure::storage::Storage;
use crate::usecase::doctor::{diagnose, fix_missing, Check, FixOutcome, SystemRunner};

/// Entry point for `usagi doctor`. With `fix`, attempts to install missing
/// tools (or prints manual steps); otherwise just prints the diagnostics.
pub fn run(fix: bool) -> anyhow::Result<()> {
    let storage = Storage::open_default()?;
    let checks = diagnose(&storage);
    let lines = if fix {
        let outcomes = fix_missing(&checks, std::env::consts::OS, &SystemRunner);
        render_fixes(&outcomes)
    } else {
        render(&checks)
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
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
}
