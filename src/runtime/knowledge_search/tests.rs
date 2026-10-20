use super::*;
use std::fs;

fn issue(root: &Path, file: &str, number: usize, status: &str, body: &str) {
    fs::create_dir_all(root.join(".usagi/issues")).unwrap();
    fs::write(root.join(".usagi/issues").join(file), format!("---\nnumber: {number}\ntitle: Example issue\nstatus: {status}\npriority: high\ndependson: []\ncreated_at: 2026-10-20T00:00:00Z\nupdated_at: 2026-10-20T00:00:00Z\n---\n{body}\n")).unwrap();
}
fn memory(root: &Path, file: &str, name: &str, body: &str) {
    fs::create_dir_all(root.join(".usagi/memory")).unwrap();
    fs::write(root.join(".usagi/memory").join(file), format!("---\nname: {name}\ntitle: Remember this\ntype: project\ncreated_at: 2026-10-20T00:00:00Z\nupdated_at: 2026-10-20T00:00:00Z\n---\n{body}\n")).unwrap();
}

#[test]
fn knowledge_search_uses_body_matching_readiness_and_exact_source_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut reader = KnowledgeSearch::new(root);
    assert_eq!(reader.search(""), Results::default());
    issue(root, "001-original.md", 1, "todo", "needle 日本語");
    issue(root, "001-sibling.md", 1, "done", "other needle");
    issue(root, "002-ready.md", 2, "todo", "NEEDLE");
    issue(root, "003-done.md", 3, "done", "needle");
    memory(root, "noncanonical.md", "memory-name", "NEEDLE 日本語");
    let results = reader.search("NeEdLe");
    assert_eq!(results.documents.len(), 5);
    assert!(results.errors.is_empty());
    assert!(results.documents[0].detail.contains("ambiguous"));
    assert!(results.documents[2].detail.ends_with("ready"));
    assert!(results.documents[3].detail.ends_with("not ready"));
    assert_eq!(results.documents[4].path, ".usagi/memory/noncanonical.md");
    for doc in results.documents {
        let lines = reader.read(&doc.path).unwrap().join("\n");
        assert!(lines.to_lowercase().contains("needle"));
    }
    assert_eq!(reader.search("日本語").documents.len(), 2);
    assert_eq!(reader.search("memory-name").documents.len(), 1);
    assert_eq!(reader.search("Remember").documents.len(), 1);
    assert_eq!(reader.search("Example").documents.len(), 4);
    assert!(reader.search("missing").documents.is_empty());
}

#[test]
fn knowledge_search_limits_each_store_independently_and_reports_partial_failure() {
    let tmp = tempfile::tempdir().unwrap();
    for number in 1..=102 {
        issue(tmp.path(), &format!("{number:03}-x.md"), number, "todo", "");
        memory(
            tmp.path(),
            &format!("m{number}.md"),
            &format!("m{number}"),
            "",
        );
    }
    let mut reader = KnowledgeSearch::new(tmp.path());
    let result = reader.search("");
    assert_eq!(result.documents.len(), 200);
    assert_eq!(result.omitted, 4);
    fs::remove_dir_all(tmp.path().join(".usagi/issues")).unwrap();
    fs::write(tmp.path().join(".usagi/issues"), "not a directory").unwrap();
    let result = reader.search("Remember");
    assert_eq!(result.documents.len(), 100);
    assert_eq!(result.errors.len(), 1);
    fs::remove_dir_all(tmp.path().join(".usagi/memory")).unwrap();
    fs::write(tmp.path().join(".usagi/memory"), "not a directory").unwrap();
    let result = reader.search("Remember");
    assert_eq!(result.errors.len(), 2);
}

#[test]
fn knowledge_search_preview_rejects_missing_escaping_symlink_and_oversized_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = KnowledgeSearch::new(tmp.path());
    for path in [
        "README.md",
        ".usagi/issues/../secret.md",
        ".usagi/memory/",
        ".usagi/memory/a.txt",
        ".usagi/issues/missing.md",
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
