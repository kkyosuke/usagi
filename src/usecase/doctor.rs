/// Result of checking a single external dependency.
#[derive(Debug, Clone)]
pub struct DependencyCheck {
    pub name: &'static str,
    pub available: bool,
}

/// Check that the external tools usagi depends on are installed.
pub fn check_dependencies() -> Vec<DependencyCheck> {
    ["git", "bash"]
        .into_iter()
        .map(|name| DependencyCheck {
            name,
            available: which(name),
        })
        .collect()
}

fn which(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
