use super::*;
use std::fs;

fn memory(root: &Path, file: &str, name: &str, body: &str) {
    fs::create_dir_all(root.join(".usagi/memory")).unwrap();
    fs::write(root.join(".usagi/memory").join(file), format!("---\nname: {name}\ntitle: Remember this\ntype: project\ncreated_at: 2026-10-20T00:00:00Z\nupdated_at: 2026-10-20T00:00:00Z\n---\n{body}\n")).unwrap();
}

#[test]
fn knowledge_search_uses_body_matching_and_exact_source_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut reader = KnowledgeSearch::new(root);
    assert_eq!(reader.search("", Arc::default()), Results::default());
    memory(root, "noncanonical.md", "memory-name", "NEEDLE 日本語");
    memory(root, "other.md", "other-name", "unrelated");
    let results = reader.search("NeEdLe", Arc::default());
    assert_eq!(results.documents.len(), 1);
    assert_eq!(results.errors, [] as [std::string::String; 0]);
    assert_eq!(results.documents[0].path, ".usagi/memory/noncanonical.md");
    assert_eq!(results.documents[0].title, "memory-name · Remember this");
    assert_eq!(results.documents[0].detail, "project");
    for doc in results.documents {
        let lines = reader.read(&doc.path).unwrap().join("\n");
        assert!(lines.to_lowercase().contains("needle"));
    }
    assert_eq!(reader.search("日本語", Arc::default()).documents.len(), 1);
    assert_eq!(
        reader.search("memory-name", Arc::default()).documents.len(),
        1
    );
    assert_eq!(reader.search("Remember", Arc::default()).documents.len(), 2);
    assert_eq!(
        reader.search("missing", Arc::default()).documents,
        [] as [usagi_tui::usecase::application::knowledge_search::Document; 0]
    );
}

#[test]
fn knowledge_search_limits_results_and_reports_store_failure() {
    let tmp = tempfile::tempdir().unwrap();
    for number in 1..=102 {
        memory(
            tmp.path(),
            &format!("m{number}.md"),
            &format!("m{number}"),
            "",
        );
    }
    let mut reader = KnowledgeSearch::new(tmp.path());
    let result = reader.search("", Arc::default());
    assert_eq!(result.documents.len(), 100);
    assert_eq!(result.omitted, 2);
    fs::remove_dir_all(tmp.path().join(".usagi/memory")).unwrap();
    fs::write(tmp.path().join(".usagi/memory"), "not a directory").unwrap();
    let result = reader.search("Remember", Arc::default());
    assert_eq!(result.documents.len(), 0);
    assert_eq!(result.errors.len(), 1);
}

#[test]
fn knowledge_search_preview_rejects_missing_escaping_symlink_and_oversized_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = KnowledgeSearch::new(tmp.path());
    for path in [
        "README.md",
        ".usagi/memory/../secret.md",
        ".usagi/memory/",
        ".usagi/memory/a.txt",
        ".usagi/issues/001-retired.md",
        ".usagi/memory/missing.md",
    ] {
        assert!(reader.read(path).is_err(), "{path}");
    }
    memory(tmp.path(), "large.md", "large", &"x".repeat(512 * 1024));
    assert!(
        reader
            .read(".usagi/memory/large.md")
            .unwrap_err()
            .contains("512 KiB")
    );
    fs::write(tmp.path().join("secret.md"), "secret").unwrap();
    std::os::unix::fs::symlink(
        tmp.path().join("secret.md"),
        tmp.path().join(".usagi/memory/link.md"),
    )
    .unwrap();
    assert!(reader.read(".usagi/memory/link.md").is_err());
}

#[test]
fn knowledge_search_cancels_a_held_store_lock() {
    use std::sync::mpsc;
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = KnowledgeSearch::new(tmp.path());
    assert_eq!(
        reader.search("", Arc::new(AtomicBool::new(true))),
        Results::default()
    );
    assert!(!tmp.path().join(".usagi").exists());

    let tmp = tempfile::tempdir().unwrap();
    memory(tmp.path(), "m.md", "m", "needle");
    let store_dir = tmp.path().join(".usagi/memory");
    fs::write(store_dir.join(".derived-dirty"), "repair").unwrap();
    let held = usagi_core::infrastructure::persistence::store_lock::StoreLock::acquire(&store_dir)
        .unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let mut reader = KnowledgeSearch::new(tmp.path());
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.send(reader.search("", worker_cancelled)).unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
    cancelled.store(true, Ordering::Release);
    let result = receiver.recv_timeout(Duration::from_secs(5));
    // Always release and join, including on a regression, before asserting.
    drop(held);
    worker.join().unwrap();
    assert_eq!(result.unwrap(), Results::default());
    // A cancelled empty-query listing must not rebuild the missing index.
    assert!(!tmp.path().join(".usagi/memory/index.json").exists());
}
