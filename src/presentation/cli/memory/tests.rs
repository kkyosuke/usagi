use super::*;
use std::env;
use std::path::Path;

fn save(repo: &Path, name: &str, title: &str) -> Vec<String> {
    execute(
        repo,
        MemoryCommand::Save {
            name: name.to_string(),
            title: title.to_string(),
            kind: MemoryType::Project,
            related: vec![],
            body: "body".to_string(),
            json: false,
        },
    )
    .unwrap()
}

#[test]
fn save_reports_the_saved_name() {
    let tmp = tempfile::tempdir().unwrap();
    let out = save(tmp.path(), "My Fact", "A title");
    assert_eq!(out, vec!["saved my-fact (project)".to_string()]);
}

#[test]
fn save_with_json_prints_the_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let out = execute(
        tmp.path(),
        MemoryCommand::Save {
            name: "fact".to_string(),
            title: "Title".to_string(),
            kind: MemoryType::User,
            related: vec!["other".to_string()],
            body: "body".to_string(),
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(out.contains("\"name\": \"fact\""));
    assert!(out.contains("\"type\": \"user\""));
    assert!(out.contains("\"other\""));
}

#[test]
fn list_reports_empty_then_lists() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let empty = execute(
        repo,
        MemoryCommand::List {
            kind: None,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(empty, vec!["No memories found.".to_string()]);

    save(repo, "fact", "Remember this");
    let out = execute(
        repo,
        MemoryCommand::List {
            kind: None,
            json: false,
        },
    )
    .unwrap()
    .join("\n");
    assert!(out.contains("fact"));
    assert!(out.contains("Remember this"));
}

#[test]
fn list_with_type_filter_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "p", "Project fact");
    let out = execute(
        repo,
        MemoryCommand::List {
            kind: Some(MemoryType::Project),
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(out.contains("\"name\": \"p\""));
    assert!(out.contains("\"file\": \"p.md\""));
}

#[test]
fn show_renders_markdown_json_and_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "fact", "Title");

    let md = execute(
        repo,
        MemoryCommand::Show {
            name: "fact".to_string(),
            json: false,
        },
    )
    .unwrap()
    .join("\n");
    assert!(md.contains("name: fact"));
    assert!(md.contains("title: Title"));

    let js = execute(
        repo,
        MemoryCommand::Show {
            name: "fact".to_string(),
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(js.contains("\"body\": \"body\""));

    let missing = execute(
        repo,
        MemoryCommand::Show {
            name: "nope".to_string(),
            json: false,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no memory 'nope'".to_string()]);
}

#[test]
fn update_changes_fields_json_and_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "fact", "Old");

    let out = execute(
        repo,
        MemoryCommand::Update {
            name: "fact".to_string(),
            title: Some("New".to_string()),
            kind: None,
            related: None,
            body: None,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(out, vec!["updated fact".to_string()]);

    let js = execute(
        repo,
        MemoryCommand::Update {
            name: "fact".to_string(),
            title: None,
            kind: Some(MemoryType::Feedback),
            related: Some(vec!["x".to_string()]),
            body: Some("nb".to_string()),
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(js.contains("\"type\": \"feedback\""));

    let missing = execute(
        repo,
        MemoryCommand::Update {
            name: "nope".to_string(),
            title: Some("x".to_string()),
            kind: None,
            related: None,
            body: None,
            json: false,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no memory 'nope'".to_string()]);
}

#[test]
fn search_human_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "deploy", "Deploy steps");

    let human = execute(
        repo,
        MemoryCommand::Search {
            query: "deploy".to_string(),
            kind: None,
            json: false,
        },
    )
    .unwrap()
    .join("\n");
    assert!(human.contains("deploy"));

    let js = execute(
        repo,
        MemoryCommand::Search {
            query: "deploy".to_string(),
            kind: None,
            json: true,
        },
    )
    .unwrap()
    .join("\n");
    assert!(js.contains("\"name\": \"deploy\""));
}

#[test]
fn delete_requires_yes_then_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    save(repo, "fact", "Title");

    let guard = execute(
        repo,
        MemoryCommand::Delete {
            name: "fact".to_string(),
            yes: false,
        },
    )
    .unwrap();
    assert_eq!(guard, vec!["pass --yes to delete 'fact'".to_string()]);

    let deleted = execute(
        repo,
        MemoryCommand::Delete {
            name: "fact".to_string(),
            yes: true,
        },
    )
    .unwrap();
    assert_eq!(deleted, vec!["deleted 'fact'".to_string()]);

    let missing = execute(
        repo,
        MemoryCommand::Delete {
            name: "fact".to_string(),
            yes: true,
        },
    )
    .unwrap();
    assert_eq!(missing, vec!["no memory 'fact'".to_string()]);
}

#[test]
fn save_propagates_store_errors() {
    let tmp = tempfile::tempdir().unwrap();
    // A file where the memory directory should be makes the save fail, so the
    // error propagates out of `execute`.
    std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
    std::fs::write(tmp.path().join(".usagi/memory"), "x").unwrap();

    let result = execute(
        tmp.path(),
        MemoryCommand::Save {
            name: "fact".to_string(),
            title: "Title".to_string(),
            kind: MemoryType::Project,
            related: vec![],
            body: "body".to_string(),
            json: false,
        },
    );
    assert!(result.is_err());
}

#[test]
fn run_executes_against_the_current_directory() {
    // `run` reads the current directory; point it at a throwaway repo.
    let _guard = crate::test_support::process_env_guard();
    let tmp = tempfile::tempdir().unwrap();
    let original = env::current_dir().unwrap();
    env::set_current_dir(tmp.path()).unwrap();
    let result = run(MemoryCommand::List {
        kind: None,
        json: false,
    });
    env::set_current_dir(original).unwrap();
    assert!(result.is_ok());
}
