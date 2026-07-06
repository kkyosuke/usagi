use crate::domain::agent_config::ProjectLanguage;
use crate::usecase::init_agent;
use anyhow::Result;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const SKIPPED_FILES_HEADER: &str = "以下のファイル生成をスキップしました (既に存在するため):";

pub fn run(yes: bool) -> Result<()> {
    run_with_current_dir(yes, std::env::current_dir)
}

fn run_with_current_dir(
    yes: bool,
    current_dir: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Result<()> {
    let cwd = current_dir()?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout();
    run_impl(yes, &cwd, &mut input, &mut output)
}

fn run_impl(yes: bool, cwd: &Path, input: &mut dyn BufRead, output: &mut dyn Write) -> Result<()> {
    init_agent_cli(cwd, yes, input, output)
}

fn init_agent_cli(
    dir: &Path,
    yes: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
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
        writeln!(output, "{}", SKIPPED_FILES_HEADER)?;
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

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
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

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
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

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
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
            (Some("package.json"), "Node.js"),
            (Some("requirements.txt"), "Python"),
            (Some("go.mod"), "Go"),
            (Some("pom.xml"), "Java"),
            (Some("Gemfile"), "Ruby"),
            (None, "Generic"),
        ];

        for &(file, expected_lang) in cases {
            let dir = tempdir().unwrap();
            if let Some(file) = file {
                std::fs::write(dir.path().join(file), "").unwrap();
            }

            let input = b"";
            let mut output = Vec::new();

            let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
            assert!(result.is_ok());

            let out_str = String::from_utf8(output).unwrap();
            assert!(
                out_str.contains(&format!("検出したプロジェクト言語/構成: {}", expected_lang)),
                "failed on {:?}, got: {}",
                file,
                out_str
            );
        }
    }

    #[test]
    fn test_init_agent_cli_existing_all_skipped() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        for file in ["CLAUDE.md", ".clinerules", ".aider.conf.yml"] {
            std::fs::write(dir.path().join(file), "dummy").unwrap();
        }

        let input = b"n\nn\nn\n";
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
        assert!(result.is_ok());

        let out_str = String::from_utf8(output).unwrap();
        assert!(!out_str.contains("以下のファイルを生成しました:"));
        assert!(out_str.contains("以下のファイル生成をスキップしました (既に存在するため):"));
        assert!(out_str.contains("  - CLAUDE.md"));
        assert!(out_str.contains("  - .clinerules"));
        assert!(out_str.contains("  - .aider.conf.yml"));
    }

    #[test]
    fn test_run_executes_against_the_current_directory() {
        let files = ["CLAUDE.md", ".clinerules", ".aider.conf.yml"];
        let backups = files.map(|file| (file, std::fs::read_to_string(file).ok()));

        let result = run(true);

        for (file, backup) in backups {
            if let Some(content) = backup {
                let _ = std::fs::write(file, content);
            } else {
                let _ = std::fs::remove_file(file);
            }
        }

        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_current_dir_reports_current_dir_errors() {
        let result = run_with_current_dir(false, || Err(std::io::Error::other("cwd error")));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "cwd error");
    }

    #[test]
    fn test_run_impl_executes_against_directory() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let input = b"";
        let mut output = Vec::new();

        let result_true = run_impl(true, tmp.path(), &mut &input[..], &mut output);
        assert!(result_true.is_ok());
        assert!(tmp.path().join("CLAUDE.md").is_file());

        let result_false = run_impl(false, tmp.path(), &mut &input[..], &mut output);
        assert!(result_false.is_ok());
    }

    struct FailOnWriteContaining {
        needle: &'static str,
        written: String,
    }

    impl FailOnWriteContaining {
        fn new(needle: &'static str) -> Self {
            Self {
                needle,
                written: String::new(),
            }
        }
    }

    impl Write for FailOnWriteContaining {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.push_str(&String::from_utf8_lossy(buf));
            if self.written.contains(self.needle) {
                Err(std::io::Error::other("forced write error"))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_init_agent_cli_output_errors_after_init_agent_outcome() {
        let cases = [
            ("検出したプロジェクト言語/構成:", false, b"".as_slice()),
            ("以下のファイルを生成しました:", false, b"".as_slice()),
            ("  - CLAUDE.md", false, b"".as_slice()),
            (SKIPPED_FILES_HEADER, true, b"n\nn\nn\n".as_slice()),
            ("  - .aider.conf.yml", true, b"n\nn\nn\n".as_slice()),
        ];

        for (needle, precreate_configs, mut input) in cases {
            let dir = tempdir().unwrap();
            std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
            if precreate_configs {
                for file in ["CLAUDE.md", ".clinerules", ".aider.conf.yml"] {
                    std::fs::write(dir.path().join(file), "dummy").unwrap();
                }
            }

            let mut output = FailOnWriteContaining::new(needle);
            let result = init_agent_cli(dir.path(), false, &mut input, &mut output);
            assert!(result.is_err(), "needle should fail: {needle}");
            assert_eq!(result.unwrap_err().to_string(), "forced write error");
        }
    }

    enum WriterFailure {
        Write,
        Flush,
    }

    struct FailingWriter {
        failure: WriterFailure,
    }

    impl FailingWriter {
        fn on_write() -> Self {
            Self {
                failure: WriterFailure::Write,
            }
        }

        fn on_flush() -> Self {
            Self {
                failure: WriterFailure::Flush,
            }
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.failure {
                WriterFailure::Write => Err(std::io::Error::other("write error")),
                WriterFailure::Flush => Ok(buf.len()),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self.failure {
                WriterFailure::Write => Ok(()),
                WriterFailure::Flush => Err(std::io::Error::other("flush error")),
            }
        }
    }

    struct FailingRead;
    impl std::io::Read for FailingRead {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read error"))
        }
    }

    #[test]
    fn test_init_agent_cli_write_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let input = b"";
        let mut output = FailingWriter::on_write();

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "write error");
        assert!(output.flush().is_ok());
    }

    #[test]
    fn test_init_agent_cli_flush_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let input = b"";
        let mut output = FailingWriter::on_flush();

        let result = init_agent_cli(dir.path(), false, &mut &input[..], &mut output);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "flush error");
    }

    #[test]
    fn test_init_agent_cli_read_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let mut input = std::io::BufReader::new(FailingRead);
        let mut output = Vec::new();

        let result = init_agent_cli(dir.path(), false, &mut input, &mut output);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "read error");
    }
}
