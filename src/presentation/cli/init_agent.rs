use crate::domain::agent_config::ProjectLanguage;
use crate::usecase::init_agent;
use anyhow::Result;
use std::io::{BufRead, Write};
use std::path::Path;

pub fn run(yes: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_impl(yes, &cwd, std::io::stdin().lock(), std::io::stdout())
}

fn run_impl(yes: bool, cwd: &Path, input: impl BufRead, mut output: impl Write) -> Result<()> {
    init_agent_cli(cwd, yes, input, &mut output)
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

    #[test]
    fn test_init_agent_cli_languages() {
        let cases = &[
            ("package.json", "Node.js"),
            ("requirements.txt", "Python"),
            ("go.mod", "Go"),
            ("pom.xml", "Java"),
            ("Gemfile", "Ruby"),
        ];

        for &(file, expected_lang) in cases {
            let dir = tempdir().unwrap();
            std::fs::write(dir.path().join(file), "").unwrap();

            let input = b"";
            let mut output = Vec::new();

            let result = init_agent_cli(dir.path(), false, &input[..], &mut output);
            assert!(result.is_ok());

            let out_str = String::from_utf8(output).unwrap();
            assert!(
                out_str.contains(&format!("検出したプロジェクト言語/構成: {}", expected_lang)),
                "failed on {}, got: {}",
                file,
                out_str
            );
        }
    }

    #[test]
    fn test_run_executes_against_the_current_directory() {
        let backup_claude = std::fs::read_to_string("CLAUDE.md").ok();
        let backup_clinerules = std::fs::read_to_string(".clinerules").ok();
        let backup_aider = std::fs::read_to_string(".aider.conf.yml").ok();

        let result = run(true);

        if let Some(content) = backup_claude {
            let _ = std::fs::write("CLAUDE.md", content);
        } else {
            let _ = std::fs::remove_file("CLAUDE.md");
        }
        if let Some(content) = backup_clinerules {
            let _ = std::fs::write(".clinerules", content);
        } else {
            let _ = std::fs::remove_file(".clinerules");
        }
        if let Some(content) = backup_aider {
            let _ = std::fs::write(".aider.conf.yml", content);
        } else {
            let _ = std::fs::remove_file(".aider.conf.yml");
        }

        assert!(result.is_ok());
    }

    #[test]
    fn test_run_impl_executes_against_directory() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let input = b"";
        let mut output = Vec::new();

        let result_true = run_impl(true, tmp.path(), &input[..], &mut output);
        assert!(result_true.is_ok());
        assert!(tmp.path().join("CLAUDE.md").is_file());

        let result_false = run_impl(false, tmp.path(), &input[..], &mut output);
        assert!(result_false.is_ok());
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "write error",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "flush error",
            ))
        }
    }

    struct FailingReader;
    impl std::io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "read error"))
        }
    }
    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "fill_buf error",
            ))
        }
        fn consume(&mut self, _amt: usize) {}
        fn read_line(&mut self, _buf: &mut String) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "read_line error",
            ))
        }
    }

    #[test]
    fn test_init_agent_cli_write_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let input = b"";
        let mut output = FailingWriter;

        let result = init_agent_cli(dir.path(), false, &input[..], &mut output);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "write error");
    }

    #[test]
    fn test_init_agent_cli_read_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let input = FailingReader;
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, input, &mut output);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "read_line error");
    }
}
