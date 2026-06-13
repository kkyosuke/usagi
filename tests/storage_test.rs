use usagi::domain::settings::Theme;
use usagi::infrastructure::storage::Storage;
use usagi::usecase::{settings, workspace};

fn temp_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let storage = Storage::new(dir.path().join("usagi"));
    (dir, storage)
}

#[test]
fn workspaces_default_to_empty_when_file_is_missing() {
    let (_dir, storage) = temp_storage();
    assert!(workspace::list(&storage).unwrap().is_empty());
}

#[test]
fn settings_default_when_file_is_missing() {
    let (_dir, storage) = temp_storage();
    let loaded = settings::load(&storage).unwrap();
    assert_eq!(loaded.theme, Theme::System);
    assert_eq!(loaded.default_workspace, None);
}

#[test]
fn add_list_touch_and_remove_workspace() {
    let (_dir, storage) = temp_storage();

    workspace::add(&storage, "alpha", "/tmp/alpha").unwrap();
    let beta = workspace::add(&storage, "beta", "/tmp/beta").unwrap();
    assert_eq!(beta.created_at, beta.updated_at);

    // Duplicate names are rejected.
    assert!(workspace::add(&storage, "alpha", "/tmp/other").is_err());

    let touched = workspace::touch(&storage, "alpha").unwrap();
    assert!(touched.updated_at > touched.created_at);

    // Most recently updated comes first.
    let names: Vec<_> = workspace::list(&storage)
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);

    workspace::remove(&storage, "beta").unwrap();
    assert!(workspace::remove(&storage, "beta").is_err());
    assert_eq!(workspace::list(&storage).unwrap().len(), 1);
}

#[test]
fn settings_round_trip() {
    let (_dir, storage) = temp_storage();

    settings::set_theme(&storage, Theme::Dark).unwrap();
    settings::set_default_workspace(&storage, Some("alpha".into())).unwrap();

    let loaded = settings::load(&storage).unwrap();
    assert_eq!(loaded.theme, Theme::Dark);
    assert_eq!(loaded.default_workspace.as_deref(), Some("alpha"));
}

#[test]
fn workspaces_and_settings_are_stored_in_separate_files() {
    let (_dir, storage) = temp_storage();

    workspace::add(&storage, "alpha", "/tmp/alpha").unwrap();
    settings::set_theme(&storage, Theme::Light).unwrap();

    assert!(storage.dir().join("workspaces.json").is_file());
    assert!(storage.dir().join("settings.json").is_file());

    let raw = std::fs::read_to_string(storage.dir().join("settings.json")).unwrap();
    assert!(raw.contains("\"theme\": \"light\""));
}
