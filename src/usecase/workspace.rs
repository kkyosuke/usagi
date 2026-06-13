use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;

/// Register a new workspace. Fails if the name is already taken.
pub fn add(storage: &Storage, name: &str, path: impl Into<PathBuf>) -> Result<Workspace> {
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
    let mut workspaces = storage.load_workspaces()?;
    let Some(workspace) = workspaces.iter_mut().find(|w| w.name == name) else {
        bail!("workspace '{name}' not found");
    };
    workspace.touch();
    let touched = workspace.clone();
    storage.save_workspaces(&workspaces)?;
    Ok(touched)
}
