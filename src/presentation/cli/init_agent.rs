use crate::domain::agent_config::ProjectLanguage;
use crate::usecase::init_agent;
use anyhow::Result;
use std::io::{BufRead, Write};
use std::path::Path;

pub fn run(yes: bool) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let cwd = std::env::current_dir()?;
    init_agent_cli(&cwd, yes, stdin.lock(), &mut stdout)
}

fn init_agent_cli(
    dir: &Path,
    yes: bool,
    mut input: impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let outcome = init_agent::init_agent(dir, yes, |filename| {
        write!(
            output,
            "ファイル '{}' が既に存在します。上書きしますか? [y/N]: ",
            filename
        )?;
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        let trimmed = answer.trim().to_ascii_lowercase();
        Ok(trimmed == "y" || trimmed == "yes")
    })?;

    let lang_str = match outcome.language {
        ProjectLanguage::Rust => "Rust",
        ProjectLanguage::NodeJs => "Node.js",
        ProjectLanguage::Python => "Python",
        ProjectLanguage::Go => "Go",
        ProjectLanguage::Java => "Java",
        ProjectLanguage::Ruby => "Ruby",
        ProjectLanguage::Generic => "Generic",
    };

    writeln!(output, "検出したプロジェクト言語/構成: {}", lang_str)?;

    if !outcome.created.is_empty() {
        writeln!(output, "以下のファイルを生成しました:")?;
        for file in &outcome.created {
            writeln!(output, "  - {}", file)?;
        }
    }

    if !outcome.skipped.is_empty() {
        writeln!(
            output,
            "以下のファイル生成をスキップしました (既に存在するため):"
        )?;
        for file in &outcome.skipped {
            writeln!(output, "  - {}", file)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_init_agent_cli_new_project() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let input = b"";
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, &input[..], &mut output);
        assert!(result.is_ok());

        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("検出したプロジェクト言語/構成: Rust"));
        assert!(out_str.contains("以下のファイルを生成しました:"));
        assert!(out_str.contains("  - CLAUDE.md"));
        assert!(out_str.contains("  - .clinerules"));
        assert!(out_str.contains("  - .aider.conf.yml"));
    }

    #[test]
    fn test_init_agent_cli_existing_no_overwrite() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        // Send "n" to prompt
        let input = b"n\n";
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, &input[..], &mut output);
        assert!(result.is_ok());

        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("ファイル 'CLAUDE.md' が既に存在します。上書きしますか? [y/N]:"));
        assert!(out_str.contains("以下のファイルを生成しました:"));
        assert!(out_str.contains("  - .clinerules"));
        assert!(out_str.contains("以下のファイル生成をスキップしました (既に存在するため):"));
        assert!(out_str.contains("  - CLAUDE.md"));

        // File should not be overwritten
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "dummy"
        );
    }

    #[test]
    fn test_init_agent_cli_existing_yes_overwrite() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        // Send "y" to prompt
        let input = b"y\n";
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, &input[..], &mut output);
        assert!(result.is_ok());

        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("ファイル 'CLAUDE.md' が既に存在します。上書きしますか? [y/N]:"));
        assert!(out_str.contains("以下のファイルを生成しました:"));
        assert!(out_str.contains("  - CLAUDE.md"));
        assert_ne!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "dummy"
        );
    }
}
