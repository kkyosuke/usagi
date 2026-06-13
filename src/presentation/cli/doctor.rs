use crate::usecase::doctor::{check_dependencies, DependencyCheck};

pub fn run() -> anyhow::Result<()> {
    for line in render(&check_dependencies()) {
        println!("{line}");
    }
    Ok(())
}

/// Formats the dependency checks into the lines printed by `usagi doctor`.
fn render(checks: &[DependencyCheck]) -> Vec<String> {
    checks
        .iter()
        .map(|check| {
            let mark = if check.available { "ok" } else { "missing" };
            format!("{:<10} {}", check.name, mark)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_formats_available_and_missing_tools() {
        let checks = vec![
            DependencyCheck {
                name: "git",
                available: true,
            },
            DependencyCheck {
                name: "nope",
                available: false,
            },
        ];
        let lines = render(&checks);
        assert_eq!(lines, vec!["git        ok", "nope       missing"]);
    }

    #[test]
    fn run_succeeds() {
        assert!(run().is_ok());
    }
}
