use crate::domain::agent_config::ProjectLanguage;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitAgentOutcome {
    pub language: ProjectLanguage,
    pub created: Vec<String>,
    pub skipped: Vec<String>,
}

/// Initialize AI agent configuration files for the project.
///
/// If a configuration file already exists, it is overwritten only if `overwrite_all` is true,
/// or if the `confirm_overwrite` closure returns `Ok(true)`.
pub fn init_agent<F>(
    dir: &Path,
    overwrite_all: bool,
    mut confirm_overwrite: F,
) -> Result<InitAgentOutcome>
where
    F: FnMut(&str) -> Result<bool>,
{
    let language = ProjectLanguage::detect(dir);
    let templates = language.config_templates();
    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for template in templates {
        let file_path = dir.join(&template.filename);
        let exists = file_path.exists();

        let should_write = !exists || overwrite_all || confirm_overwrite(&template.filename)?;

        if should_write {
            fs::write(&file_path, &template.content)
                .with_context(|| format!("failed to write {}", file_path.display()))?;
            created.push(template.filename);
        } else {
            skipped.push(template.filename);
        }
    }

    Ok(InitAgentOutcome {
        language,
        created,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_init_agent_new_project() {
        let dir = tempdir().unwrap();
        // Create Cargo.toml so it detects Rust
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let outcome = init_agent(dir.path(), false, |_| Ok(false)).unwrap();

        assert_eq!(outcome.language, ProjectLanguage::Rust);
        assert_eq!(
            outcome.created,
            vec!["CLAUDE.md", ".clinerules", ".aider.conf.yml"]
        );
        assert!(outcome.skipped.is_empty());

        assert!(dir.path().join("CLAUDE.md").is_file());
        assert!(dir.path().join(".clinerules").is_file());
        assert!(dir.path().join(".aider.conf.yml").is_file());
    }

    #[test]
    fn test_init_agent_existing_overwrite() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        // Pre-create CLAUDE.md with dummy content
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        // 1. With overwrite_all = true
        let outcome = init_agent(dir.path(), true, |_| Ok(false)).unwrap();
        assert_eq!(
            outcome.created,
            vec!["CLAUDE.md", ".clinerules", ".aider.conf.yml"]
        );
        assert!(outcome.skipped.is_empty());
        assert_ne!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "dummy"
        );
    }

    #[test]
    fn test_init_agent_existing_ask_confirm_yes() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        // 2. Ask and confirm yes
        let mut asked = Vec::new();
        let outcome = init_agent(dir.path(), false, |name| {
            asked.push(name.to_string());
            Ok(true)
        })
        .unwrap();

        assert_eq!(asked, vec!["CLAUDE.md"]);
        assert_eq!(
            outcome.created,
            vec!["CLAUDE.md", ".clinerules", ".aider.conf.yml"]
        );
        assert!(outcome.skipped.is_empty());
        assert_ne!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "dummy"
        );
    }

    #[test]
    fn test_init_agent_existing_ask_confirm_no() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        // 3. Ask and confirm no
        let mut asked = Vec::new();
        let outcome = init_agent(dir.path(), false, |name| {
            asked.push(name.to_string());
            Ok(false)
        })
        .unwrap();

        assert_eq!(asked, vec!["CLAUDE.md"]);
        assert_eq!(outcome.created, vec![".clinerules", ".aider.conf.yml"]);
        assert_eq!(outcome.skipped, vec!["CLAUDE.md"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "dummy"
        );
    }

    #[test]
    fn test_init_agent_write_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        // Create a directory where CLAUDE.md should be written to fail fs::write
        std::fs::create_dir(dir.path().join("CLAUDE.md")).unwrap();

        let result = init_agent(dir.path(), true, |_| Ok(true));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to write"));
    }

    #[test]
    fn test_init_agent_confirm_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "dummy").unwrap();

        let result = init_agent(dir.path(), false, |_| Err(anyhow::anyhow!("mock error")));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "mock error");
    }
}
