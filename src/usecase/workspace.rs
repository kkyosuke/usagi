use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;

/// Register a new workspace. Fails if the name is already taken.
pub fn add(storage: &Storage, name: &str, path: impl Into<PathBuf>) -> Result<Workspace> {
    // Hold the cross-process lock across the whole read-modify-write so a
    // concurrent writer cannot read the same list and clobber our registration
    // (or both pass the duplicate-name guard for the same name).
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    if workspaces.iter().any(|w| w.name == name) {
        bail!("workspace '{name}' already exists");
    }
    let workspace = Workspace::new(name, path);
    workspaces.push(workspace.clone());
    storage.save_workspaces(&workspaces)?;
    Ok(workspace)
}

/// List all registered workspaces, most recently updated first.
pub fn list(storage: &Storage) -> Result<Vec<Workspace>> {
    let mut workspaces = storage.load_workspaces()?;
    workspaces.sort_by_key(|w| std::cmp::Reverse(w.updated_at));
    Ok(workspaces)
}

/// Remove a workspace by name. Fails if it does not exist.
pub fn remove(storage: &Storage, name: &str) -> Result<()> {
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let before = workspaces.len();
    workspaces.retain(|w| w.name != name);
    if workspaces.len() == before {
        bail!("workspace '{name}' not found");
    }
    storage.save_workspaces(&workspaces)
}

/// Update a workspace's last-used time to now.
pub fn touch(storage: &Storage, name: &str) -> Result<Workspace> {
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let Some(workspace) = workspaces.iter_mut().find(|w| w.name == name) else {
        bail!("workspace '{name}' not found");
    };
    workspace.touch();
    let touched = workspace.clone();
    storage.save_workspaces(&workspaces)?;
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        (dir, storage)
    }

    #[test]
    fn list_defaults_to_empty_when_file_is_missing() {
        let (_dir, storage) = temp_storage();
        assert!(list(&storage).unwrap().is_empty());
    }

    #[test]
    fn add_workspace() {
        let (_dir, storage) = temp_storage();
        let ws = add(&storage, "alpha", "/tmp/alpha").unwrap();
        assert_eq!(ws.name, "alpha");
        assert_eq!(ws.path.to_str().unwrap(), "/tmp/alpha");
        assert_eq!(ws.created_at, ws.updated_at);

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "alpha");
    }

    #[test]
    fn add_rejects_duplicate_names() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        assert!(add(&storage, "alpha", "/tmp/other").is_err());
    }

    #[test]
    fn touch_updates_last_used_time() {
        let (_dir, storage) = temp_storage();
        let added = add(&storage, "alpha", "/tmp/alpha").unwrap();

        let touched = touch(&storage, "alpha").unwrap();
        assert_eq!(touched.name, "alpha");
        assert!(touched.updated_at > added.updated_at);

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces[0].updated_at, touched.updated_at);
    }

    #[test]
    fn touch_missing_workspace_errors() {
        let (_dir, storage) = temp_storage();
        assert!(touch(&storage, "ghost").is_err());
    }

    #[test]
    fn list_sorts_most_recently_updated_first() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        add(&storage, "beta", "/tmp/beta").unwrap();

        // Touch alpha so it becomes most recently updated
        touch(&storage, "alpha").unwrap();

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "alpha");
        assert_eq!(workspaces[1].name, "beta");
    }

    #[test]
    fn remove_workspace() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        add(&storage, "beta", "/tmp/beta").unwrap();

        remove(&storage, "alpha").unwrap();

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "beta");
    }

    #[test]
    fn remove_missing_workspace_errors() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();

        assert!(remove(&storage, "beta").is_err());
    }
}
