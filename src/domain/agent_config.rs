use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectLanguage {
    Rust,
    NodeJs,
    Python,
    Go,
    Java,
    Ruby,
    Generic,
}

impl ProjectLanguage {
    /// Detect project language based on existing files in directory.
    pub fn detect(dir: &Path) -> Self {
        if dir.join("Cargo.toml").exists() {
            ProjectLanguage::Rust
        } else if dir.join("package.json").exists() {
            ProjectLanguage::NodeJs
        } else if dir.join("requirements.txt").exists()
            || dir.join("pyproject.toml").exists()
            || dir.join("Pipfile").exists()
            || dir.join("setup.py").exists()
        {
            ProjectLanguage::Python
        } else if dir.join("go.mod").exists() {
            ProjectLanguage::Go
        } else if dir.join("pom.xml").exists() || dir.join("build.gradle").exists() {
            ProjectLanguage::Java
        } else if dir.join("Gemfile").exists() {
            ProjectLanguage::Ruby
        } else {
            ProjectLanguage::Generic
        }
    }

    /// Return templates of agent configuration files for the language.
    pub fn config_templates(&self) -> Vec<AgentConfigFileTemplate> {
        let (claude_content, clinerules_content) = match self {
            ProjectLanguage::Rust => (
                r#"# CLAUDE.md

## Build and Test Commands
- Build: `cargo build`
- Test all: `cargo test`
- Test specific: `cargo test <test_name>`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`

## Coding Guidelines
- Follow idiomatic Rust styles and guidelines.
- Prefer using standard library components when possible.
- Ensure all tests pass before making a pull request.
"#,
                r#"- Always run `cargo clippy --all-targets -- -D warnings` and `cargo fmt` before concluding code edits.
- Write unit tests for new logic.
- Keep dependencies updated and minimize usage of unsafe code unless necessary.
"#,
            ),
            ProjectLanguage::NodeJs => (
                r#"# CLAUDE.md

## Build and Test Commands
- Build: `npm run build`
- Test all: `npm test`
- Lint: `npm run lint`
- Format: `npm run format`

## Coding Guidelines
- Follow TypeScript/JavaScript best practices.
- Prefer clean, readable, and well-typed code.
- Ensure linting passes and tests are updated.
"#,
                r#"- Run linting and formatting tools before finishing task.
- Ensure TypeScript types are explicit and avoid using `any` where possible.
"#,
            ),
            ProjectLanguage::Python => (
                r#"# CLAUDE.md

## Build and Test Commands
- Test all: `pytest`
- Lint: `ruff check` or `flake8`
- Format: `black .` or `ruff format`

## Coding Guidelines
- Follow PEP 8 style guide.
- Use type hints where appropriate.
"#,
                r#"- Ensure Python code is formatted with black/ruff before completion.
- Write tests in `tests/` directory using pytest.
"#,
            ),
            ProjectLanguage::Go => (
                r#"# CLAUDE.md

## Build and Test Commands
- Build: `go build`
- Test all: `go test ./...`
- Lint: `golangci-lint run`
- Format: `go fmt ./...`

## Coding Guidelines
- Follow Go coding conventions (Effective Go).
- Handle all errors explicitly.
"#,
                r#"- Format Go code using `go fmt` before committing.
- Ensure test coverage is adequate and runs via `go test`.
"#,
            ),
            ProjectLanguage::Java => (
                r#"# CLAUDE.md

## Build and Test Commands
- Build: `./gradlew build` or `./mvnw package`
- Test all: `./gradlew test` or `./mvnw test`

## Coding Guidelines
- Follow Java coding conventions and naming rules.
- Ensure classes and public methods are properly documented.
"#,
                r#"- Write junit tests for new classes.
- Avoid raw types and ensure generic correctness.
"#,
            ),
            ProjectLanguage::Ruby => (
                r#"# CLAUDE.md

## Build and Test Commands
- Test all: `bundle exec rspec`
- Lint: `bundle exec rubocop`

## Coding Guidelines
- Follow Ruby style guides (RuboCop rules).
- Keep classes small and focused on a single responsibility.
"#,
                r#"- Ensure code style passes RuboCop checks.
- Add specs for modified or new behaviors.
"#,
            ),
            ProjectLanguage::Generic => (
                r#"# CLAUDE.md

## Build and Test Commands
- Build: (adjust to project's build command)
- Test: (adjust to project's test command)
- Lint: (adjust to project's lint command)

## Coding Guidelines
- Maintain consistent code style.
- Add tests for new features.
"#,
                r#"- Follow project patterns and style guides.
- Verify changes before finishing tasks.
"#,
            ),
        };

        let aider_content = r#"# Aider configuration
# Ref: https://aider.chat/docs/config.html
auto-commit: false
map-tokens: 1024
"#;

        vec![
            AgentConfigFileTemplate {
                filename: "CLAUDE.md".to_string(),
                content: claude_content.to_string(),
            },
            AgentConfigFileTemplate {
                filename: ".clinerules".to_string(),
                content: clinerules_content.to_string(),
            },
            AgentConfigFileTemplate {
                filename: ".aider.conf.yml".to_string(),
                content: aider_content.to_string(),
            },
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfigFileTemplate {
    pub filename: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_detect_rust() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Rust);
    }

    #[test]
    fn test_detect_nodejs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::NodeJs);
    }

    #[test]
    fn test_detect_python() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Python);

        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Python);
    }

    #[test]
    fn test_detect_go() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Go);
    }

    #[test]
    fn test_detect_java() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Java);

        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("build.gradle"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Java);
    }

    #[test]
    fn test_detect_ruby() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "").unwrap();
        assert_eq!(ProjectLanguage::detect(dir.path()), ProjectLanguage::Ruby);
    }

    #[test]
    fn test_detect_generic() {
        let dir = tempdir().unwrap();
        assert_eq!(
            ProjectLanguage::detect(dir.path()),
            ProjectLanguage::Generic
        );
    }

    #[test]
    fn test_config_templates() {
        let templates = ProjectLanguage::Rust.config_templates();
        assert_eq!(templates.len(), 3);
        assert_eq!(templates[0].filename, "CLAUDE.md");
        assert_eq!(templates[1].filename, ".clinerules");
        assert_eq!(templates[2].filename, ".aider.conf.yml");
    }
}
