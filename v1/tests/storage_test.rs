use std::path::{Path, PathBuf};

use usagi::domain::settings::{AgentCli, Theme};
use usagi::infrastructure::storage::{data_dir, Storage, DATA_DIR_ENV};
use usagi::usecase::{settings, workspace};

fn temp_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let storage = Storage::new(dir.path().join("usagi"));
    (dir, storage)
}

#[test]
fn settings_default_when_file_is_missing() {
    let (_dir, storage) = temp_storage();
    let loaded = settings::load(&storage).unwrap();
    assert_eq!(loaded.theme, Theme::System);
    assert_eq!(loaded.default_workspace, None);
}

#[test]
fn settings_round_trip() {
    let (_dir, storage) = temp_storage();

    settings::set_theme(&storage, Theme::Dark).unwrap();
    settings::set_default_workspace(&storage, Some("alpha".into())).unwrap();
    settings::set_notifications_enabled(&storage, false).unwrap();
    settings::set_agent_cli(&storage, AgentCli::Gemini).unwrap();

    let loaded = settings::load(&storage).unwrap();
    assert_eq!(loaded.theme, Theme::Dark);
    assert_eq!(loaded.default_workspace.as_deref(), Some("alpha"));
    assert!(!loaded.notifications_enabled);
    assert_eq!(loaded.agent_cli, AgentCli::Gemini);
}

#[test]
fn save_persists_settings_as_a_whole() {
    let (_dir, storage) = temp_storage();

    let settings = usagi::domain::settings::Settings {
        theme: Theme::Dark,
        default_workspace: Some("beta".into()),
        ..Default::default()
    };
    settings::save(&storage, &settings).unwrap();

    let loaded = settings::load(&storage).unwrap();
    assert_eq!(loaded, settings);
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

#[test]
fn load_workspaces_errors_on_corrupt_json() {
    let (_dir, storage) = temp_storage();
    std::fs::create_dir_all(storage.dir()).unwrap();
    std::fs::write(storage.dir().join("workspaces.json"), "{ not valid json").unwrap();
    assert!(workspace::list(&storage).is_err());
}

#[test]
fn load_settings_errors_when_path_is_not_a_file() {
    let (_dir, storage) = temp_storage();
    // Create a *directory* where the settings file is expected so the read
    // fails with an error other than NotFound.
    std::fs::create_dir_all(storage.dir().join("settings.json")).unwrap();
    assert!(settings::load(&storage).is_err());
}

#[test]
fn data_dir_and_open_default_respect_env_override() {
    // Touch the process-global override in a single test to avoid races.
    std::env::set_var(DATA_DIR_ENV, "/tmp/usagi-coverage-home");
    assert_eq!(
        data_dir().unwrap(),
        PathBuf::from("/tmp/usagi-coverage-home")
    );
    assert_eq!(
        Storage::open_default().unwrap().dir(),
        Path::new("/tmp/usagi-coverage-home")
    );

    std::env::remove_var(DATA_DIR_ENV);
    // With no override, the data dir falls back to ~/.usagi.
    assert!(data_dir().unwrap().ends_with(".usagi"));
}
