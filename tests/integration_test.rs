use usagi::infrastructure::storage::Storage;
use usagi::usecase::doctor::diagnose;

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
