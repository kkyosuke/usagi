use usagi::infrastructure::storage::Storage;
use usagi::usecase::doctor::diagnose;

#[test]
fn help_hides_advanced_config_command() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("--help")
        .output()
        .expect("failed to run usagi --help");

    assert!(
        output.status.success(),
        "usagi --help should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("config ")),
        "usagi --help should not list the advanced config command:\n{stdout}"
    );
}

#[test]
fn test_diagnose_reports_all_subsystems() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let storage = Storage::new(dir.path().join("usagi"));
    let names: Vec<_> = diagnose(&storage).iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "git",
            "bash",
            "Claude",
            "Codex",
            "sakana.ai",
            "Gemini",
            "notifications",
            "nerd font",
            "config"
        ]
    );
}
