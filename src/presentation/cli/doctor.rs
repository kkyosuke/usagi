use crate::infrastructure::storage::Storage;
use crate::usecase::doctor::{diagnose, Check};

pub fn run() -> anyhow::Result<()> {
    let storage = Storage::open_default()?;
    for line in render(&diagnose(&storage)) {
        println!("{line}");
    }
    Ok(())
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
        assert!(run().is_ok());
    }
}
