use usagi::infrastructure::issue_number_sequence::IssueNumberSequence;
use usagi::infrastructure::storage::Storage;
use usagi::infrastructure::store_lock::StoreLock;
use usagi::usecase::doctor::diagnose;

use std::path::Path;
use std::process::{Command, Output, Stdio};

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
            "Antigravity",
            "notifications",
            "nerd font",
            "config"
        ]
    );
}

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn concurrent_processes_in_linked_worktrees_reserve_distinct_issue_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("README.md"), "workspace\n").unwrap();
    git(root, &["add", "README.md"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let sessions = root.join(".usagi/sessions");
    let a = sessions.join("a");
    let b = sessions.join("b");
    git(
        root,
        &["worktree", "add", "-q", "-b", "test-a", a.to_str().unwrap()],
    );
    git(
        root,
        &["worktree", "add", "-q", "-b", "test-b", b.to_str().unwrap()],
    );

    // Both child processes block on the same Git-common authority lock. Dropping
    // it releases them from this barrier to race from distinct worktrees.
    let authority = IssueNumberSequence::new(root, root);
    let barrier = StoreLock::acquire(authority.dir()).unwrap();
    let spawn = |worktree: &Path, title: &str| {
        Command::new(env!("CARGO_BIN_EXE_usagi"))
            .current_dir(worktree)
            .args(["issue", "create", "--title", title, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let child_a = spawn(&a, "from a");
    let child_b = spawn(&b, "from b");
    drop(barrier);

    let outputs = [
        child_a.wait_with_output().unwrap(),
        child_b.wait_with_output().unwrap(),
    ];
    let mut numbers = Vec::new();
    for output in outputs {
        assert!(
            output.status.success(),
            "issue create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        numbers.push(value["number"].as_u64().unwrap());
    }
    numbers.sort_unstable();
    assert_eq!(numbers, vec![1, 2]);
    assert_eq!(
        std::fs::read_dir(a.join(".usagi/issues")).unwrap().count(),
        3
    );
    assert_eq!(
        std::fs::read_dir(b.join(".usagi/issues")).unwrap().count(),
        3
    );
}
